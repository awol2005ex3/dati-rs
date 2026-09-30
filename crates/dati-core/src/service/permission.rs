//! The permission ladder: global admin, then resource owner, then an ACL grant.

use crate::auth::AuthUser;
use crate::config::Config;
use crate::db::Db;
use crate::error::{DatiError, ErrorCode, Result};
use crate::model::permission::{ALL_USERS, Permission, PrincipalType, ResourceType};
use crate::repo::{acl, resource};

#[derive(Clone)]
pub struct PermissionService {
    admin_users: Vec<String>,
}

/// The rank a resource owner holds, so ownership is compared like any grant.
const OWNER_PERMISSION: Permission = Permission::Edit;

impl PermissionService {
    pub fn new(config: &Config) -> Self {
        Self {
            admin_users: config.admin_users.clone(),
        }
    }

    /// Admins come from configuration matched against the login name, never from a database column.
    pub fn is_admin(&self, user: &AuthUser) -> bool {
        self.admin_users.iter().any(|name| name == &user.name)
    }

    /// The effective permission of `user` on one resource, cached per request by the caller if needed.
    pub async fn permission_of(
        &self,
        db: &Db,
        user: &AuthUser,
        resource_type: ResourceType,
        resource_id: &str,
    ) -> Result<Option<Permission>> {
        if self.is_admin(user) {
            return Ok(Some(Permission::Edit));
        }
        match resource::owner(db, resource_type, resource_id).await? {
            resource::Owner::Missing => return Ok(None),
            resource::Owner::Existing { created_by }
                if created_by.as_deref() == Some(user.id.as_str()) =>
            {
                return Ok(Some(OWNER_PERMISSION));
            }
            resource::Owner::Existing { .. } => {}
        }
        let mut best: Option<Permission> = None;
        for (principal_type, principal_id) in [
            (PrincipalType::User, user.id.as_str()),
            (PrincipalType::Group, ALL_USERS),
        ] {
            if let Some(entry) =
                acl::find(db, resource_type, resource_id, principal_type, principal_id).await?
            {
                best = Some(match best {
                    Some(current) if current.covers(entry.permission) => current,
                    _ => entry.permission,
                });
            }
        }
        Ok(best)
    }

    pub async fn can(
        &self,
        db: &Db,
        user: &AuthUser,
        resource_type: ResourceType,
        resource_id: &str,
        required: Permission,
    ) -> Result<bool> {
        let granted = self
            .permission_of(db, user, resource_type, resource_id)
            .await?;
        Ok(granted.is_some_and(|permission| permission.covers(required)))
    }

    /// The Java services checked permission after resolving the owner, so a missing resource
    /// reported its own not-found code rather than a bare denial.
    pub async fn require(
        &self,
        db: &Db,
        user: &AuthUser,
        resource_type: ResourceType,
        resource_id: &str,
        required: Permission,
    ) -> Result<()> {
        if !self
            .can(db, user, resource_type, resource_id, required)
            .await?
        {
            return Err(denied());
        }
        Ok(())
    }

    pub fn require_admin(&self, user: &AuthUser) -> Result<()> {
        if self.is_admin(user) {
            Ok(())
        } else {
            Err(denied())
        }
    }
}

pub fn denied() -> DatiError {
    DatiError::with_message(
        ErrorCode::PermissionDenied,
        ErrorCode::PermissionDenied.template(),
    )
}

/// The not-found code belongs to the resource, mirroring the Java per-module error tables.
pub fn not_found(resource_type: ResourceType, id: &str) -> DatiError {
    let code = match resource_type {
        ResourceType::DataSource => ErrorCode::DsNotFound,
        ResourceType::Subject => ErrorCode::SmSubjectNotFound,
        ResourceType::McpService => ErrorCode::MsServiceNotFound,
    };
    DatiError::new(code, [id])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::base::Audit;
    use crate::model::datasource::DbType;
    use crate::model::permission::AclEntry;
    use crate::repo::user;
    use crate::time::new_id;

    fn authed(id: &str, name: &str) -> AuthUser {
        AuthUser {
            id: id.into(),
            name: name.into(),
            display_name: None,
        }
    }

    fn config(admins: &str) -> Config {
        Config::from_source(|key| (key == "ADMIN_USERS").then(|| admins.to_string()))
    }

    async fn source_owned_by(db: &Db, owner: &str) -> String {
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
            .bind(false)
            .bind("sqlite:demo.db")
            .bind(DbType::Sqlite.as_str())
            .execute(db.pool())
            .await
            .unwrap();
        id
    }

    async fn person(db: &Db, name: &str) -> AuthUser {
        let id = new_id();
        user::insert(
            db,
            &crate::model::user::User {
                id: id.clone(),
                name: name.into(),
                password: "hash".into(),
                display_name: None,
                audit: Audit::new(name),
            },
        )
        .await
        .unwrap();
        authed(&id, name)
    }

    #[tokio::test]
    async fn admin_owner_and_grants_rank_in_order() {
        let db = Db::sqlite_temp().await;
        let service = PermissionService::new(&config("root"));
        let owner = person(&db, "alice").await;
        let stranger = person(&db, "carol").await;
        let grantee = person(&db, "dave").await;
        let id = source_owned_by(&db, &owner.id).await;

        assert!(
            service
                .can(&db, &owner, ResourceType::DataSource, &id, Permission::Edit)
                .await
                .unwrap()
        );
        assert!(
            !service
                .can(
                    &db,
                    &stranger,
                    ResourceType::DataSource,
                    &id,
                    Permission::View
                )
                .await
                .unwrap(),
            "another user's private row is invisible"
        );

        acl::insert(
            &db,
            &AclEntry {
                id: new_id(),
                resource_type: ResourceType::DataSource,
                resource_id: id.clone(),
                principal_type: PrincipalType::User,
                principal_id: grantee.id.clone(),
                permission: Permission::View,
                audit: Audit::new(&owner.id),
            },
        )
        .await
        .unwrap();
        assert!(
            service
                .can(
                    &db,
                    &grantee,
                    ResourceType::DataSource,
                    &id,
                    Permission::View
                )
                .await
                .unwrap()
        );
        assert!(
            !service
                .can(
                    &db,
                    &grantee,
                    ResourceType::DataSource,
                    &id,
                    Permission::Edit
                )
                .await
                .unwrap(),
            "VIEW is not EDIT"
        );

        let root = authed("u-root", "root");
        assert!(
            service
                .can(&db, &root, ResourceType::DataSource, &id, Permission::Edit)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn the_public_group_grants_view_to_everyone() {
        let db = Db::sqlite_temp().await;
        let service = PermissionService::new(&config(""));
        let owner = person(&db, "alice").await;
        let other = person(&db, "bob").await;
        let id = source_owned_by(&db, &owner.id).await;
        acl::insert(
            &db,
            &AclEntry {
                id: new_id(),
                resource_type: ResourceType::DataSource,
                resource_id: id.clone(),
                principal_type: PrincipalType::Group,
                principal_id: ALL_USERS.to_string(),
                permission: Permission::View,
                audit: Audit::new(&owner.id),
            },
        )
        .await
        .unwrap();

        assert!(
            service
                .can(&db, &other, ResourceType::DataSource, &id, Permission::View)
                .await
                .unwrap()
        );
        assert!(
            !service
                .can(&db, &other, ResourceType::DataSource, &id, Permission::Edit)
                .await
                .unwrap()
        );
        assert!(
            service
                .require(&db, &other, ResourceType::DataSource, &id, Permission::View)
                .await
                .is_ok()
        );
        let err = service
            .require(&db, &other, ResourceType::DataSource, &id, Permission::Edit)
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::PermissionDenied);
        assert_eq!(err.status(), 403);
    }

    #[test]
    fn admin_is_a_username_list_and_require_admin_is_per_request() {
        let service = PermissionService::new(&config("root, ops"));
        assert!(service.is_admin(&authed("u", "ops")));
        assert!(!service.is_admin(&authed("u", "admin")));
        assert!(service.require_admin(&authed("u", "root")).is_ok());
        let err = service.require_admin(&authed("u", "guest")).unwrap_err();
        assert_eq!(err.code().code(), "PM001");
        assert_eq!(err.status(), 403);
    }

    #[test]
    fn not_found_uses_the_modules_own_code() {
        assert_eq!(
            not_found(ResourceType::DataSource, "x").code().code(),
            "DS002"
        );
        assert_eq!(not_found(ResourceType::Subject, "x").code().code(), "SM001");
        assert_eq!(
            not_found(ResourceType::McpService, "x").code().code(),
            "MS001"
        );
        assert_eq!(
            not_found(ResourceType::Subject, "s-9").message(),
            "Subject not found: s-9"
        );
    }
}
