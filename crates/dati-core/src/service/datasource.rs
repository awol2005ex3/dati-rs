//! Data source CRUD, the connection probe, and the live-metadata reads the table and column
//! services borrow. This is the Java `DataSourceService` plus its `JdbcMetaService`.

use std::collections::HashMap;

use crate::auth::AuthUser;
use crate::config::Config;
use crate::connector::{Connectors, probe_spec, spec_of};
use crate::db::Db;
use crate::error::{DatiError, ErrorCode, Result};
use crate::model::base::{Audit, PageReq, PageResponse};
use crate::model::datasource::{
    ConnectionSpec, DataSource, DataSourceInput, DataSourceView, LiveColumn, LiveTable,
    UpdateDataSourceInput,
};
use crate::model::permission::{Permission, ResourceType};
use crate::repo::resource::Visibility;
use crate::repo::{column, column_value, datasource, table as table_repo, user};
use crate::time::new_id;

use super::permission::PermissionService;

#[derive(Clone)]
pub struct DataSourceService {
    db: Db,
    config: Config,
    permissions: PermissionService,
    connectors: Connectors,
}

impl DataSourceService {
    pub fn new(db: Db, config: &Config, permissions: PermissionService) -> Self {
        Self {
            db,
            config: config.clone(),
            permissions,
            connectors: Connectors::new(),
        }
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn permissions(&self) -> &PermissionService {
        &self.permissions
    }

    /// The pool cache, for the services that report live-metadata failures under their own code
    /// instead of the `DS003` the `live_*` helpers below use.
    pub fn connectors(&self) -> &Connectors {
        &self.connectors
    }

    /// The stored row, or `DS002`: the Java services resolved the source before checking rights.
    pub async fn load(&self, id: &str) -> Result<DataSource> {
        datasource::find(&self.db, id)
            .await?
            .ok_or_else(|| DatiError::new(ErrorCode::DsNotFound, [id]))
    }

    /// `PermissionService.requireDataSource(id, permission)`: missing first, then denial.
    pub async fn authorize(
        &self,
        caller: &AuthUser,
        id: &str,
        permission: Permission,
    ) -> Result<DataSource> {
        let source = self.load(id).await?;
        self.permissions
            .require(&self.db, caller, ResourceType::DataSource, id, permission)
            .await?;
        Ok(source)
    }

    /// The credentials a stored row dials. Blank URLs are rejected by validation on the create
    /// path, so the fallback below only keeps a legacy row from panicking the reader.
    pub fn connection_of(source: &DataSource) -> ConnectionSpec {
        spec_of(
            source.db_type,
            source.connection_url.as_deref(),
            source.user_name.as_deref(),
            source.password.as_deref(),
        )
        .unwrap_or_else(|| ConnectionSpec {
            db_type: source.db_type,
            url: source.connection_url.clone().unwrap_or_default(),
            user: source.user_name.clone(),
            password: source.password.clone(),
        })
    }

    /// Forgets the cached pool of a source whose credentials changed or that was deleted, which is
    /// what the Java `HikariPoolManager.close` did.
    pub async fn forget(&self, source: &DataSource) {
        self.connectors.evict(&Self::connection_of(source)).await;
    }

    /// `POST /v1/data-sources/test-connection`: `true` when the database answers. Java returned a
    /// bare `false` for a failed probe, which left the console with nothing to show but "test
    /// failed", so the driver's own reason now travels back as `DS001`/`DS003`. A missing `type` is
    /// still inferred from the URL, as the Java endpoint let the driver do.
    pub async fn test_connection(&self, input: &DataSourceInput) -> Result<bool> {
        let spec = input.spec().or_else(|| {
            probe_spec(
                None,
                input.connection_url.as_deref(),
                input.username.as_deref(),
                input.password.as_deref(),
            )
        });
        let Some(spec) = spec else {
            return Err(DatiError::new(
                ErrorCode::DsConnectionFailed,
                ["no connection url to probe"],
            ));
        };
        self.connectors
            .test_connection(&spec)
            .await
            .map_err(|err| err.into_error("test-connection"))?;
        Ok(true)
    }

    // ---- live metadata reads -------------------------------------------------------------
    //
    // The Java controllers wrapped every `SQLException` these calls raise in `DS003`, failed
    // connection included, so the connector's own connection/query split is flattened here.

    pub async fn live_schemas(&self, source: &DataSource) -> Result<Vec<String>> {
        self.connectors
            .list_schemas(&Self::connection_of(source))
            .await
            .map_err(|err| err.into_sql_error("schemas"))
    }

    pub async fn live_tables(&self, source: &DataSource, schema: &str) -> Result<Vec<LiveTable>> {
        self.connectors
            .list_tables(&Self::connection_of(source), schema)
            .await
            .map_err(|err| err.into_sql_error("tables"))
    }

    pub async fn live_columns(
        &self,
        source: &DataSource,
        schema: &str,
        table: &str,
    ) -> Result<Vec<LiveColumn>> {
        self.connectors
            .list_columns(&Self::connection_of(source), schema, table)
            .await
            .map_err(|err| err.into_sql_error("columns"))
    }

    /// The distinct values one column holds, capped and truncated by the sampling configuration.
    pub async fn sample_values(
        &self,
        source: &DataSource,
        schema: Option<&str>,
        table: &str,
        column: &str,
    ) -> Result<Vec<String>> {
        let values = self
            .connectors
            .distinct_values(
                &Self::connection_of(source),
                schema,
                table,
                column,
                self.config.column_value_sample_limit,
            )
            .await
            .map_err(|err| err.into_sql_error("column-values"))?;
        let max = self.config.column_value_length_limit.max(0) as usize;
        Ok(values
            .into_iter()
            .map(|value| value.chars().take(max).collect())
            .collect())
    }

    // ---- REST-facing operations ----------------------------------------------------------

    /// `POST /v1/data-sources`: the client's `default_schema` is ignored because the connector's
    /// own answer is stored, and a failed probe leaves nothing behind.
    pub async fn add(&self, caller: &AuthUser, input: &DataSourceInput) -> Result<String> {
        input.validate()?;
        let spec = input.spec().ok_or_else(|| {
            DatiError::validation(&[
                ("type", "must not be null"),
                ("connectionUrl", "must not be blank"),
            ])
        })?;
        let detected = self.connectors.detect_schema(&spec).await?;
        let source = DataSource {
            id: new_id(),
            name: input.name.clone(),
            description: input.description.clone(),
            deleted: false,
            connection_url: input.connection_url.clone(),
            db_type: spec.db_type,
            user_name: input.username.clone(),
            password: input.password.clone(),
            default_schema: Some(detected),
            audit: Audit::new(&caller.id),
        };
        datasource::insert(&self.db, &source).await?;
        Ok(source.id)
    }

    /// `PUT /v1/data-sources/{id}`: null fields keep the stored value, and the schema is re-detected
    /// only when the connection details actually changed — both behaviours of the Java service.
    pub async fn update(
        &self,
        caller: &AuthUser,
        id: &str,
        input: &UpdateDataSourceInput,
    ) -> Result<()> {
        let current = self.authorize(caller, id, Permission::Edit).await?;
        let next_url = input
            .connection_url
            .clone()
            .or_else(|| current.connection_url.clone());
        let next_user = input.username.clone().or_else(|| current.user_name.clone());
        let next_password = input.password.clone().or_else(|| current.password.clone());
        let next_type = input.db_type.unwrap_or(current.db_type);

        let connection_changed = next_url != current.connection_url
            || next_user != current.user_name
            || next_password != current.password
            || next_type != current.db_type;

        let detected = if connection_changed {
            // A failed probe must not touch the stored row, so it runs before any write.
            let candidate = ConnectionSpec {
                db_type: next_type,
                url: next_url.clone().unwrap_or_default(),
                user: next_user,
                password: next_password,
            };
            Some(self.connectors.detect_schema(&candidate).await?)
        } else {
            None
        };

        let mut updated = current.clone();
        if let Some(name) = &input.name {
            updated.name = Some(name.clone());
        }
        if let Some(description) = &input.description {
            updated.description = Some(description.clone());
        }
        if let Some(connection_url) = &input.connection_url {
            updated.connection_url = Some(connection_url.clone());
        }
        if let Some(db_type) = input.db_type {
            updated.db_type = db_type;
        }
        if let Some(user_name) = &input.username {
            updated.user_name = Some(user_name.clone());
        }
        if let Some(password) = &input.password {
            updated.password = Some(password.clone());
        }
        if let Some(schema) = detected {
            updated.default_schema = Some(schema);
        }
        updated.audit.touch(&caller.id);
        datasource::update(&self.db, &updated).await?;

        if connection_changed {
            self.forget(&current).await;
        }
        Ok(())
    }

    /// `DELETE /v1/data-sources/{id}`: a hard delete that takes its tables, their columns and the
    /// value dictionaries with it, which is what the Java cascade did.
    pub async fn remove(&self, caller: &AuthUser, id: &str) -> Result<()> {
        let source = self.authorize(caller, id, Permission::Edit).await?;
        self.forget(&source).await;
        let tables = table_repo::list_by_datasource(&self.db, id).await?;
        let ids: Vec<String> = tables.iter().map(|row| row.id.clone()).collect();
        for table_id in &ids {
            column_value::remove_by_table(&self.db, table_id).await?;
        }
        column::remove_by_table_ids(&self.db, &ids).await?;
        table_repo::remove_by_ids(&self.db, &ids).await?;
        datasource::remove(&self.db, id).await?;
        Ok(())
    }

    /// `GET /v1/data-sources`: admins see everything, everyone else their own and granted rows.
    pub async fn list(
        &self,
        caller: &AuthUser,
        keyword: Option<&str>,
        page: PageReq,
    ) -> Result<PageResponse<DataSourceView>> {
        let visibility = if self.permissions.is_admin(caller) {
            Visibility::All
        } else {
            Visibility::User(&caller.id)
        };
        let (rows, total) = datasource::page(&self.db, keyword, visibility, page).await?;
        let names = author_names(&self.db, rows.iter().map(|row| &row.audit)).await?;
        Ok(PageResponse::new(
            rows.iter()
                .map(|row| DataSourceView::new(row, &names))
                .collect(),
            total,
            page.page,
            page.size,
        ))
    }

    /// `GET /v1/data-sources/{id}`.
    pub async fn get(&self, caller: &AuthUser, id: &str) -> Result<DataSourceView> {
        let source = self.authorize(caller, id, Permission::View).await?;
        let names = author_names(&self.db, [&source.audit]).await?;
        Ok(DataSourceView::new(&source, &names))
    }

    /// `GET /v1/data-sources/{id}/schemas`.
    pub async fn schemas(&self, caller: &AuthUser, id: &str) -> Result<Vec<String>> {
        let source = self.authorize(caller, id, Permission::View).await?;
        self.live_schemas(&source).await
    }

    /// `GET /v1/data-sources/{id}/schemas/{schema}/tables`.
    pub async fn tables(
        &self,
        caller: &AuthUser,
        id: &str,
        schema: &str,
    ) -> Result<Vec<LiveTable>> {
        let source = self.authorize(caller, id, Permission::View).await?;
        self.live_tables(&source, schema).await
    }
}

/// `BaseAssembler.fillUserInfo`: one query turns the author ids of a page into display names.
/// Unknown and absent ids stay out of the map, leaving the `*_user_name` fields null.
pub async fn author_names<'a>(
    db: &Db,
    audits: impl IntoIterator<Item = &'a Audit>,
) -> Result<HashMap<String, String>> {
    let ids: Vec<String> = audits
        .into_iter()
        .flat_map(|audit| audit.authors().map(str::to_string))
        .collect();
    user::labels(db, &ids).await
}
