//! The MCP layer: the services a client may connect to, and the data scope each one exposes.

use serde::{Deserialize, Serialize};

use crate::error::{DatiError, Result};

use super::base::Audit;

/// Lifecycle of a service. The wire value is the Java enum name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpServiceStatus {
    #[serde(rename = "DRAFT")]
    Draft,
    #[serde(rename = "PUBLISHED")]
    Published,
    #[serde(rename = "DISABLED")]
    Disabled,
}

impl McpServiceStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "DRAFT",
            Self::Published => "PUBLISHED",
            Self::Disabled => "DISABLED",
        }
    }

    pub fn from_stored(value: &str) -> Option<Self> {
        match value {
            "DRAFT" => Some(Self::Draft),
            "PUBLISHED" => Some(Self::Published),
            "DISABLED" => Some(Self::Disabled),
            _ => None,
        }
    }
}

/// What a scope row points at: one data source, or every table of one subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpDataScopeType {
    #[serde(rename = "DATA_SOURCE")]
    DataSource,
    #[serde(rename = "SUBJECT")]
    Subject,
}

impl McpDataScopeType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DataSource => "DATA_SOURCE",
            Self::Subject => "SUBJECT",
        }
    }

    pub fn from_stored(value: &str) -> Option<Self> {
        match value {
            "DATA_SOURCE" => Some(Self::DataSource),
            "SUBJECT" => Some(Self::Subject),
            _ => None,
        }
    }
}

/// The eight tool slots. Seven are prebuilt and carry their MCP-facing name, title, description and
/// annotations here rather than in the database; `PARAMETERIZED_SQL` is only ever a custom tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum McpToolType {
    #[serde(rename = "SEARCH_METADATA")]
    SearchMetadata,
    #[serde(rename = "GET_TABLE_INFO")]
    GetTableInfo,
    #[serde(rename = "LIST_TABLES")]
    ListTables,
    #[serde(rename = "EXECUTE_SQL")]
    ExecuteSql,
    #[serde(rename = "UPDATE_TABLE_INFO")]
    UpdateTableInfo,
    #[serde(rename = "UPDATE_COLUMN_INFO")]
    UpdateColumnInfo,
    #[serde(rename = "UPSERT_TERM")]
    UpsertTerm,
    #[serde(rename = "PARAMETERIZED_SQL")]
    ParameterizedSql,
}

/// Every prebuilt tool in the order `tools/list` reports them.
pub const PREBUILT_TOOLS: [McpToolType; 7] = [
    McpToolType::SearchMetadata,
    McpToolType::GetTableInfo,
    McpToolType::ListTables,
    McpToolType::ExecuteSql,
    McpToolType::UpdateTableInfo,
    McpToolType::UpdateColumnInfo,
    McpToolType::UpsertTerm,
];

impl McpToolType {
    /// Declaration order, which is the order the Java admin list returned.
    pub const ALL: [McpToolType; 8] = [
        Self::SearchMetadata,
        Self::GetTableInfo,
        Self::ListTables,
        Self::ExecuteSql,
        Self::UpdateTableInfo,
        Self::UpdateColumnInfo,
        Self::UpsertTerm,
        Self::ParameterizedSql,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SearchMetadata => "SEARCH_METADATA",
            Self::GetTableInfo => "GET_TABLE_INFO",
            Self::ListTables => "LIST_TABLES",
            Self::ExecuteSql => "EXECUTE_SQL",
            Self::UpdateTableInfo => "UPDATE_TABLE_INFO",
            Self::UpdateColumnInfo => "UPDATE_COLUMN_INFO",
            Self::UpsertTerm => "UPSERT_TERM",
            Self::ParameterizedSql => "PARAMETERIZED_SQL",
        }
    }

    pub fn from_stored(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| tool.as_str() == value)
    }

    /// The MCP-facing tool name, which only the prebuilt tools have.
    pub const fn tool_name(self) -> Option<&'static str> {
        match self {
            Self::SearchMetadata => Some("search_tables_and_terms"),
            Self::GetTableInfo => Some("get_table_schema"),
            Self::ListTables => Some("list_tables_and_terms"),
            Self::ExecuteSql => Some("execute_sql"),
            Self::UpdateTableInfo => Some("update_table_metadata"),
            Self::UpdateColumnInfo => Some("update_column_metadata"),
            Self::UpsertTerm => Some("upsert_business_term"),
            Self::ParameterizedSql => None,
        }
    }

    pub const fn title(self) -> Option<&'static str> {
        match self {
            Self::SearchMetadata => Some("Search Tables and Terms"),
            Self::GetTableInfo => Some("Get Table Schema"),
            Self::ListTables => Some("List Tables and Terms"),
            Self::ExecuteSql => Some("Execute SQL"),
            Self::UpdateTableInfo => Some("Update Table Metadata"),
            Self::UpdateColumnInfo => Some("Update Column Metadata"),
            Self::UpsertTerm => Some("Upsert Business Term"),
            Self::ParameterizedSql => None,
        }
    }

    pub const fn description(self) -> Option<&'static str> {
        match self {
            Self::SearchMetadata => Some(
                "Search relevant database tables, column names, sample values, and business terms \
                 by keywords. Always use this tool first to discover the right tables and terms \
                 instead of guessing tables or running exploratory SQL.",
            ),
            Self::GetTableInfo => Some(
                "Get detailed column schemas (names, types, comments, and sample values) for \
                 specified tables (up to 20 tables).",
            ),
            Self::ListTables => Some(
                "List all available tables (with schema, name, and description; overview only, no \
                 columns) and business terms under the service scope. Use this tool when \
                 exploring the catalog or when keyword search returns no matches.",
            ),
            Self::ExecuteSql => Some(
                "Execute an SQL query or statement against a data source and return result rows. \
                 Use this tool only when no specialized or custom analysis tool is available for \
                 your task.",
            ),
            Self::UpdateTableInfo => Some(
                "Enrich table description or aliases after analysis to improve future queries and \
                 search accuracy.",
            ),
            Self::UpdateColumnInfo => Some(
                "Enrich column descriptions (e.g. business meanings, enum mappings) or aliases \
                 after analysis to improve future queries.",
            ),
            Self::UpsertTerm => Some(
                "Create or update a business glossary term (definition, calculation rules, or \
                 aliases) under a subject after analysis.",
            ),
            Self::ParameterizedSql => None,
        }
    }

    /// The annotations object serialized for the tool definition, or `None` for no annotations at
    /// all — `EXECUTE_SQL` deliberately has none.
    pub const fn annotations_json(self) -> Option<&'static str> {
        match self {
            Self::SearchMetadata | Self::GetTableInfo | Self::ListTables => {
                Some(r#"{"readOnlyHint":true}"#)
            }
            Self::UpdateTableInfo | Self::UpdateColumnInfo | Self::UpsertTerm => Some(
                r#"{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true,"openWorldHint":true}"#,
            ),
            Self::ExecuteSql | Self::ParameterizedSql => None,
        }
    }

    /// The three metadata-writing tools start disabled; everything else starts enabled.
    pub const fn default_enabled(self) -> bool {
        !matches!(
            self,
            Self::UpdateTableInfo | Self::UpdateColumnInfo | Self::UpsertTerm
        )
    }

    pub const fn is_prebuilt(self) -> bool {
        !matches!(self, Self::ParameterizedSql)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpService {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub deleted: bool,
    pub code: String,
    pub status: Option<McpServiceStatus>,
    pub active_version_id: Option<String>,
    pub active_version_number: Option<i64>,
    pub audit: Audit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpDataScope {
    pub id: String,
    pub service_id: String,
    pub scope_type: McpDataScopeType,
    pub reference_id: String,
    pub audit: Audit,
}

/// One entry of a scope list, in both directions: `reference_name` is written by the server only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataScopeItem {
    pub id: Option<String>,
    pub scope_type: Option<McpDataScopeType>,
    pub reference_id: Option<String>,
    pub reference_name: Option<String>,
}

impl DataScopeItem {
    /// Bean Validation on the nested element, keyed by the Java property path.
    fn validate(&self, path: &str, fields: &mut Vec<(String, &'static str)>) {
        if self.scope_type.is_none() {
            fields.push((format!("{path}.scopeType"), "must not be null"));
        }
        if self.reference_id.is_none() {
            fields.push((format!("{path}.referenceId"), "must not be null"));
        }
    }
}

/// `POST /v1/mcp-services`: the code is supplied by the client, never generated.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct McpServiceCreateRequest {
    pub code: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub data_scopes: Vec<DataScopeItem>,
}

impl McpServiceCreateRequest {
    pub fn validate(&self) -> Result<()> {
        let mut fields: Vec<(String, &'static str)> = Vec::new();
        if blank(&self.code) {
            fields.push(("code".to_string(), "must not be blank"));
        }
        if blank(&self.name) {
            fields.push(("name".to_string(), "must not be blank"));
        }
        if self.data_scopes.is_empty() {
            fields.push(("dataScopes".to_string(), "must not be empty"));
        }
        for (index, item) in self.data_scopes.iter().enumerate() {
            item.validate(&format!("dataScopes[{index}]"), &mut fields);
        }
        if fields.is_empty() {
            return Ok(());
        }
        let fields = fields
            .iter()
            .map(|(f, r)| (f.as_str(), *r))
            .collect::<Vec<_>>();
        Err(DatiError::validation(&fields))
    }
}

/// `PUT /v1/mcp-services/{id}/data-scope`: unlike create, an empty list is legal and clears scope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct DataScopeRequest {
    #[serde(default)]
    pub items: Vec<DataScopeItem>,
}

impl DataScopeRequest {
    pub fn validate(&self) -> Result<()> {
        let mut fields = Vec::new();
        for (index, item) in self.items.iter().enumerate() {
            item.validate(&format!("items[{index}]"), &mut fields);
        }
        if fields.is_empty() {
            return Ok(());
        }
        let fields = fields
            .iter()
            .map(|(f, r)| (f.as_str(), *r))
            .collect::<Vec<_>>();
        Err(DatiError::validation(&fields))
    }
}

/// The `McpServiceVO` body of `PUT /v1/mcp-services/{id}`: the Java endpoint bound the whole domain
/// model but copied three fields, so everything else the client sends is read and discarded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct McpServiceUpdateRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub updated_by: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DataSourceRef {
    pub id: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DataScopeResponse {
    pub items: Vec<DataScopeItem>,
    pub resolved_data_sources: Vec<DataSourceRef>,
}

/// The Java `McpServiceVO`.
#[derive(Debug, Clone, Serialize)]
pub struct McpServiceView {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub created_by: Option<String>,
    pub created_at: Option<String>,
    pub updated_by: Option<String>,
    pub updated_at: Option<String>,
    pub created_user_name: Option<String>,
    pub updated_user_name: Option<String>,
    pub code: String,
    pub status: Option<McpServiceStatus>,
    pub endpoint_path: String,
    pub tool_count: i64,
    pub active_version_number: Option<i64>,
}

impl McpServiceView {
    pub fn new(
        service: &McpService,
        names: &std::collections::HashMap<String, String>,
        endpoint_base_url: &str,
        tool_count: i64,
    ) -> Self {
        let (created_user_name, updated_user_name) =
            super::base::author_names(&service.audit, names);
        Self {
            id: service.id.clone(),
            name: service.name.clone(),
            description: service.description.clone(),
            created_by: service.audit.created_by.clone(),
            created_at: service.audit.created_at.clone(),
            updated_by: service.audit.updated_by.clone(),
            updated_at: service.audit.updated_at.clone(),
            created_user_name,
            updated_user_name,
            code: service.code.clone(),
            status: service.status,
            endpoint_path: endpoint_path(endpoint_base_url, &service.code),
            tool_count,
            active_version_number: service.active_version_number,
        }
    }
}

/// The base URL loses every trailing slash, so an unset base still yields a relative path.
fn endpoint_path(base: &str, code: &str) -> String {
    let base = base.trim();
    let base = base.trim_end_matches('/');
    format!("{base}/{code}/mcp")
}

fn blank(value: &Option<String>) -> bool {
    value
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
        .is_empty()
}

/// `^[a-z0-9]([a-z0-9_-]{0,62}[a-z0-9])?$` of the Java service: 1-64 characters, lowercase letters,
/// digits, and `-`/`_` only between two alphanumeric ones.
pub fn code_is_valid(code: &str) -> bool {
    let chars: Vec<char> = code.chars().collect();
    let plain = |c: &char| c.is_ascii_lowercase() || c.is_ascii_digit();
    !chars.is_empty()
        && chars.len() <= 64
        && plain(chars.first().unwrap())
        && plain(chars.last().unwrap())
        && chars.iter().all(|c| plain(c) || *c == '_' || *c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_service_code_follows_the_java_pattern() {
        for good in ["a", "9", "sales", "sales-mcp", "sales_mcp", &"x".repeat(64)] {
            assert!(code_is_valid(good), "{good} is valid");
        }
        for bad in [
            "",
            "A",
            "sales-mcp!",
            "-sales",
            "sales_",
            "a".repeat(65).as_str(),
            "sa les",
        ] {
            assert!(!code_is_valid(bad), "{bad} is invalid");
        }
    }

    #[test]
    fn a_create_body_is_validated_like_spring_did() {
        let err = McpServiceCreateRequest::default().validate().unwrap_err();
        assert_eq!(
            err.message(),
            "Request parameter error: code: must not be blank; name: must not be blank; \
             dataScopes: must not be empty"
        );

        let body: McpServiceCreateRequest = serde_json::from_value(serde_json::json!({
            "code": "sales",
            "name": "销售",
            "data_scopes": [{"scope_type": "DATA_SOURCE"}, {"reference_id": "d-1"}],
        }))
        .unwrap();
        let err = body.validate().unwrap_err();
        assert_eq!(
            err.message(),
            "Request parameter error: dataScopes[0].referenceId: must not be null; \
             dataScopes[1].scopeType: must not be null"
        );

        let body: McpServiceCreateRequest = serde_json::from_value(serde_json::json!({
            "code": "sales",
            "name": "销售",
            "data_scopes": [{"scope_type": "SUBJECT", "reference_id": "s-1"}],
        }))
        .unwrap();
        body.validate().unwrap();
    }

    #[test]
    fn an_empty_scope_list_is_only_illegal_on_create() {
        // `PUT` may clear the scope; `POST` cannot, so the same body validates two ways.
        DataScopeRequest::default().validate().unwrap();
        McpServiceCreateRequest::default().validate().unwrap_err();

        let body: DataScopeRequest = serde_json::from_value(serde_json::json!({
            "items": [{"scope_type": "DATA_SOURCE"}, {}]
        }))
        .unwrap();
        let err = body.validate().unwrap_err();
        assert_eq!(
            err.message(),
            "Request parameter error: items[0].referenceId: must not be null; \
             items[1].scopeType: must not be null; items[1].referenceId: must not be null"
        );

        // An unknown scope type never reaches validation: the body itself does not read.
        serde_json::from_value::<DataScopeRequest>(serde_json::json!({
            "items": [{"scope_type": "NOPE", "reference_id": "d-1"}]
        }))
        .unwrap_err();
    }

    #[test]
    fn the_endpoint_path_matches_the_java_assembler() {
        assert_eq!(endpoint_path("", "sales"), "/sales/mcp");
        assert_eq!(
            endpoint_path("https://mcp.example.com", "sales"),
            "https://mcp.example.com/sales/mcp"
        );
        assert_eq!(
            endpoint_path("  https://mcp.example.com//  ", "sales"),
            "https://mcp.example.com/sales/mcp",
            "the base is trimmed and every trailing slash is dropped"
        );
    }

    #[test]
    fn statuses_and_scope_types_round_trip_as_enum_names() {
        assert_eq!(
            serde_json::to_string(&McpServiceStatus::Published).unwrap(),
            "\"PUBLISHED\""
        );
        assert_eq!(
            serde_json::from_str::<McpDataScopeType>("\"DATA_SOURCE\"").unwrap(),
            McpDataScopeType::DataSource
        );
        assert_eq!(
            McpServiceStatus::from_stored("DISABLED"),
            Some(McpServiceStatus::Disabled)
        );
        assert_eq!(McpServiceStatus::from_stored("PUBLISHING"), None);
    }
}
