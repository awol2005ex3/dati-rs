//! What an MCP tool call answers: the payloads of the seven prebuilt tools and the nine failure
//! texts a tool may report.
//!
//! Two JSON dialects meet in this module. The JSON-RPC envelope around these objects is camelCase,
//! while the payload itself is written by the Java `JsonUtils` mapper, which is snake_case — so
//! every field here is spelled the way the client sees it, and `type` is the discriminator the Java
//! `@JsonTypeInfo` wrote as the first key. Where a Java record was annotated
//! `@JsonInclude(NON_NULL)` the matching Rust field skips `None`; where it was not, the `Option`
//! serializes as `null`, because that is what those payloads carried.

use serde::Serialize;
use serde_json::{Map, Value};

/// A tool failure: the `ToolError` enum name, its message category, and the Chinese text the agent
/// reads. Only `message` reaches an MCP client; `category` is for the draft-test endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolError {
    ToolNotFound,
    ToolDisabled,
    ParamMissing,
    ParamInvalid,
    EntityNotFound,
    DataSourceNotFound,
    ScopeViolation,
    SqlPolicyViolation,
    SqlExecutionError,
}

impl ToolError {
    pub const fn category(self) -> &'static str {
        match self {
            Self::ToolNotFound
            | Self::ToolDisabled
            | Self::ParamMissing
            | Self::ParamInvalid
            | Self::EntityNotFound
            | Self::DataSourceNotFound => "PARAM_ERROR",
            Self::ScopeViolation => "SCOPE_ERROR",
            Self::SqlPolicyViolation => "PERMISSION_DENIED",
            Self::SqlExecutionError => "SQL_ERROR",
        }
    }

    /// The `MessageFormat` template. `{0}` is filled by `format`; `数据源不存在` deliberately has
    /// no placeholder, which is why the Java code could pass a data source id and lose it.
    pub const fn template(self) -> &'static str {
        match self {
            Self::ToolNotFound => "工具不存在",
            Self::ToolDisabled => "工具已禁用",
            Self::ParamMissing => "缺少必填参数：{0}",
            Self::ParamInvalid => "参数不合法：{0}",
            Self::EntityNotFound => "表/列/术语不存在：{0}",
            Self::DataSourceNotFound => "数据源不存在",
            Self::ScopeViolation => "数据源或表不在服务范围内：{0}",
            Self::SqlPolicyViolation => "SQL 操作被策略禁止：{0}",
            Self::SqlExecutionError => "SQL 执行失败：{0}",
        }
    }

    pub fn format(self, args: &[&str]) -> String {
        let mut out = String::with_capacity(64);
        let mut rest = self.template();
        while let Some(at) = rest.find('{') {
            let Some(width) = rest[at + 1..].find('}').map(|offset| offset + 2) else {
                break;
            };
            let index: usize = rest[at + 1..at + width - 1]
                .trim()
                .parse()
                .unwrap_or(usize::MAX);
            out.push_str(&rest[..at]);
            // `MessageFormat` prints the literal placeholder when the argument list is short.
            out.push_str(args.get(index).copied().unwrap_or("{0}"));
            rest = &rest[at + width..];
        }
        out.push_str(rest);
        out
    }
}

/// A raised tool failure: the text the agent sees plus the category the test endpoint reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolFailure {
    pub error: ToolError,
    pub message: String,
}

impl ToolFailure {
    pub fn new(error: ToolError, args: &[&str]) -> Self {
        Self {
            error,
            message: error.format(args),
        }
    }

    pub fn category(&self) -> &'static str {
        self.error.category()
    }
}

impl std::fmt::Display for ToolFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ToolFailure {}

/// How a tool call ends. A `Failure` is the Java `ToolExecuteException`: the agent is answered with
/// an errored tool result carrying that text. An `Internal` is any other error of the gateway — the
/// Java handler caught it one level up and answered a JSON-RPC internal error instead.
#[derive(Debug, thiserror::Error)]
pub enum CallError {
    #[error(transparent)]
    Failure(#[from] ToolFailure),
    #[error(transparent)]
    Internal(#[from] crate::error::DatiError),
}

/// What every tool executor returns.
pub type CallResult<T> = std::result::Result<T, CallError>;

/// One statement's outcome, mirroring the Java `StatementResult` records: a `SELECT` carries the
/// columns, rows and row count, a write only the affected count, and a failure only its message.
/// Every field the Java interface marked `NON_NULL` is an `Option` here.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct StatementResult {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<Vec<Vec<Value>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_rows: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub affected_rows: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

impl StatementResult {
    pub fn select(columns: Vec<String>, rows: Vec<Vec<Value>>) -> Self {
        let total_rows = rows.len() as i64;
        Self {
            kind: "SELECT",
            success: true,
            columns: Some(columns),
            rows: Some(rows),
            total_rows: Some(total_rows),
            affected_rows: None,
            error_message: None,
        }
    }

    pub fn write(affected_rows: u64) -> Self {
        Self {
            kind: "WRITE",
            success: true,
            columns: None,
            rows: None,
            total_rows: None,
            affected_rows: Some(affected_rows as i64),
            error_message: None,
        }
    }
}

/// A column as the schema tools report it. The Java record had no `NON_NULL`, so an unknown type or
/// comment still appears as `null`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ColumnDef {
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub column_type: Option<String>,
    pub comment: Option<String>,
    pub aliases: Vec<String>,
    pub sample_values: Vec<String>,
}

/// A table without columns, the shape `LIST_TABLES` and the search hits share. `NON_NULL` in Java,
/// so a missing schema or description is absent rather than null.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct TableDef {
    pub table: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub aliases: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<ColumnDef>>,
}

/// A data source group of a table list or a search hit. No `NON_NULL` in the Java record, so the
/// optional fields serialize as `null` when the source has no name, type or description.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct DataSourceDef {
    pub id: String,
    pub name: Option<String>,
    pub db_type: Option<String>,
    pub default_schema: Option<String>,
    pub description: Option<String>,
    pub tables: Vec<TableDef>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct TermDef {
    pub name: Option<String>,
    pub description: Option<String>,
    pub subject_name: Option<String>,
}

/// One item of a metadata-writing tool's answer. `old` and `new` hold the `{description, aliases}`
/// pair; `new` is quoted because the Java record renamed the keyword with `@JsonProperty`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct EntityUpdateResult {
    pub entity_type: &'static str,
    pub entity: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_type: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "new")]
    pub new_value: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<UpdateError>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct UpdateError {
    pub error_category: String,
    pub message: String,
}

impl UpdateError {
    pub fn of(failure: &ToolFailure) -> Self {
        Self {
            error_category: failure.category().to_string(),
            message: failure.message.clone(),
        }
    }
}

/// The `{description, aliases}` pair `old` and `new` carry; a missing alias list reads as empty,
/// which is what the Java `valueMap` wrote.
pub fn value_map(description: Option<&str>, aliases: &[String]) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert(
        "description".to_string(),
        description.map(Value::from).unwrap_or(Value::Null),
    );
    map.insert("aliases".to_string(), Value::from(aliases.to_vec()));
    map
}

/// The whole answer of one tool call. The `type` key is written first by serde because it is the
/// enum tag, which is how the Java `@JsonTypeInfo` discriminator arrived.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ToolData {
    SqlExecution {
        executed_sql: String,
        results: Vec<StatementResult>,
        #[serde(skip_serializing_if = "Option::is_none")]
        bindings: Option<Vec<Value>>,
    },
    TableMetadata {
        tables: Vec<TableDef>,
    },
    SearchHit {
        keywords: Vec<String>,
        data_sources: Vec<DataSourceDef>,
        terms: Vec<TermDef>,
    },
    TableList {
        data_sources: Vec<DataSourceDef>,
        terms: Vec<TermDef>,
    },
    MetadataUpdate {
        results: Vec<EntityUpdateResult>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_failure_text_is_the_java_message_with_the_argument_filled() {
        assert_eq!(
            ToolFailure::new(ToolError::ParamInvalid, &["must not be null"]).message,
            "参数不合法：must not be null"
        );
        assert_eq!(
            ToolFailure::new(ToolError::ScopeViolation, &["No data scope configured"]).message,
            "数据源或表不在服务范围内：No data scope configured"
        );
        assert_eq!(
            ToolFailure::new(ToolError::SqlPolicyViolation, &["DELETE not allowed"]).message,
            "SQL 操作被策略禁止：DELETE not allowed"
        );
        assert_eq!(
            ToolFailure::new(ToolError::SqlExecutionError, &["no such table: orders"]).message,
            "SQL 执行失败：no such table: orders"
        );
        assert_eq!(
            ToolFailure::new(ToolError::EntityNotFound, &["table ds-1.orders"]).message,
            "表/列/术语不存在：table ds-1.orders"
        );
        // The template has no placeholder, so the id the Java code passed is dropped.
        assert_eq!(
            ToolFailure::new(ToolError::DataSourceNotFound, &["ds-1"]).message,
            "数据源不存在"
        );
        assert_eq!(
            ToolFailure::new(ToolError::ParamMissing, &["city"]).message,
            "缺少必填参数：city"
        );
        assert_eq!(
            ToolFailure::new(ToolError::ToolNotFound, &[]).message,
            "工具不存在"
        );
        assert_eq!(
            ToolFailure::new(ToolError::ToolDisabled, &[]).message,
            "工具已禁用"
        );
        // Without an argument at all the text keeps the literal placeholder, as `MessageFormat` did.
        assert_eq!(
            ToolFailure::new(ToolError::ParamInvalid, &[] as &[&str]).message,
            "参数不合法：{0}"
        );
    }

    #[test]
    fn the_categories_are_the_java_strings() {
        assert_eq!(
            ToolFailure::new(ToolError::ParamInvalid, &[]).category(),
            "PARAM_ERROR"
        );
        assert_eq!(ToolError::ScopeViolation.category(), "SCOPE_ERROR");
        assert_eq!(
            ToolError::SqlPolicyViolation.category(),
            "PERMISSION_DENIED"
        );
        assert_eq!(ToolError::SqlExecutionError.category(), "SQL_ERROR");
    }

    #[test]
    fn a_sql_result_writes_its_discriminator_first_and_nulls_are_dropped() {
        let payload = ToolData::SqlExecution {
            executed_sql: "SELECT 1".to_string(),
            results: vec![
                StatementResult::select(vec!["one".into()], vec![vec![json!(1)]]),
                StatementResult::write(3),
            ],
            bindings: None,
        };
        let text = serde_json::to_string(&payload).unwrap();
        assert!(text.starts_with(r#"{"type":"SQL_EXECUTION""#), "{text}");
        assert_eq!(
            serde_json::to_value(&payload).unwrap(),
            json!({
                "type": "SQL_EXECUTION",
                "executed_sql": "SELECT 1",
                "results": [
                    {"type": "SELECT", "success": true, "columns": ["one"], "rows": [[1]], "total_rows": 1},
                    {"type": "WRITE", "success": true, "affected_rows": 3},
                ],
            })
        );
    }

    #[test]
    fn a_table_group_keeps_nulls_while_a_table_def_drops_them() {
        let hit = ToolData::SearchHit {
            keywords: vec!["订单".into()],
            data_sources: vec![DataSourceDef {
                id: "ds-1".into(),
                name: None,
                db_type: None,
                default_schema: None,
                description: None,
                tables: vec![TableDef {
                    table: Some("orders".into()),
                    schema: None,
                    description: None,
                    aliases: vec![],
                    columns: Some(vec![ColumnDef::default()]),
                }],
            }],
            terms: vec![TermDef::default()],
        };
        assert_eq!(
            serde_json::to_value(&hit).unwrap(),
            json!({
                "type": "SEARCH_HIT",
                "keywords": ["订单"],
                "data_sources": [{
                    "id": "ds-1",
                    "name": null,
                    "db_type": null,
                    "default_schema": null,
                    "description": null,
                    "tables": [{
                        "table": "orders",
                        "aliases": [],
                        "columns": [{
                            "name": null,
                            "type": null,
                            "comment": null,
                            "aliases": [],
                            "sample_values": [],
                        }],
                    }],
                }],
                "terms": [{"name": null, "description": null, "subject_name": null}],
            })
        );
    }

    #[test]
    fn a_metadata_update_names_the_old_and_new_pair_and_the_error() {
        let failure = ToolFailure::new(
            ToolError::ScopeViolation,
            &["Data source ds-9 not in service scope"],
        );
        let updated = EntityUpdateResult {
            entity_type: "TABLE",
            entity: "public.orders".into(),
            success: true,
            change_type: Some("UPDATE"),
            old: Some(value_map(Some("旧"), &["o".into()])),
            new_value: Some(value_map(Some("新"), &[])),
            error: None,
        };
        let failed = EntityUpdateResult {
            entity_type: "TABLE",
            entity: "orders".into(),
            success: false,
            change_type: None,
            old: None,
            new_value: None,
            error: Some(UpdateError::of(&failure)),
        };
        let payload = ToolData::MetadataUpdate {
            results: vec![updated, failed],
        };
        assert_eq!(
            serde_json::to_value(&payload).unwrap(),
            json!({
                "type": "METADATA_UPDATE",
                "results": [
                    {
                        "entity_type": "TABLE",
                        "entity": "public.orders",
                        "success": true,
                        "change_type": "UPDATE",
                        "old": {"description": "旧", "aliases": ["o"]},
                        "new": {"description": "新", "aliases": []},
                    },
                    {
                        "entity_type": "TABLE",
                        "entity": "orders",
                        "success": false,
                        "error": {
                            "error_category": "SCOPE_ERROR",
                            "message": "数据源或表不在服务范围内：Data source ds-9 not in service scope",
                        },
                    },
                ],
            })
        );
    }

    #[test]
    fn the_five_payload_names_are_the_java_type_keys() {
        let names: Vec<String> = [
            ToolData::SqlExecution {
                executed_sql: String::new(),
                results: vec![],
                bindings: None,
            },
            ToolData::TableMetadata { tables: vec![] },
            ToolData::SearchHit {
                keywords: vec![],
                data_sources: vec![],
                terms: vec![],
            },
            ToolData::TableList {
                data_sources: vec![],
                terms: vec![],
            },
            ToolData::MetadataUpdate { results: vec![] },
        ]
        .iter()
        .map(|data| {
            serde_json::to_value(data).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
        assert_eq!(
            names,
            vec![
                "SQL_EXECUTION",
                "TABLE_METADATA",
                "SEARCH_HIT",
                "TABLE_LIST",
                "METADATA_UPDATE",
            ]
        );
    }
}
