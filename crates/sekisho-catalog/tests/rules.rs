//! The catalog library's rules (plan 3, Task 1).

use std::collections::BTreeMap;
use std::path::Path;

use sekisho_catalog::labels::Labels;
use sekisho_catalog::names::{EntryKind, EntryName, EntryRef, SourceKind, SourceName};
use sekisho_catalog::pins::{ImageRef, NpmPackage, PythonPackage};
use sekisho_catalog::source::{Folder, GitRef, SourceUrl};
use sekisho_catalog::unicode::scan;

#[test]
fn entry_names_follow_the_agent_skills_rule() {
    for good in ["pdf", "code-review", "a1-b2-c3", "0", &"a".repeat(64)] {
        assert_eq!(EntryName::parse(good).unwrap().as_str(), good);
    }
    for bad in [
        "",
        &"a".repeat(65),
        "Pdf",
        "-a",
        "a-",
        "a--b",
        "a_b",
        "a.b",
        "a b",
        "é",
    ] {
        assert!(EntryName::parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn references_name_an_entry_and_maybe_a_source() {
    let bare = EntryRef::parse("pdf").unwrap();
    assert_eq!(bare.name.as_str(), "pdf");
    assert_eq!(bare.source, None);
    assert_eq!(bare.to_string(), "pdf");

    let qualified = EntryRef::parse("pdf@team-skills").unwrap();
    assert_eq!(qualified.name.as_str(), "pdf");
    assert_eq!(qualified.source.as_ref().unwrap().as_str(), "team-skills");
    assert_eq!(qualified.to_string(), "pdf@team-skills");
    assert_eq!(EntryRef::parse(&qualified.to_string()).unwrap(), qualified);

    for bad in ["pdf@", "@x", "a@b@c", "", "@", "Pdf@x", "pdf@X"] {
        assert!(EntryRef::parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn environments_is_reserved_for_git_mode() {
    assert!(SourceName::parse("team-skills").is_ok());
    let refused = SourceName::parse("environments").unwrap_err().to_string();
    assert!(refused.contains("reserved"), "{refused}");
    assert_eq!(SourceName::git_mode().as_str(), "environments");
    // A reference may still name Git mode's source.
    let r = EntryRef::parse("pdf@environments");
    assert!(r.is_err(), "Git mode's source holds no entries");
    for bad in ["", "Team", "-x", "x-", "a_b", &"a".repeat(64)] {
        assert!(SourceName::parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn kinds_have_names_and_path_segments() {
    let all = [
        (EntryKind::McpServer, "mcp_server", "mcp-servers"),
        (EntryKind::A2aAgent, "a2a_agent", "a2a-agents"),
        (EntryKind::Skill, "skill", "skills"),
        (EntryKind::Plugin, "plugin", "plugins"),
        (EntryKind::Prompt, "prompt", "prompts"),
        (EntryKind::Toolkit, "toolkit", "toolkits"),
        (EntryKind::Instruction, "instruction", "instructions"),
    ];
    assert_eq!(EntryKind::ALL.len(), all.len());
    for (kind, name, segment) in all {
        assert_eq!(kind.as_str(), name);
        assert_eq!(kind.path_segment(), segment);
        assert_eq!(EntryKind::from_path_segment(segment), Some(kind));
        assert_eq!(EntryKind::parse(name), Some(kind));
    }
    assert_eq!(EntryKind::from_path_segment("mcp_server"), None);

    assert_eq!(
        SourceKind::Catalog.entry_kinds(),
        &[
            EntryKind::McpServer,
            EntryKind::A2aAgent,
            EntryKind::Toolkit
        ]
    );
    assert_eq!(SourceKind::Skills.entry_kinds(), &[EntryKind::Skill]);
    assert_eq!(SourceKind::Plugins.entry_kinds(), &[EntryKind::Plugin]);
    assert_eq!(SourceKind::Prompts.entry_kinds(), &[EntryKind::Prompt]);
    assert_eq!(
        SourceKind::Instructions.entry_kinds(),
        &[EntryKind::Instruction]
    );
    assert!(SourceKind::Environments.entry_kinds().is_empty());
    for (kind, name) in [
        (SourceKind::Catalog, "catalog"),
        (SourceKind::Skills, "skills"),
        (SourceKind::Plugins, "plugins"),
        (SourceKind::Prompts, "prompts"),
        (SourceKind::Instructions, "instructions"),
        (SourceKind::Environments, "environments"),
    ] {
        assert_eq!(kind.as_str(), name);
        assert_eq!(SourceKind::parse(name), Some(kind));
    }
    assert_eq!(SourceKind::parse("Catalog"), None);
}

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn refused_fields(map: BTreeMap<String, String>) -> Vec<String> {
    Labels::new(map)
        .unwrap_err()
        .into_iter()
        .map(|(field, _)| field)
        .collect()
}

#[test]
fn labels_follow_the_rules() {
    let ok = Labels::new(labels(&[("cost-centre", "4711"), ("team.a_b", "x y")])).unwrap();
    assert_eq!(ok.as_map()["cost-centre"], "4711");
    assert!(Labels::new(BTreeMap::new()).unwrap().as_map().is_empty());

    let many: BTreeMap<_, _> = (0..33)
        .map(|i| (format!("k{i}"), "v".to_string()))
        .collect();
    assert_eq!(refused_fields(many), ["labels"]);
    let enough: BTreeMap<_, _> = (0..32)
        .map(|i| (format!("k{i}"), "v".to_string()))
        .collect();
    assert!(Labels::new(enough).is_ok());

    for key in [
        "Cost",
        "sekisho-env",
        "sekisho",
        "-a",
        "a-",
        "",
        "a b",
        &"a".repeat(64),
    ] {
        assert_eq!(
            refused_fields(labels(&[(key, "v")])),
            [format!("labels.{key}")],
            "{key:?}"
        );
    }
    assert!(Labels::new(labels(&[(&"a".repeat(63), "v")])).is_ok());

    for value in ["", " x", "x ", "a\u{7}", "a\nb", &"é".repeat(257)] {
        assert_eq!(
            refused_fields(labels(&[("k", value)])),
            ["labels.k"],
            "{value:?}"
        );
    }
    assert!(Labels::new(labels(&[("k", &"é".repeat(256))])).is_ok());

    // Every bad label is named, not only the first.
    let both = refused_fields(labels(&[("Bad", "v"), ("good", ""), ("fine", "v")]));
    assert_eq!(both, ["labels.Bad", "labels.good"]);
}

fn yaml(s: &str) -> serde_yaml_ng::Value {
    serde_yaml_ng::from_str(s).unwrap()
}

#[test]
fn yaml_scalars_become_label_text() {
    let l = Labels::from_yaml(&yaml(
        "cost-centre: 4711\ncritical: true\nratio: 0.5\nname: x",
    ))
    .unwrap();
    assert_eq!(
        l.as_map(),
        &labels(&[
            ("cost-centre", "4711"),
            ("critical", "true"),
            ("ratio", "0.5"),
            ("name", "x")
        ])
    );
    assert!(Labels::from_yaml(&yaml("~")).unwrap().as_map().is_empty());

    for (doc, field) in [
        ("a: {b: c}", "labels.a"),
        ("a: [b]", "labels.a"),
        ("a: ~", "labels.a"),
        ("[a, b]", "labels"),
        ("text", "labels"),
    ] {
        let fields: Vec<String> = Labels::from_yaml(&yaml(doc))
            .unwrap_err()
            .into_iter()
            .map(|(f, _)| f)
            .collect();
        assert_eq!(fields, [field], "{doc}");
    }
}

#[test]
fn npm_packages_need_one_exact_version() {
    let scoped = NpmPackage::parse_exact("@modelcontextprotocol/server-github@2025.4.8").unwrap();
    assert_eq!(scoped.name(), "@modelcontextprotocol/server-github");
    assert_eq!(scoped.version().to_string(), "2025.4.8");
    assert_eq!(
        scoped.to_string(),
        "@modelcontextprotocol/server-github@2025.4.8"
    );
    let pre = NpmPackage::parse_exact("pkg@1.2.3-beta.1").unwrap();
    assert_eq!(pre.name(), "pkg");
    assert_eq!(pre.to_string(), "pkg@1.2.3-beta.1");

    for bad in [
        "pkg",
        "pkg@latest",
        "pkg@^1.2.3",
        "pkg@~1.2.3",
        "pkg@1.x",
        "pkg@*",
        "pkg@>=1",
        "pkg@1.2.3 || 2.0.0",
        "pkg@1.2",
        "pkg@1.2.3+build.1",
        "git+https://x",
        "file:../x",
        "https://x/y.tgz",
        "github:a/b",
        "./x",
        "@scope@1.0.0",
        "@scope/@1.0.0",
        "Pkg@1.0.0",
        "@1.0.0",
        "",
    ] {
        assert!(NpmPackage::parse_exact(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn python_packages_need_one_exact_version() {
    let a = PythonPackage::parse_exact("mcp-server-fetch==0.6.2").unwrap();
    assert_eq!(a.name(), "mcp-server-fetch");
    assert_eq!(a.version(), "0.6.2");
    let b = PythonPackage::parse_exact("mcp-server-fetch@0.6.2").unwrap();
    assert_eq!(b, a);
    assert_eq!(b.to_string(), "mcp-server-fetch==0.6.2");
    assert!(PythonPackage::parse_exact("pkg==1.0.0rc1").is_ok());
    assert!(PythonPackage::parse_exact("Py_Pkg.x==1!2.0.post1+local").is_ok());

    for bad in [
        "pkg",
        "pkg>=1",
        "pkg~=1.0",
        "pkg===1",
        "pkg==1.*",
        "pkg==1,<2",
        "pkg!=1",
        "pkg<2",
        "pkg==",
        "pkg==v1",
        "git+https://x",
        "pkg @ https://x",
        "pkg@https://x",
        "pkg[cli]==1.0",
        "pkg==1.0; python_version<'3.9'",
        "-pkg==1.0",
        "==1.0",
        "",
    ] {
        assert!(PythonPackage::parse_exact(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn image_references_normalise_like_docker() {
    let hex = "a".repeat(64);

    let nginx = ImageRef::parse("nginx:1.27").unwrap();
    assert_eq!(nginx.registry(), "registry-1.docker.io");
    assert_eq!(nginx.repository(), "library/nginx");
    assert_eq!(nginx.tag(), Some("1.27"));
    assert_eq!(nginx.digest(), None);
    assert_eq!(nginx.to_string(), "registry-1.docker.io/library/nginx:1.27");

    let user = ImageRef::parse("mcp/github").unwrap();
    assert_eq!(user.registry(), "registry-1.docker.io");
    assert_eq!(user.repository(), "mcp/github");
    let hub = ImageRef::parse("docker.io/nginx").unwrap();
    assert_eq!(hub.repository(), "library/nginx");
    assert_eq!(hub.registry(), "registry-1.docker.io");

    let pinned = ImageRef::parse(&format!("ghcr.io/a/b@sha256:{hex}")).unwrap();
    assert_eq!(pinned.registry(), "ghcr.io");
    assert_eq!(pinned.repository(), "a/b");
    assert_eq!(pinned.tag(), None);
    assert_eq!(pinned.digest(), Some(format!("sha256:{hex}").as_str()));

    let both = ImageRef::parse(&format!("ghcr.io/a/b:v1@sha256:{hex}")).unwrap();
    assert_eq!(both.tag(), Some("v1"));
    assert_eq!(both.to_string(), format!("ghcr.io/a/b:v1@sha256:{hex}"));

    let local = ImageRef::parse("localhost:5000/x:1").unwrap();
    assert_eq!(local.registry(), "localhost:5000");
    assert_eq!(local.repository(), "x");
    assert_eq!(local.tag(), Some("1"));
    assert_eq!(
        ImageRef::parse("localhost/x").unwrap().registry(),
        "localhost"
    );

    let bare = ImageRef::parse("x").unwrap();
    assert_eq!(bare.tag(), None);
    assert_eq!(bare.to_string(), "registry-1.docker.io/library/x");

    let resolved = nginx.with_digest(format!("sha256:{hex}")).unwrap();
    assert_eq!(resolved.tag(), Some("1.27"));
    assert_eq!(resolved.digest(), Some(format!("sha256:{hex}").as_str()));
    assert!(nginx.with_digest("sha256:xyz".into()).is_err());

    for bad in [
        "x:Bad Tag".to_string(),
        format!("x@sha256:{}", "a".repeat(63)),
        format!("x@sha256:{}", "A".repeat(64)),
        format!("x@sha512:{hex}"),
        "X".into(),
        "".into(),
        "x:".into(),
        "x/".into(),
        "/x".into(),
        "-x".into(),
        "a//b".into(),
        "a/B".into(),
        format!("x:{}", "t".repeat(129)),
        "ghcr.io/a b".into(),
    ] {
        assert!(ImageRef::parse(&bad).is_err(), "{bad:?}");
    }
}

#[test]
fn critical_characters_are_found_with_their_place() {
    let s = scan("ok\nab\u{202E}c");
    assert_eq!(s.critical.len(), 1);
    assert_eq!(s.critical[0].line, 2);
    assert_eq!(s.critical[0].column, 3);
    assert_eq!(s.critical[0].codepoint, '\u{202E}');
    assert!(!s.critical[0].what.is_empty());
    assert!(s.warnings.is_empty());

    for c in [
        '\u{202A}',
        '\u{2066}',
        '\u{2069}',
        '\u{E0001}',
        '\u{E0041}',
        '\u{E007F}',
        '\u{E0100}',
        '\u{E01EF}',
    ] {
        let s = scan(&format!("x{c}"));
        assert_eq!(s.critical.len(), 1, "U+{:04X}", c as u32);
        assert!(s.warnings.is_empty());
    }
    for c in [
        '\u{200B}', '\u{200F}', '\u{2060}', '\u{2064}', '\u{206A}', '\u{206F}', '\u{00AD}',
        '\u{061C}', '\u{FE00}', '\u{FE0E}', '\u{FFF9}', '\u{FFFB}',
    ] {
        let s = scan(&format!("x{c}"));
        assert!(s.critical.is_empty(), "U+{:04X}", c as u32);
        assert_eq!(s.warnings.len(), 1, "U+{:04X}", c as u32);
    }
    // Emoji presentation, ordinary text and the neighbours of each range are clean.
    for c in [
        '\u{FE0F}',
        '\u{2065}',
        '\u{2029}',
        '\u{E0000}',
        '\u{E0080}',
        'é',
        '中',
    ] {
        let s = scan(&format!("x{c}"));
        assert!(
            s.critical.is_empty() && s.warnings.is_empty(),
            "U+{:04X}",
            c as u32
        );
    }

    let bom = scan("\u{FEFF}text");
    assert!(bom.critical.is_empty() && bom.warnings.is_empty());
    let late = scan("text\u{FEFF}");
    assert_eq!(late.warnings.len(), 1);
    assert_eq!((late.warnings[0].line, late.warnings[0].column), (1, 5));

    let two = scan("\u{200B}\n\n  \u{2066}\u{202C}");
    assert_eq!(two.warnings.len(), 1);
    let places: Vec<_> = two.critical.iter().map(|f| (f.line, f.column)).collect();
    assert_eq!(places, [(3, 3), (3, 4)]);
}

#[test]
fn source_settings_cannot_be_options_or_escape() {
    let url = SourceUrl::parse("https://github.com/acme/skills.git", false).unwrap();
    assert_eq!(url.host().to_string(), "github.com");
    assert_eq!(url.port(), 443);
    assert_eq!(url.as_str(), "https://github.com/acme/skills.git");
    assert_eq!(
        SourceUrl::parse("https://git.example:8443/r", false)
            .unwrap()
            .port(),
        8443
    );
    let plain = SourceUrl::parse("http://mock-model:8000/r", true).unwrap();
    assert_eq!(plain.port(), 8000);

    for bad in [
        "-x",
        "--upload-pack=x",
        "ssh://h/r",
        "git@github.com:a/b",
        "file:///r",
        "git://h/r",
        "ext::sh",
        "https://u:p@h/r",
        "https://u@h/r",
        "https://h/r?x",
        "https://h/r#x",
        "https://h/r#",
        "http://h/r",
        " https://h/r",
        "https://h/r\n",
        "https://h\\r",
        "https:///r",
        "",
    ] {
        assert!(SourceUrl::parse(bad, false).is_err(), "{bad:?}");
    }
    assert!(SourceUrl::parse("http://h/r", true).is_ok());
    for bad in ["ssh://h/r", "https://u:p@h/r", "file:///r"] {
        assert!(SourceUrl::parse(bad, true).is_err(), "{bad:?}");
    }
    // A refused URL's password never appears in the error.
    let err = SourceUrl::parse("https://u:hunter2@h/r", false)
        .unwrap_err()
        .to_string();
    assert!(!err.contains("hunter2"), "{err}");

    for good in ["main", "v1.2.0", "release/2026", "HEAD", "a_b-c.d"] {
        assert_eq!(GitRef::parse(good).unwrap().as_str(), good);
    }
    for bad in [
        "--upload-pack=x",
        "-b",
        "a..b",
        "a//b",
        "a@{1}",
        "/a",
        "",
        "a b",
        "a:b",
        "a~1",
        "a^",
        &"a".repeat(256),
    ] {
        assert!(GitRef::parse(bad).is_err(), "{bad:?}");
    }
    assert!(GitRef::parse(&"a".repeat(255)).is_ok());

    for bad in [
        "../x", "a/../b", "/abs", "a\\b", ".", "./a", "a/.", "a//b", "a/", "a b",
    ] {
        assert!(Folder::parse(bad).is_err(), "{bad:?}");
    }
    let root = Path::new("/tmp/checkout");
    assert_eq!(Folder::parse("").unwrap().join(root), root);
    assert_eq!(
        Folder::parse("catalog").unwrap().join(root),
        root.join("catalog")
    );
    assert_eq!(
        Folder::parse("a/b").unwrap().join(root),
        root.join("a").join("b")
    );
    assert_eq!(Folder::parse("a/b").unwrap().as_str(), "a/b");
    assert!(Folder::parse(".claude/skills").is_ok());
}
