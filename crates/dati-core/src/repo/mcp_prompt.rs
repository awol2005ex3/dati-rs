//! The `mcp_prompt` table: a text template a service exposes through `prompts/list`.

use sqlx::Row;
use sqlx::any::AnyRow;

use crate::db::Db;
use crate::error::Result;
use crate::model::base::Audit;
use crate::model::mcp_prompt::{McpPrompt, PromptParameter};

use super::{Statement, flag, optional, required};

const TABLE: &str = "mcp_prompt";

const COLUMNS: &str = "id, created_by, created_at, updated_by, updated_at, service_id, name, \
     description, enabled, content, parameters";

/// The Java reader swallowed any parse failure and reported no parameters at all, so a corrupt
/// column degrades to an empty list rather than an error.
fn to_parameters(row: &AnyRow) -> Vec<PromptParameter> {
    optional(row, "parameters")
        .and_then(|text| serde_json::from_str::<Vec<PromptParameter>>(&text).ok())
        .unwrap_or_default()
}

fn to_prompt(row: &AnyRow) -> Result<McpPrompt> {
    Ok(McpPrompt {
        id: required(row, "id")?,
        service_id: required(row, "service_id")?,
        name: optional(row, "name"),
        description: optional(row, "description"),
        enabled: flag(row, "enabled"),
        content: optional(row, "content"),
        parameters: to_parameters(row),
        audit: Audit {
            created_by: optional(row, "created_by"),
            created_at: optional(row, "created_at"),
            updated_by: optional(row, "updated_by"),
            updated_at: optional(row, "updated_at"),
        },
    })
}

/// The prompts of one service, newest first — the order the Java list query declared.
pub async fn list(db: &Db, service_id: &str) -> Result<Vec<McpPrompt>> {
    let rows = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE service_id = ? ORDER BY created_at DESC, id DESC"
    ))
    .bind(service_id)
    .fetch_all(db)
    .await?;
    rows.iter().map(to_prompt).collect()
}

pub async fn find(db: &Db, service_id: &str, id: &str) -> Result<Option<McpPrompt>> {
    let row = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE service_id = ? AND id = ?"
    ))
    .bind(service_id)
    .bind(id)
    .fetch_optional(db)
    .await?;
    row.as_ref().map(to_prompt).transpose()
}

/// Stores one prompt under the id the caller picked, so a snapshot restore can write back the row it
/// published with its original identity.
pub async fn insert(db: &Db, prompt: &McpPrompt) -> Result<String> {
    Statement::new(format!(
        "INSERT INTO {TABLE} (id, created_by, created_at, updated_by, updated_at, service_id, \
         name, description, enabled, content, parameters) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    ))
    .bind(prompt.id.clone())
    .bind(prompt.audit.created_by.clone())
    .bind(prompt.audit.created_at.clone())
    .bind(prompt.audit.updated_by.clone())
    .bind(prompt.audit.updated_at.clone())
    .bind(prompt.service_id.clone())
    .bind(prompt.name.clone())
    .bind(prompt.description.clone())
    .bind(prompt.enabled)
    .bind(prompt.content.clone())
    .bind(write_parameters(&prompt.parameters))
    .execute(db)
    .await?;
    Ok(prompt.id.clone())
}

/// Writes back every mutable column; the caller merged the request into the stored row first.
pub async fn update(db: &Db, prompt: &McpPrompt) -> Result<()> {
    Statement::new(format!(
        "UPDATE {TABLE} SET name = ?, description = ?, enabled = ?, content = ?, parameters = ?, \
         updated_by = ?, updated_at = ? WHERE id = ? AND service_id = ?"
    ))
    .bind(prompt.name.clone())
    .bind(prompt.description.clone())
    .bind(prompt.enabled)
    .bind(prompt.content.clone())
    .bind(write_parameters(&prompt.parameters))
    .bind(prompt.audit.updated_by.clone())
    .bind(prompt.audit.updated_at.clone())
    .bind(prompt.id.clone())
    .bind(prompt.service_id.clone())
    .execute(db)
    .await?;
    Ok(())
}

/// Prompt names are unique per service; the row the caller is editing itself does not collide.
pub async fn name_taken(
    db: &Db,
    service_id: &str,
    name: &str,
    except_id: Option<&str>,
) -> Result<bool> {
    let mut statement = Statement::new(format!(
        "SELECT COUNT(*) AS total FROM {TABLE} WHERE service_id = ? AND name = ?"
    ))
    .bind(service_id)
    .bind(name.to_string());
    if let Some(except_id) = except_id {
        statement = statement
            .push_text(" AND id <> ?")
            .bind(except_id.to_string());
    }
    let row = statement.fetch_one(db).await?;
    Ok(row.try_get::<i64, _>("total").unwrap_or_default() > 0)
}

pub async fn remove(db: &Db, service_id: &str, id: &str) -> Result<bool> {
    let affected = Statement::new(format!(
        "DELETE FROM {TABLE} WHERE service_id = ? AND id = ?"
    ))
    .bind(service_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(affected > 0)
}

/// Deleting a service took its prompts with it.
pub async fn remove_all(db: &Db, service_id: &str) -> Result<u64> {
    let affected = Statement::new(format!("DELETE FROM {TABLE} WHERE service_id = ?"))
        .bind(service_id)
        .execute(db)
        .await?;
    Ok(affected)
}

/// The parameter list as stored. An empty list is written as `[]` rather than left null, because a
/// null and an empty list read back the same way and the Java mapper stored whatever it was given.
fn write_parameters(parameters: &[PromptParameter]) -> String {
    serde_json::to_string(parameters).unwrap_or_else(|_| "[]".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(id: &str, service_id: &str, name: &str) -> McpPrompt {
        McpPrompt {
            id: id.to_string(),
            service_id: service_id.to_string(),
            name: Some(name.to_string()),
            description: Some("周报".into()),
            enabled: true,
            content: Some("统计 {{city}} 的订单".into()),
            parameters: vec![PromptParameter {
                name: Some("city".into()),
                description: None,
                required: true,
            }],
            audit: Audit::new("u-1"),
        }
    }

    #[tokio::test]
    async fn a_prompt_round_trips_through_its_json_parameter_column() {
        let db = Db::sqlite_temp().await;
        insert(&db, &prompt("p-1", "svc-1", "weekly"))
            .await
            .unwrap();

        let found = find(&db, "svc-1", "p-1").await.unwrap().unwrap();
        let written = prompt("p-1", "svc-1", "weekly");
        assert_eq!(found.id, written.id);
        assert_eq!(found.name, written.name);
        assert_eq!(found.description, written.description);
        assert_eq!(found.content, written.content);
        assert_eq!(found.parameters, written.parameters);
        assert_eq!(found.audit.created_by.as_deref(), Some("u-1"));
        assert!(find(&db, "svc-2", "p-1").await.unwrap().is_none());

        let mut changed = found.clone();
        changed.content = Some("SELECT 1".into());
        changed.parameters.clear();
        changed.enabled = false;
        changed.audit.touch("u-2");
        update(&db, &changed).await.unwrap();

        let found = find(&db, "svc-1", "p-1").await.unwrap().unwrap();
        assert!(!found.enabled);
        assert_eq!(found.content.as_deref(), Some("SELECT 1"));
        assert!(found.parameters.is_empty());
        assert_eq!(found.audit.created_by.as_deref(), Some("u-1"));
        assert_eq!(found.audit.updated_by.as_deref(), Some("u-2"));
    }

    #[tokio::test]
    async fn a_corrupt_parameter_column_reads_as_no_parameters() {
        let db = Db::sqlite_temp().await;
        Statement::new(
            "INSERT INTO mcp_prompt (id, service_id, name, enabled, content, parameters) \
             VALUES ('p-1', 'svc-1', 'weekly', 1, 'c', 'not json')",
        )
        .execute(&db)
        .await
        .unwrap();
        let found = find(&db, "svc-1", "p-1").await.unwrap().unwrap();
        assert!(found.parameters.is_empty());
        assert_eq!(found.description, None);
    }

    #[tokio::test]
    async fn names_are_unique_within_a_service_and_rows_are_hard_deleted() {
        let db = Db::sqlite_temp().await;
        insert(&db, &prompt("p-1", "svc-1", "weekly"))
            .await
            .unwrap();
        insert(&db, &prompt("p-2", "svc-2", "weekly"))
            .await
            .unwrap();

        assert!(name_taken(&db, "svc-1", "weekly", None).await.unwrap());
        assert!(
            !name_taken(&db, "svc-1", "weekly", Some("p-1"))
                .await
                .unwrap()
        );
        assert!(
            name_taken(&db, "svc-2", "weekly", Some("p-1"))
                .await
                .unwrap()
        );

        assert!(remove(&db, "svc-1", "p-1").await.unwrap());
        assert!(!remove(&db, "svc-1", "p-1").await.unwrap());
        assert_eq!(remove_all(&db, "svc-2").await.unwrap(), 1);
        assert!(list(&db, "svc-2").await.unwrap().is_empty());
    }
}
