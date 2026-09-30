//! End-to-end checks of the auth slice over the real HTTP stack.

use axum::Router;
use axum::body::to_bytes;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use dati_app::{api, state::AppState};
use dati_core::config::{Config, MetaKind};
use dati_core::db::Db;
use dati_core::time::new_id;

struct TestApp {
    router: Router,
    db: Db,
}

async fn boot() -> TestApp {
    let dir = tempfile::tempdir().expect("tempdir");
    let url = format!(
        "sqlite:{}/meta.db?mode=rwc",
        dir.path().to_string_lossy().replace('\\', "/")
    );
    // The pool keeps the file open; dropping the guard would delete it mid-test.
    std::mem::forget(dir);
    let db = Db::connect(&url, MetaKind::Sqlite).await.expect("connect");
    let config = Config::from_source(|key| (key == "ADMIN_USERS").then(|| "root".to_string()));
    TestApp {
        router: api::router(AppState::new(db.clone(), config)),
        db,
    }
}

struct Reply {
    status: StatusCode,
    body: Value,
    headers: HeaderMap,
}

impl Reply {
    fn code(&self) -> &str {
        self.body["code"].as_str().unwrap_or_default()
    }

    fn message(&self) -> &str {
        self.body["message"].as_str().unwrap_or_default()
    }
}

/// One request through the router; `token` is sent as a bearer credential when given.
async fn call(
    app: &Router,
    method: Method,
    uri: &str,
    body: Option<&Value>,
    token: Option<&str>,
) -> Reply {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let payload = match body {
        Some(value) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            axum::body::Body::from(value.to_string())
        }
        None => axum::body::Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(payload).expect("valid request"))
        .await
        .expect("infallible router");
    let (parts, body) = response.into_parts();
    let bytes = to_bytes(body, 1 << 20).await.expect("readable body");
    Reply {
        status: parts.status,
        body: if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).expect("json body")
        },
        headers: parts.headers,
    }
}

async fn register(app: &Router, name: &str, password: &str) -> String {
    let reply = call(
        app,
        Method::POST,
        "/v1/auth/register",
        Some(&json!({ "name": name, "password": password, "display_name": name })),
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    reply.body["id"].as_str().expect("id").to_string()
}

async fn login(app: &Router, name: &str, password: &str) -> String {
    let reply = call(
        app,
        Method::POST,
        "/v1/auth/login",
        Some(&json!({ "type": "local", "name": name, "password": password })),
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    reply.body["token"].as_str().expect("token").to_string()
}

#[tokio::test]
async fn protected_calls_answer_with_a_bare_401() {
    let app = boot().await;
    for (method, uri) in [
        (Method::GET, "/v1/auth/me"),
        (Method::GET, "/v1/auth/api-keys"),
        (Method::GET, "/v1/users/search?keyword=abc"),
        (Method::GET, "/v1/system/config"),
    ] {
        let reply = call(&app.router, method.clone(), uri, None, None).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{method} {uri}");
        assert_eq!(reply.body, Value::Null, "the interceptor sent no body");
        assert_eq!(
            reply
                .headers
                .get(header::WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer")
        );
    }
}

#[tokio::test]
async fn register_login_and_me_round_trip() {
    let app = boot().await;
    let id = register(&app.router, "alice", "wonderland").await;
    assert_eq!(id.len(), 36);

    let token = login(&app.router, "alice", "wonderland").await;
    let reply = call(&app.router, Method::GET, "/v1/auth/me", None, Some(&token)).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.body,
        json!({ "id": id, "name": "alice", "display_name": "alice" })
    );

    let reply = call(
        &app.router,
        Method::POST,
        "/v1/auth/register",
        Some(&json!({ "name": "alice", "password": "other" })),
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.code(), "AUTH003");
    assert_eq!(reply.message(), "Username already exists: alice");
    assert!(
        reply.body["timestamp"].is_i64(),
        "error bodies carry a timestamp"
    );

    let reply = call(
        &app.router,
        Method::POST,
        "/v1/auth/login",
        Some(&json!({ "type": "local", "name": "alice", "password": "nope" })),
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.code(), "AUTH001");
    assert_eq!(
        reply.message(),
        "Authentication failed: invalid credentials"
    );

    let reply = call(
        &app.router,
        Method::POST,
        "/v1/auth/login",
        Some(&json!({ "type": "ldap", "name": "alice", "password": "wonderland" })),
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.code(), "AUTH004");
}

#[tokio::test]
async fn validation_and_malformed_bodies_answer_with_cm001() {
    let app = boot().await;
    let reply = call(
        &app.router,
        Method::POST,
        "/v1/auth/login",
        Some(&json!({ "type": "local" })),
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.message(),
        "Request parameter error: name: must not be blank; password: must not be blank"
    );

    let token = login_after_register(&app, "alice").await;
    let reply = call(
        &app.router,
        Method::POST,
        "/v1/auth/register",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(
        reply.status,
        StatusCode::BAD_REQUEST,
        "an empty body cannot deserialize"
    );
    assert_eq!(reply.code(), "CM001");
}

#[tokio::test]
async fn api_keys_authenticate_requests_and_disappear_on_delete() {
    let app = boot().await;
    let alice = register(&app.router, "alice", "wonderland").await;
    let token = login(&app.router, "alice", "wonderland").await;

    let reply = call(
        &app.router,
        Method::POST,
        "/v1/auth/api-keys",
        Some(&json!({ "name": "cli", "expires_in_days": 7 })),
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CREATED);
    let id = reply.body["id"].as_str().unwrap().to_string();
    let key = reply.body["key"].as_str().unwrap().to_string();
    let mask = reply.body["key_mask"].as_str().unwrap().to_string();
    assert!(key.starts_with("sk_") && key.len() == 46);
    assert!(mask.starts_with("sk_") && mask.contains("***") && mask.len() < key.len());
    assert!(reply.body["expires_at"].is_string());

    let reply = call(&app.router, Method::GET, "/v1/auth/me", None, Some(&key)).await;
    assert_eq!(reply.status, StatusCode::OK, "the key identifies its owner");
    assert_eq!(
        reply.body,
        json!({ "id": alice, "name": "alice", "display_name": "alice" })
    );

    let reply = call(
        &app.router,
        Method::GET,
        "/v1/auth/api-keys",
        None,
        Some(&token),
    )
    .await;
    let listed = reply.body.as_array().expect("array").clone();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], json!(id));
    assert_eq!(listed[0]["key_mask"], json!(mask));
    assert_eq!(listed[0]["name"], json!("cli"));
    assert!(
        listed[0]["last_used_at"].is_string(),
        "the key login recorded a use"
    );
    assert!(
        listed[0].get("key").is_none(),
        "the list never repeats the plaintext"
    );

    let reply = call(
        &app.router,
        Method::DELETE,
        &format!("/v1/auth/api-keys/{id}"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    assert_eq!(reply.body, Value::Null);

    let reply = call(&app.router, Method::GET, "/v1/auth/me", None, Some(&key)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        reply.body,
        Value::Null,
        "401s from the middleware stay empty"
    );
}

#[tokio::test]
async fn api_keys_belong_to_their_owner_and_expiry_is_a_short_list() {
    let app = boot().await;
    register(&app.router, "alice", "wonderland").await;
    let alice = login(&app.router, "alice", "wonderland").await;
    let bob = login_after_register(&app, "bob").await;

    let reply = call(
        &app.router,
        Method::POST,
        "/v1/auth/api-keys",
        Some(&json!({ "name": "cli" })),
        Some(&alice),
    )
    .await;
    let id = reply.body["id"].as_str().unwrap().to_string();
    assert!(
        reply.body["expires_at"].is_null(),
        "no expiry means the key never dies"
    );

    let reply = call(
        &app.router,
        Method::DELETE,
        &format!("/v1/auth/api-keys/{id}"),
        None,
        Some(&bob),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.code(), "AUTH005");

    let reply = call(
        &app.router,
        Method::POST,
        "/v1/auth/api-keys",
        Some(&json!({ "name": "cli", "expires_in_days": 15 })),
        Some(&alice),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.message(),
        "expiresInDays must be one of 7/30/90/180/365"
    );

    let reply = call(
        &app.router,
        Method::GET,
        "/v1/auth/api-keys",
        None,
        Some(&bob),
    )
    .await;
    assert_eq!(reply.body, json!([]), "keys are scoped to their owner");
}

#[tokio::test]
async fn user_search_fills_the_grant_dialog() {
    let app = boot().await;
    register(&app.router, "alice", "wonderland").await;
    register(&app.router, "bob", "thepool").await;
    let token = login(&app.router, "alice", "wonderland").await;

    let reply = call(
        &app.router,
        Method::GET,
        "/v1/users/search?keyword=LI",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let found = reply.body.as_array().expect("array").clone();
    assert_eq!(
        found.len(),
        1,
        "only alice matches, and case does not matter"
    );
    assert_eq!(found[0]["name"], json!("alice"));
    assert_eq!(found[0]["display_name"], json!("alice"));
    assert_eq!(found[0]["id"].as_str().map(str::len), Some(36));

    let reply = call(
        &app.router,
        Method::GET,
        "/v1/users/search",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(reply.body, json!([]), "no keyword means no rows");

    // LIKE wildcards reach the database unescaped, exactly as the Java query let them through.
    let reply = call(
        &app.router,
        Method::GET,
        "/v1/users/search?keyword=%25",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(reply.body.as_array().map(Vec::len), Some(2));
}

#[tokio::test]
async fn acl_grants_round_trip_through_http() {
    let app = boot().await;
    let alice = register(&app.router, "alice", "wonderland").await;
    let bob = register(&app.router, "bob", "thepool").await;
    let alice_token = login(&app.router, "alice", "wonderland").await;
    let bob_token = login(&app.router, "bob", "thepool").await;
    let source = seed_data_source(&app.db, &alice).await;
    let uri = format!("/v1/acls/DATA_SOURCE/{source}");

    let reply = call(&app.router, Method::GET, &uri, None, Some(&bob_token)).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.code(), "PM001");

    let reply = call(
        &app.router,
        Method::POST,
        &uri,
        Some(&json!({ "principal_type": "USER", "principal_id": bob, "permission": "VIEW" })),
        Some(&alice_token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let grant_id = reply.body["id"].as_str().unwrap().to_string();
    assert_eq!(grant_id.len(), 36);

    let reply = call(&app.router, Method::GET, &uri, None, Some(&alice_token)).await;
    assert_eq!(
        reply.body,
        json!([{
            "id": grant_id,
            "principal_type": "USER",
            "principal_id": bob,
            "principal_name": "bob",
            "permission": "VIEW",
            "created_by": alice,
        }])
    );

    let reply = call(
        &app.router,
        Method::POST,
        &uri,
        Some(&json!({ "principal_type": "GROUP", "principal_id": "ALL_USERS", "permission": "EDIT" })),
        Some(&alice_token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.message(), "Request parameter error: permission");

    let reply = call(
        &app.router,
        Method::DELETE,
        &format!("{uri}/USER/{bob}"),
        None,
        Some(&alice_token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.body,
        json!({ "id": source }),
        "revoke answers with the resource id"
    );

    let reply = call(&app.router, Method::GET, &uri, None, Some(&alice_token)).await;
    assert_eq!(reply.body, json!([]));

    let reply = call(
        &app.router,
        Method::GET,
        "/v1/acls/TABLE/whatever",
        None,
        Some(&alice_token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.code(), "CM001");

    let reply = call(
        &app.router,
        Method::GET,
        "/v1/acls/DATA_SOURCE/nope",
        None,
        Some(&alice_token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.code(), "DS002");
    assert_eq!(reply.message(), "Data source not found: nope");
}

async fn login_after_register(app: &TestApp, name: &str) -> String {
    register(&app.router, name, "wonderland").await;
    login(&app.router, name, "wonderland").await
}

async fn seed_data_source(db: &Db, owner: &str) -> String {
    let sql = db.sql(
        "INSERT INTO data_source (id, created_by, created_at, updated_by, updated_at, name, \
         deleted, connection_url, type) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    );
    let id = new_id();
    sqlx::query(&sql)
        .bind(&id)
        .bind(owner)
        .bind("2026-01-01T00:00:00Z")
        .bind(owner)
        .bind("2026-01-01T00:00:00Z")
        .bind("demo")
        .bind(false)
        .bind("sqlite:demo.db")
        .bind("SQLITE")
        .execute(db.pool())
        .await
        .expect("seed row");
    id
}
