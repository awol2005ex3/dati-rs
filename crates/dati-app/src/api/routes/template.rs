//! `/v1/template/preview` and `/v1/template/extract`: the live help the custom-tool editor shows
//! while an author types. They touch no stored service, so — unlike every other `/v1` resource —
//! they ask only for a login, never for a permission, matching the Java controller.

use axum::routing::post;
use axum::{Json, Router};

use dati_core::model::template::{
    TemplateExtractRequest, TemplateExtractResponse, TemplatePreviewRequest,
    TemplatePreviewResponse,
};
use dati_core::service::template_preview;

use crate::api::error::{ApiResult, JsonBody};
use crate::api::extract::Authed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/template/preview", post(preview))
        .route("/v1/template/extract", post(extract))
}

async fn preview(
    Authed(caller): Authed,
    JsonBody(request): JsonBody<TemplatePreviewRequest>,
) -> ApiResult<Json<TemplatePreviewResponse>> {
    Ok(Json(template_preview::preview(&caller, &request)?))
}

async fn extract(
    Authed(_caller): Authed,
    JsonBody(request): JsonBody<TemplateExtractRequest>,
) -> ApiResult<Json<TemplateExtractResponse>> {
    Ok(Json(template_preview::extract(&request)?))
}
