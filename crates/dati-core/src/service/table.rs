//! Tables registered from a data source: the Java `TableService`.

use crate::auth::AuthUser;
use crate::db::Db;
use crate::error::{DatiError, ErrorCode, Result};
use crate::model::base::{Audit, PageReq, PageResponse};
use crate::model::datasource::{
    AddTableRequest, ColumnInfo, DataSource, LiveTable, TableInfo, TableInfoView,
    TableUpdateRequest,
};
use crate::model::permission::Permission;
use crate::repo::{column, column_value, table};
use crate::time::new_id;

use super::datasource::{DataSourceService, author_names};

#[derive(Clone)]
pub struct TableService {
    db: Db,
    data_sources: DataSourceService,
}

impl TableService {
    pub fn new(db: Db, data_sources: DataSourceService) -> Self {
        Self { db, data_sources }
    }

    /// The stored row, or `DS007`. Tables are addressed by id everywhere, so the data source in
    /// the path never has to match — the Java service looked the row up by id alone.
    pub async fn load(&self, table_id: &str) -> Result<TableInfo> {
        table::find(&self.db, table_id)
            .await?
            .ok_or_else(|| DatiError::new(ErrorCode::DsTableNotFound, [table_id]))
    }

    /// The table plus its owning source, already authorised for `permission`.
    pub async fn authorized(
        &self,
        caller: &AuthUser,
        table_id: &str,
        permission: Permission,
    ) -> Result<(TableInfo, DataSource)> {
        let row = self.load(table_id).await?;
        let source = self
            .data_sources
            .authorize(caller, &row.data_source_id, permission)
            .await?;
        Ok((row, source))
    }

    /// `GET /v1/data-sources/{id}/tables`.
    pub async fn list(
        &self,
        caller: &AuthUser,
        data_source_id: &str,
        keyword: Option<&str>,
        page: PageReq,
    ) -> Result<PageResponse<TableInfoView>> {
        self.data_sources
            .authorize(caller, data_source_id, Permission::View)
            .await?;
        let (rows, total) = table::page(&self.db, data_source_id, keyword, page).await?;
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

    /// `GET /v1/data-sources/{id}/tables/added-names`: the names the picker greys out.
    pub async fn added_names(
        &self,
        caller: &AuthUser,
        data_source_id: &str,
    ) -> Result<Vec<String>> {
        self.data_sources
            .authorize(caller, data_source_id, Permission::View)
            .await?;
        let rows = table::list_by_datasource(&self.db, data_source_id).await?;
        Ok(rows.iter().filter_map(|row| row.name.clone()).collect())
    }

    /// `POST /v1/data-sources/{id}/tables/batch`: register live tables together with their
    /// columns. A name the database does not have is `DS007`; a connection failure while reading
    /// live metadata is `DS004`, which is what the Java import threw.
    pub async fn batch_add(
        &self,
        caller: &AuthUser,
        data_source_id: &str,
        requests: &[AddTableRequest],
    ) -> Result<Vec<String>> {
        let mut violations = Vec::new();
        for (index, request) in requests.iter().enumerate() {
            for (field, reason) in request.violations() {
                violations.push(format!("tables[{index}].{field}: {reason}"));
            }
        }
        if !violations.is_empty() {
            return Err(DatiError::new(
                ErrorCode::InvalidParameter,
                [violations.join("; ")],
            ));
        }
        let source = self
            .data_sources
            .authorize(caller, data_source_id, Permission::Edit)
            .await?;
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        let connection = DataSourceService::connection_of(&source);
        let connectors = self.data_sources.connectors();

        // One live listing per schema: two same-named tables in different schemas must not be
        // confused for one another, which is why the check is per (schema, name) pair.
        let mut listed: Vec<(String, Vec<LiveTable>)> = Vec::new();
        for request in requests {
            let schema = request.schema.clone().unwrap_or_default();
            if listed.iter().any(|(known, _)| *known == schema) {
                continue;
            }
            let live = connectors
                .list_tables(&connection, &schema)
                .await
                .map_err(|err| err.into_sync_error("batch-add-tables"))?;
            listed.push((schema, live));
        }

        let mut ids = Vec::new();
        for request in requests {
            let schema = request.schema.clone().unwrap_or_default();
            let name = request.name.clone().unwrap_or_default();
            let live = listed
                .iter()
                .find(|(listed_schema, _)| *listed_schema == schema)
                .map(|(_, tables)| tables.as_slice())
                .unwrap_or_default();
            let Some(live_table) = live.iter().find(|row| row.name == name) else {
                return Err(DatiError::new(ErrorCode::DsTableNotFound, [name.as_str()]));
            };
            let description = live_table.comment.clone();

            let row = TableInfo {
                id: new_id(),
                name: Some(name.clone()),
                description,
                deleted: false,
                data_source_id: data_source_id.to_string(),
                schema: Some(schema.clone()),
                aliases: Vec::new(),
                audit: Audit::new(&caller.id),
            };
            let table_id = table::insert(&self.db, &row).await?;
            ids.push(table_id.clone());

            let live_columns = connectors
                .list_columns(&connection, &schema, &name)
                .await
                .map_err(|err| {
                    DatiError::new(
                        ErrorCode::DsSyncFailed,
                        [format!("{name}: {}", err.reason())],
                    )
                })?;
            for column in live_columns {
                column::insert(
                    &self.db,
                    &ColumnInfo {
                        id: new_id(),
                        name: Some(column.name),
                        description: column.comment.filter(|value| !value.is_empty()),
                        deleted: false,
                        table_id: table_id.clone(),
                        column_type: Some(column.type_name),
                        aliases: Vec::new(),
                        extract_value_enabled: false,
                        audit: Audit::new(&caller.id),
                    },
                )
                .await?;
            }
        }
        Ok(ids)
    }

    /// `DELETE /v1/data-sources/{id}/tables/{tableId}`: the owning source authorises the delete,
    /// and its columns and value dictionaries go with it.
    pub async fn remove(&self, caller: &AuthUser, table_id: &str) -> Result<()> {
        let row = self.load(table_id).await?;
        self.data_sources
            .authorize(caller, &row.data_source_id, Permission::Edit)
            .await?;
        column_value::remove_by_table(&self.db, table_id).await?;
        column::remove_by_table(&self.db, table_id).await?;
        table::remove(&self.db, table_id).await?;
        Ok(())
    }

    /// `PUT /v1/data-sources/{id}/tables/{tableId}`: description and aliases are written
    /// unconditionally, so an omitted field clears the stored value exactly like the Java
    /// assembler did. `updated_by` stays as it was; only the timestamp moves.
    pub async fn update(
        &self,
        caller: &AuthUser,
        table_id: &str,
        request: &TableUpdateRequest,
    ) -> Result<()> {
        let row = self.load(table_id).await?;
        self.data_sources
            .authorize(caller, &row.data_source_id, Permission::Edit)
            .await?;
        let mut updated = row;
        updated.description = request.description.clone();
        updated.aliases = request.aliases.clone().unwrap_or_default();
        updated.audit.updated_at = Some(crate::time::now_iso());
        table::update(&self.db, &updated).await?;
        Ok(())
    }
}
