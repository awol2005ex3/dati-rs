//! The `column_value` table: the per-column value dictionary the search layer reads.

use sqlx::Row;
use sqlx::any::AnyRow;

use crate::db::{Db, read_json_list};
use crate::error::Result;
use crate::model::base::{Audit, PageReq, write_json_list};
use crate::model::datasource::ColumnValue;

use super::{Filter, Statement, count, in_clause, keyword_match, optional, required};

const COLUMNS: &str = "id, created_by, created_at, updated_by, updated_at, table_id, column_name, \
     value_text, synonyms, sort_no";

const TABLE: &str = "column_value";

pub fn to_value(row: &AnyRow) -> Result<ColumnValue> {
    Ok(ColumnValue {
        id: required(row, "id")?,
        table_id: required(row, "table_id")?,
        column_name: required(row, "column_name")?,
        value: required(row, "value_text")?,
        synonyms: read_json_list(row, "synonyms"),
        sort_no: row.try_get::<i64, _>("sort_no").unwrap_or_default(),
        audit: Audit {
            created_by: optional(row, "created_by"),
            created_at: optional(row, "created_at"),
            updated_by: optional(row, "updated_by"),
            updated_at: optional(row, "updated_at"),
        },
    })
}

pub async fn insert(db: &Db, value: &ColumnValue) -> Result<()> {
    Statement::new(format!(
        "INSERT INTO {TABLE} (id, created_by, created_at, updated_by, updated_at, table_id, \
         column_name, value_text, synonyms, sort_no) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    ))
    .bind(value.id.clone())
    .bind(value.audit.created_by.clone())
    .bind(value.audit.created_at.clone())
    .bind(value.audit.updated_by.clone())
    .bind(value.audit.updated_at.clone())
    .bind(value.table_id.clone())
    .bind(value.column_name.clone())
    .bind(value.value.clone())
    .bind(write_json_list(&value.synonyms))
    .bind(value.sort_no)
    .execute(db)
    .await?;
    Ok(())
}

/// Writes a dictionary entry by id: an existing row keeps its position and creation audit, a new
/// one is inserted. This is what the Java `saveBatch` into Elasticsearch meant for value documents.
pub async fn save(db: &Db, value: &ColumnValue) -> Result<()> {
    let affected = Statement::new(format!(
        "UPDATE {TABLE} SET value_text = ?, synonyms = ?, updated_by = ?, updated_at = ? WHERE id = ?"
    ))
    .bind(value.value.clone())
    .bind(write_json_list(&value.synonyms))
    .bind(value.audit.updated_by.clone())
    .bind(value.audit.updated_at.clone())
    .bind(value.id.clone())
    .execute(db)
    .await?;
    if affected == 0 {
        insert(db, value).await?;
    }
    Ok(())
}

pub async fn find(db: &Db, id: &str) -> Result<Option<ColumnValue>> {
    let row = Statement::new(format!("SELECT {COLUMNS} FROM {TABLE} WHERE id = ?"))
        .bind(id)
        .fetch_optional(db)
        .await?;
    row.as_ref().map(to_value).transpose()
}

/// The whole dictionary of one column, ordered as it was built.
pub async fn list_by_column(
    db: &Db,
    table_id: &str,
    column_name: &str,
) -> Result<Vec<ColumnValue>> {
    let rows = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE table_id = ? AND column_name = ? ORDER BY sort_no ASC, id ASC"
    ))
    .bind(table_id)
    .bind(column_name)
    .fetch_all(db)
    .await?;
    rows.iter().map(to_value).collect()
}

/// The dictionaries of a batch of tables, capped so one search cannot pull the whole store into
/// memory. This is what the Java `findByTableIdAndType(FIELD_VALUE, 5000)` limit guarded.
pub async fn list_by_tables(db: &Db, table_ids: &[String], limit: i64) -> Result<Vec<ColumnValue>> {
    if table_ids.is_empty() {
        return Ok(Vec::new());
    }
    let (clause, binds) = in_clause("table_id", table_ids);
    let mut statement = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE {clause} \
         ORDER BY table_id ASC, column_name ASC, sort_no ASC LIMIT ?"
    ));
    for bind in binds {
        statement = statement.bind(bind);
    }
    let rows = statement.bind(limit).fetch_all(db).await?;
    rows.iter().map(to_value).collect()
}

/// The value documents the search matches against: any value text or synonym carrying a keyword.
pub async fn matching(
    db: &Db,
    table_ids: &[String],
    keywords: &[String],
) -> Result<Vec<ColumnValue>> {
    if table_ids.is_empty() || keywords.is_empty() {
        return Ok(Vec::new());
    }
    let (clause, binds) = in_clause("table_id", table_ids);
    let (pattern, pattern_binds) = keyword_match(keywords, &["value_text", "synonyms"]);
    let mut statement = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE {clause} AND {pattern} \
         ORDER BY table_id ASC, column_name ASC, sort_no ASC"
    ));
    for bind in binds.into_iter().chain(pattern_binds) {
        statement = statement.bind(bind);
    }
    let rows = statement.fetch_all(db).await?;
    rows.iter().map(to_value).collect()
}

/// `GET .../columns/{columnId}/values`: the dictionary page, optionally filtered by value text.
pub async fn page(
    db: &Db,
    table_id: &str,
    column_name: &str,
    keyword: Option<&str>,
    page: PageReq,
) -> Result<(Vec<ColumnValue>, i64)> {
    let mut filter = Filter::default()
        .add("table_id = ?", [table_id.into()])
        .add("column_name = ?", [column_name.into()]);
    if let Some(keyword) = keyword.filter(|v| !v.trim().is_empty()) {
        filter = filter.add(
            "(LOWER(value_text) LIKE ? OR LOWER(synonyms) LIKE ?)",
            [
                format!("%{}%", keyword.to_lowercase()).into(),
                format!("%{}%", keyword.to_lowercase()).into(),
            ],
        );
    }
    let total = count(&filter.count(TABLE), db).await?;
    let rows = filter
        .select(COLUMNS, TABLE)
        .push_text(" ORDER BY sort_no ASC, id ASC LIMIT ? OFFSET ?")
        .bind(page.size)
        .bind(page.offset())
        .fetch_all(db)
        .await?;
    Ok((
        rows.iter().map(to_value).collect::<Result<Vec<_>>>()?,
        total,
    ))
}

/// Position for the next entry of a column, so a sampled dictionary reads top-down.
pub async fn next_sort_no(db: &Db, table_id: &str, column_name: &str) -> Result<i64> {
    let row = Statement::new(format!(
        "SELECT COALESCE(MAX(sort_no), -1) AS highest FROM {TABLE} WHERE table_id = ? AND column_name = ?"
    ))
    .bind(table_id)
    .bind(column_name)
    .fetch_one(db)
    .await?;
    Ok(row.try_get::<i64, _>("highest").unwrap_or_default() + 1)
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

pub async fn remove_by_column(db: &Db, table_id: &str, column_name: &str) -> Result<u64> {
    let affected = Statement::new(format!(
        "DELETE FROM {TABLE} WHERE table_id = ? AND column_name = ?"
    ))
    .bind(table_id)
    .bind(column_name)
    .execute(db)
    .await?;
    Ok(affected)
}

pub async fn remove_by_table(db: &Db, table_id: &str) -> Result<u64> {
    let affected = Statement::new(format!("DELETE FROM {TABLE} WHERE table_id = ?"))
        .bind(table_id)
        .execute(db)
        .await?;
    Ok(affected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::new_id;

    fn value(table_id: &str, column: &str, text: &str, synonyms: &[&str]) -> ColumnValue {
        ColumnValue {
            id: new_id(),
            table_id: table_id.into(),
            column_name: column.into(),
            value: text.into(),
            synonyms: synonyms.iter().map(|v| v.to_string()).collect(),
            sort_no: 0,
            audit: Audit::new("u-1"),
        }
    }

    #[tokio::test]
    async fn save_inserts_then_updates_in_place() {
        let db = Db::sqlite_temp().await;
        let mut entry = value("t-1", "status", "paid", &["已支付"]);
        entry.sort_no = super::next_sort_no(&db, "t-1", "status").await.unwrap();
        save(&db, &entry).await.unwrap();
        assert_eq!(entry.sort_no, 0, "an empty dictionary starts at zero");

        let found = find(&db, &entry.id).await.unwrap().unwrap();
        assert_eq!(found.value, "paid");
        assert_eq!(found.synonyms, vec!["已支付".to_string()]);

        let mut edited = found.clone();
        edited.value = "PAID".into();
        edited.synonyms = vec!["已支付".into(), "已付款".into()];
        save(&db, &edited).await.unwrap();
        let after = find(&db, &edited.id).await.unwrap().unwrap();
        assert_eq!(after.value, "PAID");
        assert_eq!(after.synonyms.len(), 2);
        assert_eq!(after.sort_no, found.sort_no, "position survives an edit");
        assert_eq!(after.audit.created_at, found.audit.created_at);

        // A second column of the same table counts from its own zero.
        assert_eq!(super::next_sort_no(&db, "t-1", "channel").await.unwrap(), 0);
        let mut second = value("t-1", "status", "refunded", &[]);
        second.sort_no = super::next_sort_no(&db, "t-1", "status").await.unwrap();
        save(&db, &second).await.unwrap();
        assert_eq!(second.sort_no, 1);
        let list = list_by_column(&db, "t-1", "status").await.unwrap();
        assert_eq!(
            list.iter().map(|v| v.value.as_str()).collect::<Vec<_>>(),
            vec!["PAID", "refunded"]
        );
    }

    #[tokio::test]
    async fn paging_matches_values_and_synonyms() {
        let db = Db::sqlite_temp().await;
        for (index, text) in ["paid", "refunded", "shipped"].iter().enumerate() {
            let mut entry = value(
                "t-1",
                "status",
                text,
                if index == 0 { &["已支付"] } else { &[] },
            );
            entry.sort_no = index as i64;
            insert(&db, &entry).await.unwrap();
        }
        let (rows, total) = page(&db, "t-1", "status", None, PageReq { page: 1, size: 2 })
            .await
            .unwrap();
        assert_eq!(total, 3);
        assert_eq!(rows[1].value, "refunded", "sort_no decides the order");

        let (hits, total) = page(
            &db,
            "t-1",
            "status",
            Some("已支"),
            PageReq { page: 1, size: 10 },
        )
        .await
        .unwrap();
        assert_eq!((hits.len(), total as usize), (1, 1));
        assert_eq!(hits[0].value, "paid", "synonyms are searchable");

        assert_eq!(remove_by_column(&db, "t-1", "status").await.unwrap(), 3);
        assert!(
            list_by_column(&db, "t-1", "status")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn removing_a_table_clears_its_dictionary() {
        let db = Db::sqlite_temp().await;
        insert(&db, &value("t-1", "a", "x", &[])).await.unwrap();
        let keep = value("t-2", "a", "y", &[]);
        insert(&db, &keep).await.unwrap();
        assert_eq!(remove_by_table(&db, "t-1").await.unwrap(), 1);
        assert_eq!(remove_by_table(&db, "t-1").await.unwrap(), 0);
        assert_eq!(list_by_column(&db, "t-2", "a").await.unwrap().len(), 1);
        assert_eq!(
            remove_by_ids(&db, std::slice::from_ref(&keep.id))
                .await
                .unwrap(),
            1
        );
    }
}
