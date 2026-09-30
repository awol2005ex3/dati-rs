//! `/v1/acls`: who may see which resource.

use axum::extract::{Path, State};
use axum::routing::{delete, get};
use axum::{Json, Router};

use dati_core::error::{DatiError, ErrorCode};
use dati_core::model::base::IdResponse;
use dati_core::model::permission::{AclEntryView, GrantRequest, PrincipalType, ResourceType};

use crate::api::error::{ApiResult, AppError, JsonBody};
use crate::api::extract::Authed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/acls/{resource_type}/{resource_id}",
            get(list).post(grant),
        )
        .route(
            "/v1/acls/{resource_type}/{resource_id}/{principal_type}/{principal_id}",
            delete(revoke),
        )
}

async fn grant(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path((resource_type, resource_id)): Path<(String, String)>,
    JsonBody(request): JsonBody<GrantRequest>,
) -> ApiResult<Json<IdResponse>> {
    let resource_type = parse_type(&resource_type)?;
    let id = state
        .acl
        .grant(&caller, resource_type, &resource_id, &request)
        .await?;
    Ok(Json(IdResponse::new(id)))
}

/// Revoking answers with the resource id, which is what the Java controller returned.
async fn revoke(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path((resource_type, resource_id, principal_type, principal_id)): Path<(
        String,
        String,
        String,
        String,
    )>,
) -> ApiResult<Json<IdResponse>> {
    let resource_type = parse_type(&resource_type)?;
    let principal_type = parse_principal_type(&principal_type)?;
    state
        .acl
        .revoke(
            &caller,
            resource_type,
            &resource_id,
            principal_type,
            &principal_id,
        )
        .await?;
    Ok(Json(IdResponse::new(resource_id)))
}

async fn list(
    State(state): State<AppState>,
    Authed(caller): Authed,
    Path((resource_type, resource_id)): Path<(String, String)>,
) -> ApiResult<Json<Vec<AclEntryView>>> {
    let resource_type = parse_type(&resource_type)?;
    Ok(Json(
        state.acl.list(&caller, resource_type, &resource_id).await?,
    ))
}

fn parse_type(raw: &str) -> ApiResult<ResourceType> {
    ResourceType::from_stored(raw).ok_or_else(|| {
        AppError::new(DatiError::new(
            ErrorCode::InvalidParameter,
            ["resource type is not one of DATA_SOURCE/SUBJECT/MCP_SERVICE"],
        ))
    })
}

fn parse_principal_type(raw: &str) -> ApiResult<PrincipalType> {
    PrincipalType::from_stored(raw).ok_or_else(|| {
        AppError::new(DatiError::new(
            ErrorCode::InvalidParameter,
            ["principal type is not one of USER/GROUP"],
        ))
    })
}
