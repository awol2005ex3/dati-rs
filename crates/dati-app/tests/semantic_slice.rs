//! End-to-end checks of the semantic slice: subjects, the tables they hold, and the terms and
//! field bindings that ground them — over a real sqlite metadata store and a fake sqlite
//! warehouse, both created per test and never touched outside this machine.

use axum::Router;
use axum::body::to_bytes;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use dati_app::{api, state::AppState};
use dati_core::config::{Config, MetaKind};
use dati_core::db::Db;

struct TestApp {
    router: Router,
    warehouse: String,
}

async fn boot() -> TestApp {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_string_lossy().replace('\\', "/");
    // The pools keep the files open; dropping the guard would delete them mid-test.
    std::mem::forget(dir);
    let db = Db::connect(&format!("sqlite:{root}/meta.db?mode=rwc"), MetaKind::Sqlite)
        .await
        .expect("metadata store");
    let warehouse = format!("sqlite:{root}/warehouse.db?mode=rwc");
    seed_warehouse(&warehouse).await;
    let config = Config::from_source(|key| (key == "ADMIN_USERS").then(|| "root".to_string()));
    TestApp {
        router: api::router(AppState::new(db, config)),
        warehouse,
    }
}

async fn seed_warehouse(url: &str) {
    sqlx::any::install_default_drivers();
    let pool = sqlx::any::AnyPoolOptions::new()
        .max_connections(2)
        .connect(url)
        .await
        .expect("warehouse");
    for statement in [
        "CREATE TABLE orders (id INTEGER, status TEXT, amount REAL)",
        "INSERT INTO orders (id, status, amount) VALUES (1, 'paid', 9.99)",
    ] {
        sqlx::query(statement)
            .execute(&pool)
            .await
            .expect("seed warehouse");
    }
    pool.close().await;
}

struct Reply {
    status: StatusCode,
    body: Value,
}

impl Reply {
    fn code(&self) -> &str {
        self.body["code"].as_str().unwrap_or_default()
    }

    fn message(&self) -> &str {
        self.body["message"].as_str().unwrap_or_default()
    }

    fn expect(self, status: StatusCode) -> Value {
        assert_eq!(self.status, status, "{:?} {}", self.body, status);
        self.body
    }
}

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
    }
}

async fn register(app: &Router, name: &str) -> String {
    call(
        app,
        Method::POST,
        "/v1/auth/register",
        Some(&json!({ "name": name, "password": "wonderland", "display_name": name })),
        None,
    )
    .await
    .expect(StatusCode::OK);
    call(
        app,
        Method::POST,
        "/v1/auth/login",
        Some(&json!({ "type": "local", "name": name, "password": "wonderland" })),
        None,
    )
    .await
    .expect(StatusCode::OK)["token"]
        .as_str()
        .expect("token")
        .to_string()
}

async fn user_id(app: &Router, token: &str) -> String {
    call(app, Method::GET, "/v1/auth/me", None, Some(token))
        .await
        .expect(StatusCode::OK)["id"]
        .as_str()
        .expect("id")
        .to_string()
}

/// A registered warehouse with its `orders` table, returning `(data source id, table id)`.
async fn warehouse_tables(app: &Router, warehouse: &str, token: &str) -> (String, String) {
    let source = call(
        app,
        Method::POST,
        "/v1/data-sources",
        Some(&json!({
            "name": "warehouse",
            "type": "SQLITE",
            "connection_url": warehouse,
            "username": "none",
            "password": "",
        })),
        Some(token),
    )
    .await
    .expect(StatusCode::OK);
    let source_id = source["id"].as_str().expect("id").to_string();
    call(
        app,
        Method::POST,
        &format!("/v1/data-sources/{source_id}/tables/batch"),
        Some(&json!([{"name": "orders", "schema": "main"}])),
        Some(token),
    )
    .await
    .expect(StatusCode::OK);
    let tables = call(
        app,
        Method::GET,
        &format!("/v1/data-sources/{source_id}/tables?keyword=orders"),
        None,
        Some(token),
    )
    .await
    .expect(StatusCode::OK);
    let table_id = tables["data"][0]["id"]
        .as_str()
        .expect("table id")
        .to_string();
    (source_id, table_id)
}

async fn subject_id(app: &Router, token: &str, source_id: &str, name: &str) -> String {
    call(
        app,
        Method::POST,
        "/v1/subjects",
        Some(&json!({
            "name": name,
            "description": "订单业务域",
            "datasource_id": source_id,
            "aliases": ["交易"],
        })),
        Some(token),
    )
    .await
    .expect(StatusCode::OK)["id"]
        .as_str()
        .expect("id")
        .to_string()
}

async fn term_id(app: &Router, token: &str, subject: &str, name: &str) -> String {
    call(
        app,
        Method::POST,
        &format!("/v1/subjects/{subject}/terms"),
        Some(&json!({ "name": name, "description": "一段时间的成交额", "aliases": ["GMV"] })),
        Some(token),
    )
    .await
    .expect(StatusCode::OK)["id"]
        .as_str()
        .expect("id")
        .to_string()
}

#[tokio::test]
async fn a_subject_pages_binds_and_releases_its_tables() {
    let app = boot().await;
    let token = register(&app.router, "alice").await;
    let (source_id, table_id) = warehouse_tables(&app.router, &app.warehouse, &token).await;

    let reply = call(
        &app.router,
        Method::POST,
        "/v1/subjects",
        Some(&json!({})),
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.code(), "CM001");
    assert_eq!(
        reply.message(),
        "Request parameter error: name: must not be blank; datasourceId: must not be blank"
    );

    let subject = subject_id(&app.router, &token, &source_id, "orders").await;
    let row = call(
        &app.router,
        Method::GET,
        &format!("/v1/subjects/{subject}"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(row["name"], json!("orders"));
    assert_eq!(row["datasource_id"], json!(source_id));
    assert_eq!(row["datasource_name"], json!("warehouse"));
    assert_eq!(row["aliases"], json!(["交易"]));
    assert_eq!(row["created_user_name"], json!("alice"));

    // A page, with the Java ordering and paging contract.
    let listed = call(
        &app.router,
        Method::GET,
        "/v1/subjects?keyword=ORDERS",
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(listed["total"], json!(1));
    assert_eq!(listed["page"], json!(1));
    assert_eq!(listed["size"], json!(10));
    assert_eq!(listed["total_pages"], json!(1));
    assert_eq!(listed["data"][0]["id"], json!(subject));

    let reply = call(
        &app.router,
        Method::GET,
        "/v1/subjects?size=101",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(
        reply.message(),
        "Request parameter error: size: must be less than or equal to 100"
    );

    // The picker needs its schema, and an absent one is a binder error before any lookup.
    let reply = call(
        &app.router,
        Method::GET,
        &format!("/v1/subjects/{subject}/available-tables"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.message(),
        "Request parameter error: Required query parameter 'schema' is not present."
    );

    let available = call(
        &app.router,
        Method::GET,
        &format!("/v1/subjects/{subject}/available-tables?schema=main"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(
        available,
        json!([{"table_id": table_id, "table_name": "orders", "schema": "main", "description": null}])
    );
    assert!(
        call(
            &app.router,
            Method::GET,
            &format!("/v1/subjects/{subject}/available-tables?schema="),
            None,
            Some(&token),
        )
        .await
        .expect(StatusCode::OK)
        .is_array(),
        "an empty schema binds and simply matches nothing"
    );

    call(
        &app.router,
        Method::POST,
        &format!("/v1/subjects/{subject}/tables"),
        Some(&json!({ "table_id": table_id })),
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    let reply = call(
        &app.router,
        Method::POST,
        &format!("/v1/subjects/{subject}/tables"),
        Some(&json!({ "table_id": table_id })),
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.code(), "SM005");
    assert_eq!(
        reply.message(),
        format!("Table orders is already associated with subject {subject}")
    );

    let bound = call(
        &app.router,
        Method::GET,
        &format!("/v1/subjects/{subject}/tables"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(bound["total"], json!(1));
    assert_eq!(bound["data"][0]["name"], json!("orders"));
    assert_eq!(bound["data"][0]["created_user_name"], json!("alice"));

    // An edit that survives a reload, and only merges what the body carried.
    call(
        &app.router,
        Method::PUT,
        &format!("/v1/subjects/{subject}"),
        Some(&json!({ "description": "改过的说明", "aliases": [] })),
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    let row = call(
        &app.router,
        Method::GET,
        &format!("/v1/subjects/{subject}"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(
        row["name"],
        json!("orders"),
        "an absent name keeps its value"
    );
    assert_eq!(row["description"], json!("改过的说明"));
    assert_eq!(row["aliases"], json!([]), "an empty list clears them");

    let reply = call(
        &app.router,
        Method::DELETE,
        &format!("/v1/subjects/{subject}/tables/{table_id}"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(reply, json!({ "id": table_id }));
    let reply = call(
        &app.router,
        Method::DELETE,
        &format!("/v1/subjects/{subject}/tables/{table_id}"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.code(), "SM006");
    assert_eq!(
        reply.message(),
        format!("Association between subject {subject} and table {table_id} not found")
    );

    // Deleting the subject leaves the bindings alone, exactly as the Java service did: the row is
    // simply gone, so the page that named it is a 404.
    call(
        &app.router,
        Method::DELETE,
        &format!("/v1/subjects/{subject}"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    let reply = call(
        &app.router,
        Method::GET,
        &format!("/v1/subjects/{subject}"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.code(), "SM001");
    assert_eq!(reply.message(), format!("Subject not found: {subject}"));
}

#[tokio::test]
async fn a_term_binds_fields_and_releases_them_by_sentinel() {
    let app = boot().await;
    let token = register(&app.router, "alice").await;
    let (source_id, table_id) = warehouse_tables(&app.router, &app.warehouse, &token).await;
    let subject = subject_id(&app.router, &token, &source_id, "orders").await;
    call(
        &app.router,
        Method::POST,
        &format!("/v1/subjects/{subject}/tables"),
        Some(&json!({ "table_id": table_id })),
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    let term = term_id(&app.router, &token, &subject, "成交总额").await;

    let reply = call(
        &app.router,
        Method::POST,
        &format!("/v1/subjects/{subject}/terms"),
        Some(&json!({ "description": "无名字" })),
        Some(&token),
    )
    .await;
    assert_eq!(
        reply.message(),
        "Request parameter error: name: must not be blank"
    );

    let row = call(
        &app.router,
        Method::GET,
        &format!("/v1/terms/{term}"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(row["name"], json!("成交总额"));
    assert_eq!(row["subject_id"], json!(subject));
    assert_eq!(row["aliases"], json!(["GMV"]));
    assert_eq!(row["relations"], json!([]));
    assert_eq!(row["created_user_name"], json!("alice"));

    // A FIELD binding must name the field; the sentinel `_` means the whole table instead.
    let reply = call(
        &app.router,
        Method::POST,
        &format!("/v1/terms/{term}/relations"),
        Some(&json!({ "entity_type": "FIELD", "table_id": table_id })),
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.code(), "CM001");
    assert_eq!(
        reply.message(),
        "Request parameter error: fieldName is required"
    );

    let reply = call(
        &app.router,
        Method::POST,
        &format!("/v1/terms/{term}/relations"),
        Some(&json!({ "table_id": table_id })),
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.code(), "CM001");
    assert_eq!(
        reply.message(),
        "Request parameter error: entityType: must not be null"
    );

    // The sentinel is a value of `field_name`, never a way out of naming the table.
    let reply = call(
        &app.router,
        Method::POST,
        &format!("/v1/terms/{term}/relations"),
        Some(&json!({ "entity_type": "TABLE", "field_name": "_" })),
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.code(), "CM001");
    assert_eq!(
        reply.message(),
        "Request parameter error: tableId: must not be blank"
    );

    let body = json!({ "entity_type": "FIELD", "table_id": table_id, "field_name": "status" });
    call(
        &app.router,
        Method::POST,
        &format!("/v1/terms/{term}/relations"),
        Some(&body),
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    let reply = call(
        &app.router,
        Method::POST,
        &format!("/v1/terms/{term}/relations"),
        Some(&body),
        Some(&token),
    )
    .await;
    assert_eq!(
        reply.message(),
        "Request parameter error: Term relation already exists"
    );

    call(
        &app.router,
        Method::POST,
        &format!("/v1/terms/{term}/relations"),
        Some(&json!({ "entity_type": "TABLE", "table_id": table_id, "field_name": "_" })),
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);

    let row = call(
        &app.router,
        Method::GET,
        &format!("/v1/terms/{term}"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    let relations = row["relations"].as_array().expect("relations");
    assert_eq!(relations.len(), 2);
    assert!(
        relations
            .iter()
            .all(|item| item["table_name"] == json!("orders") && item["schema"] == json!("main")),
        "the bound table is resolved for every relation: {relations:?}"
    );
    assert!(
        relations
            .iter()
            .any(|item| item["entity_type"] == json!("TABLE"))
    );
    let field = relations
        .iter()
        .find(|item| item["entity_type"] == json!("FIELD"))
        .expect("field relation");
    assert_eq!(field["field_name"], json!("status"));

    // A page of terms carries the same bindings, filled in one batch.
    let listed = call(
        &app.router,
        Method::GET,
        &format!("/v1/subjects/{subject}/terms?keyword=%E6%88%90%E4%BA%A4"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(listed["total"], json!(1));
    assert_eq!(listed["data"][0]["id"], json!(term));
    assert_eq!(
        listed["data"][0]["relations"]
            .as_array()
            .expect("relations")
            .len(),
        2
    );

    // An edit merges, then the sentinel release takes both bindings of the table at once.
    call(
        &app.router,
        Method::PUT,
        &format!("/v1/terms/{term}"),
        Some(&json!({ "name": "GMV", "aliases": [] })),
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    let row = call(
        &app.router,
        Method::GET,
        &format!("/v1/terms/{term}"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(row["name"], json!("GMV"));
    assert_eq!(
        row["description"],
        json!("一段时间的成交额"),
        "absent fields kept"
    );
    assert_eq!(row["aliases"], json!([]));

    let reply = call(
        &app.router,
        Method::DELETE,
        &format!("/v1/terms/{term}/relations/{table_id}/missing"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.code(), "CM002");
    assert_eq!(
        reply.message(),
        "Resource not found",
        "the template has no placeholder"
    );

    call(
        &app.router,
        Method::DELETE,
        &format!("/v1/terms/{term}/relations/{table_id}/_"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    let row = call(
        &app.router,
        Method::GET,
        &format!("/v1/terms/{term}"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(row["relations"], json!([]));
    // Releasing nothing with the sentinel is not an error.
    call(
        &app.router,
        Method::DELETE,
        &format!("/v1/terms/{term}/relations/{table_id}/_"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);

    call(
        &app.router,
        Method::DELETE,
        &format!("/v1/terms/{term}"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    let reply = call(
        &app.router,
        Method::GET,
        &format!("/v1/terms/{term}"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.code(), "SM002");
    assert_eq!(reply.message(), format!("Term not found: {term}"));
}

#[tokio::test]
async fn a_table_outside_the_subject_cannot_be_bound() {
    let app = boot().await;
    let token = register(&app.router, "alice").await;
    let (source_id, table_id) = warehouse_tables(&app.router, &app.warehouse, &token).await;
    let other = subject_id(&app.router, &token, &source_id, "other").await;
    let term = term_id(&app.router, &token, &other, "GMV").await;

    let reply = call(
        &app.router,
        Method::POST,
        &format!("/v1/terms/{term}/relations"),
        Some(&json!({ "entity_type": "TABLE", "table_id": "missing" })),
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.code(), "SM003");
    assert_eq!(
        reply.message(),
        format!("Table missing does not belong to subject {other}"),
        "the raw table id is reported here, unlike the subject binding"
    );

    let reply = call(
        &app.router,
        Method::POST,
        &format!("/v1/subjects/{other}/tables"),
        Some(&json!({ "table_id": "missing" })),
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.code(), "DS002");
    assert_eq!(reply.message(), "Data source not found: missing");

    let second = call(
        &app.router,
        Method::POST,
        "/v1/data-sources",
        Some(&json!({
            "name": "crm",
            "type": "SQLITE",
            "connection_url": app.warehouse,
            "username": "none",
        })),
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    let second_id = second["id"].as_str().expect("id").to_string();

    // A subject of the second source cannot hold a table of the first, however similar the
    // warehouses look: the check is on ownership, not on the name.
    let foreign = subject_id(&app.router, &token, &second_id, "crm-domain").await;
    let reply = call(
        &app.router,
        Method::POST,
        &format!("/v1/subjects/{foreign}/tables"),
        Some(&json!({ "table_id": table_id })),
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.code(), "SM003");
    assert_eq!(
        reply.message(),
        format!("Table orders does not belong to subject {foreign}"),
        "a foreign table is named, not its id"
    );
}

#[tokio::test]
async fn rights_over_a_term_come_from_its_subject() {
    let app = boot().await;
    let alice = register(&app.router, "alice").await;
    let bob = register(&app.router, "bob").await;
    let (source_id, table_id) = warehouse_tables(&app.router, &app.warehouse, &alice).await;
    let subject = subject_id(&app.router, &alice, &source_id, "orders").await;
    let term = term_id(&app.router, &alice, &subject, "成交总额").await;

    for (method, uri, body) in [
        (Method::GET, format!("/v1/subjects/{subject}"), None),
        (Method::GET, format!("/v1/subjects/{subject}/terms"), None),
        (Method::GET, format!("/v1/terms/{term}"), None),
        (
            Method::POST,
            format!("/v1/terms/{term}/relations"),
            Some(json!({ "entity_type": "TABLE", "table_id": table_id })),
        ),
        (Method::DELETE, format!("/v1/terms/{term}"), None),
    ] {
        let reply = call(&app.router, method.clone(), &uri, body.as_ref(), Some(&bob)).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method} {uri}");
        assert_eq!(reply.code(), "PM001", "{method} {uri}");
        assert_eq!(reply.message(), "No permission to access this resource");
    }

    // A stranger cannot even found a subject on a source they may not see.
    let reply = call(
        &app.router,
        Method::POST,
        "/v1/subjects",
        Some(&json!({ "name": "sneak", "datasource_id": source_id })),
        Some(&bob),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.code(), "PM001");

    let listed = call(&app.router, Method::GET, "/v1/subjects", None, Some(&bob))
        .await
        .expect(StatusCode::OK);
    assert_eq!(
        listed["total"],
        json!(0),
        "the list is scoped to what the caller may see"
    );

    // VIEW on the subject is enough to read its glossary, never to write into it.
    let grant = json!({
        "principal_type": "USER",
        "principal_id": user_id(&app.router, &bob).await,
        "permission": "VIEW",
    });
    call(
        &app.router,
        Method::POST,
        &format!("/v1/acls/SUBJECT/{subject}"),
        Some(&grant),
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    let row = call(
        &app.router,
        Method::GET,
        &format!("/v1/terms/{term}"),
        None,
        Some(&bob),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(row["name"], json!("成交总额"));
    let reply = call(
        &app.router,
        Method::PUT,
        &format!("/v1/terms/{term}"),
        Some(&json!({ "name": "hijacked" })),
        Some(&bob),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.code(), "PM001");

    // Existence still comes before the denial: a bad subject id is a 404 for anybody.
    let reply = call(
        &app.router,
        Method::GET,
        "/v1/subjects/nope",
        None,
        Some(&bob),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.code(), "SM001");
    let reply = call(&app.router, Method::GET, "/v1/terms/nope", None, Some(&bob)).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.code(), "SM002");
}
