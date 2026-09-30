//! `prompts`: the text templates a service exposes to an MCP client, edited under the service.

use axum::extract::{Path, State};
use axum::routing::{get, put};
use axum::{Json, Router};

use dati_core::model::base::IdResponse;
use dati_core::model::mcp_prompt::{McpPromptRequest, McpPromptView};

use crate::api::error::{ApiResult, JsonBody};
use crate::api::extract::Authed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/mcp-services/{service_id}/prompts",
            get(list).post(create),
        )
        .route(
            "/v1/mcp-services/{service_id}/prompts/{prompt_id}",
            put(update).delete(remove),
        )
}

async fn list(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(service_id): Path<String>,
) -> ApiResult<Json<Vec<McpPromptView>>> {
    Ok(Json(state.mcp_prompts.list(&caller, &service_id).await?))
}

async fn create(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(service_id): Path<String>,
    JsonBody(request): JsonBody<McpPromptRequest>,
) -> ApiResult<Json<IdResponse>> {
    let id = state
        .mcp_prompts
        .create(&caller, &service_id, &request)
        .await?;
    Ok(Json(IdResponse::new(id)))
}

async fn update(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path((service_id, prompt_id)): Path<(String, String)>,
    JsonBody(request): JsonBody<McpPromptRequest>,
) -> ApiResult<Json<IdResponse>> {
    state
        .mcp_prompts
        .update(&caller, &service_id, &prompt_id, &request)
        .await?;
    Ok(Json(IdResponse::new(prompt_id)))
}

async fn remove(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path((service_id, prompt_id)): Path<(String, String)>,
) -> ApiResult<Json<IdResponse>> {
    state
        .mcp_prompts
        .remove(&caller, &service_id, &prompt_id)
        .await?;
    Ok(Json(IdResponse::new(prompt_id)))
}
