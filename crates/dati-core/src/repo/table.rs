//! The `table_info` table: tables registered from a data source.

use sqlx::any::AnyRow;

use crate::db::{Db, read_json_list};
use crate::error::Result;
use crate::model::base::{Audit, PageReq, write_json_list};
use crate::model::datasource::TableInfo;

use super::{Bind, Filter, Statement, count, flag, optional, required};

pub(crate) const COLUMNS: &str = "id, created_by, created_at, updated_by, updated_at, name, description, \
     deleted, data_source_id, table_schema, aliases";

pub(crate) const TABLE: &str = "table_info";

pub fn to_table(row: &AnyRow) -> Result<TableInfo> {
    Ok(TableInfo {
        id: required(row, "id")?,
        name: optional(row, "name"),
        description: optional(row, "description"),
        deleted: flag(row, "deleted"),
        data_source_id: required(row, "data_source_id")?,
        schema: optional(row, "table_schema"),
        aliases: read_json_list(row, "aliases"),
        audit: Audit {
            created_by: optional(row, "created_by"),
            created_at: optional(row, "created_at"),
            updated_by: optional(row, "updated_by"),
            updated_at: optional(row, "updated_at"),
        },
    })
}

pub async fn insert(db: &Db, table: &TableInfo) -> Result<String> {
    Statement::new(format!(
        "INSERT INTO {TABLE} (id, created_by, created_at, updated_by, updated_at, name, description, \
         deleted, data_source_id, table_schema, aliases) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    ))
    .bind(table.id.clone())
    .bind(table.audit.created_by.clone())
    .bind(table.audit.created_at.clone())
    .bind(table.audit.updated_by.clone())
    .bind(table.audit.updated_at.clone())
    .bind(table.name.clone())
    .bind(table.description.clone())
    .bind(table.deleted)
    .bind(table.data_source_id.clone())
    .bind(table.schema.clone())
    .bind(write_json_list(&table.aliases))
    .execute(db)
    .await?;
    Ok(table.id.clone())
}

/// Writes the editable columns back; the owning data source and creation audit never move.
pub async fn update(db: &Db, table: &TableInfo) -> Result<()> {
    Statement::new(format!(
        "UPDATE {TABLE} SET name = ?, description = ?, deleted = ?, table_schema = ?, aliases = ?, \
         updated_by = ?, updated_at = ? WHERE id = ?"
    ))
    .bind(table.name.clone())
    .bind(table.description.clone())
    .bind(table.deleted)
    .bind(table.schema.clone())
    .bind(write_json_list(&table.aliases))
    .bind(table.audit.updated_by.clone())
    .bind(table.audit.updated_at.clone())
    .bind(table.id.clone())
    .execute(db)
    .await?;
    Ok(())
}

pub async fn find(db: &Db, id: &str) -> Result<Option<TableInfo>> {
    let row = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE id = ? AND deleted = ?"
    ))
    .bind(id)
    .bind(false)
    .fetch_optional(db)
    .await?;
    row.as_ref().map(to_table).transpose()
}

/// Every table of a data source, in registration order.
pub async fn list_by_datasource(db: &Db, data_source_id: &str) -> Result<Vec<TableInfo>> {
    let rows = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE data_source_id = ? AND deleted = ? ORDER BY created_at ASC, id ASC"
    ))
    .bind(data_source_id)
    .bind(false)
    .fetch_all(db)
    .await?;
    rows.iter().map(to_table).collect()
}

/// `GET /v1/data-sources/{id}/tables`: newest first, optionally filtered by name.
pub async fn page(
    db: &Db,
    data_source_id: &str,
    keyword: Option<&str>,
    page: PageReq,
) -> Result<(Vec<TableInfo>, i64)> {
    let mut filter = Filter::default()
        .add("deleted = ?", [false.into()])
        .add("data_source_id = ?", [data_source_id.into()]);
    if let Some(keyword) = keyword.filter(|v| !v.trim().is_empty()) {
        filter = filter.add(
            "LOWER(name) LIKE ?",
            [format!("%{}%", keyword.to_lowercase()).into()],
        );
    }
    let total = count(&filter.count(TABLE), db).await?;
    let rows = filter
        .select(COLUMNS, TABLE)
        .push_text(" ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?")
        .bind(page.size)
        .bind(page.offset())
        .fetch_all(db)
        .await?;
    Ok((
        rows.iter().map(to_table).collect::<Result<Vec<_>>>()?,
        total,
    ))
}

/// The tables of one data source in one schema: what `GET /v1/subjects/{id}/available-tables`
/// starts from before it removes the tables the subject already holds.
pub async fn list_by_datasource_and_schema(
    db: &Db,
    data_source_id: &str,
    schema: &str,
) -> Result<Vec<TableInfo>> {
    let rows = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE data_source_id = ? AND table_schema = ? AND deleted = ? \
         ORDER BY created_at ASC, id ASC"
    ))
    .bind(data_source_id)
    .bind(schema)
    .bind(false)
    .fetch_all(db)
    .await?;
    rows.iter().map(to_table).collect()
}

/// The tables a metadata search may look at: everything registered under one of the given data
/// sources, plus everything bound to one of the given subjects. This replaces the document filter
/// the Java Elasticsearch query ran on `entity.datasourceId` / `entity.subjectId`.
pub async fn list_in_scope(
    db: &Db,
    datasource_ids: &[String],
    subject_ids: &[String],
) -> Result<Vec<TableInfo>> {
    if datasource_ids.is_empty() && subject_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut branches = Vec::new();
    let mut binds = Vec::new();
    if !datasource_ids.is_empty() {
        branches.push(format!(
            "data_source_id IN ({})",
            vec!["?"; datasource_ids.len()].join(", ")
        ));
        binds.extend(datasource_ids.iter().cloned().map(Bind::from));
    }
    if !subject_ids.is_empty() {
        branches.push(format!(
            "id IN (SELECT table_id FROM subject_table WHERE subject_id IN ({}))",
            vec!["?"; subject_ids.len()].join(", ")
        ));
        binds.extend(subject_ids.iter().cloned().map(Bind::from));
    }
    let mut statement = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE deleted = ? AND ({}) ORDER BY created_at ASC, id ASC",
        branches.join(" OR ")
    ));
    statement = statement.bind(false);
    for bind in binds {
        statement = statement.bind(bind);
    }
    let rows = statement.fetch_all(db).await?;
    rows.iter().map(to_table).collect()
}

/// A batch read used by the search layer and the term bindings, which resolve table ids rather
/// than names. Deleted rows are skipped: a dropped table must not surface in a search hit.
pub async fn find_by_ids(db: &Db, ids: &[String]) -> Result<Vec<TableInfo>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; ids.len()].join(", ");
    let mut statement = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE id IN ({placeholders}) AND deleted = ?"
    ));
    for id in ids {
        statement = statement.bind(id.clone());
    }
    let rows = statement.bind(false).fetch_all(db).await?;
    rows.iter().map(to_table).collect()
}

pub async fn remove(db: &Db, id: &str) -> Result<bool> {
    let affected = Statement::new(format!("DELETE FROM {TABLE} WHERE id = ?"))
        .bind(id)
        .execute(db)
        .await?;
    Ok(affected > 0)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::new_id;

    fn table(ds: &str, name: &str, aliases: &[&str]) -> TableInfo {
        TableInfo {
            id: new_id(),
            name: Some(name.into()),
            description: Some("订单表".into()),
            deleted: false,
            data_source_id: ds.into(),
            schema: Some("main".into()),
            aliases: aliases.iter().map(|v| v.to_string()).collect(),
            audit: Audit::new("u-1"),
        }
    }

    #[tokio::test]
    async fn aliases_survive_the_json_column() {
        let db = Db::sqlite_temp().await;
        let row = table("ds-1", "orders", &["销售单", "order"]);
        insert(&db, &row).await.unwrap();
        let found = find(&db, &row.id).await.unwrap().unwrap();
        assert_eq!(found, row);
        assert_eq!(found.aliases.len(), 2);

        let empty = TableInfo {
            id: new_id(),
            aliases: Vec::new(),
            description: None,
            ..row.clone()
        };
        insert(&db, &empty).await.unwrap();
        let found = find(&db, &empty.id).await.unwrap().unwrap();
        assert!(found.aliases.is_empty());
        assert_eq!(found.description, None);
    }

    #[tokio::test]
    async fn update_touches_only_its_own_columns() {
        let db = Db::sqlite_temp().await;
        let row = table("ds-1", "orders", &[]);
        insert(&db, &row).await.unwrap();
        let changed = TableInfo {
            name: Some("orders_v2".into()),
            aliases: vec!["orders".into()],
            ..row.clone()
        };
        update(&db, &changed).await.unwrap();
        let found = find(&db, &row.id).await.unwrap().unwrap();
        assert_eq!(found.name.as_deref(), Some("orders_v2"));
        assert_eq!(found.aliases, vec!["orders".to_string()]);
        assert_eq!(found.data_source_id, "ds-1");
        assert_eq!(found.audit.created_by.as_deref(), Some("u-1"));
    }

    #[tokio::test]
    async fn listing_and_paging_stay_inside_one_data_source() {
        let db = Db::sqlite_temp().await;
        for name in ["orders", "order_items", "users"] {
            insert(&db, &table("ds-1", name, &[])).await.unwrap();
        }
        insert(&db, &table("ds-2", "elsewhere", &[])).await.unwrap();
        sqlx::query(
            &db.sql(
                "INSERT INTO table_info (id, name, deleted, data_source_id) VALUES (?, ?, ?, ?)",
            ),
        )
        .bind(new_id())
        .bind("gone")
        .bind(true)
        .bind("ds-1")
        .execute(db.pool())
        .await
        .unwrap();

        assert_eq!(list_by_datasource(&db, "ds-1").await.unwrap().len(), 3);
        let (rows, total) = page(&db, "ds-1", None, PageReq { page: 1, size: 2 })
            .await
            .unwrap();
        assert_eq!(total, 3);
        assert_eq!(rows.len(), 2);
        let (hits, total) = page(&db, "ds-1", Some("ORDER"), PageReq { page: 1, size: 10 })
            .await
            .unwrap();
        assert_eq!(total, 2, "keyword matching ignores case");
        assert_eq!(hits.len(), 2);

        let victim = hits[0].id.clone();
        assert!(remove(&db, &victim).await.unwrap());
        assert!(!remove(&db, &victim).await.unwrap());
        assert_eq!(list_by_datasource(&db, "ds-1").await.unwrap().len(), 2);
    }
}
