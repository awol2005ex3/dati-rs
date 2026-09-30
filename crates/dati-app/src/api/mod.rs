//! The REST surface of the admin API.

pub mod error;
pub mod extract;
pub mod routes;

use axum::Router;

use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(routes::auth::routes())
        .merge(routes::api_key::routes())
        .merge(routes::user::routes())
        .merge(routes::acl::routes())
        .merge(routes::datasource::routes())
        .merge(routes::table::routes())
        .merge(routes::column::routes())
        .merge(routes::subject::routes())
        .merge(routes::term::routes())
        .merge(routes::mcp_service::routes())
        .merge(routes::mcp_tool::routes())
        .merge(routes::mcp_prompt::routes())
        .merge(routes::mcp_version::routes())
        .merge(routes::mcp_usage::routes())
        .merge(routes::mcp_endpoint::routes())
        .merge(routes::template::routes())
        .merge(routes::system::routes())
        .with_state(state)
}
