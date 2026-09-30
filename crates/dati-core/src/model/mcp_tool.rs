//! The tool slots of an MCP service: the seven prebuilt configurations, the parameterized-SQL
//! tools an author writes, and the policy that decides which statements `execute_sql` may run.

use serde::{Deserialize, Serialize};

use crate::error::{DatiError, Result};
use crate::sql_analysis::{SqlAnalysisResult, SqlOperationType};

use super::mcp::McpToolType;

const DEFAULT_TIMEOUT: i32 = 30;
const DEFAULT_MAX_ROWS: i32 = 1000;

/// The statement kinds `EXECUTE_SQL` accepts. A field the client omits keeps the Java initializer,
/// so a policy that only names `allow_insert` still reads `SELECT`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SqlPolicy {
    pub allow_select: bool,
    pub allow_insert: bool,
    pub allow_update: bool,
    pub allow_delete: bool,
    pub allow_ddl: bool,
    pub allow_multi: bool,
    pub allow_metadata: bool,
    pub allow_transaction: bool,
    pub allow_set: bool,
}

impl Default for SqlPolicy {
    fn default() -> Self {
        Self {
            allow_select: true,
            allow_insert: false,
            allow_update: false,
            allow_delete: false,
            allow_ddl: false,
            allow_multi: false,
            allow_metadata: false,
            allow_transaction: false,
            allow_set: false,
        }
    }
}

impl SqlPolicy {
    /// The reason the SQL is refused — the detail text the Java exception carried — or `None` when
    /// every statement of the result is allowed.
    pub fn violation(&self, result: &SqlAnalysisResult) -> Option<String> {
        if result.is_multi() && !self.allow_multi {
            return Some("multi-statement not allowed".to_string());
        }
        let denied = |kind| !self.allows(kind);
        let refused = match result.statement_types.iter().find(|kind| denied(**kind)) {
            // A `MULTI` never appears in the per-statement list, so this arm only keeps the match
            // total; the marker above is what rejects a multi-statement call.
            Some(SqlOperationType::Multi) => "multi-statement not allowed".to_string(),
            Some(kind) => format!("{} not allowed", kind.as_str()),
            None => return None,
        };
        Some(refused)
    }

    fn allows(&self, kind: SqlOperationType) -> bool {
        match kind {
            SqlOperationType::Select => self.allow_select,
            SqlOperationType::Insert => self.allow_insert,
            SqlOperationType::Update => self.allow_update,
            SqlOperationType::Delete => self.allow_delete,
            // A merge writes both ways, so it needs both grants.
            SqlOperationType::Merge => self.allow_insert && self.allow_update,
            SqlOperationType::Ddl => self.allow_ddl,
            SqlOperationType::Metadata => self.allow_metadata,
            SqlOperationType::Transaction => self.allow_transaction,
            SqlOperationType::Set => self.allow_set,
            SqlOperationType::Multi => self.allow_multi,
            // Nothing was recognised, so nothing can be named: the database gets the verdict.
            SqlOperationType::Other => true,
        }
    }
}

/// One declared argument of a custom tool. `type` is free text on the wire; the MCP schema
/// generator maps the five documented names onto JSON types.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolParameter {
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub param_type: Option<String>,
    pub required: bool,
    pub default_value: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchMetadataConfig {
    pub timeout: i32,
}

impl Default for SearchMetadataConfig {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GetTableInfoConfig {
    pub timeout: i32,
}

impl Default for GetTableInfoConfig {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// `LIST_TABLES` always lists the whole service scope, so it has nothing to configure.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListTablesConfig {}

/// The three metadata-writing tools carry no per-service settings either.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateMetadataConfig {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecuteSqlConfig {
    pub sql_policy: SqlPolicy,
    pub timeout: i32,
    pub max_rows: i32,
}

impl Default for ExecuteSqlConfig {
    fn default() -> Self {
        Self {
            sql_policy: SqlPolicy::default(),
            timeout: DEFAULT_TIMEOUT,
            max_rows: DEFAULT_MAX_ROWS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ParamSqlConfig {
    pub data_source_id: Option<String>,
    pub sql_template: Option<String>,
    pub parameters: Vec<ToolParameter>,
    pub timeout: i32,
    pub max_rows: i32,
    pub read_only: Option<bool>,
    pub idempotent: Option<bool>,
    pub destructive: Option<bool>,
}

impl Default for ParamSqlConfig {
    fn default() -> Self {
        Self {
            data_source_id: None,
            sql_template: None,
            parameters: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
            max_rows: DEFAULT_MAX_ROWS,
            read_only: None,
            idempotent: None,
            destructive: None,
        }
    }
}

/// The config of one tool slot. Which shape applies is decided by the tool type, exactly like the
/// Java mappers picked the concrete class from the row's `tool_type` column.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ToolConfig {
    SearchMetadata(SearchMetadataConfig),
    GetTableInfo(GetTableInfoConfig),
    ListTables(ListTablesConfig),
    ExecuteSql(ExecuteSqlConfig),
    UpdateMetadata(UpdateMetadataConfig),
    ParameterizedSql(ParamSqlConfig),
}

impl ToolConfig {
    /// The config a slot reports when nothing was stored for it.
    pub fn default_for(tool_type: McpToolType) -> Self {
        match tool_type {
            McpToolType::SearchMetadata => Self::SearchMetadata(SearchMetadataConfig::default()),
            McpToolType::GetTableInfo => Self::GetTableInfo(GetTableInfoConfig::default()),
            McpToolType::ListTables => Self::ListTables(ListTablesConfig::default()),
            McpToolType::ExecuteSql => Self::ExecuteSql(ExecuteSqlConfig::default()),
            McpToolType::UpdateTableInfo
            | McpToolType::UpdateColumnInfo
            | McpToolType::UpsertTerm => Self::UpdateMetadata(UpdateMetadataConfig::default()),
            McpToolType::ParameterizedSql => Self::ParameterizedSql(ParamSqlConfig::default()),
        }
    }

    /// Reads a stored or submitted config object, dropping keys the tool does not know and filling
    /// the rest from the defaults of that tool's type.
    pub fn parse(tool_type: McpToolType, json: &str) -> Result<Self> {
        let parsed = match tool_type {
            McpToolType::SearchMetadata => {
                serde_json::from_str::<SearchMetadataConfig>(json).map(Self::SearchMetadata)
            }
            McpToolType::GetTableInfo => {
                serde_json::from_str::<GetTableInfoConfig>(json).map(Self::GetTableInfo)
            }
            McpToolType::ExecuteSql => {
                serde_json::from_str::<ExecuteSqlConfig>(json).map(Self::ExecuteSql)
            }
            McpToolType::ParameterizedSql => {
                serde_json::from_str::<ParamSqlConfig>(json).map(Self::ParameterizedSql)
            }
            // The Java assembler ignored an submitted config for the tools that have no settings.
            _ => Ok(Self::default_for(tool_type)),
        };
        parsed.map_err(|_| DatiError::validation(&[("config", "must be valid JSON")]))
    }

    /// The text stored in the `config` column.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }

    pub fn as_param_sql(&self) -> Option<&ParamSqlConfig> {
        match self {
            Self::ParameterizedSql(config) => Some(config),
            _ => None,
        }
    }

    pub fn as_execute_sql(&self) -> Option<&ExecuteSqlConfig> {
        match self {
            Self::ExecuteSql(config) => Some(config),
            _ => None,
        }
    }
}

/// A prebuilt slot as stored: the type fixes name, title and description, so only the two mutable
/// columns are kept.
#[derive(Debug, Clone, PartialEq)]
pub struct McpPrebuiltToolConfig {
    pub id: String,
    pub service_id: String,
    pub tool_type: McpToolType,
    pub enabled: bool,
    pub config: ToolConfig,
}

/// A tool an author wrote: `PARAMETERIZED_SQL` plus its own name.
#[derive(Debug, Clone, PartialEq)]
pub struct McpCustomTool {
    pub id: String,
    pub service_id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub title: Option<String>,
    pub tool_type: McpToolType,
    pub enabled: bool,
    /// `None` means the request left the config alone, which is how the Java update kept the stored
    /// template instead of overwriting it with an empty one.
    pub config: Option<ToolConfig>,
}

/// The `PUT /{toolId}` body, which is also the create body with the description requirement added
/// by the Java subclass. `config` is a JSON *string*, the way the frontend sends it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct CustomToolRequest {
    pub tool_type: Option<McpToolType>,
    pub name: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub enabled: Option<bool>,
    pub config: Option<String>,
}

impl CustomToolRequest {
    /// Bean Validation of the Java DTO: `@NotNull toolType` on both paths, plus the create-only
    /// `@NotBlank description`.
    pub fn validate(&self, for_create: bool) -> Result<()> {
        let mut fields: Vec<(&str, &str)> = Vec::new();
        if self.tool_type.is_none() {
            fields.push(("tool_type", "must not be null"));
        }
        if for_create
            && self
                .description
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .is_empty()
        {
            fields.push(("description", "description is required"));
        }
        if fields.is_empty() {
            return Ok(());
        }
        Err(DatiError::validation(&fields))
    }
}

/// The `McpToolVO` both halves of the tools list carry.
#[derive(Debug, Clone, Serialize)]
pub struct McpToolView {
    pub id: String,
    pub tool_type: McpToolType,
    pub name: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub enabled: bool,
    pub config: ToolConfig,
}

/// `GET /v1/mcp-services/{id}/tools`: the seven prebuilt slots and the custom tools, newest first.
#[derive(Debug, Clone, Serialize)]
pub struct ToolsResponse {
    pub prebuilt: Vec<McpToolView>,
    pub custom: Vec<McpToolView>,
}

/// `POST /tools/detect-annotations`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct DetectAnnotationsRequest {
    pub template: Option<String>,
    #[serde(default)]
    pub parameters: Vec<ToolParameter>,
}

impl DetectAnnotationsRequest {
    pub fn validate(&self) -> Result<()> {
        if self
            .template
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
        {
            return Err(DatiError::validation(&[("template", "must not be blank")]));
        }
        Ok(())
    }
}

/// The same four fields as the Java `DetectedAnnotations` record, which is what the endpoint echoes
/// back; `detected_operation` stays a string because `UNKNOWN` is a value the Java code produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DetectAnnotationsResponse {
    pub read_only: Option<bool>,
    pub idempotent: Option<bool>,
    pub destructive: Option<bool>,
    pub detected_operation: String,
}

/// `^[A-Za-z0-9_.\-]{1,128}$`, the Java tool-name pattern.
pub fn tool_name_is_valid(name: &str) -> bool {
    let chars: Vec<char> = name.chars().collect();
    !chars.is_empty()
        && chars.len() <= 128
        && chars
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql_analysis;

    fn policy(select: bool, insert: bool, update: bool, delete: bool) -> SqlPolicy {
        SqlPolicy {
            allow_select: select,
            allow_insert: insert,
            allow_update: update,
            allow_delete: delete,
            ..SqlPolicy::default()
        }
    }

    #[test]
    fn a_default_policy_reads_select_and_nothing_else() {
        let default = SqlPolicy::default();
        assert!(default.allow_select);
        assert!(
            !(default.allow_insert
                || default.allow_update
                || default.allow_delete
                || default.allow_ddl
                || default.allow_multi
                || default.allow_metadata
                || default.allow_transaction
                || default.allow_set)
        );

        let select = sql_analysis::analyze("SELECT * FROM orders");
        assert_eq!(default.violation(&select), None);

        for (sql, expected) in [
            ("UPDATE orders SET x = ?", "UPDATE not allowed"),
            ("DELETE FROM orders", "DELETE not allowed"),
            ("DROP TABLE orders", "DDL not allowed"),
            ("SHOW TABLES", "METADATA not allowed"),
        ] {
            let result = sql_analysis::analyze(sql);
            assert_eq!(default.violation(&result).as_deref(), Some(expected));
        }
    }

    #[test]
    fn a_merge_needs_both_write_grants_and_an_unknown_type_passes() {
        let merge = sql_analysis::analyze(
            "MERGE INTO orders t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET t.x = s.x",
        );
        assert_eq!(
            policy(true, true, false, false)
                .violation(&merge)
                .as_deref(),
            Some("MERGE not allowed")
        );
        assert_eq!(policy(true, true, true, false).violation(&merge), None);

        // The analyzer answers OTHER with no statement types, so the policy cannot name a verdict.
        let garbled = sql_analysis::analyze("NOT A SQL STATEMENT !!!");
        assert_eq!(garbled.statement_types.len(), 0);
        assert_eq!(SqlPolicy::default().violation(&garbled), None);
    }

    #[test]
    fn a_multi_statement_is_only_the_markers_call() {
        let both = sql_analysis::analyze("SELECT * FROM orders; DELETE FROM orders");
        assert!(both.is_multi());
        assert_eq!(
            policy(true, false, false, true).violation(&both).as_deref(),
            Some("multi-statement not allowed")
        );
        let mut allow_all = policy(true, true, true, true);
        allow_all.allow_multi = true;
        assert_eq!(allow_all.violation(&both), None);
    }

    #[test]
    fn each_tool_type_reads_its_own_config_shape() {
        let execute: ToolConfig =
            ToolConfig::parse(McpToolType::ExecuteSql, r#"{"timeout":5,"max_rows":10}"#).unwrap();
        assert_eq!(
            execute.to_json(),
            r#"{"sql_policy":{"allow_select":true,"allow_insert":false,"allow_update":false,"allow_delete":false,"allow_ddl":false,"allow_multi":false,"allow_metadata":false,"allow_transaction":false,"allow_set":false},"timeout":5,"max_rows":10}"#
        );

        // An omitted field keeps the Java initializer rather than zero, and unknown keys are dropped.
        let partial = ToolConfig::parse(
            McpToolType::ExecuteSql,
            r#"{"junk":1,"sql_policy":{"allow_insert":true}}"#,
        )
        .unwrap();
        let config = partial.as_execute_sql().unwrap();
        assert_eq!(config.timeout, 30);
        assert_eq!(config.max_rows, 1000);
        assert!(
            config.sql_policy.allow_select,
            "a policy that names one grant keeps SELECT"
        );
        assert!(config.sql_policy.allow_insert);

        let empty = ToolConfig::parse(McpToolType::ParameterizedSql, "{}").unwrap();
        assert_eq!(
            empty.to_json(),
            r#"{"data_source_id":null,"sql_template":null,"parameters":[],"timeout":30,"max_rows":1000,"read_only":null,"idempotent":null,"destructive":null}"#
        );

        // The tools without settings ignore whatever the caller sent.
        assert_eq!(
            ToolConfig::parse(McpToolType::UpsertTerm, r#"{"timeout":9}"#)
                .unwrap()
                .to_json(),
            "{}"
        );
        ToolConfig::parse(McpToolType::GetTableInfo, "not json").unwrap_err();
    }

    #[test]
    fn a_prebuilt_tool_of_every_type_has_a_default_config() {
        for tool_type in McpToolType::ALL {
            // An empty object reads as the same config the slot reports when nothing is stored.
            assert_eq!(
                ToolConfig::default_for(tool_type),
                ToolConfig::parse(tool_type, "{}").unwrap(),
                "{tool_type:?}"
            );
        }
        assert_eq!(
            ToolConfig::default_for(McpToolType::ListTables).to_json(),
            "{}"
        );
        assert_eq!(
            ToolConfig::default_for(McpToolType::SearchMetadata).to_json(),
            r#"{"timeout":30}"#
        );
        assert_eq!(
            ToolConfig::default_for(McpToolType::GetTableInfo).to_json(),
            r#"{"timeout":30}"#
        );
        assert_eq!(
            ToolConfig::default_for(McpToolType::UpdateTableInfo).to_json(),
            "{}"
        );
    }

    #[test]
    fn a_create_body_needs_a_type_and_a_description() {
        let err = CustomToolRequest::default().validate(true).unwrap_err();
        assert_eq!(
            err.message(),
            "Request parameter error: tool_type: must not be null; description: description is required"
        );
        CustomToolRequest {
            tool_type: Some(McpToolType::ParameterizedSql),
            description: Some("按城市统计订单".into()),
            ..Default::default()
        }
        .validate(true)
        .unwrap();
        // An update toggles `enabled` alone, so the description requirement is create-only.
        CustomToolRequest {
            tool_type: Some(McpToolType::ExecuteSql),
            enabled: Some(false),
            ..Default::default()
        }
        .validate(false)
        .unwrap();
    }

    #[test]
    fn a_tool_name_follows_the_java_pattern() {
        for good in ["query_orders", "Query-Orders.v2", "a", &"x".repeat(128)] {
            assert!(tool_name_is_valid(good), "{good} is valid");
        }
        for bad in ["", " ", "订单", "query orders", "a/b", &"x".repeat(129)] {
            assert!(!tool_name_is_valid(bad), "{bad} is invalid");
        }
    }

    #[test]
    fn the_tools_payload_serializes_config_as_an_object() {
        let view = McpToolView {
            id: "SEARCH_METADATA".into(),
            tool_type: McpToolType::SearchMetadata,
            name: Some("search_tables_and_terms".into()),
            title: Some("Search Tables and Terms".into()),
            description: Some("desc".into()),
            enabled: true,
            config: ToolConfig::default_for(McpToolType::SearchMetadata),
        };
        let payload = ToolsResponse {
            prebuilt: vec![view],
            custom: vec![],
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["prebuilt"][0]["id"], "SEARCH_METADATA");
        assert_eq!(json["prebuilt"][0]["tool_type"], "SEARCH_METADATA");
        assert_eq!(json["prebuilt"][0]["config"]["timeout"], 30);
        assert_eq!(json["custom"].as_array().unwrap().len(), 0);

        let body: CustomToolRequest = serde_json::from_value(serde_json::json!({
            "tool_type": "PARAMETERIZED_SQL",
            "name": "by_city",
            "description": "d",
            "config": "{\"sql_template\":\"SELECT 1\"}",
        }))
        .unwrap();
        assert_eq!(body.tool_type, Some(McpToolType::ParameterizedSql));
        assert_eq!(
            body.config.as_deref(),
            Some(r#"{"sql_template":"SELECT 1"}"#)
        );
    }
}
