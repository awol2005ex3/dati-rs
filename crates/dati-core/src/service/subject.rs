//! Subjects: the business domains a data source is described with, and the tables they own.
//!
//! This is the Java `SubjectService`. Where that service wrote a document into the Elasticsearch
//! index on every create, update and delete, the search layer here reads the `subject`, `term` and
//! `table_info` rows directly, so there is no second copy of the metadata to keep in step.

use std::collections::HashMap;

use crate::auth::AuthUser;
use crate::db::Db;
use crate::error::{DatiError, ErrorCode, Result};
use crate::model::base::{Audit, PageReq, PageResponse};
use crate::model::datasource::{TableInfo, TableInfoView};
use crate::model::permission::{Permission, ResourceType};
use crate::model::semantic::{
    AvailableTable, CreateSubjectRequest, Subject, SubjectView, UpdateSubjectRequest,
};
use crate::repo::resource::Visibility;
use crate::repo::{datasource, subject, table};
use crate::time::new_id;

use super::datasource::{DataSourceService, author_names};
use super::permission::PermissionService;

#[derive(Clone)]
pub struct SubjectService {
    db: Db,
    data_sources: DataSourceService,
}

impl SubjectService {
    pub fn new(db: Db, data_sources: DataSourceService) -> Self {
        Self { db, data_sources }
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn permissions(&self) -> &PermissionService {
        self.data_sources.permissions()
    }

    /// The stored row, or `SM001`. Every semantic endpoint resolves the subject before it asks
    /// whether the caller may act on it, so a bad id never reports a bare denial.
    pub async fn load(&self, id: &str) -> Result<Subject> {
        subject::find(&self.db, id)
            .await?
            .ok_or_else(|| DatiError::new(ErrorCode::SmSubjectNotFound, [id]))
    }

    /// The subject plus the right it grants: `PermissionService.requireSubject(id, permission)`.
    pub async fn authorize(
        &self,
        caller: &AuthUser,
        id: &str,
        permission: Permission,
    ) -> Result<Subject> {
        let row = self.load(id).await?;
        self.permissions()
            .require(&self.db, caller, ResourceType::Subject, id, permission)
            .await?;
        Ok(row)
    }

    /// `POST /v1/subjects`: creating a subject in someone else's data source needs `VIEW` on it,
    /// which is what the Java service checked before the insert.
    pub async fn create(
        &self,
        caller: &AuthUser,
        request: &CreateSubjectRequest,
    ) -> Result<String> {
        request.validate()?;
        let datasource_id = request.datasource_id.clone().unwrap_or_default();
        self.data_sources
            .authorize(caller, &datasource_id, Permission::View)
            .await?;
        let row = Subject {
            id: new_id(),
            name: request.name.clone(),
            description: request.description.clone(),
            deleted: false,
            datasource_id,
            aliases: request.aliases.clone(),
            audit: Audit::new(&caller.id),
        };
        subject::insert(&self.db, &row).await
    }

    /// `PUT /v1/subjects/{id}`: only the fields the body carries are written, so an absent name
    /// keeps the stored one; the owning data source cannot be moved.
    pub async fn update(
        &self,
        caller: &AuthUser,
        id: &str,
        request: &UpdateSubjectRequest,
    ) -> Result<()> {
        let mut current = self.authorize(caller, id, Permission::Edit).await?;
        if let Some(name) = request.name.clone() {
            current.name = Some(name);
        }
        if let Some(description) = request.description.clone() {
            current.description = Some(description);
        }
        if let Some(aliases) = request.aliases.clone() {
            current.aliases = aliases;
        }
        current.audit.touch(&caller.id);
        subject::update(&self.db, &current).await
    }

    /// `DELETE /v1/subjects/{id}`. The table bindings stay behind, exactly as the Java delete left
    /// them: nothing reads a binding whose subject is gone.
    pub async fn remove(&self, caller: &AuthUser, id: &str) -> Result<()> {
        self.authorize(caller, id, Permission::Edit).await?;
        subject::remove(&self.db, id).await?;
        Ok(())
    }

    /// `GET /v1/subjects/{id}`.
    pub async fn get(&self, caller: &AuthUser, id: &str) -> Result<SubjectView> {
        let row = self.authorize(caller, id, Permission::View).await?;
        let names = author_names(&self.db, [&row.audit]).await?;
        let datasource_names = self
            .datasource_names(std::slice::from_ref(&row.datasource_id))
            .await?;
        Ok(label(SubjectView::new(&row, &names), &datasource_names))
    }

    /// `GET /v1/subjects`: admins see every subject, everyone else the ones they own or were
    /// granted.
    pub async fn list(
        &self,
        caller: &AuthUser,
        keyword: Option<&str>,
        page: PageReq,
    ) -> Result<PageResponse<SubjectView>> {
        let visibility = if self.permissions().is_admin(caller) {
            Visibility::All
        } else {
            Visibility::User(&caller.id)
        };
        let (rows, total) = subject::page(&self.db, keyword, visibility, page).await?;
        let names = author_names(&self.db, rows.iter().map(|row| &row.audit)).await?;
        let datasource_names = self
            .datasource_names(
                &rows
                    .iter()
                    .map(|row| row.datasource_id.clone())
                    .collect::<Vec<_>>(),
            )
            .await?;
        Ok(PageResponse::new(
            rows.iter()
                .map(|row| label(SubjectView::new(row, &names), &datasource_names))
                .collect(),
            total,
            page.page,
            page.size,
        ))
    }

    /// `GET /v1/subjects/{id}/tables`: the tables bound to the subject.
    pub async fn tables(
        &self,
        caller: &AuthUser,
        id: &str,
        keyword: Option<&str>,
        page: PageReq,
    ) -> Result<PageResponse<TableInfoView>> {
        self.authorize(caller, id, Permission::View).await?;
        let (rows, total) = subject::page_tables(&self.db, id, keyword, page).await?;
        let names = author_names(&self.db, rows.iter().map(|row| &row.audit)).await?;
        Ok(PageResponse::new(
            rows.iter()
                .map(|row| TableInfoView::new(row, &names))
                .collect(),
            total,
            page.page,
            page.size,
        ))
    }

    /// `GET /v1/subjects/{id}/available-tables?schema=`: the picker list of live tables the
    /// subject does not hold yet.
    pub async fn available_tables(
        &self,
        caller: &AuthUser,
        id: &str,
        schema: &str,
    ) -> Result<Vec<AvailableTable>> {
        let row = self.authorize(caller, id, Permission::View).await?;
        let bound = subject::table_ids(&self.db, id).await?;
        let tables =
            table::list_by_datasource_and_schema(&self.db, &row.datasource_id, schema).await?;
        Ok(tables
            .iter()
            .filter(|table| !bound.iter().any(|id| id == &table.id))
            .map(available)
            .collect())
    }

    /// `POST /v1/subjects/{id}/tables`: only a table of the subject's own data source can be
    /// bound, and both rejections name the table rather than its id.
    pub async fn add_table(&self, caller: &AuthUser, id: &str, table_id: &str) -> Result<()> {
        let row = self.authorize(caller, id, Permission::Edit).await?;
        let table = table::find(&self.db, table_id)
            .await?
            .ok_or_else(|| DatiError::new(ErrorCode::DsNotFound, [table_id]))?;
        let name = table.name.as_deref().unwrap_or_default();
        if table.data_source_id != row.datasource_id {
            return Err(DatiError::new(ErrorCode::SmTableNotInSubject, [name, id]));
        }
        if subject::contains(&self.db, id, table_id).await? {
            return Err(DatiError::new(
                ErrorCode::SmTableAlreadyAssociated,
                [name, id],
            ));
        }
        subject::link(&self.db, id, table_id, &Audit::new(&caller.id)).await?;
        Ok(())
    }

    /// `DELETE /v1/subjects/{id}/tables/{tableId}`: dropping a binding that was never made is a
    /// 404 (`SM006`), not the silent success the delete-by-filter would have been.
    pub async fn remove_table(&self, caller: &AuthUser, id: &str, table_id: &str) -> Result<()> {
        self.authorize(caller, id, Permission::Edit).await?;
        if subject::unlink(&self.db, id, table_id).await? == 0 {
            return Err(DatiError::new(
                ErrorCode::SmAssociationNotFound,
                [id, table_id],
            ));
        }
        Ok(())
    }

    /// Subject ids to names, for the term responses that label their subject.
    pub async fn name_map(&self, ids: &[String]) -> Result<HashMap<String, String>> {
        subject::name_map(&self.db, ids).await
    }

    async fn datasource_names(&self, ids: &[String]) -> Result<HashMap<String, String>> {
        datasource::name_map(&self.db, ids).await
    }
}

fn label(view: SubjectView, names: &HashMap<String, String>) -> SubjectView {
    let datasource_name = names.get(&view.datasource_id).cloned();
    view.with_datasource_name(datasource_name)
}

fn available(table: &TableInfo) -> AvailableTable {
    AvailableTable {
        table_id: table.id.clone(),
        table_name: table.name.clone(),
        schema: table.schema.clone(),
        description: table.description.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::error::ErrorCode;
    use crate::model::datasource::{DataSource, DbType};
    use crate::model::permission::Permission;
    use crate::model::user::User;
    use crate::repo::{acl, user};

    fn authed(id: &str, name: &str) -> AuthUser {
        AuthUser {
            id: id.into(),
            name: name.into(),
            display_name: None,
        }
    }

    async fn person(db: &Db, name: &str) -> AuthUser {
        let id = new_id();
        user::insert(
            db,
            &User {
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

    async fn source(db: &Db, owner: &str) -> DataSource {
        let row = DataSource {
            id: new_id(),
            name: Some("warehouse".into()),
            description: None,
            deleted: false,
            connection_url: Some("sqlite:demo.db".into()),
            db_type: DbType::Sqlite,
            user_name: Some("none".into()),
            password: None,
            default_schema: Some("main".into()),
            audit: Audit::new(owner),
        };
        datasource::insert(db, &row).await.unwrap();
        row
    }

    fn service(db: &Db) -> SubjectService {
        let config = Config::from_source(|_| None);
        let permissions = PermissionService::new(&config);
        SubjectService::new(
            db.clone(),
            DataSourceService::new(db.clone(), &config, permissions),
        )
    }

    fn create(name: &str, datasource_id: &str) -> CreateSubjectRequest {
        CreateSubjectRequest {
            name: Some(name.into()),
            description: Some("订单业务域".into()),
            datasource_id: Some(datasource_id.into()),
            aliases: vec!["交易".into()],
        }
    }

    #[tokio::test]
    async fn a_subject_can_only_be_founded_on_a_visible_data_source() {
        let db = Db::sqlite_temp().await;
        let service = service(&db);
        let owner = person(&db, "alice").await;
        let stranger = person(&db, "carol").await;
        let source = source(&db, &owner.id).await;

        let err = service
            .create(&stranger, &create("orders", &source.id))
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::PermissionDenied);

        let id = service
            .create(&owner, &create("orders", &source.id))
            .await
            .unwrap();
        let found = service.get(&owner, &id).await.unwrap();
        assert_eq!(found.name.as_deref(), Some("orders"));
        assert_eq!(found.datasource_name.as_deref(), Some("warehouse"));
        assert_eq!(found.created_user_name.as_deref(), Some("alice"));

        let err = service
            .create(&owner, &create("orders", "missing"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::DsNotFound);
    }

    #[tokio::test]
    async fn updates_merge_and_only_editors_may_write() {
        let db = Db::sqlite_temp().await;
        let service = service(&db);
        let owner = person(&db, "alice").await;
        let source = source(&db, &owner.id).await;
        let id = service
            .create(&owner, &create("orders", &source.id))
            .await
            .unwrap();

        service
            .update(
                &owner,
                &id,
                &UpdateSubjectRequest {
                    description: Some("改过的说明".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let stored = service.load(&id).await.unwrap();
        assert_eq!(stored.name.as_deref(), Some("orders"), "absent name kept");
        assert_eq!(stored.aliases, vec!["交易".to_string()]);
        assert_eq!(stored.description.as_deref(), Some("改过的说明"));

        service
            .update(
                &owner,
                &id,
                &UpdateSubjectRequest {
                    aliases: Some(Vec::new()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(service.load(&id).await.unwrap().aliases.is_empty());

        let stranger = person(&db, "carol").await;
        let err = service
            .update(
                &stranger,
                &id,
                &UpdateSubjectRequest {
                    name: Some("hijacked".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::PermissionDenied);
        assert_eq!(err.status(), 403);
        assert_eq!(service.load(&id).await.unwrap().id, id);
    }

    #[tokio::test]
    async fn listing_shows_only_what_the_caller_may_see() {
        let db = Db::sqlite_temp().await;
        let service = service(&db);
        let owner = person(&db, "alice").await;
        let admin = person(&db, "root").await;
        let source = source(&db, &owner.id).await;
        let mine = service
            .create(&owner, &create("orders", &source.id))
            .await
            .unwrap();
        let other = Subject {
            id: new_id(),
            name: Some("hr".into()),
            description: None,
            deleted: false,
            datasource_id: source.id.clone(),
            aliases: Vec::new(),
            audit: Audit::new("u-someone-else"),
        };
        subject::insert(&db, &other).await.unwrap();

        let page = PageReq { page: 1, size: 10 };
        let visible = service.list(&owner, None, page).await.unwrap();
        assert_eq!(visible.total, 1);
        assert_eq!(visible.data[0].id, mine);

        // `root` is not the configured admin (`admin` is), so it needs a grant to see this row.
        acl::insert(
            &db,
            &crate::model::permission::AclEntry {
                id: new_id(),
                resource_type: ResourceType::Subject,
                resource_id: other.id.clone(),
                principal_type: crate::model::permission::PrincipalType::User,
                principal_id: admin.id.clone(),
                permission: Permission::View,
                audit: Audit::new(&owner.id),
            },
        )
        .await
        .unwrap();
        let names: Vec<String> = service
            .list(&admin, None, page)
            .await
            .unwrap()
            .data
            .into_iter()
            .map(|row| row.name.unwrap_or_default())
            .collect();
        assert_eq!(names, vec!["hr".to_string()]);
    }

    #[tokio::test]
    async fn bindings_round_trip_through_the_service() {
        let db = Db::sqlite_temp().await;
        let service = service(&db);
        let owner = person(&db, "alice").await;
        let source = source(&db, &owner.id).await;
        let subject_id = service
            .create(&owner, &create("orders", &source.id))
            .await
            .unwrap();

        let orders = TableInfo {
            id: new_id(),
            name: Some("orders".into()),
            description: Some("订单表".into()),
            deleted: false,
            data_source_id: source.id.clone(),
            schema: Some("main".into()),
            aliases: Vec::new(),
            audit: Audit::new(&owner.id),
        };
        let other = TableInfo {
            id: new_id(),
            name: Some("salaries".into()),
            data_source_id: new_id(),
            ..orders.clone()
        };
        table::insert(&db, &orders).await.unwrap();
        table::insert(&db, &other).await.unwrap();

        let available = service
            .available_tables(&owner, &subject_id, "main")
            .await
            .unwrap();
        assert_eq!(
            available.len(),
            1,
            "only the subject's own source is listed"
        );
        assert_eq!(available[0].table_name.as_deref(), Some("orders"));

        subject::link(&db, &subject_id, &orders.id, &Audit::new(&owner.id))
            .await
            .unwrap();
        assert!(
            service
                .available_tables(&owner, &subject_id, "main")
                .await
                .unwrap()
                .is_empty()
        );

        let page = PageReq { page: 1, size: 10 };
        let bound = service
            .tables(&owner, &subject_id, None, page)
            .await
            .unwrap();
        assert_eq!(bound.total, 1);
        assert_eq!(bound.data[0].name.as_deref(), Some("orders"));
        assert_eq!(bound.data[0].created_user_name.as_deref(), Some("alice"));

        service.remove(&owner, &subject_id).await.unwrap();
        let err = service.get(&owner, &subject_id).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::SmSubjectNotFound);
        assert_eq!(err.status(), 404);
    }

    #[tokio::test]
    async fn a_table_is_bound_within_the_subjects_source_and_unbound_once() {
        let db = Db::sqlite_temp().await;
        let service = service(&db);
        let owner = person(&db, "alice").await;
        let home = source(&db, &owner.id).await;
        let elsewhere = source(&db, &owner.id).await;
        let subject_id = service
            .create(&owner, &create("orders", &home.id))
            .await
            .unwrap();

        let orders = TableInfo {
            id: new_id(),
            name: Some("orders".into()),
            description: None,
            deleted: false,
            data_source_id: home.id.clone(),
            schema: Some("main".into()),
            aliases: Vec::new(),
            audit: Audit::new(&owner.id),
        };
        let salaries = TableInfo {
            id: new_id(),
            name: Some("salaries".into()),
            data_source_id: elsewhere.id.clone(),
            ..orders.clone()
        };
        table::insert(&db, &orders).await.unwrap();
        table::insert(&db, &salaries).await.unwrap();

        let err = service
            .add_table(&owner, &subject_id, "missing")
            .await
            .unwrap_err();
        assert_eq!(
            err.message(),
            "Data source not found: missing",
            "the Java service looked the table up with the data source's error code"
        );

        let err = service
            .add_table(&owner, &subject_id, &salaries.id)
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::SmTableNotInSubject);
        assert_eq!(
            err.message(),
            format!("Table salaries does not belong to subject {subject_id}"),
            "a rejection names the table, not its id"
        );

        service
            .add_table(&owner, &subject_id, &orders.id)
            .await
            .unwrap();
        let err = service
            .add_table(&owner, &subject_id, &orders.id)
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::SmTableAlreadyAssociated);
        assert_eq!(
            err.message(),
            format!("Table orders is already associated with subject {subject_id}")
        );

        let err = service
            .remove_table(&owner, &subject_id, &salaries.id)
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::SmAssociationNotFound);
        assert_eq!(err.status(), 404);
        assert_eq!(
            err.message(),
            format!(
                "Association between subject {subject_id} and table {} not found",
                salaries.id
            )
        );

        service
            .remove_table(&owner, &subject_id, &orders.id)
            .await
            .unwrap();
        let available = service
            .available_tables(&owner, &subject_id, "main")
            .await
            .unwrap();
        assert_eq!(available.len(), 1, "the table is free to bind again");
    }
}
