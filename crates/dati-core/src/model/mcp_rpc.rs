//! The wire shapes of the MCP endpoint: the JSON-RPC 2.0 envelope and the MCP result objects it
//! carries.
//!
//! Two dialects meet here, as they do in [`crate::model::mcp_tool_data`]: everything in this module
//! is camelCase because it is the protocol envelope the Java MCP SDK wrote, while the tool payload
//! placed inside a `tools/call` result stays snake_case.
//!
//! These are documents rather than domain models — nothing reads them back — so they are built as
//! [`Value`] instead of typed structs with serde renames. The builders mirror the SDK records field
//! for field, including which keys a `null` left out.

use serde_json::{Map, Value, json};

/// The only protocol version the endpoint speaks; `McpProtocolHandler.PROTOCOL_VERSION`.
pub const PROTOCOL_VERSION: &str = "2025-11-25";

/// `McpSchema.ErrorCodes`, of which the endpoint uses four.
pub const PARSE_ERROR: i64 = -32700;
pub const METHOD_NOT_FOUND_CODE: i64 = -32601;
pub const INVALID_PARAMS_CODE: i64 = -32602;
pub const INTERNAL_ERROR_CODE: i64 = -32603;

/// A request id. The Java transport-level error envelope read an integral node as a `long` and
/// everything else as its text, so a number that survives that round trip is echoed as a number.
#[derive(Debug, Clone, PartialEq)]
pub enum Id {
    Number(i64),
    Text(String),
}

impl Id {
    fn read(value: &Value) -> Option<Self> {
        match value {
            Value::Null => None,
            Value::String(text) => Some(Self::Text(text.clone())),
            Value::Number(number) => Some(match number.as_i64() {
                Some(whole) => Self::Number(whole),
                None => Self::Text(number.to_string()),
            }),
            // A boolean, array or object id has no JSON-RPC meaning. Java echoed `asText()`, which
            // for the two containers is the empty string; the text form is the closer port.
            other => Some(Self::Text(other.to_string())),
        }
    }

    pub fn of(body: &Value) -> Option<Self> {
        body.get("id").and_then(Self::read)
    }

    fn into_value(self) -> Value {
        match self {
            Self::Number(whole) => json!(whole),
            Self::Text(text) => json!(text),
        }
    }
}

/// A JSON-RPC request: the three fields the endpoint dispatches on. `params` is kept as a tree
/// because each method reads a different record out of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub id: Id,
    pub method: String,
    pub params: Value,
}

/// The typed `params` of `tools/call` and `prompts/get`. The Java handler mapped the tree onto the
/// SDK's request record, which decided what a mismatch costs: a scalar `name` is coerced to its
/// text, while an object `name` or a non-object `arguments` fails the mapping — and that failure
/// escapes as a JSON-RPC internal error rather than as a missing parameter.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CallParams {
    pub name: Option<String>,
    pub arguments: Map<String, Value>,
}

/// The detail the transport layer reports for a `params` object it cannot map. Jackson named the
/// offending property in a message no client parsed, so the port keeps the one stable form.
pub const INVALID_PARAMS_DETAIL: &str = "Failed to convert request params";

/// The text every `-32603` answer carries: the Java handler prefixed the exception message with it,
/// so a client can tell an abort of the gateway from a tool's own error text.
pub fn internal_error(detail: &str) -> String {
    format!("Internal error: {detail}")
}

impl CallParams {
    /// `None` is the failed mapping; `Some` with both fields unset is what an absent `params`
    /// reads as.
    pub fn read(params: &Value) -> Option<Self> {
        if params.is_null() {
            return Some(Self::default());
        }
        let Value::Object(fields) = params else {
            return None;
        };
        let name = match fields.get("name") {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) => Some(text.clone()),
            Some(Value::Number(_)) | Some(Value::Bool(_)) => Some(fields["name"].to_string()),
            Some(_) => return None,
        };
        let arguments = match fields.get("arguments") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(object)) => object.clone(),
            Some(_) => return None,
        };
        Some(Self { name, arguments })
    }
}

/// What one request body turned out to be.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Request(Request),
    /// A notification, a client-side response, or a batch: the Java handler dispatched only on
    /// `JSONRPCRequest` and answered everything else with 202 and no body.
    Ignored,
}

/// Reads the envelope. `Err` is the text the transport layer reports as a `Parse error`, which is
/// also how a body that is not JSON at all, or not a message, arrives.
pub fn parse(body: &str) -> std::result::Result<Message, String> {
    let root: Value = serde_json::from_str(body).map_err(|err| err.to_string())?;
    let Value::Object(fields) = &root else {
        return Ok(Message::Ignored);
    };
    let method = fields.get("method").and_then(Value::as_str);
    if let Some(method) = method {
        let id = fields.get("id").and_then(Id::read);
        let params = fields.get("params").cloned().unwrap_or(Value::Null);
        return Ok(match id {
            Some(id) => Message::Request(Request {
                id,
                method: method.to_string(),
                params,
            }),
            None => Message::Ignored,
        });
    }
    if fields.contains_key("result") || fields.contains_key("error") {
        return Ok(Message::Ignored);
    }
    Err("Unknown message type".to_string())
}

/// The `method` of the body, the way the transport gate read it: an unreadable body simply has no
/// method, which makes it a non-`initialize` one.
pub fn method_of(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()?
        .get("method")?
        .as_str()
        .map(ToString::to_string)
}

/// An answered request: `{jsonrpc, id, result}`.
pub fn success(id: Id, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id.into_value(), "result": result})
}

/// A failed one. Without an id to echo the SDK's own record could not be built, so the Java
/// transport fell back to the minimal envelope, which is what `id: None` produces here.
pub fn failure(id: Option<Id>, code: i64, message: &str) -> Value {
    let error = json!({"code": code, "message": message});
    match id {
        Some(id) => json!({"jsonrpc": "2.0", "id": id.into_value(), "error": error}),
        None => json!({"jsonrpc": "2.0", "error": error}),
    }
}

/// One tool of `tools/list`. `title`, `description` and `annotations` are the three keys the SDK
/// omitted when they were absent — `EXECUTE_SQL` deliberately has no annotations at all.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub input_schema: Value,
    pub annotations: Option<Value>,
}

impl ToolDefinition {
    pub fn to_value(&self) -> Value {
        let mut out = Map::new();
        out.insert("name".to_string(), json!(self.name));
        insert(
            &mut out,
            "title",
            self.title.clone().map(|value| json!(value)),
        );
        insert(
            &mut out,
            "description",
            self.description.clone().map(|value| json!(value)),
        );
        out.insert("inputSchema".to_string(), self.input_schema.clone());
        insert(&mut out, "annotations", self.annotations.clone());
        Value::Object(out)
    }
}

fn insert(out: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        out.insert(key.to_string(), value);
    }
}

/// The four annotation flags, minus the ones that were never set. `openWorldHint` only the prebuilt
/// write tools declare.
pub fn annotations(
    read_only: Option<bool>,
    destructive: Option<bool>,
    idempotent: Option<bool>,
    open_world: Option<bool>,
) -> Option<Value> {
    let mut out = Map::new();
    for (key, value) in [
        ("readOnlyHint", read_only),
        ("destructiveHint", destructive),
        ("idempotentHint", idempotent),
        ("openWorldHint", open_world),
    ] {
        if let Some(value) = value {
            out.insert(key.to_string(), json!(value));
        }
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

/// `ListToolsResult`.
pub fn list_tools(tools: Vec<ToolDefinition>) -> Value {
    json!({"tools": tools.iter().map(ToolDefinition::to_value).collect::<Vec<_>>()})
}

/// `CallToolResult` for a tool that answered: the payload as one text block, and again as the
/// structured content the SDK parsed out of that same text.
pub fn call_tool_result(payload: Value) -> Value {
    json!({
        "content": [text(payload.to_string())],
        "structuredContent": payload,
        "isError": false,
    })
}

/// `CallToolResult` for a `ToolExecuteException`: the message alone, with no structured content.
pub fn call_tool_error(message: &str) -> Value {
    json!({"content": [text(message.to_string())], "isError": true})
}

fn text(value: String) -> Value {
    json!({"type": "text", "text": value})
}

/// `InitializeResult`. A capability is advertised as `{listChanged: false}` — the endpoint is
/// stateless, so it never emits a notification about its own tool or prompt list.
pub fn initialize_result(has_tools: bool, has_prompts: bool, server_info: Value) -> Value {
    let mut capabilities = Map::new();
    if has_tools {
        capabilities.insert("tools".to_string(), json!({"listChanged": false}));
    }
    if has_prompts {
        capabilities.insert("prompts".to_string(), json!({"listChanged": false}));
    }
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": Value::Object(capabilities),
        "serverInfo": server_info,
    })
}

/// `Implementation`: the service name, the `v`-prefixed active version, and its description when it
/// has one.
pub fn server_info(name: Option<&str>, version: &str, description: Option<&str>) -> Value {
    let mut out = Map::new();
    out.insert("name".to_string(), json!(name));
    out.insert("version".to_string(), json!(version));
    insert(
        &mut out,
        "description",
        description.map(|value| json!(value)),
    );
    Value::Object(out)
}

/// One prompt of `prompts/list`, with its declared arguments.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PromptDefinition {
    pub name: Option<String>,
    pub description: Option<String>,
    pub arguments: Vec<PromptArgument>,
}

impl PromptDefinition {
    pub fn to_value(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "arguments": self
                .arguments
                .iter()
                .map(PromptArgument::to_value)
                .collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PromptArgument {
    pub name: Option<String>,
    pub description: String,
    pub required: bool,
}

impl PromptArgument {
    fn to_value(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "required": self.required,
        })
    }
}

/// `ListPromptsResult`.
pub fn list_prompts(prompts: Vec<PromptDefinition>) -> Value {
    json!({"prompts": prompts.iter().map(PromptDefinition::to_value).collect::<Vec<_>>()})
}

/// `GetPromptResult`: the rendered template as the one user message the Java converter built.
pub fn get_prompt_result(rendered: &str) -> Value {
    json!({"messages": [{"role": "user", "content": text(rendered.to_string())}]})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_envelope_with_an_id_is_the_full_response_and_without_it_is_the_minimal_one() {
        assert_eq!(
            success(Id::Number(7), json!({"ping": true})),
            json!({"jsonrpc": "2.0", "id": 7, "result": {"ping": true}})
        );
        assert_eq!(
            failure(
                Some(Id::Text("a".into())),
                METHOD_NOT_FOUND_CODE,
                "Method not found: x"
            ),
            json!({
                "jsonrpc": "2.0",
                "id": "a",
                "error": {"code": -32601, "message": "Method not found: x"},
            })
        );
        // The SDK record rejected a null id, so the fallback envelope has no id key at all.
        assert_eq!(
            failure(None, INTERNAL_ERROR_CODE, "Service is disabled"),
            json!({
                "jsonrpc": "2.0",
                "error": {"code": -32603, "message": "Service is disabled"},
            })
        );
    }

    #[test]
    fn an_id_is_a_number_only_when_it_was_one() {
        assert_eq!(Id::of(&json!({"id": 12})), Some(Id::Number(12)));
        assert_eq!(Id::of(&json!({"id": "12"})), Some(Id::Text("12".into())));
        assert_eq!(Id::of(&json!({"id": 1.5})), Some(Id::Text("1.5".into())));
        assert_eq!(Id::of(&json!({"id": null})), None);
        assert_eq!(Id::of(&json!({})), None);
    }

    #[test]
    fn only_a_method_with_an_id_is_a_request() {
        let message = parse(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).unwrap();
        assert_eq!(
            message,
            Message::Request(Request {
                id: Id::Number(1),
                method: "ping".to_string(),
                params: Value::Null,
            })
        );
        // A notification and a batch are accepted without an answer; a client response too.
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap(),
            Message::Ignored
        );
        assert_eq!(
            parse("[{\"method\":\"ping\",\"id\":1}]").unwrap(),
            Message::Ignored
        );
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#).unwrap(),
            Message::Ignored
        );
    }

    #[test]
    fn an_unreadable_or_unrecognised_body_is_a_parse_error() {
        assert!(parse("").is_err());
        assert!(parse("not json").is_err());
        assert_eq!(parse("{}").unwrap_err(), "Unknown message type");
        assert_eq!(parse(r#"{"result":1}"#), Ok(Message::Ignored));
    }

    #[test]
    fn the_gate_reads_the_method_without_needing_a_valid_envelope() {
        assert_eq!(
            method_of(r#"{"method":"initialize","id":1}"#).as_deref(),
            Some("initialize")
        );
        assert_eq!(method_of("{}"), None);
        assert_eq!(method_of("{{{"), None);
        assert_eq!(method_of(r#"{"method":42}"#), None);
    }

    #[test]
    fn params_are_mapped_the_way_jackson_mapped_the_request_record() {
        // An absent or null `params` is a request with nothing in it, not a failure.
        assert_eq!(
            CallParams::read(&Value::Null).unwrap(),
            CallParams::default()
        );
        let read = CallParams::read(&json!({ "name": "orders" })).unwrap();
        assert_eq!(read.name.as_deref(), Some("orders"));
        assert!(read.arguments.is_empty());

        // A scalar name is coerced to its text; only a structure cannot become one.
        assert_eq!(
            CallParams::read(&json!({"name": 123}))
                .unwrap()
                .name
                .as_deref(),
            Some("123")
        );
        assert_eq!(
            CallParams::read(&json!({"name": false}))
                .unwrap()
                .name
                .as_deref(),
            Some("false")
        );
        assert_eq!(CallParams::read(&json!({"name": {"a": 1}})), None);
        assert_eq!(CallParams::read(&json!("x")), None);
        assert_eq!(CallParams::read(&json!({"arguments": []})), None);
        assert_eq!(
            CallParams::read(&json!({"name": "x", "arguments": {"k": 1}}))
                .unwrap()
                .arguments
                .get("k"),
            Some(&json!(1))
        );
    }

    #[test]
    fn a_tool_definition_leaves_out_what_the_java_builder_never_set() {
        let read_only = ToolDefinition {
            name: "search_tables_and_terms".into(),
            title: Some("Search Tables and Terms".into()),
            description: Some("d".into()),
            input_schema: json!({"type": "object"}),
            annotations: annotations(Some(true), None, None, None),
        };
        assert_eq!(
            read_only.to_value(),
            json!({
                "name": "search_tables_and_terms",
                "title": "Search Tables and Terms",
                "description": "d",
                "inputSchema": {"type": "object"},
                "annotations": {"readOnlyHint": true},
            })
        );

        let execute = ToolDefinition {
            name: "execute_sql".into(),
            description: None,
            input_schema: json!({"type": "object"}),
            ..ToolDefinition::default()
        };
        let value = execute.to_value();
        assert!(
            value.get("title").is_none(),
            "an unset title is absent, not null"
        );
        assert!(value.get("description").is_none());
        assert!(
            value.get("annotations").is_none(),
            "EXECUTE_SQL has no annotations at all"
        );

        // A custom tool whose flags are all unset reaches the same verdict as EXECUTE_SQL.
        assert!(annotations(None, None, None, None).is_none());
    }

    #[test]
    fn a_tool_result_carries_the_payload_twice_the_way_the_sdk_did() {
        let payload = json!({"type": "TABLE_METADATA", "tables": []});
        let result = call_tool_result(payload.clone());
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"][0]["type"], "text");
        assert_eq!(result["content"][0]["text"], payload.to_string());
        assert_eq!(result["structuredContent"], payload);

        assert_eq!(
            call_tool_error("数据源不存在"),
            json!({"content": [{"type": "text", "text": "数据源不存在"}], "isError": true})
        );
    }

    #[test]
    fn capabilities_appear_only_for_a_snapshot_that_has_something_to_offer() {
        let empty = initialize_result(false, false, server_info(None, "v0", None));
        assert_eq!(empty["capabilities"], json!({}));
        assert_eq!(empty["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(empty["serverInfo"], json!({"name": null, "version": "v0"}));

        let full = initialize_result(true, true, server_info(Some("销售"), "v3", Some("d")));
        assert_eq!(
            full["capabilities"],
            json!({"tools": {"listChanged": false}, "prompts": {"listChanged": false}})
        );
        assert_eq!(full["serverInfo"]["description"], "d");
    }

    #[test]
    fn a_prompt_list_entry_always_names_its_arguments_and_their_bound() {
        let prompt = PromptDefinition {
            name: Some("analyze_table".into()),
            description: None,
            arguments: vec![PromptArgument {
                name: Some("table".into()),
                description: String::new(),
                required: true,
            }],
        };
        assert_eq!(
            list_prompts(vec![prompt]),
            json!({
                "prompts": [{
                    "name": "analyze_table",
                    "description": null,
                    "arguments": [{"name": "table", "description": "", "required": true}],
                }]
            })
        );

        assert_eq!(
            get_prompt_result("look at orders"),
            json!({
                "messages": [{
                    "role": "user",
                    "content": {"type": "text", "text": "look at orders"},
                }]
            })
        );
    }
}
