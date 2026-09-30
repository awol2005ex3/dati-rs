//! The `mcp_metadata_audit_log` table. Nothing reads it back through an endpoint — the Java
//! version only ever wrote a row when a metadata tool changed something.

use crate::db::Db;
use crate::error::Result;
use crate::model::mcp_audit::MetadataAuditLog;

use super::Statement;

const TABLE: &str = "mcp_metadata_audit_log";

pub async fn insert(db: &Db, row: &MetadataAuditLog) -> Result<()> {
    Statement::new(format!(
        "INSERT INTO {TABLE} (id, created_by, created_at, updated_by, updated_at, service_id, \
         tool_type, entity_type, entity_id, entity_name, change_type, old_value, new_value) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    ))
    .bind(row.id.clone())
    .bind(row.audit.created_by.clone())
    .bind(row.audit.created_at.clone())
    .bind(row.audit.updated_by.clone())
    .bind(row.audit.updated_at.clone())
    .bind(row.service_id.clone())
    .bind(row.tool_type.clone())
    .bind(row.entity_type.clone())
    .bind(row.entity_id.clone())
    .bind(row.entity_name.clone())
    .bind(row.change_type.clone())
    .bind(row.old_value.clone())
    .bind(row.new_value.clone())
    .execute(db)
    .await?;
    Ok(())
}
