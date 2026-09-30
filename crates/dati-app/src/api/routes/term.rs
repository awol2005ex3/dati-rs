//! `/v1/terms` and the term half of `/v1/subjects/{subjectId}/terms`: the glossary and its
//! bindings onto tables and fields.

use axum::extract::{Path, State};
use axum::routing::{delete, get, post};
use axum::{Json, Router};

use dati_core::model::base::{IdResponse, PageResponse};
use dati_core::model::semantic::{
    CreateTermRequest, LinkTermRelationRequest, TermView, UpdateTermRequest,
};

use crate::api::error::{ApiResult, JsonBody};
use crate::api::extract::{Authed, Paging};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/subjects/{subject_id}/terms", get(list).post(create))
        .route("/v1/terms/{id}", get(one).put(update).delete(remove))
        .route("/v1/terms/{id}/relations", post(link))
        .route(
            "/v1/terms/{id}/relations/{table_id}/{field_name}",
            delete(unlink),
        )
}

async fn create(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(subject_id): Path<String>,
    JsonBody(request): JsonBody<CreateTermRequest>,
) -> ApiResult<Json<IdResponse>> {
    let id = state.terms.create(&caller, &subject_id, &request).await?;
    Ok(Json(IdResponse::new(id)))
}

async fn update(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<UpdateTermRequest>,
) -> ApiResult<Json<IdResponse>> {
    state.terms.update(&caller, &id, &request).await?;
    Ok(Json(IdResponse::new(id)))
}

async fn remove(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(id): Path<String>,
) -> ApiResult<Json<IdResponse>> {
    state.terms.remove(&caller, &id).await?;
    Ok(Json(IdResponse::new(id)))
}

async fn one(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(id): Path<String>,
) -> ApiResult<Json<TermView>> {
    Ok(Json(state.terms.get(&caller, &id).await?))
}

async fn list(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(subject_id): Path<String>,
    paging: Paging,
) -> ApiResult<Json<PageResponse<TermView>>> {
    Ok(Json(
        state
            .terms
            .list(&caller, &subject_id, paging.keyword.as_deref(), paging.page)
            .await?,
    ))
}

/// `_` stands for "no field": a path or body cannot carry an absent value, so the Java
/// controller translated the sentinel before calling the service.
fn unbind(field_name: Option<String>) -> Option<String> {
    field_name.filter(|value| value != "_")
}

async fn link(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(id): Path<String>,
    JsonBody(mut request): JsonBody<LinkTermRelationRequest>,
) -> ApiResult<Json<IdResponse>> {
    request.field_name = unbind(request.field_name);
    state.terms.link(&caller, &id, &request).await?;
    Ok(Json(IdResponse::new(id)))
}

async fn unlink(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path((id, table_id, field_name)): Path<(String, String, String)>,
) -> ApiResult<Json<IdResponse>> {
    state
        .terms
        .unlink(&caller, &id, &table_id, unbind(Some(field_name)).as_deref())
        .await?;
    Ok(Json(IdResponse::new(id)))
}
