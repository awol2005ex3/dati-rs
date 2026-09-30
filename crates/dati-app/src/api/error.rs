//! Turns a `DatiError` into the Java error body, and a request-body problem into `CM001`.

use axum::Json as AxumJson;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{FromRequest, Request};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;

use dati_core::error::{DatiError, ErrorCode};

pub struct AppError(DatiError);

impl AppError {
    pub fn new(error: DatiError) -> Self {
        Self(error)
    }
}

impl From<DatiError> for AppError {
    fn from(error: DatiError) -> Self {
        Self(error)
    }
}

impl From<JsonRejection> for AppError {
    fn from(rejection: JsonRejection) -> Self {
        tracing::debug!(rejection = %rejection.body_text(), "rejected request body");
        Self(DatiError::new(
            ErrorCode::InvalidParameter,
            ["request body must be valid JSON"],
        ))
    }
}

/// A query parameter the binder could not read at all; `Paging` reports the individual fields
/// itself, so this only catches malformed percent-encoding and bad booleans.
impl From<QueryRejection> for AppError {
    fn from(rejection: QueryRejection) -> Self {
        tracing::debug!(rejection = %rejection.body_text(), "rejected query string");
        Self(DatiError::new(
            ErrorCode::InvalidParameter,
            ["query parameters must be valid"],
        ))
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.0.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %self.0.message(), "request failed");
        }
        (status, AxumJson(self.0.body())).into_response()
    }
}

/// A JSON body that fails as the rest of the API does: `CM001` with an error body, not axum's
/// own plain-text rejection.
pub struct JsonBody<T>(pub T);

impl<T, S> FromRequest<S> for JsonBody<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let AxumJson(body) = AxumJson::<T>::from_request(request, state).await?;
        Ok(Self(body))
    }
}

/// Every handler answers with this; the Java controllers had no success envelope.
pub type ApiResult<T> = Result<T, AppError>;
