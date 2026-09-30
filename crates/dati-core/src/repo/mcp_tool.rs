//! The two tool tables: `mcp_custom_tool`, whose rows an author writes, and
//! `mcp_prebuilt_tool_config`, which stores only the mutable settings of the seven fixed slots.

use std::collections::HashMap;

use sqlx::Row;
use sqlx::any::AnyRow;

use crate::db::Db;
use crate::error::{DatiError, ErrorCode, Result};
use crate::model::base::Audit;
use crate::model::mcp::{McpToolType, PREBUILT_TOOLS};
use crate::model::mcp_tool::{McpCustomTool, McpPrebuiltToolConfig, ToolConfig};

use super::{Statement, flag, optional, required};

const TABLE: &str = "mcp_custom_tool";

const COLUMNS: &str = "id, service_id, name, description, tool_type, title, enabled, config";

const PREBUILT: &str = "mcp_prebuilt_tool_config";

const PREBUILT_COLUMNS: &str = "id, service_id, tool_type, enabled, config";

/// The stored `config` column, or the slot's own defaults when nothing was written.
fn stored_config(tool_type: McpToolType, row: &AnyRow) -> Result<ToolConfig> {
    match optional(row, "config") {
        None => Ok(ToolConfig::default_for(tool_type)),
        Some(json) => ToolConfig::parse(tool_type, &json),
    }
}

fn tool_type_of(row: &AnyRow) -> Result<McpToolType> {
    let stored = required(row, "tool_type")?;
    McpToolType::from_stored(&stored).ok_or_else(|| {
        tracing::error!(stored, "tool row has an unknown tool type");
        DatiError::new(ErrorCode::InternalError, [stored])
    })
}

fn to_custom_tool(row: &AnyRow) -> Result<McpCustomTool> {
    let tool_type = tool_type_of(row)?;
    Ok(McpCustomTool {
        id: required(row, "id")?,
        service_id: required(row, "service_id")?,
        name: optional(row, "name"),
        description: optional(row, "description"),
        title: optional(row, "title"),
        tool_type,
        enabled: flag(row, "enabled"),
        config: Some(stored_config(tool_type, row)?),
    })
}

fn to_prebuilt(row: &AnyRow) -> Result<McpPrebuiltToolConfig> {
    let tool_type = tool_type_of(row)?;
    Ok(McpPrebuiltToolConfig {
        id: required(row, "id")?,
        service_id: required(row, "service_id")?,
        tool_type,
        enabled: flag(row, "enabled"),
        config: stored_config(tool_type, row)?,
    })
}

// ---- prebuilt slots ------------------------------------------------------------------------

/// Every stored slot. A slot the service never touched has no row at all, which is why the caller
/// fills the seven from [`prebuilt_one`] rather than from this list.
pub async fn prebuilt(db: &Db, service_id: &str) -> Result<Vec<McpPrebuiltToolConfig>> {
    let rows = Statement::new(format!(
        "SELECT {PREBUILT_COLUMNS} FROM {PREBUILT} WHERE service_id = ?"
    ))
    .bind(service_id)
    .fetch_all(db)
    .await?;
    rows.iter().map(to_prebuilt).collect()
}

pub async fn prebuilt_one(
    db: &Db,
    service_id: &str,
    tool_type: McpToolType,
) -> Result<Option<McpPrebuiltToolConfig>> {
    let row = Statement::new(format!(
        "SELECT {PREBUILT_COLUMNS} FROM {PREBUILT} WHERE service_id = ? AND tool_type = ?"
    ))
    .bind(service_id)
    .bind(tool_type.as_str())
    .fetch_optional(db)
    .await?;
    row.as_ref().map(to_prebuilt).transpose()
}

/// The Java save inserted a row the first time a slot was touched and updated it afterwards; the
/// unique index on `(service_id, tool_type)` is what makes the two a single upsert here.
pub async fn save_prebuilt(
    db: &Db,
    service_id: &str,
    tool_type: McpToolType,
    enabled: bool,
    config: &ToolConfig,
    audit: &Audit,
) -> Result<()> {
    if prebuilt_one(db, service_id, tool_type).await?.is_some() {
        Statement::new(format!(
            "UPDATE {PREBUILT} SET enabled = ?, config = ?, updated_by = ?, updated_at = ? \
             WHERE service_id = ? AND tool_type = ?"
        ))
        .bind(enabled)
        .bind(config.to_json())
        .bind(audit.updated_by.clone())
        .bind(audit.updated_at.clone())
        .bind(service_id)
        .bind(tool_type.as_str())
        .execute(db)
        .await?;
        return Ok(());
    }
    Statement::new(format!(
        "INSERT INTO {PREBUILT} (id, created_by, created_at, updated_by, updated_at, deleted, \
         service_id, tool_type, enabled, config) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    ))
    .bind(crate::time::new_id())
    .bind(audit.created_by.clone())
    .bind(audit.created_at.clone())
    .bind(audit.updated_by.clone())
    .bind(audit.updated_at.clone())
    .bind(false)
    .bind(service_id.to_string())
    .bind(tool_type.as_str().to_string())
    .bind(enabled)
    .bind(config.to_json())
    .execute(db)
    .await?;
    Ok(())
}

/// Deleting a service took its prebuilt slots with it.
pub async fn remove_all_prebuilt(db: &Db, service_id: &str) -> Result<u64> {
    let affected = Statement::new(format!("DELETE FROM {PREBUILT} WHERE service_id = ?"))
        .bind(service_id)
        .execute(db)
        .await?;
    Ok(affected)
}

// ---- custom tools --------------------------------------------------------------------------

/// `tool_count` on the service list: the Java assembler added the number of prebuilt tool slots —
/// all of them, whether enabled or not — to the custom tools stored for the service.
pub async fn counts_by_service(db: &Db, service_ids: &[String]) -> Result<HashMap<String, i64>> {
    let mut counts = HashMap::new();
    if service_ids.is_empty() {
        return Ok(counts);
    }
    let (clause, binds) = super::in_clause("service_id", service_ids);
    let mut statement = Statement::new(format!(
        "SELECT service_id, COUNT(*) AS total FROM {TABLE} WHERE {clause} GROUP BY service_id"
    ));
    for bind in binds {
        statement = statement.bind(bind);
    }
    for row in statement.fetch_all(db).await? {
        let service_id = required(&row, "service_id")?;
        let custom = row
            .try_get::<i64, _>("total")
            .or_else(|_| row.try_get::<i32, _>("total").map(i64::from))
            .unwrap_or_default();
        counts.insert(service_id, custom);
    }
    Ok(counts)
}

/// The count one service reports, which is what `GET /v1/mcp-services/{id}` needs.
pub fn total(counts: &HashMap<String, i64>, service_id: &str) -> i64 {
    PREBUILT_TOOLS.len() as i64 + counts.get(service_id).copied().unwrap_or_default()
}

/// The tools of one service, newest first — the order the Java list query declared.
pub async fn custom(db: &Db, service_id: &str) -> Result<Vec<McpCustomTool>> {
    let rows = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE service_id = ? ORDER BY created_at DESC, id DESC"
    ))
    .bind(service_id)
    .fetch_all(db)
    .await?;
    rows.iter().map(to_custom_tool).collect()
}

pub async fn find_custom(db: &Db, service_id: &str, id: &str) -> Result<Option<McpCustomTool>> {
    let row = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE service_id = ? AND id = ?"
    ))
    .bind(service_id)
    .bind(id)
    .fetch_optional(db)
    .await?;
    row.as_ref().map(to_custom_tool).transpose()
}

/// Stores one tool under the id the caller picked, so a snapshot restore can write back the row it
/// published with its original identity.
pub async fn insert_custom(db: &Db, tool: &McpCustomTool, audit: &Audit) -> Result<String> {
    Statement::new(format!(
        "INSERT INTO {TABLE} (id, created_by, created_at, updated_by, updated_at, service_id, \
         name, description, tool_type, title, enabled, config) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    ))
    .bind(tool.id.clone())
    .bind(audit.created_by.clone())
    .bind(audit.created_at.clone())
    .bind(audit.updated_by.clone())
    .bind(audit.updated_at.clone())
    .bind(tool.service_id.clone())
    .bind(tool.name.clone())
    .bind(tool.description.clone())
    .bind(tool.tool_type.as_str().to_string())
    .bind(tool.title.clone())
    .bind(tool.enabled)
    .bind(tool.config.as_ref().map(ToolConfig::to_json))
    .execute(db)
    .await?;
    Ok(tool.id.clone())
}

/// Writes back every mutable column; the caller merged the request into the stored row first,
/// which is what the Java service did field by field.
pub async fn update_custom(db: &Db, tool: &McpCustomTool, audit: &Audit) -> Result<()> {
    Statement::new(format!(
        "UPDATE {TABLE} SET name = ?, description = ?, tool_type = ?, title = ?, enabled = ?, \
         config = ?, updated_by = ?, updated_at = ? WHERE id = ? AND service_id = ?"
    ))
    .bind(tool.name.clone())
    .bind(tool.description.clone())
    .bind(tool.tool_type.as_str().to_string())
    .bind(tool.title.clone())
    .bind(tool.enabled)
    .bind(tool.config.as_ref().map(ToolConfig::to_json))
    .bind(audit.updated_by.clone())
    .bind(audit.updated_at.clone())
    .bind(tool.id.clone())
    .bind(tool.service_id.clone())
    .execute(db)
    .await?;
    Ok(())
}

/// Tool names are unique per service; the row the caller is editing itself does not collide.
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

pub async fn delete_custom(db: &Db, service_id: &str, id: &str) -> Result<bool> {
    let affected = Statement::new(format!(
        "DELETE FROM {TABLE} WHERE service_id = ? AND id = ?"
    ))
    .bind(service_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(affected > 0)
}

/// Deleting a service took its custom tools with it, without a cascade of its own.
pub async fn remove_all(db: &Db, service_id: &str) -> Result<u64> {
    let affected = Statement::new(format!("DELETE FROM {TABLE} WHERE service_id = ?"))
        .bind(service_id)
        .execute(db)
        .await?;
    Ok(affected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::mcp_tool::ParamSqlConfig;

    async fn store() -> Db {
        Db::sqlite_temp().await
    }

    fn audit() -> Audit {
        Audit::new("u-1")
    }

    fn tool(id: &str, service_id: &str, name: &str) -> McpCustomTool {
        McpCustomTool {
            id: id.to_string(),
            service_id: service_id.to_string(),
            name: Some(name.to_string()),
            description: Some("按城市统计".into()),
            title: Some("By city".into()),
            tool_type: McpToolType::ParameterizedSql,
            enabled: true,
            config: Some(ToolConfig::ParameterizedSql(ParamSqlConfig {
                data_source_id: Some("d-1".into()),
                sql_template: Some("SELECT * FROM orders WHERE city = ?".into()),
                ..ParamSqlConfig::default()
            })),
        }
    }

    #[tokio::test]
    async fn a_custom_tool_round_trips_with_its_config() {
        let db = store().await;
        let row = tool("t-1", "svc-1", "by_city");
        insert_custom(&db, &row, &audit()).await.unwrap();

        let found = find_custom(&db, "svc-1", "t-1").await.unwrap().unwrap();
        assert_eq!(found, row);
        assert_eq!(found.service_id, "svc-1");
        assert!(find_custom(&db, "svc-2", "t-1").await.unwrap().is_none());

        let mut changed = found.clone();
        changed.name = Some("by_city_v2".into());
        changed.enabled = false;
        changed.config = Some(ToolConfig::default_for(McpToolType::ParameterizedSql));
        update_custom(&db, &changed, &audit()).await.unwrap();
        let found = find_custom(&db, "svc-1", "t-1").await.unwrap().unwrap();
        assert_eq!(found.name.as_deref(), Some("by_city_v2"));
        assert!(!found.enabled);
        assert_eq!(
            found
                .config
                .as_ref()
                .unwrap()
                .as_param_sql()
                .unwrap()
                .sql_template,
            None,
            "an empty config reads as the defaults of its type"
        );
    }

    #[tokio::test]
    async fn a_tool_without_a_stored_config_reads_as_its_defaults() {
        let db = store().await;
        Statement::new(
            "INSERT INTO mcp_custom_tool (id, service_id, name, description, tool_type, enabled) \
             VALUES ('t-1', 'svc-1', 'by_city', 'd', 'PARAMETERIZED_SQL', 1)",
        )
        .execute(&db)
        .await
        .unwrap();
        let found = find_custom(&db, "svc-1", "t-1").await.unwrap().unwrap();
        assert_eq!(
            found.config.unwrap(),
            ToolConfig::default_for(McpToolType::ParameterizedSql)
        );
        assert_eq!(found.title, None);
    }

    #[tokio::test]
    async fn names_are_unique_within_a_service_only() {
        let db = store().await;
        insert_custom(&db, &tool("t-1", "svc-1", "by_city"), &audit())
            .await
            .unwrap();
        insert_custom(&db, &tool("t-2", "svc-2", "by_city"), &audit())
            .await
            .unwrap();

        assert!(name_taken(&db, "svc-1", "by_city", None).await.unwrap());
        assert!(
            !name_taken(&db, "svc-1", "by_city", Some("t-1"))
                .await
                .unwrap()
        );
        assert!(
            name_taken(&db, "svc-1", "by_city", Some("t-9"))
                .await
                .unwrap()
        );
        assert!(!name_taken(&db, "svc-1", "other", None).await.unwrap());
        assert!(name_taken(&db, "svc-2", "by_city", None).await.unwrap());
    }

    #[tokio::test]
    async fn a_service_lists_its_tools_newest_first_and_counts_them() {
        let db = store().await;
        for id in ["t-1", "t-2"] {
            insert_custom(&db, &tool(id, "svc-1", &format!("by_city_{id}")), &audit())
                .await
                .unwrap();
        }
        let listed = custom(&db, "svc-1").await.unwrap();
        assert_eq!(listed.len(), 2);
        // Both rows share a second-precision `created_at`, so the id tiebreaker keeps the order stable.
        assert_eq!(listed[0].id, "t-2");
        assert_eq!(listed[1].id, "t-1");

        let counts = counts_by_service(&db, &["svc-1".into(), "svc-2".into()])
            .await
            .unwrap();
        assert_eq!(total(&counts, "svc-1"), PREBUILT_TOOLS.len() as i64 + 2);
        assert_eq!(total(&counts, "svc-2"), PREBUILT_TOOLS.len() as i64);

        assert!(delete_custom(&db, "svc-1", "t-1").await.unwrap());
        assert!(!delete_custom(&db, "svc-1", "t-1").await.unwrap());
        assert_eq!(remove_all(&db, "svc-1").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_prebuilt_slot_is_created_by_its_first_save() {
        let db = store().await;
        assert!(
            prebuilt_one(&db, "svc-1", McpToolType::ExecuteSql)
                .await
                .unwrap()
                .is_none()
        );
        assert!(prebuilt(&db, "svc-1").await.unwrap().is_empty());

        let config = ToolConfig::default_for(McpToolType::ExecuteSql);
        save_prebuilt(
            &db,
            "svc-1",
            McpToolType::ExecuteSql,
            false,
            &config,
            &audit(),
        )
        .await
        .unwrap();
        let slot = prebuilt_one(&db, "svc-1", McpToolType::ExecuteSql)
            .await
            .unwrap()
            .unwrap();
        assert!(!slot.enabled);
        assert_eq!(slot.config, config);

        let mut config2 = config.as_execute_sql().unwrap().clone();
        config2.timeout = 5;
        save_prebuilt(
            &db,
            "svc-1",
            McpToolType::ExecuteSql,
            true,
            &ToolConfig::ExecuteSql(config2),
            &audit(),
        )
        .await
        .unwrap();
        let rows = prebuilt(&db, "svc-1").await.unwrap();
        assert_eq!(
            rows.len(),
            1,
            "the second save updated the row the first one inserted"
        );
        assert!(rows[0].enabled);
        assert_eq!(rows[0].config.as_execute_sql().unwrap().timeout, 5);

        assert_eq!(remove_all_prebuilt(&db, "svc-1").await.unwrap(), 1);
        assert!(
            prebuilt_one(&db, "svc-1", McpToolType::ExecuteSql)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_slot_of_an_unknown_type_is_reported() {
        let db = store().await;
        Statement::new(
            "INSERT INTO mcp_prebuilt_tool_config (id, service_id, tool_type, enabled) \
             VALUES ('c-1', 'svc-1', 'IMPORT_TABLES', 1)",
        )
        .execute(&db)
        .await
        .unwrap();
        assert_eq!(
            prebuilt(&db, "svc-1").await.unwrap_err().code(),
            ErrorCode::InternalError
        );
    }
}
