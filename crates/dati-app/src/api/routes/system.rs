//! `GET /v1/system/config`: the sampling limits (`COLUMN_VALUE_SAMPLE_LIMIT`,
//! `COLUMN_VALUE_LENGTH_LIMIT`) and the database list the console needs before it can render the
//! data source form. Login is enough to read it — the Java controller asked for no permission.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};

use dati_core::model::datasource::SystemConfigResponse;

use crate::api::error::ApiResult;
use crate::api::extract::Authed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/system/config", get(config))
}

async fn config(
    State(state): State<AppState>,
    Authed(_caller): Authed,
) -> ApiResult<Json<SystemConfigResponse>> {
    Ok(Json(SystemConfigResponse::new(
        state.config.column_value_sample_limit,
        state.config.column_value_length_limit,
    )))
}
