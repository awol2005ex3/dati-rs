//! What an MCP call cost, and what the console shows about it: one row per call, one aggregated
//! row per service/tool/day, and the report the `stats` endpoint builds out of those.
//!
//! The Java side kept the counters in memory (`McpUsageCollector`) and wrote them out in batches,
//! so these two tables are never read inside a request — only the report and the log page read them.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::model::base::Audit;

/// A row of `mcp_invocation_log`: one JSON-RPC request that ran.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InvocationLog {
    pub id: String,
    pub audit: Audit,
    pub service_id: String,
    pub method: String,
    pub tool_name: String,
    pub caller_id: Option<String>,
    pub caller_name: Option<String>,
    pub client_ip: Option<String>,
    pub success: bool,
    pub duration_ms: i64,
    pub error_message: Option<String>,
    /// `yyyy-MM-dd`, the day the counters were accumulated under.
    pub stat_date: String,
}

/// A row of `mcp_usage_daily_stat`: the counters of one service, tool and day.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DailyStat {
    pub id: String,
    pub audit: Audit,
    pub service_id: String,
    pub tool_name: String,
    pub stat_date: String,
    pub total_calls: i64,
    pub success_calls: i64,
    pub failed_calls: i64,
    pub total_duration_ms: i64,
    pub max_duration_ms: i64,
}

/// A call the collector is asked to remember. The tool name is absent for `prompts/get`, whose
/// subject is a prompt, and the Java collector wrote an empty string for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRecord {
    pub service_id: String,
    pub method: String,
    pub tool_name: String,
    pub caller_id: Option<String>,
    pub caller_name: Option<String>,
    pub client_ip: Option<String>,
    pub success: bool,
    pub duration_ms: i64,
    pub error_message: Option<String>,
}

/// `GET /v1/mcp-services/{id}/stats`: the Java `McpUsageStatsVO`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct UsageStatsView {
    pub total_calls: i64,
    pub today_calls: i64,
    pub success_rate: f64,
    pub avg_duration_ms: f64,
    pub daily_trend: Vec<DailyStatItem>,
    pub tool_distribution: Vec<ToolStatItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DailyStatItem {
    pub date: String,
    pub total_calls: i64,
    pub success_calls: i64,
    pub failed_calls: i64,
    pub success_rate: f64,
    pub avg_duration_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolStatItem {
    pub tool_name: String,
    pub call_count: i64,
    pub percentage: f64,
    pub success_rate: f64,
}

impl UsageStatsView {
    /// Rolls the day/tool rows of the window up into the report. `today` is the date stamp the
    /// `today_calls` counter is measured against; the Java service read the clock itself.
    pub fn of(stats: &[DailyStat], today: &str) -> Self {
        if stats.is_empty() {
            // The Java service answered a bare 100% success rate when nothing was recorded.
            return Self {
                success_rate: 100.0,
                ..Self::default()
            };
        }

        let mut by_day: BTreeMap<&str, (i64, i64, i64, i64)> = BTreeMap::new();
        for row in stats {
            let day = by_day.entry(row.stat_date.as_str()).or_default();
            day.0 += row.total_calls;
            day.1 += row.success_calls;
            day.2 += row.failed_calls;
            day.3 += row.total_duration_ms;
        }

        let mut total_calls = 0;
        let mut total_success = 0;
        let mut total_duration = 0;
        let mut today_calls = 0;
        let daily_trend = by_day
            .iter()
            .map(|(date, (total, success, failed, duration))| {
                total_calls += total;
                total_success += success;
                total_duration += duration;
                if *date == today {
                    today_calls += total;
                }
                DailyStatItem {
                    date: (*date).to_string(),
                    total_calls: *total,
                    success_calls: *success,
                    failed_calls: *failed,
                    success_rate: rate(*success, *total),
                    avg_duration_ms: if *total > 0 {
                        round_one_decimal(*duration as f64 / *total as f64)
                    } else {
                        0.0
                    },
                }
            })
            .collect();

        let mut by_tool: BTreeMap<&str, (i64, i64)> = BTreeMap::new();
        for row in stats {
            let tool = by_tool.entry(row.tool_name.as_str()).or_default();
            tool.0 += row.total_calls;
            tool.1 += row.success_calls;
        }
        let mut tool_distribution: Vec<ToolStatItem> = by_tool
            .iter()
            .map(|(name, (total, success))| ToolStatItem {
                // The empty tool name is a protocol call, not a tool: the Java label for it.
                tool_name: if name.is_empty() {
                    "General".to_string()
                } else {
                    (*name).to_string()
                },
                call_count: *total,
                percentage: if total_calls > 0 {
                    round_one_decimal(*total as f64 / total_calls as f64 * 100.0)
                } else {
                    0.0
                },
                success_rate: rate(*success, *total),
            })
            .collect();
        // Busiest tool first; the sort is stable, so equal counts keep the name order above.
        tool_distribution.sort_by_key(|entry| std::cmp::Reverse(entry.call_count));

        Self {
            total_calls,
            today_calls,
            success_rate: rate(total_success, total_calls),
            avg_duration_ms: if total_calls > 0 {
                round_one_decimal(total_duration as f64 / total_calls as f64)
            } else {
                0.0
            },
            daily_trend,
            tool_distribution,
        }
    }
}

/// The percentage of `total` that succeeded, or the perfect score an empty day reported.
fn rate(success: i64, total: i64) -> f64 {
    if total > 0 {
        round_one_decimal(success as f64 / total as f64 * 100.0)
    } else {
        100.0
    }
}

fn round_one_decimal(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

/// `GET /v1/mcp-services/{id}/invocation-logs`: the Java `McpInvocationLogVO`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InvocationLogView {
    pub id: String,
    pub service_id: String,
    pub method: String,
    pub tool_name: String,
    pub caller_id: Option<String>,
    pub caller_name: Option<String>,
    pub client_ip: Option<String>,
    pub success: bool,
    pub duration_ms: i64,
    pub error_message: Option<String>,
    pub stat_date: String,
    pub created_at: Option<String>,
}

impl InvocationLogView {
    pub fn new(log: &InvocationLog) -> Self {
        Self {
            id: log.id.clone(),
            service_id: log.service_id.clone(),
            method: log.method.clone(),
            tool_name: log.tool_name.clone(),
            caller_id: log.caller_id.clone(),
            caller_name: log.caller_name.clone(),
            client_ip: log.client_ip.clone(),
            success: log.success,
            duration_ms: log.duration_ms,
            error_message: log.error_message.clone(),
            stat_date: log.stat_date.clone(),
            created_at: log.audit.created_at.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(tool: &str, date: &str, total: i64, success: i64, duration: i64) -> DailyStat {
        DailyStat {
            service_id: "svc-1".to_string(),
            tool_name: tool.to_string(),
            stat_date: date.to_string(),
            total_calls: total,
            success_calls: success,
            failed_calls: total - success,
            total_duration_ms: duration,
            max_duration_ms: duration,
            ..DailyStat::default()
        }
    }

    #[test]
    fn an_empty_window_still_reports_a_perfect_success_rate() {
        let view = UsageStatsView::of(&[], "2026-09-28");
        assert_eq!(
            serde_json::to_value(&view).unwrap(),
            serde_json::json!({
                "total_calls": 0,
                "today_calls": 0,
                "success_rate": 100.0,
                "avg_duration_ms": 0.0,
                "daily_trend": [],
                "tool_distribution": [],
            })
        );
    }

    #[test]
    fn the_trend_is_per_day_and_the_totals_are_the_sum_of_it() {
        let view = UsageStatsView::of(
            &[
                stat("execute_sql", "2026-09-27", 4, 3, 400),
                stat("list_tables", "2026-09-28", 6, 6, 100),
                stat("", "2026-09-28", 1, 0, 50),
            ],
            "2026-09-28",
        );
        assert_eq!(view.total_calls, 11);
        assert_eq!(view.today_calls, 7);
        assert_eq!(view.success_rate, 81.8); // 9 of 11.
        assert_eq!(view.avg_duration_ms, 50.0); // 550 of 11.
        assert_eq!(
            view.daily_trend,
            vec![
                DailyStatItem {
                    date: "2026-09-27".into(),
                    total_calls: 4,
                    success_calls: 3,
                    failed_calls: 1,
                    success_rate: 75.0,
                    avg_duration_ms: 100.0,
                },
                DailyStatItem {
                    date: "2026-09-28".into(),
                    total_calls: 7,
                    success_calls: 6,
                    failed_calls: 1,
                    success_rate: 85.7,
                    avg_duration_ms: 21.4,
                },
            ]
        );
    }

    #[test]
    fn the_tool_split_counts_downwards_and_names_the_empty_tool_general() {
        let view = UsageStatsView::of(
            &[
                stat("execute_sql", "2026-09-27", 2, 2, 20),
                stat("execute_sql", "2026-09-28", 4, 3, 40),
                stat("", "2026-09-28", 4, 4, 8),
            ],
            "2026-09-28",
        );
        assert_eq!(
            view.tool_distribution,
            vec![
                ToolStatItem {
                    tool_name: "execute_sql".into(),
                    call_count: 6,
                    percentage: 60.0,
                    success_rate: 83.3,
                },
                ToolStatItem {
                    tool_name: "General".into(),
                    call_count: 4,
                    percentage: 40.0,
                    success_rate: 100.0,
                },
            ]
        );
    }
}
