//! `/v1/subjects`: the business domains, the tables they hold and the tables they may still take.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::routing::{delete, get};
use axum::{Json, Router};

use dati_core::error::{DatiError, ErrorCode};
use dati_core::model::base::{IdResponse, PageResponse};
use dati_core::model::datasource::TableInfoView;
use dati_core::model::semantic::{
    AddTableToSubjectRequest, AvailableTable, CreateSubjectRequest, SubjectView,
    UpdateSubjectRequest,
};

use crate::api::error::{ApiResult, AppError, JsonBody};
use crate::api::extract::{Authed, Paging};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/subjects", get(list).post(create))
        .route("/v1/subjects/{id}", get(one).put(update).delete(remove))
        .route("/v1/subjects/{id}/tables", get(list_tables).post(add_table))
        .route("/v1/subjects/{id}/tables/{table_id}", delete(remove_table))
        .route("/v1/subjects/{id}/available-tables", get(available_tables))
}

async fn create(
    State(state): State<AppState>,
    Authed(caller): Authed,
    JsonBody(request): JsonBody<CreateSubjectRequest>,
) -> ApiResult<Json<IdResponse>> {
    let id = state.subjects.create(&caller, &request).await?;
    Ok(Json(IdResponse::new(id)))
}

async fn update(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<UpdateSubjectRequest>,
) -> ApiResult<Json<IdResponse>> {
    state.subjects.update(&caller, &id, &request).await?;
    Ok(Json(IdResponse::new(id)))
}

async fn remove(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(id): Path<String>,
) -> ApiResult<Json<IdResponse>> {
    state.subjects.remove(&caller, &id).await?;
    Ok(Json(IdResponse::new(id)))
}

async fn one(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(id): Path<String>,
) -> ApiResult<Json<SubjectView>> {
    Ok(Json(state.subjects.get(&caller, &id).await?))
}

async fn list(
    State(state): State<AppState>,
    Authed(caller): Authed,
    paging: Paging,
) -> ApiResult<Json<PageResponse<SubjectView>>> {
    Ok(Json(
        state
            .subjects
            .list(&caller, paging.keyword.as_deref(), paging.page)
            .await?,
    ))
}

async fn list_tables(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(id): Path<String>,
    paging: Paging,
) -> ApiResult<Json<PageResponse<TableInfoView>>> {
    Ok(Json(
        state
            .subjects
            .tables(&caller, &id, paging.keyword.as_deref(), paging.page)
            .await?,
    ))
}

/// The Java controller echoed the request's table id, not a generated one.
async fn add_table(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<AddTableToSubjectRequest>,
) -> ApiResult<Json<IdResponse>> {
    request.validate()?;
    let table_id = request.table_id.unwrap_or_default();
    state.subjects.add_table(&caller, &id, &table_id).await?;
    Ok(Json(IdResponse::new(table_id)))
}

async fn remove_table(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path((id, table_id)): Path<(String, String)>,
) -> ApiResult<Json<IdResponse>> {
    state.subjects.remove_table(&caller, &id, &table_id).await?;
    Ok(Json(IdResponse::new(table_id)))
}

/// `schema` is a required parameter in the Java controller; an absent one is rejected before the
/// service sees it, while `?schema=` binds the empty string and simply finds nothing.
async fn available_tables(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Vec<AvailableTable>>> {
    let schema = required_query(&params, "schema")?;
    Ok(Json(
        state
            .subjects
            .available_tables(&caller, &id, schema)
            .await?,
    ))
}

fn required_query<'a>(
    params: &'a HashMap<String, String>,
    name: &str,
) -> Result<&'a String, AppError> {
    params.get(name).ok_or_else(|| {
        AppError::new(DatiError::new(
            ErrorCode::InvalidParameter,
            [format!("Required query parameter '{name}' is not present.")],
        ))
    })
}
