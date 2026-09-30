//! Granting, revoking and listing the ACL entries of one resource.

use crate::auth::AuthUser;
use crate::db::Db;
use crate::error::{DatiError, ErrorCode, Result};
use crate::model::base::Audit;
use crate::model::permission::{
    AclEntry, AclEntryView, GrantRequest, Permission, PrincipalType, ResourceType,
};
use crate::repo::{acl, resource, user};
use crate::time::new_id;

use super::permission::{PermissionService, not_found};

#[derive(Clone)]
pub struct AclService {
    db: Db,
    permissions: PermissionService,
}

impl AclService {
    pub fn new(db: Db, permissions: PermissionService) -> Self {
        Self { db, permissions }
    }

    /// `POST /v1/acls/{type}/{resourceId}`: an idempotent upsert of one principal's level.
    pub async fn grant(
        &self,
        caller: &AuthUser,
        resource_type: ResourceType,
        resource_id: &str,
        request: &GrantRequest,
    ) -> Result<String> {
        let (principal_type, principal_id, permission) = request.validate()?;
        self.require_editable(caller, resource_type, resource_id)
            .await?;
        if principal_type == PrincipalType::User
            && user::find_by_id(&self.db, &principal_id).await?.is_none()
        {
            return Err(DatiError::new(
                ErrorCode::InvalidParameter,
                ["principal_id"],
            ));
        }

        let existing = acl::find(
            &self.db,
            resource_type,
            resource_id,
            principal_type,
            &principal_id,
        )
        .await?;
        match existing {
            Some(entry) => {
                acl::update_permission(&self.db, &entry.id, permission, &caller.id).await?;
                Ok(entry.id)
            }
            None => {
                let entry = AclEntry {
                    id: new_id(),
                    resource_type,
                    resource_id: resource_id.to_string(),
                    principal_type,
                    principal_id,
                    permission,
                    audit: Audit::new(&caller.id),
                };
                acl::insert(&self.db, &entry).await?;
                Ok(entry.id)
            }
        }
    }

    /// `DELETE /v1/acls/{type}/{resourceId}/{principalType}/{principalId}`: revoking a grant that
    /// is not there succeeds, as the derived Spring delete was a no-op.
    pub async fn revoke(
        &self,
        caller: &AuthUser,
        resource_type: ResourceType,
        resource_id: &str,
        principal_type: PrincipalType,
        principal_id: &str,
    ) -> Result<()> {
        self.require_editable(caller, resource_type, resource_id)
            .await?;
        acl::delete(
            &self.db,
            resource_type,
            resource_id,
            principal_type,
            principal_id,
        )
        .await?;
        Ok(())
    }

    /// `GET /v1/acls/{type}/{resourceId}`: only editors see the grant list; group rows keep the
    /// principal id as their display name because no user record stands behind them.
    pub async fn list(
        &self,
        caller: &AuthUser,
        resource_type: ResourceType,
        resource_id: &str,
    ) -> Result<Vec<AclEntryView>> {
        self.require_editable(caller, resource_type, resource_id)
            .await?;
        let entries = acl::list(&self.db, resource_type, resource_id).await?;
        let ids: Vec<String> = entries
            .iter()
            .filter(|entry| entry.principal_type == PrincipalType::User)
            .map(|entry| entry.principal_id.clone())
            .collect();
        let names = user::find_by_ids(&self.db, &ids)
            .await?
            .into_iter()
            .map(|person| (person.id.clone(), person.label()))
            .collect::<std::collections::HashMap<_, _>>();
        Ok(entries
            .iter()
            .map(|entry| AclEntryView::new(entry, names.get(&entry.principal_id).cloned()))
            .collect())
    }

    /// A grant, revoke or read all need edit rights on a resource that still exists, and the Java
    /// service resolved the owner first, so a missing row reported its own not-found code.
    async fn require_editable(
        &self,
        caller: &AuthUser,
        resource_type: ResourceType,
        resource_id: &str,
    ) -> Result<()> {
        match resource::owner(&self.db, resource_type, resource_id).await? {
            resource::Owner::Missing => return Err(not_found(resource_type, resource_id)),
            resource::Owner::Existing { .. } => {}
        }
        self.permissions
            .require(
                &self.db,
                caller,
                resource_type,
                resource_id,
                Permission::Edit,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::datasource::DbType;
    use crate::model::permission::ALL_USERS;
    use crate::repo::user as users;

    struct Fixture {
        service: AclService,
        resource_id: String,
        owner: AuthUser,
        stranger: AuthUser,
    }

    async fn fixture() -> Fixture {
        let db = Db::sqlite_temp().await;
        let config = crate::config::Config::from_source(|_key| None::<String>);
        let permissions = PermissionService::new(&config);
        let owner = person(&db, "alice").await;
        let stranger = person(&db, "bob").await;
        Fixture {
            service: AclService::new(db.clone(), permissions),
            resource_id: source(&db, &owner.id).await,
            owner,
            stranger,
        }
    }

    async fn person(db: &Db, name: &str) -> AuthUser {
        let id = new_id();
        users::insert(
            db,
            &crate::model::user::User {
                id: id.clone(),
                name: name.into(),
                password: "hash".into(),
                display_name: Some(name.to_uppercase()),
                audit: Audit::new("system"),
            },
        )
        .await
        .unwrap();
        AuthUser {
            id,
            name: name.into(),
            display_name: None,
        }
    }

    async fn source(db: &Db, owner_id: &str) -> String {
        let id = new_id();
        let sql = db.sql(
            "INSERT INTO data_source (id, created_by, created_at, updated_by, updated_at, name, \
             deleted, connection_url, type) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        );
        sqlx::query(&sql)
            .bind(&id)
            .bind(owner_id)
            .bind(crate::time::now_iso())
            .bind(owner_id)
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

    fn grant(
        principal_type: PrincipalType,
        principal_id: &str,
        permission: Permission,
    ) -> GrantRequest {
        GrantRequest {
            principal_type: Some(principal_type),
            principal_id: Some(principal_id.to_string()),
            permission: Some(permission),
        }
    }

    const TYPE: ResourceType = ResourceType::DataSource;

    #[tokio::test]
    async fn only_editors_may_touch_the_grant_list() {
        let f = fixture().await;
        let err = f
            .service
            .grant(
                &f.stranger,
                TYPE,
                &f.resource_id,
                &grant(PrincipalType::User, &f.stranger.id, Permission::View),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::PermissionDenied);

        let err = f
            .service
            .list(&f.stranger, TYPE, &f.resource_id)
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::PermissionDenied);

        let err = f
            .service
            .revoke(
                &f.stranger,
                TYPE,
                &f.resource_id,
                PrincipalType::User,
                &f.stranger.id,
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::PermissionDenied);
    }

    #[tokio::test]
    async fn granting_is_an_upsert_and_listing_resolves_names() {
        let f = fixture().await;
        let first = f
            .service
            .grant(
                &f.owner,
                TYPE,
                &f.resource_id,
                &grant(PrincipalType::User, &f.stranger.id, Permission::View),
            )
            .await
            .unwrap();
        let second = f
            .service
            .grant(
                &f.owner,
                TYPE,
                &f.resource_id,
                &grant(PrincipalType::User, &f.stranger.id, Permission::Edit),
            )
            .await
            .unwrap();
        assert_eq!(first, second, "the same principal keeps one row");

        let entries = f
            .service
            .list(&f.owner, TYPE, &f.resource_id)
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].permission, Permission::Edit);
        assert_eq!(entries[0].principal_name.as_deref(), Some("BOB"));
        assert_eq!(entries[0].created_by.as_deref(), Some(f.owner.id.as_str()));

        // The grant now lets the grantee edit, and so to grant on.
        f.service
            .grant(
                &f.stranger,
                TYPE,
                &f.resource_id,
                &grant(PrincipalType::Group, ALL_USERS, Permission::View),
            )
            .await
            .unwrap();
        assert_eq!(
            f.service
                .list(&f.stranger, TYPE, &f.resource_id)
                .await
                .unwrap()
                .len(),
            2
        );

        f.service
            .revoke(
                &f.owner,
                TYPE,
                &f.resource_id,
                PrincipalType::Group,
                ALL_USERS,
            )
            .await
            .unwrap();
        f.service
            .revoke(
                &f.owner,
                TYPE,
                &f.resource_id,
                PrincipalType::Group,
                ALL_USERS,
            )
            .await
            .unwrap();
        assert_eq!(
            f.service
                .list(&f.owner, TYPE, &f.resource_id)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn unknown_resources_principals_and_group_rules_are_rejected() {
        let f = fixture().await;
        let err = f
            .service
            .grant(
                &f.owner,
                TYPE,
                "no-such-row",
                &grant(PrincipalType::User, &f.stranger.id, Permission::View),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::DsNotFound);
        assert_eq!(err.message(), "Data source not found: no-such-row");

        let err = f
            .service
            .grant(
                &f.owner,
                TYPE,
                &f.resource_id,
                &grant(PrincipalType::User, "ghost", Permission::View),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidParameter);
        assert_eq!(err.message(), "Request parameter error: principal_id");

        let err = f
            .service
            .grant(
                &f.owner,
                TYPE,
                &f.resource_id,
                &grant(PrincipalType::Group, "TEAM_A", Permission::View),
            )
            .await
            .unwrap_err();
        assert_eq!(err.message(), "Request parameter error: principal_id");
        let err = f
            .service
            .grant(
                &f.owner,
                TYPE,
                &f.resource_id,
                &grant(PrincipalType::Group, ALL_USERS, Permission::Edit),
            )
            .await
            .unwrap_err();
        assert_eq!(err.message(), "Request parameter error: permission");
    }

    #[tokio::test]
    async fn an_edit_grant_opens_the_resource_to_its_grantee_only() {
        let f = fixture().await;
        f.service
            .grant(
                &f.owner,
                TYPE,
                &f.resource_id,
                &grant(PrincipalType::User, &f.stranger.id, Permission::Edit),
            )
            .await
            .unwrap();
        assert!(
            f.service
                .permissions
                .can(
                    &f.service.db,
                    &f.stranger,
                    TYPE,
                    &f.resource_id,
                    Permission::Edit
                )
                .await
                .unwrap()
        );
    }
}
