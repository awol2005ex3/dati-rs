//! End-to-end checks of the MCP transport: `POST /{code}/mcp` over a real sqlite metadata store,
//! from the draft that must stay invisible to the usage rows a published call leaves behind.
//!
//! The gate order, the envelopes and the rendered prompts are all asserted through HTTP here, so
//! nothing depends on the endpoint's internals — which is the point: the reused frontend and any
//! MCP client only ever see this surface.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::Router;
use axum::body::to_bytes;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use dati_app::{api, state::AppState};
use dati_core::config::{Config, MetaKind};
use dati_core::db::Db;

/// `McpProtocolHandler.PROTOCOL_VERSION`, the only value the transport gate accepts.
const PROTOCOL: &str = "2025-11-25";
const VERSION_HEADER: &str = "mcp-protocol-version";

/// The peer the test asks axum to report, and so the client ip of a call with no proxy header.
fn peer() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9)), 5000)
}

async fn boot() -> (Router, String, AppState) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_string_lossy().replace('\\', "/");
    // The pools keep the files open; dropping the guard would delete them mid-test.
    std::mem::forget(dir);
    let db = Db::connect(&format!("sqlite:{root}/meta.db?mode=rwc"), MetaKind::Sqlite)
        .await
        .expect("metadata store");
    let config = Config::from_source(|key| (key == "ADMIN_USERS").then(|| "root".to_string()));
    let state = AppState::new(db, config);
    // Production binds the peer address with `into_make_service_with_connect_info`; a test router
    // reaches the same extractor through this layer.
    let app = api::router(state.clone()).layer(MockConnectInfo(peer()));
    (app, root, state)
}

struct Rpc {
    status: StatusCode,
    body: Value,
}

impl Rpc {
    fn code(&self) -> Option<i64> {
        self.body.get("error")?.get("code")?.as_i64()
    }

    fn message(&self) -> &str {
        self.body
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or_default()
    }

    fn expect(self, status: StatusCode) -> Value {
        assert_eq!(self.status, status, "{:?} {}", self.body, status);
        self.body
    }

    /// The status and nothing else: how the transport refuses a request it will not answer.
    fn expect_silent(self, status: StatusCode) {
        assert_eq!(self.status, status, "{}", self.body);
        assert_eq!(self.body, Value::Null, "no body was written");
    }
}

async fn request(
    app: &Router,
    method: Method,
    uri: &str,
    payload: Option<&str>,
    token: Option<&str>,
    extra: &[(&str, &str)],
) -> Rpc {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    for (name, value) in extra {
        builder = builder.header(*name, *value);
    }
    let body = match payload {
        Some(payload) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            axum::body::Body::from(payload.to_string())
        }
        None => axum::body::Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).expect("valid request"))
        .await
        .expect("infallible router");
    let (parts, body) = response.into_parts();
    let bytes = to_bytes(body, 1 << 20).await.expect("readable body");
    Rpc {
        status: parts.status,
        body: if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).expect("json body")
        },
    }
}

/// One JSON-RPC request of `code`. Every caller sends the version header unless it is testing
/// the gate that reads it.
async fn mcp(
    app: &Router,
    code: &str,
    body: &Value,
    token: Option<&str>,
    extra: &[(&str, &str)],
) -> Rpc {
    let mut headers: Vec<(&str, &str)> = vec![(VERSION_HEADER, PROTOCOL)];
    headers.extend_from_slice(extra);
    request(
        app,
        Method::POST,
        &format!("/{code}/mcp"),
        Some(&body.to_string()),
        token,
        &headers,
    )
    .await
}

/// The same, with a body the transport never gets to parse as JSON.
async fn mcp_raw(app: &Router, code: &str, body: &str, token: &str) -> Rpc {
    request(
        app,
        Method::POST,
        &format!("/{code}/mcp"),
        Some(body),
        Some(token),
        &[(VERSION_HEADER, PROTOCOL)],
    )
    .await
}

async fn call(app: &Router, method: Method, uri: &str, body: Option<&Value>, token: &str) -> Value {
    let payload = body.map(|body| body.to_string());
    request(app, method, uri, payload.as_deref(), Some(token), &[])
        .await
        .expect(StatusCode::OK)
}

async fn register(app: &Router, name: &str) -> String {
    call(
        app,
        Method::POST,
        "/v1/auth/register",
        Some(&json!({ "name": name, "password": "wonderland", "display_name": name })),
        name,
    )
    .await;
    call(
        app,
        Method::POST,
        "/v1/auth/login",
        Some(&json!({ "type": "local", "name": name, "password": "wonderland" })),
        name,
    )
    .await["token"]
        .as_str()
        .expect("token")
        .to_string()
}

/// A published service named `sales`: one sqlite source in scope and one prompt with a required
/// argument. Returns the service id and the source id.
async fn published(app: &Router, root: &str, token: &str) -> (String, String) {
    let source = call(
        app,
        Method::POST,
        "/v1/data-sources",
        Some(&json!({
            "name": "warehouse",
            "type": "SQLITE",
            "connection_url": format!("sqlite:{root}/warehouse.db?mode=rwc"),
            "username": "none",
            "password": "",
        })),
        token,
    )
    .await["id"]
        .as_str()
        .expect("id")
        .to_string();
    let id = call(
        app,
        Method::POST,
        "/v1/mcp-services",
        Some(&json!({
            "code": "sales",
            "name": "销售问数",
            "description": "面向销售的 MCP 服务",
            "data_scopes": [{ "scope_type": "DATA_SOURCE", "reference_id": source }],
        })),
        token,
    )
    .await["id"]
        .as_str()
        .expect("id")
        .to_string();
    call(
        app,
        Method::POST,
        &format!("/v1/mcp-services/{id}/prompts"),
        Some(&json!({
            "name": "weekly",
            "description": "周报",
            "content": "统计 {{city}} 的订单",
            "parameters": [{ "name": "city", "description": "城市", "required": true }],
        })),
        token,
    )
    .await;
    call(
        app,
        Method::POST,
        &format!("/v1/mcp-services/{id}/publish"),
        Some(&json!({ "release_note": "第一次上线" })),
        token,
    )
    .await;
    (id, source)
}

fn envelope(method: &str, id: Value, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn call_of(id: i64, name: &str, arguments: Value) -> Value {
    envelope(
        "tools/call",
        json!(id),
        json!({ "name": name, "arguments": arguments }),
    )
}

#[tokio::test]
async fn a_draft_is_invisible_and_a_published_service_answers_the_handshake() {
    let (app, root, _state) = boot().await;
    let alice = register(&app, "alice").await;
    let source = call(
        &app,
        Method::POST,
        "/v1/data-sources",
        Some(&json!({
            "name": "warehouse",
            "type": "SQLITE",
            "connection_url": format!("sqlite:{root}/warehouse.db?mode=rwc"),
            "username": "none",
            "password": "",
        })),
        &alice,
    )
    .await["id"]
        .as_str()
        .expect("id")
        .to_string();
    let id = call(
        &app,
        Method::POST,
        "/v1/mcp-services",
        Some(&json!({
            "code": "sales",
            "name": "销售问数",
            "description": "面向销售的 MCP 服务",
            "data_scopes": [{ "scope_type": "DATA_SOURCE", "reference_id": source }],
        })),
        &alice,
    )
    .await["id"]
        .as_str()
        .expect("id")
        .to_string();

    let initialize = envelope("initialize", json!(1), Value::Null);
    // A draft is answered exactly like a code nobody ever registered.
    mcp(&app, "sales", &initialize, Some(&alice), &[])
        .await
        .expect_silent(StatusCode::NOT_FOUND);
    mcp(&app, "never-heard-of-it", &initialize, Some(&alice), &[])
        .await
        .expect_silent(StatusCode::NOT_FOUND);

    call(
        &app,
        Method::POST,
        &format!("/v1/mcp-services/{id}/publish"),
        Some(&json!({ "release_note": "第一次上线" })),
        &alice,
    )
    .await;

    // `initialize` is the one method the version header may skip, and this call sends none.
    let hello = request(
        &app,
        Method::POST,
        "/sales/mcp",
        Some(&initialize.to_string()),
        Some(&alice),
        &[],
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(hello["result"]["protocolVersion"], PROTOCOL);
    assert_eq!(
        hello["result"]["capabilities"],
        json!({"tools": {"listChanged": false}}),
        "a service with no published prompt does not advertise one, and the stateless endpoint \
         says it will never announce a change to the tools it does have"
    );
    assert_eq!(
        hello["result"]["serverInfo"],
        json!({"name": "销售问数", "version": "v1", "description": "面向销售的 MCP 服务"})
    );

    let tools = mcp(
        &app,
        "sales",
        &envelope("tools/list", json!(2), Value::Null),
        Some(&alice),
        &[],
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(
        tools["result"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|tool| tool["name"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        vec![
            "search_tables_and_terms",
            "get_table_schema",
            "list_tables_and_terms",
            "execute_sql",
        ],
        "the three metadata-writing tools start disabled, so they are not listed at all"
    );
    let listed = &tools["result"]["tools"];
    assert_eq!(listed[0]["title"], "Search Tables and Terms");
    assert_eq!(listed[0]["annotations"], json!({"readOnlyHint": true}));
    assert!(
        listed[3].get("annotations").is_none(),
        "EXECUTE_SQL declares none: {}",
        listed[3]
    );
    assert_eq!(
        listed[3]["inputSchema"]["required"],
        json!(["data_source_id", "sql"])
    );
    // Its description is written per service, from the slot's policy and the caller.
    let description = listed[3]["description"].as_str().expect("description");
    assert!(
        description.starts_with("Execute an SQL query or statement against a data source"),
        "{description}"
    );
    let (before_id, user_id) = description
        .rsplit_once(", user id: ")
        .expect("the context suffix");
    assert!(
        before_id.ends_with(
            ". Allowed operations: SELECT. Max rows: 1000. Context: current user name: alice"
        ),
        "{before_id}"
    );
    assert!(!user_id.is_empty(), "the caller's id is named last");

    assert_eq!(
        mcp(
            &app,
            "sales",
            &envelope("ping", json!(3), Value::Null),
            Some(&alice),
            &[],
        )
        .await
        .expect(StatusCode::OK)["result"],
        json!({})
    );

    call(
        &app,
        Method::POST,
        &format!("/v1/mcp-services/{id}/disable"),
        None,
        &alice,
    )
    .await;
    let off = mcp(&app, "sales", &initialize, Some(&alice), &[]).await;
    assert_eq!(
        off.expect(StatusCode::SERVICE_UNAVAILABLE),
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {"code": -32603, "message": "Service is disabled"},
        })
    );
}

#[tokio::test]
async fn the_transport_decides_before_the_protocol_ever_runs() {
    let (app, root, _state) = boot().await;
    let alice = register(&app, "alice").await;
    let bob = register(&app, "bob").await;
    published(&app, &root, &alice).await;
    let tools_list = envelope("tools/list", json!("a"), Value::Null);

    // Authentication is the route extractor's job, and it answers the Java interceptor's bare 401.
    request(
        &app,
        Method::POST,
        "/sales/mcp",
        Some(&tools_list.to_string()),
        None,
        &[(VERSION_HEADER, PROTOCOL)],
    )
    .await
    .expect_silent(StatusCode::UNAUTHORIZED);

    // A stranger cannot tell anything about the service: the denial carries the envelope, because
    // the Java handler had already read the body by then.
    let denied = mcp(&app, "sales", &tools_list, Some(&bob), &[]).await;
    assert_eq!(
        denied.expect(StatusCode::FORBIDDEN),
        json!({
            "jsonrpc": "2.0",
            "id": "a",
            "error": {"code": -32603, "message": "Permission denied"},
        })
    );

    // DNS-rebinding protection: a browser origin that is not loopback or whitelisted is turned away
    // without a word, while a localhost one passes.
    mcp(
        &app,
        "sales",
        &tools_list,
        Some(&alice),
        &[("origin", "http://attacker.example")],
    )
    .await
    .expect_silent(StatusCode::FORBIDDEN);
    mcp(
        &app,
        "sales",
        &tools_list,
        Some(&alice),
        &[("origin", "http://localhost:8080")],
    )
    .await
    .expect(StatusCode::OK);

    // The version gate: required for everything after the handshake.
    request(
        &app,
        Method::POST,
        "/sales/mcp",
        Some(&tools_list.to_string()),
        Some(&alice),
        &[],
    )
    .await
    .expect_silent(StatusCode::BAD_REQUEST);
    request(
        &app,
        Method::POST,
        "/sales/mcp",
        Some(&tools_list.to_string()),
        Some(&alice),
        &[(VERSION_HEADER, "2025-06-18")],
    )
    .await
    .expect_silent(StatusCode::BAD_REQUEST);

    // A body the transport cannot read is a parse error with no id to echo.
    let broken = mcp_raw(&app, "sales", "not json at all", &alice).await;
    assert_eq!(broken.status, StatusCode::BAD_REQUEST);
    assert_eq!(broken.code(), Some(-32700));
    assert!(
        broken.message().starts_with("Parse error: "),
        "{}",
        broken.message()
    );
    assert!(broken.body.get("id").is_none(), "{}", broken.body);

    // Readable JSON that is no kind of message is the same refusal, with the Java detail.
    let headless = mcp_raw(&app, "sales", r#"{"jsonrpc":"2.0","id":5}"#, &alice).await;
    assert_eq!(headless.status, StatusCode::BAD_REQUEST);
    assert_eq!(headless.code(), Some(-32700));
    assert_eq!(headless.message(), "Parse error: Unknown message type");
    assert_eq!(headless.body["id"], json!(5));

    // A notification, a batch and a client-side response all get the empty 202.
    mcp_raw(
        &app,
        "sales",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        &alice,
    )
    .await
    .expect_silent(StatusCode::ACCEPTED);
    mcp_raw(
        &app,
        "sales",
        r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#,
        &alice,
    )
    .await
    .expect_silent(StatusCode::ACCEPTED);
    mcp_raw(
        &app,
        "sales",
        r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
        &alice,
    )
    .await
    .expect_silent(StatusCode::ACCEPTED);

    // Past the gates an unknown method is answered by the protocol, with the id echoed intact.
    let unknown = mcp(
        &app,
        "sales",
        &envelope("resources/list", json!(7), Value::Null),
        Some(&alice),
        &[],
    )
    .await;
    assert_eq!(unknown.code(), Some(-32601));
    assert_eq!(unknown.message(), "Method not found: resources/list");
    assert_eq!(unknown.expect(StatusCode::OK)["id"], json!(7));
}

#[tokio::test]
async fn a_prompt_round_trips_through_the_frozen_snapshot() {
    let (app, root, _state) = boot().await;
    let alice = register(&app, "alice").await;
    let (id, _source) = published(&app, &root, &alice).await;

    let hello = mcp(
        &app,
        "sales",
        &envelope("initialize", json!(0), Value::Null),
        Some(&alice),
        &[],
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(
        hello["result"]["capabilities"],
        json!({"tools": {"listChanged": false}, "prompts": {"listChanged": false}}),
        "the published prompt is what makes the second capability appear"
    );

    let listed = mcp(
        &app,
        "sales",
        &envelope("prompts/list", json!(1), Value::Null),
        Some(&alice),
        &[],
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(
        listed["result"]["prompts"],
        json!([{
            "name": "weekly",
            "description": "周报",
            "arguments": [{"name": "city", "description": "城市", "required": true}],
        }])
    );

    let got = mcp(
        &app,
        "sales",
        &envelope(
            "prompts/get",
            json!(2),
            json!({"name": "weekly", "arguments": {"city": "上海"}}),
        ),
        Some(&alice),
        &[],
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(
        got["result"]["messages"],
        json!([{"role": "user", "content": {"type": "text", "text": "统计 上海 的订单"}}])
    );

    // The two refusals are argument errors, and they arrive before the template runs.
    let missing = mcp(
        &app,
        "sales",
        &envelope("prompts/get", json!(3), json!({"name": "weekly"})),
        Some(&alice),
        &[],
    )
    .await;
    assert_eq!(missing.code(), Some(-32602));
    assert_eq!(missing.message(), "Missing required argument: city");

    let unknown = mcp(
        &app,
        "sales",
        &envelope(
            "prompts/get",
            json!(4),
            json!({"name": "nope", "arguments": {"city": "上海"}}),
        ),
        Some(&alice),
        &[],
    )
    .await;
    assert_eq!(unknown.code(), Some(-32602));
    assert_eq!(unknown.message(), "Unknown prompt: nope");

    // An edit to the draft is invisible until the next publish: the endpoint read the snapshot, so
    // disabling the prompt now changes nothing for a client.
    let prompt_id = call(
        &app,
        Method::GET,
        &format!("/v1/mcp-services/{id}/prompts"),
        None,
        &alice,
    )
    .await[0]["id"]
        .as_str()
        .expect("id")
        .to_string();
    call(
        &app,
        Method::PUT,
        &format!("/v1/mcp-services/{id}/prompts/{prompt_id}"),
        Some(&json!({ "name": "weekly", "enabled": false })),
        &alice,
    )
    .await;
    mcp(
        &app,
        "sales",
        &envelope(
            "prompts/get",
            json!(5),
            json!({"name": "weekly", "arguments": {"city": "北京"}}),
        ),
        Some(&alice),
        &[],
    )
    .await
    .expect(StatusCode::OK);
}

#[tokio::test]
async fn every_execution_the_endpoint_answers_leaves_a_usage_row() {
    let (app, root, state) = boot().await;
    let alice = register(&app, "alice").await;
    let (id, source) = published(&app, &root, &alice).await;
    let proxied = [("x-forwarded-for", "203.0.113.7, 10.0.0.1")];

    // A SELECT the default policy allows.
    let ok = mcp(
        &app,
        "sales",
        &call_of(
            1,
            "execute_sql",
            json!({"data_source_id": source, "sql": "SELECT 1 AS one"}),
        ),
        Some(&alice),
        &proxied,
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(ok["result"]["isError"], json!(false));
    let payload = &ok["result"]["structuredContent"];
    assert_eq!(payload["type"], "SQL_EXECUTION");
    assert_eq!(payload["executed_sql"], "SELECT 1 AS one");
    assert_eq!(payload["results"][0]["columns"], json!(["one"]));
    assert_eq!(payload["results"][0]["rows"], json!([[1]]));
    assert_eq!(
        ok["result"]["content"][0]["text"],
        payload.to_string(),
        "the SDK put the same JSON in the text block it parsed the content from"
    );

    // A write the policy refuses is the tool's own error, inside a successful response.
    let refused = mcp(
        &app,
        "sales",
        &call_of(
            2,
            "execute_sql",
            json!({"data_source_id": source, "sql": "INSERT INTO orders VALUES (1)"}),
        ),
        Some(&alice),
        &proxied,
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(refused["result"]["isError"], json!(true));
    assert_eq!(refused["id"], json!(2));
    assert_eq!(
        refused["result"]["content"][0]["text"],
        "SQL 操作被策略禁止：INSERT not allowed"
    );
    assert!(
        refused["result"].get("structuredContent").is_none(),
        "a failure carries no payload"
    );

    // Two argument problems, kept apart exactly as the Java handler kept them apart.
    let unknown_tool = mcp(
        &app,
        "sales",
        &call_of(3, "nope", json!({})),
        Some(&alice),
        &[],
    )
    .await;
    assert_eq!(unknown_tool.code(), Some(-32602));
    assert_eq!(unknown_tool.message(), "Unknown tool: nope");

    let bad_params = mcp(
        &app,
        "sales",
        &envelope(
            "tools/call",
            json!(4),
            json!({"name": "execute_sql", "arguments": "not an object"}),
        ),
        Some(&alice),
        &[],
    )
    .await;
    assert_eq!(bad_params.code(), Some(-32603));
    assert_eq!(
        bad_params.message(),
        "Internal error: Failed to convert request params"
    );

    let prompt_ok = mcp(
        &app,
        "sales",
        &envelope(
            "prompts/get",
            json!(5),
            json!({"name": "weekly", "arguments": {"city": "上海"}}),
        ),
        Some(&alice),
        &proxied,
    )
    .await;
    prompt_ok.expect(StatusCode::OK);
    let prompt_failed = mcp(
        &app,
        "sales",
        &envelope("prompts/get", json!(6), json!({"name": "weekly"})),
        Some(&alice),
        &[],
    )
    .await;
    prompt_failed.expect(StatusCode::OK);

    // The handshake and a metadata query are not usage; the six calls above are.
    state.usage.flush().await;
    let stats = call(
        &app,
        Method::GET,
        &format!("/v1/mcp-services/{id}/stats"),
        None,
        &alice,
    )
    .await;
    assert_eq!(stats["total_calls"], 6);
    assert_eq!(
        stats["today_calls"], 6,
        "the default window is the retention"
    );
    assert_eq!(stats["success_rate"], json!(33.3), "two of six");
    assert_eq!(stats["daily_trend"].as_array().expect("days").len(), 1);
    assert_eq!(
        stats["tool_distribution"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|item| item["tool_name"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        vec!["execute_sql", "weekly", "General", "nope"],
        "busiest first, and the unmappable call is named General"
    );
    assert_eq!(stats["tool_distribution"][0]["call_count"], 2);
    assert_eq!(stats["tool_distribution"][0]["success_rate"], json!(50.0));

    let logs = call(
        &app,
        Method::GET,
        &format!("/v1/mcp-services/{id}/invocation-logs?page=1&size=20"),
        None,
        &alice,
    )
    .await;
    assert_eq!(logs["total"], 6);
    let rows = logs["data"].as_array().expect("rows");
    let find = |name: &str| {
        rows.iter()
            .find(|row| row["tool_name"] == json!(name))
            .unwrap_or_else(|| panic!("no {name} row in {rows:?}"))
    };
    assert_eq!(find("execute_sql")["client_ip"], "203.0.113.7");
    assert_eq!(
        find("execute_sql")["method"],
        "tools/call",
        "both of its calls came through the proxy, so either row answers this"
    );
    assert_eq!(
        find("nope")["client_ip"],
        "198.51.100.9",
        "the connected peer"
    );
    assert_eq!(find("nope")["error_message"], "Unknown tool: nope");
    assert_eq!(find("nope")["success"], json!(false));
    // The log keeps the empty tool name the call was recorded under; only the report renames it.
    assert!(
        find("")["error_message"]
            .as_str()
            .is_some_and(|text| text.starts_with("Internal error: ")),
        "{:?}",
        find("")
    );
    assert_eq!(find("weekly")["caller_name"], "alice");

    // The two filters the console sends.
    let filtered = call(
        &app,
        Method::GET,
        &format!("/v1/mcp-services/{id}/invocation-logs?toolName=execute_sql&success=false"),
        None,
        &alice,
    )
    .await;
    assert_eq!(filtered["total"], 1);
    assert_eq!(
        filtered["data"][0]["error_message"],
        "SQL 操作被策略禁止：INSERT not allowed"
    );
    assert_eq!(filtered["size"], 10);

    let bad_filter = request(
        &app,
        Method::GET,
        &format!("/v1/mcp-services/{id}/invocation-logs?success=maybe"),
        None,
        Some(&alice),
        &[],
    )
    .await;
    assert_eq!(bad_filter.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad_filter.body["code"], json!("CM001"));
    assert_eq!(
        bad_filter.body["message"],
        "Request parameter error: success: must be true or false"
    );
}
