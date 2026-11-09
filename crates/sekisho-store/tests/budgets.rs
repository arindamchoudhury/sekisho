mod common;

use std::collections::HashSet;
use std::time::Duration;

use sekisho_core::keys::EnvKeyId;
use sekisho_core::money::MicroUsd;
use sekisho_core::names::EnvName;
use sekisho_core::window::{BudgetWindow, WindowKind};
use sekisho_store::{
    Budget, BudgetScope, ChangeKind, Charged, EnvironmentSpec, FieldError, NewBudget, Store,
    StoreError,
};
use time::OffsetDateTime;
use time::macros::datetime;
use tokio_stream::StreamExt;
use uuid::Uuid;

const NOW: OffsetDateTime = datetime!(2026-11-08 12:00 UTC);

async fn env(store: &Store, name: &str) -> EnvName {
    let spec = EnvironmentSpec {
        name: EnvName::parse(name).unwrap(),
        description: String::new(),
        notice: String::new(),
        access_groups: Vec::new(),
        models: Vec::new(),
    };
    store.upsert_environment(spec).await.unwrap().spec.name
}

fn new_budget(
    name: &str,
    scope: BudgetScope,
    usd: &str,
    kind: WindowKind,
    calendar: bool,
) -> NewBudget {
    NewBudget {
        name: name.into(),
        scope,
        limit: MicroUsd::from_usd_str(usd).unwrap(),
        window: BudgetWindow { kind, calendar },
    }
}

async fn budget(store: &Store, scope: BudgetScope, usd: &str, kind: WindowKind) -> Budget {
    store
        .create_budget(new_budget("team", scope, usd, kind, true))
        .await
        .unwrap()
}

fn person(person: &str, environment: Option<&EnvName>) -> BudgetScope {
    BudgetScope::Person {
        person: person.into(),
        environment: environment.cloned(),
    }
}

fn fields(err: StoreError) -> Vec<String> {
    match err {
        StoreError::Validation(f) => f
            .into_iter()
            .map(|FieldError { field, .. }| field)
            .collect(),
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[tokio::test]
async fn applicable_budgets_by_scope() {
    let (store, _sb) = common::store().await;
    let data = env(&store, "data-team").await;
    let ml = env(&store, "ml-team").await;
    let (key, _) = store
        .create_key(&data, "alice@example.com", "ci", None)
        .await
        .unwrap();
    let (other_key, _) = store
        .create_key(&data, "bob@example.com", "ci-2", None)
        .await
        .unwrap();
    for (name, scope) in [
        ("env", BudgetScope::Environment(data.clone())),
        ("key", BudgetScope::Key(key.id.clone())),
        ("person-in-env", person("alice@example.com", Some(&data))),
        ("person-global", person("alice@example.com", None)),
        ("person-other-env", person("alice@example.com", Some(&ml))),
        ("other-env", BudgetScope::Environment(ml.clone())),
        ("other-key", BudgetScope::Key(other_key.id.clone())),
        ("other-person", person("bob@example.com", None)),
    ] {
        store
            .create_budget(new_budget(name, scope, "10", WindowKind::Month, true))
            .await
            .unwrap();
    }
    let applicable: HashSet<String> = store
        .applicable_budgets(&data, &key.id, "alice@example.com")
        .await
        .unwrap()
        .into_iter()
        .map(|b| b.name)
        .collect();
    assert_eq!(
        applicable,
        HashSet::from(["env", "key", "person-in-env", "person-global"].map(String::from))
    );
}

#[tokio::test]
async fn charge_and_exhaust() {
    let (store, _sb) = common::store().await;
    let env = env(&store, "data-team").await;
    let now = NOW;
    let b = budget(
        &store,
        BudgetScope::Environment(env.clone()),
        "0.01",
        WindowKind::Month,
    )
    .await;
    for _ in 0..3 {
        store.charge(&[b.id], MicroUsd(4500), now).await.unwrap();
    }
    let s = &store.status(std::slice::from_ref(&b), now).await.unwrap()[0];
    assert_eq!(s.spent, MicroUsd(13500));
    assert!(s.exhausted());
    assert_eq!(s.resets_at, datetime!(2026-12-01 0:00 UTC)); // now = 2026-11-08, calendar month
    let c = store.charge(&[b.id], MicroUsd(500), now).await.unwrap();
    assert_eq!(
        c,
        vec![Charged {
            budget: b.id,
            before: MicroUsd(13500),
            after: MicroUsd(14000)
        }]
    );
}

#[tokio::test]
async fn concurrent_charges_are_not_lost() {
    let (store, _sb) = common::store().await;
    let env = env(&store, "data-team").await;
    let now = NOW;
    let b = budget(
        &store,
        BudgetScope::Environment(env),
        "1",
        WindowKind::Month,
    )
    .await;
    let tasks: Vec<_> = (0..50)
        .map(|_| {
            let s = store.clone();
            let id = b.id;
            tokio::spawn(async move { s.charge(&[id], MicroUsd(100), now).await })
        })
        .collect();
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    assert_eq!(
        store.status(&[b], now).await.unwrap()[0].spent,
        MicroUsd(5000)
    );
}

#[tokio::test]
async fn threshold_crossed_exactly_once() {
    let (store, _sb) = common::store().await;
    let env = env(&store, "data-team").await;
    let now = NOW;
    let b = budget(
        &store,
        BudgetScope::Environment(env),
        "1",
        WindowKind::Month,
    )
    .await;
    // 50 concurrent charges of 100: exactly one sees spend pass 3200, standing in for any alert threshold
    let tasks: Vec<_> = (0..50)
        .map(|_| {
            let s = store.clone();
            let id = b.id;
            tokio::spawn(async move { s.charge(&[id], MicroUsd(100), now).await })
        })
        .collect();
    let mut crossed = 0;
    for t in tasks {
        for c in t.await.unwrap().unwrap() {
            if c.before < MicroUsd(3200) && c.after >= MicroUsd(3200) {
                crossed += 1;
            }
        }
    }
    assert_eq!(crossed, 1);
}

#[tokio::test]
async fn concurrent_charges_across_budgets_in_any_order() {
    // Two budgets charged together in opposite orders must neither deadlock nor lose spend.
    let (store, _sb) = common::store().await;
    let env = env(&store, "data-team").await;
    let a = budget(
        &store,
        BudgetScope::Environment(env.clone()),
        "1",
        WindowKind::Month,
    )
    .await;
    let b = budget(
        &store,
        person("alice@example.com", None),
        "1",
        WindowKind::Month,
    )
    .await;
    let tasks: Vec<_> = (0..40)
        .map(|i| {
            let s = store.clone();
            let ids = if i % 2 == 0 {
                [a.id, b.id]
            } else {
                [b.id, a.id]
            };
            tokio::spawn(async move { s.charge(&ids, MicroUsd(10), NOW).await })
        })
        .collect();
    for t in tasks {
        assert_eq!(t.await.unwrap().unwrap().len(), 2);
    }
    let spent: Vec<MicroUsd> = store
        .status(&[a, b], NOW)
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.spent)
        .collect();
    assert_eq!(spent, [MicroUsd(400), MicroUsd(400)]);
}

#[tokio::test]
async fn spend_resets_in_new_window() {
    let (store, _sb) = common::store().await;
    let env = env(&store, "data-team").await;
    let b = budget(
        &store,
        BudgetScope::Environment(env),
        "0.01",
        WindowKind::Month,
    )
    .await;
    store.charge(&[b.id], MicroUsd(4500), NOW).await.unwrap();
    let last_second = &store
        .status(std::slice::from_ref(&b), datetime!(2026-11-30 23:59:59 UTC))
        .await
        .unwrap()[0];
    assert_eq!(last_second.spent, MicroUsd(4500));
    let december = &store
        .status(std::slice::from_ref(&b), datetime!(2026-12-01 0:00 UTC))
        .await
        .unwrap()[0];
    assert_eq!(december.spent, MicroUsd(0));
    assert_eq!(december.resets_at, datetime!(2027-01-01 0:00 UTC));
    assert!(!december.exhausted());
    // A charge in December counts there, not in November.
    let c = store
        .charge(&[b.id], MicroUsd(100), datetime!(2026-12-01 0:00 UTC))
        .await
        .unwrap();
    assert_eq!((c[0].before, c[0].after), (MicroUsd(0), MicroUsd(100)));
    assert_eq!(
        store.status(&[b], NOW).await.unwrap()[0].spent,
        MicroUsd(4500)
    );
}

#[tokio::test]
async fn rolling_window_counts_from_creation() {
    let (store, _sb) = common::store().await;
    let env = env(&store, "data-team").await;
    let b = store
        .create_budget(new_budget(
            "daily",
            BudgetScope::Environment(env),
            "1",
            WindowKind::Day,
            false,
        ))
        .await
        .unwrap();
    let hour = time::Duration::HOUR;
    store
        .charge(&[b.id], MicroUsd(700), b.created_at + hour)
        .await
        .unwrap();
    let same_day = &store
        .status(std::slice::from_ref(&b), b.created_at + 23 * hour)
        .await
        .unwrap()[0];
    assert_eq!(
        (same_day.spent, same_day.resets_at),
        (MicroUsd(700), b.created_at + 24 * hour)
    );
    let next_day = &store
        .status(std::slice::from_ref(&b), b.created_at + 25 * hour)
        .await
        .unwrap()[0];
    assert_eq!(
        (next_day.spent, next_day.resets_at),
        (MicroUsd(0), b.created_at + 48 * hour)
    );
}

#[tokio::test]
async fn budget_crud_validation_and_notifications() {
    let (store, _sb) = common::store().await;
    let data = env(&store, "data-team").await;
    let (key, _) = store
        .create_key(&data, "alice@example.com", "ci", None)
        .await
        .unwrap();
    let mut rx = store.listen().await.unwrap();

    let mut bad = new_budget("", person("", None), "1", WindowKind::Week, true);
    bad.limit = MicroUsd(-1);
    assert_eq!(
        fields(store.create_budget(bad).await.unwrap_err()),
        ["name", "person", "limit"]
    );
    let unknown_env = BudgetScope::Environment(EnvName::parse("nope").unwrap());
    assert_eq!(
        fields(
            store
                .create_budget(new_budget("x", unknown_env, "1", WindowKind::Week, true))
                .await
                .unwrap_err()
        ),
        ["environment"]
    );
    let unknown_person_env = person("alice@example.com", Some(&EnvName::parse("nope").unwrap()));
    assert_eq!(
        fields(
            store
                .create_budget(new_budget(
                    "x",
                    unknown_person_env,
                    "1",
                    WindowKind::Week,
                    true
                ))
                .await
                .unwrap_err()
        ),
        ["environment"]
    );
    let unknown_key = BudgetScope::Key(EnvKeyId::parse("aaaaaaaaaaaa").unwrap());
    assert_eq!(
        fields(
            store
                .create_budget(new_budget("x", unknown_key, "1", WindowKind::Week, true))
                .await
                .unwrap_err()
        ),
        ["key"]
    );

    let weekly = store
        .create_budget(new_budget(
            "weekly",
            BudgetScope::Key(key.id.clone()),
            "5",
            WindowKind::Week,
            false,
        ))
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), rx.next())
            .await
            .unwrap(),
        Some(ChangeKind::Budgets)
    );
    assert_eq!(
        (weekly.name.as_str(), weekly.limit),
        ("weekly", MicroUsd(5_000_000))
    );
    assert_eq!(
        weekly.window,
        BudgetWindow {
            kind: WindowKind::Week,
            calendar: false
        }
    );
    let alice = store
        .create_budget(new_budget(
            "alice",
            person("alice@example.com", Some(&data)),
            "2",
            WindowKind::Day,
            true,
        ))
        .await
        .unwrap();
    let team = store
        .create_budget(new_budget(
            "team",
            BudgetScope::Environment(data.clone()),
            "0",
            WindowKind::Month,
            true,
        ))
        .await
        .unwrap();
    assert_eq!(
        store.list_budgets().await.unwrap(),
        vec![alice.clone(), team.clone(), weekly.clone()]
    ); // by name
    // A zero limit is exhausted before any spend.
    assert!(
        store
            .status(std::slice::from_ref(&team), NOW)
            .await
            .unwrap()[0]
            .exhausted()
    );

    store.delete_budget(weekly.id).await.unwrap();
    assert_eq!(store.list_budgets().await.unwrap(), vec![alice, team]);
    assert!(matches!(
        store.delete_budget(weekly.id).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store.delete_budget(Uuid::nil()).await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn charge_edge_cases() {
    let (store, _sb) = common::store().await;
    let env = env(&store, "data-team").await;
    let a = budget(
        &store,
        BudgetScope::Environment(env.clone()),
        "1",
        WindowKind::Month,
    )
    .await;
    let b = budget(
        &store,
        person("alice@example.com", None),
        "1",
        WindowKind::Week,
    )
    .await;

    let both = store
        .charge(&[a.id, b.id], MicroUsd(250), NOW)
        .await
        .unwrap();
    let both: HashSet<(Uuid, MicroUsd)> = both.into_iter().map(|c| (c.budget, c.after)).collect();
    assert_eq!(
        both,
        HashSet::from([(a.id, MicroUsd(250)), (b.id, MicroUsd(250))])
    );

    assert_eq!(store.charge(&[a.id], MicroUsd(0), NOW).await.unwrap(), []);
    assert_eq!(
        fields(store.charge(&[a.id], MicroUsd(-1), NOW).await.unwrap_err()),
        ["amount"]
    );
    store.delete_budget(b.id).await.unwrap();
    let after_delete = store
        .charge(&[b.id, a.id], MicroUsd(50), NOW)
        .await
        .unwrap();
    assert_eq!(
        after_delete,
        vec![Charged {
            budget: a.id,
            before: MicroUsd(250),
            after: MicroUsd(300)
        }]
    );
    assert_eq!(store.charge(&[], MicroUsd(50), NOW).await.unwrap(), []);
}

#[tokio::test]
async fn get_budget_by_id() {
    let (store, _sb) = common::store().await;
    let env = env(&store, "data-team").await;
    let b = budget(
        &store,
        BudgetScope::Environment(env),
        "1",
        WindowKind::Month,
    )
    .await;
    assert_eq!(store.get_budget(b.id).await.unwrap(), b);
    assert!(matches!(
        store.get_budget(Uuid::nil()).await,
        Err(StoreError::NotFound)
    ));
}
