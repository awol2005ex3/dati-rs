//! MCP services: the endpoints a client connects to, and the metadata each one may talk about.
//!
//! This is the Java `McpServiceService` and `McpServiceDataScopeService` in one object, because a
//! service without a data scope is not usable and the two were always called together.

use std::collections::HashMap;

use crate::auth::AuthUser;
use crate::config::Config;
use crate::db::Db;
use crate::error::{DatiError, ErrorCode, NO_ARGS, Result};
use crate::model::base::{Audit, PageReq, PageResponse};
use crate::model::mcp::{
    DataScopeItem, DataScopeRequest, DataScopeResponse, DataSourceRef, McpDataScope,
    McpDataScopeType, McpService, McpServiceCreateRequest, McpServiceStatus,
    McpServiceUpdateRequest, McpServiceView, code_is_valid,
};
use crate::model::permission::{Permission, ResourceType};
use crate::repo::resource::Visibility;
use crate::repo::{datasource, mcp_prompt, mcp_service, mcp_snapshot, mcp_tool, subject};
use crate::time::new_id;

use super::datasource::{DataSourceService, author_names};
use super::permission::PermissionService;
use super::subject::SubjectService;

#[derive(Clone)]
pub struct McpServiceService {
    db: Db,
    data_sources: DataSourceService,
    subjects: SubjectService,
    endpoint_base_url: String,
}

impl McpServiceService {
    pub fn new(
        db: Db,
        data_sources: DataSourceService,
        subjects: SubjectService,
        config: &Config,
    ) -> Self {
        Self {
            db,
            data_sources,
            subjects,
            endpoint_base_url: config.mcp_endpoint_base_url.clone(),
        }
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn permissions(&self) -> &PermissionService {
        self.data_sources.permissions()
    }

    /// The stored row, or `MS001`. Every endpoint resolves the service before it asks whether the
    /// caller may act on it, so a bad id never reports a bare denial.
    pub async fn load(&self, id: &str) -> Result<McpService> {
        mcp_service::find(&self.db, id)
            .await?
            .ok_or_else(|| DatiError::new(ErrorCode::MsServiceNotFound, [id]))
    }

    /// The MCP endpoint is addressed by code rather than by id.
    pub async fn load_by_code(&self, code: &str) -> Result<McpService> {
        mcp_service::find_by_code(&self.db, code)
            .await?
            .ok_or_else(|| DatiError::new(ErrorCode::MsServiceNotFound, [code]))
    }

    pub async fn authorize(
        &self,
        caller: &AuthUser,
        id: &str,
        permission: Permission,
    ) -> Result<McpService> {
        let row = self.load(id).await?;
        self.permissions()
            .require(&self.db, caller, ResourceType::McpService, id, permission)
            .await?;
        Ok(row)
    }

    /// `POST /v1/mcp-services`. The scope is checked before anything is written, so a service that
    /// points at data the caller cannot see is never stored.
    pub async fn create(
        &self,
        caller: &AuthUser,
        request: &McpServiceCreateRequest,
    ) -> Result<String> {
        request.validate()?;
        let code = request.code.clone().unwrap_or_default();
        if code.trim().is_empty() {
            // Unreachable over REST, where `@NotBlank code` already answered `CM001`.
            return Err(DatiError::new(ErrorCode::MsServiceCodeRequired, NO_ARGS));
        }
        if !code_is_valid(&code) {
            return Err(DatiError::new(ErrorCode::MsServiceCodeInvalid, NO_ARGS));
        }
        if mcp_service::code_taken(&self.db, &code).await? {
            return Err(DatiError::new(
                ErrorCode::MsServiceCodeExists,
                [code.clone()],
            ));
        }
        if request.data_scopes.is_empty() {
            return Err(DatiError::new(
                ErrorCode::MsServiceDataScopeRequired,
                NO_ARGS,
            ));
        }
        self.require_scope_readable(caller, &request.data_scopes)
            .await?;

        let audit = Audit::new(&caller.id);
        let row = McpService {
            id: new_id(),
            name: request.name.clone(),
            description: request.description.clone(),
            deleted: false,
            code,
            // Whatever the client sent, a new service starts as a draft.
            status: Some(McpServiceStatus::Draft),
            active_version_id: None,
            active_version_number: None,
            audit: audit.clone(),
        };
        mcp_service::insert(&self.db, &row).await?;
        for item in &request.data_scopes {
            mcp_service::insert_scope(&self.db, &row.id, &of(item, &row.id), &audit).await?;
        }
        Ok(row.id)
    }

    /// `PUT /v1/mcp-services/{id}`: only the name and the description move, and a field the body
    /// omits keeps its stored value.
    pub async fn update(
        &self,
        caller: &AuthUser,
        id: &str,
        request: &McpServiceUpdateRequest,
    ) -> Result<()> {
        let mut row = self.authorize(caller, id, Permission::Edit).await?;
        if let Some(name) = &request.name {
            row.name = Some(name.clone());
        }
        if let Some(description) = &request.description {
            row.description = Some(description.clone());
        }
        let who = request.updated_by.clone().unwrap_or(caller.id.clone());
        row.audit.touch(&who);
        mcp_service::update_profile(&self.db, &row).await
    }

    /// `GET /v1/mcp-services/{id}`.
    pub async fn get(&self, caller: &AuthUser, id: &str) -> Result<McpServiceView> {
        let row = self.authorize(caller, id, Permission::View).await?;
        let names = author_names(&self.db, [&row.audit]).await?;
        let counts = mcp_tool::counts_by_service(&self.db, std::slice::from_ref(&row.id)).await?;
        Ok(McpServiceView::new(
            &row,
            &names,
            &self.endpoint_base_url,
            mcp_tool::total(&counts, id),
        ))
    }

    /// `GET /v1/mcp-services`: newest first, and a non-admin only sees what it owns or was granted.
    pub async fn list(
        &self,
        caller: &AuthUser,
        keyword: Option<&str>,
        status: Option<McpServiceStatus>,
        page_req: PageReq,
    ) -> Result<PageResponse<McpServiceView>> {
        let visibility = if self.permissions().is_admin(caller) {
            Visibility::All
        } else {
            Visibility::User(&caller.id)
        };
        let (rows, total) =
            mcp_service::page(&self.db, keyword, status, visibility, page_req).await?;
        let names = author_names(&self.db, rows.iter().map(|row| &row.audit)).await?;
        let ids: Vec<String> = rows.iter().map(|row| row.id.clone()).collect();
        let counts = mcp_tool::counts_by_service(&self.db, &ids).await?;
        let data = rows
            .iter()
            .map(|row| {
                McpServiceView::new(
                    row,
                    &names,
                    &self.endpoint_base_url,
                    mcp_tool::total(&counts, &row.id),
                )
            })
            .collect();
        Ok(PageResponse::new(data, total, page_req.page, page_req.size))
    }

    /// `DELETE /v1/mcp-services/{id}`: the whole configuration graph goes with the service, and a
    /// published service needs no disabling first.
    pub async fn remove(&self, caller: &AuthUser, id: &str) -> Result<()> {
        self.authorize(caller, id, Permission::Edit).await?;
        mcp_service::clear_scopes(&self.db, id).await?;
        mcp_tool::remove_all_prebuilt(&self.db, id).await?;
        mcp_tool::remove_all(&self.db, id).await?;
        mcp_prompt::remove_all(&self.db, id).await?;
        mcp_snapshot::remove_all(&self.db, id).await?;
        mcp_service::remove(&self.db, id).await?;
        Ok(())
    }

    /// `GET /v1/mcp-services/{id}/data-scope`: the stored rows with their reference names, plus the
    /// data sources the scope resolves to.
    pub async fn data_scope(&self, caller: &AuthUser, id: &str) -> Result<DataScopeResponse> {
        self.authorize(caller, id, Permission::View).await?;
        self.read_data_scope(id).await
    }

    /// The same payload for a caller who was already checked: the publish path needs the scope of
    /// its own service several times over.
    pub async fn read_data_scope(&self, id: &str) -> Result<DataScopeResponse> {
        let rows = mcp_service::scopes(&self.db, id).await?;
        let source_names =
            datasource::name_map(&self.db, &referenced(&rows, McpDataScopeType::DataSource))
                .await?;
        let subject_names =
            subject::name_map(&self.db, &referenced(&rows, McpDataScopeType::Subject)).await?;
        let items = rows
            .iter()
            .map(|row| DataScopeItem {
                id: Some(row.id.clone()),
                scope_type: Some(row.scope_type),
                reference_id: Some(row.reference_id.clone()),
                reference_name: Some(label(row, &source_names, &subject_names)),
            })
            .collect();
        let resolved_ids = self.resolve(&rows).await?;
        let resolved = resolved_ids
            .into_iter()
            .map(|id| DataSourceRef {
                name: source_names.get(&id).cloned(),
                id,
            })
            .collect();
        Ok(DataScopeResponse {
            items,
            resolved_data_sources: resolved,
        })
    }

    /// `PUT /v1/mcp-services/{id}/data-scope`: a whole replacement, validated before the old rows
    /// are dropped, so an invalid item leaves the previous scope intact.
    pub async fn save_data_scope(
        &self,
        caller: &AuthUser,
        id: &str,
        request: &DataScopeRequest,
    ) -> Result<()> {
        request.validate()?;
        self.authorize(caller, id, Permission::Edit).await?;
        self.require_scope_readable(caller, &request.items).await?;
        let audit = Audit::new(&caller.id);
        mcp_service::clear_scopes(&self.db, id).await?;
        for item in &request.items {
            mcp_service::insert_scope(&self.db, id, &of(item, id), &audit).await?;
        }
        Ok(())
    }

    /// The scope rows as stored, for the publish path and the endpoint.
    pub async fn scope_rows(&self, id: &str) -> Result<Vec<McpDataScope>> {
        mcp_service::scopes(&self.db, id).await
    }

    /// The data sources a scope covers: every direct reference plus the source behind every
    /// subject. Resolving does not re-check rights — the Java service checked them when the scope
    /// was written, and an endpoint call runs on a published snapshot.
    pub async fn resolved_data_source_ids(&self, id: &str) -> Result<Vec<String>> {
        let rows = mcp_service::scopes(&self.db, id).await?;
        self.resolve(&rows).await
    }

    async fn resolve(&self, rows: &[McpDataScope]) -> Result<Vec<String>> {
        let mut sources: Vec<String> = Vec::new();
        let mut subjects: Vec<String> = Vec::new();
        for row in rows {
            let bucket = match row.scope_type {
                McpDataScopeType::DataSource => &mut sources,
                McpDataScopeType::Subject => &mut subjects,
            };
            if !bucket.contains(&row.reference_id) {
                bucket.push(row.reference_id.clone());
            }
        }
        if !subjects.is_empty() {
            let map = subject::source_map(&self.db, &subjects).await?;
            // A subject that has gone away contributes nothing, exactly like the Java lookup that
            // dropped the missing ids. Sorted because the Java version passed them through a
            // `HashSet` first, which made their order in the response unreproducible.
            let mut expanded: Vec<String> = subjects
                .iter()
                .filter_map(|id| map.get(id).cloned())
                .collect();
            expanded.sort();
            for id in expanded {
                if !sources.contains(&id) {
                    sources.push(id);
                }
            }
        }
        Ok(sources)
    }

    /// `VIEW` on every referenced data source or subject, which is what the Java service required
    /// before it stored a scope pointing at someone else's metadata.
    async fn require_scope_readable(
        &self,
        caller: &AuthUser,
        items: &[DataScopeItem],
    ) -> Result<()> {
        for item in items {
            // A row that names neither is skipped as the Java `switch` skipped an unknown type;
            // validation has already rejected it on every REST path.
            let (Some(scope_type), Some(reference_id)) = (item.scope_type, &item.reference_id)
            else {
                continue;
            };
            match scope_type {
                McpDataScopeType::DataSource => {
                    self.data_sources
                        .authorize(caller, reference_id, Permission::View)
                        .await?;
                }
                McpDataScopeType::Subject => {
                    self.subjects
                        .authorize(caller, reference_id, Permission::View)
                        .await?;
                }
            }
        }
        Ok(())
    }

    /// The same `VIEW` walk over stored scope rows rather than request items: the publish path
    /// re-checks that the caller may still see everything a service's scope points at before it
    /// freezes that scope into a version.
    pub async fn require_scope_rows_readable(
        &self,
        caller: &AuthUser,
        rows: &[McpDataScope],
    ) -> Result<()> {
        for row in rows {
            match row.scope_type {
                McpDataScopeType::DataSource => {
                    self.data_sources
                        .authorize(caller, &row.reference_id, Permission::View)
                        .await?;
                }
                McpDataScopeType::Subject => {
                    self.subjects
                        .authorize(caller, &row.reference_id, Permission::View)
                        .await?;
                }
            }
        }
        Ok(())
    }
}

fn of(item: &DataScopeItem, service_id: &str) -> McpDataScope {
    McpDataScope {
        id: String::new(),
        service_id: service_id.to_string(),
        scope_type: item.scope_type.unwrap_or(McpDataScopeType::DataSource),
        reference_id: item.reference_id.clone().unwrap_or_default(),
        audit: Audit::default(),
    }
}

fn referenced(rows: &[McpDataScope], scope_type: McpDataScopeType) -> Vec<String> {
    rows.iter()
        .filter(|row| row.scope_type == scope_type)
        .map(|row| row.reference_id.clone())
        .collect()
}

/// A scope reference keeps its id as the label when the row behind it is gone.
fn label(
    row: &McpDataScope,
    source_names: &HashMap<String, String>,
    subject_names: &HashMap<String, String>,
) -> String {
    let names = match row.scope_type {
        McpDataScopeType::DataSource => source_names,
        McpDataScopeType::Subject => subject_names,
    };
    names
        .get(&row.reference_id)
        .cloned()
        .unwrap_or_else(|| row.reference_id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::datasource::{DataSource, DbType};

    async fn store() -> (Db, DataSourceService, SubjectService, McpServiceService) {
        let db = Db::sqlite_temp().await;
        sqlx::query(
            "INSERT INTO users (id, name, password) \
                     VALUES ('u-1', 'alice', 'x'), ('u-2', 'bob', 'x'), ('u-3', 'admin', 'x')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        let config = Config::from_source(|_| None);
        let permissions = PermissionService::new(&config);
        let data_sources = DataSourceService::new(db.clone(), &config, permissions.clone());
        let subjects = SubjectService::new(db.clone(), data_sources.clone());
        let services =
            McpServiceService::new(db.clone(), data_sources.clone(), subjects.clone(), &config);
        (db, data_sources, subjects, services)
    }

    fn caller(id: &str, name: &str) -> AuthUser {
        AuthUser {
            id: id.to_string(),
            name: name.to_string(),
            display_name: None,
        }
    }

    /// A data source owned by `owner`, written through the repo so the ACL path stays real.
    async fn source(db: &Db, id: &str, owner: &str, name: &str) -> String {
        let row = DataSource {
            id: id.to_string(),
            name: Some(name.to_string()),
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
        row.id
    }

    async fn subject_of(db: &Db, id: &str, owner: &str, source_id: &str, name: &str) -> String {
        let row = crate::model::semantic::Subject {
            id: id.to_string(),
            name: Some(name.to_string()),
            description: None,
            deleted: false,
            datasource_id: source_id.to_string(),
            aliases: vec![],
            audit: Audit::new(owner),
        };
        subject::insert(db, &row).await.unwrap();
        row.id
    }

    fn body(code: &str, items: &[(&McpDataScopeType, &str)]) -> McpServiceCreateRequest {
        McpServiceCreateRequest {
            code: Some(code.to_string()),
            name: Some("销售服务".into()),
            description: Some("desc".into()),
            data_scopes: items
                .iter()
                .map(|(scope_type, reference_id)| DataScopeItem {
                    id: None,
                    scope_type: Some(**scope_type),
                    reference_id: Some(reference_id.to_string()),
                    reference_name: None,
                })
                .collect(),
        }
    }

    fn page_req() -> PageReq {
        PageReq { page: 1, size: 10 }
    }

    #[tokio::test]
    async fn a_service_starts_as_a_draft_and_keeps_its_scope() {
        let (db, _, _, services) = store().await;
        let alice = caller("u-1", "alice");
        let warehouse = source(&db, "d-1", "u-1", "warehouse").await;

        let id = services
            .create(
                &alice,
                &body("sales", &[(&McpDataScopeType::DataSource, "d-1")]),
            )
            .await
            .unwrap();
        let row = services.load(&id).await.unwrap();
        assert_eq!(row.code, "sales");
        assert_eq!(row.status, Some(McpServiceStatus::Draft));
        assert_eq!(row.active_version_number, None);
        assert_eq!(row.audit.created_by.as_deref(), Some("u-1"));

        let scope = services.read_data_scope(&id).await.unwrap();
        assert_eq!(scope.items.len(), 1);
        assert_eq!(scope.items[0].reference_name.as_deref(), Some("warehouse"));
        assert_eq!(scope.resolved_data_sources.len(), 1);
        assert_eq!(scope.resolved_data_sources[0].id, warehouse);
    }

    #[tokio::test]
    async fn a_code_is_validated_before_anything_is_stored() {
        let (db, _, _, services) = store().await;
        let alice = caller("u-1", "alice");
        source(&db, "d-1", "u-1", "warehouse").await;
        let good = body("sales", &[(&McpDataScopeType::DataSource, "d-1")]);

        let mut invalid_code = good.clone();
        invalid_code.code = Some("Sales!".into());
        let err = services.create(&alice, &invalid_code).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::MsServiceCodeInvalid);

        services.create(&alice, &good).await.unwrap();
        let err = services.create(&alice, &good).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::MsServiceCodeExists);
        assert_eq!(err.message(), "Service code already exists: sales");

        let mut empty = good.clone();
        empty.data_scopes.clear();
        // `@NotEmpty dataScopes` answers first over REST; `MS018` is the same guard the Java
        // service kept for callers that bypassed the request DTO.
        let err = services.create(&alice, &empty).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidParameter);
        assert_eq!(
            err.message(),
            "Request parameter error: dataScopes: must not be empty"
        );

        // Nothing half-written survived any of the rejections.
        assert_eq!(
            services
                .list(&alice, None, None, page_req())
                .await
                .unwrap()
                .total,
            1
        );
    }

    #[tokio::test]
    async fn a_scope_must_point_at_metadata_the_caller_can_see() {
        let (db, _, _, services) = store().await;
        let alice = caller("u-1", "alice");
        let bob = caller("u-2", "bob");
        source(&db, "d-1", "u-2", "theirs").await;
        subject_of(&db, "s-1", "u-2", "d-1", "交易域").await;

        let err = services
            .create(
                &alice,
                &body("sales", &[(&McpDataScopeType::DataSource, "gone")]),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::DsNotFound);
        assert_eq!(err.message(), "Data source not found: gone");

        let err = services
            .create(
                &alice,
                &body("sales", &[(&McpDataScopeType::DataSource, "d-1")]),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::PermissionDenied);

        let err = services
            .create(
                &alice,
                &body("sales", &[(&McpDataScopeType::Subject, "s-1")]),
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.code(),
            ErrorCode::PermissionDenied,
            "bob's subject is not alice's to borrow"
        );

        let id = services
            .create(&bob, &body("sales", &[(&McpDataScopeType::Subject, "s-1")]))
            .await
            .unwrap();
        // The subject contributes the source it belongs to.
        let scope = services.read_data_scope(&id).await.unwrap();
        assert_eq!(scope.items[0].reference_name.as_deref(), Some("交易域"));
        assert_eq!(scope.resolved_data_sources[0].id, "d-1");
    }

    #[tokio::test]
    async fn a_scope_put_replaces_the_whole_list_or_nothing() {
        let (db, _, _, services) = store().await;
        let alice = caller("u-1", "alice");
        source(&db, "d-1", "u-1", "warehouse").await;
        source(&db, "d-2", "u-1", "crm").await;
        let id = services
            .create(
                &alice,
                &body("sales", &[(&McpDataScopeType::DataSource, "d-1")]),
            )
            .await
            .unwrap();

        let request = DataScopeRequest {
            items: vec![
                DataScopeItem {
                    id: None,
                    scope_type: Some(McpDataScopeType::DataSource),
                    reference_id: Some("d-2".into()),
                    reference_name: None,
                },
                DataScopeItem {
                    id: None,
                    scope_type: Some(McpDataScopeType::DataSource),
                    reference_id: Some("d-1".into()),
                    reference_name: None,
                },
            ],
        };
        services
            .save_data_scope(&alice, &id, &request)
            .await
            .unwrap();
        let scope = services.data_scope(&alice, &id).await.unwrap();
        let resolved: Vec<String> = scope
            .resolved_data_sources
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert_eq!(resolved, vec!["d-2".to_string(), "d-1".to_string()]);
        assert!(scope.items.iter().all(|item| item.id.is_some()));

        // An item that does not read leaves the stored scope untouched.
        let broken: DataScopeRequest =
            serde_json::from_value(serde_json::json!({ "items": [{ "scope_type": "SUBJECT" }] }))
                .unwrap();
        let err = services
            .save_data_scope(&alice, &id, &broken)
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidParameter);
        assert_eq!(
            err.message(),
            "Request parameter error: items[0].referenceId: must not be null"
        );
        assert_eq!(services.scope_rows(&id).await.unwrap().len(), 2);

        // Clearing is legal, and resolves to nothing.
        services
            .save_data_scope(&alice, &id, &DataScopeRequest::default())
            .await
            .unwrap();
        assert!(services.scope_rows(&id).await.unwrap().is_empty());
        assert!(
            services
                .resolved_data_source_ids(&id)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn the_view_reports_the_endpoint_and_the_tool_slots() {
        let (db, _, _, services) = store().await;
        let alice = caller("u-1", "alice");
        source(&db, "d-1", "u-1", "warehouse").await;
        let id = services
            .create(
                &alice,
                &body("sales", &[(&McpDataScopeType::DataSource, "d-1")]),
            )
            .await
            .unwrap();

        let view = services.get(&alice, &id).await.unwrap();
        assert_eq!(
            view.endpoint_path, "/sales/mcp",
            "no base url is configured"
        );
        assert_eq!(
            view.tool_count, 7,
            "the prebuilt slots count even with no rows"
        );
        assert_eq!(view.created_user_name.as_deref(), Some("alice"));
        assert_eq!(view.status, Some(McpServiceStatus::Draft));
        assert_eq!(view.active_version_number, None);

        let page = services.list(&alice, None, None, page_req()).await.unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.data[0].id, id);

        // Only the profile moves; the code and the status stay where they are.
        services
            .update(
                &alice,
                &id,
                &McpServiceUpdateRequest {
                    name: Some("新名字".into()),
                    description: None,
                    updated_by: None,
                },
            )
            .await
            .unwrap();
        let row = services.load(&id).await.unwrap();
        assert_eq!(row.name.as_deref(), Some("新名字"));
        assert_eq!(row.description.as_deref(), Some("desc"));
        assert_eq!(row.code, "sales");
        assert_eq!(row.status, Some(McpServiceStatus::Draft));

        // A stranger sees neither the list nor the detail.
        let bob = caller("u-2", "bob");
        assert_eq!(
            services.get(&bob, &id).await.unwrap_err().code(),
            ErrorCode::PermissionDenied
        );
        assert_eq!(
            services
                .list(&bob, None, None, page_req())
                .await
                .unwrap()
                .total,
            0
        );
        assert_eq!(
            services.load(&id).await.unwrap().code,
            "sales",
            "bob still cannot reach it, but the row is there"
        );
        let err = services
            .update(&bob, &id, &McpServiceUpdateRequest::default())
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::PermissionDenied);

        services.remove(&alice, &id).await.unwrap();
        assert_eq!(
            services.load(&id).await.unwrap_err().code(),
            ErrorCode::MsServiceNotFound
        );
    }

    #[tokio::test]
    async fn an_admin_sees_every_service_and_a_keyword_reaches_the_code() {
        let (db, _, _, services) = store().await;
        let alice = caller("u-1", "alice");
        // "admin" is the name the default configuration treats as an administrator.
        let root = caller("u-3", "admin");
        source(&db, "d-1", "u-1", "warehouse").await;
        let id = services
            .create(
                &alice,
                &body("sales-mcp", &[(&McpDataScopeType::DataSource, "d-1")]),
            )
            .await
            .unwrap();

        let page = services
            .list(&root, Some("SALES-MCP"), None, page_req())
            .await
            .unwrap();
        assert_eq!(page.total, 1, "a code keyword finds another user's service");
        assert_eq!(page.data[0].id, id);
        assert_eq!(page.data[0].created_user_name.as_deref(), Some("alice"));

        // An admin may edit and delete what it does not own.
        services
            .update(
                &root,
                &id,
                &McpServiceUpdateRequest {
                    name: None,
                    description: Some("by admin".into()),
                    updated_by: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            services.load(&id).await.unwrap().description.as_deref(),
            Some("by admin")
        );
        assert_eq!(
            services
                .list(&alice, Some("nothing"), None, page_req())
                .await
                .unwrap()
                .total_pages,
            0
        );
    }

    #[tokio::test]
    async fn a_deleted_service_leaves_no_scope_behind() {
        let (db, _, _, services) = store().await;
        let alice = caller("u-1", "alice");
        source(&db, "d-1", "u-1", "warehouse").await;
        let id = services
            .create(
                &alice,
                &body("sales", &[(&McpDataScopeType::DataSource, "d-1")]),
            )
            .await
            .unwrap();
        assert_eq!(services.scope_rows(&id).await.unwrap().len(), 1);

        services.remove(&alice, &id).await.unwrap();
        assert!(services.scope_rows(&id).await.unwrap().is_empty());
        // The freed code is available again for a new service.
        services
            .create(
                &alice,
                &body("sales", &[(&McpDataScopeType::DataSource, "d-1")]),
            )
            .await
            .unwrap();
        let err = services.load_by_code("nope").await.unwrap_err();
        assert_eq!(err.message(), "MCP service not found: nope");
    }

    #[tokio::test]
    async fn subject_rows_that_are_gone_resolve_to_nothing() {
        let (db, _, _, services) = store().await;
        let alice = caller("u-1", "alice");
        let id = services
            .create(
                &alice,
                &McpServiceCreateRequest {
                    code: Some("sales".into()),
                    name: Some("销售".into()),
                    description: None,
                    data_scopes: vec![DataScopeItem {
                        id: None,
                        scope_type: Some(McpDataScopeType::Subject),
                        // Written straight in: the REST path would have rejected this reference.
                        reference_id: Some("gone".into()),
                        reference_name: None,
                    }],
                },
            )
            .await
            .ok();
        // Creating needs VIEW on the subject, so the row cannot exist through the service.
        assert!(id.is_none());

        let scope = DataScopeItem {
            id: None,
            scope_type: Some(McpDataScopeType::Subject),
            reference_id: Some("gone".into()),
            reference_name: None,
        };
        mcp_service::insert_scope(&db, "svc-1", &of(&scope, "svc-1"), &Audit::new("u-1"))
            .await
            .unwrap();
        assert!(
            services
                .resolved_data_source_ids("svc-1")
                .await
                .unwrap()
                .is_empty()
        );
        let labelled = services.read_data_scope("svc-1").await.unwrap();
        assert_eq!(
            labelled.items[0].reference_name.as_deref(),
            Some("gone"),
            "a missing subject is labelled with its id"
        );
    }
}
