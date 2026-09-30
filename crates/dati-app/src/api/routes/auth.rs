//! `/v1/auth`: login, registration and the current identity.

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};

use dati_core::model::base::IdResponse;
use dati_core::model::user::{LoginRequest, LoginResponse, RegisterRequest, UserBrief};

use crate::api::error::{ApiResult, JsonBody};
use crate::api::extract::Authed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/auth/login", post(login))
        .route("/v1/auth/register", post(register))
        .route("/v1/auth/me", get(me))
}

async fn login(
    State(state): State<AppState>,
    JsonBody(request): JsonBody<LoginRequest>,
) -> ApiResult<Json<LoginResponse>> {
    Ok(Json(state.auth.login(&request).await?))
}

async fn register(
    State(state): State<AppState>,
    JsonBody(request): JsonBody<RegisterRequest>,
) -> ApiResult<Json<IdResponse>> {
    Ok(Json(IdResponse::new(state.auth.register(&request).await?)))
}

/// The identity travels in the token, so `/me` never re-reads the user table.
async fn me(Authed(user): Authed) -> Json<UserBrief> {
    Json(UserBrief {
        id: user.id,
        name: user.name,
        display_name: user.display_name,
    })
}
