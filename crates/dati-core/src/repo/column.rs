//! The `column_info` table: columns registered from a live table.

use sqlx::any::AnyRow;

use crate::db::{Db, read_json_list};
use crate::error::Result;
use crate::model::base::{Audit, PageReq, write_json_list};
use crate::model::datasource::ColumnInfo;

use super::{Filter, Statement, count, flag, in_clause, keyword_match, optional, required};

const COLUMNS: &str = "id, created_by, created_at, updated_by, updated_at, name, description, \
     deleted, table_id, column_type, aliases, extract_value_enabled";

const TABLE: &str = "column_info";

pub fn to_column(row: &AnyRow) -> Result<ColumnInfo> {
    Ok(ColumnInfo {
        id: required(row, "id")?,
        name: optional(row, "name"),
        description: optional(row, "description"),
        deleted: flag(row, "deleted"),
        table_id: required(row, "table_id")?,
        column_type: optional(row, "column_type"),
        aliases: read_json_list(row, "aliases"),
        extract_value_enabled: flag(row, "extract_value_enabled"),
        audit: Audit {
            created_by: optional(row, "created_by"),
            created_at: optional(row, "created_at"),
            updated_by: optional(row, "updated_by"),
            updated_at: optional(row, "updated_at"),
        },
    })
}

pub async fn insert(db: &Db, column: &ColumnInfo) -> Result<String> {
    Statement::new(format!(
        "INSERT INTO {TABLE} (id, created_by, created_at, updated_by, updated_at, name, description, \
         deleted, table_id, column_type, aliases, extract_value_enabled) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    ))
    .bind(column.id.clone())
    .bind(column.audit.created_by.clone())
    .bind(column.audit.created_at.clone())
    .bind(column.audit.updated_by.clone())
    .bind(column.audit.updated_at.clone())
    .bind(column.name.clone())
    .bind(column.description.clone())
    .bind(column.deleted)
    .bind(column.table_id.clone())
    .bind(column.column_type.clone())
    .bind(write_json_list(&column.aliases))
    .bind(column.extract_value_enabled)
    .execute(db)
    .await?;
    Ok(column.id.clone())
}

/// Writes every mutable column, including `extract_value_enabled`.
pub async fn update(db: &Db, column: &ColumnInfo) -> Result<()> {
    Statement::new(format!(
        "UPDATE {TABLE} SET name = ?, description = ?, deleted = ?, column_type = ?, aliases = ?, \
         extract_value_enabled = ?, updated_by = ?, updated_at = ? WHERE id = ?"
    ))
    .bind(column.name.clone())
    .bind(column.description.clone())
    .bind(column.deleted)
    .bind(column.column_type.clone())
    .bind(write_json_list(&column.aliases))
    .bind(column.extract_value_enabled)
    .bind(column.audit.updated_by.clone())
    .bind(column.audit.updated_at.clone())
    .bind(column.id.clone())
    .execute(db)
    .await?;
    Ok(())
}

pub async fn find(db: &Db, id: &str) -> Result<Option<ColumnInfo>> {
    let row = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE id = ? AND deleted = ?"
    ))
    .bind(id)
    .bind(false)
    .fetch_optional(db)
    .await?;
    row.as_ref().map(to_column).transpose()
}

/// Columns in registration order, which is the database's own declaration order after a sync.
pub async fn list_by_table(db: &Db, table_id: &str) -> Result<Vec<ColumnInfo>> {
    let rows = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE table_id = ? AND deleted = ? ORDER BY created_at ASC, id ASC"
    ))
    .bind(table_id)
    .bind(false)
    .fetch_all(db)
    .await?;
    rows.iter().map(to_column).collect()
}

/// Every column of a batch of tables, in registration order: what a search hit reads in one go.
pub async fn list_by_tables(db: &Db, table_ids: &[String]) -> Result<Vec<ColumnInfo>> {
    if table_ids.is_empty() {
        return Ok(Vec::new());
    }
    let (clause, binds) = in_clause("table_id", table_ids);
    let mut statement = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE deleted = ? AND {clause} ORDER BY created_at ASC, id ASC"
    ));
    statement = statement.bind(false);
    for bind in binds {
        statement = statement.bind(bind);
    }
    let rows = statement.fetch_all(db).await?;
    rows.iter().map(to_column).collect()
}

/// The columns whose name, description or aliases carry one of the keywords: the Java `FIELD`
/// documents of the search index.
pub async fn matching(
    db: &Db,
    table_ids: &[String],
    keywords: &[String],
) -> Result<Vec<ColumnInfo>> {
    if table_ids.is_empty() || keywords.is_empty() {
        return Ok(Vec::new());
    }
    let (clause, binds) = in_clause("table_id", table_ids);
    let (pattern, pattern_binds) = keyword_match(keywords, &["name", "description", "aliases"]);
    let mut statement = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE deleted = ? AND {clause} AND {pattern} \
         ORDER BY created_at ASC, id ASC"
    ));
    statement = statement.bind(false);
    for bind in binds.into_iter().chain(pattern_binds) {
        statement = statement.bind(bind);
    }
    let rows = statement.fetch_all(db).await?;
    rows.iter().map(to_column).collect()
}

pub async fn page(
    db: &Db,
    table_id: &str,
    keyword: Option<&str>,
    page: PageReq,
) -> Result<(Vec<ColumnInfo>, i64)> {
    let mut filter = Filter::default()
        .add("deleted = ?", [false.into()])
        .add("table_id = ?", [table_id.into()]);
    if let Some(keyword) = keyword.filter(|v| !v.trim().is_empty()) {
        filter = filter.add(
            "LOWER(name) LIKE ?",
            [format!("%{}%", keyword.to_lowercase()).into()],
        );
    }
    let total = count(&filter.count(TABLE), db).await?;
    let rows = filter
        .select(COLUMNS, TABLE)
        // The Java list sorted by creation only; ties fall back to the id so a page is stable.
        .push_text(" ORDER BY created_at ASC, id ASC LIMIT ? OFFSET ?")
        .bind(page.size)
        .bind(page.offset())
        .fetch_all(db)
        .await?;
    Ok((
        rows.iter().map(to_column).collect::<Result<Vec<_>>>()?,
        total,
    ))
}

pub async fn remove_by_ids(db: &Db, ids: &[String]) -> Result<u64> {
    let mut removed = 0u64;
    for id in ids {
        removed += Statement::new(format!("DELETE FROM {TABLE} WHERE id = ?"))
            .bind(id.clone())
            .execute(db)
            .await?;
    }
    Ok(removed)
}

pub async fn remove_by_table(db: &Db, table_id: &str) -> Result<u64> {
    let affected = Statement::new(format!("DELETE FROM {TABLE} WHERE table_id = ?"))
        .bind(table_id)
        .execute(db)
        .await?;
    Ok(affected)
}

/// The cascade a data source or table delete needs; an empty list stays inside the database.
pub async fn remove_by_table_ids(db: &Db, table_ids: &[String]) -> Result<u64> {
    let mut removed = 0u64;
    for table_id in table_ids {
        removed += remove_by_table(db, table_id).await?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::new_id;

    fn column(table_id: &str, name: &str) -> ColumnInfo {
        ColumnInfo {
            id: new_id(),
            name: Some(name.into()),
            description: Some("主键".into()),
            deleted: false,
            table_id: table_id.into(),
            column_type: Some("BIGINT".into()),
            aliases: vec!["id".into()],
            extract_value_enabled: false,
            audit: Audit::new("u-1"),
        }
    }

    /// `created_at` only has second precision, so paging tests stamp an explicit order.
    fn column_at(table_id: &str, name: &str, ordinal: i64) -> ColumnInfo {
        let mut row = column(table_id, name);
        let stamp = format!("2026-01-01T00:00:0{ordinal}Z");
        row.audit.created_at = Some(stamp.clone());
        row.audit.updated_at = Some(stamp);
        row
    }

    #[tokio::test]
    async fn insert_find_and_update_a_column() {
        let db = Db::sqlite_temp().await;
        let table_id = new_id();
        let row = column(&table_id, "order_id");
        insert(&db, &row).await.unwrap();
        assert_eq!(find(&db, &row.id).await.unwrap().unwrap(), row);

        let changed = ColumnInfo {
            extract_value_enabled: true,
            column_type: Some("TEXT".into()),
            aliases: Vec::new(),
            ..row.clone()
        };
        update(&db, &changed).await.unwrap();
        let found = find(&db, &row.id).await.unwrap().unwrap();
        assert!(found.extract_value_enabled);
        assert_eq!(found.column_type.as_deref(), Some("TEXT"));
        assert!(
            found.aliases.is_empty(),
            "an empty alias list clears the column"
        );
    }

    #[tokio::test]
    async fn paging_is_scoped_to_the_table_and_oldest_first() {
        let db = Db::sqlite_temp().await;
        let mine = new_id();
        let mut ids = Vec::new();
        for (ordinal, name) in ["a", "b", "c"].iter().enumerate() {
            let row = column_at(&mine, name, ordinal as i64);
            ids.push(row.id.clone());
            insert(&db, &row).await.unwrap();
        }
        insert(&db, &column_at(&new_id(), "elsewhere", 9))
            .await
            .unwrap();
        let (rows, total) = page(&db, &mine, None, PageReq { page: 1, size: 2 })
            .await
            .unwrap();
        assert_eq!(total, 3);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name.as_deref(), Some("a"), "declaration order wins");

        let (hits, _) = page(&db, &mine, Some("C"), PageReq { page: 1, size: 10 })
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);

        assert_eq!(
            remove_by_ids(&db, &[ids[0].clone(), "absent".into()])
                .await
                .unwrap(),
            1
        );
        assert_eq!(list_by_table(&db, &mine).await.unwrap().len(), 2);
        assert_eq!(remove_by_table(&db, &mine).await.unwrap(), 2);
        assert!(list_by_table(&db, &mine).await.unwrap().is_empty());
    }
}
