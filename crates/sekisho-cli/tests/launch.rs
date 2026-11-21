//! Local MCP servers on machines (plan 4, Task 5): each runner's
//! publish-time cutoff at the entry's applied time (D200), the install's
//! package registries (D199), containers by digest, and configuration
//! values from the person's own environment.

use sekisho_catalog::delivery::{Defaults, DeliveredEntry, Delivery, Registries};
use sekisho_catalog::model::{ConfigField, ConfigTarget, EntrySpec, McpRuntime, McpServerSpec};
use sekisho_catalog::names::{EntryKind, EntryName, SourceName};
use sekisho_catalog::pins::{ImageRef, NpmPackage, PythonPackage};
use sekisho_cli::install::launch::{self, EnvValue, LocalServer};
use sekisho_core::names::EnvName;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const CUTOFF: &str = "2026-11-01T10:00:00Z";
fn applied() -> OffsetDateTime {
    OffsetDateTime::parse(CUTOFF, &Rfc3339).unwrap()
}

const DIGEST: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

fn server(name: &str, runtime: McpRuntime, config: Vec<ConfigField>) -> DeliveredEntry {
    DeliveredEntry {
        kind: EntryKind::McpServer,
        source: SourceName::parse("team").unwrap(),
        name: EntryName::parse(name).unwrap(),
        commit: "c1".into(),
        digest: "0".repeat(64),
        applied_at: applied(),
        tools: None,
        spec: EntrySpec::McpServer(McpServerSpec {
            description: String::new(),
            runtime,
            config,
            tool_preview: Vec::new(),
            unsupported_tools: Vec::new(),
            static_oauth: false,
        }),
        files: Vec::new(),
    }
}

fn delivery(entries: Vec<DeliveredEntry>, registries: Registries) -> Delivery {
    Delivery {
        environment: EnvName::parse("a").unwrap(),
        notice: String::new(),
        revision: String::new(),
        gateway_url: "https://gw.example.com/env/a".into(),
        models: Vec::new(),
        defaults: Defaults::default(),
        mcp: None,
        entries,
        quarantined: Vec::new(),
        registries,
    }
}

fn npx(name: &str) -> McpRuntime {
    McpRuntime::Npx {
        package: NpmPackage::parse_exact(&format!("@acme/{name}@1.2.3")).unwrap(),
        args: vec!["--port".into(), "1".into()],
    }
}

fn uvx(command: Option<&str>) -> McpRuntime {
    McpRuntime::Uvx {
        package: PythonPackage::parse_exact("mcp-time==1.0.0").unwrap(),
        command: command.map(str::to_owned),
        args: vec!["--local".into()],
    }
}

fn field(key: &str, required: bool, sensitive: bool, value: Option<&str>) -> ConfigField {
    ConfigField {
        key: key.into(),
        name: key.into(),
        description: String::new(),
        required,
        sensitive,
        value: value.map(str::to_owned),
        target: ConfigTarget::Env,
        prefix: None,
    }
}

fn only(d: &Delivery) -> LocalServer {
    let (servers, warnings) = launch::local_servers(d);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(servers.len(), 1, "{servers:?}");
    servers.into_iter().next().unwrap()
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn npx_gets_the_applied_time() {
    let s = only(&delivery(
        vec![server("fetch", npx("fetch"), vec![])],
        Registries::default(),
    ));
    assert_eq!(s.name, "fetch");
    assert_eq!(s.command, "npx");
    assert_eq!(
        s.args,
        strings(&[
            "--yes",
            &format!("--before={CUTOFF}"),
            "@acme/fetch@1.2.3",
            "--port",
            "1"
        ])
    );
    assert_eq!(s.applied_at, applied());

    // A plugin's own launch: npx, and npm exec.
    let p = launch::pinned(
        "npx",
        &strings(&["-y", "@acme/x@2.0.0"]),
        applied(),
        &Registries::default(),
    )
    .unwrap();
    assert_eq!(
        p.args,
        strings(&[&format!("--before={CUTOFF}"), "-y", "@acme/x@2.0.0"])
    );
    assert_eq!(p.no_cutoff, None);
    let p = launch::pinned(
        "npm",
        &strings(&["exec", "--package=@acme/x@2.0.0", "--", "x"]),
        applied(),
        &Registries::default(),
    )
    .unwrap();
    assert_eq!(
        p.args,
        strings(&[
            "exec",
            &format!("--before={CUTOFF}"),
            "--package=@acme/x@2.0.0",
            "--",
            "x"
        ])
    );
}

#[test]
fn uvx_gets_exclude_newer() {
    let s = only(&delivery(
        vec![server("time", uvx(None), vec![])],
        Registries::default(),
    ));
    assert_eq!(s.command, "uvx");
    assert_eq!(
        s.args,
        strings(&["--exclude-newer", CUTOFF, "mcp-time==1.0.0", "--local"])
    );
    let s = only(&delivery(
        vec![server("time", uvx(Some("time-server")), vec![])],
        Registries::default(),
    ));
    assert_eq!(
        s.args,
        strings(&[
            "--exclude-newer",
            CUTOFF,
            "--from",
            "mcp-time==1.0.0",
            "time-server",
            "--local"
        ])
    );

    let p = launch::pinned(
        "uv",
        &strings(&["tool", "run", "mcp-time==1.0.0"]),
        applied(),
        &Registries::default(),
    )
    .unwrap();
    assert_eq!(
        p.args,
        strings(&["tool", "run", "--exclude-newer", CUTOFF, "mcp-time==1.0.0"])
    );
    // pipx run takes the cutoff from its environment, whichever backend
    // (pip or uv) it uses.
    let p = launch::pinned(
        "pipx",
        &strings(&["run", "mcp-time==1.0.0"]),
        applied(),
        &Registries::default(),
    )
    .unwrap();
    assert_eq!(p.args, strings(&["run", "mcp-time==1.0.0"]));
    assert!(
        p.env.contains(&("UV_EXCLUDE_NEWER".into(), CUTOFF.into())),
        "{:?}",
        p.env
    );
    assert!(
        p.env
            .contains(&("PIP_UPLOADED_PRIOR_TO".into(), CUTOFF.into())),
        "{:?}",
        p.env
    );
    assert_eq!(p.no_cutoff, None);
}

#[test]
fn runners_without_a_cutoff_warn_once() {
    let mut runners = Vec::new();
    for (command, args) in [
        ("bunx", strings(&["@acme/a@1.0.0"])),
        ("bunx", strings(&["@acme/b@1.0.0"])),
        ("pnpm", strings(&["dlx", "@acme/c@1.0.0"])),
        ("yarn", strings(&["dlx", "@acme/d@1.0.0"])),
    ] {
        let p = launch::pinned(command, &args, applied(), &Registries::default()).unwrap();
        assert_eq!(p.args, args, "nothing added for {command}");
        runners.push(p.no_cutoff.expect("no absolute cutoff"));
    }
    let warning = launch::no_cutoff_warning(&runners).unwrap();
    assert_eq!(warning.matches("bunx").count(), 1, "{warning}");
    assert!(
        warning.contains("pnpm dlx") && warning.contains("yarn dlx"),
        "{warning}"
    );
    assert!(launch::no_cutoff_warning(&[]).is_none());
}

#[test]
fn registries_reach_each_server() {
    let registries = Registries {
        npm: Some("https://npm.example.com/".into()),
        python: Some("https://pypi.example.com/simple/".into()),
    };
    let (servers, _) = launch::local_servers(&delivery(
        vec![
            server("fetch", npx("fetch"), vec![]),
            server("time", uvx(None), vec![]),
        ],
        registries.clone(),
    ));
    let env = |name: &str| servers.iter().find(|s| s.name == name).unwrap().env.clone();
    assert_eq!(
        env("fetch"),
        [(
            "NPM_CONFIG_REGISTRY".to_owned(),
            EnvValue::Literal("https://npm.example.com/".into())
        )]
    );
    let time = env("time");
    for var in ["UV_DEFAULT_INDEX", "PIP_INDEX_URL"] {
        assert!(
            time.contains(&(
                var.into(),
                EnvValue::Literal("https://pypi.example.com/simple/".into())
            )),
            "{var}: {time:?}"
        );
    }
    let p = launch::pinned("bunx", &strings(&["@acme/a@1.0.0"]), applied(), &registries).unwrap();
    assert!(p.env.contains(&(
        "NPM_CONFIG_REGISTRY".into(),
        "https://npm.example.com/".into()
    )));
}

#[test]
fn images_run_by_digest_only() {
    let image = ImageRef::parse("acme/server:1.0").unwrap();
    let pinned = image.with_digest(DIGEST.into()).unwrap();
    let container = |image: ImageRef, port: Option<u16>| McpRuntime::Container {
        image,
        command: Some("serve".into()),
        args: vec!["--stdio".into()],
        port,
        path: None,
    };
    let s = only(&delivery(
        vec![server(
            "box",
            container(pinned.clone(), None),
            vec![field("TOKEN", true, true, None)],
        )],
        Registries::default(),
    ));
    assert_eq!(s.command, "docker");
    assert_eq!(
        s.args,
        strings(&[
            "run",
            "--rm",
            "-i",
            "-e",
            "TOKEN",
            &format!("docker.io/acme/server@{DIGEST}"),
            "serve",
            "--stdio"
        ])
    );
    // A tag alone, or a server listening on a port: not started.
    for runtime in [container(image, None), container(pinned, Some(8080))] {
        let (servers, warnings) = launch::local_servers(&delivery(
            vec![server("box", runtime, vec![])],
            Registries::default(),
        ));
        assert!(servers.is_empty(), "{servers:?}");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }
}

#[test]
fn config_values_come_from_the_persons_environment() {
    let mut header = field("X-API-KEY", true, false, None);
    header.target = ConfigTarget::Header;
    let s = only(&delivery(
        vec![server(
            "fetch",
            npx("fetch"),
            vec![
                field("GITHUB_TOKEN", true, false, None),
                field("LOG_LEVEL", false, false, None),
                field("MODE", true, false, Some("fast")),
                header,
            ],
        )],
        Registries::default(),
    ));
    assert_eq!(
        s.env,
        [
            (
                "GITHUB_TOKEN".to_owned(),
                EnvValue::FromUser {
                    name: "GITHUB_TOKEN".into(),
                    required: true
                }
            ),
            (
                "LOG_LEVEL".to_owned(),
                EnvValue::FromUser {
                    name: "LOG_LEVEL".into(),
                    required: false
                }
            ),
            ("MODE".to_owned(), EnvValue::Literal("fast".into())),
        ]
    );
}

#[test]
fn sensitive_values_are_never_written() {
    let s = only(&delivery(
        vec![server(
            "fetch",
            npx("fetch"),
            vec![field("API_KEY", true, true, Some("s3cret-value"))],
        )],
        Registries::default(),
    ));
    assert_eq!(
        s.env,
        [(
            "API_KEY".to_owned(),
            EnvValue::FromUser {
                name: "API_KEY".into(),
                required: true
            }
        )]
    );
    assert!(!format!("{s:?}").contains("s3cret-value"));
}

#[test]
fn a_local_server_reaches_the_tools_config() {
    use sekisho_cli::adapters::{TestAdapter, Tool};
    use sekisho_cli::install::{self, InstallError, Options, Scope, ToolHomes};
    let tmp = tempfile::TempDir::new().unwrap();
    let homes = ToolHomes::new(
        Tool::ALL
            .iter()
            .map(|t| (*t, tmp.path().join("home").join(t.as_str())))
            .collect(),
    );
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let scope = Scope::project(&project, &tmp.path().join("config"), homes);
    let d = delivery(
        vec![server(
            "fetch",
            npx("fetch"),
            vec![field("GITHUB_TOKEN", true, false, None)],
        )],
        Registries::default(),
    )
    .seal();
    let fetch = |_: &str| -> Result<Vec<u8>, InstallError> { unreachable!("no files") };
    install::install(
        &d,
        &[&TestAdapter::new(Tool::ClaudeCode)],
        &scope,
        &Options::default(),
        &fetch,
    )
    .unwrap();
    let mcp: serde_json::Value =
        serde_json::from_slice(&std::fs::read(project.join(".claude-code/mcp.json")).unwrap())
            .unwrap();
    let fetch = &mcp["mcpServers"]["fetch"];
    assert_eq!(fetch["command"], "npx");
    assert_eq!(fetch["args"][1], format!("--before={CUTOFF}"));
    assert_eq!(fetch["env"]["GITHUB_TOKEN"], "${GITHUB_TOKEN}");
}
