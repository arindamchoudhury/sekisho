//! The environment format, `sekisho/v1alpha1` (D174): the API's
//! snake_case fields after `api_version` and `kind: Environment`, read
//! from YAML (Git mode, `sekisho check`, `PUT` as YAML) or from the API's
//! JSON. Owners, ids, timestamps and budgets are never in it.
//!
//! Every problem is reported at once, with the field named as the API
//! names it (`models[1]`, `labels.cost-centre`, `mcp_servers[0].server`).

use std::collections::{HashMap, HashSet};

use sekisho_core::access::is_group_reference;
use sekisho_core::names::{EnvName, ModelName};
use serde_yaml_ng::{Mapping, Value};

use crate::labels::Labels;
use crate::names::EntryRef;

pub const API_VERSION: &str = "sekisho/v1alpha1";
pub const KIND: &str = "Environment";
/// agentgateway's Gemini paths read the model from after `/models/`.
const RESERVED_NAME: &str = "models";

/// The document's fields, in the order `to_yaml` writes them.
const FIELDS: [&str; 17] = [
    "api_version",
    "kind",
    "name",
    "description",
    "notice",
    "access_groups",
    "models",
    "default_models",
    "small_model",
    "labels",
    "mcp_servers",
    "a2a_agents",
    "skills",
    "plugins",
    "prompts",
    "instructions",
    "toolkits",
];
/// Fields of the API's answer that a `PUT` may send back unchanged; the
/// server reads `owner` itself. None of them is ever in YAML.
const ANSWER_ONLY: [&str; 8] = [
    "id",
    "owner",
    "owner_email",
    "created_at",
    "updated_at",
    "effective_default_models",
    "catalog_status",
    "delivered",
];

/// One thing wrong with a document.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Problem {
    /// As the API names it; empty for the document as a whole.
    pub field: String,
    pub message: String,
}

impl Problem {
    fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DefaultModels {
    pub anthropic: Option<ModelName>,
    pub openai: Option<ModelName>,
    pub gemini: Option<ModelName>,
}

/// An MCP server an environment lists, with the tools it allows; `None`
/// allows them all. Written as a bare name when `tools` is `None`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerUse {
    pub server: EntryRef,
    pub tools: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvironmentDoc {
    pub api_version: String,
    /// `Environment` when given; JSON may leave it out.
    pub kind: Option<String>,
    pub name: EnvName,
    pub description: String,
    pub notice: String,
    pub access_groups: Vec<String>,
    pub models: Vec<ModelName>,
    pub default_models: DefaultModels,
    pub small_model: Option<ModelName>,
    pub labels: Labels,
    pub mcp_servers: Vec<McpServerUse>,
    pub a2a_agents: Vec<EntryRef>,
    pub skills: Vec<EntryRef>,
    pub plugins: Vec<EntryRef>,
    pub prompts: Vec<EntryRef>,
    /// Delivered in this order, then each toolkit's (D220).
    pub instructions: Vec<EntryRef>,
    pub toolkits: Vec<EntryRef>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    Yaml,
    Json,
}

/// Reads a YAML document; `api_version` and `kind` are required.
pub fn parse_yaml(text: &str) -> Result<EnvironmentDoc, Vec<Problem>> {
    let value: Value = serde_yaml_ng::from_str(text)
        .map_err(|e| vec![Problem::new("", format!("isn't valid YAML: {e}"))])?;
    read(&value, Origin::Yaml, true)
}

/// Reads the API's JSON. `api_version` may be left out; `kind` too unless
/// `require_kind`. The read-only fields of a `GET` answer are ignored.
pub fn from_json_value(
    json: serde_json::Value,
    require_kind: bool,
) -> Result<EnvironmentDoc, Vec<Problem>> {
    let value = serde_yaml_ng::to_value(&json)
        .map_err(|e| vec![Problem::new("", format!("can't be read: {e}"))])?;
    read(&value, Origin::Json, require_kind)
}

/// Text from a scalar: numbers and booleans become their text, as YAML
/// readers expect.
fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Where each item a reader kept stood in its list, so a check names the
/// item's own position even after an earlier one was refused.
#[derive(Default)]
struct Positions(HashMap<&'static str, Vec<usize>>);

impl Positions {
    fn at(&self, list: &str, i: usize) -> usize {
        self.0
            .get(list)
            .and_then(|p| p.get(i))
            .copied()
            .unwrap_or(i)
    }
}

struct Reader<'a> {
    map: &'a Mapping,
    problems: Vec<Problem>,
    positions: Positions,
}

impl Reader<'_> {
    fn get(&self, key: &str) -> Option<&Value> {
        self.map.get(key).filter(|v| !v.is_null())
    }

    fn text(&mut self, key: &str) -> Option<String> {
        let value = self.get(key)?;
        let text = scalar(value);
        if text.is_none() {
            self.problems.push(Problem::new(key, "must be text"));
        }
        text
    }

    fn list(&mut self, key: &str) -> Vec<(usize, Value)> {
        match self.get(key) {
            None => Vec::new(),
            Some(Value::Sequence(items)) => items.iter().cloned().enumerate().collect(),
            Some(_) => {
                self.problems.push(Problem::new(key, "must be a list"));
                Vec::new()
            }
        }
    }

    /// Keeps the position of an item read from `list`.
    fn kept<T>(&mut self, list: &'static str, i: usize, item: Option<T>) -> Option<T> {
        if item.is_some() {
            self.positions.0.entry(list).or_default().push(i);
        }
        item
    }

    fn texts(&mut self, key: &'static str) -> Vec<String> {
        let mut out = Vec::new();
        for (i, item) in self.list(key) {
            let text = scalar(&item);
            if text.is_none() {
                self.problems
                    .push(Problem::new(format!("{key}[{i}]"), "must be text"));
            }
            out.extend(self.kept(key, i, text));
        }
        out
    }

    fn model(&mut self, field: &str, value: &Value) -> Option<ModelName> {
        let Some(text) = scalar(value) else {
            self.problems
                .push(Problem::new(field, "must be a model's name"));
            return None;
        };
        ModelName::parse(&text)
            .map_err(|e| self.problems.push(Problem::new(field, e.to_string())))
            .ok()
    }

    fn reference(&mut self, field: &str, value: &Value) -> Option<EntryRef> {
        let Some(text) = scalar(value) else {
            self.problems.push(Problem::new(
                field,
                "must be an entry's name, or name@source",
            ));
            return None;
        };
        EntryRef::parse(&text)
            .map_err(|e| self.problems.push(Problem::new(field, e.to_string())))
            .ok()
    }

    fn references(&mut self, key: &'static str) -> Vec<EntryRef> {
        let items = self.list(key);
        let mut out = Vec::new();
        for (i, v) in &items {
            let r = self.reference(&format!("{key}[{i}]"), v);
            out.extend(self.kept(key, *i, r));
        }
        out
    }

    fn mcp_server(&mut self, i: usize, value: &Value) -> Option<McpServerUse> {
        let field = format!("mcp_servers[{i}]");
        match value {
            Value::Mapping(map) => {
                let mut ok = true;
                for key in map.keys() {
                    let key = scalar(key).unwrap_or_default();
                    if key != "server" && key != "tools" {
                        self.problems.push(Problem::new(
                            format!("{field}.{key}"),
                            "isn't a field: use server and tools",
                        ));
                        ok = false;
                    }
                }
                let server = match map.get("server") {
                    Some(v) => self.reference(&format!("{field}.server"), v),
                    None => {
                        self.problems
                            .push(Problem::new(format!("{field}.server"), "is required"));
                        None
                    }
                };
                let tools = match map.get("tools") {
                    None | Some(Value::Null) => None,
                    Some(Value::Sequence(items)) => {
                        let mut tools = Vec::new();
                        for (j, t) in items.iter().enumerate() {
                            match scalar(t) {
                                Some(t) => tools.push(t),
                                None => {
                                    self.problems.push(Problem::new(
                                        format!("{field}.tools[{j}]"),
                                        "must be a tool's name",
                                    ));
                                    ok = false;
                                }
                            }
                        }
                        Some(tools)
                    }
                    Some(_) => {
                        self.problems
                            .push(Problem::new(format!("{field}.tools"), "must be a list"));
                        ok = false;
                        None
                    }
                };
                (ok && server.is_some()).then(|| McpServerUse {
                    server: server.expect("checked"),
                    tools,
                })
            }
            Value::String(_) => self.reference(&field, value).map(|server| McpServerUse {
                server,
                tools: None,
            }),
            _ => {
                self.problems.push(Problem::new(
                    field,
                    "must be a server's name, or {server, tools}",
                ));
                None
            }
        }
    }
}

fn read(value: &Value, origin: Origin, require_kind: bool) -> Result<EnvironmentDoc, Vec<Problem>> {
    let Value::Mapping(map) = value else {
        return Err(vec![Problem::new(
            "",
            "an environment must be a map of fields",
        )]);
    };
    let mut r = Reader {
        map,
        problems: Vec::new(),
        positions: Positions::default(),
    };
    for key in map.keys() {
        let Some(key) = key.as_str() else {
            r.problems
                .push(Problem::new("", "field names must be text"));
            continue;
        };
        if FIELDS.contains(&key) || (origin == Origin::Json && ANSWER_ONLY.contains(&key)) {
            continue;
        }
        let message = match key {
            "owner" => "owners are set in Sekisho, not in the environment's YAML",
            k if ANSWER_ONLY.contains(&k) => "is set by Sekisho; leave it out",
            _ => "isn't a field of an environment",
        };
        r.problems.push(Problem::new(key, message));
    }

    let api_version = match (r.text("api_version"), origin) {
        (Some(v), _) if v == API_VERSION => v,
        (Some(v), _) => {
            r.problems.push(Problem::new(
                "api_version",
                format!("{v} isn't known; use {API_VERSION}"),
            ));
            v
        }
        (None, Origin::Yaml) => {
            r.problems.push(Problem::new(
                "api_version",
                format!("is required: {API_VERSION}"),
            ));
            String::new()
        }
        (None, Origin::Json) => API_VERSION.to_owned(),
    };
    let kind = r.text("kind");
    match kind.as_deref() {
        Some(KIND) => {}
        Some(other) => r.problems.push(Problem::new(
            "kind",
            format!("{other} isn't an environment; use {KIND}"),
        )),
        None if require_kind => r
            .problems
            .push(Problem::new("kind", format!("is required: {KIND}"))),
        None => {}
    }
    let name = match r.text("name") {
        Some(n) => EnvName::parse(&n)
            .map_err(|e| r.problems.push(Problem::new("name", e.to_string())))
            .ok(),
        None => {
            r.problems.push(Problem::new("name", "is required"));
            None
        }
    };
    let description = r.text("description").unwrap_or_default();
    let notice = r.text("notice").unwrap_or_default();
    let access_groups = r.texts("access_groups");
    let mut models = Vec::new();
    for (i, v) in r.list("models") {
        let m = r.model(&format!("models[{i}]"), &v);
        models.extend(r.kept("models", i, m));
    }

    let mut default_models = DefaultModels::default();
    match r.get("default_models").cloned() {
        None => {}
        Some(Value::Mapping(m)) => {
            for (key, value) in &m {
                let key = scalar(key).unwrap_or_default();
                let field = format!("default_models.{key}");
                let slot = match key.as_str() {
                    "anthropic" => &mut default_models.anthropic,
                    "openai" => &mut default_models.openai,
                    "gemini" => &mut default_models.gemini,
                    _ => {
                        r.problems.push(Problem::new(
                            field,
                            "isn't an API format: use anthropic, openai or gemini",
                        ));
                        continue;
                    }
                };
                if !value.is_null() {
                    *slot = r.model(&field, value);
                }
            }
        }
        Some(_) => r.problems.push(Problem::new(
            "default_models",
            "must be a map of formats to models",
        )),
    }
    let small_model = r
        .get("small_model")
        .cloned()
        .and_then(|v| r.model("small_model", &v));
    let labels = match r.get("labels") {
        None => Labels::default(),
        Some(v) => Labels::from_yaml(v).unwrap_or_else(|errors| {
            r.problems
                .extend(errors.into_iter().map(|(f, m)| Problem::new(f, m)));
            Labels::default()
        }),
    };
    let mut mcp_servers = Vec::new();
    for (i, v) in r.list("mcp_servers") {
        let u = r.mcp_server(i, &v);
        mcp_servers.extend(r.kept("mcp_servers", i, u));
    }
    let a2a_agents = r.references("a2a_agents");
    let skills = r.references("skills");
    let plugins = r.references("plugins");
    let prompts = r.references("prompts");
    let instructions = r.references("instructions");
    let toolkits = r.references("toolkits");

    let mut problems = r.problems;
    let positions = r.positions;
    let doc = EnvironmentDoc {
        api_version,
        kind,
        name: name.unwrap_or_else(|| EnvName::parse("unnamed").expect("a valid name")),
        description,
        notice,
        access_groups,
        models,
        default_models,
        small_model,
        labels,
        mcp_servers,
        a2a_agents,
        skills,
        plugins,
        prompts,
        instructions,
        toolkits,
    };
    // The offline checks too, at each item's own position. A model that
    // couldn't be read can't be a default that could, so defaults are
    // judged against the models that were read.
    problems.extend(doc.check_at(&positions));
    if problems.is_empty() {
        Ok(doc)
    } else {
        Err(problems)
    }
}

/// Positions of items already seen earlier in the list.
fn repeats<T: AsRef<str>>(items: impl IntoIterator<Item = T>) -> Vec<usize> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .enumerate()
        .filter(|(_, item)| !seen.insert(item.as_ref().to_owned()))
        .map(|(i, _)| i)
        .collect()
}

impl EnvironmentDoc {
    /// What can be checked without the install: repeated models or
    /// references within a kind, defaults that aren't listed, tool names
    /// that are empty or repeated, the reserved name, access-group syntax.
    /// Whether entries exist and formats reach models is the store's.
    pub fn check(&self) -> Vec<Problem> {
        self.check_at(&Positions::default())
    }

    fn check_at(&self, at: &Positions) -> Vec<Problem> {
        let mut p = Vec::new();
        if self.name.as_str() == RESERVED_NAME {
            p.push(Problem::new(
                "name",
                "models is reserved: in a Gemini path the gateway reads the model from after /models/",
            ));
        }
        for (i, group) in self.access_groups.iter().enumerate() {
            if !is_group_reference(group) {
                p.push(Problem::new(
                    format!("access_groups[{}]", at.at("access_groups", i)),
                    "isn't a group: use a Sekisho group's name or <connector>/<group>",
                ));
            }
        }
        for i in repeats(self.models.iter().map(ModelName::as_str)) {
            p.push(Problem::new(
                format!("models[{}]", at.at("models", i)),
                "is listed twice",
            ));
        }
        let listed = |m: &ModelName| self.models.contains(m);
        for (format, model) in [
            ("anthropic", &self.default_models.anthropic),
            ("openai", &self.default_models.openai),
            ("gemini", &self.default_models.gemini),
        ] {
            if model.as_ref().is_some_and(|m| !listed(m)) {
                p.push(Problem::new(
                    format!("default_models.{format}"),
                    "isn't one of the environment's models",
                ));
            }
        }
        if self.small_model.as_ref().is_some_and(|m| !listed(m)) {
            p.push(Problem::new(
                "small_model",
                "isn't one of the environment's models",
            ));
        }
        for i in repeats(self.mcp_servers.iter().map(|u| u.server.to_string())) {
            p.push(Problem::new(
                format!("mcp_servers[{}]", at.at("mcp_servers", i)),
                "is listed twice",
            ));
        }
        for (i, u) in self.mcp_servers.iter().enumerate() {
            let Some(tools) = &u.tools else { continue };
            let field = format!("mcp_servers[{}].tools", at.at("mcp_servers", i));
            if tools.is_empty() {
                p.push(Problem::new(
                    field.clone(),
                    "give at least one tool, or leave tools out to allow them all",
                ));
            }
            let mut seen = HashSet::new();
            for (j, tool) in tools.iter().enumerate() {
                if tool.trim().is_empty() {
                    p.push(Problem::new(
                        format!("{field}[{j}]"),
                        "a tool's name can't be empty",
                    ));
                } else if !seen.insert(tool) {
                    p.push(Problem::new(format!("{field}[{j}]"), "is listed twice"));
                }
            }
        }
        for (kind, refs) in [
            ("a2a_agents", &self.a2a_agents),
            ("skills", &self.skills),
            ("plugins", &self.plugins),
            ("prompts", &self.prompts),
            ("instructions", &self.instructions),
            ("toolkits", &self.toolkits),
        ] {
            for i in repeats(refs.iter().map(ToString::to_string)) {
                p.push(Problem::new(
                    format!("{kind}[{}]", at.at(kind, i)),
                    "is listed twice",
                ));
            }
        }
        p
    }

    /// The YAML form: fields in the format's order, empty lists and unset
    /// defaults left out.
    pub fn to_yaml(&self) -> String {
        let text = |s: &str| Value::String(s.to_owned());
        let list = |items: Vec<Value>| Value::Sequence(items);
        let mut m = Mapping::new();
        m.insert(text("api_version"), text(&self.api_version));
        m.insert(text("kind"), text(KIND));
        m.insert(text("name"), text(self.name.as_str()));
        if !self.description.is_empty() {
            m.insert(text("description"), text(&self.description));
        }
        if !self.notice.is_empty() {
            m.insert(text("notice"), text(&self.notice));
        }
        if !self.access_groups.is_empty() {
            m.insert(
                text("access_groups"),
                list(self.access_groups.iter().map(|g| text(g)).collect()),
            );
        }
        if !self.models.is_empty() {
            m.insert(
                text("models"),
                list(self.models.iter().map(|x| text(x.as_str())).collect()),
            );
        }
        let mut defaults = Mapping::new();
        for (format, model) in [
            ("anthropic", &self.default_models.anthropic),
            ("openai", &self.default_models.openai),
            ("gemini", &self.default_models.gemini),
        ] {
            if let Some(model) = model {
                defaults.insert(text(format), text(model.as_str()));
            }
        }
        if !defaults.is_empty() {
            m.insert(text("default_models"), Value::Mapping(defaults));
        }
        if let Some(small) = &self.small_model {
            m.insert(text("small_model"), text(small.as_str()));
        }
        if !self.labels.as_map().is_empty() {
            let labels = self
                .labels
                .as_map()
                .iter()
                .map(|(k, v)| (text(k), text(v)))
                .collect();
            m.insert(text("labels"), Value::Mapping(labels));
        }
        if !self.mcp_servers.is_empty() {
            let servers = self
                .mcp_servers
                .iter()
                .map(|u| match &u.tools {
                    None => text(&u.server.to_string()),
                    Some(tools) => {
                        let mut s = Mapping::new();
                        s.insert(text("server"), text(&u.server.to_string()));
                        s.insert(text("tools"), list(tools.iter().map(|t| text(t)).collect()));
                        Value::Mapping(s)
                    }
                })
                .collect();
            m.insert(text("mcp_servers"), list(servers));
        }
        for (key, refs) in [
            ("a2a_agents", &self.a2a_agents),
            ("skills", &self.skills),
            ("plugins", &self.plugins),
            ("prompts", &self.prompts),
            ("instructions", &self.instructions),
            ("toolkits", &self.toolkits),
        ] {
            if !refs.is_empty() {
                m.insert(
                    text(key),
                    list(refs.iter().map(|r| text(&r.to_string())).collect()),
                );
            }
        }
        serde_yaml_ng::to_string(&Value::Mapping(m)).expect("a map of text serialises")
    }
}
