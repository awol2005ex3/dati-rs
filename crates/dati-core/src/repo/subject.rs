//! The `subject` table plus its `subject_table` bindings: a business domain and the tables it owns.

use std::collections::HashMap;

use sqlx::any::AnyRow;

use crate::db::{Db, read_json_list};
use crate::error::Result;
use crate::model::base::{Audit, PageReq, write_json_list};
use crate::model::datasource::TableInfo;
use crate::model::permission::ResourceType;
use crate::model::semantic::Subject;

use super::resource::Visibility;
use super::table::{self, TABLE as TABLE_INFO};
use super::{Filter, Statement, count, flag, optional, required};

const COLUMNS: &str = "id, created_by, created_at, updated_by, updated_at, name, description, \
     deleted, datasource_id, aliases";

const TABLE: &str = "subject";

const BINDINGS: &str = "subject_table";

pub fn to_subject(row: &AnyRow) -> Result<Subject> {
    Ok(Subject {
        id: required(row, "id")?,
        name: optional(row, "name"),
        description: optional(row, "description"),
        deleted: flag(row, "deleted"),
        datasource_id: required(row, "datasource_id")?,
        aliases: read_json_list(row, "aliases"),
        audit: Audit {
            created_by: optional(row, "created_by"),
            created_at: optional(row, "created_at"),
            updated_by: optional(row, "updated_by"),
            updated_at: optional(row, "updated_at"),
        },
    })
}

pub async fn insert(db: &Db, subject: &Subject) -> Result<String> {
    Statement::new(format!(
        "INSERT INTO {TABLE} (id, created_by, created_at, updated_by, updated_at, name, \
         description, deleted, datasource_id, aliases) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    ))
    .bind(subject.id.clone())
    .bind(subject.audit.created_by.clone())
    .bind(subject.audit.created_at.clone())
    .bind(subject.audit.updated_by.clone())
    .bind(subject.audit.updated_at.clone())
    .bind(subject.name.clone())
    .bind(subject.description.clone())
    .bind(subject.deleted)
    .bind(subject.datasource_id.clone())
    .bind(write_json_list(&subject.aliases))
    .execute(db)
    .await?;
    Ok(subject.id.clone())
}

/// Writes back the editable columns. The owning data source never moves, matching the Java update
/// body, which had no `datasourceId` field at all.
pub async fn update(db: &Db, subject: &Subject) -> Result<()> {
    Statement::new(format!(
        "UPDATE {TABLE} SET name = ?, description = ?, aliases = ?, updated_by = ?, updated_at = ? \
         WHERE id = ?"
    ))
    .bind(subject.name.clone())
    .bind(subject.description.clone())
    .bind(write_json_list(&subject.aliases))
    .bind(subject.audit.updated_by.clone())
    .bind(subject.audit.updated_at.clone())
    .bind(subject.id.clone())
    .execute(db)
    .await?;
    Ok(())
}

pub async fn find(db: &Db, id: &str) -> Result<Option<Subject>> {
    let row = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE id = ? AND deleted = ?"
    ))
    .bind(id)
    .bind(false)
    .fetch_optional(db)
    .await?;
    row.as_ref().map(to_subject).transpose()
}

/// `GET /v1/subjects`: newest first. A keyword matches the id as a prefix or the name loosely,
/// and a non-admin only sees the subjects it owns or was granted.
pub async fn page(
    db: &Db,
    keyword: Option<&str>,
    visibility: Visibility<'_>,
    page_req: PageReq,
) -> Result<(Vec<Subject>, i64)> {
    let mut filter = Filter::default().add("deleted = ?", [false.into()]);
    if let Some(keyword) = keyword.filter(|v| !v.trim().is_empty()) {
        filter = filter.add(
            "(id LIKE ? OR LOWER(name) LIKE ?)",
            [
                format!("{keyword}%").into(),
                format!("%{}%", keyword.to_lowercase()).into(),
            ],
        );
    }
    filter = visibility.apply(filter, ResourceType::Subject);
    let total = count(&filter.count(TABLE), db).await?;
    let rows = filter
        .select(COLUMNS, TABLE)
        .push_text(" ORDER BY updated_at DESC, id DESC LIMIT ? OFFSET ?")
        .bind(page_req.size)
        .bind(page_req.offset())
        .fetch_all(db)
        .await?;
    Ok((
        rows.iter().map(to_subject).collect::<Result<Vec<_>>>()?,
        total,
    ))
}

/// Ids to names, for the term responses that label their subject.
pub async fn name_map(db: &Db, ids: &[String]) -> Result<HashMap<String, String>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = vec!["?"; ids.len()].join(", ");
    let mut statement = Statement::new(format!(
        "SELECT id, name FROM {TABLE} WHERE id IN ({placeholders})"
    ));
    for id in ids {
        statement = statement.bind(id.clone());
    }
    let rows = statement.fetch_all(db).await?;
    Ok(rows
        .iter()
        .filter_map(|row| Some((required(row, "id").ok()?, optional(row, "name")?)))
        .collect())
}

/// Subject ids to the data source they belong to. A subject that no longer exists is simply absent
/// from the map, which is how the Java batch lookup behaved.
pub async fn source_map(db: &Db, ids: &[String]) -> Result<HashMap<String, String>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let (clause, binds) = super::in_clause("id", ids);
    let mut statement = Statement::new(format!(
        "SELECT id, datasource_id FROM {TABLE} WHERE deleted = ? AND {clause}"
    ));
    statement = statement.bind(false);
    for bind in binds {
        statement = statement.bind(bind);
    }
    let rows = statement.fetch_all(db).await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            Some((
                required(row, "id").ok()?,
                required(row, "datasource_id").ok()?,
            ))
        })
        .collect())
}

pub async fn remove(db: &Db, id: &str) -> Result<bool> {
    let affected = Statement::new(format!("DELETE FROM {TABLE} WHERE id = ?"))
        .bind(id)
        .execute(db)
        .await?;
    Ok(affected > 0)
}

// ---- bindings -------------------------------------------------------------------------------

pub async fn link(db: &Db, subject_id: &str, table_id: &str, audit: &Audit) -> Result<String> {
    let id = crate::time::new_id();
    Statement::new(format!(
        "INSERT INTO {BINDINGS} (id, created_by, created_at, updated_by, updated_at, subject_id, \
         table_id) VALUES (?, ?, ?, ?, ?, ?, ?)"
    ))
    .bind(id.clone())
    .bind(audit.created_by.clone())
    .bind(audit.created_at.clone())
    .bind(audit.updated_by.clone())
    .bind(audit.updated_at.clone())
    .bind(subject_id.to_string())
    .bind(table_id.to_string())
    .execute(db)
    .await?;
    Ok(id)
}

pub async fn contains(db: &Db, subject_id: &str, table_id: &str) -> Result<bool> {
    let statement = Statement::new(format!(
        "SELECT COUNT(*) AS total FROM {BINDINGS} WHERE subject_id = ? AND table_id = ?"
    ))
    .bind(subject_id)
    .bind(table_id);
    Ok(count(&statement, db).await? > 0)
}

/// Every table bound to the subject, in the order they were bound.
pub async fn table_ids(db: &Db, subject_id: &str) -> Result<Vec<String>> {
    let rows = Statement::new(format!(
        "SELECT table_id FROM {BINDINGS} WHERE subject_id = ? ORDER BY created_at ASC, id ASC"
    ))
    .bind(subject_id)
    .fetch_all(db)
    .await?;
    rows.iter().map(|row| required(row, "table_id")).collect()
}

pub async fn unlink(db: &Db, subject_id: &str, table_id: &str) -> Result<u64> {
    let affected = Statement::new(format!(
        "DELETE FROM {BINDINGS} WHERE subject_id = ? AND table_id = ?"
    ))
    .bind(subject_id)
    .bind(table_id)
    .execute(db)
    .await?;
    Ok(affected)
}

/// `GET /v1/subjects/{id}/tables`: the bound tables, newest first, optionally filtered by name.
pub async fn page_tables(
    db: &Db,
    subject_id: &str,
    keyword: Option<&str>,
    page_req: PageReq,
) -> Result<(Vec<TableInfo>, i64)> {
    let mut filter = Filter::default().add("deleted = ?", [false.into()]).add(
        &format!("id IN (SELECT table_id FROM {BINDINGS} WHERE subject_id = ?)"),
        [subject_id.into()],
    );
    if let Some(keyword) = keyword.filter(|v| !v.trim().is_empty()) {
        filter = filter.add(
            "LOWER(name) LIKE ?",
            [format!("%{}%", keyword.to_lowercase()).into()],
        );
    }
    let total = count(&filter.count(TABLE_INFO), db).await?;
    let rows = filter
        .select(table::COLUMNS, TABLE_INFO)
        .push_text(" ORDER BY updated_at DESC, id DESC LIMIT ? OFFSET ?")
        .bind(page_req.size)
        .bind(page_req.offset())
        .fetch_all(db)
        .await?;
    Ok((
        rows.iter()
            .map(table::to_table)
            .collect::<Result<Vec<_>>>()?,
        total,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::permission::{ALL_USERS, AclEntry, Permission, PrincipalType};
    use crate::repo::acl;
    use crate::time::new_id;

    fn subject(ds: &str, name: &str, aliases: &[&str]) -> Subject {
        Subject {
            id: new_id(),
            name: Some(name.into()),
            description: Some("订单业务域".into()),
            deleted: false,
            datasource_id: ds.into(),
            aliases: aliases.iter().map(|v| v.to_string()).collect(),
            audit: Audit::new("u-1"),
        }
    }

    fn table_info(ds: &str, name: &str) -> TableInfo {
        TableInfo {
            id: new_id(),
            name: Some(name.into()),
            description: None,
            deleted: false,
            data_source_id: ds.into(),
            schema: Some("main".into()),
            aliases: Vec::new(),
            audit: Audit::new("u-1"),
        }
    }

    #[tokio::test]
    async fn a_subject_round_trips_its_aliases_and_keeps_its_source() {
        let db = Db::sqlite_temp().await;
        let row = subject("ds-1", "orders", &["交易", "trade"]);
        insert(&db, &row).await.unwrap();
        assert_eq!(find(&db, &row.id).await.unwrap().unwrap(), row);

        let mut edited = row.clone();
        edited.name = Some("orders_v2".into());
        edited.aliases = Vec::new();
        edited.audit.touch("u-2");
        update(&db, &edited).await.unwrap();

        let found = find(&db, &row.id).await.unwrap().unwrap();
        assert_eq!(found.name.as_deref(), Some("orders_v2"));
        assert!(found.aliases.is_empty());
        assert_eq!(found.datasource_id, "ds-1");
        assert_eq!(
            found.audit.created_by.as_deref(),
            Some("u-1"),
            "creator stays"
        );
        assert_eq!(found.audit.updated_by.as_deref(), Some("u-2"));
        assert!(find(&db, "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn keyword_matches_id_prefix_and_loose_name() {
        let db = Db::sqlite_temp().await;
        let named = subject("ds-1", "Orders", &[]);
        insert(&db, &named).await.unwrap();
        insert(&db, &subject("ds-1", "物流", &[])).await.unwrap();

        let (hits, total) = page(
            &db,
            Some("ORDER"),
            Visibility::All,
            PageReq { page: 1, size: 10 },
        )
        .await
        .unwrap();
        assert_eq!((total, hits.len()), (1, 1));
        assert_eq!(hits[0].name.as_deref(), Some("Orders"));

        let by_id = named.id[..8].to_string();
        let (hits, _) = page(
            &db,
            Some(&by_id),
            Visibility::All,
            PageReq { page: 1, size: 10 },
        )
        .await
        .unwrap();
        assert_eq!(
            hits.len(),
            1,
            "the id prefix branch of the Java keyword query"
        );
    }

    #[tokio::test]
    async fn listing_respects_visibility_and_soft_delete() {
        let db = Db::sqlite_temp().await;
        let own = subject("ds-1", "own", &[]);
        let public = subject("ds-1", "public", &[]);
        let hidden = subject("ds-1", "gone", &[]);
        insert(&db, &own).await.unwrap();
        insert(&db, &public).await.unwrap();
        insert(&db, &hidden).await.unwrap();
        remove(&db, &hidden.id).await.unwrap();
        acl::insert(
            &db,
            &AclEntry {
                id: new_id(),
                resource_type: ResourceType::Subject,
                resource_id: public.id.clone(),
                principal_type: PrincipalType::Group,
                principal_id: ALL_USERS.to_string(),
                permission: Permission::View,
                audit: Audit::new("u-1"),
            },
        )
        .await
        .unwrap();

        let (rows, total) = page(
            &db,
            None,
            Visibility::User("u-stranger"),
            PageReq { page: 1, size: 10 },
        )
        .await
        .unwrap();
        assert_eq!(total, 1, "only the shared subject is visible to a stranger");
        assert_eq!(rows[0].id, public.id);

        let names = name_map(&db, &[own.id.clone(), public.id.clone(), "missing".into()])
            .await
            .unwrap();
        assert_eq!(names.len(), 2);
        assert_eq!(names[&own.id], "own");
    }

    #[tokio::test]
    async fn bindings_page_the_subjects_tables() {
        let db = Db::sqlite_temp().await;
        let row = subject("ds-1", "orders", &[]);
        insert(&db, &row).await.unwrap();
        let orders = table_info("ds-1", "orders");
        let customers = table_info("ds-1", "customers");
        let elsewhere = table_info("ds-2", "salaries");
        for table in [&orders, &customers, &elsewhere] {
            table::insert(&db, table).await.unwrap();
        }
        link(&db, &row.id, &orders.id, &Audit::new("u-1"))
            .await
            .unwrap();
        link(&db, &row.id, &customers.id, &Audit::new("u-1"))
            .await
            .unwrap();

        assert!(contains(&db, &row.id, &orders.id).await.unwrap());
        assert!(!contains(&db, &row.id, &elsewhere.id).await.unwrap());
        let mut ids = table_ids(&db, &row.id).await.unwrap();
        ids.sort();
        let mut bound = vec![orders.id.clone(), customers.id.clone()];
        bound.sort();
        assert_eq!(ids, bound);

        let (tables, total) = page_tables(&db, &row.id, None, PageReq { page: 1, size: 10 })
            .await
            .unwrap();
        assert_eq!((total, tables.len()), (2, 2));
        let (names, total) =
            page_tables(&db, &row.id, Some("ORDER"), PageReq { page: 1, size: 10 })
                .await
                .unwrap();
        assert_eq!((total, names.len()), (1, 1));
        assert_eq!(names[0].name.as_deref(), Some("orders"));

        assert_eq!(unlink(&db, &row.id, &orders.id).await.unwrap(), 1);
        assert_eq!(unlink(&db, &row.id, &orders.id).await.unwrap(), 0);
        assert_eq!(table_ids(&db, &row.id).await.unwrap(), vec![customers.id]);
    }
}
