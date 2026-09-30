//! End-to-end checks of the datasource slice: the REST surface over a real sqlite metadata store
//! and a fake sqlite warehouse, both created per test.
//!
//! Nothing here touches a server outside this machine: the warehouse is a throwaway file, which is
//! also what keeps the connection-failure cases honest (a path into a missing directory).

use axum::Router;
use axum::body::to_bytes;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use dati_app::{api, state::AppState};
use dati_core::config::{Config, MetaKind};
use dati_core::db::Db;
use dati_core::model::datasource::DbType;

struct TestApp {
    router: Router,
    /// `sqlite:` URL of the fake warehouse, as the frontend would send it.
    warehouse: String,
}

/// One directory per boot: the metadata store and the warehouse live side by side.
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

/// The warehouse is loaded with a bare pool: `Db::connect` would add the metadata tables to it and
/// every live listing would report them.
async fn seed_warehouse(url: &str) {
    sqlx::any::install_default_drivers();
    let pool = sqlx::any::AnyPoolOptions::new()
        .max_connections(2)
        .connect(url)
        .await
        .expect("warehouse");
    for statement in [
        "CREATE TABLE orders (id INTEGER, status TEXT, amount REAL)",
        "INSERT INTO orders (id, status, amount) VALUES (1, 'paid', 9.99), (2, 'paid', 12.5), \
         (3, 'refunded', 3.0), (4, 'shipped', 7.25)",
        "CREATE TABLE customers (id INTEGER, name TEXT)",
    ] {
        sqlx::query(statement)
            .execute(&pool)
            .await
            .expect("seed warehouse");
    }
    pool.close().await;
}

async fn alter_warehouse(url: &str, statement: &str) {
    sqlx::any::install_default_drivers();
    let pool = sqlx::any::AnyPoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await
        .expect("warehouse");
    sqlx::query(statement)
        .execute(&pool)
        .await
        .expect("alter warehouse");
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

async fn login_after_register(app: &Router, name: &str) -> String {
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

/// A data source pointed at the warehouse, created through the API so the detected schema is real.
async fn add_data_source(app: &Router, warehouse: &str, token: &str) -> String {
    let body = json!({
        "name": "warehouse",
        "description": "the fake one",
        "type": "SQLITE",
        "connection_url": warehouse,
        "username": "none",
        "password": "",
        "default_schema": "whatever_the_client_sent",
    });
    call(
        app,
        Method::POST,
        "/v1/data-sources",
        Some(&body),
        Some(token),
    )
    .await
    .expect(StatusCode::OK)["id"]
        .as_str()
        .expect("id")
        .to_string()
}

/// Register `orders` and return its table id.
async fn add_orders(app: &Router, id: &str, token: &str) -> String {
    let body = json!([{"name": "orders", "schema": "main"}]);
    call(
        app,
        Method::POST,
        &format!("/v1/data-sources/{id}/tables/batch"),
        Some(&body),
        Some(token),
    )
    .await
    .expect(StatusCode::OK);
    let tables = call(
        app,
        Method::GET,
        &format!("/v1/data-sources/{id}/tables?keyword=orders"),
        None,
        Some(token),
    )
    .await
    .expect(StatusCode::OK);
    tables["data"][0]["id"]
        .as_str()
        .expect("table id")
        .to_string()
}

async fn column_id(app: &Router, id: &str, table_id: &str, name: &str, token: &str) -> String {
    let uri = format!("/v1/data-sources/{id}/tables/{table_id}/columns?keyword={name}&size=100");
    let body = call(app, Method::GET, &uri, None, Some(token))
        .await
        .expect(StatusCode::OK);
    body["data"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["name"] == json!(name))
        .unwrap_or_else(|| panic!("no column {name} in {body}"))["id"]
        .as_str()
        .expect("column id")
        .to_string()
}

/// One page of the value dictionary of `base`, which is a column URL.
async fn values(app: &Router, base: &str, token: &str) -> Value {
    call(
        app,
        Method::GET,
        &format!("{base}/values?size=100"),
        None,
        Some(token),
    )
    .await
    .expect(StatusCode::OK)
}

/// Rows of a page, keyed by `name`: ids are random, so the database's own order is not meaningful.
fn rows_by_name(body: &Value) -> Vec<(String, Value)> {
    body["data"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| (row["name"].as_str().expect("name").to_string(), row.clone()))
        .collect()
}

fn row(rows: &[(String, Value)], name: &str) -> Value {
    rows.iter()
        .find(|(row_name, _)| row_name == name)
        .unwrap_or_else(|| panic!("no row {name} in {rows:?}"))
        .1
        .clone()
}

#[tokio::test]
async fn system_config_offers_the_databases_the_server_can_connect_to() {
    let app = boot().await;
    let token = login_after_register(&app.router, "alice").await;
    let body = call(
        &app.router,
        Method::GET,
        "/v1/system/config",
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(body["column_value_sample_limit"], json!(1000));
    assert_eq!(body["column_value_length_limit"], json!(256));
    assert_eq!(
        body["supported_database_types"],
        json!([
            {
                "type": "MYSQL",
                "label": "MySQL",
                "default_port": 3306,
                "connection_url_template": "mysql://localhost:3306/database_name"
            },
            {
                "type": "POSTGRESQL",
                "label": "PostgreSQL",
                "default_port": 5432,
                "connection_url_template": "postgresql://localhost:5432/database_name"
            },
            {
                "type": "MARIADB",
                "label": "MariaDB",
                "default_port": 3306,
                "connection_url_template": "mariadb://localhost:3306/database_name"
            },
            {
                "type": "DORIS",
                "label": "Apache Doris",
                "default_port": 9030,
                "connection_url_template": "mysql://localhost:9030/database_name"
            },
            {
                "type": "SQLITE",
                "label": "SQLite",
                "default_port": 0,
                "connection_url_template": DbType::Sqlite.connection_url_template()
            }
        ])
    );
}

#[tokio::test]
async fn probing_and_registering_a_data_source_detects_the_schema() {
    let app = boot().await;
    let token = login_after_register(&app.router, "alice").await;
    let missing = "sqlite:D:/definitely/not/here/dati-test.db";

    let probe = json!({ "type": "SQLITE", "connection_url": app.warehouse, "username": "none" });
    assert_eq!(
        call(
            &app.router,
            Method::POST,
            "/v1/data-sources/test-connection",
            Some(&probe),
            Some(&token)
        )
        .await
        .expect(StatusCode::OK),
        json!(true)
    );
    let broken = call(
        &app.router,
        Method::POST,
        "/v1/data-sources/test-connection",
        Some(&json!({ "type": "SQLITE", "connection_url": missing, "username": "none" })),
        Some(&token),
    )
    .await;
    assert_eq!(broken.status, StatusCode::BAD_REQUEST);
    assert_eq!(broken.body["code"], json!("DS001"));
    assert!(
        broken.message().contains("unable to open database file"),
        "the probe reports what the driver said: {:?}",
        broken.message()
    );

    // The type is what lets the driver choose the database, so the URL alone still probes.
    let inferred = json!({ "connection_url": app.warehouse, "username": "none" });
    assert_eq!(
        call(
            &app.router,
            Method::POST,
            "/v1/data-sources/test-connection",
            Some(&inferred),
            Some(&token)
        )
        .await
        .expect(StatusCode::OK),
        json!(true)
    );

    let id = add_data_source(&app.router, &app.warehouse, &token).await;
    let listed = call(
        &app.router,
        Method::GET,
        "/v1/data-sources",
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(listed["total"], json!(1));
    assert_eq!(listed["page"], json!(1));
    assert_eq!(listed["size"], json!(10));
    assert_eq!(listed["total_pages"], json!(1));
    let row = &listed["data"][0];
    assert_eq!(row["id"], json!(id));
    assert_eq!(row["type"], json!("SQLITE"));
    assert_eq!(
        row["default_schema"],
        json!("main"),
        "the client value was ignored"
    );
    assert_eq!(row["created_user_name"], json!("alice"));
    assert_eq!(row["updated_user_name"], json!("alice"));
    assert!(
        row.get("password").is_none(),
        "the view never carries the password: {row}"
    );

    // A probe that fails on save leaves nothing behind.
    let body = json!({ "type": "SQLITE", "connection_url": missing, "username": "none" });
    let reply = call(
        &app.router,
        Method::POST,
        "/v1/data-sources",
        Some(&body),
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.code(), "DS001");
    assert!(reply.message().starts_with("Database connection failed: "));
    let listed = call(
        &app.router,
        Method::GET,
        "/v1/data-sources",
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(listed["total"], json!(1), "only the good row survived");

    // Bean Validation on the posted entity.
    let reply = call(
        &app.router,
        Method::POST,
        "/v1/data-sources",
        Some(&json!({ "name": "x" })),
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.message(),
        "Request parameter error: type: must not be null; connectionUrl: must not be blank; username: must not be blank"
    );
}

#[tokio::test]
async fn paging_keywords_and_unknown_ids_behave_like_the_java_endpoints() {
    let app = boot().await;
    let token = login_after_register(&app.router, "alice").await;
    let id = add_data_source(&app.router, &app.warehouse, &token).await;

    for (query, expected) in [
        ("page=0", "page: must be greater than or equal to 1"),
        ("size=0", "size: must be greater than or equal to 1"),
        ("size=101", "size: must be less than or equal to 100"),
        ("page=abc", "page: must be a number"),
        (
            "page=0&size=500",
            "page: must be greater than or equal to 1; size: must be less than or equal to 100",
        ),
    ] {
        let reply = call(
            &app.router,
            Method::GET,
            &format!("/v1/data-sources?{query}"),
            None,
            Some(&token),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(reply.code(), "CM001", "{query}");
        assert_eq!(
            reply.message(),
            format!("Request parameter error: {expected}"),
            "{query}"
        );
    }

    // Defaults apply, and an empty value reads like a missing one.
    let listed = call(
        &app.router,
        Method::GET,
        "/v1/data-sources?page=&size=",
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(listed["page"], json!(1));
    assert_eq!(listed["size"], json!(10));

    // The Java query matched name OR id, case-insensitively.
    for keyword in ["WAREHOUSE", "%"] {
        let query = format!("/v1/data-sources?keyword={}", urlencoded(keyword));
        let listed = call(&app.router, Method::GET, &query, None, Some(&token))
            .await
            .expect(StatusCode::OK);
        assert_eq!(listed["total"], json!(1), "keyword {keyword}");
    }
    let listed = call(
        &app.router,
        Method::GET,
        "/v1/data-sources?keyword=nothing",
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(listed["total"], json!(0));
    assert_eq!(listed["data"], json!([]));

    let reply = call(
        &app.router,
        Method::GET,
        "/v1/data-sources/nope",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.code(), "DS002");
    assert_eq!(reply.message(), "Data source not found: nope");

    let reply = call(
        &app.router,
        Method::GET,
        "/v1/data-sources/nope/schemas",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(reply.code(), "DS002");

    let reply = call(
        &app.router,
        Method::PUT,
        "/v1/data-sources/nope",
        Some(&json!({"name": "x"})),
        Some(&token),
    )
    .await;
    assert_eq!(reply.code(), "DS002");

    // An id-shaped URL that is not a known method stays a method problem, not a 404.
    let reply = call(
        &app.router,
        Method::PATCH,
        &format!("/v1/data-sources/{id}"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
}

fn urlencoded(value: &str) -> String {
    value.bytes().map(|b| format!("%{b:02X}")).collect()
}

#[tokio::test]
async fn live_metadata_flows_into_the_registered_tables() {
    let app = boot().await;
    let token = login_after_register(&app.router, "alice").await;
    let id = add_data_source(&app.router, &app.warehouse, &token).await;

    let schemas = call(
        &app.router,
        Method::GET,
        &format!("/v1/data-sources/{id}/schemas"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(schemas, json!(["main"]));

    let live = call(
        &app.router,
        Method::GET,
        &format!("/v1/data-sources/{id}/schemas/main/tables"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(
        live,
        json!([
            { "name": "customers", "comment": null },
            { "name": "orders", "comment": null },
        ]),
        "internal sqlite objects stay hidden"
    );

    // A body element without a schema fails validation before anything is looked up.
    let reply = call(
        &app.router,
        Method::POST,
        &format!("/v1/data-sources/{id}/tables/batch"),
        Some(&json!([{"name": "orders", "schema": "main"}, {"name": "customers"}])),
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.message(),
        "Request parameter error: tables[1].schema: must not be blank"
    );

    let reply = call(
        &app.router,
        Method::POST,
        &format!("/v1/data-sources/{id}/tables/batch"),
        Some(&json!([{"name": "ghost", "schema": "main"}])),
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.code(), "DS007");
    assert_eq!(reply.message(), "Table not found: ghost");

    let body =
        json!([{"name": "orders", "schema": "main"}, {"name": "customers", "schema": "main"}]);
    let added = call(
        &app.router,
        Method::POST,
        &format!("/v1/data-sources/{id}/tables/batch"),
        Some(&body),
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(
        added,
        json!({ "id": "2" }),
        "the count is reported in the id field"
    );

    let names = call(
        &app.router,
        Method::GET,
        &format!("/v1/data-sources/{id}/tables/added-names"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    // The rows come back in whatever order the database picked, so compare them as a set.
    let mut added_names: Vec<String> = names
        .as_array()
        .expect("array")
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    added_names.sort();
    assert_eq!(
        added_names,
        vec!["customers".to_string(), "orders".to_string()]
    );

    let tables = call(
        &app.router,
        Method::GET,
        &format!("/v1/data-sources/{id}/tables"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(tables["total"], json!(2));
    let tables = rows_by_name(&tables);
    let orders = row(&tables, "orders");
    assert_eq!(orders["schema"], json!("main"));
    assert_eq!(orders["datasource_id"], json!(id));
    assert_eq!(orders["aliases"], json!([]));
    assert_eq!(orders["created_user_name"], json!("alice"));
    let table_id = orders["id"].as_str().expect("table id").to_string();

    // The columns came along with the table.
    let columns = call(
        &app.router,
        Method::GET,
        &format!("/v1/data-sources/{id}/tables/{table_id}/columns?size=100"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    let columns = rows_by_name(&columns);
    let mut summary: Vec<(&str, &str)> = columns
        .iter()
        .map(|(name, row)| (name.as_str(), row["column_type"].as_str().expect("type")))
        .collect();
    summary.sort();
    assert_eq!(
        summary,
        vec![("amount", "REAL"), ("id", "INTEGER"), ("status", "TEXT")]
    );
    assert!(
        columns
            .iter()
            .all(|(_, row)| row["extract_value_enabled"] == json!(false))
    );

    // One statement read both tables, and each kept its own columns.
    let customers_id = row(&tables, "customers")["id"]
        .as_str()
        .expect("table id")
        .to_string();
    let columns = call(
        &app.router,
        Method::GET,
        &format!("/v1/data-sources/{id}/tables/{customers_id}/columns?size=100"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    let columns = rows_by_name(&columns);
    let mut summary: Vec<(&str, &str)> = columns
        .iter()
        .map(|(name, row)| (name.as_str(), row["column_type"].as_str().expect("type")))
        .collect();
    summary.sort();
    assert_eq!(summary, vec![("id", "INTEGER"), ("name", "TEXT")]);

    // Editing is unconditional for description and aliases: omitting them clears them again.
    let body = json!({ "description": "销售订单", "aliases": ["订单", "orders"] });
    let reply = call(
        &app.router,
        Method::PUT,
        &format!("/v1/data-sources/{id}/tables/{table_id}"),
        Some(&body),
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(reply, json!({ "id": table_id }));
    let tables = call(
        &app.router,
        Method::GET,
        &format!("/v1/data-sources/{id}/tables"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    let orders = row(&rows_by_name(&tables), "orders");
    assert_eq!(orders["description"], json!("销售订单"));
    assert_eq!(orders["aliases"], json!(["订单", "orders"]));
    call(
        &app.router,
        Method::PUT,
        &format!("/v1/data-sources/{id}/tables/{table_id}"),
        Some(&json!({})),
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    let tables = call(
        &app.router,
        Method::GET,
        &format!("/v1/data-sources/{id}/tables"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    let orders = row(&rows_by_name(&tables), "orders");
    assert!(
        orders["description"].is_null(),
        "an omitted description is cleared"
    );
    assert_eq!(orders["aliases"], json!([]));

    // Deleting the table takes its columns with it.
    call(
        &app.router,
        Method::DELETE,
        &format!("/v1/data-sources/{id}/tables/{table_id}"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    let reply = call(
        &app.router,
        Method::GET,
        &format!("/v1/data-sources/{id}/tables/{table_id}/columns"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.code(), "DS007");
    assert_eq!(reply.message(), format!("Table not found: {table_id}"));
}

#[tokio::test]
async fn column_sync_adopts_the_live_list_and_keeps_edited_state() {
    let app = boot().await;
    let token = login_after_register(&app.router, "alice").await;
    let id = add_data_source(&app.router, &app.warehouse, &token).await;
    let table_id = add_orders(&app.router, &id, &token).await;
    let uri = |path: &str| format!("/v1/data-sources/{id}/tables/{table_id}/columns{path}");

    let status_id = column_id(&app.router, &id, &table_id, "status", &token).await;

    // An edit that a sync must survive.
    call(
        &app.router,
        Method::PUT,
        &uri(&format!("/{status_id}")),
        Some(&json!({ "description": "订单状态", "aliases": ["状态"] })),
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    alter_warehouse(&app.warehouse, "ALTER TABLE orders ADD COLUMN note TEXT").await;

    let reply = call(
        &app.router,
        Method::POST,
        &uri("/sync?overwrite_existing=true"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(reply, json!({ "id": table_id }));

    let after = call(
        &app.router,
        Method::GET,
        &uri("?size=100"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(after["total"], json!(4), "the new live column was adopted");
    let kept = &after["data"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["name"] == json!("status"))
        .expect("status column");
    assert_eq!(
        kept["id"],
        json!(status_id),
        "a surviving column keeps its id"
    );
    assert_eq!(kept["description"], json!("订单状态"));
    assert_eq!(kept["aliases"], json!(["状态"]));

    // And the column that vanishes from the database goes away on the next sync.
    alter_warehouse(&app.warehouse, "ALTER TABLE orders DROP COLUMN note").await;
    call(&app.router, Method::POST, &uri("/sync"), None, Some(&token))
        .await
        .expect(StatusCode::OK);
    let after = call(
        &app.router,
        Method::GET,
        &uri("?size=100"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(after["total"], json!(3));
    let mut names: Vec<String> = rows_by_name(&after)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec!["amount".to_string(), "id".to_string(), "status".to_string()],
        "the dropped live column was deleted"
    );

    // The service reads through the source named in the path, not the one owning the table, so a
    // second source on the same database syncs the first one's table instead of being rejected.
    let other = add_data_source(&app.router, &app.warehouse, &token).await;
    let reply = call(
        &app.router,
        Method::POST,
        &format!("/v1/data-sources/{other}/tables/{table_id}/columns/sync"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(reply.expect(StatusCode::OK), json!({ "id": table_id }));
}

#[tokio::test]
async fn value_dictionary_samples_edits_and_drops_with_the_flag() {
    let app = boot().await;
    let token = login_after_register(&app.router, "alice").await;
    let id = add_data_source(&app.router, &app.warehouse, &token).await;
    let table_id = add_orders(&app.router, &id, &token).await;
    let status_id = column_id(&app.router, &id, &table_id, "status", &token).await;
    let base = format!("/v1/data-sources/{id}/tables/{table_id}/columns/{status_id}");

    // Extraction needs no flag on the Java side, so sample straight away.
    call(
        &app.router,
        Method::POST,
        &format!("{base}/values/extract?overwrite=false"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    let page = values(&app.router, &base, &token).await;
    let sampled: Vec<&str> = page["data"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row["value"].as_str().expect("value"))
        .collect();
    assert_eq!(
        sampled,
        vec!["paid", "refunded", "shipped"],
        "distinct, sorted, blank-free"
    );
    assert_eq!(page["data"][0]["synonyms"], json!([]));

    // A second extract without overwrite adds nothing.
    call(
        &app.router,
        Method::POST,
        &format!("{base}/values/extract"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(values(&app.router, &base, &token).await["total"], json!(3));

    // Turning the flag on keeps the dictionary; the editor then merges synonyms and drops rows.
    call(
        &app.router,
        Method::PUT,
        &base,
        Some(&json!({ "extract_value_enabled": true })),
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    let page = values(&app.router, &base, &token).await;
    let paid_id = page["data"][0]["id"]
        .as_str()
        .expect("value id")
        .to_string();
    let shipped_id = page["data"][2]["id"]
        .as_str()
        .expect("value id")
        .to_string();
    let body = json!({
        "values": [
            { "id": paid_id, "value": "paid", "synonyms": ["已支付", "paid"] },
            { "value": "pending", "synonyms": ["待支付"] },
        ],
        "deleted_ids": [shipped_id],
    });
    call(
        &app.router,
        Method::PUT,
        &format!("{base}/values"),
        Some(&body),
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    let page = values(&app.router, &base, &token).await;
    assert_eq!(page["total"], json!(3));
    let rows = page["data"].as_array().expect("rows");
    let paid = rows
        .iter()
        .find(|row| row["id"] == json!(paid_id))
        .expect("paid kept");
    assert_eq!(
        paid["synonyms"],
        json!(["已支付"]),
        "the value itself is not its own synonym"
    );
    assert!(
        rows.iter().all(|row| row["value"] != json!("shipped")),
        "deleted_ids are gone"
    );
    let pending = rows
        .iter()
        .find(|row| row["value"] == json!("pending"))
        .expect("row added");
    assert_eq!(
        pending["id"].as_str().map(str::len),
        Some(36),
        "an idless row is inserted"
    );

    // A re-extract with overwrite rebuilds the dictionary, which drops the hand-edited synonyms.
    call(
        &app.router,
        Method::POST,
        &format!("{base}/values/extract?overwrite=true"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    let page = values(&app.router, &base, &token).await;
    assert_eq!(
        page["total"],
        json!(3),
        "paid, refunded, shipped, no duplicate pending"
    );
    assert_eq!(page["data"][0]["synonyms"], json!([]));

    // Switching extraction off wipes the dictionary, as the Java service did.
    call(
        &app.router,
        Method::PUT,
        &base,
        Some(&json!({ "extract_value_enabled": false })),
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(values(&app.router, &base, &token).await["data"], json!([]));

    // The three different answers for a column that is not there, one per Java service.
    let reply = call(
        &app.router,
        Method::POST,
        "/v1/data-sources/x/tables/y/columns/nope/values/extract",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.code(), "CM002");
    assert_eq!(
        reply.message(),
        "Resource not found",
        "the detail had nowhere to go in the template"
    );

    let reply = call(
        &app.router,
        Method::GET,
        "/v1/data-sources/x/tables/y/columns/nope/values",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(reply.code(), "CM003");
    assert_eq!(reply.message(), "Column not found: nope");

    let reply = call(
        &app.router,
        Method::PUT,
        "/v1/data-sources/x/tables/y/columns/nope",
        Some(&json!({})),
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.code(), "DS002");
    assert_eq!(
        reply.message(),
        "Data source not found: Column not found: nope",
        "updateColumn threw the data-source code with a composed detail"
    );

    let reply = call(
        &app.router,
        Method::GET,
        "/v1/data-sources/x/tables/nope/columns",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.code(), "DS007");
}

#[tokio::test]
async fn rights_are_checked_through_the_owning_data_source() {
    let app = boot().await;
    let alice = login_after_register(&app.router, "alice").await;
    let bob = login_after_register(&app.router, "bob").await;
    let id = add_data_source(&app.router, &app.warehouse, &alice).await;
    let table_id = add_orders(&app.router, &id, &alice).await;

    let reply = call(
        &app.router,
        Method::GET,
        &format!("/v1/data-sources/{id}"),
        None,
        Some(&bob),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.code(), "PM001");
    assert_eq!(reply.message(), "No permission to access this resource");

    // Everything below the source inherits its rights, and existence is reported first.
    for (method, uri) in [
        (Method::GET, format!("/v1/data-sources/{id}/tables")),
        (Method::GET, format!("/v1/data-sources/{id}/schemas")),
        (
            Method::GET,
            format!("/v1/data-sources/{id}/tables/{table_id}/columns"),
        ),
        (
            Method::DELETE,
            format!("/v1/data-sources/{id}/tables/{table_id}"),
        ),
    ] {
        let reply = call(&app.router, method.clone(), &uri, None, Some(&bob)).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method} {uri}");
        assert_eq!(reply.code(), "PM001", "{method} {uri}");
    }
    let reply = call(
        &app.router,
        Method::GET,
        "/v1/data-sources/nope/tables",
        None,
        Some(&bob),
    )
    .await;
    assert_eq!(
        reply.status,
        StatusCode::NOT_FOUND,
        "a missing source is not a permission problem"
    );
    assert_eq!(reply.code(), "DS002");

    // The list is scoped to what the caller may see.
    let listed = call(
        &app.router,
        Method::GET,
        "/v1/data-sources",
        None,
        Some(&bob),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(listed["total"], json!(0));

    let grant = json!({ "principal_type": "USER", "principal_id": user_id(&app.router, &bob).await, "permission": "VIEW" });
    call(
        &app.router,
        Method::POST,
        &format!("/v1/acls/DATA_SOURCE/{id}"),
        Some(&grant),
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    let listed = call(
        &app.router,
        Method::GET,
        &format!("/v1/data-sources/{id}"),
        None,
        Some(&bob),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(listed["name"], json!("warehouse"));
    let reply = call(
        &app.router,
        Method::PUT,
        &format!("/v1/data-sources/{id}/tables/{table_id}"),
        Some(&json!({"description": "x"})),
        Some(&bob),
    )
    .await;
    assert_eq!(
        reply.status,
        StatusCode::FORBIDDEN,
        "VIEW does not buy EDIT"
    );
}

async fn user_id(app: &Router, token: &str) -> String {
    call(app, Method::GET, "/v1/auth/me", None, Some(token))
        .await
        .expect(StatusCode::OK)["id"]
        .as_str()
        .expect("id")
        .to_string()
}

#[tokio::test]
async fn updates_only_redetect_when_the_connection_changes_and_delete_cascades() {
    let app = boot().await;
    let token = login_after_register(&app.router, "alice").await;
    let id = add_data_source(&app.router, &app.warehouse, &token).await;
    let table_id = add_orders(&app.router, &id, &token).await;

    // Renaming does not touch the detected schema, and the editor is recorded this time.
    let reply = call(
        &app.router,
        Method::PUT,
        &format!("/v1/data-sources/{id}"),
        Some(&json!({"name": "renamed"})),
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(reply, json!({ "id": id }));
    let row = call(
        &app.router,
        Method::GET,
        &format!("/v1/data-sources/{id}"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(row["name"], json!("renamed"));
    assert_eq!(row["default_schema"], json!("main"));
    assert_eq!(row["connection_url"], json!(app.warehouse));

    // New credentials are re-detected; a probe that fails leaves the stored row alone.
    let body = json!({ "connection_url": "sqlite:D:/definitely/not/here/dati-test.db" });
    let reply = call(
        &app.router,
        Method::PUT,
        &format!("/v1/data-sources/{id}"),
        Some(&body),
        Some(&token),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.code(), "DS001");
    let row = call(
        &app.router,
        Method::GET,
        &format!("/v1/data-sources/{id}"),
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(
        row["connection_url"],
        json!(app.warehouse),
        "a failed probe never reached the row"
    );

    let reply = call(
        &app.router,
        Method::DELETE,
        &format!("/v1/data-sources/{id}"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(reply.expect(StatusCode::OK), json!({ "id": id }));
    let reply = call(
        &app.router,
        Method::GET,
        &format!("/v1/data-sources/{id}"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(reply.code(), "DS002");
    let reply = call(
        &app.router,
        Method::GET,
        &format!("/v1/data-sources/{id}/tables/{table_id}/columns"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(
        reply.code(),
        "DS007",
        "the cascade took the table, and columns resolve the table first"
    );
    let listed = call(
        &app.router,
        Method::GET,
        "/v1/data-sources",
        None,
        Some(&token),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(listed["total"], json!(0));
}
