//! Catalogs in the store (plan 3, Task 5): sources, syncs, entries,
//! revisions, quarantine and registry credentials. Each test runs on its
//! own `postgres:18`.

mod common;

use base64::Engine;
use common::{person, store};
use sekisho_catalog::model::{
    CheckedEntry, EntrySpec, McpRuntime, McpServerSpec, Refusal, SkillSpec, ToolkitServer,
    ToolkitSpec,
};
use sekisho_catalog::names::{EntryKind, EntryName, EntryRef, SourceKind, SourceName};
use sekisho_catalog::source::{Folder, GitRef, SourceUrl};
use sekisho_catalog::tree::TreeFile;
use sekisho_core::secret_box::SecretBox;
use sekisho_store::{
    EntryKey, FieldError, FoundEntry, NewSource, ResolveError, Source, SourcePatch, Store,
    StoreError, SyncFound, SyncOutcome, SyncOutcomeKind, SyncRecord,
};
use time::{Duration, OffsetDateTime};

fn source_name(s: &str) -> SourceName {
    SourceName::parse(s).unwrap()
}

fn entry_name(s: &str) -> EntryName {
    EntryName::parse(s).unwrap()
}

fn new_source(name: &str, token: Option<&str>) -> NewSource {
    NewSource {
        name: source_name(name),
        kind: SourceKind::Skills,
        url: SourceUrl::parse(&format!("https://git.example.com/{name}.git"), false).unwrap(),
        git_ref: Some(GitRef::parse("main").unwrap()),
        folder: Folder::parse("skills").unwrap(),
        username: None,
        token: token.map(str::to_owned),
        allow_private_network: false,
    }
}

async fn add(store: &Store, sb: &SecretBox, name: &str, actor: uuid::Uuid) -> Source {
    store
        .add_source(sb, new_source(name, None), actor)
        .await
        .unwrap()
}

/// A skill whose SKILL.md is `body`, plus any extra files.
fn skill(name: &str, body: &str, extra: &[(&str, &[u8])]) -> CheckedEntry {
    let mut files = vec![TreeFile {
        path: "SKILL.md".into(),
        bytes: body.as_bytes().to_vec(),
        executable: false,
    }];
    files.extend(extra.iter().map(|(p, b)| TreeFile {
        path: (*p).into(),
        bytes: b.to_vec(),
        executable: false,
    }));
    CheckedEntry {
        kind: EntryKind::Skill,
        name: entry_name(name),
        path: name.into(),
        spec: EntrySpec::Skill(SkillSpec {
            description: format!("The {name} skill."),
            license: None,
            compatibility: None,
        }),
        files,
    }
}

fn ok_sync(commit: &str, entries: Vec<CheckedEntry>, refused: Vec<Refusal>) -> SyncRecord {
    let now = OffsetDateTime::now_utc();
    SyncRecord {
        commit: Some(commit.into()),
        started_at: now,
        finished_at: now,
        outcome: SyncOutcome::Ok(SyncFound {
            entries: entries.into_iter().map(FoundEntry::from_checked).collect(),
            refused,
            warnings: Vec::new(),
            vmcps: Vec::new(),
            environments: serde_json::Value::Null,
        }),
    }
}

fn next() -> OffsetDateTime {
    OffsetDateTime::now_utc() + Duration::hours(1)
}

fn key(source: &str, name: &str) -> EntryKey {
    EntryKey {
        kind: EntryKind::Skill,
        source: source_name(source),
        name: entry_name(name),
    }
}

fn names(list: &[(EntryKind, EntryName)]) -> Vec<&str> {
    let mut v: Vec<&str> = list.iter().map(|(_, n)| n.as_str()).collect();
    v.sort();
    v
}

/// A count from SQL the test writes itself.
async fn count(store: &Store, sql: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
        .fetch_one(store.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_source_is_added_with_a_sealed_token() {
    let (store, sb) = store().await;
    let admin = person(&store, "admin@example.com", &[]).await;
    let token = "ghp_TOKENVALUE1234567890";
    let mut new = new_source("team-skills", Some(token));
    new.username = Some("x-access-token".into());
    let source = store.add_source(&sb, new, admin).await.unwrap();
    assert!(source.has_token);
    assert_eq!(source.name.as_str(), "team-skills");
    assert_eq!(source.kind, SourceKind::Skills);
    assert_eq!(
        source.url.as_str(),
        "https://git.example.com/team-skills.git"
    );
    assert_eq!(source.git_ref.as_ref().unwrap().as_str(), "main");
    assert_eq!(source.folder.as_str(), "skills");
    assert_eq!(source.username.as_deref(), Some("x-access-token"));
    assert_eq!(source.added_by, admin);
    assert_eq!(source.applied_commit, None);
    assert!(source.latest_sync.is_none() && !source.pending && !source.syncing);

    let sealed: Vec<u8> = sqlx::query_scalar("select sealed_token from catalog_sources")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert!(!sealed.windows(token.len()).any(|w| w == token.as_bytes()));
    assert_eq!(
        store
            .source_token(&sb, &source_name("team-skills"))
            .await
            .unwrap()
            .as_deref(),
        Some(token)
    );

    let taken = store
        .add_source(&sb, new_source("team-skills", None), admin)
        .await;
    assert!(matches!(taken, Err(StoreError::Conflict(_))), "{taken:?}");
    assert_eq!(store.list_sources().await.unwrap().len(), 1);

    // What reaches git's header and URL can't break out of them.
    let mut bad = new_source("bad", Some("tok\r\nX-Evil: 1"));
    bad.username = Some("user:name".into());
    match store.add_source(&sb, bad, admin).await {
        Err(StoreError::Validation(errors)) => {
            let mut fields: Vec<&str> = errors.iter().map(|e| e.field.as_str()).collect();
            fields.sort();
            assert_eq!(fields, ["token", "username"]);
        }
        other => panic!("{other:?}"),
    }
    // Only Git mode's source has the environments kind.
    let mut envs = new_source("team-envs", None);
    envs.kind = SourceKind::Environments;
    assert!(matches!(
        store.add_source(&sb, envs, admin).await,
        Err(StoreError::Validation(e)) if e[0].field == "kind"
    ));
}

#[tokio::test]
async fn changing_the_url_needs_the_token_again() {
    let (store, sb) = store().await;
    let admin = person(&store, "admin@example.com", &[]).await;
    store
        .add_source(&sb, new_source("private", Some("tok-1")), admin)
        .await
        .unwrap();
    let elsewhere = SourceUrl::parse("https://evil.example.com/x.git", false).unwrap();
    let moved = store
        .update_source(
            &sb,
            &source_name("private"),
            SourcePatch {
                url: Some(elsewhere.clone()),
                ..SourcePatch::default()
            },
        )
        .await;
    assert!(
        matches!(&moved, Err(StoreError::Validation(e)) if e[0].field == "token"),
        "{moved:?}"
    );
    let moved = store
        .update_source(
            &sb,
            &source_name("private"),
            SourcePatch {
                url: Some(elsewhere.clone()),
                token: Some(Some("tok-2".into())),
                ..SourcePatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(moved.url, elsewhere);
    assert_eq!(
        store
            .source_token(&sb, &source_name("private"))
            .await
            .unwrap()
            .as_deref(),
        Some("tok-2")
    );
    // Clearing the token, then a public source moves freely.
    let cleared = store
        .update_source(
            &sb,
            &source_name("private"),
            SourcePatch {
                token: Some(None),
                ..SourcePatch::default()
            },
        )
        .await
        .unwrap();
    assert!(!cleared.has_token);
    add(&store, &sb, "public", admin).await;
    let patch = SourcePatch {
        url: Some(SourceUrl::parse("https://git.example.com/other.git", false).unwrap()),
        git_ref: Some(None),
        folder: Some(Folder::parse("").unwrap()),
        allow_private_network: Some(true),
        ..SourcePatch::default()
    };
    let before = store
        .get_source(&source_name("public"))
        .await
        .unwrap()
        .updated_at;
    let public = store
        .update_source(&sb, &source_name("public"), patch)
        .await
        .unwrap();
    assert_eq!(public.git_ref, None);
    assert!(public.allow_private_network);
    // A change of settings is dated, so a sync can tell its last read is stale.
    assert!(public.updated_at > before);
    assert!(matches!(
        store
            .update_source(&sb, &source_name("nope"), SourcePatch::default())
            .await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn a_first_sync_applies_itself() {
    let (store, sb) = store().await;
    let admin = person(&store, "admin@example.com", &[]).await;
    add(&store, &sb, "a", admin).await;
    let report = store
        .record_sync(
            &sb,
            &source_name("a"),
            ok_sync(
                "c1",
                vec![skill("pdf", "v1", &[]), skill("review", "r1", &[])],
                vec![],
            ),
            next(),
        )
        .await
        .unwrap();
    assert_eq!(report.outcome, SyncOutcomeKind::Ok);
    assert!(report.applied);
    assert_eq!(names(&report.new), ["pdf", "review"]);

    let source = store.get_source(&source_name("a")).await.unwrap();
    assert_eq!(source.applied_commit.as_deref(), Some("c1"));
    assert!(!source.pending);
    assert_eq!(
        source.latest_sync.as_ref().unwrap().outcome,
        SyncOutcomeKind::Ok
    );

    let pdf = store.get_entry(&key("a", "pdf")).await.unwrap();
    assert_eq!(pdf.owner, admin);
    assert_eq!(pdf.path, "pdf");
    let applied = pdf.applied.unwrap();
    assert_eq!(applied.commit, "c1");
    assert!(applied.applied_at.is_some());
    assert_eq!(applied.files[0].path, "SKILL.md");
    assert_eq!(applied.files[0].size, 2);
    assert!(pdf.pending.is_none());
    assert_eq!(
        store
            .list_entries(Some(EntryKind::Skill))
            .await
            .unwrap()
            .len(),
        2
    );
    assert!(
        store
            .list_entries(Some(EntryKind::Prompt))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_later_sync_waits_for_apply() {
    let (store, sb) = store().await;
    let admin = person(&store, "admin@example.com", &[]).await;
    add(&store, &sb, "a", admin).await;
    let a = source_name("a");
    store
        .record_sync(
            &sb,
            &a,
            ok_sync(
                "c1",
                vec![skill("pdf", "v1", &[]), skill("review", "r1", &[])],
                vec![],
            ),
            next(),
        )
        .await
        .unwrap();
    let report = store
        .record_sync(
            &sb,
            &a,
            ok_sync(
                "c2",
                vec![skill("pdf", "v2", &[]), skill("review", "r1", &[])],
                vec![],
            ),
            next(),
        )
        .await
        .unwrap();
    assert!(!report.applied);
    assert_eq!(names(&report.updated), ["pdf"]);
    assert_eq!(names(&report.unchanged), ["review"]);

    let pdf = store.get_entry(&key("a", "pdf")).await.unwrap();
    let first = pdf.applied.clone().unwrap();
    assert_eq!(first.commit, "c1");
    let pending = pdf.pending.clone().unwrap();
    assert_eq!(pending.commit, "c2");
    assert_eq!(pending.applied_at, None);
    assert!(store.get_source(&a).await.unwrap().pending);
    assert_eq!(
        store
            .entry_file(&key("a", "pdf"), "SKILL.md")
            .await
            .unwrap(),
        b"v1"
    );

    let settings_changed = store.get_source(&a).await.unwrap().updated_at;
    let applied = store
        .apply_source(&a, OffsetDateTime::now_utc())
        .await
        .unwrap();
    assert_eq!(names(&applied.updated), ["pdf"]);
    // Applying isn't a change of the source's settings.
    assert_eq!(
        store.get_source(&a).await.unwrap().updated_at,
        settings_changed
    );
    assert!(applied.removed.is_empty());
    let pdf = store.get_entry(&key("a", "pdf")).await.unwrap();
    assert_eq!(pdf.applied.as_ref().unwrap().commit, "c2");
    assert!(pdf.applied.unwrap().applied_at.is_some());
    assert!(pdf.pending.is_none());
    assert_eq!(
        store
            .entry_file(&key("a", "pdf"), "SKILL.md")
            .await
            .unwrap(),
        b"v2"
    );
    let source = store.get_source(&a).await.unwrap();
    assert!(!source.pending);
    assert_eq!(source.applied_commit.as_deref(), Some("c2"));
    // Nothing is left to apply.
    assert!(matches!(
        store.apply_source(&a, OffsetDateTime::now_utc()).await,
        Err(StoreError::Conflict(_))
    ));
}

#[tokio::test]
async fn refused_and_removed_entries_keep_their_revision() {
    let (store, sb) = store().await;
    let admin = person(&store, "admin@example.com", &[]).await;
    add(&store, &sb, "a", admin).await;
    let a = source_name("a");
    let first = vec![
        skill("refused", "1", &[]),
        skill("gone", "1", &[]),
        skill("broken", "1", &[]),
    ];
    store
        .record_sync(&sb, &a, ok_sync("c1", first, vec![]), next())
        .await
        .unwrap();

    // "refused" is refused by name; "broken"'s folder failed without one.
    let refusals = vec![
        Refusal {
            path: "refused".into(),
            kind: Some(EntryKind::Skill),
            name: Some("refused".into()),
            reason: "a hidden character".into(),
        },
        Refusal {
            path: "broken".into(),
            kind: Some(EntryKind::Skill),
            name: None,
            reason: "SKILL.md can't be read".into(),
        },
    ];
    let report = store
        .record_sync(&sb, &a, ok_sync("c2", vec![], refusals), next())
        .await
        .unwrap();
    assert_eq!(names(&report.missing), ["gone"]);

    let applied = store
        .apply_source(&a, OffsetDateTime::now_utc())
        .await
        .unwrap();
    assert_eq!(names(&applied.removed), ["gone"]);
    for name in ["refused", "broken"] {
        let e = store.get_entry(&key("a", name)).await.unwrap();
        assert_eq!(e.removed_at, None, "{name}");
        assert_eq!(e.applied.unwrap().commit, "c1", "{name}");
    }
    let gone = store.get_entry(&key("a", "gone")).await.unwrap();
    assert!(gone.removed_at.is_some());
    assert_eq!(gone.applied.unwrap().commit, "c1");

    // Found again, it comes back at apply.
    store
        .record_sync(
            &sb,
            &a,
            ok_sync("c3", vec![skill("gone", "1", &[])], vec![]),
            next(),
        )
        .await
        .unwrap();
    store
        .apply_source(&a, OffsetDateTime::now_utc())
        .await
        .unwrap();
    assert_eq!(
        store.get_entry(&key("a", "gone")).await.unwrap().removed_at,
        None
    );
}

#[tokio::test]
async fn an_unchanged_digest_makes_no_revision() {
    let (store, sb) = store().await;
    let admin = person(&store, "admin@example.com", &[]).await;
    add(&store, &sb, "a", admin).await;
    let a = source_name("a");
    store
        .record_sync(
            &sb,
            &a,
            ok_sync("c1", vec![skill("pdf", "v1", &[])], vec![]),
            next(),
        )
        .await
        .unwrap();
    let report = store
        .record_sync(
            &sb,
            &a,
            ok_sync("c2", vec![skill("pdf", "v1", &[])], vec![]),
            next(),
        )
        .await
        .unwrap();
    assert_eq!(names(&report.unchanged), ["pdf"]);
    assert_eq!(
        count(&store, "select count(*) from catalog_revisions").await,
        1
    );
}

#[tokio::test]
async fn identical_files_are_stored_once() {
    let (store, sb) = store().await;
    let admin = person(&store, "admin@example.com", &[]).await;
    add(&store, &sb, "a", admin).await;
    let licence: &[u8] = b"Apache-2.0\n";
    let entries = vec![
        skill("pdf", "p", &[("LICENSE", licence)]),
        skill("review", "r", &[("LICENSE", licence)]),
    ];
    store
        .record_sync(
            &sb,
            &source_name("a"),
            ok_sync("c1", entries, vec![]),
            next(),
        )
        .await
        .unwrap();
    assert_eq!(count(&store, "select count(*) from catalog_blobs").await, 3);
    let pdf = store
        .get_entry(&key("a", "pdf"))
        .await
        .unwrap()
        .applied
        .unwrap();
    let file = pdf.files.iter().find(|f| f.path == "LICENSE").unwrap();
    assert_eq!(store.blob(&file.sha256).await.unwrap(), licence);
    assert!(matches!(
        store.entry_file(&key("a", "pdf"), "nope").await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn only_one_replica_claims_a_source() {
    let (store, sb) = store().await;
    let admin = person(&store, "admin@example.com", &[]).await;
    add(&store, &sb, "a", admin).await;
    let a = source_name("a");
    let now = OffsetDateTime::now_utc();
    let lease = Duration::minutes(10);
    let (one, two) = tokio::join!(
        store.claim_source(&a, now, lease),
        store.claim_source(&a, now, lease)
    );
    assert!(one.unwrap() ^ two.unwrap(), "exactly one replica claims it");
    assert!(store.get_source(&a).await.unwrap().syncing);
    assert_eq!(store.claim_due_source(now, lease).await.unwrap(), None);

    // Recording the sync releases the claim and sets the next one.
    store
        .record_sync(
            &sb,
            &a,
            ok_sync("c1", vec![], vec![]),
            now + Duration::hours(1),
        )
        .await
        .unwrap();
    assert!(!store.get_source(&a).await.unwrap().syncing);
    assert_eq!(store.claim_due_source(now, lease).await.unwrap(), None);
    let later = now + Duration::hours(2);
    assert_eq!(
        store.claim_due_source(later, lease).await.unwrap(),
        Some(a.clone())
    );
    // An expired lease can be claimed again.
    assert!(
        store
            .claim_source(&a, later + Duration::minutes(11), lease)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn a_failed_sync_changes_no_entry() {
    let (store, sb) = store().await;
    let admin = person(&store, "admin@example.com", &[]).await;
    add(&store, &sb, "a", admin).await;
    let a = source_name("a");
    store
        .record_sync(
            &sb,
            &a,
            ok_sync("c1", vec![skill("pdf", "v1", &[])], vec![]),
            next(),
        )
        .await
        .unwrap();
    let now = OffsetDateTime::now_utc();
    let failed = SyncRecord {
        commit: None,
        started_at: now,
        finished_at: now,
        outcome: SyncOutcome::Failed("git fetch failed: could not resolve host".into()),
    };
    let report = store.record_sync(&sb, &a, failed, next()).await.unwrap();
    assert_eq!(report.outcome, SyncOutcomeKind::Failed);
    let source = store.get_source(&a).await.unwrap();
    let latest = source.latest_sync.unwrap();
    assert_eq!(latest.outcome, SyncOutcomeKind::Failed);
    assert!(latest.error.unwrap().contains("could not resolve host"));
    assert!(!source.pending);
    let pdf = store.get_entry(&key("a", "pdf")).await.unwrap();
    assert_eq!(pdf.applied.unwrap().commit, "c1");
    assert!(pdf.pending.is_none());

    let unchanged = SyncRecord {
        commit: Some("c1".into()),
        started_at: now,
        finished_at: now,
        outcome: SyncOutcome::Unchanged,
    };
    let report = store.record_sync(&sb, &a, unchanged, next()).await.unwrap();
    assert_eq!(report.outcome, SyncOutcomeKind::Unchanged);
    assert_eq!(
        count(&store, "select count(*) from catalog_revisions").await,
        1
    );
}

#[tokio::test]
async fn bare_references_resolve_or_name_the_sources() {
    let (store, sb) = store().await;
    let admin = person(&store, "admin@example.com", &[]).await;
    for s in ["a", "b"] {
        add(&store, &sb, s, admin).await;
        store
            .record_sync(
                &sb,
                &source_name(s),
                ok_sync("c1", vec![skill("pdf", s, &[])], vec![]),
                next(),
            )
            .await
            .unwrap();
    }
    add(&store, &sb, "c", admin).await;
    store
        .record_sync(
            &sb,
            &source_name("c"),
            ok_sync("c1", vec![skill("only", "o", &[])], vec![]),
            next(),
        )
        .await
        .unwrap();
    let r = |s: &str| EntryRef::parse(s).unwrap();
    match store.resolve_entry(EntryKind::Skill, &r("pdf")).await {
        Err(ResolveError::Ambiguous(sources)) => {
            let sources: Vec<&str> = sources.iter().map(SourceName::as_str).collect();
            assert_eq!(sources, ["a", "b"]);
        }
        other => panic!("{other:?}"),
    }
    let a_pdf = store
        .resolve_entry(EntryKind::Skill, &r("pdf@a"))
        .await
        .unwrap();
    assert_eq!(a_pdf, store.get_entry(&key("a", "pdf")).await.unwrap().id);
    let only = store
        .resolve_entry(EntryKind::Skill, &r("only"))
        .await
        .unwrap();
    assert_eq!(only, store.get_entry(&key("c", "only")).await.unwrap().id);
    for (kind, reference) in [
        (EntryKind::Skill, "nope"),
        (EntryKind::Skill, "pdf@c"),
        (EntryKind::Prompt, "pdf"),
    ] {
        assert!(
            matches!(
                store.resolve_entry(kind, &r(reference)).await,
                Err(ResolveError::NotFound)
            ),
            "{reference}"
        );
    }
}

#[tokio::test]
async fn quarantine_is_kept_with_its_history() {
    let (store, sb) = store().await;
    let admin = person(&store, "admin@example.com", &[]).await;
    let other = person(&store, "other@example.com", &[]).await;
    add(&store, &sb, "a", admin).await;
    store
        .record_sync(
            &sb,
            &source_name("a"),
            ok_sync("c1", vec![skill("pdf", "v1", &[])], vec![]),
            next(),
        )
        .await
        .unwrap();
    let pdf = key("a", "pdf");
    let t0 = OffsetDateTime::now_utc();

    for bad in ["", "   ", &"r".repeat(501)] {
        assert!(matches!(
            store.quarantine(&pdf, bad, admin, t0).await,
            Err(StoreError::Validation(e)) if e[0].field == "reason"
        ));
    }
    assert!(matches!(
        store.lift_quarantine(&pdf, admin, t0).await,
        Err(StoreError::Conflict(_))
    ));
    store
        .quarantine(&pdf, "leaks tokens", admin, t0)
        .await
        .unwrap();
    assert!(matches!(
        store.quarantine(&pdf, "again", admin, t0).await,
        Err(StoreError::Conflict(_))
    ));
    let q = store.get_entry(&pdf).await.unwrap().quarantine.unwrap();
    assert_eq!((q.reason.as_str(), q.by), ("leaks tokens", admin));
    store
        .lift_quarantine(&pdf, other, t0 + Duration::minutes(1))
        .await
        .unwrap();
    assert!(store.get_entry(&pdf).await.unwrap().quarantine.is_none());
    store
        .quarantine(&pdf, "still leaks", other, t0 + Duration::minutes(2))
        .await
        .unwrap();

    let history = store.quarantine_history(&pdf).await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].reason, "leaks tokens");
    assert_eq!(history[0].quarantined_by, admin);
    assert_eq!(history[0].lifted_by, Some(other));
    assert!(history[0].lifted_at.is_some());
    assert_eq!(history[1].reason, "still leaks");
    assert_eq!(history[1].lifted_by, None);
}

#[tokio::test]
async fn owners_must_be_enabled_users() {
    let (store, sb) = store().await;
    let admin = person(&store, "admin@example.com", &[]).await;
    let leaver = person(&store, "leaver@example.com", &[]).await;
    let heir = person(&store, "heir@example.com", &[]).await;
    store
        .set_disabled(&leaver, true, OffsetDateTime::now_utc())
        .await
        .unwrap();
    add(&store, &sb, "a", admin).await;
    store
        .record_sync(
            &sb,
            &source_name("a"),
            ok_sync("c1", vec![skill("pdf", "v1", &[])], vec![]),
            next(),
        )
        .await
        .unwrap();
    let pdf = key("a", "pdf");
    for owner in [leaver, uuid::Uuid::from_u128(42)] {
        match store.set_entry_owner(&pdf, owner).await {
            Err(StoreError::Validation(errors)) => {
                assert_eq!(
                    errors,
                    [FieldError::new("owner", errors[0].message.clone())]
                )
            }
            other => panic!("{other:?}"),
        }
    }
    store.set_entry_owner(&pdf, heir).await.unwrap();
    assert_eq!(store.get_entry(&pdf).await.unwrap().owner, heir);
    assert!(matches!(
        store.set_entry_owner(&key("a", "nope"), heir).await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn registry_passwords_are_sealed() {
    let (store, sb) = store().await;
    let password = "registry-password-123";
    store
        .set_registry_credential(&sb, "GHCR.io", "robot", password)
        .await
        .unwrap();
    store
        .set_registry_credential(&sb, "localhost:5000", "me", "pw")
        .await
        .unwrap();
    let sealed: Vec<u8> = sqlx::query_scalar(
        "select sealed_password from registry_credentials where host = 'ghcr.io'",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert!(
        !sealed
            .windows(password.len())
            .any(|w| w == password.as_bytes())
    );
    assert_eq!(
        store.registry_credential(&sb, "ghcr.io").await.unwrap(),
        Some(("robot".to_string(), password.to_string()))
    );
    assert_eq!(
        store.registry_hosts().await.unwrap(),
        [
            ("ghcr.io".to_string(), "robot".to_string()),
            ("localhost:5000".to_string(), "me".to_string())
        ]
    );
    for (host, user, pass) in [
        ("a/b", "u", "p"),
        ("ghcr.io", "", "p"),
        ("ghcr.io", "u", ""),
        ("h:x", "u", "p"),
    ] {
        assert!(
            matches!(
                store.set_registry_credential(&sb, host, user, pass).await,
                Err(StoreError::Validation(_))
            ),
            "{host} {user} {pass}"
        );
    }
    store.delete_registry_credential("ghcr.io").await.unwrap();
    assert_eq!(
        store.registry_credential(&sb, "ghcr.io").await.unwrap(),
        None
    );
    assert!(matches!(
        store.delete_registry_credential("ghcr.io").await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn sync_reports_never_hold_the_token() {
    let (store, sb) = store().await;
    let admin = person(&store, "admin@example.com", &[]).await;
    let token = "glpat-SECRETVALUE987";
    let mut new = new_source("private", Some(token));
    new.username = Some("oauth2".into());
    store.add_source(&sb, new, admin).await.unwrap();
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("oauth2:{token}"));
    let now = OffsetDateTime::now_utc();
    let message = format!(
        "fatal: could not read from https://oauth2:{token}@git.example.com/x.git (Authorization: Basic {basic})"
    );
    let failed = SyncRecord {
        commit: None,
        started_at: now,
        finished_at: now,
        outcome: SyncOutcome::Failed(message),
    };
    let report = store
        .record_sync(&sb, &source_name("private"), failed, next())
        .await
        .unwrap();
    assert!(!report.error.as_deref().unwrap().contains(token));
    let refusal = Refusal {
        path: "x".into(),
        kind: None,
        name: None,
        reason: format!("token {token} in a file"),
    };
    store
        .record_sync(
            &sb,
            &source_name("private"),
            ok_sync("c1", vec![], vec![refusal]),
            next(),
        )
        .await
        .unwrap();
    let stored: Vec<String> =
        sqlx::query_scalar("select coalesce(error, '') || report::text from catalog_syncs")
            .fetch_all(store.pool())
            .await
            .unwrap();
    assert_eq!(stored.len(), 2);
    for row in stored {
        assert!(!row.contains(token) && !row.contains(&basic), "{row}");
    }
}

#[tokio::test]
async fn deleting_a_source_deletes_its_entries() {
    let (store, sb) = store().await;
    let admin = person(&store, "admin@example.com", &[]).await;
    add(&store, &sb, "a", admin).await;
    let a = source_name("a");
    store
        .record_sync(
            &sb,
            &a,
            ok_sync("c1", vec![skill("pdf", "v1", &[])], vec![]),
            next(),
        )
        .await
        .unwrap();
    store
        .quarantine(&key("a", "pdf"), "bad", admin, OffsetDateTime::now_utc())
        .await
        .unwrap();
    store.delete_source(&a).await.unwrap();
    assert!(matches!(
        store.get_source(&a).await,
        Err(StoreError::NotFound)
    ));
    for table in [
        "catalog_entries",
        "catalog_revisions",
        "catalog_syncs",
        "catalog_quarantines",
    ] {
        assert_eq!(
            count(&store, &format!("select count(*) from {table}")).await,
            0,
            "{table}"
        );
    }
    assert!(matches!(
        store.delete_source(&a).await,
        Err(StoreError::NotFound)
    ));
}

// ------------------------------------------------------------ toolkits (Task 7b)

fn reference(s: &str) -> EntryRef {
    EntryRef::parse(s).unwrap()
}

fn docs_server() -> CheckedEntry {
    CheckedEntry {
        kind: EntryKind::McpServer,
        name: entry_name("docs"),
        path: "servers.yaml".into(),
        spec: EntrySpec::McpServer(McpServerSpec {
            description: "Docs.".into(),
            runtime: McpRuntime::Remote {
                url: "https://mcp.example.com/docs".into(),
            },
            config: Vec::new(),
            tool_preview: Vec::new(),
            unsupported_tools: Vec::new(),
            static_oauth: false,
        }),
        files: Vec::new(),
    }
}

/// A toolkit listing these MCP servers (with their tools) and skills.
fn toolkit(
    name: &str,
    description: &str,
    servers: &[(&str, Option<&[&str]>)],
    skills: &[&str],
) -> CheckedEntry {
    CheckedEntry {
        kind: EntryKind::Toolkit,
        name: entry_name(name),
        path: format!("{name}.yaml"),
        spec: EntrySpec::Toolkit(ToolkitSpec {
            description: description.into(),
            mcp_servers: servers
                .iter()
                .map(|(s, tools)| ToolkitServer {
                    server: reference(s),
                    tools: tools.map(|t| t.iter().map(|x| (*x).to_owned()).collect()),
                })
                .collect(),
            a2a_agents: Vec::new(),
            skills: skills.iter().map(|s| reference(s)).collect(),
            plugins: Vec::new(),
            prompts: Vec::new(),
        }),
        files: Vec::new(),
    }
}

fn kit_key(source: &str, name: &str) -> EntryKey {
    EntryKey {
        kind: EntryKind::Toolkit,
        source: source_name(source),
        name: entry_name(name),
    }
}

/// Adds `name` and records one sync of it with `entries`.
async fn source_with(
    store: &Store,
    sb: &SecretBox,
    name: &str,
    actor: uuid::Uuid,
    commit: &str,
    entries: Vec<CheckedEntry>,
) -> sekisho_store::SyncReport {
    add(store, sb, name, actor).await;
    sync(store, sb, name, commit, entries).await
}

async fn sync(
    store: &Store,
    sb: &SecretBox,
    name: &str,
    commit: &str,
    entries: Vec<CheckedEntry>,
) -> sekisho_store::SyncReport {
    store
        .record_sync(
            sb,
            &source_name(name),
            ok_sync(commit, entries, Vec::new()),
            next(),
        )
        .await
        .unwrap()
}

/// `(kind, written, source/name, tools)` of a revision's members.
async fn members(
    store: &Store,
    revision: uuid::Uuid,
) -> Vec<(EntryKind, String, String, Option<Vec<String>>)> {
    store
        .toolkit_members(revision)
        .await
        .unwrap()
        .into_iter()
        .map(|m| {
            (
                m.kind,
                m.written,
                format!("{}/{}", m.key.source, m.key.name),
                m.tools,
            )
        })
        .collect()
}

#[tokio::test]
async fn toolkit_members_are_bound_at_sync() {
    let (store, sb) = store().await;
    let admin = person(&store, "admin@example.com", &[]).await;
    source_with(
        &store,
        &sb,
        "a",
        admin,
        "c1",
        vec![skill("pdf", "pdf", &[])],
    )
    .await;
    let report = source_with(
        &store,
        &sb,
        "kits",
        admin,
        "k1",
        vec![
            docs_server(),
            toolkit(
                "finops",
                "Costs.",
                &[("docs", Some(&["search", "read"]))],
                &["pdf"],
            ),
        ],
    )
    .await;
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    assert_eq!(names(&report.new), ["docs", "finops"]);

    let kit = store.get_entry(&kit_key("kits", "finops")).await.unwrap();
    let applied = kit.applied.unwrap();
    assert_eq!(
        members(&store, applied.id).await,
        [
            (
                EntryKind::McpServer,
                "docs".to_string(),
                "kits/docs".to_string(),
                Some(vec!["search".to_string(), "read".to_string()])
            ),
            (
                EntryKind::Skill,
                "pdf".to_string(),
                "a/pdf".to_string(),
                None
            ),
        ]
    );
    let pdf = store.get_entry(&key("a", "pdf")).await.unwrap();
    assert_eq!(pdf.toolkits, [kit_key("kits", "finops")]);
    assert!(kit.toolkits.is_empty());
}

#[tokio::test]
async fn toolkit_references_stay_bound() {
    let (store, sb) = store().await;
    let admin = person(&store, "admin@example.com", &[]).await;
    source_with(
        &store,
        &sb,
        "a",
        admin,
        "c1",
        vec![skill("pdf", "pdf", &[])],
    )
    .await;
    let kit = || toolkit("kit", "One skill.", &[], &["pdf"]);
    source_with(&store, &sb, "kits", admin, "k1", vec![kit()]).await;
    // A second source with a pdf skill makes the bare name ambiguous.
    source_with(
        &store,
        &sb,
        "b",
        admin,
        "c1",
        vec![skill("pdf", "pdf b", &[])],
    )
    .await;

    let report = sync(&store, &sb, "kits", "k2", vec![kit()]).await;
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    assert_eq!(names(&report.unchanged), ["kit"]);
    let entry = store.get_entry(&kit_key("kits", "kit")).await.unwrap();
    assert!(entry.pending.is_none());
    let members = members(&store, entry.applied.unwrap().id).await;
    assert_eq!(members[0].2, "a/pdf");
}

#[tokio::test]
async fn a_toolkit_naming_no_entry_or_two_is_refused() {
    let (store, sb) = store().await;
    let admin = person(&store, "admin@example.com", &[]).await;
    let report = source_with(
        &store,
        &sb,
        "kits",
        admin,
        "k1",
        vec![toolkit("kit", "v1", &[("nope", None)], &["pdf@a"])],
    )
    .await;
    assert_eq!(report.refused.len(), 1, "{:?}", report.refused);
    let refusal = &report.refused[0];
    assert_eq!(refusal.kind, Some(EntryKind::Toolkit));
    assert_eq!(refusal.name.as_deref(), Some("kit"));
    assert_eq!(refusal.path, "kit.yaml");
    assert!(
        refusal.reason.contains("no MCP server named nope"),
        "{}",
        refusal.reason
    );
    assert!(
        refusal.reason.contains("no skill named pdf@a"),
        "{}",
        refusal.reason
    );
    assert!(matches!(
        store.get_entry(&kit_key("kits", "kit")).await,
        Err(StoreError::NotFound)
    ));

    // A version that binds, applied; then one naming a name two sources hold.
    source_with(
        &store,
        &sb,
        "a",
        admin,
        "c1",
        vec![skill("pdf", "pdf", &[])],
    )
    .await;
    sync(
        &store,
        &sb,
        "kits",
        "k2",
        vec![toolkit("kit", "v1", &[], &["pdf"])],
    )
    .await;
    // The source's first sync, whose toolkit was refused, applied itself.
    store
        .apply_source(&source_name("kits"), OffsetDateTime::now_utc())
        .await
        .unwrap();
    let applied = store
        .get_entry(&kit_key("kits", "kit"))
        .await
        .unwrap()
        .applied
        .unwrap();
    source_with(
        &store,
        &sb,
        "b",
        admin,
        "c1",
        vec![skill("pdf", "pdf b", &[])],
    )
    .await;
    let report = sync(
        &store,
        &sb,
        "kits",
        "k3",
        vec![toolkit("kit", "v2", &[], &["pdf"])],
    )
    .await;
    assert_eq!(report.refused.len(), 1, "{:?}", report.refused);
    assert_eq!(
        report.refused[0].reason,
        "skills[0]: pdf is in sources a, b: write pdf@a or pdf@b"
    );
    assert!(report.missing.is_empty(), "{:?}", report.missing);
    let entry = store.get_entry(&kit_key("kits", "kit")).await.unwrap();
    assert_eq!(entry.applied.unwrap().id, applied.id);
    assert!(entry.pending.is_none());

    // Two references to one entry.
    let report = sync(
        &store,
        &sb,
        "kits",
        "k4",
        vec![toolkit("kit", "v3", &[], &["pdf@a", "pdf@b", "pdf@a"])],
    )
    .await;
    assert_eq!(report.refused.len(), 1, "{:?}", report.refused);
    assert!(
        report.refused[0]
            .reason
            .contains("skills[2]: pdf@a names the same skill as skills[0]"),
        "{}",
        report.refused[0].reason
    );
}

#[tokio::test]
async fn a_toolkit_update_waits_for_apply() {
    let (store, sb) = store().await;
    let admin = person(&store, "admin@example.com", &[]).await;
    source_with(
        &store,
        &sb,
        "a",
        admin,
        "c1",
        vec![skill("pdf", "pdf", &[])],
    )
    .await;
    source_with(
        &store,
        &sb,
        "kits",
        admin,
        "k1",
        vec![
            skill("notes", "notes", &[]),
            toolkit("kit", "v1", &[], &["pdf"]),
        ],
    )
    .await;
    let report = sync(
        &store,
        &sb,
        "kits",
        "k2",
        vec![
            skill("notes", "notes", &[]),
            toolkit("kit", "v2", &[], &["pdf", "notes"]),
        ],
    )
    .await;
    assert_eq!(names(&report.updated), ["kit"]);
    let entry = store.get_entry(&kit_key("kits", "kit")).await.unwrap();
    let (applied, pending) = (entry.applied.unwrap(), entry.pending.unwrap());
    assert_eq!(members(&store, applied.id).await.len(), 1);
    assert_eq!(members(&store, pending.id).await.len(), 2);
    let notes = store.get_entry(&key("kits", "notes")).await.unwrap();
    assert!(notes.toolkits.is_empty(), "only applied revisions count");

    let done = store
        .apply_source(&source_name("kits"), OffsetDateTime::now_utc())
        .await
        .unwrap();
    assert_eq!(names(&done.updated), ["kit"]);
    let entry = store.get_entry(&kit_key("kits", "kit")).await.unwrap();
    assert_eq!(entry.applied.unwrap().id, pending.id);
    let notes = store.get_entry(&key("kits", "notes")).await.unwrap();
    assert_eq!(notes.toolkits, [kit_key("kits", "kit")]);
}

#[tokio::test]
async fn a_source_a_toolkit_uses_cannot_be_deleted() {
    let (store, sb) = store().await;
    let admin = person(&store, "admin@example.com", &[]).await;
    source_with(
        &store,
        &sb,
        "a",
        admin,
        "c1",
        vec![skill("pdf", "pdf", &[])],
    )
    .await;
    source_with(
        &store,
        &sb,
        "kits",
        admin,
        "k1",
        vec![
            skill("notes", "notes", &[]),
            toolkit("kit", "v1", &[], &["pdf", "notes"]),
        ],
    )
    .await;
    match store.delete_source(&source_name("a")).await {
        Err(StoreError::InUse(m)) => assert!(m.contains("toolkits/kits/kit"), "{m}"),
        other => panic!("expected InUse, got {other:?}"),
    }
    // A pending revision counts too.
    source_with(
        &store,
        &sb,
        "b",
        admin,
        "c1",
        vec![skill("calc", "calc", &[])],
    )
    .await;
    sync(
        &store,
        &sb,
        "kits",
        "k2",
        vec![
            skill("notes", "notes", &[]),
            toolkit("kit", "v2", &[], &["pdf", "notes", "calc"]),
        ],
    )
    .await;
    assert!(matches!(
        store.delete_source(&source_name("b")).await,
        Err(StoreError::InUse(_))
    ));
    // The toolkit's own source can go, and then the others.
    store.delete_source(&source_name("kits")).await.unwrap();
    store.delete_source(&source_name("a")).await.unwrap();
    store.delete_source(&source_name("b")).await.unwrap();
    assert_eq!(
        count(&store, "select count(*) from catalog_toolkit_members").await,
        0
    );
}
