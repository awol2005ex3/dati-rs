//! Repositories: hand-written SQL over the shared metadata store.

pub mod acl;
pub mod api_key;
pub mod column;
pub mod column_value;
pub mod datasource;
pub mod mcp_audit;
pub mod mcp_invocation_log;
pub mod mcp_prompt;
pub mod mcp_service;
pub mod mcp_snapshot;
pub mod mcp_tool;
pub mod mcp_usage_stat;
pub mod resource;
pub mod subject;
pub mod table;
pub mod term;
pub mod user;

use sqlx::Row;
use sqlx::any::AnyRow;

use crate::db::Db;
use crate::error::{DatiError, ErrorCode, Result};

/// A bindable value. `sqlx::any` erases argument types, so a statement built at run time needs a
/// small enum to remember what each placeholder carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bind {
    Text(Option<String>),
    Bool(bool),
    Int(i64),
    /// A numeric column the domain model allows to stay empty, such as the published version of a
    /// service that has never been published.
    IntOption(Option<i64>),
}

impl From<String> for Bind {
    fn from(value: String) -> Self {
        Self::Text(Some(value))
    }
}

impl From<&str> for Bind {
    fn from(value: &str) -> Self {
        Self::Text(Some(value.to_string()))
    }
}

impl From<Option<String>> for Bind {
    fn from(value: Option<String>) -> Self {
        Self::Text(value)
    }
}

impl From<bool> for Bind {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<i64> for Bind {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}

impl From<Option<i64>> for Bind {
    fn from(value: Option<i64>) -> Self {
        Self::IntOption(value)
    }
}

/// A `?`-style statement whose text and bindings are assembled together, so a run-time filter can
/// be executed against any of the three metadata dialects.
#[derive(Debug, Clone, Default)]
pub struct Statement {
    text: String,
    binds: Vec<Bind>,
}

impl Statement {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            binds: Vec::new(),
        }
    }

    /// Appends raw SQL; every fragment here is built from constants, never from user input.
    pub fn push_text(mut self, extra: &str) -> Self {
        self.text.push_str(extra);
        self
    }

    pub fn bind(mut self, bind: impl Into<Bind>) -> Self {
        self.binds.push(bind.into());
        self
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub async fn fetch_all(&self, db: &Db) -> Result<Vec<AnyRow>> {
        let text = db.sql(&self.text);
        Ok(self.query(&text).fetch_all(db.pool()).await?)
    }

    pub async fn fetch_one(&self, db: &Db) -> Result<AnyRow> {
        let text = db.sql(&self.text);
        Ok(self.query(&text).fetch_one(db.pool()).await?)
    }

    pub async fn fetch_optional(&self, db: &Db) -> Result<Option<AnyRow>> {
        let text = db.sql(&self.text);
        Ok(self.query(&text).fetch_optional(db.pool()).await?)
    }

    pub async fn execute(&self, db: &Db) -> Result<u64> {
        let text = db.sql(&self.text);
        let result = self.query(&text).execute(db.pool()).await?;
        Ok(result.rows_affected())
    }

    /// Attaches the bindings to the dialect-rewritten statement text. `sqlx::any` erases the
    /// argument types, which is why the bindings are kept alongside the text.
    fn query<'q>(
        &'q self,
        text: &'q str,
    ) -> sqlx::query::Query<'q, sqlx::any::Any, <sqlx::any::Any as sqlx::Database>::Arguments<'q>>
    {
        let mut query = sqlx::query(text);
        for bind in &self.binds {
            query = match bind {
                Bind::Text(value) => query.bind(value.as_deref()),
                Bind::Bool(value) => query.bind(*value),
                Bind::Int(value) => query.bind(*value),
                Bind::IntOption(value) => query.bind(*value),
            };
        }
        query
    }
}

/// A `WHERE` fragment and its bindings, shared by a paged list and its `COUNT` twin so the two can
/// never disagree about what is visible.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    clauses: Vec<String>,
    binds: Vec<Bind>,
}

impl Filter {
    pub fn add(mut self, clause: &str, binds: impl IntoIterator<Item = Bind>) -> Self {
        self.clauses.push(clause.to_string());
        self.binds.extend(binds);
        self
    }

    fn where_text(&self) -> String {
        if self.clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", self.clauses.join(" AND "))
        }
    }

    /// `SELECT <columns> FROM <source><where>`, with the filter's bindings already attached.
    pub fn select(&self, columns: &str, source: &str) -> Statement {
        let mut statement = Statement::new(format!(
            "SELECT {columns} FROM {source}{}",
            self.where_text()
        ));
        for bind in self.binds.clone() {
            statement = statement.bind(bind);
        }
        statement
    }

    pub fn count(&self, source: &str) -> Statement {
        self.select("COUNT(*) AS total", source)
    }
}

/// Runs a `SELECT COUNT(*) AS total` statement.
pub async fn count(statement: &Statement, db: &Db) -> Result<i64> {
    let row = statement.fetch_one(db).await?;
    Ok(row.try_get::<i64, _>("total").unwrap_or_default())
}

/// A text column that must be present; a null means the row was written by a buggy code path.
pub fn required(row: &AnyRow, column: &str) -> Result<String> {
    optional(row, column).ok_or_else(|| {
        tracing::error!(column, "required column is null");
        DatiError::with_message(ErrorCode::InternalError, "Internal server error")
    })
}

/// A text column as `Option`, mapping empty strings to `None` so JSON stays clean.
pub fn optional(row: &AnyRow, column: &str) -> Option<String> {
    match row.try_get::<Option<String>, _>(column) {
        Ok(Some(value)) if !value.is_empty() => Some(value),
        Ok(_) => None,
        // MySQL reports VARCHAR as String, sqlite as String too; a &str read covers the rest.
        Err(_) => row
            .try_get::<String, _>(column)
            .ok()
            .filter(|v| !v.is_empty()),
    }
}

/// A numeric column read as `i64`: sqlite hands back an integer, postgres a bigint, and mysql can
/// answer with a smaller width for a column declared `INT`.
pub fn integer(row: &AnyRow, column: &str) -> i64 {
    row.try_get::<i64, _>(column)
        .or_else(|_| row.try_get::<i32, _>(column).map(i64::from))
        .unwrap_or_default()
}

/// Soft-delete and enablement flags: sqlite and mysql return 0/1 integers, postgres a bool.
pub fn flag(row: &AnyRow, column: &str) -> bool {
    if let Ok(value) = row.try_get::<bool, _>(column) {
        return value;
    }
    row.try_get::<i64, _>(column)
        .or_else(|_| row.try_get::<i32, _>(column).map(i64::from))
        .unwrap_or(0)
        != 0
}

/// A loose-match clause over several text columns, one branch per keyword: a row hits if any
/// keyword appears in any column, which is the OR semantics of the Elasticsearch multi-match the
/// Java search ran. Lower-casing both sides keeps the match case-insensitive on every dialect.
pub fn keyword_match(keywords: &[String], columns: &[&str]) -> (String, Vec<Bind>) {
    let mut groups = Vec::new();
    let mut binds = Vec::new();
    for keyword in keywords {
        let pattern = format!("%{}%", keyword.to_lowercase());
        let branches = columns
            .iter()
            .map(|column| format!("LOWER({column}) LIKE ?"));
        groups.push(format!("({})", branches.collect::<Vec<_>>().join(" OR ")));
        binds.extend(columns.iter().map(|_| pattern.clone().into()));
    }
    (format!("({})", groups.join(" OR ")), binds)
}

/// `<column> IN (?, ?, …)` for a batch read, with the matching bindings. Callers guard against an
/// empty list, which no dialect accepts as an `IN` argument list.
pub fn in_clause(column: &str, ids: &[String]) -> (String, Vec<Bind>) {
    let placeholders = vec!["?"; ids.len()].join(", ");
    (
        format!("{column} IN ({placeholders})"),
        ids.iter().cloned().map(Bind::from).collect(),
    )
}

#[cfg(test)]
mod tests {
    use crate::db::Db;
    use crate::error::ErrorCode;

    #[tokio::test]
    async fn flag_and_optional_read_a_real_sqlite_row() {
        let db = Db::sqlite_temp().await;
        let sql =
            db.sql("INSERT INTO users (id, name, password, display_name) VALUES (?, ?, ?, ?)");
        sqlx::query(&sql)
            .bind("u-1")
            .bind("alice")
            .bind("hash")
            .bind("")
            .execute(db.pool())
            .await
            .unwrap();
        let select = db.sql("SELECT name, display_name FROM users WHERE id = ?");
        let row = sqlx::query(&select)
            .bind("u-1")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(super::required(&row, "name").unwrap(), "alice");
        assert_eq!(super::optional(&row, "display_name"), None);
        assert_eq!(
            super::required(&row, "missing_column").unwrap_err().code(),
            ErrorCode::InternalError
        );
        assert!(!super::flag(&row, "missing_column"));
    }
}
