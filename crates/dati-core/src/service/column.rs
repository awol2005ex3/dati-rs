//! Columns of a registered table and their value dictionary: the Java `ColumnService` and
//! `ColumnValueService`.

use crate::auth::AuthUser;
use crate::db::Db;
use crate::error::{DatiError, ErrorCode, Result};
use crate::model::base::{Audit, PageReq, PageResponse};
use crate::model::datasource::{
    ColumnInfo, ColumnInfoView, ColumnUpdateRequest, ColumnValue, ColumnValueListRequest,
    ColumnValueView, TableInfo,
};
use crate::model::permission::Permission;
use crate::repo::{column, column_value};
use crate::time::new_id;

use super::datasource::{DataSourceService, author_names};
use super::table::TableService;

#[derive(Clone)]
pub struct ColumnService {
    db: Db,
    data_sources: DataSourceService,
    tables: TableService,
}

impl ColumnService {
    pub fn new(db: Db, data_sources: DataSourceService, tables: TableService) -> Self {
        Self {
            db,
            data_sources,
            tables,
        }
    }

    /// `GET .../tables/{tableId}/columns`: the table is resolved first, so a stale id answers
    /// `DS007` rather than a permission denial.
    pub async fn list(
        &self,
        caller: &AuthUser,
        table_id: &str,
        keyword: Option<&str>,
        page: PageReq,
    ) -> Result<PageResponse<ColumnInfoView>> {
        let row = self.tables.load(table_id).await?;
        self.data_sources
            .authorize(caller, &row.data_source_id, Permission::View)
            .await?;
        let (rows, total) = column::page(&self.db, table_id, keyword, page).await?;
        let names = author_names(&self.db, rows.iter().map(|row| &row.audit)).await?;
        Ok(PageResponse::new(
            rows.iter()
                .map(|row| ColumnInfoView::new(row, &names))
                .collect(),
            total,
            page.page,
            page.size,
        ))
    }

    /// `PUT .../columns/{id}`: a null field keeps the stored value. Turning value extraction off
    /// drops the column's dictionary, which is what the Java service did.
    pub async fn update(
        &self,
        caller: &AuthUser,
        column_id: &str,
        request: &ColumnUpdateRequest,
    ) -> Result<()> {
        let row = self.load(column_id).await?;
        let table = self.owner_of(&row).await?;
        self.data_sources
            .authorize(caller, &table.data_source_id, Permission::Edit)
            .await?;

        let was_enabled = row.extract_value_enabled;
        let mut updated = row.clone();
        if let Some(name) = &request.name {
            updated.name = Some(name.clone());
        }
        if let Some(description) = &request.description {
            updated.description = Some(description.clone());
        }
        if let Some(column_type) = &request.column_type {
            updated.column_type = Some(column_type.clone());
        }
        if let Some(aliases) = &request.aliases {
            updated.aliases = aliases.clone();
        }
        if let Some(enabled) = request.extract_value_enabled {
            updated.extract_value_enabled = enabled;
        }
        // Hibernate refreshed `updated_at` on save; the Java code never reassigned `updated_by`.
        updated.audit.updated_at = Some(crate::time::now_iso());
        column::update(&self.db, &updated).await?;

        if was_enabled && !updated.extract_value_enabled {
            column_value::remove_by_column(&self.db, &row.table_id, col_name(&row)).await?;
        }
        Ok(())
    }

    /// `POST .../columns/sync`: adopt the live column list. Columns that are still there keep their
    /// id, aliases and extraction flag; only the columns that vanished are deleted, together with
    /// their dictionaries.
    pub async fn sync(
        &self,
        caller: &AuthUser,
        data_source_id: &str,
        table_id: &str,
        overwrite_existing: bool,
    ) -> Result<()> {
        // The Java service checked the rights on the data source named in the path before it even
        // looked the table up, so a mismatched path id is not rejected here.
        let source = self
            .data_sources
            .authorize(caller, data_source_id, Permission::Edit)
            .await?;
        let row = self.tables.load(table_id).await?;
        let existing = column::list_by_table(&self.db, table_id).await?;
        let live = self
            .data_sources
            .live_columns(&source, schema_of(&row), name_of(&row))
            .await?;

        let live_names: Vec<&str> = live.iter().map(|column| column.name.as_str()).collect();
        for stale in existing
            .iter()
            .filter(|col| !live_names.contains(&col_name(col)))
        {
            column::remove_by_ids(&self.db, std::slice::from_ref(&stale.id)).await?;
            column_value::remove_by_column(&self.db, table_id, col_name(stale)).await?;
        }

        for column in live {
            match existing
                .iter()
                .find(|stored| col_name(stored) == column.name)
            {
                Some(stored) => {
                    let mut kept = stored.clone();
                    kept.column_type = Some(column.type_name);
                    if overwrite_existing
                        && let Some(comment) = column.comment.filter(|value| !value.is_empty())
                    {
                        kept.description = Some(comment);
                    }
                    kept.audit.updated_at = Some(crate::time::now_iso());
                    column::update(&self.db, &kept).await?;
                }
                None => {
                    column::insert(
                        &self.db,
                        &ColumnInfo {
                            id: new_id(),
                            name: Some(column.name),
                            description: column.comment.filter(|value| !value.is_empty()),
                            deleted: false,
                            table_id: table_id.to_string(),
                            column_type: Some(column.type_name),
                            aliases: Vec::new(),
                            extract_value_enabled: false,
                            audit: Audit::new(&caller.id),
                        },
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }

    // ---- the value dictionary ------------------------------------------------------------

    /// `POST .../columns/{columnId}/values/extract`: sample the live column into the dictionary.
    pub async fn extract(
        &self,
        caller: &AuthUser,
        data_source_id: &str,
        column_id: &str,
        overwrite: bool,
    ) -> Result<()> {
        // The Java extract reported a missing column as a plain `NOT_FOUND`, whose template carries
        // no placeholder, so the "Column not found" detail was dropped from the message.
        let row = column::find(&self.db, column_id).await?.ok_or_else(|| {
            DatiError::new(
                ErrorCode::NotFound,
                [format!("Column not found: {column_id}")],
            )
        })?;
        let table = self.owner_of(&row).await?;
        if !data_source_id.is_empty() && data_source_id != table.data_source_id {
            return Err(DatiError::new(
                ErrorCode::InvalidParameter,
                [format!(
                    "Column does not belong to data source: {data_source_id}"
                )],
            ));
        }
        let source = self
            .data_sources
            .authorize(caller, &table.data_source_id, Permission::Edit)
            .await?;

        let column_name = col_name(&row);
        if overwrite {
            column_value::remove_by_column(&self.db, &row.table_id, column_name).await?;
        }
        let known: Vec<String> = if overwrite {
            Vec::new()
        } else {
            column_value::list_by_column(&self.db, &row.table_id, column_name)
                .await?
                .into_iter()
                .map(|value| value.value)
                .collect()
        };

        let sampled = self
            .data_sources
            .sample_values(
                &source,
                table.schema.as_deref(),
                name_of(&table),
                column_name,
            )
            .await?;
        let mut sort_no = column_value::next_sort_no(&self.db, &row.table_id, column_name).await?;
        for value in sampled {
            if value.trim().is_empty() || known.iter().any(|existing| existing == &value) {
                continue;
            }
            column_value::insert(
                &self.db,
                &ColumnValue {
                    id: new_id(),
                    table_id: row.table_id.clone(),
                    column_name: column_name.to_string(),
                    value,
                    synonyms: Vec::new(),
                    sort_no,
                    audit: Audit::new(&caller.id),
                },
            )
            .await?;
            sort_no += 1;
        }
        Ok(())
    }

    /// `PUT .../columns/{columnId}/values`: delete the removed rows, upsert the rest. An item
    /// without an id is new; a row that keeps its id also keeps its position in the dictionary.
    pub async fn save_values(
        &self,
        caller: &AuthUser,
        column_id: &str,
        request: &ColumnValueListRequest,
    ) -> Result<()> {
        let row = self.load_for_dictionary(column_id).await?;
        let table = self.owner_of(&row).await?;
        self.data_sources
            .authorize(caller, &table.data_source_id, Permission::Edit)
            .await?;

        column_value::remove_by_ids(&self.db, &request.deleted_ids).await?;
        let column_name = col_name(&row);
        let mut sort_no = column_value::next_sort_no(&self.db, &row.table_id, column_name).await?;
        for item in &request.values {
            let id = item
                .id
                .clone()
                .filter(|id| !id.trim().is_empty())
                .unwrap_or_else(new_id);
            let existing = column_value::find(&self.db, &id).await?;
            let (position, audit) = match existing {
                Some(value) => (
                    value.sort_no,
                    Audit {
                        updated_by: Some(caller.id.clone()),
                        updated_at: Some(crate::time::now_iso()),
                        ..value.audit
                    },
                ),
                None => {
                    sort_no += 1;
                    (sort_no - 1, Audit::new(&caller.id))
                }
            };
            column_value::save(
                &self.db,
                &ColumnValue {
                    id,
                    table_id: row.table_id.clone(),
                    column_name: column_name.to_string(),
                    value: item.value.clone(),
                    synonyms: dedup_synonyms(&item.value, &item.synonyms),
                    sort_no: position,
                    audit,
                },
            )
            .await?;
        }
        Ok(())
    }

    /// `GET .../columns/{columnId}/values`: one page of the dictionary.
    pub async fn list_values(
        &self,
        caller: &AuthUser,
        column_id: &str,
        keyword: Option<&str>,
        page: PageReq,
    ) -> Result<PageResponse<ColumnValueView>> {
        let row = self.load_for_dictionary(column_id).await?;
        let table = self.owner_of(&row).await?;
        self.data_sources
            .authorize(caller, &table.data_source_id, Permission::View)
            .await?;
        let column_name = col_name(&row);
        let (rows, total) =
            column_value::page(&self.db, &row.table_id, column_name, keyword, page).await?;
        Ok(PageResponse::new(
            rows.iter().map(ColumnValueView::from).collect(),
            total,
            page.page,
            page.size,
        ))
    }

    // ---- shared resolution ---------------------------------------------------------------

    /// The Java services resolved a column through its table and authorised the owning source.
    async fn owner_of(&self, row: &ColumnInfo) -> Result<TableInfo> {
        self.tables.load(&row.table_id).await
    }

    async fn load(&self, column_id: &str) -> Result<ColumnInfo> {
        column::find(&self.db, column_id).await?.ok_or_else(|| {
            // `ColumnService.updateColumn` used the data-source code with a composed detail.
            DatiError::new(
                ErrorCode::DsNotFound,
                [format!("Column not found: {column_id}")],
            )
        })
    }

    async fn load_for_dictionary(&self, column_id: &str) -> Result<ColumnInfo> {
        column::find(&self.db, column_id).await?.ok_or_else(|| {
            // `ColumnValueService` threw a bare message instead of a coded error, which the
            // Java handler rendered as `CM003` with a 500 status.
            DatiError::with_message(
                ErrorCode::InternalError,
                format!("Column not found: {column_id}"),
            )
        })
    }
}

fn col_name(column: &ColumnInfo) -> &str {
    column.name.as_deref().unwrap_or_default()
}

fn name_of(table: &TableInfo) -> &str {
    table.name.as_deref().unwrap_or_default()
}

fn schema_of(table: &TableInfo) -> &str {
    table.schema.as_deref().unwrap_or_default()
}

/// The Java document stored `keywords = [value, synonyms…]` distinct, so the value itself wins over
/// a synonym that happens to repeat it.
fn dedup_synonyms(value: &str, synonyms: &[String]) -> Vec<String> {
    let mut seen: Vec<String> = vec![value.to_string()];
    let mut out = Vec::new();
    for synonym in synonyms {
        if !seen.contains(synonym) {
            seen.push(synonym.clone());
            out.push(synonym.clone());
        }
    }
    out
}
