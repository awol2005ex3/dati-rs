//! Resource ownership and the SQL row-filter every list endpoint shares.

use crate::db::Db;
use crate::error::Result;
use crate::model::permission::ResourceType;

use super::Filter;

pub const fn table(resource_type: ResourceType) -> &'static str {
    match resource_type {
        ResourceType::DataSource => "data_source",
        ResourceType::Subject => "subject",
        ResourceType::McpService => "mcp_service",
    }
}

pub enum Owner {
    Missing,
    Existing { created_by: Option<String> },
}

impl Owner {
    pub fn created_by(&self) -> Option<&str> {
        match self {
            Self::Missing => None,
            Self::Existing { created_by } => created_by.as_deref(),
        }
    }

    pub fn exists(&self) -> bool {
        !matches!(self, Self::Missing)
    }
}

/// Soft-deleted rows are invisible for ownership purposes, exactly as in the Java services.
pub async fn owner(db: &Db, resource_type: ResourceType, id: &str) -> Result<Owner> {
    let sql = db.sql(&format!(
        "SELECT created_by FROM {} WHERE id = ? AND deleted = ?",
        table(resource_type)
    ));
    let row = sqlx::query(&sql)
        .bind(id)
        .bind(false)
        .fetch_optional(db.pool())
        .await?;
    Ok(match row {
        None => Owner::Missing,
        Some(row) => Owner::Existing {
            created_by: super::optional(&row, "created_by"),
        },
    })
}

/// Restricts a paged list to rows the caller may see: their own, or ones granted to them
/// directly or to the public group. The Java version did this in SQL, not after fetching.
///
/// Bind order: `user_id`, `resource_type`, `user_id`, `ALL_USERS`.
pub fn accessible_clause(alias: &str) -> String {
    format!(
        "({alias}.created_by = ? OR EXISTS (SELECT 1 FROM resource_acl a WHERE a.resource_type = ? \
         AND a.resource_id = {alias}.id AND ((a.principal_type = 'USER' AND a.principal_id = ?) \
         OR (a.principal_type = 'GROUP' AND a.principal_id = ?))))"
    )
}

/// Who a list endpoint may show: everybody sees their own and granted rows; admins see all.
#[derive(Debug, Clone, Copy)]
pub enum Visibility<'a> {
    All,
    User(&'a str),
}

impl<'a> Visibility<'a> {
    /// Adds the row filter to a list query. The list's own table is used as the qualifier, so the
    /// clause and the `FROM` always name the same resource type.
    pub fn apply(self, filter: Filter, resource_type: ResourceType) -> Filter {
        match self {
            Self::All => filter,
            Self::User(user_id) => filter.add(
                &accessible_clause(table(resource_type)),
                [
                    user_id.into(),
                    resource_type.as_str().into(),
                    user_id.into(),
                    crate::model::permission::ALL_USERS.into(),
                ],
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::datasource::DbType;
    use sqlx::Row;

    use crate::model::permission::{ALL_USERS, AclEntry, Permission, PrincipalType};
    use crate::repo::{acl, user};
    use crate::time::new_id;

    async fn visible_ids(db: &Db, user_id: &str) -> Vec<String> {
        let sql = db.sql(&format!(
            "SELECT s.id FROM data_source s WHERE s.deleted = ? AND {} ORDER BY s.id",
            accessible_clause("s")
        ));
        sqlx::query(&sql)
            .bind(false)
            .bind(user_id)
            .bind(ResourceType::DataSource.as_str())
            .bind(user_id)
            .bind(ALL_USERS)
            .fetch_all(db.pool())
            .await
            .unwrap()
            .iter()
            .map(|r| r.try_get::<String, _>("id").unwrap())
            .collect()
    }

    async fn insert_source(db: &Db, owner: &str, deleted: bool) -> String {
        let id = new_id();
        let sql = db.sql(
            "INSERT INTO data_source (id, created_by, created_at, updated_by, updated_at, name, \
             deleted, connection_url, type) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        );
        sqlx::query(&sql)
            .bind(&id)
            .bind(owner)
            .bind(crate::time::now_iso())
            .bind(owner)
            .bind(crate::time::now_iso())
            .bind("demo")
            .bind(deleted)
            .bind("sqlite:demo.db")
            .bind(DbType::Sqlite.as_str())
            .execute(db.pool())
            .await
            .unwrap();
        id
    }

    #[tokio::test]
    async fn owner_resolves_created_by_and_ignores_soft_deleted_rows() {
        let db = Db::sqlite_temp().await;
        let id = insert_source(&db, "u-1", false).await;
        let found = owner(&db, ResourceType::DataSource, &id).await.unwrap();
        assert_eq!(found.created_by(), Some("u-1"));
        assert!(found.exists());

        let gone = insert_source(&db, "u-2", true).await;
        assert!(
            !owner(&db, ResourceType::DataSource, &gone)
                .await
                .unwrap()
                .exists()
        );
        assert!(
            !owner(&db, ResourceType::DataSource, "nope")
                .await
                .unwrap()
                .exists()
        );
    }

    #[tokio::test]
    async fn accessibility_shows_only_own_and_granted_rows() {
        let db = Db::sqlite_temp().await;
        sqlx::query(
            "INSERT INTO users (id, name, password) VALUES ('u-owner', 'owner', 'x'), \
                     ('u-other', 'other', 'x'), ('u-grantee', 'grantee', 'x')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        let owned = insert_source(&db, "u-owner", false).await;
        let public = insert_source(&db, "u-other", false).await;
        let direct = insert_source(&db, "u-other", false).await;

        acl::insert(
            &db,
            &AclEntry {
                id: new_id(),
                resource_type: ResourceType::DataSource,
                resource_id: public.clone(),
                principal_type: PrincipalType::Group,
                principal_id: ALL_USERS.to_string(),
                permission: Permission::View,
                audit: crate::model::base::Audit::new("u-owner"),
            },
        )
        .await
        .unwrap();
        acl::insert(
            &db,
            &AclEntry {
                id: new_id(),
                resource_type: ResourceType::DataSource,
                resource_id: direct.clone(),
                principal_type: PrincipalType::User,
                principal_id: "u-grantee".to_string(),
                permission: Permission::Edit,
                audit: crate::model::base::Audit::new("u-owner"),
            },
        )
        .await
        .unwrap();

        let mut granted = vec![direct.clone(), public.clone()];
        granted.sort();
        assert_eq!(visible_ids(&db, "u-grantee").await, granted);

        let mut own_and_public = vec![owned.clone(), public.clone()];
        own_and_public.sort();
        assert_eq!(
            visible_ids(&db, "u-owner").await,
            own_and_public,
            "own rows, plus the row it shared with everyone"
        );
        assert_eq!(
            visible_ids(&db, "u-other").await.len(),
            2,
            "the two rows it owns itself"
        );
        assert_eq!(
            visible_ids(&db, "u-nobody").await.len(),
            1,
            "only the public row"
        );
    }

    #[tokio::test]
    async fn user_rows_round_trip() {
        let db = Db::sqlite_temp().await;
        let alice = crate::model::user::User {
            id: new_id(),
            name: "alice".into(),
            password: "hash".into(),
            display_name: Some("Alice".into()),
            audit: crate::model::base::Audit::new("system"),
        };
        user::insert(&db, &alice).await.unwrap();
        assert_eq!(
            user::find_by_name(&db, "alice").await.unwrap(),
            Some(alice.clone())
        );
        assert_eq!(user::find_by_id(&db, &alice.id).await.unwrap(), Some(alice));
        assert!(user::find_by_name(&db, "bob").await.unwrap().is_none());
        assert_eq!(user::count(&db).await.unwrap(), 1);
    }
}
