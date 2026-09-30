//! End-to-end checks of the MCP service slice: `/v1/mcp-services` and its data scope, over a real
//! sqlite metadata store created per test and never touched outside this machine.

use axum::Router;
use axum::body::to_bytes;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use dati_app::{api, state::AppState};
use dati_core::config::{Config, MetaKind};
use dati_core::db::Db;

async fn boot() -> (Router, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_string_lossy().replace('\\', "/");
    // The pools keep the files open; dropping the guard would delete them mid-test.
    std::mem::forget(dir);
    let db = Db::connect(&format!("sqlite:{root}/meta.db?mode=rwc"), MetaKind::Sqlite)
        .await
        .expect("metadata store");
    let config = Config::from_source(|key| (key == "ADMIN_USERS").then(|| "root".to_string()));
    (api::router(AppState::new(db, config)), root)
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

/// A registered sqlite data source owned by the caller.
async fn data_source(app: &Router, root: &str, token: &str, name: &str) -> String {
    call(
        app,
        Method::POST,
        "/v1/data-sources",
        Some(&json!({
            "name": name,
            "type": "SQLITE",
            "connection_url": format!("sqlite:{root}/{name}.db?mode=rwc"),
            "username": "none",
            "password": "",
        })),
        Some(token),
    )
    .await
    .expect(StatusCode::OK)["id"]
        .as_str()
        .expect("id")
        .to_string()
}

async fn subject_of(app: &Router, token: &str, source_id: &str, name: &str) -> String {
    call(
        app,
        Method::POST,
        "/v1/subjects",
        Some(&json!({ "name": name, "datasource_id": source_id })),
        Some(token),
    )
    .await
    .expect(StatusCode::OK)["id"]
        .as_str()
        .expect("id")
        .to_string()
}

/// A draft service owned by `token`, scoped to one data source.
async fn service(app: &Router, token: &str, code: &str, source_id: &str) -> String {
    call(
        app,
        Method::POST,
        "/v1/mcp-services",
        Some(&json!({
            "code": code,
            "name": "销售问数",
            "data_scopes": [{ "scope_type": "DATA_SOURCE", "reference_id": source_id }],
        })),
        Some(token),
    )
    .await
    .expect(StatusCode::OK)["id"]
        .as_str()
        .expect("id")
        .to_string()
}

/// The config a parameterized tool is stored as: a JSON document in a string field.
const PARAM_CONFIG: &str = r#"{"sql_template":"SELECT * FROM orders WHERE city = {{city}}","parameters":[{"name":"city","type":"string"}]}"#;

#[tokio::test]
async fn tools_read_their_defaults_and_an_author_writes_a_custom_one() {
    let (app, root) = boot().await;
    let alice = register(&app, "alice").await;
    let warehouse = data_source(&app, &root, &alice, "warehouse").await;
    let id = service(&app, &alice, "sales", &warehouse).await;
    let tools = format!("/v1/mcp-services/{id}/tools");

    let list = call(&app, Method::GET, &tools, None, Some(&alice))
        .await
        .expect(StatusCode::OK);
    assert_eq!(list["prebuilt"].as_array().expect("prebuilt").len(), 7);
    assert_eq!(list["prebuilt"][0]["id"], "SEARCH_METADATA");
    assert_eq!(list["prebuilt"][0]["name"], "search_tables_and_terms");
    assert_eq!(list["prebuilt"][3]["enabled"], true);
    assert_eq!(
        list["prebuilt"][3]["config"]["timeout"], 30,
        "an untouched EXECUTE_SQL reports its own defaults"
    );
    assert_eq!(
        list["prebuilt"][6]["enabled"], false,
        "UPSERT_TERM starts disabled"
    );
    assert!(list["custom"].as_array().expect("custom").is_empty());

    // The editor's annotation probe needs no permission on the service, only a login.
    let detected = call(
        &app,
        Method::POST,
        &format!("{tools}/detect-annotations"),
        Some(&json!({
            "template": "SELECT * FROM orders WHERE city = {{city}}",
            "parameters": [{ "name": "city", "type": "string" }],
        })),
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(detected["detected_operation"], "SELECT");
    assert_eq!(detected["read_only"], true);
    assert_eq!(detected["destructive"], false);

    let created = call(
        &app,
        Method::POST,
        &tools,
        Some(&json!({
            "tool_type": "PARAMETERIZED_SQL",
            "name": "by_city",
            "description": "按城市统计订单",
            "config": PARAM_CONFIG,
        })),
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    let tool_id = created["id"].as_str().expect("id").to_string();

    let list = call(&app, Method::GET, &tools, None, Some(&alice))
        .await
        .expect(StatusCode::OK);
    assert_eq!(list["custom"].as_array().expect("custom").len(), 1);
    assert_eq!(list["custom"][0]["name"], "by_city");
    assert_eq!(list["custom"][0]["config"]["parameters"][0]["name"], "city");

    // The service view now counts the custom tool alongside the seven slots.
    let view = call(
        &app,
        Method::GET,
        &format!("/v1/mcp-services/{id}"),
        None,
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(view["tool_count"], 8);

    // A prebuilt update addresses the slot by its type.
    call(
        &app,
        Method::PUT,
        &format!("{tools}/EXECUTE_SQL"),
        Some(&json!({ "tool_type": "EXECUTE_SQL", "enabled": false })),
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    let list = call(&app, Method::GET, &tools, None, Some(&alice))
        .await
        .expect(StatusCode::OK);
    assert_eq!(list["prebuilt"][3]["enabled"], false);

    call(
        &app,
        Method::DELETE,
        &format!("{tools}/{tool_id}"),
        None,
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    let list = call(&app, Method::GET, &tools, None, Some(&alice))
        .await
        .expect(StatusCode::OK);
    assert!(list["custom"].as_array().expect("custom").is_empty());

    // A template referencing an undeclared parameter is refused as MS009, before anything is stored.
    let bad = call(
        &app,
        Method::POST,
        &tools,
        Some(&json!({
            "tool_type": "PARAMETERIZED_SQL",
            "name": "broken",
            "description": "d",
            "config": r#"{"sql_template":"SELECT * FROM t WHERE c = {{city}}"}"#,
        })),
        Some(&alice),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad.code(), "MS009");
}

#[tokio::test]
async fn prompts_round_trip_and_content_is_checked_against_parameters() {
    let (app, root) = boot().await;
    let alice = register(&app, "alice").await;
    let warehouse = data_source(&app, &root, &alice, "warehouse").await;
    let id = service(&app, &alice, "sales", &warehouse).await;
    let prompts = format!("/v1/mcp-services/{id}/prompts");

    let created = call(
        &app,
        Method::POST,
        &prompts,
        Some(&json!({
            "name": "weekly",
            "description": "周报",
            "content": "统计 {{city}} 的订单",
            "parameters": [{ "name": "city" }],
        })),
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    let prompt_id = created["id"].as_str().expect("id").to_string();

    let list = call(&app, Method::GET, &prompts, None, Some(&alice))
        .await
        .expect(StatusCode::OK);
    let rows = list.as_array().expect("array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "weekly");
    assert_eq!(rows[0]["enabled"], true, "a create always starts enabled");
    assert_eq!(rows[0]["parameters"][0]["name"], "city");

    // Content and declared parameters must agree.
    let mismatch = call(
        &app,
        Method::POST,
        &prompts,
        Some(&json!({
            "name": "wrong",
            "content": "统计 {{region}} 的订单",
            "parameters": [{ "name": "city" }],
        })),
        Some(&alice),
    )
    .await;
    assert_eq!(mismatch.status, StatusCode::BAD_REQUEST);
    assert_eq!(mismatch.code(), "MS007");

    call(
        &app,
        Method::DELETE,
        &format!("{prompts}/{prompt_id}"),
        None,
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    assert!(
        call(&app, Method::GET, &prompts, None, Some(&alice))
            .await
            .expect(StatusCode::OK)
            .as_array()
            .expect("array")
            .is_empty()
    );
}

#[tokio::test]
async fn template_preview_needs_a_login_but_not_a_service() {
    let (app, _root) = boot().await;
    let alice = register(&app, "alice").await;

    let rendered = call(
        &app,
        Method::POST,
        "/v1/template/preview",
        Some(&json!({
            "mode": "SQL",
            "template": "SELECT * FROM orders WHERE city = {{city}} AND amount > {{min}}",
            "values": { "city": "上海", "min": 100 },
        })),
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(
        rendered["rendered"],
        "SELECT * FROM orders WHERE city = '上海' AND amount > 100"
    );

    let extracted = call(
        &app,
        Method::POST,
        "/v1/template/extract",
        Some(&json!({ "template": "SELECT {{_now}} FROM t WHERE a = {{city}} AND b = {{city}}" })),
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(
        extracted["variables"],
        json!(["city"]),
        "system variables are not listed"
    );

    let anonymous = call(
        &app,
        Method::POST,
        "/v1/template/preview",
        Some(&json!({})),
        None,
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_service_is_born_a_draft_and_carries_its_scope_and_endpoint() {
    let (app, root) = boot().await;
    let alice = register(&app, "alice").await;
    let warehouse = data_source(&app, &root, &alice, "warehouse").await;
    let domain = subject_of(&app, &alice, &warehouse, "交易域").await;

    let created = call(
        &app,
        Method::POST,
        "/v1/mcp-services",
        Some(&json!({
            "code": "sales-mcp",
            "name": "销售问数",
            "description": "面向销售的 MCP 服务",
            "data_scopes": [
                { "scope_type": "DATA_SOURCE", "reference_id": warehouse },
                { "scope_type": "SUBJECT", "reference_id": domain },
            ],
        })),
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    let id = created["id"].as_str().expect("id").to_string();

    let view = call(
        &app,
        Method::GET,
        &format!("/v1/mcp-services/{id}"),
        None,
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(view["code"], "sales-mcp");
    assert_eq!(view["status"], "DRAFT");
    assert_eq!(view["endpoint_path"], "/sales-mcp/mcp");
    assert_eq!(
        view["tool_count"], 7,
        "the prebuilt slots exist before any tool row does"
    );
    assert_eq!(view["active_version_number"], Value::Null);
    assert_eq!(view["created_user_name"], "alice");

    let scope = call(
        &app,
        Method::GET,
        &format!("/v1/mcp-services/{id}/data-scope"),
        None,
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(scope["items"].as_array().expect("items").len(), 2);
    assert_eq!(scope["items"][0]["reference_name"], "warehouse");
    assert_eq!(scope["items"][1]["reference_name"], "交易域");
    // The subject resolves to the source it belongs to, so the list holds one entry.
    let resolved: Vec<&str> = scope["resolved_data_sources"]
        .as_array()
        .expect("resolved")
        .iter()
        .map(|row| row["id"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(resolved, vec![warehouse.as_str()]);

    // Only the profile moves; the code and the status stay put.
    call(
        &app,
        Method::PUT,
        &format!("/v1/mcp-services/{id}"),
        Some(&json!({ "name": "改名", "code": "hacked", "status": "PUBLISHED" })),
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    let view = call(
        &app,
        Method::GET,
        &format!("/v1/mcp-services/{id}"),
        None,
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(view["name"], "改名");
    assert_eq!(view["code"], "sales-mcp");
    assert_eq!(view["status"], "DRAFT");

    // A whole replacement of the scope, and an empty one is legal here.
    call(
        &app,
        Method::PUT,
        &format!("/v1/mcp-services/{id}/data-scope"),
        Some(&json!({ "items": [{ "scope_type": "SUBJECT", "reference_id": domain }] })),
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    let scope = call(
        &app,
        Method::GET,
        &format!("/v1/mcp-services/{id}/data-scope"),
        None,
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(scope["items"].as_array().expect("items").len(), 1);

    call(
        &app,
        Method::PUT,
        &format!("/v1/mcp-services/{id}/data-scope"),
        Some(&json!({ "items": [] })),
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    let scope = call(
        &app,
        Method::GET,
        &format!("/v1/mcp-services/{id}/data-scope"),
        None,
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    assert!(scope["items"].as_array().expect("items").is_empty());
    assert!(
        scope["resolved_data_sources"]
            .as_array()
            .expect("resolved")
            .is_empty()
    );

    call(
        &app,
        Method::DELETE,
        &format!("/v1/mcp-services/{id}"),
        None,
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    let gone = call(
        &app,
        Method::GET,
        &format!("/v1/mcp-services/{id}"),
        None,
        Some(&alice),
    )
    .await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
    assert_eq!(gone.code(), "MS001");
    assert_eq!(gone.message(), format!("MCP service not found: {id}"));
}

#[tokio::test]
async fn the_code_and_the_scope_are_checked_before_anything_is_stored() {
    let (app, root) = boot().await;
    let alice = register(&app, "alice").await;
    let bob = register(&app, "bob").await;
    let theirs = data_source(&app, &root, &bob, "theirs").await;

    let blank = call(
        &app,
        Method::POST,
        "/v1/mcp-services",
        Some(&json!({ "code": "", "name": "s", "data_scopes": [] })),
        Some(&alice),
    )
    .await;
    assert_eq!(blank.status, StatusCode::BAD_REQUEST);
    assert_eq!(blank.code(), "CM001");
    assert_eq!(
        blank.message(),
        "Request parameter error: code: must not be blank; dataScopes: must not be empty"
    );

    let bad_code = call(
        &app,
        Method::POST,
        "/v1/mcp-services",
        Some(&json!({
            "code": "Sales!",
            "name": "s",
            "data_scopes": [{ "scope_type": "DATA_SOURCE", "reference_id": "whatever" }],
        })),
        Some(&alice),
    )
    .await;
    assert_eq!(bad_code.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad_code.code(), "MS011");

    let not_mine = call(
        &app,
        Method::POST,
        "/v1/mcp-services",
        Some(&json!({
            "code": "sales",
            "name": "s",
            "data_scopes": [{ "scope_type": "DATA_SOURCE", "reference_id": theirs }],
        })),
        Some(&alice),
    )
    .await;
    assert_eq!(not_mine.status, StatusCode::FORBIDDEN);
    assert_eq!(not_mine.code(), "PM001");

    let item = json!({ "scope_type": "SUBJECT" });
    let created = call(
        &app,
        Method::POST,
        "/v1/mcp-services",
        Some(&json!({
            "code": "sales",
            "name": "销售问数",
            "data_scopes": [{ "scope_type": "DATA_SOURCE", "reference_id": theirs }],
        })),
        Some(&bob),
    )
    .await
    .expect(StatusCode::OK)["id"]
        .as_str()
        .expect("id")
        .to_string();

    let duplicate = call(
        &app,
        Method::POST,
        "/v1/mcp-services",
        Some(&json!({
            "code": "sales",
            "name": "another",
            "data_scopes": [{ "scope_type": "DATA_SOURCE", "reference_id": "x" }],
        })),
        Some(&bob),
    )
    .await;
    assert_eq!(duplicate.status, StatusCode::CONFLICT);
    assert_eq!(duplicate.code(), "MS010");
    assert_eq!(duplicate.message(), "Service code already exists: sales");

    // A stranger cannot read it either, and an incomplete scope item never reaches the store.
    let denied = call(
        &app,
        Method::GET,
        &format!("/v1/mcp-services/{created}"),
        None,
        Some(&alice),
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert_eq!(denied.code(), "PM001");

    let incomplete = call(
        &app,
        Method::PUT,
        &format!("/v1/mcp-services/{created}/data-scope"),
        Some(&json!({ "items": [item] })),
        Some(&bob),
    )
    .await;
    assert_eq!(incomplete.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        incomplete.message(),
        "Request parameter error: items[0].referenceId: must not be null"
    );
    let untouched = call(
        &app,
        Method::GET,
        &format!("/v1/mcp-services/{created}/data-scope"),
        None,
        Some(&bob),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(untouched["items"].as_array().expect("items").len(), 1);
}

#[tokio::test]
async fn the_list_filters_by_keyword_and_status_and_hides_foreign_rows() {
    let (app, root) = boot().await;
    let alice = register(&app, "alice").await;
    let root_token = register(&app, "root").await;
    let warehouse = data_source(&app, &root, &alice, "warehouse").await;
    call(
        &app,
        Method::POST,
        "/v1/mcp-services",
        Some(&json!({
            "code": "sales-mcp",
            "name": "销售问数",
            "data_scopes": [{ "scope_type": "DATA_SOURCE", "reference_id": warehouse }],
        })),
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);

    let page = call(
        &app,
        Method::GET,
        "/v1/mcp-services?keyword=SALES-MCP",
        None,
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(page["total"], 1);
    assert_eq!(page["data"][0]["code"], "sales-mcp");
    assert_eq!(page["page"], 1);
    assert_eq!(page["size"], 10);
    assert_eq!(page["total_pages"], 1);

    let miss = call(
        &app,
        Method::GET,
        "/v1/mcp-services?keyword=nothing",
        None,
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(miss["total"], 0);
    assert_eq!(miss["data"].as_array().expect("data").len(), 0);

    let published = call(
        &app,
        Method::GET,
        "/v1/mcp-services?status=PUBLISHED",
        None,
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(published["total"], 0, "a draft is not published");
    let drafts = call(
        &app,
        Method::GET,
        "/v1/mcp-services?status=DRAFT",
        None,
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(drafts["total"], 1);

    let nonsense = call(
        &app,
        Method::GET,
        "/v1/mcp-services?status=PUBLISH",
        None,
        Some(&alice),
    )
    .await;
    assert_eq!(nonsense.status, StatusCode::BAD_REQUEST);
    assert_eq!(nonsense.code(), "CM001");
    assert_eq!(
        nonsense.message(),
        "Request parameter error: status: must be one of DRAFT, PUBLISHED, DISABLED"
    );

    // A stranger's list is empty; the admin's holds the row.
    let bob = register(&app, "bob").await;
    assert_eq!(
        call(&app, Method::GET, "/v1/mcp-services", None, Some(&bob))
            .await
            .expect(StatusCode::OK)["total"],
        0
    );
    assert_eq!(
        call(
            &app,
            Method::GET,
            "/v1/mcp-services",
            None,
            Some(&root_token)
        )
        .await
        .expect(StatusCode::OK)["total"],
        1
    );

    // Nothing is readable without a credential at all.
    let anonymous = call(&app, Method::GET, "/v1/mcp-services", None, None).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    assert_eq!(anonymous.body, Value::Null);
}

#[tokio::test]
async fn the_publish_lifecycle_guards_its_transitions() {
    let (app, root) = boot().await;
    let alice = register(&app, "alice").await;
    let warehouse = data_source(&app, &root, &alice, "warehouse").await;
    let id = service(&app, &alice, "sales", &warehouse).await;

    // A draft cannot be taken down — it was never online.
    let bad_transition = call(
        &app,
        Method::POST,
        &format!("/v1/mcp-services/{id}/disable"),
        None,
        Some(&alice),
    )
    .await;
    assert_eq!(bad_transition.status, StatusCode::CONFLICT);
    assert_eq!(bad_transition.code(), "MS016");
    assert_eq!(
        bad_transition.message(),
        "Invalid status transition for MCP service: \
         Only PUBLISHED service can be disabled, current: DRAFT"
    );

    // With the scope cleared there is nothing to publish.
    call(
        &app,
        Method::PUT,
        &format!("/v1/mcp-services/{id}/data-scope"),
        Some(&json!({ "items": [] })),
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    let empty = call(
        &app,
        Method::POST,
        &format!("/v1/mcp-services/{id}/publish"),
        Some(&json!({})),
        Some(&alice),
    )
    .await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST);
    assert_eq!(empty.code(), "MS019");
    assert_eq!(
        empty.message(),
        format!("Data scope is empty, cannot publish MCP service: {id}")
    );

    // An anonymous publish is rejected at the door.
    let anonymous = call(
        &app,
        Method::POST,
        &format!("/v1/mcp-services/{id}/publish"),
        Some(&json!({})),
        None,
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_snapshot_freezes_the_service_and_a_rollback_brings_it_back() {
    let (app, root) = boot().await;
    let alice = register(&app, "alice").await;
    let warehouse = data_source(&app, &root, &alice, "warehouse").await;
    let id = service(&app, &alice, "sales", &warehouse).await;
    let base = format!("/v1/mcp-services/{id}");

    // Put a custom tool and a prompt in the draft so the snapshot has something to freeze.
    call(
        &app,
        Method::POST,
        &format!("{base}/tools"),
        Some(&json!({
            "tool_type": "PARAMETERIZED_SQL",
            "name": "by_city",
            "description": "按城市统计订单",
            "config": PARAM_CONFIG,
        })),
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    call(
        &app,
        Method::POST,
        &format!("{base}/prompts"),
        Some(&json!({
            "name": "weekly",
            "content": "统计 {{city}} 的订单",
            "parameters": [{ "name": "city" }],
        })),
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);

    // Before any publish there is no running version, so the diff reports an initial release.
    let initial = call(
        &app,
        Method::GET,
        &format!("{base}/diff"),
        None,
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(initial["has_changes"], true);
    assert_eq!(initial["modified_components"], json!(["INITIAL_RELEASE"]));
    assert_eq!(initial["active_version_number"], Value::Null);

    // Publish: the returned id is the new snapshot's, and the service becomes active and published.
    let published = call(
        &app,
        Method::POST,
        &format!("{base}/publish"),
        Some(&json!({ "release_note": "  first  " })),
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    let v1 = published["id"].as_str().expect("snapshot id").to_string();

    let view = call(&app, Method::GET, &base, None, Some(&alice))
        .await
        .expect(StatusCode::OK);
    assert_eq!(view["status"], "PUBLISHED");
    assert_eq!(view["active_version_number"], 1);

    let clean = call(
        &app,
        Method::GET,
        &format!("{base}/diff"),
        None,
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(clean["has_changes"], false);
    assert_eq!(clean["active_version_number"], 1);
    assert_eq!(
        clean["modified_components"]
            .as_array()
            .expect("components")
            .len(),
        0
    );

    let snapshots = call(
        &app,
        Method::GET,
        &format!("{base}/snapshots"),
        None,
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    let rows = snapshots.as_array().expect("array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], v1);
    assert_eq!(rows[0]["version_number"], 1);
    assert_eq!(
        rows[0]["release_note"], "first",
        "the note is trimmed on the way in"
    );
    assert_eq!(rows[0]["created_user_name"], "alice");

    // Drift the draft: a second tool and a renamed service.
    call(
        &app,
        Method::POST,
        &format!("{base}/tools"),
        Some(&json!({
            "tool_type": "PARAMETERIZED_SQL",
            "name": "by_date",
            "description": "按日期统计",
            "config": PARAM_CONFIG,
        })),
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    call(
        &app,
        Method::PUT,
        &base,
        Some(&json!({ "name": "改名" })),
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);

    let dirty = call(
        &app,
        Method::GET,
        &format!("{base}/diff"),
        None,
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(dirty["has_changes"], true);
    assert_eq!(dirty["basic_info_changed"], true);
    assert_eq!(dirty["tools_changed"], true);
    assert_eq!(dirty["added_tools"], json!(["by_date"]));

    // Roll back to v1: the restored draft is republished as version 2.
    let rolled = call(
        &app,
        Method::POST,
        &format!("{base}/rollback"),
        Some(&json!({ "target_version_number": 1, "release_note": "undo" })),
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    let v2 = rolled["id"].as_str().expect("snapshot id").to_string();
    assert_ne!(v1, v2);

    let view = call(&app, Method::GET, &base, None, Some(&alice))
        .await
        .expect(StatusCode::OK);
    assert_eq!(view["name"], "销售问数", "the frozen profile came back");
    assert_eq!(view["status"], "PUBLISHED");
    assert_eq!(view["active_version_number"], 2);
    assert_eq!(
        view["tool_count"], 8,
        "the tool added after v1 is gone (7 prebuilt + by_city)"
    );

    let snapshots = call(
        &app,
        Method::GET,
        &format!("{base}/snapshots"),
        None,
        Some(&alice),
    )
    .await
    .expect(StatusCode::OK);
    let rows = snapshots.as_array().expect("array");
    assert_eq!(rows.len(), 2, "history is append-only");
    assert_eq!(rows[0]["version_number"], 2);
    assert_eq!(rows[0]["release_note"], "Rollback to v1: undo");
    assert_eq!(rows[1]["version_number"], 1);

    // A rollback to a version that never existed reports MS017 before touching anything.
    let missing = call(
        &app,
        Method::POST,
        &format!("{base}/rollback"),
        Some(&json!({ "target_version_number": 99 })),
        Some(&alice),
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.code(), "MS017");
    assert_eq!(
        missing.message(),
        format!("MCP service version not found: v99 of service {id}")
    );

    // A rollback with no target is a parameter error.
    let no_target = call(
        &app,
        Method::POST,
        &format!("{base}/rollback"),
        Some(&json!({ "release_note": "x" })),
        Some(&alice),
    )
    .await;
    assert_eq!(no_target.status, StatusCode::BAD_REQUEST);
    assert_eq!(no_target.code(), "CM001");
    assert_eq!(
        no_target.message(),
        "Request parameter error: target_version_number: must not be null"
    );
}
