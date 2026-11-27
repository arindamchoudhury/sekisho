//! The install engine: what each adapter wants, against the lock and the
//! disk, then the writes.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use sekisho_catalog::delivery::{Delivery, EntryAddress};
use sekisho_catalog::model::sha256_hex;
use sekisho_catalog::names::EntryKind;
use sekisho_catalog::tree::Limits;
use sekisho_catalog::unicode;
use sekisho_core::names::EnvName;
use serde_json::Value;

use super::files::{self, Disk, Journal};
use super::instructions::{ALWAYS_LOADED_WARNING_BYTES, is_own_instructions_file};
use super::lock::{EntryLock, EnvLock, FileLock, KeyLock, Lock, StoredFile, StoredKey, ToolLock};
use super::{
    Clash, Fetch, InstallError, Item, Layered, Options, Report, Scope, Sign, exclude, merge,
};
use crate::adapters::{
    Adapter, ContentKind, FileWrite, Format, KeyWrite, Origin, RelPath, RenderContext, Rendered,
    Root, ScopeKind, Tool,
};

/// At most this many files, and bytes, per install (D207).
const MAX_FILES: usize = 2_000;
const MAX_BYTES: u64 = 200 << 20;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

fn refused(e: impl std::fmt::Display) -> InstallError {
    InstallError::Refused(e.to_string())
}

/// A key file's changes, applied in one read-merge-write.
struct KeyFile {
    format: Format,
    root: PathBuf,
    /// Each key set to a value, or removed (`None`).
    ops: Vec<(Vec<String>, Option<Value>)>,
    /// Sekisho made the file and nothing of Sekisho's is left in it.
    delete_if_empty: bool,
}

struct Planner<'a> {
    scope: &'a Scope,
    options: &'a Options,
    lock: Lock,
    items: Vec<Item>,
    consent: Vec<String>,
    clashes: Vec<Clash>,
    writes: BTreeMap<PathBuf, (Vec<u8>, bool, PathBuf)>,
    removes: BTreeMap<PathBuf, PathBuf>,
    keys: BTreeMap<PathBuf, KeyFile>,
    seen_files: HashSet<(Tool, Root, RelPath)>,
    seen_keys: HashSet<(Tool, Root, RelPath, Vec<String>)>,
    texts: HashMap<PathBuf, Option<String>>,
}

fn abs(root: &Path, rel: &RelPath) -> PathBuf {
    let mut p = root.to_path_buf();
    for segment in rel.segments() {
        p.push(segment);
    }
    p
}

fn value_sha(value: &Value) -> String {
    sha256_hex(&serde_json::to_vec(value).expect("JSON values serialize"))
}

/// A unified diff of two texts; `None` unless both are UTF-8.
fn text_diff(old: Option<&[u8]>, new: &[u8]) -> Option<String> {
    let old = std::str::from_utf8(old.unwrap_or_default()).ok()?;
    let new = std::str::from_utf8(new).ok()?;
    Some(
        similar::TextDiff::from_lines(old, new)
            .unified_diff()
            .header("before", "after")
            .to_string(),
    )
}

fn value_diff(old: Option<&Value>, new: &Value) -> Option<String> {
    let pretty = |v: &Value| serde_json::to_string_pretty(v).expect("JSON values serialize") + "\n";
    let old = old.map(pretty).unwrap_or_default();
    text_diff(Some(old.as_bytes()), pretty(new).as_bytes())
}

impl<'a> Planner<'a> {
    fn new(scope: &'a Scope, options: &'a Options, lock: Lock) -> Self {
        Self {
            scope,
            options,
            lock,
            items: Vec::new(),
            consent: Vec::new(),
            clashes: Vec::new(),
            writes: BTreeMap::new(),
            removes: BTreeMap::new(),
            keys: BTreeMap::new(),
            seen_files: HashSet::new(),
            seen_keys: HashSet::new(),
            texts: HashMap::new(),
        }
    }

    fn dir(&self, root: Root) -> Result<PathBuf, InstallError> {
        self.scope
            .root(root)
            .ok_or_else(|| refused("a global install has no project folder to write in"))
    }

    fn what_file(&self, root: Root, path: &RelPath) -> String {
        match (root, self.scope.kind()) {
            (Root::Project, _) => path.to_string(),
            (Root::SekishoConfig, ScopeKind::Project) => format!(".sekisho/{path}"),
            _ => match self.scope.root(root) {
                Some(dir) => format!("{}/{path}", dir.display()).replace('\\', "/"),
                None => path.to_string(),
            },
        }
    }

    /// Refuses a repository's or person's own instructions file (D223),
    /// whatever adapter asks for it.
    fn check_own(&self, root: Root, path: &RelPath) -> Result<(), InstallError> {
        if is_own_instructions_file(root, path) {
            return Err(refused(format!(
                "{} is the repository's or the person's own instructions: Sekisho never \
                 writes it",
                self.what_file(root, path)
            )));
        }
        Ok(())
    }

    fn what_key(&self, root: Root, file: &RelPath, key: &[String]) -> String {
        format!("{} {}", self.what_file(root, file), merge::show_key(key))
    }

    fn item(&mut self, tool: Tool, sign: Sign, what: String, diff: Option<String>) {
        self.items.push(Item {
            tool,
            sign,
            what,
            diff,
        });
    }

    /// A key file's text on disk, read once.
    fn text(&mut self, path: &Path) -> Result<Option<String>, InstallError> {
        if let Some(t) = self.texts.get(path) {
            return Ok(t.clone());
        }
        let text = match files::read(path)? {
            Disk::Missing => None,
            Disk::File(b) => Some(
                String::from_utf8(b)
                    .map_err(|_| refused(format!("{} isn't UTF-8 text", path.display())))?,
            ),
            Disk::Folder => return Err(refused(format!("{} is a folder", path.display()))),
        };
        self.texts.insert(path.to_path_buf(), text.clone());
        Ok(text)
    }

    fn disk_value(
        &mut self,
        path: &Path,
        format: Format,
        key: &[String],
    ) -> Result<Option<Value>, InstallError> {
        match self.text(path)? {
            None => Ok(None),
            Some(t) => merge::get(format, &t, key)
                .map_err(|e| refused(format!("can't read {}: {e}", path.display()))),
        }
    }

    fn clash(&mut self, tool: Tool, origin: &Origin, what: String, others: &[String]) {
        let (kind, name) = origin
            .named()
            .unwrap_or((ContentKind::Models, what.clone()));
        let held_by = EnvName::parse(&others[0]).expect("lock environment names are checked");
        self.clashes.push(Clash {
            tool,
            kind,
            name,
            path_or_key: what,
            held_by,
        });
    }

    /// `env` wants this whole file for `tool`.
    fn want_file(&mut self, env: &str, tool: Tool, w: &FileWrite) -> Result<(), InstallError> {
        self.check_own(w.root, &w.path)?;
        let root = self.dir(w.root)?;
        files::check_no_links(&root, &w.path)?;
        let path = abs(&root, &w.path);
        let sha = sha256_hex(&w.content);
        let what = self.what_file(w.root, &w.path);
        self.seen_files.insert((tool, w.root, w.path.clone()));
        let found = self
            .lock
            .files
            .iter()
            .position(|l| l.tool == tool && l.root == w.root && l.path == w.path);
        let Some(i) = found else {
            match files::read(&path)? {
                // The user's own copy, the same: adopted.
                Disk::File(b) if sha256_hex(&b) == sha => {}
                Disk::Missing => {
                    self.writes
                        .insert(path, (w.content.clone(), w.executable, root));
                    self.item(tool, Sign::Added, what, None);
                }
                other => {
                    self.consent.push(what.clone());
                    let diff = text_diff(other.bytes(), &w.content);
                    self.writes
                        .insert(path, (w.content.clone(), w.executable, root));
                    self.item(tool, Sign::Updated, what, diff);
                }
            }
            self.lock.files.push(FileLock {
                tool,
                root: w.root,
                path: w.path.clone(),
                sha256: sha,
                origin: w.origin.clone(),
                envs: vec![env.to_owned()],
            });
            return Ok(());
        };
        let held = &self.lock.files[i];
        if held.sha256 == sha {
            if !held.envs.iter().any(|e| e == env) {
                self.lock.files[i].envs.push(env.to_owned());
            }
            return Ok(());
        }
        let others: Vec<String> = held.envs.iter().filter(|e| *e != env).cloned().collect();
        let models = held.origin == Origin::Models && w.origin == Origin::Models;
        let same_entry = held.origin == w.origin && matches!(w.origin, Origin::Entry(_));
        if !others.is_empty() && !models && !same_entry {
            self.clash(tool, &w.origin, what, &others);
            return Ok(());
        }
        let disk = files::read(&path)?;
        let edited = disk.bytes().map(sha256_hex).as_deref() != Some(held.sha256.as_str());
        if edited && !self.options.force {
            self.item(tool, Sign::Kept, what, None);
            return Ok(());
        }
        let diff = text_diff(disk.bytes(), &w.content);
        self.writes
            .insert(path, (w.content.clone(), w.executable, root));
        self.item(tool, Sign::Updated, what, diff);
        let held = &mut self.lock.files[i];
        held.sha256 = sha;
        held.origin = w.origin.clone();
        if models {
            held.envs = vec![env.to_owned()];
        } else if !held.envs.iter().any(|e| e == env) {
            held.envs.push(env.to_owned());
        }
        Ok(())
    }

    fn set_key(&mut self, path: PathBuf, root: PathBuf, w: &KeyWrite) {
        self.keys
            .entry(path)
            .or_insert_with(|| KeyFile {
                format: w.format,
                root,
                ops: Vec::new(),
                delete_if_empty: false,
            })
            .ops
            .push((w.key.clone(), Some(w.value.clone())));
    }

    /// `env` wants this key for `tool`.
    fn want_key(&mut self, env: &str, tool: Tool, w: &KeyWrite) -> Result<(), InstallError> {
        self.check_own(w.root, &w.file)?;
        let root = self.dir(w.root)?;
        files::check_no_links(&root, &w.file)?;
        let path = abs(&root, &w.file);
        let sha = value_sha(&w.value);
        let what = self.what_key(w.root, &w.file, &w.key);
        self.seen_keys
            .insert((tool, w.root, w.file.clone(), w.key.clone()));
        let found =
            self.lock.keys.iter().position(|l| {
                l.tool == tool && l.root == w.root && l.file == w.file && l.key == w.key
            });
        let Some(i) = found else {
            let text = self.text(&path)?;
            let existed = text.is_some();
            // The person's own Markdown file gets Sekisho's block only once
            // they agree (D202); after that, lines come and go in it.
            if w.format == Format::MarkdownBlock
                && text.as_deref().is_some_and(|t| !merge::has_block(t))
                && !self.keys.contains_key(&path)
            {
                let file = self.what_file(w.root, &w.file);
                if !self.consent.contains(&file) {
                    self.consent.push(file);
                }
            }
            // A value of the person's own gets Sekisho's section only once
            // they agree (D202), Codex's `developer_instructions` say.
            if text
                .as_deref()
                .is_some_and(|t| merge::foreign_text(w.format, t, &w.key))
                && !self.consent.contains(&what)
            {
                self.consent.push(what.clone());
            }
            let current = self.disk_value(&path, w.format, &w.key)?;
            match &current {
                Some(v) if *v == w.value => {}
                None => {
                    self.set_key(path, root, w);
                    self.item(tool, Sign::Added, what, None);
                }
                Some(v) => {
                    self.consent.push(what.clone());
                    let diff = value_diff(Some(v), &w.value);
                    self.set_key(path, root, w);
                    self.item(tool, Sign::Updated, what, diff);
                }
            }
            self.lock.keys.push(KeyLock {
                tool,
                root: w.root,
                file: w.file.clone(),
                format: w.format,
                key: w.key.clone(),
                sha256: sha,
                origin: w.origin.clone(),
                envs: vec![env.to_owned()],
                created_file: !existed,
            });
            return Ok(());
        };
        let held = &self.lock.keys[i];
        if held.sha256 == sha {
            if !held.envs.iter().any(|e| e == env) {
                self.lock.keys[i].envs.push(env.to_owned());
            }
            return Ok(());
        }
        let others: Vec<String> = held.envs.iter().filter(|e| *e != env).cloned().collect();
        let models = held.origin == Origin::Models && w.origin == Origin::Models;
        let same_entry = held.origin == w.origin && matches!(w.origin, Origin::Entry(_));
        if !others.is_empty() && !models && !same_entry {
            self.clash(tool, &w.origin, what, &others);
            return Ok(());
        }
        let held_sha = held.sha256.clone();
        let current = self.disk_value(&path, w.format, &w.key)?;
        let edited = current.as_ref().map(value_sha).as_deref() != Some(held_sha.as_str());
        if edited && !self.options.force {
            self.item(tool, Sign::Kept, what, None);
            return Ok(());
        }
        let diff = value_diff(current.as_ref(), &w.value);
        self.set_key(path, root, w);
        self.item(tool, Sign::Updated, what, diff);
        let held = &mut self.lock.keys[i];
        held.sha256 = sha;
        held.origin = w.origin.clone();
        if models {
            held.envs = vec![env.to_owned()];
        } else if !held.envs.iter().any(|e| e == env) {
            held.envs.push(env.to_owned());
        }
        Ok(())
    }

    /// `env` no longer delivers what it held for `tool` and this run didn't
    /// want, among the origins `keep_if` passes.
    fn release(&mut self, env: &str, tool: Tool, only: impl Fn(&Origin) -> bool) {
        for l in &mut self.lock.files {
            if l.tool == tool
                && only(&l.origin)
                && !self.seen_files.contains(&(tool, l.root, l.path.clone()))
            {
                l.envs.retain(|e| e != env);
            }
        }
        for l in &mut self.lock.keys {
            if l.tool == tool
                && only(&l.origin)
                && !self
                    .seen_keys
                    .contains(&(tool, l.root, l.file.clone(), l.key.clone()))
            {
                l.envs.retain(|e| e != env);
            }
        }
    }

    /// Files and keys no environment holds any more go, unless edited
    /// (D203) or another tool still holds the same one (D201).
    fn finalize(&mut self) -> Result<(), InstallError> {
        let (gone, kept): (Vec<FileLock>, Vec<FileLock>) = std::mem::take(&mut self.lock.files)
            .into_iter()
            .partition(|l| l.envs.is_empty());
        self.lock.files = kept;
        for l in gone {
            let root = self.dir(l.root)?;
            let path = abs(&root, &l.path);
            let shared = self
                .lock
                .files
                .iter()
                .any(|o| o.root == l.root && o.path == l.path);
            if shared || self.writes.contains_key(&path) {
                continue;
            }
            files::check_no_links(&root, &l.path)?;
            let what = self.what_file(l.root, &l.path);
            match files::read(&path)? {
                Disk::Missing => {}
                Disk::File(b) if self.options.force || sha256_hex(&b) == l.sha256 => {
                    self.removes.insert(path, root);
                    self.item(l.tool, Sign::Removed, what, None);
                }
                _ => self.item(l.tool, Sign::Kept, what, None),
            }
        }

        let (gone, kept): (Vec<KeyLock>, Vec<KeyLock>) = std::mem::take(&mut self.lock.keys)
            .into_iter()
            .partition(|l| l.envs.is_empty());
        self.lock.keys = kept;
        let mut created: HashSet<PathBuf> = HashSet::new();
        for l in gone {
            let root = self.dir(l.root)?;
            let path = abs(&root, &l.file);
            let shared = self
                .lock
                .keys
                .iter()
                .any(|o| o.root == l.root && o.file == l.file && o.key == l.key);
            let rewritten = self
                .keys
                .get(&path)
                .is_some_and(|f| f.ops.iter().any(|(k, v)| *k == l.key && v.is_some()));
            if shared || rewritten {
                continue;
            }
            files::check_no_links(&root, &l.file)?;
            if l.created_file {
                created.insert(path.clone());
            }
            let what = self.what_key(l.root, &l.file, &l.key);
            let current = self.disk_value(&path, l.format, &l.key)?;
            match current {
                None => {}
                Some(v) if self.options.force || value_sha(&v) == l.sha256 => {
                    self.keys
                        .entry(path)
                        .or_insert_with(|| KeyFile {
                            format: l.format,
                            root,
                            ops: Vec::new(),
                            delete_if_empty: false,
                        })
                        .ops
                        .push((l.key.clone(), None));
                    self.item(l.tool, Sign::Removed, what, None);
                }
                Some(_) => self.item(l.tool, Sign::Kept, what, None),
            }
        }
        // A file Sekisho made goes once nothing of its is left in it.
        for path in created {
            let still = self.lock.keys.iter().any(|o| {
                self.scope
                    .root(o.root)
                    .is_some_and(|r| abs(&r, &o.file) == path)
            });
            if let Some(f) = self.keys.get_mut(&path) {
                f.delete_if_empty = !still;
            }
        }
        Ok(())
    }

    fn report(&mut self, warnings: Vec<String>) -> Report {
        let mut items = std::mem::take(&mut self.items);
        items.sort_by(|a, b| (a.tool, &a.what, a.sign).cmp(&(b.tool, &b.what, b.sign)));
        Report {
            items,
            warnings,
            models: models(&self.lock),
            layered: Vec::new(),
        }
    }

    /// Writes everything, the lock last; puts it all back on a failure.
    fn execute(self) -> Result<Vec<String>, InstallError> {
        let mut journal = Journal::default();
        let result = (|| {
            for (path, (bytes, executable, root)) in &self.writes {
                journal.write(path, bytes, *executable, root)?;
            }
            for (path, root) in &self.removes {
                journal.remove(path, root)?;
            }
            for (path, file) in &self.keys {
                let apply = |text: Option<String>| -> Result<(String, bool), InstallError> {
                    let mut text = text;
                    let mut empty = false;
                    for (key, value) in &file.ops {
                        match value {
                            Some(v) => {
                                text = Some(
                                    merge::set(file.format, text.as_deref(), key, v).map_err(
                                        |e| {
                                            refused(format!("can't change {}: {e}", path.display()))
                                        },
                                    )?,
                                );
                                empty = false;
                            }
                            None => {
                                if let Some(t) = &text {
                                    let (t, e) =
                                        merge::remove(file.format, t, key).map_err(|e| {
                                            refused(format!("can't change {}: {e}", path.display()))
                                        })?;
                                    text = Some(t);
                                    empty = e;
                                }
                            }
                        }
                    }
                    Ok((text.unwrap_or_default(), empty))
                };
                let read_text = || -> Result<Option<String>, InstallError> {
                    Ok(files::read(path)?
                        .bytes()
                        .map(|b| String::from_utf8_lossy(b).into_owned()))
                };
                let before = read_text()?;
                let mut out = apply(before.clone())?;
                // Saved meanwhile by an editor: merged again, never lost.
                let now = read_text()?;
                if now != before {
                    out = apply(now)?;
                }
                let (text, empty) = out;
                if empty && file.delete_if_empty {
                    journal.remove(path, &file.root)?;
                } else {
                    journal.write(path, text.as_bytes(), false, &file.root)?;
                }
            }
            let lock_path = self.scope.lock_path();
            let stop = match self.scope.project_folder() {
                Some(p) => p.to_path_buf(),
                None => lock_path
                    .parent()
                    .and_then(Path::parent)
                    .map(Path::to_path_buf)
                    .unwrap_or_default(),
            };
            if self.lock.is_empty() {
                journal.remove(&lock_path, &stop)?;
            } else {
                journal.write(&lock_path, &self.lock.to_bytes(), false, &stop)?;
            }
            Ok(())
        })();
        if let Err(e) = result {
            journal.roll_back();
            return Err(e);
        }
        let mut warnings = Vec::new();
        if let Some(project) = self.scope.project_folder() {
            let paths = exclude_paths(&self.lock);
            if let Err(e) = exclude::update(project, &paths) {
                warnings.push(format!("can't update .git/info/exclude: {e}"));
            }
        }
        Ok(warnings)
    }
}

/// The project files the lock holds, for `.git/info/exclude` (D206).
fn exclude_paths(lock: &Lock) -> Vec<String> {
    if lock.is_empty() {
        return Vec::new();
    }
    let mut paths: BTreeSet<String> = BTreeSet::new();
    for l in &lock.files {
        if l.root == Root::Project {
            paths.insert(l.path.to_string());
        }
    }
    for l in &lock.keys {
        if l.root == Root::Project {
            paths.insert(l.file.to_string());
        }
    }
    let mut out = vec![".sekisho/".to_owned()];
    out.extend(paths);
    out
}

fn models(lock: &Lock) -> Vec<(Tool, EnvName)> {
    lock.models
        .iter()
        .filter_map(|(t, e)| Some((Tool::parse(t)?, EnvName::parse(e).ok()?)))
        .collect()
}

/// Every delivered file's bytes, checked (D207).
fn fetch_all(
    d: &Delivery,
    fetch: Fetch<'_>,
    warnings: &mut Vec<String>,
) -> Result<BTreeMap<String, Vec<u8>>, InstallError> {
    let limits = Limits::DEFAULT;
    let mut blobs: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let (mut files, mut bytes) = (0usize, 0u64);
    for e in &d.entries {
        let label = format!("{} {}@{}", e.kind.as_str(), e.name, e.source);
        let (mut entry_files, mut entry_bytes) = (0usize, 0u64);
        for f in &e.files {
            RelPath::parse(&f.path).map_err(|err| refused(format!("{label}: {err}")))?;
            entry_files += 1;
            entry_bytes += f.size;
            files += 1;
            bytes += f.size;
            if f.size > limits.file_bytes {
                return Err(refused(format!("{label}: {} is over 10 MiB", f.path)));
            }
            if entry_files > limits.entry_files || entry_bytes > limits.entry_bytes {
                return Err(refused(format!("{label} is over 5,000 files or 50 MiB")));
            }
            if files > MAX_FILES || bytes > MAX_BYTES {
                return Err(refused(
                    "the environment delivers more than 2,000 files or 200 MiB",
                ));
            }
            let content = match blobs.get(&f.sha256) {
                Some(b) => b.clone(),
                None => {
                    let b = fetch(&f.sha256)?;
                    if sha256_hex(&b) != f.sha256 || b.len() as u64 != f.size {
                        return Err(refused(format!(
                            "{label}: {}'s SHA-256 doesn't match the delivery document",
                            f.path
                        )));
                    }
                    blobs.insert(f.sha256.clone(), b.clone());
                    b
                }
            };
            if let Ok(text) = std::str::from_utf8(&content) {
                let scan = unicode::scan(text);
                if let Some(c) = scan.critical.first() {
                    return Err(refused(format!("{label}: {}: {c}", f.path)));
                }
                for w in scan.warnings {
                    warnings.push(format!("{label}: {}: {w}", f.path));
                }
            }
        }
    }
    Ok(blobs)
}

/// Each delivered plugin's installed name: its own, or `<source>-<name>`
/// when two share one (D204).
fn plugin_names(d: &Delivery) -> HashMap<EntryAddress, String> {
    let mut count: HashMap<&str, usize> = HashMap::new();
    for e in &d.entries {
        if e.kind == sekisho_catalog::names::EntryKind::Plugin {
            *count.entry(e.name.as_str()).or_default() += 1;
        }
    }
    d.entries
        .iter()
        .filter(|e| e.kind == sekisho_catalog::names::EntryKind::Plugin)
        .map(|e| {
            let name = if count[e.name.as_str()] > 1 {
                format!("{}-{}", e.source, e.name)
            } else {
                e.name.to_string()
            };
            (
                EntryAddress {
                    kind: e.kind,
                    source: e.source.clone(),
                    name: e.name.clone(),
                },
                name,
            )
        })
        .collect()
}

/// An environment's model settings for a tool, kept in the lock.
fn stored(rendered: &Rendered) -> ToolLock {
    ToolLock {
        model_files: rendered
            .files
            .iter()
            .filter(|f| f.origin == Origin::Models)
            .map(|f| StoredFile {
                root: f.root,
                path: f.path.clone(),
                content: B64.encode(&f.content),
                executable: f.executable,
            })
            .collect(),
        model_keys: rendered
            .keys
            .iter()
            .filter(|k| k.origin == Origin::Models)
            .map(|k| StoredKey {
                root: k.root,
                file: k.file.clone(),
                format: k.format,
                key: k.key.clone(),
                value: k.value.clone(),
            })
            .collect(),
        run: rendered.run.clone(),
        always_loaded: rendered.always_loaded,
    }
}

/// Instructions `d` delivers whose names another environment installed
/// for one of `tools` delivers from another entry with other content
/// (D215, D216): the instruction files carry each environment's name, so
/// the engine's path check alone wouldn't see them.
fn instruction_clashes(lock: &Lock, d: &Delivery, tools: &[Tool]) -> Vec<Clash> {
    let env = d.environment.as_str();
    let mut out = Vec::new();
    for e in d
        .entries
        .iter()
        .filter(|e| e.kind == EntryKind::Instruction)
    {
        for (other, held) in &lock.environments {
            if other == env {
                continue;
            }
            let differs = held.entries.iter().any(|h| {
                h.kind == EntryKind::Instruction
                    && h.name == e.name
                    && h.source != e.source
                    && h.digest != e.digest
            });
            let Ok(held_by) = EnvName::parse(other) else {
                continue;
            };
            for &tool in tools.iter().filter(|t| held.tools.contains_key(t)) {
                if differs {
                    out.push(Clash {
                        tool,
                        kind: ContentKind::Instructions,
                        name: e.name.to_string(),
                        path_or_key: format!("instruction {}@{}", e.name, e.source),
                        held_by: held_by.clone(),
                    });
                }
            }
        }
    }
    out
}

/// What a session here also gets from the other scope's install (D214).
fn layered(
    scope: &Scope,
    lock: &Lock,
    adapters: &[&dyn Adapter],
) -> Result<Vec<Layered>, InstallError> {
    let Some(other) = scope.other() else {
        return Ok(Vec::new());
    };
    let theirs = Lock::read(&other.lock_path()).map_err(|e| refused(e.message))?;
    let mut out = Vec::new();
    for a in adapters {
        let tool = a.tool();
        let layering = a.layering();
        let here: BTreeSet<(ContentKind, String)> = names(lock, tool, None);
        for (env, held) in &theirs.environments {
            let also_here = lock
                .environments
                .get(env)
                .is_some_and(|e| e.tools.contains_key(&tool));
            if !held.tools.contains_key(&tool) || also_here {
                continue;
            }
            let Ok(other_env) = EnvName::parse(env) else {
                continue;
            };
            for (kind, name) in names(&theirs, tool, Some(env)) {
                let both = here.contains(&(kind, name.clone()));
                let used = if both {
                    layering
                        .get(kind)
                        .same_name_wins
                        .unwrap_or(ScopeKind::Global)
                } else {
                    ScopeKind::Global
                };
                out.push(Layered {
                    tool,
                    other_env: other_env.clone(),
                    kind,
                    name,
                    here: both,
                    used,
                });
            }
        }
    }
    Ok(out)
}

/// The catalog entries a lock holds for `tool` (from `env` only, if given).
fn names(lock: &Lock, tool: Tool, env: Option<&str>) -> BTreeSet<(ContentKind, String)> {
    let holds = |envs: &[String]| env.is_none_or(|e| envs.iter().any(|x| x == e));
    let origins = lock
        .files
        .iter()
        .filter(|l| l.tool == tool && holds(&l.envs))
        .map(|l| &l.origin)
        .chain(
            lock.keys
                .iter()
                .filter(|l| l.tool == tool && holds(&l.envs))
                .map(|l| &l.origin),
        );
    origins
        .filter_map(|o| match o {
            Origin::Entry(a) => Some((ContentKind::of(a.kind), a.name.to_string())),
            _ => None,
        })
        .collect()
}

pub(super) fn install(
    d: &Delivery,
    adapters: &[&dyn Adapter],
    scope: &Scope,
    options: &Options,
    fetch: Fetch<'_>,
) -> Result<Report, InstallError> {
    let lock = Lock::read(&scope.lock_path()).map_err(|e| refused(e.message))?;
    let mut warnings = Vec::new();
    let blobs = fetch_all(d, fetch, &mut warnings)?;
    for q in &d.quarantined {
        warnings.push(format!(
            "{} {}@{} is quarantined, so it is removed",
            q.kind.as_str(),
            q.name,
            q.source
        ));
    }
    let mut delivery = d.clone();
    delivery.entries.retain(|e| {
        !d.quarantined
            .iter()
            .any(|q| q.kind == e.kind && q.source == e.source && q.name == e.name)
    });
    let names = plugin_names(&delivery);
    let (servers, server_warnings) = super::launch::local_servers(&delivery);
    warnings.extend(server_warnings);
    let sekisho = options.sekisho.as_deref().unwrap_or("sekisho");
    let env = d.environment.to_string();

    let mut p = Planner::new(scope, options, lock);
    let previous = p.lock.environments.get(&env).cloned();
    let order = previous.as_ref().map_or_else(
        || {
            p.lock
                .environments
                .values()
                .map(|e| e.order + 1)
                .max()
                .unwrap_or(0)
        },
        |e| e.order,
    );
    let mut env_lock = EnvLock {
        revision: d.revision.clone(),
        order,
        entries: delivery
            .entries
            .iter()
            .map(|e| EntryLock {
                kind: e.kind,
                source: e.source.clone(),
                name: e.name.clone(),
                commit: e.commit.clone(),
                digest: e.digest.clone(),
            })
            .collect(),
        tools: previous.map(|e| e.tools).unwrap_or_default(),
    };
    for a in adapters {
        let tool = a.tool();
        let current = p
            .lock
            .models
            .get(tool.as_str())
            .filter(|m| {
                p.lock
                    .environments
                    .get(*m)
                    .is_some_and(|e| e.tools.contains_key(&tool))
            })
            .cloned();
        let is_model = options.models || current.is_none() || current.as_deref() == Some(&env);
        let read = |root: Root, rel: &RelPath| -> Option<String> {
            let dir = scope.root(root)?;
            match files::read(&abs(&dir, rel)).ok()? {
                Disk::File(b) => String::from_utf8(b).ok(),
                _ => None,
            }
        };
        let render = |models: bool| {
            let ctx = RenderContext {
                scope,
                blobs: &blobs,
                plugin_names: &names,
                local_servers: &servers,
                models,
                sekisho,
                read: &read,
            };
            a.render(&delivery, &ctx)
                .map_err(|e| refused(format!("{tool}: {e}")))
        };
        let rendered = render(is_model)?;
        let standby = if is_model {
            stored(&rendered)
        } else {
            stored(&render(true)?)
        };
        env_lock.tools.insert(tool, standby);
        for s in &rendered.skipped {
            warnings.push(format!("{tool}: {} skipped: {}", s.what, s.why));
        }
        for w in &rendered.warnings {
            warnings.push(format!("{tool}: {w}"));
        }
        for f in &rendered.files {
            p.want_file(&env, tool, f)?;
        }
        for k in &rendered.keys {
            p.want_key(&env, tool, k)?;
        }
        p.release(&env, tool, |_| true);
        if is_model {
            if let Some(prev) = current.filter(|c| *c != env) {
                p.release(&prev, tool, |o| *o == Origin::Models);
            }
            p.lock.models.insert(tool.as_str().to_owned(), env.clone());
        }
    }
    let tools: Vec<Tool> = adapters.iter().map(|a| a.tool()).collect();
    p.clashes
        .extend(instruction_clashes(&p.lock, &delivery, &tools));
    p.lock.environments.insert(env, env_lock);
    for &tool in &tools {
        let total: u64 = p
            .lock
            .environments
            .values()
            .filter_map(|e| e.tools.get(&tool))
            .map(|t| t.always_loaded)
            .sum();
        if total > ALWAYS_LOADED_WARNING_BYTES {
            warnings.push(format!(
                "{tool}: every session here loads {total} bytes of Sekisho's instructions, \
                 more than one environment may deliver ({ALWAYS_LOADED_WARNING_BYTES}); every \
                 session pays for them (D221)"
            ));
        }
    }
    p.finalize()?;
    if !p.clashes.is_empty() {
        return Err(InstallError::Clash(std::mem::take(&mut p.clashes)));
    }
    if !p.consent.is_empty() && !options.yes && !options.dry_run {
        return Err(InstallError::NeedsConsent(std::mem::take(&mut p.consent)));
    }
    let mut report = p.report(warnings);
    report.layered = layered(scope, &p.lock, adapters)?;
    if options.dry_run {
        return Ok(report);
    }
    let more = p.execute()?;
    report.warnings.extend(more);
    Ok(report)
}

pub(super) fn uninstall(
    scope: &Scope,
    envs: Option<&[EnvName]>,
    tools: Option<&[Tool]>,
    options: &Options,
) -> Result<Report, InstallError> {
    let lock = Lock::read(&scope.lock_path()).map_err(|e| refused(e.message))?;
    let targets: Vec<String> = match envs {
        Some(e) => e.iter().map(ToString::to_string).collect(),
        None => lock.environments.keys().cloned().collect(),
    };
    let tools: Vec<Tool> = match tools {
        Some(t) => t.to_vec(),
        None => {
            let mut all: BTreeSet<Tool> = BTreeSet::new();
            all.extend(
                lock.environments
                    .values()
                    .flat_map(|e| e.tools.keys().copied()),
            );
            all.extend(lock.files.iter().map(|l| l.tool));
            all.extend(lock.keys.iter().map(|l| l.tool));
            all.into_iter().collect()
        }
    };
    let models_before = lock.models.clone();
    let mut p = Planner::new(scope, options, lock);
    for env in &targets {
        for &tool in &tools {
            p.release(env, tool, |_| true);
        }
        if let Some(e) = p.lock.environments.get_mut(env) {
            for tool in &tools {
                e.tools.remove(tool);
            }
            if e.tools.is_empty() {
                p.lock.environments.remove(env);
            }
        }
    }
    // The models go to the earliest environment left for the tool (D216).
    for &tool in &tools {
        let Some(model_env) = models_before.get(tool.as_str()) else {
            continue;
        };
        if !targets.contains(model_env) {
            continue;
        }
        let next = p
            .lock
            .environments
            .iter()
            .filter(|(_, e)| e.tools.contains_key(&tool))
            .min_by_key(|(_, e)| e.order)
            .map(|(name, e)| (name.clone(), e.tools[&tool].clone()));
        match next {
            Some((name, held)) => {
                for f in &held.model_files {
                    let content = B64
                        .decode(&f.content)
                        .map_err(|e| refused(format!("the lock's model file: {e}")))?;
                    p.want_file(
                        &name,
                        tool,
                        &FileWrite {
                            root: f.root,
                            path: f.path.clone(),
                            content,
                            executable: f.executable,
                            origin: Origin::Models,
                        },
                    )?;
                }
                for k in &held.model_keys {
                    p.want_key(
                        &name,
                        tool,
                        &KeyWrite {
                            root: k.root,
                            file: k.file.clone(),
                            format: k.format,
                            key: k.key.clone(),
                            value: k.value.clone(),
                            origin: Origin::Models,
                        },
                    )?;
                }
                p.lock.models.insert(tool.as_str().to_owned(), name);
            }
            None => {
                p.lock.models.remove(tool.as_str());
            }
        }
    }
    p.finalize()?;
    if !p.clashes.is_empty() {
        return Err(InstallError::Clash(std::mem::take(&mut p.clashes)));
    }
    if !p.consent.is_empty() && !options.yes && !options.dry_run {
        return Err(InstallError::NeedsConsent(std::mem::take(&mut p.consent)));
    }
    let mut report = p.report(Vec::new());
    if options.dry_run {
        return Ok(report);
    }
    let more = p.execute()?;
    report.warnings.extend(more);
    Ok(report)
}
