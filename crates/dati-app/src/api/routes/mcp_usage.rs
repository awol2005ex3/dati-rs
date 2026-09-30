//! `/v1/mcp-services/{id}/stats` and `/…/invocation-logs`: the console's two reads over the
//! usage tables.
//!
//! The Java controller checked neither the service's existence nor the caller's right to see it —
//! the interceptor only required a valid token — so an unknown id simply reports an empty window.
//! Being more strict here would change what the reused frontend sees, so the leniency is kept.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};

use dati_core::error::DatiError;
use dati_core::model::base::{PageReq, PageResponse};
use dati_core::model::mcp_usage::{InvocationLogView, UsageStatsView};

use crate::api::error::{ApiResult, AppError};
use crate::api::extract::{Authed, Paging};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/mcp-services/{id}/stats", get(stats))
        .route(
            "/v1/mcp-services/{id}/invocation-logs",
            get(invocation_logs),
        )
}

/// `days` is a primitive `int` to Spring, so only a number that one could hold is accepted; the
/// `15` it defaults to is the retention the Java `@RequestParam` carried.
async fn stats(
    State(state): State<AppState>,
    Authed(_caller): Authed,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<UsageStatsView>> {
    let days = integer_param(&params, "days", 15)?;
    Ok(Json(state.mcp_usage.stats(&id, days).await?))
}

async fn invocation_logs(
    State(state): State<AppState>,
    Authed(_caller): Authed,
    Path(id): Path<String>,
    paging: Paging,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<PageResponse<InvocationLogView>>> {
    let tool_name = params.get("toolName").map(String::as_str);
    let success = boolean_param(&params, "success")?;
    let PageReq { page, size } = paging.page;
    let (rows, total) = state
        .mcp_usage
        .logs(&id, tool_name, success, PageReq { page, size })
        .await?;
    Ok(Json(PageResponse::new(rows, total, page, size)))
}

fn integer_param(
    params: &HashMap<String, String>,
    name: &str,
    default: i64,
) -> Result<i64, AppError> {
    let Some(raw) = params.get(name).filter(|value| !value.trim().is_empty()) else {
        return Ok(default);
    };
    raw.trim()
        .parse::<i64>()
        .map_err(|_| AppError::new(DatiError::validation(&[(name, "must be a whole number")])))
}

/// The set Spring's text-to-`Boolean` binder accepted; an empty value bound no filter at all, and
/// anything else was a conversion failure.
fn boolean_param(params: &HashMap<String, String>, name: &str) -> Result<Option<bool>, AppError> {
    let Some(raw) = params.get(name) else {
        return Ok(None);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "" => Ok(None),
        "true" | "on" | "yes" | "1" => Ok(Some(true)),
        "false" | "off" | "no" | "0" => Ok(Some(false)),
        _ => Err(AppError::new(DatiError::validation(&[(
            name,
            "must be true or false",
        )]))),
    }
}
