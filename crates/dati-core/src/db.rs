//! Metadata-store access: one connection pool and one SQL text for sqlite / mysql / postgres.
//!
//! Two dialect differences are absorbed here instead of leaking into every repository:
//! placeholder style (`?` everywhere, `$n` on postgres) and migration DDL.

use std::collections::HashSet;

use sqlx::any::{Any, AnyPoolOptions};
use sqlx::pool::Pool;
use sqlx::{Executor, Row};

/// Type alias kept local: `sqlx::any::AnyPool` is only re-exported behind a doc-hidden path.
pub type AnyPool = Pool<Any>;

use crate::config::MetaKind;
use crate::error::{DatiError, ErrorCode, Result};
use crate::time;

const MIGRATION_TABLE: &str = "schema_migration";

#[derive(Clone)]
pub struct Db {
    pool: AnyPool,
    kind: MetaKind,
}

impl Db {
    pub async fn connect(url: &str, kind: MetaKind) -> Result<Self> {
        sqlx::any::install_default_drivers();
        let pool = AnyPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(url)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "cannot reach the metadata store");
                DatiError::new(ErrorCode::InternalError, ["metadata store unavailable"])
            })?;
        let db = Self { pool, kind };
        db.migrate().await?;
        Ok(db)
    }

    /// An isolated sqlite store, created per test so cases never share state.
    #[cfg(test)]
    pub async fn sqlite_temp() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = format!(
            "sqlite:{}/meta.db?mode=rwc",
            dir.path().to_string_lossy().replace('\\', "/")
        );
        let db = Db::connect(&url, MetaKind::Sqlite)
            .await
            .expect("migrate sqlite temp db");
        // The pool keeps the file open; releasing the guard would delete it mid-test.
        std::mem::forget(dir);
        db
    }

    pub fn kind(&self) -> MetaKind {
        self.kind
    }

    pub fn pool(&self) -> &AnyPool {
        &self.pool
    }

    /// Rewrites a `?`-style statement for the target dialect. Call sites bind against the
    /// returned `String` in the same scope, so the borrow stays valid.
    pub fn sql(&self, statement: &str) -> String {
        match self.kind {
            MetaKind::Postgres => rewrite_placeholders(statement),
            _ => statement.to_string(),
        }
    }

    pub async fn execute(&self, statement: &str) -> Result<u64> {
        let sql = self.sql(statement);
        self.pool
            .execute(sql.as_str())
            .await
            .map_err(context)
            .map(|r| r.rows_affected())
    }

    /// Applies every embedded migration not yet recorded in this database.
    pub async fn migrate(&self) -> Result<()> {
        let create = format!(
            "CREATE TABLE IF NOT EXISTS {MIGRATION_TABLE} (version VARCHAR(64) NOT NULL UNIQUE, applied_at VARCHAR(32) NOT NULL)"
        );
        self.pool.execute(create.as_str()).await.map_err(context)?;

        let select = self.sql(&format!("SELECT version FROM {MIGRATION_TABLE}"));
        let applied: HashSet<String> = sqlx::query(&select)
            .fetch_all(&self.pool)
            .await
            .map_err(context)?
            .iter()
            .map(|row| row.try_get::<String, _>("version"))
            .collect::<std::result::Result<_, _>>()
            .map_err(context)?;

        for (version, dialect, script) in migrations() {
            if dialect != self.kind || applied.contains(version) {
                continue;
            }
            for statement in split_statements(script) {
                self.pool.execute(statement.as_str()).await.map_err(|err| {
                    tracing::error!(error = %err, version, "migration statement failed");
                    DatiError::new(
                        ErrorCode::InternalError,
                        [format!("migration {version} failed: {err}")],
                    )
                })?;
            }
            let insert = self.sql(&format!(
                "INSERT INTO {MIGRATION_TABLE} (version, applied_at) VALUES (?, ?)"
            ));
            sqlx::query(&insert)
                .bind(version)
                .bind(time::now_iso())
                .execute(&self.pool)
                .await
                .map_err(context)?;
            tracing::info!(version, dialect = dialect.as_str(), "migration applied");
        }
        Ok(())
    }
}

/// Percent-encodes everything sqlx would misread inside URL userinfo.
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// `?` -> `$n`, skipping quoted text and comments so literal question marks survive.
pub fn rewrite_placeholders(statement: &str) -> String {
    map_placeholders(statement, &mut |index| format!("${}", index + 1))
}

/// Replaces every `?` that is not inside quoted text or a comment with what `render` makes of that
/// placeholder's zero-based position.
pub fn map_placeholders(statement: &str, render: &mut dyn FnMut(usize) -> String) -> String {
    let mut out = String::with_capacity(statement.len() + 8);
    let chars: Vec<char> = statement.chars().collect();
    let mut index = 0usize;
    let mut param = 0usize;

    while index < chars.len() {
        let ch = chars[index];
        match ch {
            '\'' | '"' | '`' => {
                index += 1;
                out.push(ch);
                while index < chars.len() {
                    let inner = chars[index];
                    if inner == '\\' && ch == '\'' && index + 1 < chars.len() {
                        out.push('\\');
                        out.push(chars[index + 1]);
                        index += 2;
                        continue;
                    }
                    index += 1;
                    out.push(inner);
                    if inner == ch {
                        break;
                    }
                }
            }
            '-' if chars.get(index + 1) == Some(&'-') => {
                index = copy_line(&chars, index, &mut out);
            }
            '/' if chars.get(index + 1) == Some(&'*') => {
                out.push('/');
                out.push('*');
                index += 2;
                while index < chars.len() {
                    if chars[index] == '*' && chars.get(index + 1) == Some(&'/') {
                        out.push('*');
                        out.push('/');
                        index += 2;
                        break;
                    }
                    out.push(chars[index]);
                    index += 1;
                }
            }
            '?' => {
                out.push_str(&render(param));
                param += 1;
                index += 1;
            }
            _ => {
                out.push(ch);
                index += 1;
            }
        }
    }
    out
}

fn copy_line(chars: &[char], from: usize, out: &mut String) -> usize {
    let mut index = from;
    while index < chars.len() && chars[index] != '\n' {
        out.push(chars[index]);
        index += 1;
    }
    index
}

/// Splits a migration script into individually executable statements.
pub fn split_statements(script: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = script.chars().collect();
    let mut index = 0usize;
    let (mut in_single, mut in_double, mut in_backtick, mut in_block, mut mut_line) =
        (false, false, false, false, false);

    while index < chars.len() {
        let ch = chars[index];
        let next = chars.get(index + 1).copied();
        if mut_line {
            if ch == '\n' {
                mut_line = false;
                current.push(ch);
            } else {
                current.push(ch);
            }
            index += 1;
            continue;
        }
        if in_block {
            current.push(ch);
            if ch == '*' && next == Some('/') {
                current.push('/');
                index += 2;
                in_block = false;
                continue;
            }
            index += 1;
            continue;
        }
        if !(in_single || in_double || in_backtick) {
            if ch == '-' && next == Some('-') {
                mut_line = true;
                current.push('-');
                current.push('-');
                index += 2;
                continue;
            }
            if ch == '/' && next == Some('*') {
                in_block = true;
                current.push('/');
                current.push('*');
                index += 2;
                continue;
            }
        }
        match ch {
            '\'' if !(in_double || in_backtick) => in_single = !in_single,
            '"' if !(in_single || in_backtick) => in_double = !in_double,
            '`' if !(in_single || in_double) => in_backtick = !in_backtick,
            ';' if !(in_single || in_double || in_backtick) => {
                push_statement(&mut statements, &mut current);
                index += 1;
                continue;
            }
            _ => {}
        }
        current.push(ch);
        index += 1;
    }
    push_statement(&mut statements, &mut current);
    statements
}

fn push_statement(out: &mut Vec<String>, current: &mut String) {
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_string());
    }
    current.clear();
}

fn migrations() -> Vec<(&'static str, MetaKind, &'static str)> {
    vec![
        (
            "0001_init",
            MetaKind::Sqlite,
            include_str!("../../../migrations/sqlite/0001_init.sql"),
        ),
        (
            "0001_init",
            MetaKind::Mysql,
            include_str!("../../../migrations/mysql/0001_init.sql"),
        ),
        (
            "0001_init",
            MetaKind::Postgres,
            include_str!("../../../migrations/postgres/0001_init.sql"),
        ),
    ]
}

fn context(err: sqlx::Error) -> DatiError {
    context_with(err, "query")
}

/// Reads a nullable text column holding a JSON array; absent or malformed yields empty.
pub fn read_json_list(row: &sqlx::any::AnyRow, column: &str) -> Vec<String> {
    row.try_get::<Option<String>, _>(column)
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str::<Vec<String>>(&text).ok())
        .unwrap_or_default()
}

/// Wraps a driver error, translating unique-constraint violations into `code`.
pub fn context_with(err: sqlx::Error, subject: &str) -> DatiError {
    if let sqlx::Error::Database(db) = &err
        && let Some(sqlstate) = db.code()
        && (sqlstate.starts_with("23505") || sqlstate.starts_with("23000"))
    {
        tracing::debug!(error = %err, subject, "unique constraint violated");
        return DatiError::new(
            ErrorCode::InternalError,
            [format!("unique constraint violated: {subject}")],
        );
    }
    tracing::error!(error = %err, subject, "database error");
    DatiError::from(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_rewrite_numbers_in_order() {
        assert_eq!(
            rewrite_placeholders("SELECT * FROM t WHERE a = ? AND b = ? OR c = ?"),
            "SELECT * FROM t WHERE a = $1 AND b = $2 OR c = $3"
        );
    }

    #[test]
    fn placeholder_rewrite_skips_literals_and_comments() {
        assert_eq!(
            rewrite_placeholders("SELECT '?' AS q, \"?\" FROM t -- where ?\nAND x = ?"),
            "SELECT '?' AS q, \"?\" FROM t -- where ?\nAND x = $1"
        );
        assert_eq!(
            rewrite_placeholders("SELECT/*?*/a FROM t WHERE b = ?"),
            "SELECT/*?*/a FROM t WHERE b = $1"
        );
        assert_eq!(
            rewrite_placeholders(r"SELECT 'it\'s ?' , ?"),
            r"SELECT 'it\'s ?' , $1"
        );
    }

    #[test]
    fn percent_encoding_keeps_urls_parsable() {
        assert_eq!(percent_encode("root"), "root");
        assert_eq!(percent_encode("p@ss:w/rd"), "p%40ss%3Aw%2Frd");
        assert_eq!(percent_encode("a~b.c-d_e"), "a~b.c-d_e");
    }

    #[test]
    fn statement_splitter_respects_quotes_and_comments() {
        let script =
            "CREATE TABLE a (x INT);\n-- drop; me\nCREATE TABLE b (y VARCHAR(10) DEFAULT ';');\n\n";
        let parts = split_statements(script);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert!(parts[0].starts_with("CREATE TABLE a"));
        assert!(parts[1].contains("DEFAULT ';'"));
    }

    #[test]
    fn every_dialect_ships_the_same_tables() {
        for (version, kind, script) in migrations() {
            assert!(
                !split_statements(script).is_empty(),
                "{version} for {kind:?} is empty"
            );
            if version != "0001_init" {
                continue;
            }
            for table in [
                "users",
                "api_key",
                "data_source",
                "table_info",
                "column_info",
                "column_value",
                "subject",
                "subject_table",
                "term",
                "term_relation",
                "resource_acl",
                "mcp_service",
                "mcp_service_snapshot",
                "mcp_service_data_scope",
                "mcp_prebuilt_tool_config",
                "mcp_custom_tool",
                "mcp_prompt",
                "mcp_invocation_log",
                "mcp_usage_daily_stat",
                "mcp_metadata_audit_log",
            ] {
                assert!(
                    script.contains(table),
                    "{kind:?} migration is missing table {table}"
                );
            }
        }
    }

    /// A version that exists for one dialect only would leave the other two stores behind forever,
    /// so the sets have to stay equal.
    #[test]
    fn every_dialect_carries_every_version() {
        let all = migrations();
        let mut versions: Vec<&str> = all.iter().map(|(version, _, _)| *version).collect();
        versions.sort_unstable();
        versions.dedup();
        for kind in [MetaKind::Sqlite, MetaKind::Mysql, MetaKind::Postgres] {
            for version in &versions {
                assert!(
                    all.iter().any(|(v, k, _)| *k == kind && v == version),
                    "{kind:?} has no {version}"
                );
            }
        }
    }

    #[tokio::test]
    async fn applies_migrations_twice_on_sqlite_without_error() {
        let db = Db::sqlite_temp().await;
        for _ in 0..2 {
            let sql = db.sql("SELECT COUNT(*) AS c FROM users");
            let row = sqlx::query(&sql).fetch_one(db.pool()).await.unwrap();
            assert_eq!(row.try_get::<i64, _>("c").unwrap(), 0);
        }
        let sql = db.sql("SELECT COUNT(*) AS c FROM schema_migration");
        let row = sqlx::query(&sql).fetch_one(db.pool()).await.unwrap();
        assert_eq!(row.try_get::<i64, _>("c").unwrap(), 1);
    }
}
