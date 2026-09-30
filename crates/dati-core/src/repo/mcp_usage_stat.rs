//! The `mcp_usage_daily_stat` table: the counters the collector flushed, one row per
//! service, tool and day.
//!
//! The unique key is `(service_id, tool_name, stat_date)`, and the collector wrote it the way the
//! Java DAO did: try the increment first, and insert the row only when nothing was there to
//! increment.

use sqlx::any::AnyRow;

use crate::db::Db;
use crate::error::Result;
use crate::model::base::Audit;
use crate::model::mcp_usage::DailyStat;

use super::{Statement, integer, optional, required};

const TABLE: &str = "mcp_usage_daily_stat";

const COLUMNS: &str = "id, created_by, created_at, updated_by, updated_at, service_id, tool_name, \
     stat_date, total_calls, success_calls, failed_calls, total_duration_ms, max_duration_ms";

fn to_stat(row: &AnyRow) -> Result<DailyStat> {
    Ok(DailyStat {
        id: required(row, "id")?,
        service_id: required(row, "service_id")?,
        tool_name: optional(row, "tool_name").unwrap_or_default(),
        stat_date: optional(row, "stat_date").unwrap_or_default(),
        total_calls: integer(row, "total_calls"),
        success_calls: integer(row, "success_calls"),
        failed_calls: integer(row, "failed_calls"),
        total_duration_ms: integer(row, "total_duration_ms"),
        max_duration_ms: integer(row, "max_duration_ms"),
        audit: Audit {
            created_by: optional(row, "created_by"),
            created_at: optional(row, "created_at"),
            updated_by: optional(row, "updated_by"),
            updated_at: optional(row, "updated_at"),
        },
    })
}

/// Adds one flush's worth of counters to the day's row, and returns how many rows it touched —
/// zero means the day had not been seen yet and the caller has to insert it.
///
/// The duration sums, while the maximum keeps the larger of the two: the Java update wrote the
/// same `CASE`, because a flush that reports a fast batch must not lower the record.
#[allow(clippy::too_many_arguments)]
pub async fn increment(
    db: &Db,
    service_id: &str,
    tool_name: &str,
    stat_date: &str,
    total: i64,
    success: i64,
    failed: i64,
    duration: i64,
    max_duration: i64,
) -> Result<u64> {
    Statement::new(format!(
        "UPDATE {TABLE} SET total_calls = total_calls + ?, success_calls = success_calls + ?, \
         failed_calls = failed_calls + ?, total_duration_ms = total_duration_ms + ?, \
         max_duration_ms = CASE WHEN ? > max_duration_ms THEN ? ELSE max_duration_ms END, \
         updated_at = ? WHERE service_id = ? AND tool_name = ? AND stat_date = ?"
    ))
    .bind(total)
    .bind(success)
    .bind(failed)
    .bind(duration)
    .bind(max_duration)
    .bind(max_duration)
    .bind(crate::time::now_iso())
    .bind(service_id.to_string())
    .bind(tool_name.to_string())
    .bind(stat_date.to_string())
    .execute(db)
    .await
}

/// The row of a day the increment found nothing to update.
pub async fn insert(db: &Db, stat: &DailyStat) -> Result<()> {
    Statement::new(format!(
        "INSERT INTO {TABLE} (id, created_by, created_at, updated_by, updated_at, service_id, \
         tool_name, stat_date, total_calls, success_calls, failed_calls, total_duration_ms, \
         max_duration_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    ))
    .bind(stat.id.clone())
    .bind(stat.audit.created_by.clone())
    .bind(stat.audit.created_at.clone())
    .bind(stat.audit.updated_by.clone())
    .bind(stat.audit.updated_at.clone())
    .bind(stat.service_id.clone())
    .bind(stat.tool_name.clone())
    .bind(stat.stat_date.clone())
    .bind(stat.total_calls)
    .bind(stat.success_calls)
    .bind(stat.failed_calls)
    .bind(stat.total_duration_ms)
    .bind(stat.max_duration_ms)
    .execute(db)
    .await?;
    Ok(())
}

/// `GET /v1/mcp-services/{id}/stats`: every day of the window, oldest first.
pub async fn list_since(db: &Db, service_id: &str, since_date: &str) -> Result<Vec<DailyStat>> {
    let rows = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE service_id = ? AND stat_date >= ? \
         ORDER BY stat_date ASC, tool_name ASC, id ASC"
    ))
    .bind(service_id.to_string())
    .bind(since_date.to_string())
    .fetch_all(db)
    .await?;
    rows.iter().map(to_stat).collect()
}

/// The retention purge, measured in calendar days rather than instants: a day that is outside the
/// window is dropped whole.
pub async fn remove_before(db: &Db, stat_date: &str) -> Result<u64> {
    Statement::new(format!("DELETE FROM {TABLE} WHERE stat_date < ?"))
        .bind(stat_date.to_string())
        .execute(db)
        .await
}

/// How many rows a service has, so a test or a purge can state what it expected.
pub async fn count_of(db: &Db, service_id: &str) -> Result<i64> {
    let row = Statement::new(format!(
        "SELECT COUNT(*) AS total FROM {TABLE} WHERE service_id = ?"
    ))
    .bind(service_id.to_string())
    .fetch_one(db)
    .await?;
    Ok(integer(&row, "total"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::new_id;

    fn stat(tool: &str, date: &str) -> DailyStat {
        DailyStat {
            id: new_id(),
            service_id: "svc-1".to_string(),
            tool_name: tool.to_string(),
            stat_date: date.to_string(),
            total_calls: 1,
            success_calls: 1,
            failed_calls: 0,
            total_duration_ms: 10,
            max_duration_ms: 10,
            audit: Audit::timestamped(),
        }
    }

    #[tokio::test]
    async fn a_day_is_created_by_insert_and_grown_by_increment() {
        let db = Db::sqlite_temp().await;
        // Nothing to increment yet: the flush answers zero, which is the cue to insert.
        assert_eq!(
            increment(&db, "svc-1", "execute_sql", "2026-09-28", 1, 1, 0, 10, 10)
                .await
                .unwrap(),
            0
        );
        insert(&db, &stat("execute_sql", "2026-09-28"))
            .await
            .unwrap();

        assert_eq!(
            increment(&db, "svc-1", "execute_sql", "2026-09-28", 3, 2, 1, 90, 50)
                .await
                .unwrap(),
            1
        );
        let rows = list_since(&db, "svc-1", "2026-09-28").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].total_calls, 4);
        assert_eq!(rows[0].success_calls, 3);
        assert_eq!(rows[0].failed_calls, 1);
        assert_eq!(rows[0].total_duration_ms, 100);
        // The recorded maximum survives a faster batch.
        assert_eq!(rows[0].max_duration_ms, 50);
        assert!(rows[0].audit.updated_at.is_some());

        // A slower batch raises it again.
        increment(&db, "svc-1", "execute_sql", "2026-09-28", 1, 1, 0, 400, 400)
            .await
            .unwrap();
        assert_eq!(
            list_since(&db, "svc-1", "2026-09-28").await.unwrap()[0].max_duration_ms,
            400
        );
    }

    #[tokio::test]
    async fn days_are_read_per_service_and_purged_by_date() {
        let db = Db::sqlite_temp().await;
        insert(&db, &stat("execute_sql", "2026-09-20"))
            .await
            .unwrap();
        insert(&db, &stat("list_tables", "2026-09-27"))
            .await
            .unwrap();
        insert(&db, &stat("", "2026-09-28")).await.unwrap();
        insert(&db, &{
            let mut other = stat("execute_sql", "2026-09-28");
            other.service_id = "svc-2".to_string();
            other
        })
        .await
        .unwrap();

        let rows = list_since(&db, "svc-1", "2026-09-25").await.unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| row.stat_date.as_str())
                .collect::<Vec<_>>(),
            vec!["2026-09-27", "2026-09-28"]
        );
        // The protocol calls were recorded under the empty tool name.
        assert_eq!(rows[1].tool_name, "");
        assert_eq!(count_of(&db, "svc-1").await.unwrap(), 3);

        // Only the one day outside the window goes, in either service.
        assert_eq!(remove_before(&db, "2026-09-26").await.unwrap(), 1);
        assert_eq!(count_of(&db, "svc-1").await.unwrap(), 2);
        assert_eq!(count_of(&db, "svc-2").await.unwrap(), 1);
    }
}
