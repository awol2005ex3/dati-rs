//! `/v1/data-sources/{datasourceId}/tables`: which live tables the catalogue holds.
//!
//! The table paths carry the data source in the URL for the frontend's sake, but every handler
//! resolves the row by its own id, exactly like the Java service did — a mismatched prefix is
//! therefore not rejected here.

use axum::extract::{Path, State};
use axum::routing::{get, post, put};
use axum::{Json, Router};

use dati_core::model::base::{IdResponse, PageResponse};
use dati_core::model::datasource::{AddTableRequest, TableInfoView, TableUpdateRequest};

use crate::api::error::{ApiResult, JsonBody};
use crate::api::extract::{Authed, Paging};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/data-sources/{datasource_id}/tables", get(list))
        .route(
            "/v1/data-sources/{datasource_id}/tables/added-names",
            get(added_names),
        )
        .route(
            "/v1/data-sources/{datasource_id}/tables/batch",
            post(batch_add),
        )
        .route(
            "/v1/data-sources/{datasource_id}/tables/{table_id}",
            put(update).delete(remove),
        )
}

async fn list(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(datasource_id): Path<String>,
    paging: Paging,
) -> ApiResult<Json<PageResponse<TableInfoView>>> {
    Ok(Json(
        state
            .tables
            .list(
                &caller,
                &datasource_id,
                paging.keyword.as_deref(),
                paging.page,
            )
            .await?,
    ))
}

async fn added_names(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(datasource_id): Path<String>,
) -> ApiResult<Json<Vec<String>>> {
    Ok(Json(
        state.tables.added_names(&caller, &datasource_id).await?,
    ))
}

/// The Java controller reported how many tables it registered, in the `id` field.
async fn batch_add(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(datasource_id): Path<String>,
    JsonBody(requests): JsonBody<Vec<AddTableRequest>>,
) -> ApiResult<Json<IdResponse>> {
    let ids = state
        .tables
        .batch_add(&caller, &datasource_id, &requests)
        .await?;
    Ok(Json(IdResponse::new(ids.len().to_string())))
}

async fn update(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path((_, table_id)): Path<(String, String)>,
    JsonBody(request): JsonBody<TableUpdateRequest>,
) -> ApiResult<Json<IdResponse>> {
    state.tables.update(&caller, &table_id, &request).await?;
    Ok(Json(IdResponse::new(table_id)))
}

async fn remove(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path((_, table_id)): Path<(String, String)>,
) -> ApiResult<Json<IdResponse>> {
    state.tables.remove(&caller, &table_id).await?;
    Ok(Json(IdResponse::new(table_id)))
}
