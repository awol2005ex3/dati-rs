//! The MCP endpoint: a published snapshot reshaped as protocol definitions, and one JSON-RPC
//! exchange with a client.
//!
//! Three Java classes meet here. `McpEndpointService` decides the HTTP status — the protocol
//! handler only ever produces a body — `McpProtocolHandler` dispatches one method, and the two
//! definition converters plus `SnapshotToolResolver` read the *active snapshot*, never the draft
//! tables, so an edit that was not published is invisible to a client.
//!
//! Like the Java endpoint this one is stateless: no session id is issued, every request is served
//! from the snapshot it arrives with, and the only background work is the usage buffer the request
//! feeds. Authentication is the route's `Authed` extractor, which is why the Java
//! `user != null` branches are not reproduced: an unauthenticated call never reaches this code, it
//! is answered 401 at the interceptor the same way Spring's was.

use std::collections::HashSet;
use std::time::Instant;

use serde_json::{Map, Value, json};

use crate::auth::AuthUser;
use crate::config::Config;
use crate::error::{ErrorCode, Result};
use crate::model::base::Audit;
use crate::model::mcp::{McpDataScope, McpService, McpServiceStatus, McpToolType};
use crate::model::mcp_args::input_schema;
use crate::model::mcp_rpc::{
    self as rpc, CallParams, Id, Message, PromptArgument, PromptDefinition, Request, ToolDefinition,
};
use crate::model::mcp_tool::{
    McpCustomTool, McpPrebuiltToolConfig, SqlPolicy, ToolConfig, ToolParameter,
};
use crate::model::mcp_tool_data::{CallError, ToolFailure};
use crate::model::mcp_usage::UsageRecord;
use crate::model::mcp_version::{SnapshotContent, SnapshotPrompt};
use crate::model::permission::{Permission, ResourceType};
use crate::repo::mcp_snapshot;
use crate::template;

use super::mcp_exec::{McpExecutors, ToolContext};
use super::mcp_service::McpServiceService;
use super::mcp_usage::UsageCollector;

/// `McpSchema.METHOD_*`, the six the handler answers.
const METHOD_INITIALIZE: &str = "initialize";
const METHOD_PING: &str = "ping";
const METHOD_TOOLS_LIST: &str = "tools/list";
const METHOD_TOOLS_CALL: &str = "tools/call";
const METHOD_PROMPTS_LIST: &str = "prompts/list";
const METHOD_PROMPTS_GET: &str = "prompts/get";

/// One `POST /{code}/mcp` as the transport sees it.
pub struct EndpointCall<'a> {
    pub code: &'a str,
    pub body: &'a str,
    pub origin: Option<&'a str>,
    pub protocol_version: Option<&'a str>,
    pub caller: &'a AuthUser,
    pub client_ip: Option<&'a str>,
}

/// The HTTP status and the body to write — the Java `McpEndpointResult`. A bare status is how the
/// transport rejects a request without claiming to know the JSON-RPC id it failed to read.
pub struct EndpointResponse {
    pub status: u16,
    pub body: Option<Value>,
}

impl EndpointResponse {
    const fn status(status: u16) -> Self {
        Self { status, body: None }
    }

    fn with(status: u16, body: Value) -> Self {
        Self {
            status,
            body: Some(body),
        }
    }
}

#[derive(Clone)]
pub struct McpEndpoint {
    services: McpServiceService,
    executors: McpExecutors,
    usage: UsageCollector,
    allowed_origins: HashSet<String>,
}

impl McpEndpoint {
    pub fn new(
        services: McpServiceService,
        executors: McpExecutors,
        usage: UsageCollector,
        config: &Config,
    ) -> Self {
        Self {
            services,
            executors,
            usage,
            allowed_origins: config.mcp_allowed_origins.iter().cloned().collect(),
        }
    }

    /// The decision table, in the Java order: service semantics, caller's right to see it, then the
    /// transport headers, then the snapshot the request is actually served from.
    pub async fn handle(&self, call: &EndpointCall<'_>) -> Result<EndpointResponse> {
        let service = match self.services.load_by_code(call.code).await {
            Ok(service) => service,
            // An unknown code and a DRAFT service are indistinguishable on purpose: the endpoint
            // advertises nothing about services that are not published.
            Err(err) if err.code() == ErrorCode::MsServiceNotFound => {
                return Ok(EndpointResponse::status(404));
            }
            Err(err) => return Err(err),
        };
        if service.status == Some(McpServiceStatus::Draft) {
            return Ok(EndpointResponse::status(404));
        }
        if service.status == Some(McpServiceStatus::Disabled) {
            return Ok(EndpointResponse::with(
                503,
                envelope(call.body, rpc::INTERNAL_ERROR_CODE, "Service is disabled"),
            ));
        }
        let visible = self
            .services
            .permissions()
            .can(
                self.services.db(),
                call.caller,
                ResourceType::McpService,
                &service.id,
                Permission::View,
            )
            .await?;
        if !visible {
            return Ok(EndpointResponse::with(
                403,
                envelope(call.body, rpc::INTERNAL_ERROR_CODE, "Permission denied"),
            ));
        }
        // DNS-rebinding protection: only loopback or whitelisted origins, and a request that sends
        // no `Origin` at all (every non-browser client) is not checked.
        if let Some(origin) = call.origin.filter(|origin| !origin.trim().is_empty())
            && !self.is_trusted_origin(origin)
        {
            return Ok(EndpointResponse::status(403));
        }
        if rpc::method_of(call.body).as_deref() != Some(METHOD_INITIALIZE)
            && call.protocol_version != Some(rpc::PROTOCOL_VERSION)
        {
            return Ok(EndpointResponse::status(400));
        }
        let Some(snapshot) = self.active_snapshot(&service).await? else {
            return Ok(EndpointResponse::status(500));
        };
        let request = match rpc::parse(call.body) {
            Ok(Message::Request(request)) => request,
            Ok(Message::Ignored) => return Ok(EndpointResponse::status(202)),
            Err(detail) => {
                return Ok(EndpointResponse::with(
                    400,
                    envelope(
                        call.body,
                        rpc::PARSE_ERROR,
                        &format!("Parse error: {detail}"),
                    ),
                ));
            }
        };
        let started = Instant::now();
        let response = self
            .dispatch(&service, &snapshot, call.caller, &request)
            .await;
        self.record_usage(&service.id, &request, &response, started, call);
        Ok(EndpointResponse::with(200, response))
    }

    /// The version the endpoint serves. A service that points at a snapshot row which is gone has
    /// nothing to answer with, which the transport reports as 500 rather than as an empty server.
    async fn active_snapshot(&self, service: &McpService) -> Result<Option<ActiveSnapshot>> {
        let Some(version_id) = service.active_version_id.as_deref() else {
            return Ok(None);
        };
        let Some(row) = mcp_snapshot::find_by_id(self.services.db(), version_id).await? else {
            return Ok(None);
        };
        Ok(Some(ActiveSnapshot::of(&service.id, &row.content)?))
    }

    /// `McpProtocolHandler.handle`: one method in, one envelope out. Nothing escapes — a tool's own
    /// failure becomes an errored result and any other error an internal one, so the transport
    /// always answers 200 once the request reached the protocol.
    async fn dispatch(
        &self,
        service: &McpService,
        snapshot: &ActiveSnapshot,
        caller: &AuthUser,
        request: &Request,
    ) -> Value {
        let result = match request.method.as_str() {
            METHOD_INITIALIZE => Ok(initialize(service, snapshot, caller)),
            METHOD_PING => Ok(json!({})),
            METHOD_TOOLS_LIST => Ok(rpc::list_tools(tool_definitions(snapshot, caller))),
            METHOD_TOOLS_CALL => self.tools_call(service, snapshot, caller, request).await,
            METHOD_PROMPTS_LIST => Ok(rpc::list_prompts(prompt_definitions(snapshot))),
            METHOD_PROMPTS_GET => prompts_get(snapshot, request),
            method => {
                return rpc::failure(
                    Some(request.id.clone()),
                    rpc::METHOD_NOT_FOUND_CODE,
                    &format!("Method not found: {method}"),
                );
            }
        };
        match result {
            Ok(value) => rpc::success(request.id.clone(), value),
            Err(DispatchError::Failure(failure)) => {
                rpc::success(request.id.clone(), rpc::call_tool_error(&failure.message))
            }
            Err(DispatchError::InvalidParams(detail)) => {
                rpc::failure(Some(request.id.clone()), rpc::INVALID_PARAMS_CODE, &detail)
            }
            Err(DispatchError::Internal(detail)) => rpc::failure(
                Some(request.id.clone()),
                rpc::INTERNAL_ERROR_CODE,
                &rpc::internal_error(&detail),
            ),
        }
    }

    async fn tools_call(
        &self,
        service: &McpService,
        snapshot: &ActiveSnapshot,
        caller: &AuthUser,
        request: &Request,
    ) -> std::result::Result<Value, DispatchError> {
        let Some(params) = CallParams::read(&request.params) else {
            return Err(DispatchError::Internal(
                rpc::INVALID_PARAMS_DETAIL.to_string(),
            ));
        };
        let Some(name) = params.name.filter(|name| !name.trim().is_empty()) else {
            return Err(DispatchError::InvalidParams(
                "Missing required parameter: name".to_string(),
            ));
        };
        // A disabled tool is indistinguishable from one that was never published.
        let Some(tool) = snapshot.resolve(&name) else {
            return Err(DispatchError::InvalidParams(format!(
                "Unknown tool: {name}"
            )));
        };
        let ctx = ToolContext {
            service_id: &service.id,
            tool_type: tool.tool_type,
            config: tool.config,
            arguments: params.arguments,
            scope_items: &snapshot.scopes,
            caller,
        };
        let data = self.executors.execute(&ctx).await?;
        let payload =
            serde_json::to_value(data).map_err(|err| DispatchError::Internal(err.to_string()))?;
        Ok(rpc::call_tool_result(payload))
    }

    /// Only the two execution methods are usage; a protocol handshake or a metadata query would
    /// pollute the counters the console reports.
    fn record_usage(
        &self,
        service_id: &str,
        request: &Request,
        envelope: &Value,
        started: Instant,
        call: &EndpointCall<'_>,
    ) {
        if request.method != METHOD_TOOLS_CALL && request.method != METHOD_PROMPTS_GET {
            return;
        }
        let (success, error_message) = usage_outcome(envelope);
        self.usage.record(UsageRecord {
            service_id: service_id.to_string(),
            method: request.method.clone(),
            tool_name: CallParams::read(&request.params)
                .and_then(|params| params.name)
                .unwrap_or_default(),
            caller_id: Some(call.caller.id.clone()),
            caller_name: Some(call.caller.name.clone()),
            client_ip: call.client_ip.map(str::to_string),
            success,
            duration_ms: started.elapsed().as_millis() as i64,
            error_message,
        });
        // The Java collector flushed itself on a virtual thread once the queue reached the batch
        // size; the scheduled flush remains the backstop for everything below it.
        if self.usage.over_batch() {
            let collector = self.usage.clone();
            tokio::spawn(async move {
                collector.flush().await;
            });
        }
    }

    /// Loopback is always trusted, because a browser-based MCP client runs locally.
    fn is_trusted_origin(&self, origin: &str) -> bool {
        let normalized = origin.strip_suffix('/').unwrap_or(origin);
        if self.allowed_origins.contains(normalized) {
            return true;
        }
        matches!(
            host_of(origin).as_deref(),
            Some("localhost") | Some("127.0.0.1") | Some("[::1]") | Some("::1")
        )
    }
}

/// What one dispatch decision produced, beyond a result: the three ways the Java handler answered.
enum DispatchError {
    /// A `ToolExecuteException`: the agent's problem, inside a successful response.
    Failure(ToolFailure),
    /// A bad method argument: `-32602`.
    InvalidParams(String),
    /// Anything else the gateway caught: `-32603`.
    Internal(String),
}

impl From<CallError> for DispatchError {
    fn from(err: CallError) -> Self {
        match err {
            CallError::Failure(failure) => Self::Failure(failure),
            CallError::Internal(err) => Self::Internal(err.message().to_string()),
        }
    }
}

/// The active snapshot with every stored configuration already read, which is the work the Java
/// snapshot mapper did between the DAO and the handler. The scope items are frozen here too: they
/// come from the snapshot, never from the live draft.
struct ActiveSnapshot {
    prebuilt: Vec<McpPrebuiltToolConfig>,
    custom: Vec<McpCustomTool>,
    prompts: Vec<SnapshotPrompt>,
    scopes: Vec<McpDataScope>,
}

impl ActiveSnapshot {
    fn of(service_id: &str, content: &SnapshotContent) -> Result<Self> {
        let mut prebuilt = Vec::with_capacity(content.prebuilt_tools.len());
        for slot in &content.prebuilt_tools {
            prebuilt.push(slot.to_config()?);
        }
        let mut custom = Vec::with_capacity(content.custom_tools.len());
        for tool in &content.custom_tools {
            custom.push(tool.to_tool()?);
        }
        Ok(Self {
            prebuilt,
            custom,
            prompts: content.prompts.clone(),
            scopes: content
                .data_scopes
                .iter()
                .map(|scope| McpDataScope {
                    id: String::new(),
                    service_id: service_id.to_string(),
                    scope_type: scope.scope_type,
                    reference_id: scope.reference_id.clone(),
                    audit: Audit::default(),
                })
                .collect(),
        })
    }

    /// `SnapshotToolResolver.resolve`: the enabled prebuilt slots by their fixed MCP name, then the
    /// enabled custom tools by the name their author gave them.
    fn resolve(&self, name: &str) -> Option<ResolvedTool<'_>> {
        if let Some(slot) = self
            .prebuilt
            .iter()
            .filter(|slot| slot.enabled)
            .find(|slot| slot.tool_type.tool_name() == Some(name))
        {
            return Some(ResolvedTool {
                tool_type: slot.tool_type,
                config: &slot.config,
            });
        }
        let tool = self
            .custom
            .iter()
            .filter(|tool| tool.enabled)
            .find(|tool| tool.name.as_deref() == Some(name))?;
        // `ActiveSnapshot::of` parses every stored config, so a custom tool it carries always has
        // one; a tool without a config has nothing an executor could run.
        let config = tool.config.as_ref()?;
        Some(ResolvedTool {
            tool_type: tool.tool_type,
            config,
        })
    }
}

struct ResolvedTool<'a> {
    tool_type: McpToolType,
    config: &'a ToolConfig,
}

/// `ToolDefinitionConverter.convert`: the prebuilt slots in their fixed order, then the custom
/// tools by name, with a name that collides with an earlier entry skipped rather than shadowing it.
fn tool_definitions(snapshot: &ActiveSnapshot, caller: &AuthUser) -> Vec<ToolDefinition> {
    let mut definitions = Vec::new();
    let mut used: HashSet<&str> = HashSet::new();
    for tool_type in McpToolType::ALL {
        // `PARAMETERIZED_SQL` has no MCP name of its own and is never a prebuilt slot.
        let Some(name) = tool_type.tool_name() else {
            continue;
        };
        let Some(slot) = snapshot
            .prebuilt
            .iter()
            .find(|slot| slot.tool_type == tool_type && slot.enabled)
        else {
            continue;
        };
        definitions.push(ToolDefinition {
            name: name.to_string(),
            title: tool_type.title().map(str::to_string),
            description: prebuilt_description(tool_type, &slot.config, caller),
            input_schema: input_schema(tool_type),
            annotations: tool_type
                .annotations_json()
                .and_then(|text| serde_json::from_str(text).ok()),
        });
        used.insert(name);
    }

    let mut customs: Vec<&McpCustomTool> =
        snapshot.custom.iter().filter(|tool| tool.enabled).collect();
    customs.sort_by_key(|tool| tool.name.as_deref().unwrap_or_default());
    for tool in customs {
        let name = tool.name.as_deref().unwrap_or_default();
        if !used.insert(name) {
            continue;
        }
        definitions.push(ToolDefinition {
            name: name.to_string(),
            title: tool.title.clone().filter(|title| !title.trim().is_empty()),
            description: tool.description.clone(),
            input_schema: custom_input_schema(tool.tool_type, tool.config.as_ref()),
            annotations: custom_annotations(tool.tool_type, tool.config.as_ref()),
        });
    }
    definitions
}

/// `EXECUTE_SQL` is the one prebuilt description that is written per service, because what the tool
/// may do depends on the policy and limits of the slot.
fn prebuilt_description(
    tool_type: McpToolType,
    config: &ToolConfig,
    caller: &AuthUser,
) -> Option<String> {
    if let (McpToolType::ExecuteSql, ToolConfig::ExecuteSql(config)) = (tool_type, config) {
        return Some(execute_sql_description(
            &config.sql_policy,
            config.max_rows,
            caller,
        ));
    }
    tool_type.description().map(str::to_string)
}

fn execute_sql_description(policy: &SqlPolicy, max_rows: i32, caller: &AuthUser) -> String {
    let base = McpToolType::ExecuteSql.description().unwrap_or_default();
    // One trailing period is replaced by the sentence the rest of this builds; the fixed text ends
    // with one, so the join never doubles it.
    let mut out = String::from(base.strip_suffix('.').unwrap_or(base));

    let mut ops: Vec<&str> = Vec::new();
    for (allowed, name) in [
        (policy.allow_select, "SELECT"),
        (policy.allow_insert, "INSERT"),
        (policy.allow_update, "UPDATE"),
        (policy.allow_delete, "DELETE"),
        (policy.allow_ddl, "DDL"),
        (policy.allow_metadata, "METADATA"),
        (policy.allow_transaction, "TRANSACTION"),
        (policy.allow_set, "SET"),
    ] {
        if allowed {
            ops.push(name);
        }
    }
    out.push_str(". Allowed operations: ");
    match ops.len() {
        8 => out.push_str("ALL"),
        0 => out.push_str("NONE"),
        _ => out.push_str(&ops.join(", ")),
    }
    if policy.allow_multi {
        out.push_str(" (multi-statement supported)");
    }
    if max_rows > 0 {
        out.push_str(&format!(". Max rows: {max_rows}"));
    }
    out.push_str(&format!(
        ". Context: current user name: {}, user id: {}",
        caller.name, caller.id
    ));
    out
}

fn custom_input_schema(tool_type: McpToolType, config: Option<&ToolConfig>) -> Value {
    if let Some(ToolConfig::ParameterizedSql(config)) = config {
        return param_sql_schema(&config.parameters);
    }
    if tool_type.is_prebuilt() {
        return input_schema(tool_type);
    }
    json!({"type": "object", "additionalProperties": false})
}

fn custom_annotations(tool_type: McpToolType, config: Option<&ToolConfig>) -> Option<Value> {
    let ToolConfig::ParameterizedSql(config) = config? else {
        return None;
    };
    if tool_type != McpToolType::ParameterizedSql {
        return None;
    }
    rpc::annotations(
        config.read_only,
        config.destructive,
        config.idempotent,
        None,
    )
}

/// The schema of a parameterized tool, from the parameters its author declared. Unlike the
/// prebuilt schemas this one leaves `additionalProperties` unset.
fn param_sql_schema(parameters: &[ToolParameter]) -> Value {
    let mut properties = Map::new();
    let mut required: Vec<Value> = Vec::new();
    for parameter in parameters {
        // A parameter that was never named cannot be passed, so it is left out rather than
        // published under an empty key.
        let Some(name) = parameter.name.as_deref() else {
            continue;
        };
        let mut property = Map::new();
        property.insert(
            "type".to_string(),
            json!(json_schema_type(&parameter.param_type)),
        );
        if let Some(description) = &parameter.description {
            property.insert("description".to_string(), json!(description));
        }
        if let Some(default) = &parameter.default_value {
            property.insert("default".to_string(), json!(default));
        }
        properties.insert(name.to_string(), Value::Object(property));
        if parameter.required {
            required.push(json!(name));
        }
    }
    let mut schema = Map::new();
    schema.insert("type".to_string(), json!("object"));
    schema.insert("properties".to_string(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".to_string(), Value::Array(required));
    }
    Value::Object(schema)
}

fn json_schema_type(param_type: &Option<String>) -> &'static str {
    match param_type.as_deref() {
        Some("Number") => "number",
        Some("Boolean") => "boolean",
        Some("Array") => "array",
        // `String`, `DateTime`, and anything the author typed in.
        _ => "string",
    }
}

/// `PromptDefinitionConverter.list`.
fn prompt_definitions(snapshot: &ActiveSnapshot) -> Vec<PromptDefinition> {
    snapshot
        .prompts
        .iter()
        .filter(|prompt| prompt.enabled)
        .map(|prompt| PromptDefinition {
            name: prompt.name.clone(),
            description: Some(prompt.description.clone().unwrap_or_default()),
            arguments: prompt
                .parameters
                .iter()
                .map(|parameter| PromptArgument {
                    name: parameter.name.clone(),
                    description: parameter.description.clone().unwrap_or_default(),
                    required: parameter.required,
                })
                .collect(),
        })
        .collect()
}

fn has_enabled_prompts(snapshot: &ActiveSnapshot) -> bool {
    snapshot.prompts.iter().any(|prompt| prompt.enabled)
}

fn initialize(service: &McpService, snapshot: &ActiveSnapshot, caller: &AuthUser) -> Value {
    let version = format!("v{}", service.active_version_number.unwrap_or(0));
    rpc::initialize_result(
        !tool_definitions(snapshot, caller).is_empty(),
        has_enabled_prompts(snapshot),
        rpc::server_info(
            service.name.as_deref(),
            &version,
            service
                .description
                .as_deref()
                .filter(|text| !text.trim().is_empty()),
        ),
    )
}

/// `PromptDefinitionConverter.get`, with the two ways it refused (`-32602`) kept apart from the
/// template engine failing (`-32603`, because the Java handler only caught the former locally).
fn prompts_get(
    snapshot: &ActiveSnapshot,
    request: &Request,
) -> std::result::Result<Value, DispatchError> {
    let Some(params) = CallParams::read(&request.params) else {
        return Err(DispatchError::Internal(
            rpc::INVALID_PARAMS_DETAIL.to_string(),
        ));
    };
    let Some(name) = params.name.filter(|name| !name.trim().is_empty()) else {
        return Err(DispatchError::InvalidParams(
            "Missing required parameter: name".to_string(),
        ));
    };
    match render_prompt(&snapshot.prompts, &name, &params.arguments) {
        Ok(rendered) => Ok(rpc::get_prompt_result(&rendered)),
        Err(refusal) => Err(match refusal {
            PromptRefusal::Unknown(name) => {
                DispatchError::InvalidParams(format!("Unknown prompt: {name}"))
            }
            PromptRefusal::MissingArgument(name) => {
                DispatchError::InvalidParams(format!("Missing required argument: {name}"))
            }
            PromptRefusal::Render(detail) => DispatchError::Internal(detail),
        }),
    }
}

#[derive(Debug)]
enum PromptRefusal {
    Unknown(String),
    MissingArgument(String),
    Render(String),
}

fn render_prompt(
    prompts: &[SnapshotPrompt],
    name: &str,
    arguments: &Map<String, Value>,
) -> std::result::Result<String, PromptRefusal> {
    let Some(prompt) = prompts
        .iter()
        .find(|prompt| prompt.enabled && prompt.name.as_deref() == Some(name))
    else {
        return Err(PromptRefusal::Unknown(name.to_string()));
    };
    for parameter in &prompt.parameters {
        let name = parameter.name.clone().unwrap_or_default();
        // `containsKey`, not a non-null check: an argument the client sent as `null` was sent.
        if parameter.required && !arguments.contains_key(&name) {
            return Err(PromptRefusal::MissingArgument(name));
        }
    }
    let Some(content) = prompt.content.as_deref() else {
        return Err(PromptRefusal::Render("Prompt content is empty".to_string()));
    };
    let compiled = template::parse(content).map_err(PromptRefusal::Render)?;
    Ok(template::render_text(
        &compiled,
        &template::values_of(arguments),
    ))
}

/// The JSON-RPC error envelope the transport writes before the protocol ever runs, echoing the
/// request id when the body still has one.
fn envelope(body: &str, code: i64, message: &str) -> Value {
    let id = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|root| Id::of(&root));
    rpc::failure(id, code, message)
}

/// Whether the response counts as a successful call, and the text to record when it does not: the
/// JSON-RPC error, or a tool result that reported its own failure.
fn usage_outcome(envelope: &Value) -> (bool, Option<String>) {
    if let Some(error) = envelope.get("error") {
        return (
            false,
            Some(
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ),
        );
    }
    let Some(result) = envelope.get("result").filter(|value| value.is_object()) else {
        return (true, None);
    };
    if result.get("isError") != Some(&json!(true)) {
        return (true, None);
    }
    let message = match result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|blocks| blocks.first())
    {
        Some(block) if block.get("type").and_then(Value::as_str) == Some("text") => block
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        Some(other) => other.to_string(),
        None => "Tool error".to_string(),
    };
    (false, Some(message))
}

/// The authority host of a full origin, which is what `java.net.URI.getHost` answered — and which
/// it answered as `null` for anything that is not a hierarchical URI, so a bare `localhost` is not
/// trusted either.
fn host_of(origin: &str) -> Option<String> {
    let (_, rest) = origin.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if let Some((host, _)) = authority.split_once(']') {
        return Some(format!("{host}]"));
    }
    let host = authority
        .split_once(':')
        .map_or(authority, |(host, _)| host);
    (!host.is_empty()).then(|| host.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::mcp_prompt::PromptParameter;
    use crate::model::mcp_version::{ServiceInfo, SnapshotCustom, SnapshotPrebuilt};

    fn caller() -> AuthUser {
        AuthUser {
            id: "u-123".to_string(),
            name: "alice".to_string(),
            display_name: Some("Alice".to_string()),
        }
    }

    fn prebuilt(tool_type: McpToolType, enabled: bool, config: Value) -> SnapshotPrebuilt {
        SnapshotPrebuilt {
            service_id: "svc".to_string(),
            tool_type,
            enabled,
            config,
        }
    }

    fn custom(name: &str, enabled: bool, config: Value) -> SnapshotCustom {
        SnapshotCustom {
            service_id: "svc".to_string(),
            name: Some(name.to_string()),
            tool_type: McpToolType::ParameterizedSql,
            title: None,
            description: Some("d".to_string()),
            enabled,
            config,
        }
    }

    fn snapshot(
        prebuilt: Vec<SnapshotPrebuilt>,
        custom_tools: Vec<SnapshotCustom>,
        prompts: Vec<SnapshotPrompt>,
    ) -> ActiveSnapshot {
        ActiveSnapshot::of(
            "svc",
            &SnapshotContent {
                service_info: ServiceInfo::default(),
                data_scopes: Vec::new(),
                prebuilt_tools: prebuilt,
                custom_tools,
                prompts,
            },
        )
        .unwrap()
    }

    #[test]
    fn only_enabled_slots_are_published_and_in_the_fixed_order() {
        let all = snapshot(
            vec![
                prebuilt(McpToolType::ExecuteSql, true, json!({})),
                prebuilt(McpToolType::GetTableInfo, true, json!({})),
                prebuilt(McpToolType::ListTables, true, json!({})),
                prebuilt(McpToolType::SearchMetadata, true, json!({})),
                prebuilt(McpToolType::UpsertTerm, false, json!({})),
            ],
            vec![
                custom("zz_last", true, json!({})),
                custom("aa_first", true, json!({})),
            ],
            Vec::new(),
        );
        let definitions = tool_definitions(&all, &caller());
        assert_eq!(
            definitions
                .iter()
                .map(|definition| definition.name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "search_tables_and_terms",
                "get_table_schema",
                "list_tables_and_terms",
                "execute_sql",
                "aa_first",
                "zz_last",
            ]
        );
        assert!(
            definitions
                .iter()
                .all(|definition| definition.name != "upsert_business_term"),
            "a disabled tool is not listed at all"
        );
    }

    #[test]
    fn a_custom_tool_that_reuses_a_name_is_skipped_rather_than_shadowing() {
        let all = snapshot(
            vec![prebuilt(McpToolType::SearchMetadata, true, json!({}))],
            vec![
                custom("search_tables_and_terms", true, json!({})),
                custom("dup", true, json!({})),
                custom("dup", true, json!({})),
            ],
            Vec::new(),
        );
        assert_eq!(
            tool_definitions(&all, &caller())
                .iter()
                .map(|definition| definition.name.as_str())
                .collect::<Vec<_>>(),
            vec!["search_tables_and_terms", "dup"]
        );
    }

    #[test]
    fn the_execute_sql_description_names_what_the_policy_allows_and_who_is_asking() {
        let with_default_policy = snapshot(
            vec![prebuilt(McpToolType::ExecuteSql, true, json!({}))],
            Vec::new(),
            Vec::new(),
        );
        let definitions = tool_definitions(&with_default_policy, &caller());
        assert_eq!(
            definitions[0].description.as_deref().unwrap(),
            "Execute an SQL query or statement against a data source and return result rows. \
             Use this tool only when no specialized or custom analysis tool is available for your \
             task. Allowed operations: SELECT. Max rows: 1000. Context: current user name: alice, \
             user id: u-123"
        );

        let writes = snapshot(
            vec![prebuilt(
                McpToolType::ExecuteSql,
                true,
                json!({
                    "max_rows": 500,
                    "sql_policy": {
                        "allow_select": true, "allow_insert": true, "allow_update": true,
                        "allow_multi": true,
                    },
                }),
            )],
            Vec::new(),
            Vec::new(),
        );
        let description = tool_definitions(&writes, &caller())[0]
            .description
            .clone()
            .unwrap();
        assert!(
            description
                .ends_with("Allowed operations: SELECT, INSERT, UPDATE (multi-statement supported). Max rows: 500. Context: current user name: alice, user id: u-123"),
            "{description}"
        );

        let everything = snapshot(
            vec![prebuilt(
                McpToolType::ExecuteSql,
                true,
                json!({"sql_policy": {
                    "allow_select": true, "allow_insert": true, "allow_update": true,
                    "allow_delete": true, "allow_ddl": true, "allow_metadata": true,
                    "allow_transaction": true, "allow_set": true,
                }}),
            )],
            Vec::new(),
            Vec::new(),
        );
        let description = tool_definitions(&everything, &caller())[0]
            .description
            .clone()
            .unwrap();
        assert!(
            description.contains(". Allowed operations: ALL. Max rows: 1000."),
            "{description}"
        );

        let nothing = snapshot(
            vec![prebuilt(
                McpToolType::ExecuteSql,
                true,
                json!({"max_rows": 0, "sql_policy": {
                    "allow_select": false,
                }}),
            )],
            Vec::new(),
            Vec::new(),
        );
        let description = tool_definitions(&nothing, &caller())[0]
            .description
            .clone()
            .unwrap();
        assert!(
            description.contains(". Allowed operations: NONE."),
            "{description}"
        );
        assert!(
            !description.contains("Max rows"),
            "a limit of zero is not advertised"
        );
    }

    #[test]
    fn a_custom_tool_publishes_the_schema_of_its_declared_parameters() {
        let all = snapshot(
            Vec::new(),
            vec![custom(
                "list_tasks",
                true,
                json!({
                    "sql_template": "SELECT * FROM tasks WHERE status = {{status}}",
                    "parameters": [
                        {"name": "status", "type": "String", "required": true, "description": "task status"},
                        {"name": "limit", "type": "Number", "default_value": "10"},
                        {"name": "flag", "type": "Boolean"},
                        {"name": "list", "type": "Array"},
                        {"name": "since", "type": "DateTime"},
                    ],
                    "read_only": true, "destructive": false,
                }),
            )],
            Vec::new(),
        );
        let definitions = tool_definitions(&all, &caller());
        let tool = definitions[0].to_value();
        assert_eq!(
            tool["title"],
            Value::Null,
            "an unset title stays out of the object"
        );
        assert_eq!(
            tool["inputSchema"],
            json!({
                "type": "object",
                "properties": {
                    "status": {"type": "string", "description": "task status"},
                    "limit": {"type": "number", "default": "10"},
                    "flag": {"type": "boolean"},
                    "list": {"type": "array"},
                    "since": {"type": "string"},
                },
                "required": ["status"],
            }),
            "the parameter schema declares no additionalProperties, unlike a prebuilt one"
        );
        assert_eq!(
            tool["annotations"],
            json!({"readOnlyHint": true, "destructiveHint": false})
        );

        // A tool with no flags set at all is published without an annotations key.
        let plain = snapshot(Vec::new(), vec![custom("raw", true, json!({}))], Vec::new());
        let value = tool_definitions(&plain, &caller())[0].to_value();
        assert!(value.get("annotations").is_none(), "{value}");
        assert_eq!(value["inputSchema"]["properties"], json!({}));
    }

    #[test]
    fn the_resolver_and_the_definitions_agree_that_disabled_means_unknown() {
        let all = snapshot(
            vec![prebuilt(McpToolType::ListTables, false, json!({}))],
            vec![custom("hidden", false, json!({}))],
            Vec::new(),
        );
        assert!(tool_definitions(&all, &caller()).is_empty());
        assert!(all.resolve("list_tables_and_terms").is_none());
        assert!(all.resolve("hidden").is_none());
        assert!(all.resolve("").is_none());
    }

    #[test]
    fn a_prompt_is_rendered_with_the_text_template_engine_and_its_arguments() {
        let prompt = SnapshotPrompt {
            service_id: "svc".to_string(),
            name: Some("analyze_table".to_string()),
            description: None,
            enabled: true,
            content: Some("请分析 {{table}} 表的数据。".to_string()),
            parameters: vec![PromptParameter {
                name: Some("table".to_string()),
                description: Some("表名".to_string()),
                required: true,
            }],
        };
        let all = snapshot(Vec::new(), Vec::new(), vec![prompt.clone()]);
        assert_eq!(
            render_prompt(
                &all.prompts,
                "analyze_table",
                &json!({"table": "orders"}).as_object().unwrap().clone()
            )
            .unwrap(),
            "请分析 orders 表的数据。"
        );

        // The list keeps the prompt visible with an empty rather than a missing description.
        assert_eq!(
            rpc::list_prompts(prompt_definitions(&all))["prompts"][0],
            json!({
                "name": "analyze_table",
                "description": "",
                "arguments": [{"name": "table", "description": "表名", "required": true}],
            })
        );

        // A disabled prompt is unknown to `prompts/get` as well as absent from `prompts/list`.
        let mut off = prompt.clone();
        off.enabled = false;
        let disabled = snapshot(Vec::new(), Vec::new(), vec![off]);
        assert!(prompt_definitions(&disabled).is_empty());
        let error = render_prompt(&disabled.prompts, "analyze_table", &Map::new()).unwrap_err();
        assert!(matches!(error, PromptRefusal::Unknown(name) if name == "analyze_table"));

        let missing = render_prompt(&all.prompts, "analyze_table", &Map::new()).unwrap_err();
        assert!(matches!(missing, PromptRefusal::MissingArgument(name) if name == "table"));

        // An argument the client sent as null was still sent, so it satisfies the declaration.
        render_prompt(
            &all.prompts,
            "analyze_table",
            &json!({"table": null}).as_object().unwrap().clone(),
        )
        .unwrap();

        let broken = snapshot(
            Vec::new(),
            Vec::new(),
            vec![SnapshotPrompt {
                name: Some("broken".to_string()),
                enabled: true,
                content: Some("{{#if}}".to_string()),
                ..prompt
            }],
        );
        // The required argument is supplied here: the refusals are ordered, and a prompt the client
        // did not fill correctly never reaches the template engine.
        assert!(matches!(
            render_prompt(
                &broken.prompts,
                "broken",
                &json!({"table": "orders"}).as_object().unwrap().clone()
            ),
            Err(PromptRefusal::Render(_))
        ));
    }

    #[test]
    fn initialize_advertises_only_what_the_snapshot_has() {
        let mut service = McpService {
            id: "svc".to_string(),
            name: Some("Test MCP Service".to_string()),
            description: Some("Test MCP service for unit tests".to_string()),
            deleted: false,
            code: "test".to_string(),
            status: Some(McpServiceStatus::Published),
            active_version_id: Some("snap".to_string()),
            active_version_number: Some(1),
            audit: Audit::default(),
        };
        let all = snapshot(
            vec![prebuilt(McpToolType::SearchMetadata, true, json!({}))],
            Vec::new(),
            Vec::new(),
        );
        let value = initialize(&service, &all, &caller());
        assert_eq!(value["protocolVersion"], rpc::PROTOCOL_VERSION);
        assert_eq!(
            value["capabilities"],
            json!({"tools": {"listChanged": false}})
        );
        assert_eq!(value["serverInfo"]["name"], "Test MCP Service");
        assert_eq!(value["serverInfo"]["version"], "v1");
        assert_eq!(
            value["serverInfo"]["description"],
            "Test MCP service for unit tests"
        );

        service.active_version_number = None;
        service.description = Some("  ".to_string());
        let empty = snapshot(Vec::new(), Vec::new(), Vec::new());
        let value = initialize(&service, &empty, &caller());
        assert_eq!(value["capabilities"], json!({}));
        assert_eq!(value["serverInfo"]["version"], "v0");
        assert!(value["serverInfo"].get("description").is_none());
    }

    #[test]
    fn the_usage_recorder_reads_its_verdict_off_the_response() {
        let ok = rpc::success(Id::Number(1), rpc::call_tool_result(json!({"type": "X"})));
        assert_eq!(usage_outcome(&ok), (true, None));

        let denied = rpc::success(
            Id::Number(1),
            rpc::call_tool_error("数据源或表不在服务范围内：No data scope configured"),
        );
        assert_eq!(
            usage_outcome(&denied),
            (
                false,
                Some("数据源或表不在服务范围内：No data scope configured".to_string())
            )
        );

        let aborted = rpc::failure(
            Some(Id::Number(1)),
            rpc::METHOD_NOT_FOUND_CODE,
            "Method not found: x",
        );
        assert_eq!(
            usage_outcome(&aborted),
            (false, Some("Method not found: x".to_string()))
        );

        let protocol = rpc::success(Id::Number(1), json!({}));
        assert_eq!(usage_outcome(&protocol), (true, None));

        // An errored result with no text at all still has a message to store.
        let empty_content =
            json!({"jsonrpc": "2.0", "id": 1, "result": {"isError": true, "content": []}});
        assert_eq!(
            usage_outcome(&empty_content),
            (false, Some("Tool error".to_string()))
        );
    }

    #[test]
    fn a_transport_error_echoes_an_id_only_when_the_body_has_one() {
        assert_eq!(
            envelope(
                r#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#,
                rpc::INTERNAL_ERROR_CODE,
                "Service is disabled"
            ),
            json!({"jsonrpc": "2.0", "id": 7, "error": {"code": -32603, "message": "Service is disabled"}})
        );
        assert_eq!(
            envelope("not json", rpc::PARSE_ERROR, "Parse error: x"),
            json!({"jsonrpc": "2.0", "error": {"code": -32700, "message": "Parse error: x"}})
        );
    }

    #[test]
    fn only_a_full_loopback_origin_is_trusted_by_default() {
        assert_eq!(
            host_of("http://localhost:8080").as_deref(),
            Some("localhost")
        );
        assert_eq!(host_of("http://127.0.0.1").as_deref(), Some("127.0.0.1"));
        assert_eq!(host_of("http://[::1]:8080/mcp").as_deref(), Some("[::1]"));
        assert_eq!(
            host_of("https://evil.example.com").as_deref(),
            Some("evil.example.com")
        );
        assert_eq!(host_of("localhost"), None, "a bare host is not an origin");
        assert_eq!(host_of("http://"), None);
    }
}
