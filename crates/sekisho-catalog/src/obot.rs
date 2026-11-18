//! Obot's MCP catalog format, read as Obot `5e471f101` reads it
//! (`pkg/mcpcatalog/walker.go`, `validation.go`,
//! `MCPServerCatalogEntryManifest`): which files are catalogs, and what
//! each item becomes. Obot's npx, uvx, containerized and remote runtimes
//! are read; the rest are refused, and fields only Obot's hosting uses are
//! listed as ignored (D176).

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_yaml_ng::{Mapping, Value};

use crate::check::{CheckError, CheckOptions, Item, ItemRead};
use crate::model::{
    ConfigField, ConfigTarget, EntrySpec, McpRuntime, McpServerSpec, ObotVmcp, Refusal,
    ToolPreview, VmcpComponent, Warning,
};
use crate::names::{EntryKind, EntryName};
use crate::pins::{ImageRef, NpmPackage, PythonPackage};
use crate::source::endpoint_url;

// ------------------------------------------------------------ the files

const PATTERNS: &str = ".obotcatalogs";
const IGNORE: &str = ".ignoreobotcatalogs";
const DEFAULT_PATTERNS: [&str; 3] = ["*.json", "*.yaml", "*.yml"];
/// Pattern files are short lists; this is generous.
const PATTERN_FILE_LIMIT: u64 = 64 << 10;

/// The catalog files of a source, as Obot's `WalkCatalogFiles` picks them.
#[derive(Default)]
pub(crate) struct CatalogFiles {
    /// Path relative to the source's folder, and where it is.
    pub files: Vec<(String, PathBuf)>,
    pub refused: Vec<Refusal>,
    pub warnings: Vec<Warning>,
}

struct Patterns(Vec<glob::Pattern>);

impl Patterns {
    /// Go's `path.Match`: `*` never crosses `/`, and matches a leading `.`.
    const OPTIONS: glob::MatchOptions = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };

    fn matches(&self, candidate: &str) -> bool {
        self.0
            .iter()
            .any(|p| p.matches_with(candidate, Self::OPTIONS))
    }
}

/// Reads a pattern file: one pattern a line, `#` comments; missing or
/// empty is `defaults`. A symlink fails the source, never followed.
fn read_patterns(
    root: &Path,
    file: &str,
    defaults: &[&str],
    warnings: &mut Vec<Warning>,
) -> Result<Patterns, CheckError> {
    let path = root.join(file);
    let lines: Vec<String> = match fs::symlink_metadata(&path) {
        Err(_) => Vec::new(),
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            return Err(CheckError::Unreadable {
                reason: format!("{file} is a symlink or not a file"),
            });
        }
        Ok(_) => {
            let mut text = String::new();
            fs::File::open(&path)
                .and_then(|f| f.take(PATTERN_FILE_LIMIT).read_to_string(&mut text))
                .map_err(|e| CheckError::Unreadable {
                    reason: format!("{file}: {}", e.kind()),
                })?;
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(str::to_owned)
                .collect()
        }
    };
    let lines = if lines.is_empty() {
        defaults.iter().map(|s| s.to_string()).collect()
    } else {
        lines
    };
    let mut patterns = Vec::new();
    for line in lines {
        match glob::Pattern::new(&line) {
            Ok(p) => patterns.push(p),
            Err(_) => warnings.push(Warning {
                path: file.to_owned(),
                message: format!("the pattern {line:?} isn't valid and matches nothing"),
            }),
        }
    }
    Ok(Patterns(patterns))
}

/// Picks the catalog files under `root`: `.obotcatalogs` patterns (Obot's
/// defaults without it) less `.ignoreobotcatalogs`, hidden folders
/// skipped, a basename pattern matching at any depth and a path pattern
/// from the root. More than `limit` files fail the source.
pub(crate) fn catalog_files(root: &Path, limit: usize) -> Result<CatalogFiles, CheckError> {
    let mut found = CatalogFiles::default();
    let patterns = read_patterns(root, PATTERNS, &DEFAULT_PATTERNS, &mut found.warnings)?;
    let ignore = read_patterns(root, IGNORE, &[], &mut found.warnings)?;
    walk(root, "", &patterns, &ignore, limit, &mut found)?;
    Ok(found)
}

fn walk(
    dir: &Path,
    prefix: &str,
    patterns: &Patterns,
    ignore: &Patterns,
    limit: usize,
    found: &mut CatalogFiles,
) -> Result<(), CheckError> {
    let unreadable = |e: std::io::Error| CheckError::Unreadable {
        reason: format!(
            "{}: {}",
            if prefix.is_empty() { "." } else { prefix },
            e.kind()
        ),
    };
    let mut items = fs::read_dir(dir)
        .map_err(unreadable)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(unreadable)?;
    items.sort_by_key(|i| i.file_name());
    for item in items {
        let name = item.file_name().to_string_lossy().into_owned();
        let rel = format!("{prefix}{name}");
        let kind = item.file_type().map_err(unreadable)?;
        if kind.is_dir() {
            if name.starts_with('.') || ignore.matches(&rel) {
                continue;
            }
            walk(
                &item.path(),
                &format!("{rel}/"),
                patterns,
                ignore,
                limit,
                found,
            )?;
            continue;
        }
        let included = patterns.matches(&name) || patterns.matches(&rel);
        if !included || ignore.matches(&rel) {
            continue;
        }
        if !kind.is_file() {
            found.refused.push(Refusal {
                path: rel,
                kind: None,
                name: None,
                reason: "a symlink or special file isn't read".to_owned(),
            });
            continue;
        }
        if found.files.len() == limit {
            return Err(CheckError::TooManyCatalogFiles { limit });
        }
        found.files.push((rel, item.path()));
    }
    Ok(())
}

// ------------------------------------------------------------ fields

/// A map from a catalog, read as Obot's YAML reads it: scalars where text
/// is wanted become their text.
pub(crate) struct Fields<'a> {
    map: &'a Mapping,
    /// Where the map is, for messages: `npxConfig.`.
    at: String,
}

impl<'a> Fields<'a> {
    pub(crate) fn root(map: &'a Mapping) -> Self {
        Self {
            map,
            at: String::new(),
        }
    }

    fn get(&self, key: &str) -> Option<&'a Value> {
        self.map.get(key).filter(|v| !v.is_null())
    }

    pub(crate) fn has(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    fn scalar(value: &Value) -> Option<String> {
        match value {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }

    /// Text, or `None` when absent or empty.
    pub(crate) fn text(&self, key: &str) -> Result<Option<String>, String> {
        match self.get(key) {
            None => Ok(None),
            Some(v) => Self::scalar(v)
                .map(|s| Some(s).filter(|s| !s.is_empty()))
                .ok_or_else(|| format!("{}{key} must be text", self.at)),
        }
    }

    fn flag(&self, key: &str) -> Result<bool, String> {
        match self.get(key) {
            None => Ok(false),
            Some(Value::Bool(b)) => Ok(*b),
            Some(_) => Err(format!("{}{key} must be true or false", self.at)),
        }
    }

    fn texts(&self, key: &str) -> Result<Vec<String>, String> {
        match self.get(key) {
            None => Ok(Vec::new()),
            Some(Value::Sequence(items)) => items
                .iter()
                .map(|v| {
                    Self::scalar(v)
                        .ok_or_else(|| format!("{}{key} must be a list of text", self.at))
                })
                .collect(),
            Some(_) => Err(format!("{}{key} must be a list of text", self.at)),
        }
    }

    fn object(&self, key: &str) -> Result<Option<Fields<'a>>, String> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Mapping(map)) => Ok(Some(Fields {
                map,
                at: format!("{}{key}.", self.at),
            })),
            Some(_) => Err(format!("{}{key} must be a map", self.at)),
        }
    }

    fn list(&self, key: &str) -> Result<Vec<Fields<'a>>, String> {
        match self.get(key) {
            None => Ok(Vec::new()),
            Some(Value::Sequence(items)) => items
                .iter()
                .enumerate()
                .map(|(i, v)| match v {
                    Value::Mapping(map) => Ok(Fields {
                        map,
                        at: format!("{}{key}[{i}].", self.at),
                    }),
                    _ => Err(format!("{}{key}[{i}] must be a map", self.at)),
                })
                .collect(),
            Some(_) => Err(format!("{}{key} must be a list", self.at)),
        }
    }
}

// ------------------------------------------------------------ names and config

/// An Obot name made into an entry name: lowercase, every run of other
/// characters one `-`, trimmed, cut to 64 and trimmed again.
pub(crate) fn entry_name_from(raw: &str) -> Option<EntryName> {
    let mut out = String::new();
    for c in raw.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    let cut = &trimmed[..trimmed.len().min(64)];
    EntryName::parse(cut.trim_end_matches('-')).ok()
}

/// A configuration field as a catalog writes it, in either format.
pub(crate) struct RawConfig {
    pub name: String,
    pub description: String,
    pub key: Option<String>,
    pub value: Option<String>,
    pub sensitive: bool,
    pub required: bool,
    pub prefix: Option<String>,
    pub usage: Option<String>,
}

/// Checks a configuration field and puts its key in Obot's normal form:
/// `GITHUB_TOKEN` for a variable, `X-API-KEY` for a header. A sensitive
/// value is refused: delivered config files hold no secrets (D168).
pub(crate) fn config_field(raw: RawConfig) -> Result<ConfigField, String> {
    let key = raw
        .key
        .filter(|k| !k.is_empty())
        .unwrap_or_else(|| raw.name.clone());
    let label = if raw.name.is_empty() { &key } else { &raw.name };
    let target = match raw.usage.as_deref() {
        Some("env") => ConfigTarget::Env,
        Some("header") => ConfigTarget::Header,
        Some(other @ ("file" | "dynamicFile" | "interpolated")) => {
            return Err(format!(
                "config {label}: usage {other} isn't supported; Sekisho passes settings as environment variables or headers"
            ));
        }
        Some(other) => return Err(format!("config {label}: usage {other} isn't known")),
        None => return Err(format!("config {label} needs a usage: env or header")),
    };
    let key = match target {
        ConfigTarget::Env => key.to_uppercase().replace('-', "_"),
        ConfigTarget::Header => key.to_uppercase().replace('_', "-"),
    };
    let b = key.as_bytes();
    let valid = match target {
        ConfigTarget::Env => {
            b.first()
                .is_some_and(|c| c.is_ascii_uppercase() || *c == b'_')
                && b.iter()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_')
        }
        ConfigTarget::Header => {
            b.first().is_some_and(u8::is_ascii_alphanumeric)
                && b.iter()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'-')
        }
    };
    if !valid {
        return Err(format!(
            "config {label}: the key isn't a valid variable or header name"
        ));
    }
    let value = raw.value.filter(|v| !v.is_empty());
    if target == ConfigTarget::Env && value.is_some() && crate::launch::steers_installs(&key) {
        return Err(format!(
            "config {label}: {key} can't be set by a catalog: it changes which package is installed or what runs"
        ));
    }
    if raw.sensitive && value.is_some() {
        return Err(format!(
            "config {label}: a sensitive value can't be kept in a catalog; leave it for each person to give"
        ));
    }
    Ok(ConfigField {
        key,
        name: raw.name,
        description: raw.description,
        required: raw.required,
        sensitive: raw.sensitive,
        value,
        target,
        prefix: raw.prefix.filter(|p| !p.is_empty()),
    })
}

// ------------------------------------------------------------ items

/// Obot fields that only its hosting uses, at the item's top level.
const HOSTING_TOP: [&str; 4] = [
    "resources",
    "minObotVersion",
    "maxObotVersion",
    "injectObotHeaders",
];
/// The same, inside a runtime's config.
const HOSTING_RUNTIME: [&str; 4] = [
    "egressDomains",
    "denyAllEgress",
    "startupTimeoutSeconds",
    "healthzPath",
];

fn refused(kind: Option<EntryKind>, name: Option<String>, reason: String) -> ItemRead {
    ItemRead {
        item: Item::Refused { kind, name, reason },
        warnings: Vec::new(),
    }
}

/// One Obot catalog item: an MCP server, or a virtual server (`type: vmcp`).
pub(crate) fn read_item(map: &Mapping, opts: &CheckOptions) -> ItemRead {
    let f = Fields::root(map);
    let raw_name = f
        .text("entryKey")
        .ok()
        .flatten()
        .or_else(|| f.text("name").ok().flatten());
    match f.text("type") {
        Ok(None) => {}
        Ok(Some(t)) if t == "entry" => {}
        Ok(Some(t)) if t == "vmcp" => return read_vmcp(&f),
        Ok(Some(t)) => return refused(None, raw_name, format!("item type {t} isn't known")),
        Err(e) => return refused(None, raw_name, e),
    }
    let kind = Some(EntryKind::McpServer);
    let Some(name) = raw_name.as_deref().and_then(entry_name_from) else {
        return refused(
            kind,
            raw_name,
            "needs an entryKey or a name with letters or digits".to_owned(),
        );
    };
    let mut warnings = Vec::new();
    match server_spec(&f, opts, &name, &mut warnings) {
        Ok(spec) => ItemRead {
            item: Item::Entry {
                kind: EntryKind::McpServer,
                name,
                spec: EntrySpec::McpServer(spec),
            },
            warnings,
        },
        Err(reason) => refused(kind, Some(name.to_string()), reason),
    }
}

fn hosting(f: &Fields, fields: &[&str], name: &EntryName, warnings: &mut Vec<String>) {
    for field in fields {
        if f.has(field) {
            warnings.push(format!(
                "{name}: {}{field} is ignored; it only configures Obot's hosting",
                f.at
            ));
        }
    }
}

fn server_spec(
    f: &Fields,
    opts: &CheckOptions,
    name: &EntryName,
    warnings: &mut Vec<String>,
) -> Result<McpServerSpec, String> {
    // Obot itself refuses these since it moved them to `config`.
    if f.has("env") {
        return Err("env is no longer part of Obot's format; move it to config".to_owned());
    }
    if f.object("multiUserConfig")?
        .is_some_and(|m| m.has("userDefinedHeaders"))
    {
        return Err(
            "multiUserConfig.userDefinedHeaders is no longer part of Obot's format; move it to config"
                .to_owned(),
        );
    }
    hosting(f, &HOSTING_TOP, name, warnings);
    let description = f
        .text("shortDescription")?
        .or(f.text("description")?)
        .unwrap_or_default();

    let mut static_oauth = false;
    let runtime = match f.text("runtime")?.as_deref() {
        Some("npx") => {
            let c = f
                .object("npxConfig")?
                .ok_or("runtime npx needs npxConfig")?;
            hosting(&c, &HOSTING_RUNTIME, name, warnings);
            let package = c.text("package")?.ok_or("npxConfig needs a package")?;
            McpRuntime::Npx {
                package: NpmPackage::parse_exact(&package).map_err(|e| e.to_string())?,
                args: c.texts("args")?,
            }
        }
        Some("uvx") => {
            let c = f
                .object("uvxConfig")?
                .ok_or("runtime uvx needs uvxConfig")?;
            hosting(&c, &HOSTING_RUNTIME, name, warnings);
            let package = c.text("package")?.ok_or("uvxConfig needs a package")?;
            McpRuntime::Uvx {
                package: PythonPackage::parse_exact(&package).map_err(|e| e.to_string())?,
                command: c.text("command")?,
                args: c.texts("args")?,
            }
        }
        Some("containerized") => {
            let c = f
                .object("containerizedConfig")?
                .ok_or("runtime containerized needs containerizedConfig")?;
            hosting(&c, &HOSTING_RUNTIME, name, warnings);
            let image = c
                .text("image")?
                .ok_or("containerizedConfig needs an image")?;
            McpRuntime::Container {
                image: ImageRef::parse(&image).map_err(|e| e.to_string())?,
                command: c.text("command")?,
                args: c.texts("args")?,
                port: port(&c)?,
                path: c.text("path")?,
            }
        }
        Some("remote") => {
            let (url, oauth) = remote(f, opts)?;
            static_oauth = oauth;
            McpRuntime::Remote { url }
        }
        Some(r @ ("openapi" | "composite")) => {
            return Err(format!(
                "the {r} runtime isn't supported; Sekisho delivers npx, uvx, containerized and remote servers"
            ));
        }
        Some("vmcp") => {
            return Err(
                "runtime vmcp isn't a server; virtual servers are items with type: vmcp".to_owned(),
            );
        }
        Some(other) => return Err(format!("runtime {other} isn't known")),
        None => return Err("needs a runtime".to_owned()),
    };

    let mut config = Vec::new();
    for c in f.list("config")? {
        let label = c.text("name")?.or(c.text("key")?).unwrap_or_default();
        if c.flag("userAllowed")? {
            return Err(format!(
                "config {label}: userAllowed is only for Obot's deployed servers"
            ));
        }
        for ignored in ["options", "secretBinding"] {
            if c.has(ignored) {
                warnings.push(format!("{name}: config {label}'s {ignored} is ignored"));
            }
        }
        config.push(config_field(RawConfig {
            name: c.text("name")?.unwrap_or_default(),
            description: c.text("description")?.unwrap_or_default(),
            key: c.text("key")?,
            value: c.text("value")?,
            sensitive: c.flag("sensitive")?,
            required: c.flag("required")?,
            prefix: c.text("prefix")?,
            usage: c.text("usage")?,
        })?);
    }

    let mut tool_preview = Vec::new();
    for t in f.list("toolPreview")? {
        if let Some(tool) = t.text("name")? {
            tool_preview.push(ToolPreview {
                name: tool,
                description: t.text("description")?.unwrap_or_default(),
            });
        }
    }
    let unsupported_tools = match f.object("metadata")? {
        Some(m) => m
            .text("unsupportedTools")?
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
            .collect(),
        None => Vec::new(),
    };

    Ok(McpServerSpec {
        description,
        runtime,
        config,
        tool_preview,
        unsupported_tools,
        static_oauth,
    })
}

/// Obot's port is an integer whose zero means none.
fn port(c: &Fields) -> Result<Option<u16>, String> {
    let bad = || format!("{}port must be a port number", c.at);
    match c.text("port")? {
        None => Ok(None),
        Some(p) => match p.parse::<u16>().map_err(|_| bad())? {
            0 => Ok(None),
            n => Ok(Some(n)),
        },
    }
}

fn remote(f: &Fields, opts: &CheckOptions) -> Result<(String, bool), String> {
    let Some(c) = f.object("remoteConfig")? else {
        return Err("a remote server needs remoteConfig.fixedURL".to_owned());
    };
    if c.has("headers") {
        return Err(
            "remoteConfig.headers is no longer part of Obot's format; move it to config".to_owned(),
        );
    }
    if c.text("tunnelName")?.is_some() {
        return Err(
            "remoteConfig.tunnelName isn't supported; Sekisho reaches remote servers directly, not through Obot's tunnels"
                .to_owned(),
        );
    }
    let url = match c.text("fixedURL")? {
        Some(url) => {
            endpoint_url(&url, "remote URL", opts.allow_plain_http).map_err(|e| e.to_string())?
        }
        None if c.text("urlTemplate")?.is_some() => {
            return Err(
                "remoteConfig.urlTemplate isn't supported; give the server's remoteConfig.fixedURL"
                    .to_owned(),
            );
        }
        None => return Err("a remote server needs remoteConfig.fixedURL".to_owned()),
    };
    Ok((url, c.flag("staticOAuthRequired")?))
}

// ------------------------------------------------------------ virtual servers

/// An Obot virtual server, offered as an environment with what can't carry
/// over listed (Task 9).
fn read_vmcp(f: &Fields) -> ItemRead {
    let raw_key = f
        .text("entryKey")
        .ok()
        .flatten()
        .or_else(|| f.text("displayName").ok().flatten());
    match vmcp(f) {
        Ok(v) => ItemRead {
            item: Item::Vmcp(v),
            warnings: Vec::new(),
        },
        Err(reason) => refused(None, raw_key, reason),
    }
}

fn vmcp(f: &Fields) -> Result<ObotVmcp, String> {
    let display_name = f
        .text("displayName")?
        .ok_or("a virtual server needs a displayName")?;
    let raw_key = f.text("entryKey")?.unwrap_or_else(|| display_name.clone());
    let key = entry_name_from(&raw_key)
        .ok_or("a virtual server needs an entryKey or a displayName with letters or digits")?;
    if f.has("minObotVersion") || f.has("maxObotVersion") {
        return Err(
            "virtual servers can't have minObotVersion or maxObotVersion, as in Obot".to_owned(),
        );
    }
    let mut differences = Vec::new();
    if f.flag("toolSearch")? {
        differences.push(
            "toolSearch: Sekisho lists every allowed tool; there is no tool search".to_owned(),
        );
    }
    if !f.list("profiles")?.is_empty() {
        differences.push(
            "profiles: Sekisho has no profiles; the environment has every component".to_owned(),
        );
    }
    let mut components = Vec::new();
    for c in f.list("components")? {
        let label = c.text("name")?.or(c.text("id")?).unwrap_or_default();
        let entry_key = c
            .text("mcpServerCatalogEntryKey")?
            .ok_or_else(|| format!("component {label} needs an mcpServerCatalogEntryKey"))?;
        if entry_key.contains("::") {
            differences.push(format!(
                "component {label}: {entry_key} is an entry of another catalog; it is left out"
            ));
            continue;
        }
        let Some(server) = entry_name_from(&entry_key) else {
            differences.push(format!(
                "component {label}: {entry_key} can't name an entry; it is left out"
            ));
            continue;
        };
        if c.text("toolPrefix")?.is_some() {
            differences.push(format!(
                "component {label}: Sekisho has no tool prefixes; tools keep their names"
            ));
        }
        if !c.list("configuration")?.is_empty() {
            differences.push(format!(
                "component {label}: configuration policies aren't carried over"
            ));
        }
        if c.flag("forceSingleUser")? {
            differences.push(format!(
                "component {label}: Sekisho has no single-user servers"
            ));
        }
        let overrides = c.list("toolOverrides")?;
        let mut tools = Vec::new();
        for o in &overrides {
            let Some(tool) = o.text("name")? else {
                continue;
            };
            if let Some(renamed) = o.text("overrideName")? {
                differences.push(format!(
                    "component {label}: tool {tool} is named {renamed} in Obot; Sekisho keeps {tool}"
                ));
            }
            if o.text("overrideDescription")?.is_some() {
                differences.push(format!(
                    "component {label}: tool {tool}'s description is replaced in Obot; Sekisho keeps the server's"
                ));
            }
            if o.flag("enabled")? {
                tools.push(tool);
            }
        }
        components.push(VmcpComponent {
            server,
            tools: (!overrides.is_empty()).then_some(tools),
        });
    }
    Ok(ObotVmcp {
        key,
        display_name,
        description: f.text("description")?.unwrap_or_default(),
        components,
        differences,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_made_as_obot_sanitises_them() {
        let made = |s: &str| entry_name_from(s).map(|n| n.to_string());
        assert_eq!(made("GitHub Issues").as_deref(), Some("github-issues"));
        assert_eq!(made("  A__B--c!! ").as_deref(), Some("a-b-c"));
        assert_eq!(made("Café 2").as_deref(), Some("caf-2"));
        assert_eq!(made("!!!"), None);
        let long = format!("{}-b", "a".repeat(63));
        assert_eq!(made(&long).unwrap(), "a".repeat(63));
    }

    fn raw(usage: Option<&str>, key: &str) -> RawConfig {
        RawConfig {
            name: "N".into(),
            description: String::new(),
            key: Some(key.into()),
            value: None,
            sensitive: false,
            required: false,
            prefix: None,
            usage: usage.map(str::to_owned),
        }
    }

    #[test]
    fn config_keys_take_obots_normal_form() {
        assert_eq!(
            config_field(raw(Some("env"), "a-b_c")).unwrap().key,
            "A_B_C"
        );
        assert_eq!(
            config_field(raw(Some("header"), "x_api-key")).unwrap().key,
            "X-API-KEY"
        );
        assert!(config_field(raw(Some("env"), "a b")).is_err());
        assert!(config_field(raw(Some("env"), "1A")).is_err());
        assert!(config_field(raw(None, "a")).unwrap_err().contains("usage"));
        assert!(config_field(raw(Some("dynamicFile"), "a")).is_err());
    }
}
