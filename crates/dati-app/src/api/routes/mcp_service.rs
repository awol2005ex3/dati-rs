//! `/v1/mcp-services`: the services themselves and the metadata scope each one may talk about.
//!
//! Publish, disable/enable, snapshots, rollback and the usage endpoints are the sibling routes'
//! concern; they are merged onto the same paths.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};

use dati_core::error::DatiError;
use dati_core::model::base::{IdResponse, PageResponse};
use dati_core::model::mcp::{
    DataScopeRequest, DataScopeResponse, McpServiceCreateRequest, McpServiceStatus,
    McpServiceUpdateRequest, McpServiceView,
};

use crate::api::error::{ApiResult, AppError, JsonBody};
use crate::api::extract::{Authed, Paging};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/mcp-services", get(list).post(create))
        .route("/v1/mcp-services/{id}", get(one).put(update).delete(remove))
        .route(
            "/v1/mcp-services/{id}/data-scope",
            get(data_scope).put(save_data_scope),
        )
}

async fn create(
    State(state): State<AppState>,
    Authed(caller): Authed,
    JsonBody(request): JsonBody<McpServiceCreateRequest>,
) -> ApiResult<Json<IdResponse>> {
    let id = state.mcp_services.create(&caller, &request).await?;
    Ok(Json(IdResponse::new(id)))
}

async fn update(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<McpServiceUpdateRequest>,
) -> ApiResult<Json<IdResponse>> {
    state.mcp_services.update(&caller, &id, &request).await?;
    Ok(Json(IdResponse::new(id)))
}

async fn remove(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(id): Path<String>,
) -> ApiResult<Json<IdResponse>> {
    state.mcp_services.remove(&caller, &id).await?;
    Ok(Json(IdResponse::new(id)))
}

async fn one(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(id): Path<String>,
) -> ApiResult<Json<McpServiceView>> {
    Ok(Json(state.mcp_services.get(&caller, &id).await?))
}

/// `status` is a query parameter the binder reads as text, so an unknown value is reported the way
/// the Java type mismatch was: a `CM001` on the parameter, before the service sees it.
async fn list(
    State(state): State<AppState>,
    Authed(caller): Authed,
    paging: Paging,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<PageResponse<McpServiceView>>> {
    let status = optional_status(&params)?;
    Ok(Json(
        state
            .mcp_services
            .list(&caller, paging.keyword.as_deref(), status, paging.page)
            .await?,
    ))
}

async fn data_scope(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(id): Path<String>,
) -> ApiResult<Json<DataScopeResponse>> {
    Ok(Json(state.mcp_services.data_scope(&caller, &id).await?))
}

async fn save_data_scope(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<DataScopeRequest>,
) -> ApiResult<Json<IdResponse>> {
    state
        .mcp_services
        .save_data_scope(&caller, &id, &request)
        .await?;
    Ok(Json(IdResponse::new(id)))
}

fn optional_status(params: &HashMap<String, String>) -> Result<Option<McpServiceStatus>, AppError> {
    let Some(raw) = params
        .get("status")
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };
    McpServiceStatus::from_stored(raw).map(Some).ok_or_else(|| {
        AppError::new(DatiError::validation(&[(
            "status",
            "must be one of DRAFT, PUBLISHED, DISABLED",
        )]))
    })
}
