//! `/v1/mcp-services/{id}/…`: the publish lifecycle — freeze a version, move the service between
//! its states, read the version history, and roll the draft back to an older snapshot.
//!
//! These are merged onto the same paths the service CRUD uses; the two route files never declare
//! the same method on the same path.

use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};

use dati_core::model::base::IdResponse;
use dati_core::model::mcp_version::{
    McpServiceDiffVO, McpServiceSnapshotVO, PublishRequest, RollbackRequest,
};

use crate::api::error::{ApiResult, JsonBody};
use crate::api::extract::Authed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/mcp-services/{id}/publish", post(publish))
        .route("/v1/mcp-services/{id}/disable", post(disable))
        .route("/v1/mcp-services/{id}/enable", post(enable))
        .route("/v1/mcp-services/{id}/diff", get(diff))
        .route("/v1/mcp-services/{id}/snapshots", get(snapshots))
        .route("/v1/mcp-services/{id}/rollback", post(rollback))
}

/// `POST /{id}/publish`. Java bound the body with `required = false`, so a call with no body at all
/// still publishes; the returned id is the new snapshot's, not the service's.
async fn publish(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(id): Path<String>,
    body: Option<Json<PublishRequest>>,
) -> ApiResult<Json<IdResponse>> {
    let request = body.map(|Json(request)| request).unwrap_or_default();
    let snapshot_id = state.mcp_versions.publish(&caller, &id, &request).await?;
    Ok(Json(IdResponse::new(snapshot_id)))
}

async fn disable(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(id): Path<String>,
) -> ApiResult<Json<IdResponse>> {
    let id = state.mcp_versions.disable(&caller, &id).await?;
    Ok(Json(IdResponse::new(id)))
}

async fn enable(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(id): Path<String>,
) -> ApiResult<Json<IdResponse>> {
    let id = state.mcp_versions.enable(&caller, &id).await?;
    Ok(Json(IdResponse::new(id)))
}

async fn diff(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(id): Path<String>,
) -> ApiResult<Json<McpServiceDiffVO>> {
    Ok(Json(state.mcp_versions.diff(&caller, &id).await?))
}

async fn snapshots(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<McpServiceSnapshotVO>>> {
    Ok(Json(state.mcp_versions.snapshots(&caller, &id).await?))
}

async fn rollback(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<RollbackRequest>,
) -> ApiResult<Json<IdResponse>> {
    let snapshot_id = state.mcp_versions.rollback(&caller, &id, &request).await?;
    Ok(Json(IdResponse::new(snapshot_id)))
}
