//! The `data_source` table: connection details of a database DatI catalogs.

use sqlx::any::AnyRow;

use crate::db::Db;
use crate::error::{DatiError, ErrorCode, Result};
use crate::model::base::{Audit, PageReq};
use crate::model::datasource::{DataSource, DbType};
use crate::model::permission::ResourceType;

use super::resource::Visibility;
use super::{Filter, count, flag, optional, required};

const COLUMNS: &str = "id, created_by, created_at, updated_by, updated_at, name, description, \
     deleted, connection_url, type, user_name, encrypted_password, default_schema";

const TABLE: &str = "data_source";

pub fn to_source(row: &AnyRow) -> Result<DataSource> {
    let stored = optional(row, "type").ok_or_else(|| {
        tracing::error!("data source row has no type");
        DatiError::with_message(ErrorCode::InternalError, "data source row has no type")
    })?;
    let db_type = DbType::from_stored(&stored)
        .ok_or_else(|| DatiError::new(ErrorCode::DsUnsupportedType, [stored.as_str()]))?;
    Ok(DataSource {
        id: required(row, "id")?,
        name: optional(row, "name"),
        description: optional(row, "description"),
        deleted: flag(row, "deleted"),
        connection_url: optional(row, "connection_url"),
        db_type,
        user_name: optional(row, "user_name"),
        password: optional(row, "encrypted_password"),
        default_schema: optional(row, "default_schema"),
        audit: Audit {
            created_by: optional(row, "created_by"),
            created_at: optional(row, "created_at"),
            updated_by: optional(row, "updated_by"),
            updated_at: optional(row, "updated_at"),
        },
    })
}

pub async fn insert(db: &Db, source: &DataSource) -> Result<()> {
    super::Statement::new(format!(
        "INSERT INTO {TABLE} (id, created_by, created_at, updated_by, updated_at, name, description, \
         deleted, connection_url, type, user_name, encrypted_password, default_schema) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    ))
    .bind(source.id.clone())
    .bind(source.audit.created_by.clone())
    .bind(source.audit.created_at.clone())
    .bind(source.audit.updated_by.clone())
    .bind(source.audit.updated_at.clone())
    .bind(source.name.clone())
    .bind(source.description.clone())
    .bind(source.deleted)
    .bind(source.connection_url.clone())
    .bind(source.db_type.as_str())
    .bind(source.user_name.clone())
    .bind(source.password.clone())
    .bind(source.default_schema.clone())
    .execute(db)
    .await?;
    Ok(())
}

/// Writes back every mutable column; `id` and `created_*` stay as they were.
pub async fn update(db: &Db, source: &DataSource) -> Result<()> {
    super::Statement::new(format!(
        "UPDATE {TABLE} SET name = ?, description = ?, deleted = ?, connection_url = ?, type = ?, \
         user_name = ?, encrypted_password = ?, default_schema = ?, updated_by = ?, updated_at = ? \
         WHERE id = ?"
    ))
    .bind(source.name.clone())
    .bind(source.description.clone())
    .bind(source.deleted)
    .bind(source.connection_url.clone())
    .bind(source.db_type.as_str())
    .bind(source.user_name.clone())
    .bind(source.password.clone())
    .bind(source.default_schema.clone())
    .bind(source.audit.updated_by.clone())
    .bind(source.audit.updated_at.clone())
    .bind(source.id.clone())
    .execute(db)
    .await?;
    Ok(())
}

pub async fn find(db: &Db, id: &str) -> Result<Option<DataSource>> {
    let row = super::Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE id = ? AND deleted = ?"
    ))
    .bind(id)
    .bind(false)
    .fetch_optional(db)
    .await?;
    row.as_ref().map(to_source).transpose()
}

pub async fn remove(db: &Db, id: &str) -> Result<bool> {
    let affected = super::Statement::new(format!("DELETE FROM {TABLE} WHERE id = ?"))
        .bind(id)
        .execute(db)
        .await?;
    Ok(affected > 0)
}

/// `GET /v1/data-sources`: newest first, restricted to what the caller may see unless they are an
/// admin. Keyword matches the name loosely or the id exactly, as the Java query did.
pub async fn page(
    db: &Db,
    keyword: Option<&str>,
    visibility: Visibility<'_>,
    page: PageReq,
) -> Result<(Vec<DataSource>, i64)> {
    let mut filter = Filter::default().add("deleted = ?", [false.into()]);
    if let Some(keyword) = keyword.filter(|v| !v.trim().is_empty()) {
        filter = filter.add(
            "(LOWER(name) LIKE ? OR id = ?)",
            [
                format!("%{}%", keyword.to_lowercase()).into(),
                keyword.into(),
            ],
        );
    }
    filter = visibility.apply(filter, ResourceType::DataSource);
    let total = count(&filter.count(TABLE), db).await?;
    let rows = filter
        .select(COLUMNS, TABLE)
        .push_text(" ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?")
        .bind(page.size)
        .bind(page.offset())
        .fetch_all(db)
        .await?;
    Ok((
        rows.iter().map(to_source).collect::<Result<Vec<_>>>()?,
        total,
    ))
}

/// The sources behind a batch of search hits, so the response can name and describe them. No
/// permission check happens here: the caller resolved the scope it searched.
pub async fn find_by_ids(db: &Db, ids: &[String]) -> Result<Vec<DataSource>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let (clause, binds) = super::in_clause("id", ids);
    let mut statement = super::Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE deleted = ? AND {clause}"
    ));
    statement = statement.bind(false);
    for bind in binds {
        statement = statement.bind(bind);
    }
    let rows = statement.fetch_all(db).await?;
    rows.iter().map(to_source).collect()
}

/// Ids to names, for the subject and MCP screens that label their rows.
pub async fn name_map(
    db: &Db,
    ids: &[String],
) -> Result<std::collections::HashMap<String, String>> {
    if ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let placeholders = vec!["?"; ids.len()].join(", ");
    let mut statement = super::Statement::new(format!(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::permission::{ALL_USERS, AclEntry, Permission, PrincipalType};
    use crate::repo::{acl, user};
    use crate::time::new_id;

    fn source(owner: Option<&str>, name: &str) -> DataSource {
        DataSource {
            id: new_id(),
            name: Some(name.into()),
            description: Some("demo".into()),
            deleted: false,
            connection_url: Some("sqlite:demo.db".into()),
            db_type: DbType::Sqlite,
            user_name: Some("nobody".into()),
            password: Some("secret".into()),
            default_schema: Some("main".into()),
            audit: match owner {
                Some(owner) => Audit::new(owner),
                None => Audit::timestamped(),
            },
        }
    }

    /// `created_at` only has second precision, so paging tests stamp an explicit chronology.
    fn aged(mut row: DataSource, ordinal: i64) -> DataSource {
        let stamp = format!("2026-01-01T00:00:0{ordinal}Z");
        row.audit.created_at = Some(stamp.clone());
        row.audit.updated_at = Some(stamp);
        row
    }

    async fn person(db: &Db, name: &str) -> String {
        let id = new_id();
        user::insert(
            db,
            &crate::model::user::User {
                id: id.clone(),
                name: name.into(),
                password: "hash".into(),
                display_name: Some(format!("{name} Display")),
                audit: Audit::new(name),
            },
        )
        .await
        .unwrap();
        id
    }

    #[tokio::test]
    async fn round_trips_every_column() {
        let db = Db::sqlite_temp().await;
        let owner = person(&db, "alice").await;
        let stored = source(Some(&owner), "warehouse");
        insert(&db, &stored).await.unwrap();
        let found = find(&db, &stored.id).await.unwrap().unwrap();
        assert_eq!(found, stored);
        assert_eq!(found.db_type, DbType::Sqlite);
        assert_eq!(found.password.as_deref(), Some("secret"));

        let changed = DataSource {
            name: Some("renamed".into()),
            db_type: DbType::MySql,
            connection_url: Some("mysql://h:3306/dw".into()),
            default_schema: Some("dw".into()),
            ..found.clone()
        };
        update(&db, &changed).await.unwrap();
        let after = find(&db, &stored.id).await.unwrap().unwrap();
        assert_eq!(after.name.as_deref(), Some("renamed"));
        assert_eq!(after.db_type, DbType::MySql);
        assert_eq!(
            after.audit.created_by, found.audit.created_by,
            "author is stable"
        );

        assert!(remove(&db, &stored.id).await.unwrap());
        assert!(find(&db, &stored.id).await.unwrap().is_none());
        assert!(!remove(&db, &stored.id).await.unwrap());
    }

    #[tokio::test]
    async fn a_row_without_a_type_is_a_server_error_not_a_panic() {
        let db = Db::sqlite_temp().await;
        let id = new_id();
        sqlx::query(
            &db.sql("INSERT INTO data_source (id, name, deleted, type) VALUES (?, ?, ?, ?)"),
        )
        .bind(&id)
        .bind("legacy")
        .bind(false)
        .bind("CLICKHOUSE")
        .execute(db.pool())
        .await
        .unwrap();
        let err = find(&db, &id).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::DsUnsupportedType);
    }

    #[tokio::test]
    async fn paging_honours_visibility_and_keyword() {
        let db = Db::sqlite_temp().await;
        let alice = person(&db, "alice").await;
        let bob = person(&db, "bob").await;

        let shared = aged(source(Some(&alice), "Shared Source"), 1);
        let private = aged(source(Some(&alice), "Private Source"), 2);
        let bobs = aged(source(Some(&bob), "Bob Source"), 3);
        for row in [&shared, &private, &bobs] {
            insert(&db, row).await.unwrap();
        }
        sqlx::query(&db.sql("INSERT INTO data_source (id, name, deleted) VALUES (?, ?, ?)"))
            .bind(new_id())
            .bind("Deleted Source")
            .bind(true)
            .execute(db.pool())
            .await
            .unwrap();
        acl::insert(
            &db,
            &AclEntry {
                id: new_id(),
                resource_type: ResourceType::DataSource,
                resource_id: shared.id.clone(),
                principal_type: PrincipalType::Group,
                principal_id: ALL_USERS.to_string(),
                permission: Permission::View,
                audit: Audit::new(&alice),
            },
        )
        .await
        .unwrap();

        let page = PageReq { page: 1, size: 10 };
        let (all, total) = page_of(&db, None, Visibility::All, page).await;
        assert_eq!(total, 3, "soft-deleted rows are gone for admins too");
        assert_eq!(all.len(), 3);

        let (visible, _) = page_of(&db, None, Visibility::User(&bob), page).await;
        let names: Vec<&str> = visible.iter().filter_map(|s| s.name.as_deref()).collect();
        assert_eq!(names, vec!["Bob Source", "Shared Source"]);

        let (hits, total) = page_of(&db, Some("private"), Visibility::All, page).await;
        assert_eq!(hits.len(), 1);
        assert_eq!(total, 1);

        let (by_id, _) = page_of(&db, Some(&private.id), Visibility::All, page).await;
        assert_eq!(by_id.len(), 1, "a keyword may be the id itself");

        let (first_page, total) =
            page_of(&db, None, Visibility::All, PageReq { page: 2, size: 2 }).await;
        assert_eq!(total, 3);
        assert_eq!(
            first_page.len(),
            1,
            "the remainder of a 3-row, 2-per-page list"
        );
    }

    async fn page_of(
        db: &Db,
        keyword: Option<&str>,
        visibility: Visibility<'_>,
        page: PageReq,
    ) -> (Vec<DataSource>, i64) {
        super::page(db, keyword, visibility, page).await.unwrap()
    }

    #[tokio::test]
    async fn name_map_skips_missing_and_unnamed_rows() {
        let db = Db::sqlite_temp().await;
        let named = source(Some("u-1"), "Warehouse");
        let blank = DataSource {
            name: None,
            ..source(Some("u-1"), "blank")
        };
        insert(&db, &named).await.unwrap();
        insert(&db, &blank).await.unwrap();
        let map = name_map(&db, &[named.id.clone(), blank.id.clone(), "absent".into()])
            .await
            .unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map[&named.id], "Warehouse");
        assert!(name_map(&db, &[]).await.unwrap().is_empty());
    }
}
