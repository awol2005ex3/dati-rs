//! `/v1/data-sources/{datasourceId}/tables/{tableId}/columns`: the column list, its sync, and the
//! value dictionary that replaces the Java version's Elasticsearch documents.
//!
//! Only `sync` and `extract` read the ids of their prefix; the row-oriented endpoints resolve the
//! column by its own id, so a stale prefix is not rejected, which mirrors the Java controllers.

use axum::extract::{Path, Query, State};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;

use dati_core::model::base::{IdResponse, PageResponse};
use dati_core::model::datasource::{
    ColumnInfoView, ColumnUpdateRequest, ColumnValueListRequest, ColumnValueView,
};

use crate::api::error::{ApiResult, JsonBody};
use crate::api::extract::{Authed, Paging};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/data-sources/{datasource_id}/tables/{table_id}/columns",
            get(list),
        )
        .route(
            "/v1/data-sources/{datasource_id}/tables/{table_id}/columns/sync",
            post(sync),
        )
        .route(
            "/v1/data-sources/{datasource_id}/tables/{table_id}/columns/{column_id}",
            put(update),
        )
        .route(
            "/v1/data-sources/{datasource_id}/tables/{table_id}/columns/{column_id}/values",
            get(list_values).put(save_values),
        )
        .route(
            "/v1/data-sources/{datasource_id}/tables/{table_id}/columns/{column_id}/values/extract",
            post(extract),
        )
}

/// `POST .../columns/sync`, whose `overwrite_existing` defaults to false.
#[derive(Debug, Deserialize)]
struct SyncQuery {
    #[serde(default)]
    overwrite_existing: bool,
}

/// `POST .../values/extract`, whose `overwrite` defaults to false.
#[derive(Debug, Deserialize)]
struct ExtractQuery {
    #[serde(default)]
    overwrite: bool,
}

async fn list(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path((_, table_id)): Path<(String, String)>,
    paging: Paging,
) -> ApiResult<Json<PageResponse<ColumnInfoView>>> {
    Ok(Json(
        state
            .columns
            .list(&caller, &table_id, paging.keyword.as_deref(), paging.page)
            .await?,
    ))
}

async fn update(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path((_, _, column_id)): Path<(String, String, String)>,
    JsonBody(request): JsonBody<ColumnUpdateRequest>,
) -> ApiResult<Json<IdResponse>> {
    state.columns.update(&caller, &column_id, &request).await?;
    Ok(Json(IdResponse::new(column_id)))
}

async fn sync(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path((datasource_id, table_id)): Path<(String, String)>,
    Query(query): Query<SyncQuery>,
) -> ApiResult<Json<IdResponse>> {
    state
        .columns
        .sync(&caller, &datasource_id, &table_id, query.overwrite_existing)
        .await?;
    Ok(Json(IdResponse::new(table_id)))
}

async fn extract(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path((datasource_id, _, column_id)): Path<(String, String, String)>,
    Query(query): Query<ExtractQuery>,
) -> ApiResult<Json<IdResponse>> {
    state
        .columns
        .extract(&caller, &datasource_id, &column_id, query.overwrite)
        .await?;
    Ok(Json(IdResponse::new(column_id)))
}

async fn list_values(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path((_, _, column_id)): Path<(String, String, String)>,
    paging: Paging,
) -> ApiResult<Json<PageResponse<ColumnValueView>>> {
    Ok(Json(
        state
            .columns
            .list_values(&caller, &column_id, paging.keyword.as_deref(), paging.page)
            .await?,
    ))
}

async fn save_values(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path((_, _, column_id)): Path<(String, String, String)>,
    JsonBody(request): JsonBody<ColumnValueListRequest>,
) -> ApiResult<Json<IdResponse>> {
    state
        .columns
        .save_values(&caller, &column_id, &request)
        .await?;
    Ok(Json(IdResponse::new(column_id)))
}
