//! `POST /{code}/mcp`: the MCP transport itself.
//!
//! The Java controller was as thin as this one is — it read the three headers the protocol gate
//! needs and the client address, then handed the raw body over. Everything that decides *what* to
//! answer lives in `McpEndpoint`, which is why this file does no error mapping of its own: a
//! protocol failure is a JSON-RPC envelope, not the `CM001` body the rest of the API answers with.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};

use dati_core::service::mcp_endpoint::{EndpointCall, EndpointResponse};

use crate::api::error::ApiResult;
use crate::api::extract::Authed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/{code}/mcp", post(handle))
}

async fn handle(
    State(state): State<AppState>,
    Authed(caller): Authed,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(code): Path<String>,
    headers: HeaderMap,
    body: String,
) -> ApiResult<Response> {
    let client_ip = client_ip(&headers, peer);
    let call = EndpointCall {
        code: &code,
        body: &body,
        origin: header(&headers, "origin"),
        protocol_version: header(&headers, "mcp-protocol-version"),
        caller: &caller,
        client_ip: Some(&client_ip),
    };
    Ok(answer(state.mcp_endpoint.handle(&call).await?))
}

/// The header as the Java `getHeader` saw it: absent, or not readable as text, is the same null.
fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// `X-Forwarded-For` wins, and only its first entry — the address the request entered through.
fn client_ip(headers: &HeaderMap, peer: SocketAddr) -> String {
    match header(headers, "x-forwarded-for") {
        Some(forwarded) if !forwarded.trim().is_empty() => forwarded
            .split(',')
            .next()
            .unwrap_or_default()
            .trim()
            .to_string(),
        _ => peer.ip().to_string(),
    }
}

/// The status always goes out; the body only when the transport had something to say. A rejection
/// that arrives before the JSON-RPC id is readable is answered without an envelope at all.
fn answer(result: EndpointResponse) -> Response {
    let status = axum::http::StatusCode::from_u16(result.status)
        .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    match result.body {
        Some(body) => (status, Json(body)).into_response(),
        None => status.into_response(),
    }
}
