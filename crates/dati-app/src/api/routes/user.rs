//! `/v1/users`: the account picker behind the grant dialog.

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use dati_core::model::user::UserBrief;

use crate::api::error::ApiResult;
use crate::api::extract::Authed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/users/search", get(search))
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    keyword: Option<String>,
}

async fn search(
    State(state): State<AppState>,
    Authed(_caller): Authed,
    Query(query): Query<SearchQuery>,
) -> ApiResult<Json<Vec<UserBrief>>> {
    let keyword = query.keyword.unwrap_or_default();
    Ok(Json(state.auth.search_users(&keyword).await?))
}
