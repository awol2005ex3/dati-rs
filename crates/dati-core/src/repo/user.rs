//! The `users` table.

use sqlx::Row;

use super::{optional, required};
use crate::db::Db;
use crate::error::Result;
use crate::model::base::Audit;
use crate::model::user::User;

const COLUMNS: &str =
    "id, name, password, display_name, created_by, created_at, updated_by, updated_at";

pub fn to_user(row: &sqlx::any::AnyRow) -> Result<User> {
    Ok(User {
        id: required(row, "id")?,
        name: required(row, "name")?,
        password: required(row, "password")?,
        display_name: optional(row, "display_name"),
        audit: Audit {
            created_by: optional(row, "created_by"),
            created_at: optional(row, "created_at"),
            updated_by: optional(row, "updated_by"),
            updated_at: optional(row, "updated_at"),
        },
    })
}

pub async fn insert(db: &Db, user: &User) -> Result<()> {
    let sql = db.sql(
        "INSERT INTO users (id, created_by, created_at, updated_by, updated_at, name, password, display_name) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    );
    sqlx::query(&sql)
        .bind(&user.id)
        .bind(&user.audit.created_by)
        .bind(&user.audit.created_at)
        .bind(&user.audit.updated_by)
        .bind(&user.audit.updated_at)
        .bind(&user.name)
        .bind(&user.password)
        .bind(&user.display_name)
        .execute(db.pool())
        .await
        .map_err(|err| crate::db::context_with(err, "user name"))?;
    Ok(())
}

pub async fn find_by_name(db: &Db, name: &str) -> Result<Option<User>> {
    let sql = db.sql(&format!("SELECT {COLUMNS} FROM users WHERE name = ?"));
    let row = sqlx::query(&sql)
        .bind(name)
        .fetch_optional(db.pool())
        .await?;
    row.as_ref().map(to_user).transpose()
}

pub async fn find_by_id(db: &Db, id: &str) -> Result<Option<User>> {
    let sql = db.sql(&format!("SELECT {COLUMNS} FROM users WHERE id = ?"));
    let row = sqlx::query(&sql).bind(id).fetch_optional(db.pool()).await?;
    row.as_ref().map(to_user).transpose()
}

/// `GET /v1/users/search`: an empty keyword yields nothing rather than the full table.
pub async fn search(db: &Db, keyword: &str, limit: i64) -> Result<Vec<User>> {
    let needle = format!("%{}%", keyword.to_lowercase());
    let sql = db.sql(&format!(
        "SELECT {COLUMNS} FROM users WHERE lower(name) LIKE ? OR lower(display_name) LIKE ? LIMIT ?"
    ));
    let rows = sqlx::query(&sql)
        .bind(&needle)
        .bind(&needle)
        .bind(limit)
        .fetch_all(db.pool())
        .await?;
    rows.iter().map(to_user).collect()
}

pub async fn find_by_ids(db: &Db, ids: &[String]) -> Result<Vec<User>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; ids.len()].join(", ");
    let sql = db.sql(&format!(
        "SELECT {COLUMNS} FROM users WHERE id IN ({placeholders})"
    ));
    let mut query = sqlx::query(&sql);
    for id in ids {
        query = query.bind(id);
    }
    let rows = query.fetch_all(db.pool()).await?;
    rows.iter().map(to_user).collect()
}

/// Ids to the label the UI shows for a person, for the `created_user_name` fields every list
/// endpoint returns. Unknown ids are simply absent, leaving the field null.
pub async fn labels(db: &Db, ids: &[String]) -> Result<std::collections::HashMap<String, String>> {
    let users = find_by_ids(db, ids).await?;
    Ok(users
        .iter()
        .map(|user| (user.id.clone(), user.label()))
        .collect())
}

pub async fn count(db: &Db) -> Result<i64> {
    let sql = db.sql("SELECT COUNT(*) AS total FROM users");
    let row = sqlx::query(&sql).fetch_one(db.pool()).await?;
    Ok(row.try_get::<i64, _>("total").unwrap_or_default())
}
