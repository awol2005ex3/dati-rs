//! The `resource_acl` table.

use super::{optional, required};
use crate::db::Db;
use crate::error::Result;
use crate::model::base::Audit;
use crate::model::permission::{AclEntry, Permission, PrincipalType, ResourceType};

const COLUMNS: &str = "id, resource_type, resource_id, principal_type, principal_id, permission, \
     created_by, created_at, updated_by, updated_at";

fn to_entry(row: &sqlx::any::AnyRow) -> Result<AclEntry> {
    let resource_type = optional(row, "resource_type")
        .and_then(|v| ResourceType::from_stored(&v))
        .ok_or_else(|| {
            crate::error::DatiError::with_message(
                crate::error::ErrorCode::InternalError,
                "unknown acl resource type",
            )
        })?;
    let principal_type = optional(row, "principal_type")
        .and_then(|v| PrincipalType::from_stored(&v))
        .unwrap_or(PrincipalType::User);
    let permission = optional(row, "permission")
        .and_then(|v| Permission::from_stored(&v))
        .unwrap_or(Permission::View);
    Ok(AclEntry {
        id: required(row, "id")?,
        resource_type,
        resource_id: required(row, "resource_id")?,
        principal_type,
        principal_id: required(row, "principal_id")?,
        permission,
        audit: Audit {
            created_by: optional(row, "created_by"),
            created_at: optional(row, "created_at"),
            updated_by: optional(row, "updated_by"),
            updated_at: optional(row, "updated_at"),
        },
    })
}

pub async fn list(
    db: &Db,
    resource_type: ResourceType,
    resource_id: &str,
) -> Result<Vec<AclEntry>> {
    let sql = db.sql(&format!(
        "SELECT {COLUMNS} FROM resource_acl WHERE resource_type = ? AND resource_id = ? ORDER BY created_at ASC"
    ));
    let rows = sqlx::query(&sql)
        .bind(resource_type.as_str())
        .bind(resource_id)
        .fetch_all(db.pool())
        .await?;
    rows.iter().map(to_entry).collect()
}

pub async fn find(
    db: &Db,
    resource_type: ResourceType,
    resource_id: &str,
    principal_type: PrincipalType,
    principal_id: &str,
) -> Result<Option<AclEntry>> {
    let sql = db.sql(&format!(
        "SELECT {COLUMNS} FROM resource_acl \
         WHERE resource_type = ? AND resource_id = ? AND principal_type = ? AND principal_id = ?"
    ));
    let row = sqlx::query(&sql)
        .bind(resource_type.as_str())
        .bind(resource_id)
        .bind(principal_type.as_str())
        .bind(principal_id)
        .fetch_optional(db.pool())
        .await?;
    row.as_ref().map(to_entry).transpose()
}

pub async fn insert(db: &Db, entry: &AclEntry) -> Result<()> {
    let sql = db.sql(
        "INSERT INTO resource_acl (id, created_by, created_at, updated_by, updated_at, \
         resource_type, resource_id, principal_type, principal_id, permission) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    );
    sqlx::query(&sql)
        .bind(&entry.id)
        .bind(&entry.audit.created_by)
        .bind(&entry.audit.created_at)
        .bind(&entry.audit.updated_by)
        .bind(&entry.audit.updated_at)
        .bind(entry.resource_type.as_str())
        .bind(&entry.resource_id)
        .bind(entry.principal_type.as_str())
        .bind(&entry.principal_id)
        .bind(entry.permission.as_str())
        .execute(db.pool())
        .await?;
    Ok(())
}

pub async fn update_permission(db: &Db, id: &str, permission: Permission, who: &str) -> Result<()> {
    let sql = db
        .sql("UPDATE resource_acl SET permission = ?, updated_by = ?, updated_at = ? WHERE id = ?");
    sqlx::query(&sql)
        .bind(permission.as_str())
        .bind(who)
        .bind(crate::time::now_iso())
        .bind(id)
        .execute(db.pool())
        .await?;
    Ok(())
}

pub async fn delete(
    db: &Db,
    resource_type: ResourceType,
    resource_id: &str,
    principal_type: PrincipalType,
    principal_id: &str,
) -> Result<bool> {
    let sql = db.sql(
        "DELETE FROM resource_acl \
         WHERE resource_type = ? AND resource_id = ? AND principal_type = ? AND principal_id = ?",
    );
    let result = sqlx::query(&sql)
        .bind(resource_type.as_str())
        .bind(resource_id)
        .bind(principal_type.as_str())
        .bind(principal_id)
        .execute(db.pool())
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn delete_for_resource(
    db: &Db,
    resource_type: ResourceType,
    resource_id: &str,
) -> Result<u64> {
    let sql = db.sql("DELETE FROM resource_acl WHERE resource_type = ? AND resource_id = ?");
    let result = sqlx::query(&sql)
        .bind(resource_type.as_str())
        .bind(resource_id)
        .execute(db.pool())
        .await?;
    Ok(result.rows_affected())
}
