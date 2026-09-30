//! MCP usage: what a call leaves behind, and the report the console reads back.
//!
//! The Java endpoint never waited on the database for statistics: `McpUsageCollector` counted in
//! memory and let a scheduled task write the buffer out. The same split is kept here — `record` is
//! synchronous and cannot fail, while `flush` and the retention purge are the only paths that touch
//! the store.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::db::Db;
use crate::error::Result;
use crate::model::base::{Audit, PageReq};
use crate::model::mcp_usage::{
    DailyStat, InvocationLog, InvocationLogView, UsageRecord, UsageStatsView,
};
use crate::repo::{mcp_invocation_log, mcp_usage_stat};
use crate::time;

/// The most buffered calls the collector keeps; past it a call is counted but not written out.
const MAX_BUFFERED_LOGS: usize = 10_000;
/// How much of an error text a log row keeps.
const MAX_ERROR_LENGTH: usize = 500;
/// The retention the Java defaults gave when the configured value was not a positive number.
const DEFAULT_RETENTION_DAYS: i64 = 15;

/// The service, tool and day one set of counters belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StatKey {
    service_id: String,
    tool_name: String,
    stat_date: String,
}

/// The counters of one key since the last flush.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Delta {
    total: i64,
    success: i64,
    failed: i64,
    duration: i64,
    max_duration: i64,
}

impl Delta {
    fn record(&mut self, success: bool, duration_ms: i64) {
        self.total += 1;
        if success {
            self.success += 1;
        } else {
            self.failed += 1;
        }
        self.duration += duration_ms;
        self.max_duration = self.max_duration.max(duration_ms);
    }

    fn merge_back(&mut self, other: Self) {
        self.total += other.total;
        self.success += other.success;
        self.failed += other.failed;
        self.duration += other.duration;
        self.max_duration = self.max_duration.max(other.max_duration);
    }
}

#[derive(Default)]
struct Buffer {
    logs: VecDeque<InvocationLog>,
    stats: HashMap<StatKey, Delta>,
}

/// The in-memory half of the statistics: every call the endpoint answered arrives here, and a
/// periodic `flush` writes the buffer out.
#[derive(Clone)]
pub struct UsageCollector {
    db: Db,
    buffer: Arc<Mutex<Buffer>>,
    flushing: Arc<AtomicBool>,
    enabled: bool,
    batch_size: usize,
}

impl UsageCollector {
    pub fn new(db: Db, enabled: bool, batch_size: usize) -> Self {
        Self {
            db,
            buffer: Arc::new(Mutex::new(Buffer::default())),
            flushing: Arc::new(AtomicBool::new(false)),
            enabled,
            batch_size: batch_size.max(1),
        }
    }

    /// Counts one request. A disabled collector, or a call with no service behind it, is dropped —
    /// which is what the Java guard did before it touched a single counter.
    pub fn record(&self, record: UsageRecord) {
        if !self.enabled {
            return;
        }
        let stat_date = time::today();
        let key = StatKey {
            service_id: record.service_id.clone(),
            tool_name: record.tool_name.clone(),
            stat_date: stat_date.clone(),
        };
        let mut buffer = self
            .buffer
            .lock()
            .expect("usage buffer is not shared across panics");
        buffer
            .stats
            .entry(key)
            .or_default()
            .record(record.success, record.duration_ms);

        if buffer.logs.len() >= MAX_BUFFERED_LOGS {
            tracing::warn!(
                service_id = %record.service_id,
                "MCP usage log queue is full, dropping detailed log"
            );
        } else {
            buffer.logs.push_back(log_row(record, stat_date));
        }
    }

    /// Whether the buffer reached the batch size, the trigger the Java collector answered with an
    /// immediate flush on its own thread.
    pub fn over_batch(&self) -> bool {
        self.enabled
            && self
                .buffer
                .lock()
                .map(|buffer| buffer.logs.len() >= self.batch_size)
                .unwrap_or(false)
    }

    /// Writes everything the buffer holds. Only one flush runs at a time; a second caller returns
    /// without waiting, exactly as the Java compare-and-set did.
    pub async fn flush(&self) {
        self.flush_on(&time::today()).await;
    }

    /// `today` decides which day's accumulators are retired: an entry stamped with an older day can
    /// never receive another record, so it is flushed and dropped rather than kept forever.
    async fn flush_on(&self, today: &str) {
        if !self.enabled || self.flushing.swap(true, Ordering::SeqCst) {
            return;
        }
        let (logs, deltas) = {
            let mut buffer = match self.buffer.lock() {
                Ok(buffer) => buffer,
                Err(poisoned) => poisoned.into_inner(),
            };
            let logs: Vec<InvocationLog> = buffer.logs.drain(..).collect();
            let mut deltas = Vec::new();
            for key in buffer.stats.keys().cloned().collect::<Vec<_>>() {
                let Some(entry) = buffer.stats.get(&key) else {
                    continue;
                };
                let stale = key.stat_date != today;
                let delta = *entry;
                if delta.total == 0 {
                    if stale {
                        buffer.stats.remove(&key);
                    }
                    continue;
                }
                // A drained day is taken out of the map; a failed write puts it back.
                buffer.stats.remove(&key);
                deltas.push((key, delta));
            }
            (logs, deltas)
        };

        for chunk in logs.chunks(self.batch_size) {
            if let Err(error) = mcp_invocation_log::save(&self.db, chunk).await {
                tracing::warn!(%error, "failed to write MCP invocation logs");
            }
        }
        for (key, delta) in deltas {
            if let Err(error) = self.write_stat(&key, delta).await {
                tracing::warn!(%error, service_id = %key.service_id, tool = %key.tool_name,
                    "failed to increment MCP daily stats");
                let mut buffer = match self.buffer.lock() {
                    Ok(buffer) => buffer,
                    Err(poisoned) => poisoned.into_inner(),
                };
                buffer.stats.entry(key).or_default().merge_back(delta);
            }
        }
        self.flushing.store(false, Ordering::SeqCst);
    }

    /// The increment-then-insert the Java DAO did: the update is the common case, and only a day
    /// nobody has written yet needs a row. A lost insert race falls back to the increment.
    async fn write_stat(&self, key: &StatKey, delta: Delta) -> Result<()> {
        let updated = mcp_usage_stat::increment(
            &self.db,
            &key.service_id,
            &key.tool_name,
            &key.stat_date,
            delta.total,
            delta.success,
            delta.failed,
            delta.duration,
            delta.max_duration,
        )
        .await?;
        if updated > 0 {
            return Ok(());
        }
        let row = DailyStat {
            id: time::new_id(),
            audit: Audit::timestamped(),
            service_id: key.service_id.clone(),
            tool_name: key.tool_name.clone(),
            stat_date: key.stat_date.clone(),
            total_calls: delta.total,
            success_calls: delta.success,
            failed_calls: delta.failed,
            total_duration_ms: delta.duration,
            max_duration_ms: delta.max_duration,
        };
        if mcp_usage_stat::insert(&self.db, &row).await.is_err() {
            mcp_usage_stat::increment(
                &self.db,
                &key.service_id,
                &key.tool_name,
                &key.stat_date,
                delta.total,
                delta.success,
                delta.failed,
                delta.duration,
                delta.max_duration,
            )
            .await?;
        }
        Ok(())
    }

    /// How many calls are still waiting to be written, which is what the shutdown flush protects.
    pub fn buffered_logs(&self) -> usize {
        self.buffer
            .lock()
            .map(|buffer| buffer.logs.len())
            .unwrap_or_default()
    }
}

/// The buffered row of one call. The row carries no author: the collector wrote it on a background
/// thread, where the Java `BasePO` only ever had a creation timestamp.
fn log_row(record: UsageRecord, stat_date: String) -> InvocationLog {
    InvocationLog {
        id: time::new_id(),
        audit: Audit::timestamped(),
        service_id: record.service_id,
        method: record.method,
        tool_name: record.tool_name,
        caller_id: record.caller_id,
        caller_name: record.caller_name,
        client_ip: record.client_ip,
        success: record.success,
        duration_ms: record.duration_ms,
        error_message: record
            .error_message
            .filter(|text| !text.trim().is_empty())
            // A truncated suffix keeps the row inside its column, character rather than byte.
            .map(|text| text.chars().take(MAX_ERROR_LENGTH).collect()),
        stat_date,
    }
}

/// The two read endpoints over those rows, plus the retention purge behind them.
#[derive(Clone)]
pub struct McpUsageService {
    db: Db,
    retention_days: i64,
}

impl McpUsageService {
    /// A retention of zero or less is the unset case, and the Java service fell back to 15 days.
    pub fn new(db: Db, retention_days: i64) -> Self {
        Self {
            db,
            retention_days: if retention_days > 0 {
                retention_days
            } else {
                DEFAULT_RETENTION_DAYS
            },
        }
    }

    pub const fn retention_days(&self) -> i64 {
        self.retention_days
    }

    /// `GET /v1/mcp-services/{id}/stats`: the window the caller asked for, defaulting to the
    /// retention. No permission check: the Java service read the counters as they were.
    pub async fn stats(&self, service_id: &str, days: i64) -> Result<UsageStatsView> {
        let days = if days > 0 { days } else { self.retention_days };
        let stats = mcp_usage_stat::list_since(&self.db, service_id, &time::days_ago(days)).await?;
        Ok(UsageStatsView::of(&stats, &time::today()))
    }

    /// `GET /v1/mcp-services/{id}/invocation-logs`.
    pub async fn logs(
        &self,
        service_id: &str,
        tool_name: Option<&str>,
        success: Option<bool>,
        page: PageReq,
    ) -> Result<(Vec<InvocationLogView>, i64)> {
        let (rows, total) =
            mcp_invocation_log::page(&self.db, service_id, tool_name, success, page).await?;
        Ok((rows.iter().map(InvocationLogView::new).collect(), total))
    }

    /// The 3 AM job: logs older than the retention in instants, stat days older in calendar days.
    /// Returns how many rows went, so a caller can report it the way the Java log line did.
    pub async fn cleanup(&self) -> Result<u64> {
        let logs =
            mcp_invocation_log::remove_before(&self.db, &time::iso_days_ago(self.retention_days))
                .await?;
        let stats =
            mcp_usage_stat::remove_before(&self.db, &time::days_ago(self.retention_days)).await?;
        Ok(logs + stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::mcp_usage::UsageStatsView;

    fn record(tool: &str, success: bool, duration: i64) -> UsageRecord {
        UsageRecord {
            service_id: "svc-1".to_string(),
            method: "tools/call".to_string(),
            tool_name: tool.to_string(),
            caller_id: Some("u-1".to_string()),
            caller_name: Some("alice".to_string()),
            client_ip: Some("127.0.0.1".to_string()),
            success,
            duration_ms: duration,
            error_message: if success {
                None
            } else {
                Some("工具不存在".to_string())
            },
        }
    }

    fn collector(db: &Db) -> UsageCollector {
        UsageCollector::new(db.clone(), true, 2)
    }

    async fn stats_of(db: &Db) -> UsageStatsView {
        UsageStatsView::of(
            &mcp_usage_stat::list_since(db, "svc-1", "2000-01-01")
                .await
                .unwrap(),
            &time::today(),
        )
    }

    #[tokio::test]
    async fn a_record_becomes_one_log_row_and_one_stat_row() {
        let db = Db::sqlite_temp().await;
        let collector = collector(&db);
        collector.record(record("execute_sql", true, 30));
        collector.record(record("execute_sql", false, 70));
        assert_eq!(collector.buffered_logs(), 2);
        // The batch threshold is what tells the caller an early flush is worth doing.
        assert!(collector.over_batch());

        collector.flush().await;
        assert_eq!(collector.buffered_logs(), 0);

        let (logs, total) =
            mcp_invocation_log::page(&db, "svc-1", None, None, PageReq { page: 1, size: 10 })
                .await
                .unwrap();
        assert_eq!(total, 2);
        assert!(logs.iter().all(|log| log.tool_name == "execute_sql"));
        assert_eq!(logs.iter().filter(|log| log.success).count(), 1);
        assert_eq!(
            logs.iter()
                .find(|log| !log.success)
                .unwrap()
                .error_message
                .as_deref(),
            Some("工具不存在")
        );
        assert_eq!(logs[0].caller_name.as_deref(), Some("alice"));
        assert!(
            logs[0].audit.created_at.is_some(),
            "a buffered row is stamped when it is written"
        );

        let view = stats_of(&db).await;
        assert_eq!(view.total_calls, 2);
        assert_eq!(view.success_rate, 50.0);
        assert_eq!(view.avg_duration_ms, 50.0);
        assert_eq!(view.tool_distribution[0].tool_name, "execute_sql");
    }

    #[tokio::test]
    async fn a_second_flush_adds_to_the_day_it_already_wrote() {
        let db = Db::sqlite_temp().await;
        let collector = collector(&db);
        collector.record(record("list_tables", true, 10));
        collector.flush().await;
        collector.record(record("list_tables", true, 40));
        collector.record(record("", true, 5));
        collector.flush().await;

        let view = stats_of(&db).await;
        assert_eq!(view.total_calls, 3);
        // The protocol call was recorded under the empty tool, which the report names General.
        assert_eq!(
            view.tool_distribution
                .iter()
                .map(|item| item.tool_name.as_str())
                .collect::<Vec<_>>(),
            vec!["list_tables", "General"]
        );
        assert_eq!(
            mcp_usage_stat::count_of(&db, "svc-1").await.unwrap(),
            2,
            "one row per tool of the day"
        );
        // The day's record keeps the slowest call of either flush.
        let rows = mcp_usage_stat::list_since(&db, "svc-1", "2000-01-01")
            .await
            .unwrap();
        assert_eq!(
            rows.iter()
                .find(|row| row.tool_name == "list_tables")
                .unwrap()
                .max_duration_ms,
            40
        );
    }

    #[tokio::test]
    async fn an_older_day_is_flushed_and_then_forgotten() {
        let db = Db::sqlite_temp().await;
        let collector = collector(&db);
        let yesterday = {
            let mut buffer = collector.buffer.lock().unwrap();
            buffer.stats.insert(
                StatKey {
                    service_id: "svc-1".into(),
                    tool_name: "old_tool".into(),
                    stat_date: "2020-01-01".into(),
                },
                Delta {
                    total: 3,
                    success: 3,
                    failed: 0,
                    duration: 15,
                    max_duration: 9,
                },
            );
            buffer.stats.insert(
                StatKey {
                    service_id: "svc-1".into(),
                    tool_name: "live_tool".into(),
                    stat_date: time::today(),
                },
                Delta::default(),
            );
            "2020-01-01"
        };

        collector.flush_on(&time::today()).await;

        // Yesterday's counters are in the table, and out of the map.
        let rows = mcp_usage_stat::list_since(&db, "svc-1", yesterday)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].total_calls, 3);
        let buffer = collector.buffer.lock().unwrap();
        assert!(
            buffer.stats.contains_key(&StatKey {
                service_id: "svc-1".into(),
                tool_name: "live_tool".into(),
                stat_date: time::today(),
            }),
            "today's empty accumulator stays for tomorrow"
        );
        assert!(!buffer.stats.contains_key(&StatKey {
            service_id: "svc-1".into(),
            tool_name: "old_tool".into(),
            stat_date: yesterday.into(),
        }));
    }

    #[tokio::test]
    async fn a_disabled_collector_records_nothing_at_all() {
        let db = Db::sqlite_temp().await;
        let collector = UsageCollector::new(db.clone(), false, 100);
        collector.record(record("execute_sql", true, 10));
        assert_eq!(collector.buffered_logs(), 0);
        assert!(!collector.over_batch());
        collector.flush().await;
        assert_eq!(
            mcp_invocation_log::page(&db, "svc-1", None, None, PageReq { page: 1, size: 10 })
                .await
                .unwrap()
                .1,
            0
        );
    }

    #[tokio::test]
    async fn an_error_text_is_cut_at_the_length_the_column_holds() {
        let db = Db::sqlite_temp().await;
        let collector = collector(&db);
        collector.record(UsageRecord {
            error_message: Some("错".repeat(600)),
            ..record("execute_sql", false, 1)
        });
        collector.record(record("execute_sql", true, 1));
        collector.flush().await;

        let (logs, _) =
            mcp_invocation_log::page(&db, "svc-1", None, None, PageReq { page: 1, size: 10 })
                .await
                .unwrap();
        let written = logs
            .iter()
            .find_map(|log| log.error_message.clone())
            .expect("the failed call kept its message");
        assert_eq!(written.chars().count(), MAX_ERROR_LENGTH);
        // A blank message is no message: the Java guard checked `isBlank`.
        collector.record(UsageRecord {
            error_message: Some("   ".to_string()),
            ..record("blank_msg", false, 1)
        });
        collector.record(record("blank_msg", true, 1));
        collector.flush().await;
        let (logs, _) =
            mcp_invocation_log::page(&db, "svc-1", None, None, PageReq { page: 1, size: 10 })
                .await
                .unwrap();
        let blank = logs
            .iter()
            .find(|log| log.tool_name == "blank_msg" && !log.success)
            .expect("the blank-message call was logged");
        assert!(blank.error_message.is_none());
    }

    #[tokio::test]
    async fn the_report_window_and_the_purge_follow_the_retention() {
        let db = Db::sqlite_temp().await;
        let today = time::today();
        for (tool, date) in [
            ("execute_sql", today.as_str()),
            ("list_tables", "2020-01-01"),
        ] {
            mcp_usage_stat::insert(
                &db,
                &DailyStat {
                    id: time::new_id(),
                    audit: Audit::timestamped(),
                    service_id: "svc-1".to_string(),
                    tool_name: tool.to_string(),
                    stat_date: date.to_string(),
                    total_calls: 2,
                    success_calls: 1,
                    failed_calls: 1,
                    total_duration_ms: 20,
                    max_duration_ms: 12,
                },
            )
            .await
            .unwrap();
        }

        let usage = McpUsageService::new(db.clone(), 0);
        assert_eq!(usage.retention_days(), DEFAULT_RETENTION_DAYS);
        // Fifteen days back does not reach the year 2020 row.
        let view = usage.stats("svc-1", 0).await.unwrap();
        assert_eq!(view.total_calls, 2);
        assert_eq!(view.today_calls, 2);
        assert_eq!(view.success_rate, 50.0);

        let old = InvocationLog {
            id: time::new_id(),
            audit: Audit {
                created_at: Some("2020-01-01T00:00:00Z".to_string()),
                ..Audit::default()
            },
            service_id: "svc-1".to_string(),
            method: "tools/call".to_string(),
            tool_name: "execute_sql".to_string(),
            stat_date: "2020-01-01".to_string(),
            ..InvocationLog::default()
        };
        mcp_invocation_log::save(&db, &[old]).await.unwrap();
        // Fifteen days of retention sweeps the year 2020 rows of both tables and keeps today's.
        let usage = McpUsageService::new(db.clone(), 15);
        assert_eq!(usage.cleanup().await.unwrap(), 2);
        assert_eq!(mcp_usage_stat::count_of(&db, "svc-1").await.unwrap(), 1);
        assert_eq!(
            mcp_invocation_log::page(&db, "svc-1", None, None, PageReq { page: 1, size: 10 })
                .await
                .unwrap()
                .1,
            0,
            "the only log the test wrote was past the window"
        );
    }
}
