//! The tool slots of a service: `GET/POST /v1/mcp-services/{id}/tools`, the `PUT/DELETE` on one
//! tool, and the annotation probe the editor calls before it saves a parameterized template.

use axum::extract::{Path, State};
use axum::routing::{get, post, put};
use axum::{Json, Router};

use dati_core::model::base::IdResponse;
use dati_core::model::mcp_tool::{
    CustomToolRequest, DetectAnnotationsRequest, DetectAnnotationsResponse, ToolsResponse,
};
use dati_core::service::mcp_tool::detect_annotations;

use crate::api::error::{ApiResult, JsonBody};
use crate::api::extract::Authed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/mcp-services/{service_id}/tools",
            get(list).post(create),
        )
        .route(
            "/v1/mcp-services/{service_id}/tools/detect-annotations",
            post(detect),
        )
        .route(
            "/v1/mcp-services/{service_id}/tools/{tool_id}",
            put(update).delete(remove),
        )
}

async fn list(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(service_id): Path<String>,
) -> ApiResult<Json<ToolsResponse>> {
    Ok(Json(state.mcp_tools.list(&caller, &service_id).await?))
}

async fn create(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(service_id): Path<String>,
    JsonBody(request): JsonBody<CustomToolRequest>,
) -> ApiResult<Json<IdResponse>> {
    let id = state
        .mcp_tools
        .create(&caller, &service_id, &request)
        .await?;
    Ok(Json(IdResponse::new(id)))
}

async fn update(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path((service_id, tool_id)): Path<(String, String)>,
    JsonBody(request): JsonBody<CustomToolRequest>,
) -> ApiResult<Json<IdResponse>> {
    state
        .mcp_tools
        .update(&caller, &service_id, &tool_id, &request)
        .await?;
    Ok(Json(IdResponse::new(tool_id)))
}

async fn remove(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path((service_id, tool_id)): Path<(String, String)>,
) -> ApiResult<Json<IdResponse>> {
    state
        .mcp_tools
        .remove(&caller, &service_id, &tool_id)
        .await?;
    Ok(Json(IdResponse::new(tool_id)))
}

/// The editor's preview of the three MCP annotations. It reads no stored service, so — like the
/// template endpoints — it only asks for a login, never for a permission on a service.
async fn detect(
    Authed(_caller): Authed,
    JsonBody(request): JsonBody<DetectAnnotationsRequest>,
) -> ApiResult<Json<DetectAnnotationsResponse>> {
    request.validate()?;
    let template = request.template.clone().unwrap_or_default();
    Ok(Json(detect_annotations(&template, &request.parameters)?))
}
