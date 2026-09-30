//! `/v1/data-sources`: the registered databases and the metadata read live from them.

use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};

use dati_core::model::base::{IdResponse, PageResponse};
use dati_core::model::datasource::{
    DataSourceInput, DataSourceView, LiveTable, UpdateDataSourceInput,
};

use crate::api::error::{ApiResult, JsonBody};
use crate::api::extract::{Authed, Paging};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/data-sources", get(list).post(add))
        .route("/v1/data-sources/test-connection", post(test_connection))
        .route("/v1/data-sources/{id}", get(one).put(update).delete(remove))
        .route("/v1/data-sources/{id}/schemas", get(schemas))
        .route(
            "/v1/data-sources/{id}/schemas/{schema}/tables",
            get(live_tables),
        )
}

/// `POST /v1/data-sources/test-connection`: `true` when the database answers, otherwise the
/// `DS001`/`DS003` body carrying the driver's reason, so the console can say what went wrong.
async fn test_connection(
    State(state): State<AppState>,
    Authed(_caller): Authed,
    JsonBody(input): JsonBody<DataSourceInput>,
) -> ApiResult<Json<bool>> {
    Ok(Json(state.data_sources.test_connection(&input).await?))
}

async fn add(
    State(state): State<AppState>,
    Authed(caller): Authed,
    JsonBody(input): JsonBody<DataSourceInput>,
) -> ApiResult<Json<IdResponse>> {
    let id = state.data_sources.add(&caller, &input).await?;
    Ok(Json(IdResponse::new(id)))
}

async fn update(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(id): Path<String>,
    JsonBody(input): JsonBody<UpdateDataSourceInput>,
) -> ApiResult<Json<IdResponse>> {
    state.data_sources.update(&caller, &id, &input).await?;
    Ok(Json(IdResponse::new(id)))
}

async fn remove(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(id): Path<String>,
) -> ApiResult<Json<IdResponse>> {
    state.data_sources.remove(&caller, &id).await?;
    Ok(Json(IdResponse::new(id)))
}

async fn list(
    State(state): State<AppState>,
    Authed(caller): Authed,
    paging: Paging,
) -> ApiResult<Json<PageResponse<DataSourceView>>> {
    Ok(Json(
        state
            .data_sources
            .list(&caller, paging.keyword.as_deref(), paging.page)
            .await?,
    ))
}

async fn one(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(id): Path<String>,
) -> ApiResult<Json<DataSourceView>> {
    Ok(Json(state.data_sources.get(&caller, &id).await?))
}

async fn schemas(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<String>>> {
    Ok(Json(state.data_sources.schemas(&caller, &id).await?))
}

async fn live_tables(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path((id, schema)): Path<(String, String)>,
) -> ApiResult<Json<Vec<LiveTable>>> {
    Ok(Json(
        state.data_sources.tables(&caller, &id, &schema).await?,
    ))
}
