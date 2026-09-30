//! The `mcp_invocation_log` table: one row per `tools/call` or `prompts/get` the endpoint answered.
//!
//! Rows arrive in batches from the collector, so nothing here is transactional with the call itself
//! — a log is written after the response was already sent.

use sqlx::any::AnyRow;

use crate::db::Db;
use crate::error::Result;
use crate::model::base::{Audit, PageReq};
use crate::model::mcp_usage::InvocationLog;

use super::{Filter, Statement, count, flag, integer, optional, required};

const TABLE: &str = "mcp_invocation_log";

const COLUMNS: &str = "id, created_by, created_at, updated_by, updated_at, service_id, method, \
     tool_name, caller_id, caller_name, client_ip, success, duration_ms, error_message, stat_date";

/// The Java PO carried empty strings for both; the columns are nullable, so a row written by
/// another path still reads as "no tool".
fn text_or_empty(row: &AnyRow, column: &str) -> String {
    optional(row, column).unwrap_or_default()
}

pub fn to_log(row: &AnyRow) -> Result<InvocationLog> {
    Ok(InvocationLog {
        id: required(row, "id")?,
        service_id: required(row, "service_id")?,
        method: text_or_empty(row, "method"),
        tool_name: text_or_empty(row, "tool_name"),
        caller_id: optional(row, "caller_id"),
        caller_name: optional(row, "caller_name"),
        client_ip: optional(row, "client_ip"),
        success: flag(row, "success"),
        duration_ms: integer(row, "duration_ms"),
        error_message: optional(row, "error_message"),
        stat_date: text_or_empty(row, "stat_date"),
        audit: Audit {
            created_by: optional(row, "created_by"),
            created_at: optional(row, "created_at"),
            updated_by: optional(row, "updated_by"),
            updated_at: optional(row, "updated_at"),
        },
    })
}

/// The buffered rows, in the order they were recorded. The collector flushed a batch of these the
/// way the Java DAO's `saveAll` did.
pub async fn save(db: &Db, rows: &[InvocationLog]) -> Result<u64> {
    let mut written = 0;
    for row in rows {
        Statement::new(format!(
            "INSERT INTO {TABLE} (id, created_by, created_at, updated_by, updated_at, service_id, \
             method, tool_name, caller_id, caller_name, client_ip, success, duration_ms, \
             error_message, stat_date) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        ))
        .bind(row.id.clone())
        .bind(row.audit.created_by.clone())
        .bind(row.audit.created_at.clone())
        .bind(row.audit.updated_by.clone())
        .bind(row.audit.updated_at.clone())
        .bind(row.service_id.clone())
        .bind(row.method.clone())
        .bind(row.tool_name.clone())
        .bind(row.caller_id.clone())
        .bind(row.caller_name.clone())
        .bind(row.client_ip.clone())
        .bind(row.success)
        .bind(row.duration_ms)
        .bind(row.error_message.clone())
        .bind(row.stat_date.clone())
        .execute(db)
        .await?;
        written += 1;
    }
    Ok(written)
}

/// `GET /v1/mcp-services/{id}/invocation-logs`: newest first. An empty tool name filter is no
/// filter, which is what the Java query's `:toolName = ''` branch meant.
pub async fn page(
    db: &Db,
    service_id: &str,
    tool_name: Option<&str>,
    success: Option<bool>,
    page_req: PageReq,
) -> Result<(Vec<InvocationLog>, i64)> {
    let mut filter = Filter::default().add("service_id = ?", [service_id.into()]);
    if let Some(tool_name) = tool_name.filter(|value| !value.is_empty()) {
        filter = filter.add("tool_name = ?", [tool_name.into()]);
    }
    if let Some(success) = success {
        filter = filter.add("success = ?", [success.into()]);
    }
    let total = count(&filter.count(TABLE), db).await?;
    let rows = filter
        .select(COLUMNS, TABLE)
        // The Java query ordered by the timestamp alone; the id keeps a page stable when a burst
        // of calls shares one second.
        .push_text(" ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?")
        .bind(page_req.size)
        .bind(page_req.offset())
        .fetch_all(db)
        .await?;
    Ok((rows.iter().map(to_log).collect::<Result<Vec<_>>>()?, total))
}

/// The retention purge: every log stamped before `before` (an ISO-8601 instant, so text order is
/// time order).
pub async fn remove_before(db: &Db, before: &str) -> Result<u64> {
    Statement::new(format!("DELETE FROM {TABLE} WHERE created_at < ?"))
        .bind(before.to_string())
        .execute(db)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::time::new_id;

    fn row(service: &str, tool: &str, success: bool, created_at: &str) -> InvocationLog {
        InvocationLog {
            id: new_id(),
            service_id: service.to_string(),
            method: "tools/call".to_string(),
            tool_name: tool.to_string(),
            caller_name: Some("alice".to_string()),
            success,
            duration_ms: 42,
            error_message: Some("boom".to_string()),
            stat_date: "2026-09-28".to_string(),
            audit: Audit {
                created_at: Some(created_at.to_string()),
                ..Audit::default()
            },
            ..InvocationLog::default()
        }
    }

    #[tokio::test]
    async fn a_batch_of_logs_comes_back_newest_first_and_filterable() {
        let db = Db::sqlite_temp().await;
        save(
            &db,
            &[
                row("svc-1", "execute_sql", true, "2026-09-27T10:00:00Z"),
                row("svc-1", "execute_sql", false, "2026-09-28T10:00:00Z"),
                row("svc-1", "list_tables", true, "2026-09-28T11:00:00Z"),
                row("svc-2", "execute_sql", true, "2026-09-28T12:00:00Z"),
            ],
        )
        .await
        .unwrap();

        let (logs, total) = page(&db, "svc-1", None, None, PageReq { page: 1, size: 10 })
            .await
            .unwrap();
        assert_eq!(total, 3);
        assert_eq!(
            logs.iter()
                .map(|log| (log.tool_name.as_str(), log.audit.created_at.as_deref()))
                .collect::<Vec<_>>(),
            vec![
                ("list_tables", Some("2026-09-28T11:00:00Z")),
                ("execute_sql", Some("2026-09-28T10:00:00Z")),
                ("execute_sql", Some("2026-09-27T10:00:00Z")),
            ]
        );
        assert_eq!(logs[0].method, "tools/call");
        assert_eq!(logs[1].duration_ms, 42);
        assert_eq!(logs[1].error_message.as_deref(), Some("boom"));
        assert_eq!(logs[1].caller_name.as_deref(), Some("alice"));

        let (_, total) = page(
            &db,
            "svc-1",
            Some("execute_sql"),
            None,
            PageReq { page: 1, size: 10 },
        )
        .await
        .unwrap();
        assert_eq!(total, 2);
        // An empty filter is no filter at all.
        let (_, total) = page(
            &db,
            "svc-1",
            Some(""),
            Some(false),
            PageReq { page: 1, size: 10 },
        )
        .await
        .unwrap();
        assert_eq!(total, 1);

        let (first, total) = page(&db, "svc-1", None, None, PageReq { page: 2, size: 2 })
            .await
            .unwrap();
        assert_eq!(total, 3);
        assert_eq!(first.len(), 1);
    }

    #[tokio::test]
    async fn the_purge_takes_everything_before_the_cutoff() {
        let db = Db::sqlite_temp().await;
        save(
            &db,
            &[
                row("svc-1", "execute_sql", true, "2026-09-01T00:00:00Z"),
                row("svc-1", "execute_sql", true, "2026-09-20T00:00:00Z"),
            ],
        )
        .await
        .unwrap();
        assert_eq!(remove_before(&db, "2026-09-10T00:00:00Z").await.unwrap(), 1);
        let (_, total) = page(&db, "svc-1", None, None, PageReq { page: 1, size: 10 })
            .await
            .unwrap();
        assert_eq!(total, 1);
    }
}
