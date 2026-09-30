//! Per-database SQL for reading live metadata, plus the connection URL translation the
//! connectors hand to `sqlx::any`.

use crate::error::{DatiError, ErrorCode, Result};
use crate::model::datasource::{ConnectionSpec, DbType};

/// The three metadata dialects DatI reads. MySQL, MariaDB and Doris all speak MySQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    MySql,
    Postgres,
    Sqlite,
}

pub const fn family(db_type: DbType) -> Family {
    match db_type {
        DbType::MySql | DbType::Mariadb | DbType::Doris => Family::MySql,
        DbType::PostgreSql => Family::Postgres,
        DbType::Sqlite => Family::Sqlite,
    }
}

/// Whether the source has to be opened through `sqlx::mysql` instead of `sqlx::any`.
///
/// Doris speaks MySQL but refuses what `sqlx::any` makes its MySQL driver do on the way in: the
/// driver always sends its session setup, and Doris answers `Set statement does't support
/// non-constant expr.` for the `sql_mode=(SELECT CONCAT(...))` part of it. See [`crate::connector::live`].
pub const fn typed_mysql(db_type: DbType) -> bool {
    matches!(db_type, DbType::Doris)
}

impl Family {
    /// Delimits an identifier we cannot bind as a parameter (table and column names coming from
    /// live metadata or from the sync endpoints). Any delimiter inside the name is doubled.
    pub const fn quote_char(self) -> char {
        match self {
            Self::MySql => '`',
            Self::Postgres | Self::Sqlite => '"',
        }
    }

    pub fn quote(self, identifier: &str) -> String {
        let delimiter = self.quote_char();
        let escaped = identifier.replace(delimiter, &delimiter.to_string().repeat(2));
        format!("{delimiter}{escaped}{delimiter}")
    }

    /// Schema-qualified table reference. SQLite has one attached schema, so the name stands alone.
    pub fn qualified_table(self, schema: Option<&str>, table: &str) -> String {
        let table = self.quote(table);
        match self {
            Self::Sqlite => table,
            _ => match schema {
                Some(schema) if !schema.trim().is_empty() => {
                    format!("{}.{}", self.quote(schema), table)
                }
                _ => table,
            },
        }
    }

    fn cast_to_text(self) -> &'static str {
        match self {
            Self::MySql => "CAST(?c AS CHAR)",
            Self::Postgres | Self::Sqlite => "CAST(?c AS TEXT)",
        }
    }
}

/// The URL `sqlx::any` needs, derived from the connection URL the console sends.
///
/// Credentials are injected into the authority because sqlx takes them in the URL, the way a JDBC
/// driver takes them as arguments; anything the caller already put there wins.
pub fn connection_url(spec: &ConnectionSpec) -> Result<String> {
    let raw = spec.url.trim();
    match family(spec.db_type) {
        Family::Sqlite => Ok(sqlite_url(raw)),
        Family::MySql | Family::Postgres => hosted_url(spec, raw),
    }
}

fn hosted_url(spec: &ConnectionSpec, raw: &str) -> Result<String> {
    let without_prefix = trim_jdbc_prefix(raw);
    let (scheme, rest) = without_prefix.split_once("://").ok_or_else(|| {
        DatiError::new(
            ErrorCode::DsConnectionFailed,
            [format!("malformed connection url: {raw}")],
        )
    })?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "mysql"
        && scheme != "mariadb"
        && scheme != "doris"
        && scheme != "postgresql"
        && scheme != "postgres"
    {
        return Err(DatiError::new(ErrorCode::DsUnsupportedType, [scheme]));
    }
    // sqlx accepts the mariadb scheme only through its MySQL driver; postgresql/postgres both work,
    // and a `doris:` address is a MySQL address with the Doris switches off.
    let scheme = match scheme.as_str() {
        "postgresql" | "postgres" => "postgres".to_string(),
        "doris" => "mysql".to_string(),
        other => other.to_string(),
    };
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, String::new()),
    };
    let credentials = match (spec.user.as_deref(), spec.password.as_deref()) {
        (Some(user), _) if !user.trim().is_empty() && !authority.contains('@') => {
            let mut prefix = crate::db::percent_encode(user);
            if let Some(password) = spec.password.as_deref().filter(|p| !p.is_empty()) {
                prefix.push(':');
                prefix.push_str(&crate::db::percent_encode(password));
            }
            prefix.push('@');
            prefix
        }
        _ => String::new(),
    };
    Ok(format!("{scheme}://{credentials}{authority}{path}"))
}

fn sqlite_url(raw: &str) -> String {
    let mut body = trim_jdbc_prefix(raw);
    if let Some(stripped) = body.strip_prefix("sqlite:") {
        body = stripped;
    }
    // `jdbc:sqlite://C:/x.db` and `sqlite:C:/x.db` name the same file.
    while let Some(stripped) = body.strip_prefix("//") {
        body = stripped;
    }
    let (path, query) = match body.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (body, None),
    };
    // A leading slash is part of a unix absolute path; only the `//` authority marker is dropped.
    let path = path.to_string();
    // Read-write but never created: pointing DatI at a missing file must fail instead of
    // silently registering an empty database.
    let mode = if path.starts_with(':') { "" } else { "mode=rw" };
    match query {
        Some(query) if !query.is_empty() => format!("sqlite:{path}?{query}"),
        // A named in-memory database takes no `mode`, and an empty query string would be `?` alone.
        _ if mode.is_empty() => format!("sqlite:{path}"),
        _ => format!("sqlite:{path}?{mode}"),
    }
}

/// The `jdbc:` prefix is tolerated on input only, so a URL copied out of an old Java deployment
/// still connects. Nothing the server itself stores, returns or displays uses one.
fn trim_jdbc_prefix(value: &str) -> &str {
    match value.strip_prefix("jdbc:") {
        Some(rest) => rest,
        None => value,
    }
}

/// The database behind a URL, for the connection test: the Java endpoint let `DriverManager` pick
/// the driver from the scheme alone, so a missing `type` in the body still connects.
pub fn infer_db_type(url: &str) -> Option<DbType> {
    let scheme = trim_jdbc_prefix(url)
        .split(':')
        .next()?
        .to_ascii_lowercase();
    match scheme.as_str() {
        "sqlite" => Some(DbType::Sqlite),
        "mysql" | "mariadb" => Some(DbType::MySql),
        "doris" => Some(DbType::Doris),
        "postgresql" | "postgres" => Some(DbType::PostgreSql),
        _ => None,
    }
}

/// `SELECT` used to discover the schema a connection lands in by default.
pub fn current_schema_sql(family: Family) -> &'static str {
    match family {
        Family::MySql => "SELECT DATABASE()",
        Family::Postgres => "SELECT current_schema()",
        Family::Sqlite => "PRAGMA database_list",
    }
}

/// Which column of `current_schema_sql` carries the answer.
pub const fn current_schema_column(family: Family) -> usize {
    match family {
        Family::Sqlite => 1,
        _ => 0,
    }
}

/// Schema (MySQL-family: catalog) names offered to the picker.
pub fn schemas_sql(family: Family) -> &'static str {
    match family {
        Family::MySql => "SHOW DATABASES",
        // pgjdbc hides the two system schemas from getSchemas(); match it so the picker stays usable.
        Family::Postgres => {
            "SELECT nspname FROM pg_namespace \
             WHERE nspname NOT IN ('pg_catalog', 'information_schema') AND nspname NOT LIKE 'pg_toast%' \
             ORDER BY nspname"
        }
        Family::Sqlite => "PRAGMA database_list",
    }
}

/// Whether `schemas_sql` results must be filtered (sqlite reports the temp schema too).
pub const fn schemas_skip_temp(family: Family) -> bool {
    matches!(family, Family::Sqlite)
}

/// `name, comment` for every table and view of one schema.
pub fn tables_sql(family: Family) -> &'static str {
    match family {
        Family::MySql => {
            "SELECT table_name, table_comment FROM information_schema.tables \
             WHERE table_schema = ? AND table_type IN ('BASE TABLE', 'VIEW') ORDER BY table_name"
        }
        // r ordinary, p partitioned, v view, m materialized, f foreign table.
        Family::Postgres => {
            "SELECT c.relname, COALESCE(obj_description(c.oid), '') FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = ? AND c.relkind IN ('r', 'p', 'v', 'm', 'f') ORDER BY c.relname"
        }
        Family::Sqlite => {
            "SELECT name, '' FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
             ORDER BY name"
        }
    }
}

/// `table, name, type, comment` for the columns of several tables of one schema, in one statement.
///
/// Registering the tables the picker selected needs their columns, and a remote engine answers a
/// metadata query slowly enough that one per table runs past the console's own timeout. Every family
/// therefore reports the same four columns, in the same order, so the reader has one row shape: a
/// SQLite source reads the same metadata through its `pragma_table_info` table-valued function.
pub fn columns_batch_sql(family: Family, tables: usize) -> String {
    let list = placeholder_list(tables);
    match family {
        Family::MySql => format!(
            "SELECT table_name, column_name, UPPER(data_type), column_comment \
             FROM information_schema.columns WHERE table_schema = ? AND table_name IN ({list}) \
             ORDER BY table_name, ordinal_position"
        ),
        Family::Postgres => format!(
            "SELECT c.relname, a.attname, t.typname, COALESCE(col_description(a.attrelid, a.attnum), '') \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_attribute a ON a.attrelid = c.oid JOIN pg_type t ON t.oid = a.atttypid \
             WHERE n.nspname = ? AND c.relname IN ({list}) AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY c.relname, a.attnum"
        ),
        // The type keeps the case it was declared with, the way `PRAGMA table_info` reported it.
        Family::Sqlite => format!(
            "SELECT m.name, p.name, p.type, '' FROM sqlite_master m, pragma_table_info(m.name) p \
             WHERE m.type = 'table' AND m.name IN ({list}) ORDER BY m.name, p.cid"
        ),
    }
}

/// `?, ?, ?` for an `IN` list of `count` values.
fn placeholder_list(count: usize) -> String {
    (0..count).map(|_| "?").collect::<Vec<_>>().join(", ")
}

/// The distinct-value sampling behind column value extraction.
pub fn distinct_values_sql(
    family: Family,
    schema: Option<&str>,
    table: &str,
    column: &str,
    limit: i64,
) -> String {
    let cast = family.cast_to_text().replace("?c", &family.quote(column));
    format!(
        "SELECT DISTINCT {cast} FROM {} WHERE {} IS NOT NULL LIMIT {}",
        family.qualified_table(schema, table),
        family.quote(column),
        limit.max(1)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(
        db_type: DbType,
        url: &str,
        user: Option<&str>,
        password: Option<&str>,
    ) -> ConnectionSpec {
        ConnectionSpec {
            db_type,
            url: url.into(),
            user: user.map(str::to_string),
            password: password.map(str::to_string),
        }
    }

    #[test]
    fn families_map_connectors_to_sql() {
        assert_eq!(family(DbType::Doris), Family::MySql);
        assert_eq!(family(DbType::Mariadb), Family::MySql);
        assert_eq!(family(DbType::PostgreSql), Family::Postgres);
        assert_eq!(family(DbType::Sqlite), Family::Sqlite);
        assert_eq!(current_schema_sql(Family::MySql), "SELECT DATABASE()");
        assert_eq!(
            current_schema_sql(Family::Postgres),
            "SELECT current_schema()"
        );
        assert_eq!(current_schema_column(Family::Sqlite), 1);
    }

    /// Only Doris is taken off the shared `sqlx::any` path; every other source keeps it.
    #[test]
    fn only_doris_is_opened_through_the_mysql_driver() {
        assert!(typed_mysql(DbType::Doris));
        for db_type in [
            DbType::MySql,
            DbType::Mariadb,
            DbType::PostgreSql,
            DbType::Sqlite,
        ] {
            assert!(!typed_mysql(db_type), "{db_type:?} stays on sqlx::any");
        }
    }

    #[test]
    fn driver_urls_become_the_url_sqlx_needs() {
        assert_eq!(
            connection_url(&spec(
                DbType::MySql,
                "mysql://127.0.0.1:3306/dati",
                Some("root"),
                Some("secret")
            ))
            .unwrap(),
            "mysql://root:secret@127.0.0.1:3306/dati"
        );
        assert_eq!(
            connection_url(&spec(
                DbType::PostgreSql,
                "postgresql://h:5432/dw",
                Some("dati"),
                None
            ))
            .unwrap(),
            "postgres://dati@h:5432/dw"
        );
        // A URL copied out of a Java deployment still connects.
        assert_eq!(
            connection_url(&spec(DbType::MySql, "jdbc:mysql://h:3306/dati", None, None)).unwrap(),
            "mysql://h:3306/dati"
        );
    }

    /// A `doris:` address is the MySQL protocol with the session setup switched off, so the URL the
    /// driver takes is a `mysql:` one while the type inferred from it stays Doris.
    #[test]
    fn a_doris_address_is_a_mysql_address() {
        assert_eq!(
            connection_url(&spec(
                DbType::Doris,
                "doris://h:9030/dw",
                Some("user"),
                Some("pass")
            ))
            .unwrap(),
            "mysql://user:pass@h:9030/dw"
        );
        assert_eq!(infer_db_type("doris://h:9030/dw"), Some(DbType::Doris));
        assert_eq!(infer_db_type("mysql://h:9030/dw"), Some(DbType::MySql));
    }

    #[test]
    fn url_credentials_keep_their_special_characters() {
        assert_eq!(
            connection_url(&spec(
                DbType::MySql,
                "mysql://h/db",
                Some("root"),
                Some("p@ss:w/rd")
            ))
            .unwrap(),
            "mysql://root:p%40ss%3Aw%2Frd@h/db"
        );
        // A caller that already embedded credentials is not rewritten.
        assert_eq!(
            connection_url(&spec(DbType::MySql, "mysql://a:b@h/db", Some("x"), None)).unwrap(),
            "mysql://a:b@h/db"
        );
    }

    #[test]
    fn sqlite_urls_point_at_a_file_without_creating_it() {
        assert_eq!(
            connection_url(&spec(DbType::Sqlite, "sqlite:D:/tmp/demo.db", None, None)).unwrap(),
            "sqlite:D:/tmp/demo.db?mode=rw"
        );
        // The prefixed form an operator copies from a Java configuration names the same file.
        assert_eq!(
            connection_url(&spec(
                DbType::Sqlite,
                "jdbc:sqlite://D:/tmp/demo.db",
                None,
                None
            ))
            .unwrap(),
            "sqlite:D:/tmp/demo.db?mode=rw"
        );
        assert_eq!(
            connection_url(&spec(DbType::Sqlite, "sqlite:/var/lib/demo.db", None, None)).unwrap(),
            "sqlite:/var/lib/demo.db?mode=rw"
        );
        assert_eq!(
            connection_url(&spec(DbType::Sqlite, "sqlite:demo.db?mode=rwc", None, None)).unwrap(),
            "sqlite:demo.db?mode=rwc"
        );
        assert_eq!(
            connection_url(&spec(DbType::Sqlite, "sqlite::memory:", None, None)).unwrap(),
            "sqlite::memory:"
        );
    }

    #[test]
    fn unsupported_schemes_are_rejected() {
        let err = connection_url(&spec(DbType::MySql, "jdbc:oracle://h:1521/db", None, None))
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::DsUnsupportedType);
        assert_eq!(err.status(), 400);
        assert_eq!(
            connection_url(&spec(DbType::MySql, "not-a-url", None, None))
                .unwrap_err()
                .code(),
            ErrorCode::DsConnectionFailed
        );
    }

    #[test]
    fn identifiers_are_quoted_per_dialect() {
        assert_eq!(Family::MySql.quote("orders"), "`orders`");
        assert_eq!(Family::Postgres.quote("or\"ders"), "\"or\"\"ders\"");
        assert_eq!(
            Family::Postgres.qualified_table(Some("public"), "orders"),
            "\"public\".\"orders\""
        );
        assert_eq!(
            Family::Sqlite.qualified_table(Some("main"), "orders"),
            "\"orders\""
        );
        assert_eq!(
            Family::MySql.qualified_table(Some(" "), "orders"),
            "`orders`",
            "a blank schema is no schema at all"
        );
    }

    #[test]
    fn sampling_casts_values_to_text_and_limits_rows() {
        assert_eq!(
            distinct_values_sql(Family::MySql, Some("dati"), "orders", "status", 100),
            "SELECT DISTINCT CAST(`status` AS CHAR) FROM `dati`.`orders` WHERE `status` IS NOT NULL LIMIT 100"
        );
        assert_eq!(
            distinct_values_sql(Family::Sqlite, Some("main"), "orders", "status", 0),
            "SELECT DISTINCT CAST(\"status\" AS TEXT) FROM \"orders\" WHERE \"status\" IS NOT NULL LIMIT 1"
        );
    }

    #[test]
    fn every_family_has_metadata_queries() {
        for f in [Family::MySql, Family::Postgres, Family::Sqlite] {
            assert!(!tables_sql(f).is_empty());
            assert!(!columns_batch_sql(f, 1).is_empty());
            assert!(!schemas_sql(f).is_empty());
        }
        assert!(schemas_skip_temp(Family::Sqlite));
        assert!(!schemas_skip_temp(Family::MySql));
    }

    /// The batched column read answers with four columns in every family, takes one placeholder per
    /// table name, and binds a schema only where there is one to bind.
    #[test]
    fn the_batched_column_read_binds_the_schema_then_the_table_names() {
        assert_eq!(
            columns_batch_sql(Family::MySql, 2),
            "SELECT table_name, column_name, UPPER(data_type), column_comment \
             FROM information_schema.columns WHERE table_schema = ? AND table_name IN (?, ?) \
             ORDER BY table_name, ordinal_position"
        );
        // A SQLite source has one attached schema, so its statement carries no schema placeholder.
        let sqlite = columns_batch_sql(Family::Sqlite, 1);
        assert!(
            sqlite.contains("pragma_table_info(m.name)"),
            "PRAGMA has no table to join: {sqlite}"
        );
        assert!(sqlite.contains("IN (?)"), "{sqlite}");
        assert!(!sqlite.contains("table_schema"), "{sqlite}");
        assert!(columns_batch_sql(Family::Postgres, 3).contains("IN (?, ?, ?)"));
    }
}
