//! The `api_key` table: only the SHA-256 digest of a key is persisted.

use super::{optional, required};
use crate::db::Db;
use crate::error::Result;
use crate::model::base::Audit;
use crate::model::user::ApiKey;

const COLUMNS: &str = "id, user_id, name, key_hash, key_mask, expires_at, last_used_at, \
     created_by, created_at, updated_by, updated_at";

fn to_key(row: &sqlx::any::AnyRow) -> Result<ApiKey> {
    Ok(ApiKey {
        id: required(row, "id")?,
        user_id: required(row, "user_id")?,
        name: optional(row, "name").unwrap_or_default(),
        key_hash: required(row, "key_hash")?,
        key_mask: optional(row, "key_mask").unwrap_or_default(),
        expires_at: optional(row, "expires_at"),
        last_used_at: optional(row, "last_used_at"),
        audit: Audit {
            created_by: optional(row, "created_by"),
            created_at: optional(row, "created_at"),
            updated_by: optional(row, "updated_by"),
            updated_at: optional(row, "updated_at"),
        },
    })
}

pub async fn insert(db: &Db, key: &ApiKey) -> Result<()> {
    let sql = db.sql(
        "INSERT INTO api_key (id, created_by, created_at, updated_by, updated_at, user_id, name, \
         key_hash, key_mask, expires_at, last_used_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    );
    sqlx::query(&sql)
        .bind(&key.id)
        .bind(&key.audit.created_by)
        .bind(&key.audit.created_at)
        .bind(&key.audit.updated_by)
        .bind(&key.audit.updated_at)
        .bind(&key.user_id)
        .bind(&key.name)
        .bind(&key.key_hash)
        .bind(&key.key_mask)
        .bind(&key.expires_at)
        .bind(&key.last_used_at)
        .execute(db.pool())
        .await?;
    Ok(())
}

pub async fn find_by_hash(db: &Db, key_hash: &str) -> Result<Option<ApiKey>> {
    let sql = db.sql(&format!("SELECT {COLUMNS} FROM api_key WHERE key_hash = ?"));
    let row = sqlx::query(&sql)
        .bind(key_hash)
        .fetch_optional(db.pool())
        .await?;
    row.as_ref().map(to_key).transpose()
}

/// Newest first, which is what the settings screen lists.
pub async fn list_for_user(db: &Db, user_id: &str) -> Result<Vec<ApiKey>> {
    let sql = db.sql(&format!(
        "SELECT {COLUMNS} FROM api_key WHERE user_id = ? ORDER BY created_at DESC"
    ));
    let rows = sqlx::query(&sql).bind(user_id).fetch_all(db.pool()).await?;
    rows.iter().map(to_key).collect()
}

pub async fn find_by_id(db: &Db, id: &str) -> Result<Option<ApiKey>> {
    let sql = db.sql(&format!("SELECT {COLUMNS} FROM api_key WHERE id = ?"));
    let row = sqlx::query(&sql).bind(id).fetch_optional(db.pool()).await?;
    row.as_ref().map(to_key).transpose()
}

pub async fn delete(db: &Db, id: &str) -> Result<bool> {
    let sql = db.sql("DELETE FROM api_key WHERE id = ?");
    let result = sqlx::query(&sql).bind(id).execute(db.pool()).await?;
    Ok(result.rows_affected() > 0)
}

/// Best-effort telemetry: a failure here must not fail the request being authenticated.
pub async fn touch_used(db: &Db, id: &str, used_at: &str) {
    let sql = db.sql("UPDATE api_key SET last_used_at = ?, updated_at = ? WHERE id = ?");
    if let Err(err) = sqlx::query(&sql)
        .bind(used_at)
        .bind(used_at)
        .bind(id)
        .execute(db.pool())
        .await
    {
        tracing::warn!(error = %err, "cannot record API key usage");
    }
}
