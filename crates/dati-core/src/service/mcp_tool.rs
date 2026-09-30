//! The tool slots of a service: the seven prebuilt ones, whose only stored state is enablement and
//! a config object, and the parameterized-SQL tools an author writes.
//!
//! This is the Java `McpToolService` and `McpToolAssembler` in one, because the assembler's job here
//! is only to read the defaults a slot has when no row exists yet.

use serde_json::{Map, Value};

use crate::auth::AuthUser;
use crate::db::Db;
use crate::error::{DatiError, ErrorCode, NO_ARGS, Result};
use crate::model::base::Audit;
use crate::model::mcp::{McpToolType, PREBUILT_TOOLS};
use crate::model::mcp_tool::{
    CustomToolRequest, DetectAnnotationsResponse, McpCustomTool, McpPrebuiltToolConfig,
    McpToolView, ToolConfig, ToolParameter, ToolsResponse, tool_name_is_valid,
};
use crate::model::permission::Permission;
use crate::repo::mcp_tool;
use crate::sql_analysis::{self, SqlOperationType};
use crate::system_variable;
use crate::template::{self, values_of};
use crate::time::new_id;

use super::mcp_service::McpServiceService;

#[derive(Clone)]
pub struct McpToolService {
    db: Db,
    services: McpServiceService,
}

impl McpToolService {
    pub fn new(db: Db, services: McpServiceService) -> Self {
        Self { db, services }
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    /// `GET /v1/mcp-services/{id}/tools`.
    pub async fn list(&self, caller: &AuthUser, service_id: &str) -> Result<ToolsResponse> {
        self.services
            .authorize(caller, service_id, Permission::View)
            .await?;
        Ok(ToolsResponse {
            prebuilt: self.prebuilt_list(service_id).await?,
            custom: self.custom_list(service_id).await?,
        })
    }

    /// The seven slots, in declaration order. A slot nobody touched reports its own defaults, which
    /// is why the list never reads from the stored rows alone.
    pub async fn prebuilt_list(&self, service_id: &str) -> Result<Vec<McpToolView>> {
        let stored = self.prebuilt_slots(service_id).await?;
        Ok(PREBUILT_TOOLS
            .iter()
            .map(|tool_type| {
                stored
                    .iter()
                    .find(|slot| slot.tool_type == *tool_type)
                    .map_or_else(|| default_slot(*tool_type), to_view)
            })
            .collect())
    }

    /// The stored rows plus a default for every slot that has none: what the publish snapshot and
    /// the endpoint both need.
    pub async fn prebuilt_slots(&self, service_id: &str) -> Result<Vec<McpPrebuiltToolConfig>> {
        let rows = mcp_tool::prebuilt(&self.db, service_id).await?;
        Ok(PREBUILT_TOOLS
            .iter()
            .map(|tool_type| {
                rows.iter()
                    .find(|slot| slot.tool_type == *tool_type)
                    .cloned()
                    .unwrap_or_else(|| McpPrebuiltToolConfig {
                        id: String::new(),
                        service_id: service_id.to_string(),
                        tool_type: *tool_type,
                        enabled: tool_type.default_enabled(),
                        config: ToolConfig::default_for(*tool_type),
                    })
            })
            .collect())
    }

    pub async fn custom_list(&self, service_id: &str) -> Result<Vec<McpToolView>> {
        Ok(self
            .custom_tools(service_id)
            .await?
            .iter()
            .map(to_custom_view)
            .collect())
    }

    /// The custom tools as stored, newest first.
    pub async fn custom_tools(&self, service_id: &str) -> Result<Vec<McpCustomTool>> {
        mcp_tool::custom(&self.db, service_id).await
    }

    /// `PUT /v1/mcp-services/{id}/tools/{toolId}`. A prebuilt type addresses its slot by type and
    /// ignores the path id; anything else edits the custom tool the path names.
    pub async fn update(
        &self,
        caller: &AuthUser,
        service_id: &str,
        tool_id: &str,
        request: &CustomToolRequest,
    ) -> Result<()> {
        request.validate(false)?;
        // `validate` made the type non-null, and one type decides which half of the table to write.
        let tool_type = request.tool_type.unwrap_or(McpToolType::ParameterizedSql);
        if tool_type.is_prebuilt() {
            return self
                .update_prebuilt(caller, service_id, tool_type, request)
                .await;
        }
        self.services
            .authorize(caller, service_id, Permission::Edit)
            .await?;
        let mut tool = mcp_tool::find_custom(&self.db, service_id, tool_id)
            .await?
            .ok_or_else(|| DatiError::new(ErrorCode::MsToolNotFound, [tool_id]))?;

        if let Some(name) = &request.name {
            require_tool_name(name)?;
            if tool.name.as_deref() != Some(name.as_str())
                && mcp_tool::name_taken(&self.db, service_id, name, Some(tool_id)).await?
            {
                return Err(DatiError::new(ErrorCode::MsToolNameExists, [name.clone()]));
            }
            tool.name = Some(name.clone());
        }
        if request.description.is_some() {
            tool.description = request.description.clone();
        }
        if request.title.is_some() {
            tool.title = request.title.clone();
        }
        tool.tool_type = tool_type;
        // A body that leaves `enabled` out re-enables the tool: the Java assembler read the absent
        // flag as `true`, and the update wrote the flag unconditionally.
        tool.enabled = request.enabled.unwrap_or(true);
        if let Some(config) = self.request_config(tool_type, request)? {
            validate_sql_template(Some(&config))?;
            tool.config = Some(config);
        }
        mcp_tool::update_custom(&self.db, &tool, &Audit::new(&caller.id)).await
    }

    async fn update_prebuilt(
        &self,
        caller: &AuthUser,
        service_id: &str,
        tool_type: McpToolType,
        request: &CustomToolRequest,
    ) -> Result<()> {
        self.services
            .authorize(caller, service_id, Permission::Edit)
            .await?;
        let config = match self.request_config(tool_type, request)? {
            Some(config) => config,
            None => ToolConfig::default_for(tool_type),
        };
        mcp_tool::save_prebuilt(
            &self.db,
            service_id,
            tool_type,
            request.enabled.unwrap_or(true),
            &config,
            &Audit::new(&caller.id),
        )
        .await
    }

    /// The `config` string of a request, read as the config object of this tool type. Blank and
    /// absent both mean "nothing was sent", which the Java assembler treated the same way.
    fn request_config(
        &self,
        tool_type: McpToolType,
        request: &CustomToolRequest,
    ) -> Result<Option<ToolConfig>> {
        match request
            .config
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        {
            None => Ok(None),
            Some(text) => Ok(Some(ToolConfig::parse(tool_type, text)?)),
        }
    }

    /// `POST /v1/mcp-services/{id}/tools`.
    pub async fn create(
        &self,
        caller: &AuthUser,
        service_id: &str,
        request: &CustomToolRequest,
    ) -> Result<String> {
        request.validate(true)?;
        let tool_type = request.tool_type.unwrap_or(McpToolType::ParameterizedSql);
        let config = self.request_config(tool_type, request)?;
        self.services
            .authorize(caller, service_id, Permission::Edit)
            .await?;
        let name = request.name.clone().unwrap_or_default();
        require_tool_name(&name)?;
        if mcp_tool::name_taken(&self.db, service_id, &name, None).await? {
            return Err(DatiError::new(ErrorCode::MsToolNameExists, [name.clone()]));
        }
        // A custom tool without a config has nothing to run, so creation always asks for one; an
        // update may leave the stored template alone.
        validate_sql_template(config.as_ref())?;
        let tool = McpCustomTool {
            id: new_id(),
            service_id: service_id.to_string(),
            name: Some(name),
            description: request.description.clone(),
            title: request.title.clone(),
            tool_type,
            enabled: request.enabled.unwrap_or(true),
            config,
        };
        mcp_tool::insert_custom(&self.db, &tool, &Audit::new(&caller.id)).await
    }

    /// `DELETE /v1/mcp-services/{id}/tools/{toolId}`: only a custom tool can be deleted, since a
    /// prebuilt slot is part of the service definition.
    pub async fn remove(&self, caller: &AuthUser, service_id: &str, tool_id: &str) -> Result<()> {
        self.services
            .authorize(caller, service_id, Permission::Edit)
            .await?;
        if !mcp_tool::delete_custom(&self.db, service_id, tool_id).await? {
            return Err(DatiError::new(ErrorCode::MsToolNotFound, [tool_id]));
        }
        Ok(())
    }
}

// ---- validation ---------------------------------------------------------------------------

fn require_tool_name(name: &str) -> Result<()> {
    if tool_name_is_valid(name) {
        return Ok(());
    }
    // `MS004` has no placeholder, so the name the Java exception passed was dropped by MessageFormat.
    Err(DatiError::new(ErrorCode::MsToolNameInvalid, NO_ARGS))
}

/// The three checks the Java service ran on a parameterized template, in its order.
fn validate_sql_template(config: Option<&ToolConfig>) -> Result<()> {
    let Some(ToolConfig::ParameterizedSql(params)) = config else {
        return Err(DatiError::new(
            ErrorCode::InvalidParameter,
            ["config is required for PARAMETERIZED_SQL tool"],
        ));
    };
    let Some(sql_template) = params
        .sql_template
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    else {
        return Err(DatiError::new(
            ErrorCode::InvalidParameter,
            ["sql_template is required in config"],
        ));
    };

    let compiled = template::parse(sql_template).map_err(|message| {
        DatiError::new(
            ErrorCode::MsTemplateSyntaxError,
            ["sql_template", message.as_str()],
        )
    })?;

    let declared: Vec<&str> = params
        .parameters
        .iter()
        .filter_map(|parameter| parameter.name.as_deref())
        .filter(|name| !name.trim().is_empty())
        .collect();
    let undefined: Vec<String> = compiled
        .variables()
        .into_iter()
        .filter(|name| !system_variable::is_system_variable(name))
        .filter(|name| !declared.contains(&name.as_str()))
        .collect();
    if !undefined.is_empty() {
        return Err(DatiError::new(
            ErrorCode::MsToolArgMismatch,
            [format!(
                "Template references undefined parameter(s): {}",
                undefined.join(", ")
            )],
        ));
    }

    // A `{{var}}` inside a string literal renders a `?` the driver does not count as a parameter,
    // so the statement could never bind.
    let quoted = template::find_quoted_variables(&compiled);
    if !quoted.is_empty() {
        return Err(DatiError::new(
            ErrorCode::MsTemplateQuotedVar,
            [quoted.join(", ")],
        ));
    }
    Ok(())
}

// ---- annotation detection -----------------------------------------------------------------

/// `POST /tools/detect-annotations`: renders the template with mock values, then reads the SQL the
/// way the runtime policy check will, so the editor can pre-fill the three MCP annotations.
pub fn detect_annotations(
    template_text: &str,
    parameters: &[ToolParameter],
) -> Result<DetectAnnotationsResponse> {
    if template_text.trim().is_empty() {
        return Err(DatiError::new(
            ErrorCode::InvalidParameter,
            ["template cannot be blank"],
        ));
    }
    let compiled = template::parse(template_text)
        .map_err(|message| DatiError::new(ErrorCode::InvalidParameter, [message]))?;

    let mut values = Map::new();
    for name in compiled.variables() {
        let declared = parameters
            .iter()
            .find(|parameter| parameter.name.as_deref() == Some(name.as_str()))
            .and_then(|parameter| parameter.param_type.as_deref());
        values.insert(name, mock_value(declared));
    }
    // A template the renderer refuses — a raw variable holding an array, say — is analyzed as typed,
    // which is what the Java fallback did.
    let analysis = match template::render_sql(&compiled, &values_of(&values)) {
        Ok(prepared) => sql_analysis::analyze(&prepared.sql),
        Err(_) => sql_analysis::analyze(template_text),
    };
    let operation = collapse(analysis.op_type, &analysis.statement_types);
    let (read_only, idempotent, destructive, detected_operation) =
        classify(operation, template_text);
    Ok(DetectAnnotationsResponse {
        read_only,
        idempotent,
        destructive,
        detected_operation,
    })
}

/// A multi-statement template reports the most dangerous thing it does; the three MCP hints are
/// written for a host that sees one tool, not for the statement list.
fn collapse(op_type: SqlOperationType, statement_types: &[SqlOperationType]) -> SqlOperationType {
    if op_type != SqlOperationType::Multi || statement_types.is_empty() {
        return op_type;
    }
    let any = |kinds: &[SqlOperationType]| statement_types.iter().any(|kind| kinds.contains(kind));
    if any(&[SqlOperationType::Delete, SqlOperationType::Ddl]) {
        SqlOperationType::Delete
    } else if any(&[
        SqlOperationType::Insert,
        SqlOperationType::Update,
        SqlOperationType::Merge,
    ]) {
        SqlOperationType::Update
    } else if statement_types
        .iter()
        .all(|kind| matches!(kind, SqlOperationType::Select | SqlOperationType::Metadata))
    {
        SqlOperationType::Select
    } else {
        SqlOperationType::Multi
    }
}

/// The three MCP annotations for one verdict, plus the keyword guess the Java default branch made
/// for the types that carry no meaning of their own.
fn classify(
    operation: SqlOperationType,
    template_text: &str,
) -> (Option<bool>, Option<bool>, Option<bool>, String) {
    let hints = |read_only, idempotent, destructive| {
        (
            Some(read_only),
            Some(idempotent),
            Some(destructive),
            operation.as_str().to_string(),
        )
    };
    match operation {
        SqlOperationType::Select | SqlOperationType::Metadata => hints(true, true, false),
        SqlOperationType::Update | SqlOperationType::Merge => hints(false, true, false),
        SqlOperationType::Insert => hints(false, false, false),
        SqlOperationType::Delete | SqlOperationType::Ddl => hints(false, false, true),
        SqlOperationType::Other
        | SqlOperationType::Multi
        | SqlOperationType::Transaction
        | SqlOperationType::Set => {
            let trimmed = template_text.trim().to_uppercase();
            let guess = [
                ("SELECT", Some(("SELECT", true, true, false))),
                ("WITH", Some(("SELECT", true, true, false))),
                ("DELETE", Some(("DELETE", false, false, true))),
                ("DROP", Some(("DELETE", false, false, true))),
                ("TRUNCATE", Some(("DELETE", false, false, true))),
                ("UPDATE", Some(("UPDATE", false, true, false))),
                ("INSERT", Some(("INSERT", false, false, false))),
            ]
            .into_iter()
            .find(|(prefix, _)| trimmed.starts_with(prefix))
            .and_then(|(_, guess)| guess);
            match guess {
                Some((name, read_only, idempotent, destructive)) => (
                    Some(read_only),
                    Some(idempotent),
                    Some(destructive),
                    name.to_string(),
                ),
                None => (None, None, None, operation.as_str().to_string()),
            }
        }
    }
}

/// The value the renderer binds for one variable, chosen by the type the author declared.
fn mock_value(param_type: Option<&str>) -> Value {
    match param_type.map(str::to_lowercase).as_deref() {
        Some("number" | "integer" | "int" | "double" | "float") => Value::from(1),
        Some("boolean" | "bool") => Value::Bool(true),
        Some("datetime" | "date" | "time" | "timestamp") => {
            Value::String("2026-01-01 00:00:00".into())
        }
        Some("array" | "list") => Value::Array(vec![Value::String("mock_item".into())]),
        _ => Value::String("mock".into()),
    }
}

// ---- views --------------------------------------------------------------------------------

fn default_slot(tool_type: McpToolType) -> McpToolView {
    McpToolView {
        id: tool_type.as_str().to_string(),
        tool_type,
        name: tool_type.tool_name().map(str::to_string),
        title: tool_type.title().map(str::to_string),
        description: tool_type.description().map(str::to_string),
        enabled: tool_type.default_enabled(),
        config: ToolConfig::default_for(tool_type),
    }
}

fn to_view(slot: &McpPrebuiltToolConfig) -> McpToolView {
    let mut view = default_slot(slot.tool_type);
    view.id = slot.tool_type.as_str().to_string();
    view.enabled = slot.enabled;
    view.config = slot.config.clone();
    view
}

fn to_custom_view(tool: &McpCustomTool) -> McpToolView {
    McpToolView {
        id: tool.id.clone(),
        tool_type: tool.tool_type,
        name: tool.name.clone(),
        title: tool.title.clone(),
        description: tool.description.clone(),
        enabled: tool.enabled,
        config: tool
            .config
            .clone()
            .unwrap_or_else(|| ToolConfig::default_for(tool.tool_type)),
    }
}

#[cfg(test)]
mod tests {
    use super::super::datasource::DataSourceService;
    use super::super::permission::PermissionService;
    use super::super::subject::SubjectService;
    use super::*;
    use crate::config::Config;
    use crate::model::datasource::{DataSource, DbType};
    use crate::model::mcp::{DataScopeItem, McpDataScopeType, McpServiceCreateRequest};
    use crate::repo::datasource;

    async fn store() -> (Db, McpServiceService, McpToolService) {
        let db = Db::sqlite_temp().await;
        sqlx::query(
            "INSERT INTO users (id, name, password) \
                     VALUES ('u-1', 'alice', 'x'), ('u-2', 'bob', 'x'), ('u-3', 'admin', 'x')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        let config = Config::from_source(|_| None);
        let permissions = PermissionService::new(&config);
        let data_sources = DataSourceService::new(db.clone(), &config, permissions.clone());
        let subjects = SubjectService::new(db.clone(), data_sources.clone());
        let services = McpServiceService::new(db.clone(), data_sources.clone(), subjects, &config);
        let tools = McpToolService::new(db.clone(), services.clone());
        (db, services, tools)
    }

    fn caller(id: &str, name: &str) -> AuthUser {
        AuthUser {
            id: id.to_string(),
            name: name.to_string(),
            display_name: None,
        }
    }

    /// A service owned by alice, with a data source she can see so the create passes its scope check.
    async fn service(db: &Db, services: &McpServiceService, alice: &AuthUser) -> String {
        let source = DataSource {
            id: "d-1".into(),
            name: Some("warehouse".into()),
            description: None,
            deleted: false,
            connection_url: Some("sqlite:demo.db".into()),
            db_type: DbType::Sqlite,
            user_name: Some("none".into()),
            password: None,
            default_schema: Some("main".into()),
            audit: Audit::new("u-1"),
        };
        datasource::insert(db, &source).await.unwrap();
        services
            .create(
                alice,
                &McpServiceCreateRequest {
                    code: Some("sales".into()),
                    name: Some("销售服务".into()),
                    description: None,
                    data_scopes: vec![DataScopeItem {
                        id: None,
                        scope_type: Some(McpDataScopeType::DataSource),
                        reference_id: Some("d-1".into()),
                        reference_name: None,
                    }],
                },
            )
            .await
            .unwrap()
    }

    fn custom_request(name: &str, config: &str) -> CustomToolRequest {
        CustomToolRequest {
            tool_type: Some(McpToolType::ParameterizedSql),
            name: Some(name.into()),
            title: None,
            description: Some("按城市统计订单".into()),
            enabled: None,
            config: Some(config.into()),
        }
    }

    const GOOD_CONFIG: &str = r#"{"sql_template":"SELECT * FROM orders WHERE city = {{city}}","parameters":[{"name":"city"}]}"#;

    #[tokio::test]
    async fn the_seven_prebuilt_slots_are_listed_with_their_defaults() {
        let (db, services, tools) = store().await;
        let alice = caller("u-1", "alice");
        let id = service(&db, &services, &alice).await;

        let list = tools.list(&alice, &id).await.unwrap();
        assert_eq!(list.prebuilt.len(), 7);
        assert!(list.custom.is_empty());
        // Declaration order, not storage order.
        let types: Vec<&str> = list
            .prebuilt
            .iter()
            .map(|slot| slot.tool_type.as_str())
            .collect();
        assert_eq!(
            types,
            vec![
                "SEARCH_METADATA",
                "GET_TABLE_INFO",
                "LIST_TABLES",
                "EXECUTE_SQL",
                "UPDATE_TABLE_INFO",
                "UPDATE_COLUMN_INFO",
                "UPSERT_TERM",
            ]
        );
        // The three metadata writers start disabled; the id is the type name of an untouched slot.
        assert_eq!(list.prebuilt[3].id, "EXECUTE_SQL");
        assert!(list.prebuilt[3].enabled);
        assert!(!list.prebuilt[4].enabled);
        assert!(!list.prebuilt[6].enabled);
        assert_eq!(
            list.prebuilt[0].name.as_deref(),
            Some("search_tables_and_terms")
        );
        assert_eq!(
            list.prebuilt[3].config,
            ToolConfig::default_for(McpToolType::ExecuteSql)
        );
    }

    #[tokio::test]
    async fn updating_a_prebuilt_slot_persists_enablement_and_config() {
        let (db, services, tools) = store().await;
        let alice = caller("u-1", "alice");
        let id = service(&db, &services, &alice).await;

        let request = CustomToolRequest {
            tool_type: Some(McpToolType::ExecuteSql),
            enabled: Some(false),
            config: Some(r#"{"timeout":5,"sql_policy":{"allow_insert":true}}"#.into()),
            ..Default::default()
        };
        // A prebuilt update addresses the slot by type and ignores the path id.
        tools
            .update(&alice, &id, "whatever", &request)
            .await
            .unwrap();

        let list = tools.list(&alice, &id).await.unwrap();
        let execute = list
            .prebuilt
            .iter()
            .find(|slot| slot.tool_type == McpToolType::ExecuteSql)
            .unwrap();
        assert!(!execute.enabled);
        let config = execute.config.as_execute_sql().unwrap();
        assert_eq!(config.timeout, 5);
        assert!(config.sql_policy.allow_insert);
        assert!(
            config.sql_policy.allow_select,
            "an omitted grant keeps the default"
        );

        // A slot nobody touched still reports its own defaults.
        let search = list
            .prebuilt
            .iter()
            .find(|slot| slot.tool_type == McpToolType::SearchMetadata)
            .unwrap();
        assert!(search.enabled);
        assert_eq!(
            search.config,
            ToolConfig::default_for(McpToolType::SearchMetadata)
        );
    }

    #[tokio::test]
    async fn an_author_creates_custom_tools_and_they_list_newest_first() {
        let (db, services, tools) = store().await;
        let alice = caller("u-1", "alice");
        let id = service(&db, &services, &alice).await;

        let first = tools
            .create(&alice, &id, &custom_request("by_city", GOOD_CONFIG))
            .await
            .unwrap();
        let second = tools
            .create(&alice, &id, &custom_request("by_date", GOOD_CONFIG))
            .await
            .unwrap();

        let list = tools.list(&alice, &id).await.unwrap();
        assert_eq!(list.custom.len(), 2);
        // Same-second rows fall back to the id tiebreaker, so match by name, not by position.
        let found = list
            .custom
            .iter()
            .find(|tool| tool.name.as_deref() == Some("by_date"))
            .expect("by_date is listed");
        assert_eq!(found.id, second);
        assert!(found.enabled, "a create with no flag starts enabled");
        assert_eq!(
            found.config.as_param_sql().unwrap().sql_template.as_deref(),
            Some("SELECT * FROM orders WHERE city = {{city}}")
        );
        assert!(
            list.custom
                .iter()
                .any(|tool| tool.id == first && tool.name.as_deref() == Some("by_city"))
        );
    }

    #[tokio::test]
    async fn a_create_reports_the_first_check_that_fails() {
        let (db, services, tools) = store().await;
        let alice = caller("u-1", "alice");
        let id = service(&db, &services, &alice).await;

        let mut no_description = custom_request("by_city", GOOD_CONFIG);
        no_description.description = None;
        assert_eq!(
            tools
                .create(&alice, &id, &no_description)
                .await
                .unwrap_err()
                .code(),
            ErrorCode::InvalidParameter
        );

        let mut bad_name = custom_request("订单", GOOD_CONFIG);
        bad_name.name = Some("订单".into());
        assert_eq!(
            tools
                .create(&alice, &id, &bad_name)
                .await
                .unwrap_err()
                .code(),
            ErrorCode::MsToolNameInvalid
        );

        let bad_json = custom_request("by_city", "not json");
        assert_eq!(
            tools
                .create(&alice, &id, &bad_json)
                .await
                .unwrap_err()
                .code(),
            ErrorCode::InvalidParameter
        );

        // A template referencing a parameter the author never declared.
        let undefined = custom_request(
            "by_city",
            r#"{"sql_template":"SELECT * FROM orders WHERE city = {{city}}"}"#,
        );
        let err = tools.create(&alice, &id, &undefined).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::MsToolArgMismatch);
        assert_eq!(
            err.message(),
            "Tool parameter mismatch: Template references undefined parameter(s): city"
        );

        // A variable inside a string literal, which the driver cannot bind.
        let quoted = custom_request(
            "by_city",
            r#"{"sql_template":"SELECT * FROM orders WHERE city = '{{city}}'","parameters":[{"name":"city"}]}"#,
        );
        assert_eq!(
            tools.create(&alice, &id, &quoted).await.unwrap_err().code(),
            ErrorCode::MsTemplateQuotedVar
        );

        // A template that will not parse.
        let broken = custom_request(
            "by_city",
            r#"{"sql_template":"SELECT * FROM orders WHERE x = {{y"}"#,
        );
        assert_eq!(
            tools.create(&alice, &id, &broken).await.unwrap_err().code(),
            ErrorCode::MsTemplateSyntaxError
        );

        // A parameterized tool with no config has nothing to run.
        let no_config = CustomToolRequest {
            tool_type: Some(McpToolType::ParameterizedSql),
            name: Some("by_city".into()),
            description: Some("d".into()),
            ..Default::default()
        };
        let err = tools.create(&alice, &id, &no_config).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidParameter);
        assert_eq!(
            err.message(),
            "Request parameter error: config is required for PARAMETERIZED_SQL tool"
        );

        tools
            .create(&alice, &id, &custom_request("by_city", GOOD_CONFIG))
            .await
            .unwrap();
        let err = tools
            .create(&alice, &id, &custom_request("by_city", GOOD_CONFIG))
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::MsToolNameExists);
        assert_eq!(
            err.message(),
            "Tool name already exists in this service: by_city"
        );
    }

    #[tokio::test]
    async fn updating_a_custom_tool_touches_only_the_named_fields() {
        let (db, services, tools) = store().await;
        let alice = caller("u-1", "alice");
        let id = service(&db, &services, &alice).await;
        let tool_id = tools
            .create(&alice, &id, &custom_request("by_city", GOOD_CONFIG))
            .await
            .unwrap();
        tools
            .create(&alice, &id, &custom_request("by_date", GOOD_CONFIG))
            .await
            .unwrap();

        // Disable alone: the name, description and stored template survive, and enabled follows the flag.
        tools
            .update(
                &alice,
                &id,
                &tool_id,
                &CustomToolRequest {
                    tool_type: Some(McpToolType::ParameterizedSql),
                    enabled: Some(false),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let stored = tools
            .list(&alice, &id)
            .await
            .unwrap()
            .custom
            .into_iter()
            .find(|tool| tool.id == tool_id)
            .unwrap();
        assert!(!stored.enabled);
        assert_eq!(stored.name.as_deref(), Some("by_city"));
        assert_eq!(stored.description.as_deref(), Some("按城市统计订单"));

        // Renaming onto a name another tool holds is rejected before anything is written.
        let err = tools
            .update(
                &alice,
                &id,
                &tool_id,
                &CustomToolRequest {
                    tool_type: Some(McpToolType::ParameterizedSql),
                    name: Some("by_date".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::MsToolNameExists);

        // A body that leaves `enabled` out re-enables the tool, matching the Java assembler.
        tools
            .update(
                &alice,
                &id,
                &tool_id,
                &CustomToolRequest {
                    tool_type: Some(McpToolType::ParameterizedSql),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let stored = tools
            .list(&alice, &id)
            .await
            .unwrap()
            .custom
            .into_iter()
            .find(|tool| tool.id == tool_id)
            .unwrap();
        assert!(stored.enabled);

        let err = tools
            .update(
                &alice,
                &id,
                "gone",
                &CustomToolRequest {
                    tool_type: Some(McpToolType::ParameterizedSql),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::MsToolNotFound);
    }

    #[tokio::test]
    async fn only_a_custom_tool_can_be_deleted() {
        let (db, services, tools) = store().await;
        let alice = caller("u-1", "alice");
        let id = service(&db, &services, &alice).await;
        let tool_id = tools
            .create(&alice, &id, &custom_request("by_city", GOOD_CONFIG))
            .await
            .unwrap();

        tools.remove(&alice, &id, &tool_id).await.unwrap();
        assert!(tools.list(&alice, &id).await.unwrap().custom.is_empty());
        // A second delete, and any prebuilt id, both miss the custom row.
        assert_eq!(
            tools
                .remove(&alice, &id, &tool_id)
                .await
                .unwrap_err()
                .code(),
            ErrorCode::MsToolNotFound
        );
        assert_eq!(
            tools
                .remove(&alice, &id, "EXECUTE_SQL")
                .await
                .unwrap_err()
                .code(),
            ErrorCode::MsToolNotFound
        );
    }

    #[tokio::test]
    async fn a_stranger_may_not_write_tools() {
        let (db, services, tools) = store().await;
        let alice = caller("u-1", "alice");
        let bob = caller("u-2", "bob");
        let id = service(&db, &services, &alice).await;

        assert_eq!(
            tools.list(&bob, &id).await.unwrap_err().code(),
            ErrorCode::PermissionDenied
        );
        assert_eq!(
            tools
                .create(&bob, &id, &custom_request("by_city", GOOD_CONFIG))
                .await
                .unwrap_err()
                .code(),
            ErrorCode::PermissionDenied
        );
    }

    #[test]
    fn detection_reads_the_template_like_the_runtime_will() {
        let select = detect_annotations("SELECT * FROM orders", &[]).unwrap();
        assert_eq!(
            (select.read_only, select.idempotent, select.destructive),
            (Some(true), Some(true), Some(false))
        );
        assert_eq!(select.detected_operation, "SELECT");

        let delete = detect_annotations(
            "DELETE FROM orders WHERE id = {{id}}",
            &[ToolParameter {
                name: Some("id".into()),
                ..Default::default()
            }],
        )
        .unwrap();
        assert_eq!(
            (delete.read_only, delete.destructive),
            (Some(false), Some(true))
        );
        assert_eq!(delete.detected_operation, "DELETE");

        // A multi collapses to the most dangerous thing it does.
        let both = detect_annotations("SELECT 1; DELETE FROM orders", &[]).unwrap();
        assert_eq!(both.detected_operation, "DELETE");
        assert_eq!(both.destructive, Some(true));

        // An array variable cannot be inlined as a `?`, so the raw text is analyzed instead.
        let array = detect_annotations(
            "SELECT * FROM t WHERE id IN ({{ids}})",
            &[ToolParameter {
                name: Some("ids".into()),
                param_type: Some("array".into()),
                ..Default::default()
            }],
        )
        .unwrap();
        assert_eq!(array.detected_operation, "SELECT");

        let err = detect_annotations("   ", &[]).unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidParameter);
    }
}
