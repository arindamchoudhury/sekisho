//! A `catalog` source: Obot's MCP catalogs, Sekisho's format, A2A cards and
//! the files of an entry (plan 3, Task 2).

use std::fs;
use std::path::{Path, PathBuf};

use sekisho_catalog::card::{CARD_LIMIT, check_card};
use sekisho_catalog::check::{CheckError, CheckOptions, check_source, check_source_with};
use sekisho_catalog::model::{
    A2aAgentSpec, CheckedEntry, ConfigTarget, EntrySpec, McpRuntime, McpServerSpec, SourceCheck,
    ToolkitServer, ToolkitSpec,
};
use sekisho_catalog::names::{EntryKind, EntryName, EntryRef, SourceKind};
use sekisho_catalog::tree::{Limits, TreeError, TreeFile, read_entry_files};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/catalog")
        .join(name)
}

fn check(name: &str) -> SourceCheck {
    check_source(SourceKind::Catalog, &fixture(name)).unwrap()
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

fn server<'a>(check: &'a SourceCheck, name: &str) -> &'a McpServerSpec {
    let e = entry(check, name);
    assert_eq!(e.kind, EntryKind::McpServer);
    match &e.spec {
        EntrySpec::McpServer(s) => s,
        other => panic!("{name} is {other:?}"),
    }
}

fn refusal<'a>(check: &'a SourceCheck, name: &str) -> &'a str {
    &check
        .refused
        .iter()
        .find(|r| r.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("{name} not refused: {check:#?}"))
        .reason
}

/// A catalog source written into a temporary folder.
fn temp_source(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (path, body) in files {
        let path = dir.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    dir
}

fn remote_item(name: &str, url: &str) -> String {
    format!("name: {name}\nruntime: remote\nremoteConfig:\n  fixedURL: {url}\n")
}

#[test]
fn obot_items_become_entries() {
    let c = check("obot");
    assert_eq!(
        names(&c),
        ["fetch", "github-issues", "image-server", "remote-one"]
    );
    assert!(c.refused.is_empty(), "{:#?}", c.refused);

    let gh = server(&c, "github-issues");
    assert_eq!(gh.description, "Issues and pull requests on GitHub.");
    match &gh.runtime {
        McpRuntime::Npx { package, args } => {
            assert_eq!(package.to_string(), "@acme/github-issues@1.2.3");
            assert_eq!(args, &["--stdio"]);
        }
        other => panic!("{other:?}"),
    }
    let token = &gh.config[0];
    assert_eq!(token.key, "GITHUB_TOKEN");
    assert_eq!(token.name, "GitHub token");
    assert!(token.sensitive && token.required);
    assert_eq!(token.value, None);
    assert_eq!(token.target, ConfigTarget::Env);

    let fetch = server(&c, "fetch");
    assert_eq!(fetch.description, "Fetches web pages.");
    match &fetch.runtime {
        McpRuntime::Uvx {
            package,
            command,
            args,
        } => {
            assert_eq!(package.to_string(), "mcp-server-fetch==0.6.2");
            assert_eq!(command.as_deref(), Some("mcp-server-fetch"));
            assert!(args.is_empty());
        }
        other => panic!("{other:?}"),
    }

    match &server(&c, "image-server").runtime {
        McpRuntime::Container {
            image,
            command,
            args,
            port,
            path,
        } => {
            assert_eq!(image.to_string(), "ghcr.io/o/i:1.0");
            assert_eq!(command, &None);
            assert_eq!(args, &["--verbose"]);
            assert_eq!(*port, Some(8080));
            assert_eq!(path.as_deref(), Some("/mcp"));
        }
        other => panic!("{other:?}"),
    }

    let remote = server(&c, "remote-one");
    match &remote.runtime {
        McpRuntime::Remote { url } => assert_eq!(url, "https://mcp.example.com/mcp"),
        other => panic!("{other:?}"),
    }
    assert!(remote.static_oauth);
    assert_eq!(remote.unsupported_tools, ["a", "b"]);
    assert_eq!(remote.tool_preview[0].name, "search");
    assert_eq!(remote.tool_preview[0].description, "Searches.");
    let key = &remote.config[0];
    assert_eq!(key.key, "X-API-KEY");
    assert_eq!(key.target, ConfigTarget::Header);
    assert_eq!(key.prefix.as_deref(), Some("Bearer "));
    // A YAML number is read as its text, as Obot reads it.
    assert_eq!(remote.config[1].value.as_deref(), Some("8080"));

    for e in &c.entries {
        assert_eq!(e.path, "servers.yaml");
        assert!(e.files.is_empty());
    }
}

#[test]
fn obotcatalogs_patterns_choose_the_files() {
    let c = check("patterns");
    // servers/old.yaml is ignored, .hidden is skipped, top.yaml isn't matched
    // by the source's own patterns, and README.md is never read.
    assert_eq!(names(&c), ["deep", "new"]);
    assert!(c.refused.is_empty(), "{:#?}", c.refused);
    assert_eq!(entry(&c, "deep").path, "deep/b.json");

    // Without .obotcatalogs, Obot's defaults: JSON and YAML anywhere.
    let dir = temp_source(&[
        ("a.yaml", &remote_item("a", "https://h/a")),
        ("x/b.yml", &remote_item("b", "https://h/b")),
        (
            "x/y/c.json",
            r#"{"name": "c", "runtime": "remote", "remoteConfig": {"fixedURL": "https://h/c"}}"#,
        ),
        ("d.txt", "not a catalog"),
    ]);
    let c = check_source(SourceKind::Catalog, dir.path()).unwrap();
    assert_eq!(names(&c), ["a", "b", "c"]);
}

#[test]
fn more_than_1000_catalog_files_fail_the_source() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..1000 {
        fs::write(
            dir.path().join(format!("s{i}.yaml")),
            remote_item(&format!("s{i}"), "https://h/r"),
        )
        .unwrap();
    }
    assert_eq!(
        check_source(SourceKind::Catalog, dir.path())
            .unwrap()
            .entries
            .len(),
        1000
    );
    fs::write(
        dir.path().join("one-more.yaml"),
        remote_item("x", "https://h/r"),
    )
    .unwrap();
    match check_source(SourceKind::Catalog, dir.path()) {
        Err(e @ CheckError::TooManyCatalogFiles { .. }) => {
            assert!(e.to_string().contains("1000"), "{e}")
        }
        other => panic!("{other:?}"),
    }
    // A missing folder can't be read at all either.
    assert!(matches!(
        check_source(SourceKind::Catalog, &dir.path().join("nope")),
        Err(CheckError::Missing { .. })
    ));
}

#[test]
fn inexact_and_unsupported_items_are_refused_with_reasons() {
    let c = check("refused");
    assert_eq!(names(&c), ["good"]);
    assert!(refusal(&c, "loose").contains("exact version"));
    assert!(refusal(&c, "openapi-one").contains("openapi"));
    assert!(refusal(&c, "composite-one").contains("composite"));
    assert!(refusal(&c, "templated").contains("urlTemplate"));
    assert!(refusal(&c, "hostnamed").contains("fixedURL"));
    assert!(refusal(&c, "tunnelled").contains("tunnelName"));
    assert!(refusal(&c, "bad-image").contains("image"));
    assert!(refusal(&c, "no-config").contains("uvxConfig"));
    assert!(refusal(&c, "file-config").contains("file"));
    for r in &c.refused {
        assert_eq!(r.path, "items.yaml");
        assert_eq!(r.kind, Some(EntryKind::McpServer));
    }
}

#[test]
fn hosting_fields_are_ignored_and_listed() {
    let c = check("hosting");
    assert_eq!(names(&c), ["hosted"]);
    let messages: Vec<&str> = c.warnings.iter().map(|w| w.message.as_str()).collect();
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert!(
        messages.iter().any(|m| m.contains("egressDomains")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("resources")),
        "{messages:?}"
    );
    assert!(
        messages.iter().all(|m| m.contains("hosted")),
        "{messages:?}"
    );
    assert!(c.warnings.iter().all(|w| w.path == "servers.yaml"));
}

#[test]
fn sensitive_values_in_a_catalog_are_refused() {
    let c = check("sensitive");
    assert_eq!(names(&c), ["asks"]);
    let why = refusal(&c, "leaky");
    assert!(why.contains("sensitive"), "{why}");
    assert!(!why.contains("sk-not-a-real-secret"), "{why}");
}

#[test]
fn duplicate_names_are_both_refused() {
    let c = check("duplicates");
    // The A2A agent named dup is another kind, so it stays.
    let kinds: Vec<_> = c
        .entries
        .iter()
        .map(|e| (e.name.as_str(), e.kind))
        .collect();
    assert_eq!(kinds.len(), 2, "{kinds:?}");
    assert!(kinds.contains(&("unique", EntryKind::McpServer)));
    assert!(kinds.contains(&("dup", EntryKind::A2aAgent)));
    let dups: Vec<_> = c
        .refused
        .iter()
        .filter(|r| r.name.as_deref() == Some("dup"))
        .collect();
    assert_eq!(dups.len(), 2, "{:#?}", c.refused);
    assert!(dups.iter().all(|r| r.kind == Some(EntryKind::McpServer)));
}

#[test]
fn vmcp_items_become_offers_with_differences() {
    let c = check("vmcp");
    assert_eq!(names(&c), ["a", "b"]);
    assert_eq!(c.vmcps.len(), 2);

    let team = c
        .vmcps
        .iter()
        .find(|v| v.key.as_str() == "team-tools")
        .unwrap();
    assert_eq!(team.display_name, "Team Tools");
    assert_eq!(team.description, "The team's servers.");
    let parts: Vec<_> = team
        .components
        .iter()
        .map(|p| (p.server.as_str(), p.tools.clone()))
        .collect();
    assert_eq!(
        parts,
        [
            ("a", None),
            ("b", Some(vec!["x".to_string(), "y".to_string()]))
        ]
    );
    let diffs = team.differences.join("\n");
    assert!(diffs.contains("why"), "{diffs}");
    assert!(diffs.contains("profiles"), "{diffs}");
    assert!(diffs.contains("other::c"), "{diffs}");
    assert_eq!(team.differences.len(), 3, "{diffs}");

    // No entryKey: the key comes from the display name.
    let plain = c
        .vmcps
        .iter()
        .find(|v| v.key.as_str() == "plain-set")
        .unwrap();
    assert!(plain.differences.is_empty());
}

#[test]
fn sekisho_format_has_agents_and_servers() {
    let c = check("sekisho");
    let mut kinds: Vec<_> = c
        .entries
        .iter()
        .map(|e| (e.name.as_str(), e.kind))
        .collect();
    kinds.sort();
    assert_eq!(
        kinds,
        [
            ("docs", EntryKind::McpServer),
            ("local", EntryKind::McpServer),
            ("planner", EntryKind::A2aAgent)
        ]
    );

    match &entry(&c, "planner").spec {
        EntrySpec::A2aAgent(A2aAgentSpec {
            description,
            url,
            card_url,
            card,
        }) => {
            assert_eq!(description, "Plans work.");
            assert_eq!(url, "https://agents.example.com/planner");
            assert_eq!(
                card_url.as_deref(),
                Some("https://agents.example.com/planner/card.json")
            );
            assert!(
                card.is_none(),
                "cards are fetched at sync, not read from Git"
            );
        }
        other => panic!("{other:?}"),
    }
    match &server(&c, "docs").runtime {
        McpRuntime::Remote { url } => assert_eq!(url, "https://mcp.example.com/docs?team=a"),
        other => panic!("{other:?}"),
    }
    let local = server(&c, "local");
    match &local.runtime {
        McpRuntime::Container { image, .. } => {
            assert_eq!(image.to_string(), "registry-1.docker.io/library/nginx:1.27")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(local.config[0].key, "LOG_LEVEL");
    assert_eq!(local.config[0].value.as_deref(), Some("debug"));

    assert!(refusal(&c, "coloured").contains("colour"));
    assert!(refusal(&c, "two").contains("exactly one"));
    assert!(refusal(&c, "odd").contains("Widget"));
    assert!(refusal(&c, "future").contains("sekisho/v2"));
}

#[test]
fn urls_must_be_https() {
    let dir = temp_source(&[
        (
            "plain.yaml",
            &remote_item("plain", "http://mcp.example.com/p"),
        ),
        (
            "userinfo.yaml",
            &remote_item("userinfo", "https://u:p@mcp.example.com/u"),
        ),
        (
            "fragment.yaml",
            &remote_item("fragment", "https://mcp.example.com/f#x"),
        ),
        ("ssh.yaml", &remote_item("ssh", "ssh://mcp.example.com/s")),
        (
            "agent.yaml",
            "apiVersion: sekisho/v1alpha1\nkind: A2AAgent\nname: agent\nurl: http://agents.example.com/a\n",
        ),
        (
            "card.yaml",
            "apiVersion: sekisho/v1alpha1\nkind: A2AAgent\nname: card\nurl: https://agents.example.com/c\ncard_url: http://agents.example.com/c.json\n",
        ),
    ]);
    let strict = check_source(SourceKind::Catalog, dir.path()).unwrap();
    assert!(strict.entries.is_empty(), "{:#?}", strict.entries);
    assert!(refusal(&strict, "plain").contains("https"));
    assert!(!refusal(&strict, "userinfo").contains("u:p"));

    let lax = check_source_with(
        SourceKind::Catalog,
        dir.path(),
        &CheckOptions {
            allow_plain_http: true,
        },
    )
    .unwrap();
    assert_eq!(names(&lax), ["agent", "card", "plain"]);
    for still in ["userinfo", "fragment", "ssh"] {
        refusal(&lax, still);
    }
}

#[test]
fn critical_characters_refuse_the_items_of_a_file() {
    let bad = format!(
        "- {}description: \"ok\u{202E}txt\"\n- {}",
        remote_item("one", "https://h/1").replace('\n', "\n  "),
        remote_item("two", "https://h/2").replace('\n', "\n  "),
    );
    let warn = format!(
        "{}description: a\u{200B}b\n",
        remote_item("three", "https://h/3")
    );
    let dir = temp_source(&[
        ("bad.yaml", &bad),
        ("warn.yaml", &warn),
        ("fine.yaml", &remote_item("four", "https://h/4")),
    ]);
    let c = check_source(SourceKind::Catalog, dir.path()).unwrap();
    assert_eq!(names(&c), ["four", "three"]);
    let refused: Vec<_> = c.refused.iter().filter(|r| r.path == "bad.yaml").collect();
    assert_eq!(refused.len(), 2, "{:#?}", c.refused);
    assert!(
        refused.iter().all(|r| r.reason.contains("U+202E")),
        "{refused:#?}"
    );
    assert!(
        c.warnings
            .iter()
            .any(|w| w.path == "warn.yaml" && w.message.contains("U+200B")),
        "{:#?}",
        c.warnings
    );
}

/// Makes a symlink, or says why the test can't (Windows without the
/// privilege to make one).
fn try_symlink(target: &Path, link: &Path, dir: bool) -> bool {
    #[cfg(unix)]
    let made = {
        let _ = dir;
        std::os::unix::fs::symlink(target, link)
    };
    #[cfg(windows)]
    let made = if dir {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    };
    match made {
        Ok(()) => true,
        Err(e) => {
            eprintln!("skipping the symlink case: can't make a symlink here ({e})");
            false
        }
    }
}

#[test]
fn symlinks_and_escaping_paths_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("checkout");
    let entry = root.join("skills/pdf");
    fs::create_dir_all(entry.join("scripts")).unwrap();
    fs::create_dir_all(entry.join(".git")).unwrap();
    fs::write(entry.join("SKILL.md"), "---\nname: pdf\n---\n").unwrap();
    fs::write(entry.join("scripts/run.sh"), "echo hi\n").unwrap();
    fs::write(entry.join(".git/config"), "never read").unwrap();
    fs::write(tmp.path().join("secret.txt"), "outside").unwrap();

    let files = read_entry_files(&root, &entry, &Limits::DEFAULT).unwrap();
    let paths: Vec<_> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["SKILL.md", "scripts/run.sh"]);
    assert_eq!(files[1].bytes, b"echo hi\n");

    // A folder named through `..` that leaves the checkout.
    let escape = root.join("skills/../../");
    match read_entry_files(&root, &escape, &Limits::DEFAULT) {
        Err(e @ TreeError::Escapes { .. }) => assert!(e.to_string().contains(".."), "{e}"),
        other => panic!("{other:?}"),
    }
    assert!(read_entry_files(&root, tmp.path(), &Limits::DEFAULT).is_err());

    // A symlink to the folder above, and one to a file outside.
    if try_symlink(&root, &entry.join("up"), true) {
        match read_entry_files(&root, &entry, &Limits::DEFAULT) {
            Err(e @ TreeError::Symlink { .. }) => assert!(e.to_string().contains("up"), "{e}"),
            other => panic!("{other:?}"),
        }
        fs::remove_dir(entry.join("up"))
            .or_else(|_| fs::remove_file(entry.join("up")))
            .unwrap();
    }
    if try_symlink(
        &tmp.path().join("secret.txt"),
        &entry.join("scripts/key"),
        false,
    ) {
        match read_entry_files(&root, &entry, &Limits::DEFAULT) {
            Err(e @ TreeError::Symlink { .. }) => {
                assert!(e.to_string().contains("scripts/key"), "{e}")
            }
            other => panic!("{other:?}"),
        }
        fs::remove_file(entry.join("scripts/key")).unwrap();
    }
    // The entry's own folder may not be a symlink either.
    if try_symlink(&entry, &root.join("skills/alias"), true) {
        assert!(matches!(
            read_entry_files(&root, &root.join("skills/alias"), &Limits::DEFAULT),
            Err(TreeError::Symlink { .. })
        ));
    }
}

#[test]
fn entry_limits_are_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let entry = tmp.path().join("e");
    fs::create_dir_all(&entry).unwrap();
    fs::write(entry.join("a"), [0u8; 10]).unwrap();
    fs::write(entry.join("b"), [0u8; 10]).unwrap();
    let limits = |file, bytes, files| Limits {
        file_bytes: file,
        entry_bytes: bytes,
        entry_files: files,
        ..Limits::DEFAULT
    };
    assert!(read_entry_files(tmp.path(), &entry, &limits(10, 20, 2)).is_ok());
    assert!(matches!(
        read_entry_files(tmp.path(), &entry, &limits(9, 20, 2)),
        Err(TreeError::TooBig { .. })
    ));
    assert!(matches!(
        read_entry_files(tmp.path(), &entry, &limits(10, 19, 2)),
        Err(TreeError::TooBig { .. })
    ));
    assert!(matches!(
        read_entry_files(tmp.path(), &entry, &limits(10, 20, 1)),
        Err(TreeError::TooMany { .. })
    ));
    assert_eq!(Limits::DEFAULT.file_bytes, 10 << 20);
    assert_eq!(Limits::DEFAULT.entry_bytes, 50 << 20);
    assert_eq!(Limits::DEFAULT.entry_files, 5_000);
    assert_eq!(Limits::DEFAULT.skill_md_bytes, 1 << 20);
    assert_eq!(Limits::DEFAULT.catalog_files, 1_000);
    assert_eq!(Limits::DEFAULT.manifest_bytes, 5 << 20);
}

#[test]
fn cards_are_checked() {
    let v1 = br#"{"name": "Planner", "description": "Plans.",
        "supportedInterfaces": [
            {"url": "https://a.example/rpc", "protocolBinding": "JSONRPC", "protocolVersion": "1.0"},
            {"url": "https://a.example/grpc", "protocolBinding": "GRPC", "protocolVersion": "1.0"}
        ]}"#;
    let card = check_card(v1).unwrap();
    assert_eq!(card.versions, ["1.0"]);
    assert_eq!(card.bindings, ["JSONRPC", "GRPC"]);
    assert_eq!(card.json["name"], "Planner");

    let v03 = br#"{"name": "Old", "url": "https://a.example/a2a", "protocolVersion": "0.3.0",
        "preferredTransport": "JSONRPC",
        "additionalInterfaces": [{"url": "https://a.example/rest", "transport": "HTTP+JSON"}]}"#;
    let card = check_card(v03).unwrap();
    assert_eq!(card.versions, ["0.3.0"]);
    assert_eq!(card.bindings, ["JSONRPC", "HTTP+JSON"]);
    // v0.3 defaults to JSON-RPC when no transport is named.
    let bare = check_card(br#"{"name": "Bare", "url": "https://a.example/a2a"}"#).unwrap();
    assert_eq!(bare.bindings, ["JSONRPC"]);

    for bad in [
        &br#"{"description": "no name", "url": "https://a.example"}"#[..],
        br#"{"name": "", "url": "https://a.example"}"#,
        br#"[{"name": "x"}]"#,
        br#"{"name": "x"}"#,
        br#"{"name": "x", "supportedInterfaces": []}"#,
        br#"{"name": "x", "supportedInterfaces": [{"protocolBinding": "JSONRPC"}]}"#,
        br#"not json"#,
        b"\xff\xfe",
    ] {
        assert!(check_card(bad).is_err(), "{}", String::from_utf8_lossy(bad));
    }

    let mut big = br#"{"name": "x", "url": "https://a.example", "pad": ""#.to_vec();
    big.resize(CARD_LIMIT, b'a');
    big.extend_from_slice(b"\"}");
    assert!(check_card(&big).unwrap_err().contains("1 MiB"));
    assert_eq!(CARD_LIMIT, 1 << 20);

    let hidden =
        "{\"name\": \"x\", \"url\": \"https://a.example\", \"description\": \"a\u{202E}b\"}";
    assert!(
        check_card(hidden.as_bytes())
            .unwrap_err()
            .contains("U+202E")
    );
}

fn sample(files: Vec<TreeFile>, description: &str) -> CheckedEntry {
    CheckedEntry {
        kind: EntryKind::A2aAgent,
        name: EntryName::parse("planner").unwrap(),
        path: "agents.yaml".into(),
        spec: EntrySpec::A2aAgent(A2aAgentSpec {
            description: description.into(),
            url: "https://agents.example.com/planner".into(),
            card_url: None,
            card: None,
        }),
        files,
    }
}

fn file(path: &str, bytes: &[u8], executable: bool) -> TreeFile {
    TreeFile {
        path: path.into(),
        bytes: bytes.to_vec(),
        executable,
    }
}

#[test]
fn the_digest_follows_spec_and_files() {
    let a = sample(vec![file("a", b"1", false), file("b/c", b"2", true)], "d");
    let same = sample(vec![file("b/c", b"2", true), file("a", b"1", false)], "d");
    let digest = a.digest();
    assert_eq!(digest.len(), 64);
    assert!(
        digest
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
    assert_eq!(digest, same.digest(), "file order is irrelevant");
    assert_eq!(digest, a.clone().digest(), "same input, same digest");

    for changed in [
        sample(vec![file("a", b"3", false), file("b/c", b"2", true)], "d"),
        sample(vec![file("a", b"1", false), file("b/c", b"2", false)], "d"),
        sample(vec![file("A", b"1", false), file("b/c", b"2", true)], "d"),
        sample(vec![file("a", b"1", false)], "d"),
        sample(vec![file("a", b"1", false), file("b/c", b"2", true)], "e"),
    ] {
        assert_ne!(digest, changed.digest(), "{changed:?}");
    }
}

fn reference(s: &str) -> EntryRef {
    EntryRef::parse(s).unwrap()
}

fn toolkit_item(name: &str, body: &str) -> String {
    format!(
        "apiVersion: sekisho/v1alpha1
kind: Toolkit
name: {name}
{body}"
    )
}

#[test]
fn toolkits_list_entries_of_every_kind() {
    let c = check("toolkits");
    assert!(c.refused.is_empty(), "{:#?}", c.refused);
    assert_eq!(names(&c), ["aws-cost-explorer", "cost-analyst", "finops"]);
    let finops = entry(&c, "finops");
    assert_eq!(finops.kind, EntryKind::Toolkit);
    assert_eq!(finops.path, "finops.yaml");
    assert!(finops.files.is_empty());
    assert_eq!(
        finops.spec,
        EntrySpec::Toolkit(ToolkitSpec {
            description: "Cloud cost questions and reports.".into(),
            mcp_servers: vec![
                ToolkitServer {
                    server: reference("aws-cost-explorer"),
                    tools: None,
                },
                ToolkitServer {
                    server: reference("github"),
                    tools: Some(vec!["search_issues".into(), "get_issue".into()]),
                },
            ],
            a2a_agents: vec![reference("cost-analyst")],
            skills: vec![reference("finops-report@team-skills")],
            plugins: vec![reference("focus-export")],
            prompts: vec![reference("monthly-review")],
        })
    );
    // The spec is stored as JSON and read back unchanged.
    let json = serde_json::to_value(&finops.spec).unwrap();
    assert_eq!(json["kind"], "toolkit");
    assert_eq!(
        json["skills"],
        serde_json::json!(["finops-report@team-skills"])
    );
    assert_eq!(
        json["mcp_servers"][1],
        serde_json::json!({"server": "github", "tools": ["search_issues", "get_issue"]})
    );
    let back: EntrySpec = serde_json::from_value(json).unwrap();
    assert_eq!(&back, &finops.spec);
}

#[test]
fn toolkits_cannot_hold_models_or_toolkits() {
    let dir = temp_source(&[
        (
            "models.yaml",
            &toolkit_item(
                "with-models",
                "skills: [pdf]
models: [claude-sonnet]
",
            ),
        ),
        (
            "nested.yaml",
            &toolkit_item(
                "with-toolkits",
                "skills: [pdf]
toolkits: [finops]
",
            ),
        ),
        (
            "other.yaml",
            &toolkit_item(
                "with-colour",
                "skills: [pdf]
colour: red
",
            ),
        ),
    ]);
    let c = check_source(SourceKind::Catalog, dir.path()).unwrap();
    assert!(c.entries.is_empty(), "{:#?}", c.entries);
    let models = refusal(&c, "with-models");
    assert!(models.contains("models"), "{models}");
    assert!(models.contains("environments"), "{models}");
    let nested = refusal(&c, "with-toolkits");
    assert!(nested.contains("toolkits"), "{nested}");
    assert!(nested.contains("can't hold toolkits"), "{nested}");
    let other = refusal(&c, "with-colour");
    assert!(other.contains("colour"), "{other}");
    for r in &c.refused {
        assert_eq!(r.kind, Some(EntryKind::Toolkit), "{r:?}");
    }
}

#[test]
fn empty_and_repeating_toolkits_are_refused() {
    let items = [
        toolkit_item(
            "empty",
            "description: Nothing.
",
        ),
        toolkit_item(
            "empty-lists",
            "skills: []
mcp_servers: []
",
        ),
        toolkit_item(
            "twice",
            "skills: [pdf, pdf]
",
        ),
        toolkit_item(
            "servers-twice",
            "mcp_servers: [github, {server: github, tools: [a]}]
",
        ),
        toolkit_item(
            "tools",
            "mcp_servers:
  - server: a
    tools: []
  - server: b
    tools: [x, x]
  - server: c
    tools: [\"\"]
",
        ),
        toolkit_item(
            "bad-reference",
            "skills: [Not_A_Name]
",
        ),
        toolkit_item(
            "bad-server",
            "mcp_servers: [{server: a, scope: x}]
",
        ),
        toolkit_item(
            "fine",
            "skills: [pdf, pdf@other]
",
        ),
    ];
    let files: Vec<(String, String)> = items
        .iter()
        .enumerate()
        .map(|(i, body)| (format!("kit{i}.yaml"), body.clone()))
        .collect();
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_str()))
        .collect();
    let dir = temp_source(&files);
    let c = check_source(SourceKind::Catalog, dir.path()).unwrap();
    assert_eq!(names(&c), ["fine"], "{:#?}", c.refused);
    assert!(refusal(&c, "empty").contains("at least one"));
    assert!(refusal(&c, "empty-lists").contains("at least one"));
    assert!(refusal(&c, "twice").contains("skills[1]: is listed twice"));
    assert!(refusal(&c, "servers-twice").contains("mcp_servers[1]: is listed twice"));
    let tools = refusal(&c, "tools");
    assert!(
        tools.contains("mcp_servers[0].tools: give at least one tool"),
        "{tools}"
    );
    assert!(
        tools.contains("mcp_servers[1].tools[1]: is listed twice"),
        "{tools}"
    );
    assert!(
        tools.contains("mcp_servers[2].tools[0]: a tool's name can't be empty"),
        "{tools}"
    );
    assert!(refusal(&c, "bad-reference").contains("skills[0]"));
    assert!(refusal(&c, "bad-server").contains("mcp_servers[0].scope"));
}

#[test]
fn fixed_values_cannot_steer_package_managers() {
    let item = |name: &str, key: &str, value: Option<&str>| {
        let value = value.map_or(String::new(), |v| format!("\n      value: {v}"));
        format!(
            "- entryKey: {name}\n  name: {name}\n  runtime: npx\n  npxConfig:\n    package: pkg@1.2.3\n  config:\n    - name: {key}\n      key: {key}\n      usage: env{value}\n"
        )
    };
    let files = [
        item(
            "steers",
            "NPM_CONFIG_REGISTRY",
            Some("https://evil.example"),
        ),
        item("lower", "npm-config-registry", Some("https://evil.example")),
        item("uv", "UV_INDEX_URL", Some("https://evil.example")),
        item("node", "NODE_OPTIONS", Some("--require=/tmp/x.js")),
        // A person may give their own; only a value in the catalog is refused.
        item("asks", "NPM_CONFIG_REGISTRY", None),
        item("plain", "LOG_LEVEL", Some("debug")),
    ];
    let dir = temp_source(&[("catalog.yaml", &files.concat())]);
    let c = check_source(SourceKind::Catalog, dir.path()).unwrap();
    assert_eq!(names(&c), ["asks", "plain"], "{:#?}", c.refused);
    for name in ["steers", "lower", "uv", "node"] {
        let why = refusal(&c, name);
        assert!(why.contains("can't be set by a catalog"), "{name}: {why}");
    }
}
