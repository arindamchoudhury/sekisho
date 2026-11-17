//! The `sekisho/v1alpha1` environment format (plan 3, Task 4; D174).

use std::fs;

use sekisho_catalog::check::check_source;
use sekisho_catalog::environment::{
    API_VERSION, EnvironmentDoc, Problem, from_json_value, parse_yaml,
};
use sekisho_catalog::names::SourceKind;
use serde_json::json;

const FULL: &str = r#"api_version: sekisho/v1alpha1
kind: Environment
name: data-team
description: The data team's tools.
notice: Ask in the data-help channel.
access_groups: [data-team, github/acme-data]
models: [claude-sonnet-5-5, gpt-5.1]
default_models:
  anthropic: claude-sonnet-5-5
  openai: gpt-5.1
small_model: claude-sonnet-5-5
labels:
  cost-centre: 4711
mcp_servers:
  - github
  - server: linear@obot
    tools: [create_issue, list_issues]
a2a_agents: [planner]
skills: [pdf, review@team-skills]
plugins: [formatter]
prompts: [release-notes]
toolkits: [finops]
"#;

fn fields(problems: &[Problem]) -> Vec<&str> {
    let mut v: Vec<&str> = problems.iter().map(|p| p.field.as_str()).collect();
    v.sort();
    v
}

fn yaml_problems(text: &str) -> Vec<Problem> {
    parse_yaml(text).expect_err("should be refused")
}

#[test]
fn the_yaml_round_trips() {
    let doc = parse_yaml(FULL).unwrap();
    assert_eq!(doc.api_version, API_VERSION);
    assert_eq!(doc.kind.as_deref(), Some("Environment"));
    assert_eq!(doc.name.as_str(), "data-team");
    assert_eq!(doc.notice, "Ask in the data-help channel.");
    assert_eq!(doc.access_groups, ["data-team", "github/acme-data"]);
    assert_eq!(doc.models.len(), 2);
    assert_eq!(
        doc.default_models.openai.as_ref().unwrap().as_str(),
        "gpt-5.1"
    );
    assert_eq!(doc.default_models.gemini, None);
    assert_eq!(
        doc.small_model.as_ref().unwrap().as_str(),
        "claude-sonnet-5-5"
    );
    assert_eq!(doc.labels.as_map()["cost-centre"], "4711");
    assert_eq!(doc.skills[1].to_string(), "review@team-skills");

    let yaml = doc.to_yaml();
    assert!(
        yaml.starts_with("api_version: sekisho/v1alpha1\nkind: Environment\nname: data-team\n"),
        "{yaml}"
    );
    assert!(
        yaml.contains("- github\n"),
        "a server without tools is a bare name: {yaml}"
    );
    assert_eq!(parse_yaml(&yaml).unwrap(), doc);

    // Empty lists and unset defaults are left out.
    let small =
        parse_yaml("api_version: sekisho/v1alpha1\nkind: Environment\nname: tiny\n").unwrap();
    assert_eq!(
        small.to_yaml(),
        "api_version: sekisho/v1alpha1\nkind: Environment\nname: tiny\n"
    );
}

#[test]
fn yaml_needs_api_version_and_kind() {
    assert_eq!(
        fields(&yaml_problems("kind: Environment\nname: a\n")),
        ["api_version"]
    );
    assert_eq!(
        fields(&yaml_problems("api_version: sekisho/v1alpha1\nname: a\n")),
        ["kind"]
    );
    assert_eq!(
        fields(&yaml_problems(
            "api_version: sekisho/v2\nkind: Toolkit\nname: a\n"
        )),
        ["api_version", "kind"]
    );
    assert_eq!(
        fields(&yaml_problems(
            "api_version: sekisho/v1alpha1\nkind: Environment\n"
        )),
        ["name"]
    );
    assert_eq!(fields(&yaml_problems("- a\n- b\n")), [""]);
    assert!(parse_yaml("a: [unclosed\n").is_err());
}

#[test]
fn json_may_leave_out_kind() {
    let doc = from_json_value(json!({"name": "a"}), false).unwrap();
    assert_eq!(doc.api_version, API_VERSION);
    assert_eq!(doc.kind, None);
    assert_eq!(
        fields(&from_json_value(json!({"name": "a"}), true).unwrap_err()),
        ["kind"]
    );

    // A GET answer can be sent back as is: its read-only fields are ignored.
    let answer = json!({
        "api_version": API_VERSION, "kind": "Environment", "name": "a",
        "id": "0b8e0c4e-6f1a-4b3e-9c55-1d2f3a4b5c6d", "owner": "0b8e0c4e-6f1a-4b3e-9c55-1d2f3a4b5c6d",
        "owner_email": "a@example.com", "created_at": "2026-11-17T00:00:00Z",
        "updated_at": "2026-11-17T00:00:00Z", "effective_default_models": {}, "catalog_status": [],
        "delivered": [], "labels": {"cost-centre": 4711}
    });
    let doc = from_json_value(answer, true).unwrap();
    assert_eq!(doc.labels.as_map()["cost-centre"], "4711");
    assert_eq!(
        fields(&from_json_value(json!({"name": "a", "budgets": []}), false).unwrap_err()),
        ["budgets"]
    );
}

#[test]
fn mcp_servers_take_a_name_or_a_name_with_tools() {
    let doc = from_json_value(
        json!({"name": "a", "mcp_servers": ["github", {"server": "linear@obot", "tools": ["a"]}, {"server": "jira"}]}),
        false,
    )
    .unwrap();
    let uses: Vec<_> = doc
        .mcp_servers
        .iter()
        .map(|u| (u.server.to_string(), u.tools.clone()))
        .collect();
    assert_eq!(
        uses,
        [
            ("github".to_string(), None),
            ("linear@obot".to_string(), Some(vec!["a".to_string()])),
            ("jira".to_string(), None)
        ]
    );

    let problems = from_json_value(
        json!({"name": "a", "mcp_servers": [
            "Bad Name",
            {"server": "x", "tools": []},
            {"server": "y", "tools": ["t", "t", ""]},
            {"server": "z", "colour": "red"},
            7
        ]}),
        false,
    )
    .unwrap_err();
    assert_eq!(
        fields(&problems),
        [
            "mcp_servers[0]",
            "mcp_servers[1].tools",
            "mcp_servers[2].tools[1]",
            "mcp_servers[2].tools[2]",
            "mcp_servers[3].colour",
            "mcp_servers[4]"
        ]
    );
}

#[test]
fn toolkits_are_listed_like_entries() {
    let doc = parse_yaml(
        "api_version: sekisho/v1alpha1\nkind: Environment\nname: a\ntoolkits: [finops, data@platform]\n",
    )
    .unwrap();
    let toolkits: Vec<_> = doc.toolkits.iter().map(ToString::to_string).collect();
    assert_eq!(toolkits, ["finops", "data@platform"]);
    assert!(
        doc.to_yaml()
            .ends_with("toolkits:\n- finops\n- data@platform\n"),
        "{}",
        doc.to_yaml()
    );
    assert_eq!(parse_yaml(&doc.to_yaml()).unwrap(), doc);

    let problems = from_json_value(
        json!({"name": "a", "toolkits": ["finops", "finops"]}),
        false,
    )
    .unwrap_err();
    assert_eq!(fields(&problems), ["toolkits[1]"]);
}

#[test]
fn every_problem_is_reported_at_once() {
    let problems = from_json_value(
        json!({
            "name": "a",
            "models": ["m1"],
            "default_models": {"openai": "m2"},
            "labels": {"Bad": "x"},
            "skills": ["pdf", "pdf"]
        }),
        false,
    )
    .unwrap_err();
    assert_eq!(
        fields(&problems),
        ["default_models.openai", "labels.Bad", "skills[1]"]
    );
    assert!(problems.iter().all(|p| !p.message.is_empty()));
}

#[test]
fn offline_checks_cover_what_needs_no_install() {
    let problems = from_json_value(
        json!({
            "name": "models",
            "access_groups": ["ok", "Not A Group", "github/"],
            "models": ["m1", "m1", "-bad"],
            "default_models": {"anthropic": "m1", "mistral": "m1"},
            "small_model": "m3"
        }),
        false,
    )
    .unwrap_err();
    assert_eq!(
        fields(&problems),
        [
            "access_groups[1]",
            "access_groups[2]",
            "default_models.mistral",
            "models[1]",
            "models[2]",
            "name",
            "small_model"
        ]
    );
    let name = problems.iter().find(|p| p.field == "name").unwrap();
    assert!(name.message.contains("reserved"), "{name:?}");

    // check() can run alone on a document built elsewhere.
    let doc = parse_yaml(FULL).unwrap();
    assert!(doc.check().is_empty());
    let mut changed: EnvironmentDoc = doc.clone();
    changed.small_model = Some(sekisho_core::names::ModelName::parse("other").unwrap());
    assert_eq!(fields(&changed.check()), ["small_model"]);
}

#[test]
fn owners_and_budgets_are_not_in_the_format() {
    let base = "api_version: sekisho/v1alpha1\nkind: Environment\nname: a\n";
    let owner = yaml_problems(&format!("{base}owner: someone@example.com\n"));
    assert_eq!(fields(&owner), ["owner"]);
    assert!(
        owner[0].message.contains("owners are set in Sekisho"),
        "{owner:?}"
    );
    assert_eq!(
        fields(&yaml_problems(&format!("{base}budgets: []\n"))),
        ["budgets"]
    );
    assert_eq!(fields(&yaml_problems(&format!("{base}id: x\n"))), ["id"]);
}

#[test]
fn an_environments_source_finds_environment_files() {
    let dir = tempfile::tempdir().unwrap();
    let env =
        |name: &str| format!("api_version: sekisho/v1alpha1\nkind: Environment\nname: {name}\n");
    let files: Vec<(&str, String)> = vec![
        ("a.yaml", env("alpha")),
        ("teams/b.yml", env("beta")),
        ("ci.yaml", "kind: Workflow\non: push\n".into()),
        ("notes.yaml", "just: notes\n".into()),
        ("broken.yaml", env("Bad Name")),
        ("dup1.yaml", env("gamma")),
        ("dup2.yaml", env("gamma")),
        (
            "owned.yaml",
            format!("{}owner: x@example.com\n", env("delta")),
        ),
        ("syntax.yaml", "kind: Environment\nname: [\n".into()),
        (
            "hidden.yaml",
            format!("{}notice: \"a\u{202E}b\"\n", env("epsilon")),
        ),
        (".github/env.yaml", env("zeta")),
        ("README.md", "# Environments\n".into()),
    ];
    for (path, body) in &files {
        let path = dir.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    let c = check_source(SourceKind::Environments, dir.path()).unwrap();
    let found: Vec<(&str, &str)> = c
        .environments
        .iter()
        .map(|(path, doc)| (path.as_str(), doc.name.as_str()))
        .collect();
    assert_eq!(found, [("a.yaml", "alpha"), ("teams/b.yml", "beta")]);
    assert!(c.entries.is_empty());

    let refused: Vec<&str> = c.refused.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(
        refused,
        [
            "broken.yaml",
            "dup1.yaml",
            "dup2.yaml",
            "hidden.yaml",
            "owned.yaml",
            "syntax.yaml"
        ]
    );
    let reason = |path: &str| {
        c.refused
            .iter()
            .find(|r| r.path == path)
            .unwrap()
            .reason
            .as_str()
    };
    assert!(reason("owned.yaml").contains("owners are set in Sekisho"));
    assert!(reason("dup1.yaml").contains("gamma") && reason("dup1.yaml").contains("dup2.yaml"));
    assert!(reason("broken.yaml").contains("name"));
    assert!(reason("hidden.yaml").contains("U+202E"));
}
