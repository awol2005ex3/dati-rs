//! Live connections to the databases DatI catalogs.
//!
//! Unlike the metadata store, these pools are opened lazily per connection string and cached,
//! mirroring the Java `HikariPoolManager`: same url + user + password reuses one pool, a changed
//! credential evicts it. [`live`] holds the two pool shapes that choice needs, because Doris cannot
//! be opened the way the other sources can.

mod dialect;
mod live;

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::error::{DatiError, ErrorCode, Result};
use crate::model::datasource::{ConnectionSpec, DbType, LiveColumn, LiveTable};

use live::Live;

pub use dialect::Family;
pub use dialect::{
    columns_sql, connection_url, family, infer_db_type, sqlite_pragma_columns, tables_sql,
};

/// Connection failures surface as `DS001`, everything raised while a statement runs as `DS003`,
/// which is how the Java controllers split the `SQLException` cases.
#[derive(Debug, Clone)]
pub enum ConnectError {
    Opening(String),
    Query(String),
}

impl ConnectError {
    pub fn into_error(self, subject: &str) -> DatiError {
        let code = match self {
            Self::Opening(_) => ErrorCode::DsConnectionFailed,
            Self::Query(_) => ErrorCode::DsSqlError,
        };
        self.render(subject, code)
    }

    /// The metadata endpoints wrapped *every* `SQLException`, failed connection included, in
    /// `DS003`; the callers that read live metadata use this instead of `into_error`.
    pub fn into_sql_error(self, subject: &str) -> DatiError {
        self.render(subject, ErrorCode::DsSqlError)
    }

    /// Registering live tables failed: `DS004`, the code the Java batch import used.
    pub fn into_sync_error(self, subject: &str) -> DatiError {
        self.render(subject, ErrorCode::DsSyncFailed)
    }

    /// The sanitized driver message, for callers that prefix it with their own context.
    pub fn reason(&self) -> String {
        match self {
            Self::Opening(reason) | Self::Query(reason) => sanitize(reason),
        }
    }

    fn render(self, subject: &str, code: ErrorCode) -> DatiError {
        let detail = self.reason();
        tracing::warn!(subject, reason = %detail, "data source query failed");
        DatiError::new(code, [detail])
    }
}

/// Drivers echo the connection string back in their error text, so everything after the scheme
/// separator is dropped: the host, the credentials and the database name are none of the client's
/// business when the answer is already "could not connect".
fn sanitize(reason: &str) -> String {
    let mut out = String::with_capacity(reason.len());
    for chunk in reason.split_whitespace() {
        match chunk.find("://") {
            Some(at) => {
                out.push_str(&chunk[..at + 3]);
                out.push_str("***");
            }
            None => out.push_str(chunk),
        }
        out.push(' ');
    }
    out.trim().to_string()
}

#[derive(Clone, Default)]
pub struct Connectors {
    pools: Arc<tokio::sync::Mutex<HashMap<String, Live>>>,
}

impl Connectors {
    pub fn new() -> Self {
        Self::default()
    }

    fn key(spec: &ConnectionSpec) -> String {
        let url = connection_url(spec).unwrap_or_else(|_| spec.url.clone());
        let digest = {
            let mut hasher = Sha256::new();
            hasher.update(spec.password.as_deref().unwrap_or_default().as_bytes());
            hex::encode(hasher.finalize())
        };
        format!(
            "{}|{url}|{}|{digest}",
            spec.db_type.as_str(),
            spec.user.as_deref().unwrap_or_default()
        )
    }

    async fn pool(&self, spec: &ConnectionSpec) -> std::result::Result<Live, ConnectError> {
        let key = Self::key(spec);
        let mut pools = self.pools.lock().await;
        if let Some(pool) = pools.get(&key) {
            return Ok(pool.clone());
        }
        let url =
            connection_url(spec).map_err(|err| ConnectError::Opening(err.message().to_string()))?;
        let created = Live::open(spec, &url).await?;
        pools.insert(key, created.clone());
        Ok(created)
    }

    /// Drops the cached pool for these credentials, called when a data source is edited or deleted.
    pub async fn evict(&self, spec: &ConnectionSpec) {
        let key = Self::key(spec);
        if let Some(pool) = self.pools.lock().await.remove(&key) {
            pool.close().await;
        }
    }

    /// `POST /v1/data-sources/test-connection`: the probe's own outcome, so the caller can report
    /// the driver's reason instead of the bare `false` the Java endpoint returned.
    pub async fn test_connection(
        &self,
        spec: &ConnectionSpec,
    ) -> std::result::Result<(), ConnectError> {
        self.probe(spec).await
    }

    async fn probe(&self, spec: &ConnectionSpec) -> std::result::Result<(), ConnectError> {
        let pool = self.pool(spec).await?;
        let rows = pool.select("SELECT 1", &[], 1).await?;
        if rows.is_empty() {
            return Err(ConnectError::Query(
                "the database answered SELECT 1 with no rows".to_string(),
            ));
        }
        Ok(())
    }

    /// The schema a connection lands in by default; `DS006` when the database cannot say.
    pub async fn detect_schema(&self, spec: &ConnectionSpec) -> Result<String> {
        let family = family(spec.db_type);
        let pool = self
            .pool(spec)
            .await
            .map_err(|err| err.into_error("test-connection"))?;
        let rows = pool
            .select(dialect::current_schema_sql(family), &[], 1)
            .await
            // The Java controller wrapped a failed detect in the connection code, whatever the
            // statement said, so the statement's own error is reported as one.
            .map_err(|err| ConnectError::Opening(err.reason()).into_error("detect-schema"))?;
        let found = rows
            .first()
            .and_then(|row| row.text(dialect::current_schema_column(family)));
        found
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| DatiError::new(ErrorCode::DsSchemaDetectionFailed, [spec.url.clone()]))
    }

    pub async fn list_schemas(
        &self,
        spec: &ConnectionSpec,
    ) -> std::result::Result<Vec<String>, ConnectError> {
        let family = family(spec.db_type);
        let pool = self.pool(spec).await?;
        let rows = pool
            .select(dialect::schemas_sql(family), &[], usize::MAX)
            .await?;
        let index = if dialect::current_schema_column(family) == 1 {
            1
        } else {
            0
        };
        let mut names: Vec<String> = rows.iter().filter_map(|row| row.text(index)).collect();
        if dialect::schemas_skip_temp(family) {
            names.retain(|name| name != "temp");
        }
        Ok(names)
    }

    pub async fn list_tables(
        &self,
        spec: &ConnectionSpec,
        schema: &str,
    ) -> std::result::Result<Vec<LiveTable>, ConnectError> {
        let family = family(spec.db_type);
        let pool = self.pool(spec).await?;
        // SQLite has one attached schema and its query takes no placeholder for it.
        let binds = if family == Family::Sqlite {
            Vec::new()
        } else {
            vec![json!(schema)]
        };
        let rows = pool
            .select(dialect::tables_sql(family), &binds, usize::MAX)
            .await?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                Some(LiveTable {
                    name: row.text(0)?,
                    comment: row.text(1).filter(|value| !value.is_empty()),
                })
            })
            .collect())
    }

    pub async fn list_columns(
        &self,
        spec: &ConnectionSpec,
        schema: &str,
        table: &str,
    ) -> std::result::Result<Vec<LiveColumn>, ConnectError> {
        let family = family(spec.db_type);
        let pool = self.pool(spec).await?;
        // `PRAGMA` takes no parameter, so the table name is quoted into the statement instead.
        let (sql, binds): (String, Vec<Value>) = match family {
            Family::Sqlite => (dialect::sqlite_pragma_columns(table), Vec::new()),
            _ => (
                dialect::columns_sql(family).to_string(),
                vec![json!(schema), json!(table)],
            ),
        };
        let rows = pool.select(&sql, &binds, usize::MAX).await?;
        // PRAGMA table_info is (cid, name, type, notnull, dflt_value, pk); the metadata queries
        // return (name, type, comment) in that order.
        let (name_at, type_at, comment_at) = match family {
            Family::Sqlite => (1, 2, None),
            _ => (0, 1, Some(2)),
        };
        Ok(rows
            .iter()
            .filter_map(|row| {
                Some(LiveColumn {
                    name: row.text(name_at)?,
                    type_name: row.text(type_at).unwrap_or_default(),
                    comment: comment_at
                        .and_then(|at| row.text(at))
                        .filter(|value| !value.is_empty()),
                })
            })
            .collect())
    }

    /// Distinct values of one column, the sample the value dictionary is built from.
    pub async fn distinct_values(
        &self,
        spec: &ConnectionSpec,
        schema: Option<&str>,
        table: &str,
        column: &str,
        limit: i64,
    ) -> std::result::Result<Vec<String>, ConnectError> {
        let sql = dialect::distinct_values_sql(family(spec.db_type), schema, table, column, limit);
        let pool = self.pool(spec).await?;
        let rows = pool.select(&sql, &[], usize::MAX).await?;
        Ok(rows.iter().filter_map(|row| row.text(0)).collect())
    }

    /// Runs one statement of a tool's SQL and reports what came back.
    ///
    /// The Java tool called `Statement.execute` and then walked the JDBC `getMoreResults` loop to
    /// tell a result set from an update count. Here the analyzer has already read the statement, so
    /// [`SqlRun::reads`] makes that choice and the statement is streamed or executed accordingly.
    /// Four JDBC behaviours are stood in for:
    /// - `setMaxRows(n)` stops the fetch at `n` rows, so `total_rows` counts the rows read, exactly
    ///   as the Java `rows.size()` did. A zero `n` means no limit, which is what zero meant there.
    /// - `setQueryTimeout` is a `tokio::time::timeout` around the read. The database keeps working on
    ///   the statement after the tool gives up; only the tool stops waiting.
    /// - A query that matched nothing reports an empty column list: no driver hands out labels
    ///   without a row on the `sqlx::any` read path, and sqlite hands out none at all.
    /// - A multi-statement script is handed to the driver as one query, so it runs only where the
    ///   database itself allows several statements in one round trip.
    pub async fn run_sql(
        &self,
        spec: &ConnectionSpec,
        run: &SqlRun<'_>,
    ) -> std::result::Result<StatementOutcome, ConnectError> {
        let pool = self.pool(spec).await?;
        let limit = if run.max_rows > 0 {
            run.max_rows as usize
        } else {
            usize::MAX
        };
        let work = async {
            if run.reads {
                let rows = pool.select(run.sql, &run.bindings, limit).await?;
                let columns = rows
                    .first()
                    .map(|row| row.column_names().to_vec())
                    .unwrap_or_default();
                let rows: Vec<Vec<Value>> = rows.iter().map(|row| row.values().to_vec()).collect();
                return Ok(StatementOutcome::Select {
                    columns,
                    total_rows: rows.len() as i64,
                    rows,
                });
            }
            let affected_rows = pool.write(run.sql, &run.bindings).await?;
            Ok(StatementOutcome::Write {
                affected_rows: affected_rows as i64,
            })
        };
        match tokio::time::timeout(run.timeout(), work).await {
            Ok(Ok(outcome)) => Ok(outcome),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(ConnectError::Query(format!(
                "query exceeded its {timeout} second limit",
                timeout = run.timeout_secs
            ))),
        }
    }
}

/// One statement as the SQL tools hand it to the connector.
pub struct SqlRun<'a> {
    pub sql: &'a str,
    /// Values for the `?` placeholders, in order. An empty list means the SQL runs as written.
    pub bindings: Vec<Value>,
    /// Whether the statement answers with a result set, as the analyzer read it.
    pub reads: bool,
    pub timeout_secs: i32,
    pub max_rows: i32,
}

impl SqlRun<'_> {
    fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.timeout_secs.max(1) as u64)
    }
}

/// What one statement produced: a result set, or the number of rows it changed.
#[derive(Debug, Clone, PartialEq)]
pub enum StatementOutcome {
    Select {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
        total_rows: i64,
    },
    Write {
        affected_rows: i64,
    },
}

fn query_error(err: sqlx::Error) -> ConnectError {
    match err {
        sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_) => {
            ConnectError::Opening(err.to_string())
        }
        sqlx::Error::Configuration(_) | sqlx::Error::Tls(_) | sqlx::Error::Protocol(_) => {
            ConnectError::Opening(err.to_string())
        }
        other => ConnectError::Query(other.to_string()),
    }
}

/// The connection parameters stored for a data source, without the password decoding detour:
/// the Java `EncryptionUtils` was a pass-through, so `encrypted_password` holds what the user typed.
pub fn spec_of(
    db_type: DbType,
    url: Option<&str>,
    user: Option<&str>,
    password: Option<&str>,
) -> Option<ConnectionSpec> {
    let url = url.filter(|v| !v.trim().is_empty())?;
    Some(ConnectionSpec {
        db_type,
        url: url.to_string(),
        user: user.map(str::to_string),
        password: password.map(str::to_string),
    })
}

/// The spec of a `test-connection` body, which may omit `type` because the URL carries the scheme.
pub fn probe_spec(
    db_type: Option<DbType>,
    url: Option<&str>,
    user: Option<&str>,
    password: Option<&str>,
) -> Option<ConnectionSpec> {
    let url = url.filter(|v| !v.trim().is_empty())?;
    let db_type = db_type.or_else(|| infer_db_type(url))?;
    spec_of(db_type, Some(url), user, password)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sqlite_spec(url: &str) -> ConnectionSpec {
        ConnectionSpec {
            db_type: DbType::Sqlite,
            url: url.into(),
            user: None,
            password: None,
        }
    }

    /// A throwaway database the connector reads as an external data source.
    async fn seeded_file() -> String {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir
            .path()
            .join("warehouse.db")
            .to_string_lossy()
            .replace('\\', "/");
        std::mem::forget(dir);
        let url = format!("sqlite:{path}?mode=rwc");
        sqlx::any::install_default_drivers();
        let pool = sqlx::any::AnyPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("open warehouse");
        sqlx::query("CREATE TABLE orders (id INTEGER, status TEXT, amount REAL)")
            .execute(&pool)
            .await
            .unwrap();
        for status in ["paid", "paid", "refunded", "shipped"] {
            sqlx::query("INSERT INTO orders (id, status, amount) VALUES (?, ?, ?)")
                .bind(1i64)
                .bind(status)
                .bind(9.5f64)
                .execute(&pool)
                .await
                .unwrap();
        }
        // AUTOINCREMENT makes SQLite register its own `sqlite_sequence` table, which proves the
        // system-table filter is doing more than passing an empty list.
        sqlx::query("CREATE TABLE counters (id INTEGER PRIMARY KEY AUTOINCREMENT, label TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO counters (label) VALUES (?)")
            .bind("orders")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        path
    }

    #[tokio::test]
    async fn reads_the_schema_tables_and_columns_of_a_live_sqlite_file() {
        let path = seeded_file().await;
        let connectors = Connectors::new();
        let spec = sqlite_spec(&path);

        assert!(connectors.test_connection(&spec).await.is_ok());
        assert_eq!(connectors.detect_schema(&spec).await.unwrap(), "main");
        assert_eq!(connectors.list_schemas(&spec).await.unwrap(), vec!["main"]);

        let tables = connectors.list_tables(&spec, "main").await.unwrap();
        let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"orders"), "{names:?}");
        assert!(
            !names.iter().any(|n| n.starts_with("sqlite_")),
            "sqlite_sequence and friends stay hidden: {names:?}"
        );

        let columns = connectors
            .list_columns(&spec, "main", "orders")
            .await
            .unwrap();
        assert_eq!(columns.len(), 3);
        assert_eq!(columns[0].name, "id");
        assert_eq!(columns[0].type_name, "INTEGER");
        assert_eq!(columns[1].name, "status");
        assert_eq!(columns[1].type_name, "TEXT");
        assert_eq!(columns[0].comment, None);
    }

    #[tokio::test]
    async fn samples_distinct_values_as_text() {
        let spec = sqlite_spec(&seeded_file().await);
        let connectors = Connectors::new();
        let mut values = connectors
            .distinct_values(&spec, Some("main"), "orders", "status", 100)
            .await
            .unwrap();
        values.sort();
        assert_eq!(values, vec!["paid", "refunded", "shipped"]);

        let numbers = connectors
            .distinct_values(&spec, Some("main"), "orders", "id", 100)
            .await
            .unwrap();
        assert_eq!(numbers, vec!["1"], "integers come back as text");
    }

    fn read(sql: &str) -> SqlRun<'_> {
        SqlRun {
            sql,
            bindings: Vec::new(),
            reads: true,
            timeout_secs: 30,
            max_rows: 1000,
        }
    }

    #[tokio::test]
    async fn a_query_reports_its_labels_and_its_rows_as_json() {
        let spec = sqlite_spec(&seeded_file().await);
        let connectors = Connectors::new();
        let outcome = connectors
            .run_sql(&spec, &read("SELECT id, status FROM orders"))
            .await
            .unwrap();
        assert_eq!(
            outcome,
            StatementOutcome::Select {
                columns: vec!["id".into(), "status".into()],
                rows: vec![
                    vec![json!(1), json!("paid")],
                    vec![json!(1), json!("paid")],
                    vec![json!(1), json!("refunded")],
                    vec![json!(1), json!("shipped")],
                ],
                total_rows: 4,
            }
        );

        // A real number keeps its number type; a null stays a null.
        let outcome = connectors
            .run_sql(
                &spec,
                &read("SELECT amount, 'x' AS tag, CAST(NULL AS TEXT) AS blank FROM orders LIMIT 1"),
            )
            .await
            .unwrap();
        let StatementOutcome::Select { rows, columns, .. } = outcome else {
            panic!("a SELECT answers with rows: {outcome:?}");
        };
        // NOTHING is a keyword in sqlite, so the null column is aliased out of its way.
        assert_eq!(columns, vec!["amount", "tag", "blank"]);
        assert_eq!(rows[0], vec![json!(9.5), json!("x"), Value::Null]);
    }

    #[tokio::test]
    async fn a_query_is_cut_at_the_row_limit_and_a_limit_of_zero_reads_as_no_limit() {
        let spec = sqlite_spec(&seeded_file().await);
        let connectors = Connectors::new();
        let mut limited = read("SELECT status FROM orders");
        limited.max_rows = 2;
        let outcome = connectors.run_sql(&spec, &limited).await.unwrap();
        let StatementOutcome::Select {
            rows, total_rows, ..
        } = outcome
        else {
            panic!("a SELECT answers with rows: {outcome:?}");
        };
        assert_eq!(
            (total_rows as usize, rows.len()),
            (2, 2),
            "the count is of the rows read"
        );

        let mut unbounded = read("SELECT status FROM orders");
        unbounded.max_rows = 0;
        let total = match connectors.run_sql(&spec, &unbounded).await.unwrap() {
            StatementOutcome::Select { total_rows, .. } => total_rows,
            other => panic!("{other:?}"),
        };
        assert_eq!(total, 4);
    }

    #[tokio::test]
    async fn a_write_reports_the_rows_it_changed_and_binds_its_arguments() {
        let spec = sqlite_spec(&seeded_file().await);
        let connectors = Connectors::new();
        let mut update = SqlRun {
            sql: "UPDATE orders SET status = ? WHERE status = ?",
            bindings: vec![json!("settled"), json!("paid")],
            reads: false,
            timeout_secs: 30,
            max_rows: 1000,
        };
        assert_eq!(
            connectors.run_sql(&spec, &update).await.unwrap(),
            StatementOutcome::Write { affected_rows: 2 }
        );

        update.sql = "DELETE FROM orders WHERE status = ?";
        update.bindings = vec![json!("nothing here")];
        assert_eq!(
            connectors.run_sql(&spec, &update).await.unwrap(),
            StatementOutcome::Write { affected_rows: 0 }
        );
    }

    #[tokio::test]
    async fn a_statement_the_database_rejects_is_a_query_error() {
        let spec = sqlite_spec(&seeded_file().await);
        let connectors = Connectors::new();
        let err = connectors
            .run_sql(&spec, &read("SELECT * FROM not_a_table"))
            .await
            .unwrap_err();
        assert!(matches!(err, ConnectError::Query(_)));
        let err = connectors
            .run_sql(&spec, &read("DROP TABLE"))
            .await
            .unwrap_err();
        assert_eq!(
            err.into_sql_error("execute-sql").code(),
            ErrorCode::DsSqlError
        );
    }

    #[tokio::test]
    async fn a_missing_file_fails_the_connection_instead_of_creating_it() {
        let connectors = Connectors::new();
        let spec = sqlite_spec("D:/definitely/not/here/dati-test.db");
        assert!(connectors.test_connection(&spec).await.is_err());
        let err = connectors.detect_schema(&spec).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::DsConnectionFailed);
        assert_eq!(err.status(), 400);
    }

    /// Nothing listens on port 1, so the driver's refusal is what the console should show. The pool
    /// turned every unreachable address into "pool timed out", which is not a diagnosis. Both pool
    /// shapes are covered: Doris is opened through `sqlx::mysql`, MySQL through `sqlx::any`.
    #[tokio::test]
    async fn a_refused_address_says_so_instead_of_a_pool_timeout() {
        let connectors = Connectors::new();
        for db_type in [DbType::Doris, DbType::MySql] {
            let spec = ConnectionSpec {
                db_type,
                url: "mysql://127.0.0.1:1/gip_ods".into(),
                user: Some("gip".into()),
                password: Some("secret-pw".into()),
            };
            let reason = connectors
                .test_connection(&spec)
                .await
                .unwrap_err()
                .reason();
            assert!(
                !reason.contains("pool timed out"),
                "the pool's own timeout is not a diagnosis: {reason}"
            );
            assert!(
                reason.contains("os error"),
                "driver reason expected: {reason}"
            );
            assert!(
                !reason.contains("secret-pw") && !reason.contains("gip_ods"),
                "the credentials and the database name stay out of the message: {reason}"
            );
        }
    }

    #[tokio::test]
    async fn unknown_database_type_reports_unsupported() {
        let err = connection_url(&ConnectionSpec {
            db_type: DbType::MySql,
            url: "clickhouse://h:8123/db".into(),
            user: None,
            password: None,
        })
        .unwrap_err();
        assert_eq!(err.code(), ErrorCode::DsUnsupportedType);
    }

    #[tokio::test]
    async fn pools_are_reused_and_evicted_by_credential() {
        let path = seeded_file().await;
        let connectors = Connectors::new();
        let spec = sqlite_spec(&path);
        assert!(connectors.test_connection(&spec).await.is_ok());
        assert_eq!(connectors.pools.lock().await.len(), 1);
        assert!(
            connectors.test_connection(&spec).await.is_ok(),
            "the second call reuses the pool"
        );
        assert_eq!(connectors.pools.lock().await.len(), 1);

        connectors.evict(&spec).await;
        assert!(connectors.pools.lock().await.is_empty());
    }

    #[test]
    fn sanitising_strips_url_credentials() {
        assert_eq!(
            sanitize("connect mysql://root:secret@h:3306/db refused"),
            "connect mysql://*** refused"
        );
        assert_eq!(
            sanitize("Access denied for user 'root'"),
            "Access denied for user 'root'"
        );
    }

    #[test]
    fn spec_needs_a_url() {
        assert!(spec_of(DbType::Sqlite, None, None, None).is_none());
        assert!(spec_of(DbType::Sqlite, Some("  "), None, None).is_none());
        assert_eq!(
            spec_of(DbType::Sqlite, Some("a.db"), Some("u"), Some("p")).unwrap(),
            ConnectionSpec {
                db_type: DbType::Sqlite,
                url: "a.db".into(),
                user: Some("u".into()),
                password: Some("p".into()),
            }
        );
    }
}
