//! The `mcp_metadata_audit_log` table: what a metadata-writing tool changed, one row per item.

use serde::Serialize;

use crate::model::base::Audit;

/// One recorded write. `old_value` and `new_value` are the `{description, aliases}` documents as
/// the Java executor serialized them, so they stay raw JSON text here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MetadataAuditLog {
    pub id: String,
    pub audit: Audit,
    pub service_id: String,
    pub tool_type: String,
    pub entity_type: String,
    pub entity_id: String,
    pub entity_name: String,
    pub change_type: String,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
}
