//! People through the API: who may call it, their own environments and
//! keys, users, groups, local accounts, connectors, signing keys and
//! offboarding.

mod common;

use std::time::Duration;

use common::*;
use sekisho_auth::dex::DexError;
use sekisho_testkit::FakeDex;
use serde_json::{Value, json};
use uuid::Uuid;

fn field(reply: &Reply) -> &Value {
    &reply.body["fields"][0]["field"]
}

#[tokio::test]
async fn admin_routes_need_an_admin_session() {
    let app = app().await;
    let path = "/api/v1/environments";

    let none = app.call("GET", path, None, None).await;
    assert_eq!(
        (none.status, &none.body["error"]),
        (401, &json!("unauthorized"))
    );
    assert!(
        none.headers
            .get("www-authenticate")
            .is_some_and(|v| v.to_str().unwrap().starts_with("Bearer")),
        "{:?}",
        none.headers
    );
    let bootstrap = app.call("GET", path, Some(BOOTSTRAP), None).await;
    assert_eq!(bootstrap.status, 401);
    assert!(
        bootstrap.body["message"]
            .as_str()
            .unwrap()
            .contains("the bootstrap token only works on /setup"),
        "{}",
        bootstrap.text
    );
    for token in ["not-a-token", "sek_abcdefghijkl_xyz", GATEWAY] {
        assert_eq!(
            app.call("GET", path, Some(token), None).await.status,
            401,
            "{token}"
        );
    }

    // A person who isn't an admin may use their own routes only.
    let (ada, ada_token) = app.sign_in("ada@example.com", &["eng"]).await;
    let refused = app.by(&ada_token, "GET", path, None).await;
    assert_eq!(
        (refused.status, &refused.body["error"]),
        (403, &json!("forbidden"))
    );
    assert_eq!(
        app.by(&ada_token, "GET", "/api/v1/me", None).await.status,
        200
    );

    // A marked admin, and members of SEKISHO_ADMIN_GROUPS through their
    // identity provider or a Sekisho group.
    assert_eq!(app.admin("GET", path, None).await.status, 200);
    let (_, ops) = app.sign_in("ops@example.com", &[ADMIN_GROUP]).await;
    assert_eq!(app.by(&ops, "GET", path, None).await.status, 200);
    let (lee, lee_token) = app.sign_in("lee@example.com", &[]).await;
    assert_eq!(
        app.admin(
            "POST",
            "/api/v1/groups",
            Some(json!({ "name": ADMIN_GROUP }))
        )
        .await
        .status,
        201
    );
    let added = app
        .admin(
            "PUT",
            &format!("/api/v1/groups/{ADMIN_GROUP}/members/{lee}"),
            None,
        )
        .await;
    assert_eq!(added.status, 204, "{}", added.text);
    assert_eq!(app.by(&lee_token, "GET", path, None).await.status, 200);

    // Marks apply at the next request, both ways.
    let marked = app
        .admin(
            "PATCH",
            &format!("/api/v1/users/{ada}"),
            Some(json!({ "admin": true })),
        )
        .await;
    assert_eq!(marked.status, 200, "{}", marked.text);
    assert_eq!(app.by(&ada_token, "GET", path, None).await.status, 200);
    app.admin(
        "PATCH",
        &format!("/api/v1/users/{ada}"),
        Some(json!({ "admin": false })),
    )
    .await;
    assert_eq!(app.by(&ada_token, "GET", path, None).await.status, 403);
}

#[tokio::test]
async fn me_lists_only_usable_environments() {
    let app = app().await;
    app.provider_and_model().await;
    for (name, groups) in [
        ("team-a", json!(["eng"])),
        ("team-b", json!(["ops"])),
        ("nobody", json!([])),
    ] {
        let r = app
            .admin(
                "PUT",
                &format!("/api/v1/environments/{name}"),
                Some(json!({ "models": ["mock-model"], "access_groups": groups, "notice": "Be kind" })),
            )
            .await;
        assert_eq!(r.status, 200, "{}", r.text);
    }
    let (ada, token) = app.sign_in("ada@example.com", &["eng"]).await;
    let me = app.by(&token, "GET", "/api/v1/me", None).await;
    assert_eq!(me.body["id"], json!(ada));
    assert_eq!(me.body["email"], "ada@example.com");
    assert_eq!(me.body["local"], false);
    assert_eq!(me.body["admin"], false);
    assert_eq!(me.body["groups"], json!(["eng"]));

    let envs = app.by(&token, "GET", "/api/v1/me/environments", None).await;
    assert_eq!(
        envs.body,
        json!([{ "name": "team-a", "description": "", "notice": "Be kind", "models": ["mock-model"] }])
    );
    // Admins have no implicit access either.
    let envs = app.admin("GET", "/api/v1/me/environments", None).await;
    assert_eq!(envs.body.as_array().unwrap().len(), 1, "{}", envs.text);
}

#[tokio::test]
async fn keys_are_created_by_their_owner() {
    let app = app().await;
    app.provider_and_model().await;
    assert_eq!(app.put_env("team-a", &["mock-model"]).await.status, 200);
    let ops = app
        .admin(
            "PUT",
            "/api/v1/environments/team-b",
            Some(json!({ "access_groups": ["ops"] })),
        )
        .await;
    assert_eq!(ops.status, 200);
    let (ada, ada_token) = app.sign_in("ada@example.com", &["eng"]).await;
    let (_, bob_token) = app.sign_in("bob@example.com", &["eng"]).await;

    let made = app
        .by(
            &ada_token,
            "POST",
            "/api/v1/me/keys",
            Some(json!({ "environment": "team-a", "name": "laptop" })),
        )
        .await;
    assert_eq!(made.status, 201, "{}", made.text);
    assert!(made.body["token"].as_str().unwrap().starts_with("sek_"));
    assert_eq!(
        (&made.body["owner"], &made.body["owner_email"]),
        (&json!(ada), &json!("ada@example.com"))
    );
    let id = made.body["id"].as_str().unwrap().to_owned();
    let mine = app.by(&ada_token, "GET", "/api/v1/me/keys", None).await;
    assert_eq!(mine.body.as_array().unwrap().len(), 1);
    assert!(mine.body[0].get("token").is_none(), "{}", mine.text);

    // Only for themselves, and only where they may go; an environment they
    // can't use looks the same as one that doesn't exist.
    let owner_field = app
        .by(
            &ada_token,
            "POST",
            "/api/v1/me/keys",
            Some(json!({ "environment": "team-a", "name": "x", "owner": app.owner })),
        )
        .await;
    assert_eq!(owner_field.status, 400, "{}", owner_field.text);
    let mut said = Vec::new();
    for environment in ["team-b", "no-such-env"] {
        let r = app
            .by(
                &ada_token,
                "POST",
                "/api/v1/me/keys",
                Some(json!({ "environment": environment, "name": "x" })),
            )
            .await;
        assert_eq!(
            (r.status, field(&r)),
            (422, &json!("environment")),
            "{environment}: {}",
            r.text
        );
        said.push(
            r.body["fields"][0]["message"]
                .as_str()
                .unwrap()
                .replace(environment, "ENV"),
        );
    }
    assert_eq!(said[0], said[1]);

    // Someone else's key is not there for them.
    for action in ["suspend", "resume", "revoke"] {
        let r = app
            .by(
                &bob_token,
                "POST",
                &format!("/api/v1/me/keys/{id}/{action}"),
                None,
            )
            .await;
        assert_eq!(r.status, 404, "{action}");
    }
    assert_eq!(
        app.by(
            &ada_token,
            "POST",
            &format!("/api/v1/me/keys/{id}/suspend"),
            None
        )
        .await
        .status,
        204
    );
    assert_eq!(
        app.by(
            &ada_token,
            "POST",
            &format!("/api/v1/me/keys/{id}/resume"),
            None
        )
        .await
        .status,
        204
    );

    // Admins see and revoke everyone's keys, but create none for others.
    let listed = app
        .admin("GET", "/api/v1/environments/team-a/keys", None)
        .await;
    assert_eq!(
        listed.body[0]["owner_email"], "ada@example.com",
        "{}",
        listed.text
    );
    assert_eq!(
        app.admin("POST", &format!("/api/v1/keys/{id}/revoke"), None)
            .await
            .status,
        204
    );
    assert_eq!(
        app.by(&ada_token, "GET", "/api/v1/me/keys", None)
            .await
            .body[0]["state"],
        "revoked"
    );
    let gone = app
        .admin(
            "POST",
            "/api/v1/environments/team-a/keys",
            Some(json!({ "owner": ada, "name": "x" })),
        )
        .await;
    assert_eq!(gone.status, 405, "{}", gone.text);
}

#[tokio::test]
async fn environment_owner_defaults_to_the_creator() {
    let app = app().await;
    let r = app
        .admin(
            "PUT",
            "/api/v1/environments/team-a",
            Some(json!({ "access_groups": ["eng"] })),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text);
    assert_eq!(
        (&r.body["owner"], &r.body["owner_email"]),
        (&json!(app.owner), &json!("admin@example.com"))
    );

    let (ada, _) = app.sign_in("ada@example.com", &["eng"]).await;
    let r = app
        .admin(
            "PUT",
            "/api/v1/environments/team-a",
            Some(json!({ "access_groups": ["eng"], "owner": ada })),
        )
        .await;
    assert_eq!(r.body["owner"], json!(ada));
    // owner_email is ignored on the way in; a save without an owner keeps it.
    let r = app
        .admin(
            "PUT",
            "/api/v1/environments/team-a",
            Some(json!({ "access_groups": ["eng"], "owner_email": "someone@else.com" })),
        )
        .await;
    assert_eq!(
        (r.status, &r.body["owner"]),
        (200, &json!(ada)),
        "{}",
        r.text
    );

    for owner in [Uuid::from_u128(7), {
        let (lee, _) = app.sign_in("lee@example.com", &[]).await;
        app.store
            .set_disabled(&lee, true, time::OffsetDateTime::now_utc())
            .await
            .unwrap();
        lee
    }] {
        let r = app
            .admin(
                "PUT",
                "/api/v1/environments/team-a",
                Some(json!({ "owner": owner })),
            )
            .await;
        assert_eq!((r.status, field(&r)), (422, &json!("owner")), "{}", r.text);
    }
}

#[tokio::test]
async fn person_budgets_take_a_user_id() {
    let app = app().await;
    let (ada, _) = app.sign_in("ada@example.com", &["eng"]).await;
    let budget = |person: Value| json!({ "name": "monthly", "scope": "person", "person": person, "limit": "50", "window": "month" });
    let r = app
        .admin("POST", "/api/v1/budgets", Some(budget(json!(ada))))
        .await;
    assert_eq!(
        (r.status, &r.body["person"]),
        (201, &json!(ada)),
        "{}",
        r.text
    );
    for person in [json!(Uuid::from_u128(7)), json!("ada@example.com")] {
        let r = app
            .admin("POST", "/api/v1/budgets", Some(budget(person.clone())))
            .await;
        assert_eq!(
            (r.status, field(&r)),
            (422, &json!("person")),
            "{person}: {}",
            r.text
        );
    }
}

#[tokio::test]
async fn offboarding_needs_a_new_owner_when_the_person_owns_environments() {
    let app = app().await;
    app.provider_and_model().await;
    let (ada, ada_token) = app.sign_in("ada@example.com", &["eng"]).await;
    let (bob, _) = app.sign_in("bob@example.com", &["eng"]).await;
    for name in ["team-a", "team-c"] {
        let r = app
            .admin(
                "PUT",
                &format!("/api/v1/environments/{name}"),
                Some(json!({ "access_groups": ["eng"], "owner": ada })),
            )
            .await;
        assert_eq!(r.status, 200, "{}", r.text);
    }
    let key = app
        .by(
            &ada_token,
            "POST",
            "/api/v1/me/keys",
            Some(json!({ "environment": "team-a", "name": "laptop" })),
        )
        .await;
    let offboard = format!("/api/v1/users/{ada}/offboard");

    let refused = app.admin("POST", &offboard, Some(json!({}))).await;
    assert_eq!(
        (refused.status, field(&refused)),
        (422, &json!("new_owner")),
        "{}",
        refused.text
    );
    let message = refused.body["fields"][0]["message"].as_str().unwrap();
    assert!(
        message.contains("team-a") && message.contains("team-c"),
        "{message}"
    );
    for bad in [json!(ada), json!(Uuid::from_u128(7))] {
        let r = app
            .admin("POST", &offboard, Some(json!({ "new_owner": bad })))
            .await;
        assert_eq!((r.status, field(&r)), (422, &json!("new_owner")), "{bad}");
    }
    // Nothing happened yet.
    assert_eq!(
        app.by(&ada_token, "GET", "/api/v1/me", None).await.status,
        200
    );

    let done = app
        .admin("POST", &offboard, Some(json!({ "new_owner": bob })))
        .await;
    assert_eq!(done.status, 200, "{}", done.text);
    assert_eq!(done.body["ended_sessions"], 1);
    assert_eq!(done.body["revoked_keys"], json!([key.body["id"]]));
    assert_eq!(done.body["handed_over"], json!(["team-a", "team-c"]));
    assert_eq!(done.body["dex_refresh_revoked"], true);
    assert_eq!(app.dex.revoked(), vec!["test|ada@example.com".to_owned()]);

    assert_eq!(
        app.by(&ada_token, "GET", "/api/v1/me", None).await.status,
        401
    );
    let env = app.admin("GET", "/api/v1/environments/team-a", None).await;
    assert_eq!(env.body["owner"], json!(bob));
    let user = app
        .admin("GET", &format!("/api/v1/users/{ada}"), None)
        .await;
    assert_eq!(user.body["disabled"], true);
}

#[tokio::test]
async fn offboarding_completes_when_dex_is_down() {
    let app = app().await;
    let (ada, ada_token) = app.sign_in("ada@example.com", &["eng"]).await;
    app.dex
        .fail_grpc(Some(DexError::Unreachable("gRPC call timed out".into())));
    let done = app
        .admin(
            "POST",
            &format!("/api/v1/users/{ada}/offboard"),
            Some(json!({})),
        )
        .await;
    assert_eq!(done.status, 200, "{}", done.text);
    assert_eq!(done.body["dex_refresh_revoked"], false);
    assert_eq!(done.body["ended_sessions"], 1);
    assert_eq!(
        app.by(&ada_token, "GET", "/api/v1/me", None).await.status,
        401
    );
}

#[tokio::test]
async fn admins_cannot_remove_themselves() {
    let app = app().await;
    let me = format!("/api/v1/users/{}", app.owner);
    for body in [json!({ "admin": false }), json!({ "disabled": true })] {
        let r = app.admin("PATCH", &me, Some(body.clone())).await;
        assert_eq!(
            (r.status, &r.body["error"]),
            (409, &json!("conflict")),
            "{body}"
        );
    }
    let r = app
        .admin("POST", &format!("{me}/offboard"), Some(json!({})))
        .await;
    assert_eq!(r.status, 409);
    assert_eq!(
        app.admin("GET", "/api/v1/users", None).await.status,
        200,
        "still an admin"
    );

    // An admin only through a Sekisho group can't leave or delete it.
    let (ops, ops_token) = app.sign_in("ops@example.com", &[]).await;
    app.admin(
        "POST",
        "/api/v1/groups",
        Some(json!({ "name": ADMIN_GROUP })),
    )
    .await;
    let member = format!("/api/v1/groups/{ADMIN_GROUP}/members/{ops}");
    assert_eq!(app.admin("PUT", &member, None).await.status, 204);
    assert_eq!(
        app.by(&ops_token, "DELETE", &member, None).await.status,
        409
    );
    let r = app
        .by(
            &ops_token,
            "DELETE",
            &format!("/api/v1/groups/{ADMIN_GROUP}"),
            None,
        )
        .await;
    assert_eq!(r.status, 409, "{}", r.text);
    assert_eq!(
        app.by(&ops_token, "GET", "/api/v1/users", None)
            .await
            .status,
        200
    );
    // Another admin may.
    assert_eq!(app.admin("DELETE", &member, None).await.status, 204);
    assert_eq!(
        app.by(&ops_token, "GET", "/api/v1/users", None)
            .await
            .status,
        403
    );

    // Nor delete their own local account.
    let local = app
        .admin(
            "POST",
            "/api/v1/local-accounts",
            Some(json!({ "email": "root@example.com" })),
        )
        .await;
    let root = local.body["user"]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    app.store.set_admin(&root, true).await.unwrap();
    let (root_token, _) =
        sekisho_testkit::people::session_token(&app.store, &app.sb, ISSUER, root).await;
    let r = app
        .by(
            &root_token,
            "DELETE",
            &format!("/api/v1/local-accounts/{root}"),
            None,
        )
        .await;
    assert_eq!(r.status, 409, "{}", r.text);
}

#[tokio::test]
async fn local_accounts_api() {
    let app = app().await;
    let made = app
        .admin(
            "POST",
            "/api/v1/local-accounts",
            Some(json!({ "email": " Lee@Example.com ", "name": "Lee" })),
        )
        .await;
    assert_eq!(made.status, 201, "{}", made.text);
    assert_eq!(
        (&made.body["user"]["email"], &made.body["user"]["local"]),
        (&json!("lee@example.com"), &json!(true))
    );
    let setup = made.body["setup_url"].as_str().unwrap().to_owned();
    assert!(setup.starts_with(&format!("{ISSUER}/setup/")), "{setup}");
    assert!(made.body["expires_at"].is_string());
    let id = made.body["user"]["id"].as_str().unwrap().to_owned();
    // The setup page is served by the same server.
    let path = setup.strip_prefix(ISSUER).unwrap();
    assert_eq!(app.call("GET", path, None, None).await.status, 200);

    let again = app
        .admin(
            "POST",
            "/api/v1/local-accounts",
            Some(json!({ "email": "lee@example.com" })),
        )
        .await;
    assert_eq!(again.status, 409, "{}", again.text);
    let bad = app
        .admin(
            "POST",
            "/api/v1/local-accounts",
            Some(json!({ "email": "not an address" })),
        )
        .await;
    assert_eq!((bad.status, field(&bad)), (422, &json!("email")));

    let reset = app
        .admin("POST", &format!("/api/v1/local-accounts/{id}/reset"), None)
        .await;
    assert_eq!(reset.status, 200, "{}", reset.text);
    let new_setup = reset.body["setup_url"].as_str().unwrap();
    assert_ne!(new_setup, setup);
    assert_eq!(
        app.call("GET", path, None, None).await.status,
        404,
        "the old link"
    );

    app.dex
        .fail_grpc(Some(DexError::Unreachable("gRPC call timed out".into())));
    let down = app
        .admin("POST", &format!("/api/v1/local-accounts/{id}/reset"), None)
        .await;
    assert_eq!(
        (down.status, &down.body["error"]),
        (503, &json!("unavailable"))
    );
    app.dex.fail_grpc(None);

    let (github, _) = app.sign_in("ada@example.com", &[]).await;
    for path in [
        format!("/api/v1/local-accounts/{github}/reset"),
        format!("/api/v1/local-accounts/{}/reset", Uuid::from_u128(7)),
    ] {
        assert_eq!(app.admin("POST", &path, None).await.status, 404, "{path}");
    }

    assert_eq!(
        app.admin("DELETE", &format!("/api/v1/local-accounts/{id}"), None)
            .await
            .status,
        204
    );
    let user = app.admin("GET", &format!("/api/v1/users/{id}"), None).await;
    assert_eq!(user.body["disabled"], true);
}

#[tokio::test]
async fn groups_api() {
    let app = app().await;
    let (ada, ada_token) = app.sign_in("ada@example.com", &["eng"]).await;
    let made = app
        .admin(
            "POST",
            "/api/v1/groups",
            Some(json!({ "name": "data-team" })),
        )
        .await;
    assert_eq!(made.status, 201, "{}", made.text);
    assert_eq!(
        app.admin(
            "POST",
            "/api/v1/groups",
            Some(json!({ "name": "data-team" }))
        )
        .await
        .status,
        409
    );
    for name in ["Data", "acme:team", "-x", ""] {
        let r = app
            .admin("POST", "/api/v1/groups", Some(json!({ "name": name })))
            .await;
        assert_eq!((r.status, field(&r)), (422, &json!("name")), "{name:?}");
    }
    let member = format!("/api/v1/groups/data-team/members/{ada}");
    assert_eq!(app.admin("PUT", &member, None).await.status, 204);
    assert_eq!(
        app.admin("PUT", &member, None).await.status,
        204,
        "again is fine"
    );
    let listed = app.admin("GET", "/api/v1/groups", None).await;
    assert_eq!(
        listed.body,
        json!([{ "name": "data-team", "members": [ada] }])
    );
    let me = app.by(&ada_token, "GET", "/api/v1/me", None).await;
    assert_eq!(me.body["groups"], json!(["data-team", "eng"]));

    let unknown = app
        .admin(
            "PUT",
            &format!("/api/v1/groups/data-team/members/{}", Uuid::from_u128(7)),
            None,
        )
        .await;
    assert_eq!(unknown.status, 422, "{}", unknown.text);
    assert_eq!(
        app.admin(
            "PUT",
            &format!("/api/v1/groups/no-group/members/{ada}"),
            None
        )
        .await
        .status,
        404
    );

    assert_eq!(app.admin("DELETE", &member, None).await.status, 204);
    assert_eq!(
        app.admin("DELETE", "/api/v1/groups/data-team", None)
            .await
            .status,
        204
    );
    assert_eq!(
        app.admin("DELETE", "/api/v1/groups/data-team", None)
            .await
            .status,
        404
    );
    assert_eq!(
        app.admin("GET", "/api/v1/groups", None).await.body,
        json!([])
    );
}

#[tokio::test]
async fn connectors_api_never_returns_config() {
    let app = app().await;
    let secret = "github-client-secret-value";
    let config = json!({ "clientID": "abc", "clientSecret": secret, "redirectURI": "https://dex.test/callback" });
    let put = app
        .admin(
            "PUT",
            "/api/v1/connectors/github",
            Some(json!({ "type": "github", "name": "GitHub", "config": config })),
        )
        .await;
    assert_eq!(put.status, 200, "{}", put.text);
    assert_eq!(
        put.body,
        json!({ "id": "github", "type": "github", "name": "GitHub" })
    );
    let listed = app.admin("GET", "/api/v1/connectors", None).await;
    assert_eq!(
        listed.body,
        json!([{ "id": "github", "type": "github", "name": "GitHub" }])
    );
    assert!(!listed.text.contains(secret) && !put.text.contains(secret));

    for (id, kind, config, wrong) in [
        ("local", "github", json!({}), "id"),
        ("Git-Hub", "github", json!({}), "id"),
        ("keystone", "keystone", json!({}), "type"),
        ("github", "github", json!("not an object"), "config"),
    ] {
        let r = app
            .admin(
                "PUT",
                &format!("/api/v1/connectors/{id}"),
                Some(json!({ "type": kind, "name": "x", "config": config })),
            )
            .await;
        assert_eq!(
            (r.status, field(&r)),
            (422, &json!(wrong)),
            "{id} {kind}: {}",
            r.text
        );
    }

    // The owner signed in through GitHub: deleting it would end their own
    // session, so another admin has to.
    let own = app.admin("DELETE", "/api/v1/connectors/github", None).await;
    assert_eq!(
        (own.status, &own.body["error"]),
        (409, &json!("conflict")),
        "{}",
        own.text
    );
    let sign_in = sekisho_store::SignIn {
        subject: "gitlab|kim".into(),
        email: "kim@example.com".into(),
        name: "Kim".into(),
        groups: vec![ADMIN_GROUP.into()],
        connector_id: "gitlab".into(),
        connector_user_id: "kim".into(),
    };
    let kim = match app
        .store
        .sign_in(&sign_in, time::OffsetDateTime::now_utc())
        .await
        .unwrap()
    {
        sekisho_store::SignInOutcome::User(user) => user.id,
        other => panic!("{other:?}"),
    };
    let (kim_token, _) =
        sekisho_testkit::people::session_token(&app.store, &app.sb, ISSUER, kim).await;

    // Deleting it ends the sessions of the people who signed in through it.
    let (_, ada_token) = app.sign_in("ada@example.com", &["eng"]).await;
    assert_eq!(
        app.by(&kim_token, "DELETE", "/api/v1/connectors/github", None)
            .await
            .status,
        204
    );
    for token in [&ada_token, &app.token] {
        assert_eq!(app.until_status(token, "/api/v1/me", 401).await.status, 401);
    }
    assert_eq!(
        app.by(&kim_token, "GET", "/api/v1/me", None).await.status,
        200
    );
    assert_eq!(
        app.by(&kim_token, "DELETE", "/api/v1/connectors/github", None)
            .await
            .status,
        404
    );

    app.dex
        .fail_grpc(Some(DexError::Unavailable("gRPC Internal".into())));
    let down = app.by(&kim_token, "GET", "/api/v1/connectors", None).await;
    assert_eq!(down.status, 503);
}

#[tokio::test]
async fn users_api_disables_and_ends_sessions() {
    let app = app().await;
    let (ada, ada_token) = app.sign_in("ada@example.com", &["eng"]).await;
    let (_, ops_token) = app.sign_in("ops@example.com", &[ADMIN_GROUP]).await;
    let users = app.admin("GET", "/api/v1/users", None).await;
    assert_eq!(users.body.as_array().unwrap().len(), 3, "{}", users.text);
    let one = app
        .admin("GET", &format!("/api/v1/users/{ada}"), None)
        .await;
    for field in [
        "id",
        "email",
        "name",
        "local",
        "admin",
        "admin_by_group",
        "groups",
        "connector",
        "disabled",
        "disabled_at",
        "idp_refused_at",
        "last_sign_in_at",
        "created_at",
    ] {
        assert!(one.body.get(field).is_some(), "{field}: {}", one.text);
    }
    assert_eq!(
        (&one.body["connector"], &one.body["admin"]),
        (&json!("github"), &json!(false))
    );
    let ops = app.by(&ops_token, "GET", "/api/v1/me", None).await;
    let ops_user = app
        .admin(
            "GET",
            &format!("/api/v1/users/{}", ops.body["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(
        (&ops_user.body["admin"], &ops_user.body["admin_by_group"]),
        (&json!(false), &json!(true))
    );

    let off = app
        .admin(
            "PATCH",
            &format!("/api/v1/users/{ada}"),
            Some(json!({ "disabled": true })),
        )
        .await;
    assert_eq!(
        (off.status, &off.body["disabled"]),
        (200, &json!(true)),
        "{}",
        off.text
    );
    assert_eq!(
        app.by(&ada_token, "GET", "/api/v1/me", None).await.status,
        401
    );
    // Enabled again, the person signs in again: the old session stays ended.
    let on = app
        .admin(
            "PATCH",
            &format!("/api/v1/users/{ada}"),
            Some(json!({ "disabled": false })),
        )
        .await;
    assert_eq!(on.body["disabled"], false);
    assert_eq!(
        app.until_status(&ada_token, "/api/v1/me", 401).await.status,
        401
    );
    assert_eq!(
        app.admin(
            "GET",
            &format!("/api/v1/users/{}", Uuid::from_u128(7)),
            None
        )
        .await
        .status,
        404
    );
    let unknown_field = app
        .admin(
            "PATCH",
            &format!("/api/v1/users/{ada}"),
            Some(json!({ "email": "x@y.z" })),
        )
        .await;
    assert_eq!(unknown_field.status, 400);
}

#[tokio::test]
async fn signing_key_rotation_on_demand() {
    let app = app().await;
    let keys = |r: &Reply| r.body["keys"].as_array().unwrap().len();
    let before = app.call("GET", "/oauth/jwks", None, None).await;
    assert_eq!(keys(&before), 1);
    let (_, ada_token) = app.sign_in("ada@example.com", &[]).await;
    assert_eq!(
        app.by(&ada_token, "POST", "/api/v1/signing-keys/rotate", None)
            .await
            .status,
        403
    );
    assert_eq!(
        app.admin("POST", "/api/v1/signing-keys/rotate", None)
            .await
            .status,
        204
    );
    // The new key is published before it signs; tokens of the old one work.
    let after = app.call("GET", "/oauth/jwks", None, None).await;
    assert_eq!(keys(&after), 2, "{}", after.text);
    assert_eq!(app.admin("GET", "/api/v1/users", None).await.status, 200);
}

#[tokio::test]
async fn sign_in_waits_for_dex_registration() {
    let dex = FakeDex::new();
    dex.fail_grpc(Some(DexError::Unreachable("gRPC call timed out".into())));
    let app = app_with(dex.clone()).await;
    let authorize = "/oauth/authorize?response_type=code&client_id=sekisho-cli\
                     &redirect_uri=http%3A%2F%2F127.0.0.1%3A9%2Fcb\
                     &code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM\
                     &code_challenge_method=S256";
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(app.call("GET", authorize, None, None).await.status, 503);
    let device = app
        .http
        .post(format!("{}/oauth/device_authorization", app.base))
        .form(&[("client_id", "sekisho-cli")])
        .send()
        .await
        .unwrap();
    assert_eq!(device.status().as_u16(), 503);
    // Metadata and the signing keys are there meanwhile.
    assert_eq!(
        app.call("GET", "/.well-known/oauth-authorization-server", None, None)
            .await
            .status,
        200
    );

    dex.fail_grpc(None);
    app.eventually(Duration::from_secs(5), "registered with Dex", || {
        app.state.dex_ready()
    })
    .await;
    assert_eq!(app.call("GET", authorize, None, None).await.status, 302);
}
