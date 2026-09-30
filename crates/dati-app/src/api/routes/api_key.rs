//! `/v1/auth/api-keys`: personal bearer keys.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, post};
use axum::{Json, Router};

use dati_core::model::user::{ApiKeyCreateRequest, ApiKeyView, CreatedApiKey};

use crate::api::error::{ApiResult, JsonBody};
use crate::api::extract::Authed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/auth/api-keys", post(create).get(list))
        .route("/v1/auth/api-keys/{id}", delete(revoke))
}

async fn create(
    State(state): State<AppState>,
    Authed(caller): Authed,
    JsonBody(request): JsonBody<ApiKeyCreateRequest>,
) -> ApiResult<(StatusCode, Json<CreatedApiKey>)> {
    let created = state.auth.create_api_key(&caller, &request).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

async fn list(
    State(state): State<AppState>,
    Authed(caller): Authed,
) -> ApiResult<Json<Vec<ApiKeyView>>> {
    Ok(Json(state.auth.list_api_keys(&caller).await?))
}

async fn revoke(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    state.auth.delete_api_key(&caller, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}
