//! The publish lifecycle of an MCP service: freeze the current configuration into a versioned
//! snapshot, move the service between its three states, compare the draft against the running
//! version, and restore an older snapshot as a new one.
//!
//! This is the Java `McpServicePublishService`. A publish never brings a disabled service back
//! online, and a rollback is always a forward move: the restored configuration is published as a
//! fresh, higher version rather than reactivating the old row.

use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;

use crate::auth::AuthUser;
use crate::db::Db;
use crate::error::{DatiError, ErrorCode, Result};
use crate::model::base::Audit;
use crate::model::mcp::{McpDataScope, McpService, McpServiceStatus};
use crate::model::mcp_prompt::McpPrompt;
use crate::model::mcp_tool::ToolConfig;
use crate::model::mcp_version::{
    McpServiceDiffVO, McpServiceSnapshot, McpServiceSnapshotVO, PublishRequest, RollbackRequest,
    ServiceInfo, SnapshotContent, SnapshotCustom, SnapshotPrebuilt, SnapshotPrompt, SnapshotScope,
};
use crate::model::permission::Permission;
use crate::repo::{mcp_prompt, mcp_service, mcp_snapshot, mcp_tool};
use crate::time::new_id;

use super::datasource::author_names;
use super::mcp_prompt::McpPromptService;
use super::mcp_service::McpServiceService;
use super::mcp_tool::McpToolService;

#[derive(Clone)]
pub struct McpVersionService {
    db: Db,
    services: McpServiceService,
    tools: McpToolService,
    prompts: McpPromptService,
}

impl McpVersionService {
    pub fn new(
        db: Db,
        services: McpServiceService,
        tools: McpToolService,
        prompts: McpPromptService,
    ) -> Self {
        Self {
            db,
            services,
            tools,
            prompts,
        }
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    /// `POST /v1/mcp-services/{id}/publish`. The service is loaded and its EDIT right checked before
    /// the scope is walked, so a bad id reports `MS001` rather than a bare denial.
    pub async fn publish(
        &self,
        caller: &AuthUser,
        id: &str,
        request: &PublishRequest,
    ) -> Result<String> {
        let service = self
            .services
            .authorize(caller, id, Permission::Edit)
            .await?;
        let scopes = self.services.scope_rows(id).await?;
        self.services
            .require_scope_rows_readable(caller, &scopes)
            .await?;
        if scopes.is_empty() {
            return Err(DatiError::new(ErrorCode::MsServiceDataScopeEmpty, [id]));
        }
        self.do_publish(caller, &service, request.release_note.clone())
            .await
    }

    /// `POST /v1/mcp-services/{id}/disable`: only a running service can be taken down.
    pub async fn disable(&self, caller: &AuthUser, id: &str) -> Result<String> {
        self.transition(
            caller,
            id,
            McpServiceStatus::Published,
            McpServiceStatus::Disabled,
        )
        .await
    }

    /// `POST /v1/mcp-services/{id}/enable`: only a disabled service can be brought back.
    pub async fn enable(&self, caller: &AuthUser, id: &str) -> Result<String> {
        self.transition(
            caller,
            id,
            McpServiceStatus::Disabled,
            McpServiceStatus::Published,
        )
        .await
    }

    async fn transition(
        &self,
        caller: &AuthUser,
        id: &str,
        from: McpServiceStatus,
        to: McpServiceStatus,
    ) -> Result<String> {
        let mut service = self
            .services
            .authorize(caller, id, Permission::Edit)
            .await?;
        let current = service.status.unwrap_or(McpServiceStatus::Draft);
        if current != from {
            let verb = if to == McpServiceStatus::Disabled {
                "disabled"
            } else {
                "enabled"
            };
            let detail = format!(
                "Only {} service can be {}, current: {}",
                from.as_str(),
                verb,
                current.as_str()
            );
            return Err(DatiError::new(ErrorCode::MsServiceStatusInvalid, [detail]));
        }
        service.status = Some(to);
        service.audit.touch(&caller.id);
        mcp_service::update_release(&self.db, &service).await?;
        Ok(service.id)
    }

    /// `GET /v1/mcp-services/{id}/snapshots`: the version history, newest first.
    pub async fn snapshots(
        &self,
        caller: &AuthUser,
        id: &str,
    ) -> Result<Vec<McpServiceSnapshotVO>> {
        self.services
            .authorize(caller, id, Permission::View)
            .await?;
        let rows = mcp_snapshot::list(&self.db, id).await?;
        let names = author_names(&self.db, rows.iter().map(|row| &row.audit)).await?;
        Ok(rows
            .iter()
            .map(|row| McpServiceSnapshotVO::new(row, &names))
            .collect())
    }

    /// `GET /v1/mcp-services/{id}/diff`: what the current draft changed against the running version.
    pub async fn diff(&self, caller: &AuthUser, id: &str) -> Result<McpServiceDiffVO> {
        let service = self
            .services
            .authorize(caller, id, Permission::View)
            .await?;
        let mut vo = McpServiceDiffVO::default();
        let Some(active_id) = service.active_version_id.clone() else {
            vo.has_changes = true;
            vo.modified_components = vec!["INITIAL_RELEASE".to_string()];
            return Ok(vo);
        };
        let Some(active) = mcp_snapshot::find_by_id(&self.db, &active_id).await? else {
            // The service points at a version row that is gone; treat it as fully changed.
            vo.has_changes = true;
            return Ok(vo);
        };
        vo.active_version_number = Some(active.version_number);
        let current = self.build_current_content(&service).await?;
        compare(&active.content, &current, &mut vo);
        vo.has_changes = vo.basic_info_changed
            || vo.data_scope_changed
            || vo.tools_changed
            || vo.prompts_changed;
        Ok(vo)
    }

    /// `POST /v1/mcp-services/{id}/rollback`: restore an older snapshot as the current draft, then
    /// publish that draft as a new, higher version.
    pub async fn rollback(
        &self,
        caller: &AuthUser,
        id: &str,
        request: &RollbackRequest,
    ) -> Result<String> {
        request.validate()?;
        let service = self
            .services
            .authorize(caller, id, Permission::Edit)
            .await?;
        let target = request.target_version_number.unwrap_or_default();
        let snapshot = mcp_snapshot::find(&self.db, id, target)
            .await?
            .ok_or_else(|| {
                DatiError::new(
                    ErrorCode::MsServiceVersionNotFound,
                    [format!("v{target} of service {id}")],
                )
            })?;
        let content = &snapshot.content;

        // The profile moves first: the draft must read as the restored version before it is frozen.
        let mut service = service;
        service.name = content.service_info.name.clone();
        service.description = content.service_info.description.clone();
        if let Some(code) = &content.service_info.code {
            service.code = code.clone();
        }
        service.audit.touch(&caller.id);
        mcp_service::restore_basic(&self.db, &service).await?;

        // The scope, the tools and the prompts are replaced wholesale, exactly like the Java service.
        let scopes: Vec<McpDataScope> = content
            .data_scopes
            .iter()
            .map(|scope| McpDataScope {
                id: String::new(),
                service_id: id.to_string(),
                scope_type: scope.scope_type,
                reference_id: scope.reference_id.clone(),
                audit: Audit::default(),
            })
            .collect();
        self.services
            .require_scope_rows_readable(caller, &scopes)
            .await?;
        mcp_service::clear_scopes(&self.db, id).await?;
        let audit = Audit::new(&caller.id);
        for scope in &scopes {
            mcp_service::insert_scope(&self.db, id, scope, &audit).await?;
        }

        mcp_tool::remove_all(&self.db, id).await?;
        for tool in &content.custom_tools {
            let mut restored = tool.to_tool()?;
            restored.id = new_id();
            mcp_tool::insert_custom(&self.db, &restored, &audit).await?;
        }
        for slot in &content.prebuilt_tools {
            let restored = slot.to_config()?;
            mcp_tool::save_prebuilt(
                &self.db,
                id,
                restored.tool_type,
                restored.enabled,
                &restored.config,
                &audit,
            )
            .await?;
        }

        mcp_prompt::remove_all(&self.db, id).await?;
        for prompt in &content.prompts {
            let row = McpPrompt {
                id: new_id(),
                service_id: id.to_string(),
                name: prompt.name.clone(),
                description: prompt.description.clone(),
                enabled: prompt.enabled,
                content: prompt.content.clone(),
                parameters: prompt.parameters.clone(),
                audit: audit.clone(),
            };
            mcp_prompt::insert(&self.db, &row).await?;
        }

        let note = match request.release_note.as_deref().map(str::trim) {
            Some(user_note) if !user_note.is_empty() => {
                format!("Rollback to v{target}: {user_note}")
            }
            _ => format!("Rollback to v{target}"),
        };
        self.do_publish(caller, &service, Some(note)).await
    }

    /// Freeze the current configuration and make it the running version. `release_note` is trimmed
    /// when present. The status only advances from `DRAFT`; publishing a `PUBLISHED` or `DISABLED`
    /// service never changes whether it is online.
    async fn do_publish(
        &self,
        caller: &AuthUser,
        service: &McpService,
        release_note: Option<String>,
    ) -> Result<String> {
        let content = self.build_current_content(service).await?;
        let next_version = mcp_snapshot::max_version(&self.db, &service.id)
            .await?
            .unwrap_or(0)
            + 1;
        let snapshot = McpServiceSnapshot {
            id: new_id(),
            service_id: service.id.clone(),
            version_number: next_version,
            release_note: release_note.map(|note| note.trim().to_string()),
            content,
            audit: Audit::new(&caller.id),
        };
        mcp_snapshot::insert(&self.db, &snapshot).await?;

        let mut service = service.clone();
        if !matches!(
            service.status,
            Some(McpServiceStatus::Published) | Some(McpServiceStatus::Disabled)
        ) {
            service.status = Some(McpServiceStatus::Published);
        }
        service.active_version_id = Some(snapshot.id.clone());
        service.active_version_number = Some(next_version);
        service.audit.touch(&caller.id);
        mcp_service::update_release(&self.db, &service).await?;
        Ok(snapshot.id)
    }

    /// The whole live configuration of a service, read back through the same services the editor
    /// uses, so a snapshot always captures exactly what the admin sees.
    async fn build_current_content(&self, service: &McpService) -> Result<SnapshotContent> {
        let scopes = self.services.scope_rows(&service.id).await?;
        let data_scopes = scopes
            .iter()
            .map(|row| SnapshotScope {
                service_id: row.service_id.clone(),
                scope_type: row.scope_type,
                reference_id: row.reference_id.clone(),
            })
            .collect();

        let mut prebuilt_tools = Vec::new();
        for slot in self.tools.prebuilt_slots(&service.id).await? {
            prebuilt_tools.push(SnapshotPrebuilt {
                service_id: slot.service_id.clone(),
                tool_type: slot.tool_type,
                enabled: slot.enabled,
                config: to_json(&slot.config)?,
            });
        }

        let mut custom_tools = Vec::new();
        for tool in self.tools.custom_tools(&service.id).await? {
            let config = match &tool.config {
                Some(config) => to_json(config)?,
                None => to_json(&ToolConfig::default_for(tool.tool_type))?,
            };
            custom_tools.push(SnapshotCustom {
                service_id: tool.service_id.clone(),
                name: tool.name.clone(),
                tool_type: tool.tool_type,
                title: tool.title.clone(),
                description: tool.description.clone(),
                enabled: tool.enabled,
                config,
            });
        }

        let prompts = self
            .prompts
            .stored(&service.id)
            .await?
            .iter()
            .map(|prompt| SnapshotPrompt {
                service_id: prompt.service_id.clone(),
                name: prompt.name.clone(),
                description: prompt.description.clone(),
                enabled: prompt.enabled,
                content: prompt.content.clone(),
                parameters: prompt.parameters.clone(),
            })
            .collect();

        Ok(SnapshotContent {
            service_info: ServiceInfo {
                id: Some(service.id.clone()),
                name: service.name.clone(),
                description: service.description.clone(),
                code: Some(service.code.clone()),
            },
            data_scopes,
            prebuilt_tools,
            custom_tools,
            prompts,
        })
    }
}

/// Serializes a snapshot config value, failing only on a config the type could never produce.
fn to_json<T: Serialize>(value: &T) -> Result<Value> {
    serde_json::to_value(value)
        .map_err(|_| DatiError::with_message(ErrorCode::InternalError, "Internal server error"))
}

/// Compares a frozen snapshot against the current draft and fills the diff's component flags.
fn compare(old: &SnapshotContent, current: &SnapshotContent, vo: &mut McpServiceDiffVO) {
    vo.basic_info_changed = old.service_info.name != current.service_info.name
        || old.service_info.description != current.service_info.description;
    if vo.basic_info_changed {
        vo.modified_components.push("BASIC_INFO".to_string());
    }

    let old_scopes: Vec<String> = old
        .data_scopes
        .iter()
        .map(|row| format!("{}|{}", row.scope_type.as_str(), row.reference_id))
        .collect();
    let current_scopes: Vec<String> = current
        .data_scopes
        .iter()
        .map(|row| format!("{}|{}", row.scope_type.as_str(), row.reference_id))
        .collect();
    vo.data_scope_changed = old_scopes != current_scopes;
    if vo.data_scope_changed {
        vo.modified_components.push("DATA_SCOPE".to_string());
    }

    compare_named(old, current, vo);
    compare_prompts(old, current, vo);

    vo.tools_changed =
        !vo.added_tools.is_empty() || !vo.modified_tools.is_empty() || !vo.deleted_tools.is_empty();
    if vo.tools_changed {
        vo.modified_components.push("TOOLS".to_string());
    }
    vo.prompts_changed = !vo.added_prompts.is_empty()
        || !vo.modified_prompts.is_empty()
        || !vo.deleted_prompts.is_empty();
    if vo.prompts_changed {
        vo.modified_components.push("PROMPTS".to_string());
    }
}

/// The custom tools (keyed by name) and the prebuilt slots (keyed by tool type) together feed the
/// single `TOOLS` component: a custom change and a prebuilt toggle both report under it.
fn compare_named(old: &SnapshotContent, current: &SnapshotContent, vo: &mut McpServiceDiffVO) {
    let custom_key = |tool: &SnapshotCustom| {
        format!(
            "{}|{}|{}|{}|{}|{}",
            tool.tool_type.as_str(),
            tool.name.as_deref().unwrap_or_default(),
            tool.title.as_deref().unwrap_or_default(),
            tool.description.as_deref().unwrap_or_default(),
            tool.enabled,
            tool.config,
        )
    };
    let old_custom: HashMap<&str, String> = old
        .custom_tools
        .iter()
        .map(|tool| (tool.name.as_deref().unwrap_or_default(), custom_key(tool)))
        .collect();
    let current_custom: HashMap<&str, String> = current
        .custom_tools
        .iter()
        .map(|tool| (tool.name.as_deref().unwrap_or_default(), custom_key(tool)))
        .collect();

    for tool in &current.custom_tools {
        let name = tool.name.as_deref().unwrap_or_default();
        match old_custom.get(name) {
            None => vo.added_tools.push(name.to_string()),
            Some(key) if *key != custom_key(tool) => vo.modified_tools.push(name.to_string()),
            Some(_) => {}
        }
    }
    for tool in &old.custom_tools {
        let name = tool.name.as_deref().unwrap_or_default();
        if !current_custom.contains_key(name) {
            vo.deleted_tools.push(name.to_string());
        }
    }

    // The prebuilt slots are compared by type; a changed type's name joins the modified list.
    let prebuilt_key = |slot: &SnapshotPrebuilt| format!("{}|{}", slot.enabled, slot.config);
    let old_prebuilt: HashMap<_, String> = old
        .prebuilt_tools
        .iter()
        .map(|slot| (slot.tool_type, prebuilt_key(slot)))
        .collect();
    for slot in &current.prebuilt_tools {
        let changed = match old_prebuilt.get(&slot.tool_type) {
            None => true,
            Some(key) => *key != prebuilt_key(slot),
        };
        if changed {
            vo.modified_tools.push(slot.tool_type.as_str().to_string());
        }
    }
}

fn compare_prompts(old: &SnapshotContent, current: &SnapshotContent, vo: &mut McpServiceDiffVO) {
    let prompt_key = |prompt: &SnapshotPrompt| {
        format!(
            "{}|{}|{}|{}|{}",
            prompt.name.as_deref().unwrap_or_default(),
            prompt.description.as_deref().unwrap_or_default(),
            prompt.content.as_deref().unwrap_or_default(),
            serde_json::to_string(&prompt.parameters).unwrap_or_default(),
            prompt.enabled,
        )
    };
    let old_prompts: HashMap<&str, String> = old
        .prompts
        .iter()
        .map(|prompt| {
            (
                prompt.name.as_deref().unwrap_or_default(),
                prompt_key(prompt),
            )
        })
        .collect();
    let current_prompts: HashMap<&str, String> = current
        .prompts
        .iter()
        .map(|prompt| {
            (
                prompt.name.as_deref().unwrap_or_default(),
                prompt_key(prompt),
            )
        })
        .collect();

    for prompt in &current.prompts {
        let name = prompt.name.as_deref().unwrap_or_default();
        match old_prompts.get(name) {
            None => vo.added_prompts.push(name.to_string()),
            Some(key) if *key != prompt_key(prompt) => vo.modified_prompts.push(name.to_string()),
            Some(_) => {}
        }
    }
    for prompt in &old.prompts {
        let name = prompt.name.as_deref().unwrap_or_default();
        if !current_prompts.contains_key(name) {
            vo.deleted_prompts.push(name.to_string());
        }
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
    use crate::model::mcp::{
        DataScopeItem, DataScopeRequest, McpDataScopeType, McpServiceCreateRequest,
        McpServiceUpdateRequest, McpToolType,
    };
    use crate::model::mcp_prompt::{McpPromptRequest, PromptParameter};
    use crate::model::mcp_tool::CustomToolRequest;
    use crate::repo::datasource;

    type Store = (
        Db,
        McpServiceService,
        McpToolService,
        McpPromptService,
        McpVersionService,
    );

    async fn store() -> Store {
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
        let prompts = McpPromptService::new(db.clone(), services.clone());
        let versions =
            McpVersionService::new(db.clone(), services.clone(), tools.clone(), prompts.clone());
        (db, services, tools, prompts, versions)
    }

    fn caller(id: &str, name: &str) -> AuthUser {
        AuthUser {
            id: id.to_string(),
            name: name.to_string(),
            display_name: None,
        }
    }

    /// A service owned by alice on a data source she can see, so create and publish pass their checks.
    async fn service(db: &Db, services: &McpServiceService, alice: &AuthUser) -> String {
        datasource::insert(
            db,
            &DataSource {
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
            },
        )
        .await
        .unwrap();
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

    const GOOD_CONFIG: &str = r#"{"sql_template":"SELECT * FROM orders WHERE city = {{city}}","parameters":[{"name":"city"}]}"#;

    async fn add_tool(tools: &McpToolService, alice: &AuthUser, id: &str, name: &str) {
        tools
            .create(
                alice,
                id,
                &CustomToolRequest {
                    tool_type: Some(McpToolType::ParameterizedSql),
                    name: Some(name.into()),
                    title: None,
                    description: Some("统计".into()),
                    enabled: None,
                    config: Some(GOOD_CONFIG.into()),
                },
            )
            .await
            .unwrap();
    }

    async fn add_prompt(prompts: &McpPromptService, alice: &AuthUser, id: &str, name: &str) {
        prompts
            .create(
                alice,
                id,
                &McpPromptRequest {
                    name: Some(name.into()),
                    description: None,
                    enabled: None,
                    content: Some("hi {{city}}".into()),
                    parameters: Some(vec![PromptParameter {
                        name: Some("city".into()),
                        description: None,
                        required: false,
                    }]),
                },
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_publish_freezes_the_current_configuration_and_becomes_active() {
        let (db, services, tools, prompts, versions) = store().await;
        let alice = caller("u-1", "alice");
        let id = service(&db, &services, &alice).await;
        add_tool(&tools, &alice, &id, "by_city").await;
        add_prompt(&prompts, &alice, &id, "weekly").await;

        let snapshot_id = versions
            .publish(&alice, &id, &PublishRequest::default())
            .await
            .unwrap();

        let service = services.load(&id).await.unwrap();
        assert_eq!(service.status, Some(McpServiceStatus::Published));
        assert_eq!(service.active_version_number, Some(1));
        assert_eq!(
            service.active_version_id.as_deref(),
            Some(snapshot_id.as_str())
        );

        let stored = mcp_snapshot::find(&db, &id, 1).await.unwrap().unwrap();
        assert_eq!(stored.id, snapshot_id);
        assert_eq!(stored.release_note, None);
        assert_eq!(
            stored.content.service_info.name.as_deref(),
            Some("销售服务")
        );
        assert_eq!(stored.content.data_scopes.len(), 1);
        assert_eq!(
            stored.content.custom_tools[0].name.as_deref(),
            Some("by_city")
        );
        assert_eq!(stored.content.prompts[0].name.as_deref(), Some("weekly"));
        assert_eq!(
            stored.content.prebuilt_tools.len(),
            7,
            "every slot is captured"
        );

        // A second publish takes the next number rather than replacing the first.
        let second = versions
            .publish(&alice, &id, &PublishRequest::default())
            .await
            .unwrap();
        assert_ne!(second, snapshot_id);
        assert_eq!(
            services.load(&id).await.unwrap().active_version_number,
            Some(2)
        );
    }

    #[tokio::test]
    async fn publishing_needs_the_edit_right_and_a_non_empty_scope() {
        let (db, services, _, _, versions) = store().await;
        let alice = caller("u-1", "alice");
        let bob = caller("u-2", "bob");
        let id = service(&db, &services, &alice).await;

        let err = versions
            .publish(&bob, &id, &PublishRequest::default())
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::PermissionDenied);
        let err = versions
            .publish(&alice, "gone", &PublishRequest::default())
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::MsServiceNotFound);

        // Clearing the scope is legal for the editor but leaves nothing to publish.
        services
            .save_data_scope(&alice, &id, &DataScopeRequest::default())
            .await
            .unwrap();
        let err = versions
            .publish(&alice, &id, &PublishRequest::default())
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::MsServiceDataScopeEmpty);
        assert_eq!(
            err.message(),
            format!("Data scope is empty, cannot publish MCP service: {id}")
        );
    }

    #[tokio::test]
    async fn only_a_published_service_can_be_disabled_or_enabled() {
        let (db, services, _, _, versions) = store().await;
        let alice = caller("u-1", "alice");
        let id = service(&db, &services, &alice).await;

        // A draft has never run, so there is nothing to take down.
        let err = versions.disable(&alice, &id).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::MsServiceStatusInvalid);
        assert_eq!(
            err.message(),
            "Invalid status transition for MCP service: \
             Only PUBLISHED service can be disabled, current: DRAFT"
        );

        versions
            .publish(&alice, &id, &PublishRequest::default())
            .await
            .unwrap();
        assert_eq!(versions.disable(&alice, &id).await.unwrap(), id);
        assert_eq!(
            services.load(&id).await.unwrap().status,
            Some(McpServiceStatus::Disabled)
        );

        // Disabling an already-disabled service, or enabling one that is running, both fail.
        let err = versions.disable(&alice, &id).await.unwrap_err();
        assert!(err.message().contains("current: DISABLED"));
        assert_eq!(versions.enable(&alice, &id).await.unwrap(), id);
        assert_eq!(
            services.load(&id).await.unwrap().status,
            Some(McpServiceStatus::Published)
        );
        let err = versions.enable(&alice, &id).await.unwrap_err();
        assert!(
            err.message()
                .contains("Only DISABLED service can be enabled, current: PUBLISHED")
        );
    }

    #[tokio::test]
    async fn publishing_a_disabled_service_leaves_it_disabled() {
        let (db, services, _, _, versions) = store().await;
        let alice = caller("u-1", "alice");
        let id = service(&db, &services, &alice).await;
        versions
            .publish(&alice, &id, &PublishRequest::default())
            .await
            .unwrap();
        versions.disable(&alice, &id).await.unwrap();

        versions
            .publish(&alice, &id, &PublishRequest::default())
            .await
            .unwrap();
        assert_eq!(
            services.load(&id).await.unwrap().status,
            Some(McpServiceStatus::Disabled),
            "publishing cuts a new version but does not bring the service back online"
        );
    }

    #[tokio::test]
    async fn a_diff_compares_the_draft_against_the_running_version() {
        let (db, services, tools, _, versions) = store().await;
        let alice = caller("u-1", "alice");
        let id = service(&db, &services, &alice).await;

        let initial = versions.diff(&alice, &id).await.unwrap();
        assert!(initial.has_changes);
        assert_eq!(
            initial.modified_components,
            vec!["INITIAL_RELEASE".to_string()]
        );
        assert_eq!(initial.active_version_number, None);

        versions
            .publish(&alice, &id, &PublishRequest::default())
            .await
            .unwrap();
        let clean = versions.diff(&alice, &id).await.unwrap();
        assert!(!clean.has_changes);
        assert!(clean.modified_components.is_empty());
        assert_eq!(clean.active_version_number, Some(1));

        // A new tool and a changed profile both show up.
        add_tool(&tools, &alice, &id, "by_date").await;
        services
            .update(
                &alice,
                &id,
                &McpServiceUpdateRequest {
                    name: None,
                    description: Some("now described".into()),
                    updated_by: None,
                },
            )
            .await
            .unwrap();
        let dirty = versions.diff(&alice, &id).await.unwrap();
        assert!(dirty.has_changes);
        assert!(dirty.basic_info_changed);
        assert!(dirty.tools_changed);
        assert_eq!(dirty.added_tools, vec!["by_date".to_string()]);
        assert!(
            dirty
                .modified_components
                .contains(&"BASIC_INFO".to_string())
        );
        assert!(dirty.modified_components.contains(&"TOOLS".to_string()));
    }

    #[tokio::test]
    async fn a_rollback_restores_an_older_version_as_a_new_one() {
        let (db, services, tools, prompts, versions) = store().await;
        let alice = caller("u-1", "alice");
        let id = service(&db, &services, &alice).await;
        add_tool(&tools, &alice, &id, "by_city").await;
        add_prompt(&prompts, &alice, &id, "weekly").await;
        versions
            .publish(&alice, &id, &PublishRequest::default())
            .await
            .unwrap();

        // Drift the draft: add a tool, rename the prompt, rename the service.
        add_tool(&tools, &alice, &id, "by_date").await;
        let prompt_id = prompts.stored(&id).await.unwrap()[0].id.clone();
        prompts
            .update(
                &alice,
                &id,
                &prompt_id,
                &crate::model::mcp_prompt::McpPromptRequest {
                    name: Some("renamed".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        services
            .update(
                &alice,
                &id,
                &McpServiceUpdateRequest {
                    name: Some("改名".into()),
                    description: None,
                    updated_by: None,
                },
            )
            .await
            .unwrap();

        let new_snapshot = versions
            .rollback(
                &alice,
                &id,
                &RollbackRequest {
                    target_version_number: Some(1),
                    release_note: None,
                },
            )
            .await
            .unwrap();

        // The live configuration is v1's again, but as a brand new version 2.
        let service = services.load(&id).await.unwrap();
        assert_eq!(
            service.name.as_deref(),
            Some("销售服务"),
            "the profile rolled back too"
        );
        assert_eq!(service.active_version_number, Some(2));
        assert_eq!(
            service.active_version_id.as_deref(),
            Some(new_snapshot.as_str())
        );
        assert_eq!(
            service.status,
            Some(McpServiceStatus::Published),
            "a rollback republishes, so a running service stays running"
        );

        let names: Vec<String> = tools
            .custom_tools(&id)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name.unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["by_city".to_string()],
            "the tool added after v1 is gone"
        );
        assert_eq!(prompts.stored(&id).await.unwrap().len(), 1);
        assert_eq!(
            prompts.stored(&id).await.unwrap()[0].name.as_deref(),
            Some("weekly")
        );

        let restored = mcp_snapshot::find(&db, &id, 2).await.unwrap().unwrap();
        assert_eq!(restored.release_note.as_deref(), Some("Rollback to v1"));

        // And the draft now matches the running version.
        assert!(!versions.diff(&alice, &id).await.unwrap().has_changes);
    }

    #[tokio::test]
    async fn a_rollback_validates_its_target() {
        let (db, services, _, _, versions) = store().await;
        let alice = caller("u-1", "alice");
        let id = service(&db, &services, &alice).await;

        let err = versions
            .rollback(&alice, &id, &RollbackRequest::default())
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidParameter);

        versions
            .publish(&alice, &id, &PublishRequest::default())
            .await
            .unwrap();
        let err = versions
            .rollback(
                &alice,
                &id,
                &RollbackRequest {
                    target_version_number: Some(5),
                    release_note: Some("note".into()),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::MsServiceVersionNotFound);
        assert_eq!(
            err.message(),
            format!("MCP service version not found: v5 of service {id}")
        );
        // A denial never reaches the version lookup.
        let bob = caller("u-2", "bob");
        assert_eq!(
            versions
                .rollback(
                    &bob,
                    &id,
                    &RollbackRequest {
                        target_version_number: Some(1),
                        release_note: None
                    }
                )
                .await
                .unwrap_err()
                .code(),
            ErrorCode::PermissionDenied
        );
    }

    #[tokio::test]
    async fn snapshots_are_listed_newest_first_with_author_names() {
        let (db, services, _, _, versions) = store().await;
        let alice = caller("u-1", "alice");
        let id = service(&db, &services, &alice).await;
        versions
            .publish(
                &alice,
                &id,
                &PublishRequest {
                    release_note: Some("  first  ".into()),
                },
            )
            .await
            .unwrap();
        versions
            .publish(&alice, &id, &PublishRequest::default())
            .await
            .unwrap();

        let listed = versions.snapshots(&alice, &id).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].version_number, 2, "newest first");
        assert_eq!(listed[0].release_note, None);
        assert_eq!(
            listed[1].release_note.as_deref(),
            Some("first"),
            "the note is trimmed on store"
        );
        assert_eq!(listed[1].created_user_name.as_deref(), Some("alice"));

        let bob = caller("u-2", "bob");
        assert_eq!(
            versions.snapshots(&bob, &id).await.unwrap_err().code(),
            ErrorCode::PermissionDenied
        );
    }
}
