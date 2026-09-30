//! The published snapshots of an MCP service: the serialized configuration a version froze, the
//! requests that publish or roll one back, and the two read shapes the editor consumes.
//!
//! A snapshot is a whole-configuration document stored as text in `mcp_service_snapshot`. The
//! sub-records deliberately carry no id or audit columns — restoring one is always an insert, which
//! is what avoids the Java port's stale-update bug when a restored row would collide by name.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{DatiError, Result};
use crate::model::mcp::{McpDataScopeType, McpToolType};
use crate::model::mcp_prompt::PromptParameter;
use crate::model::mcp_tool::{McpCustomTool, McpPrebuiltToolConfig, ToolConfig};

/// The service profile frozen into a snapshot. `id` is kept only as a historical marker; restore
/// writes the profile back onto the service it came from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServiceInfo {
    pub id: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotScope {
    pub service_id: String,
    pub scope_type: McpDataScopeType,
    pub reference_id: String,
}

/// A prebuilt slot's mutable state. The config is an object whose concrete shape is keyed by
/// `tool_type`, exactly as the stored column held it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotPrebuilt {
    pub service_id: String,
    pub tool_type: McpToolType,
    pub enabled: bool,
    pub config: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotCustom {
    pub service_id: String,
    pub name: Option<String>,
    pub tool_type: McpToolType,
    pub title: Option<String>,
    pub description: Option<String>,
    pub enabled: bool,
    pub config: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotPrompt {
    pub service_id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub enabled: bool,
    pub content: Option<String>,
    #[serde(default)]
    pub parameters: Vec<PromptParameter>,
}

/// The whole document serialized into `snapshot_content`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SnapshotContent {
    pub service_info: ServiceInfo,
    pub data_scopes: Vec<SnapshotScope>,
    pub prebuilt_tools: Vec<SnapshotPrebuilt>,
    pub custom_tools: Vec<SnapshotCustom>,
    pub prompts: Vec<SnapshotPrompt>,
}

impl SnapshotPrebuilt {
    /// The slot as the mutable store keeps it; the id is regenerated on insert.
    pub fn to_config(&self) -> Result<McpPrebuiltToolConfig> {
        Ok(McpPrebuiltToolConfig {
            id: String::new(),
            service_id: self.service_id.clone(),
            tool_type: self.tool_type,
            enabled: self.enabled,
            config: tool_config(self.tool_type, &self.config)?,
        })
    }
}

impl SnapshotCustom {
    /// The custom tool as stored; a fresh id is assigned on insert, which is what makes a restore
    /// collision-free.
    pub fn to_tool(&self) -> Result<McpCustomTool> {
        Ok(McpCustomTool {
            id: String::new(),
            service_id: self.service_id.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            title: self.title.clone(),
            tool_type: self.tool_type,
            enabled: self.enabled,
            config: Some(tool_config(self.tool_type, &self.config)?),
        })
    }
}

/// Reads a snapshot's config object as the concrete config of its tool type, mirroring the Java
/// `deserializeConfig` routing on `tool_type`.
fn tool_config(tool_type: McpToolType, value: &Value) -> Result<ToolConfig> {
    ToolConfig::parse(tool_type, &value.to_string())
}

/// A stored snapshot row. `content` is the parsed document; the raw text lives only in the store.
#[derive(Debug, Clone, PartialEq)]
pub struct McpServiceSnapshot {
    pub id: String,
    pub service_id: String,
    pub version_number: i64,
    pub release_note: Option<String>,
    pub content: SnapshotContent,
    pub audit: crate::model::base::Audit,
}

/// `POST /{id}/publish`: the body is optional and unvalidated, so a blank note is simply stored.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PublishRequest {
    pub release_note: Option<String>,
}

/// `POST /{id}/rollback`: `@NotNull` on the target version, matching the Java request.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RollbackRequest {
    pub target_version_number: Option<i64>,
    pub release_note: Option<String>,
}

impl RollbackRequest {
    pub fn validate(&self) -> Result<()> {
        if self.target_version_number.is_none() {
            return Err(DatiError::validation(&[(
                "target_version_number",
                "must not be null",
            )]));
        }
        Ok(())
    }
}

/// `McpServiceSnapshotVO`: the audit columns plus the version identity, never the content. The
/// `name`/`description` are the inherited base columns, which a snapshot row never populates.
#[derive(Debug, Clone, Serialize)]
pub struct McpServiceSnapshotVO {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub created_by: Option<String>,
    pub created_at: Option<String>,
    pub updated_by: Option<String>,
    pub updated_at: Option<String>,
    pub created_user_name: Option<String>,
    pub updated_user_name: Option<String>,
    pub service_id: String,
    pub version_number: i64,
    pub release_note: Option<String>,
}

impl McpServiceSnapshotVO {
    pub fn new(
        snapshot: &McpServiceSnapshot,
        names: &std::collections::HashMap<String, String>,
    ) -> Self {
        let (created_user_name, updated_user_name) =
            crate::model::base::author_names(&snapshot.audit, names);
        Self {
            id: snapshot.id.clone(),
            name: None,
            description: None,
            created_by: snapshot.audit.created_by.clone(),
            created_at: snapshot.audit.created_at.clone(),
            updated_by: snapshot.audit.updated_by.clone(),
            updated_at: snapshot.audit.updated_at.clone(),
            created_user_name,
            updated_user_name,
            service_id: snapshot.service_id.clone(),
            version_number: snapshot.version_number,
            release_note: snapshot.release_note.clone(),
        }
    }
}

/// `McpServiceDiffVO`: what differs between the active snapshot and the current draft.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct McpServiceDiffVO {
    pub has_changes: bool,
    pub active_version_number: Option<i64>,
    pub modified_components: Vec<String>,
    pub basic_info_changed: bool,
    pub data_scope_changed: bool,
    pub tools_changed: bool,
    pub prompts_changed: bool,
    pub added_tools: Vec<String>,
    pub modified_tools: Vec<String>,
    pub deleted_tools: Vec<String>,
    pub added_prompts: Vec<String>,
    pub modified_prompts: Vec<String>,
    pub deleted_prompts: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::mcp::McpDataScopeType;
    use serde_json::json;

    #[test]
    fn a_rollback_names_a_target_version_and_a_publish_does_not() {
        let err = RollbackRequest::default().validate().unwrap_err();
        assert_eq!(
            err.message(),
            "Request parameter error: target_version_number: must not be null"
        );
        RollbackRequest {
            target_version_number: Some(2),
            release_note: None,
        }
        .validate()
        .unwrap();

        let request: PublishRequest = serde_json::from_value(json!({})).unwrap();
        assert_eq!(request.release_note, None, "an empty body still publishes");
        let request: PublishRequest =
            serde_json::from_value(json!({ "release_note": "first" })).unwrap();
        assert_eq!(request.release_note.as_deref(), Some("first"));
    }

    fn content() -> SnapshotContent {
        SnapshotContent {
            service_info: ServiceInfo {
                id: Some("svc".into()),
                name: Some("销售".into()),
                description: None,
                code: Some("sales".into()),
            },
            data_scopes: vec![SnapshotScope {
                service_id: "svc".into(),
                scope_type: McpDataScopeType::DataSource,
                reference_id: "d-1".into(),
            }],
            prebuilt_tools: vec![SnapshotPrebuilt {
                service_id: "svc".into(),
                tool_type: McpToolType::ExecuteSql,
                enabled: true,
                config: json!({ "timeout": 5, "max_rows": 10 }),
            }],
            custom_tools: vec![SnapshotCustom {
                service_id: "svc".into(),
                name: Some("by_city".into()),
                tool_type: McpToolType::ParameterizedSql,
                title: None,
                description: Some("d".into()),
                enabled: true,
                config: json!({ "sql_template": "SELECT 1" }),
            }],
            prompts: vec![SnapshotPrompt {
                service_id: "svc".into(),
                name: Some("weekly".into()),
                description: None,
                enabled: true,
                content: Some("hi {{city}}".into()),
                parameters: vec![PromptParameter {
                    name: Some("city".into()),
                    description: None,
                    required: false,
                }],
            }],
        }
    }

    #[test]
    fn the_snapshot_document_is_stored_and_read_as_one_json_value() {
        let original = content();
        let stored = serde_json::to_string(&original).unwrap();
        let read_back: SnapshotContent = serde_json::from_str(&stored).unwrap();
        assert_eq!(read_back, original);
        // Every collection lives under the snake_case key the Java `SnapshotContent` used.
        let value: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(value["service_info"]["code"], "sales");
        assert_eq!(value["data_scopes"][0]["scope_type"], "DATA_SOURCE");
    }

    #[test]
    fn a_restored_slot_and_tool_rebuild_their_configs_by_tool_type() {
        let restored = content();
        let slot = restored.prebuilt_tools[0].to_config().unwrap();
        assert_eq!(slot.id, "", "a restore regenerates the row id");
        let execute = slot.config.as_execute_sql().unwrap();
        assert_eq!(execute.timeout, 5);
        assert_eq!(execute.max_rows, 10);
        assert!(
            execute.sql_policy.allow_select,
            "a stored policy that omits keys keeps the defaults"
        );

        let tool = restored.custom_tools[0].to_tool().unwrap();
        assert_eq!(tool.name.as_deref(), Some("by_city"));
        let config = tool.config.unwrap();
        assert_eq!(
            config.as_param_sql().unwrap().sql_template.as_deref(),
            Some("SELECT 1")
        );
    }
}
