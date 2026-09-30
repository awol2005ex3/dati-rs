//! Timestamp and identifier helpers.
//!
//! The metadata store is reached through `sqlx::any`, which has no `chrono` support, so
//! instants are persisted as ISO-8601 UTC text. That representation also sorts chronologically,
//! which keeps `ORDER BY created_at` correct across all three dialects.

use chrono::{SecondsFormat, Utc};

/// Second-precision UTC instant, e.g. `2026-09-24T08:30:00Z`.
pub fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// UTC instant `days` from now, or `None` when `days` is `None` (meaning "never expires").
pub fn plus_days(days: Option<i64>) -> Option<String> {
    days.map(|d| {
        Utc::now()
            .checked_add_days(chrono::Days::new(d.max(0) as u64))
            .unwrap_or_else(Utc::now)
            .to_rfc3339_opts(SecondsFormat::Secs, true)
    })
}

/// Local calendar date used by the stats tables, e.g. `2026-09-24`.
pub fn today() -> String {
    Utc::now().format("%Y-%m-%d").to_string()
}

/// The calendar date `days` back, the window a stats query or the retention purge starts from.
pub fn days_ago(days: i64) -> String {
    Utc::now()
        .checked_sub_days(chrono::Days::new(days.max(0) as u64))
        .unwrap_or_else(Utc::now)
        .format("%Y-%m-%d")
        .to_string()
}

/// The instant `days` back, as the log purge compares it against `created_at`.
pub fn iso_days_ago(days: i64) -> String {
    Utc::now()
        .checked_sub_days(chrono::Days::new(days.max(0) as u64))
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub fn now_millis() -> i64 {
    Utc::now().timestamp_millis()
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_instants_sort_chronologically() {
        let older = "2026-01-01T00:00:00Z".to_string();
        let newer = now_iso();
        assert!(older < newer, "{older} should precede {newer}");
        assert_eq!(newer.len(), 20);
        assert!(newer.ends_with('Z'));
    }

    #[test]
    fn plus_days_rounds_down_to_none() {
        assert!(plus_days(None).is_none());
        assert!(plus_days(Some(7)).unwrap().ends_with('Z'));
    }

    #[test]
    fn ids_are_unique_uuids() {
        assert_eq!(new_id().len(), 36);
        assert_ne!(new_id(), new_id());
    }
}
