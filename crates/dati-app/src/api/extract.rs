//! Request parts: the credential check every protected handler leans on, and the paging query
//! object every list endpoint shares.

use axum::extract::{FromRequestParts, Query};
use axum::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use dati_core::auth::AuthUser;
use dati_core::error::{DatiError, ErrorCode};
use dati_core::model::base::PageReq;

use crate::api::error::AppError;
use crate::state::AppState;

/// An authenticated caller, resolved from `Authorization: Bearer <jwt | sk-key>`.
pub struct Authed(pub AuthUser);

/// The Java interceptor answered with a bare 401, no body, only `WWW-Authenticate: Bearer`.
pub struct Unauthorized;

impl IntoResponse for Unauthorized {
    fn into_response(self) -> Response {
        (
            StatusCode::UNAUTHORIZED,
            [(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"))],
        )
            .into_response()
    }
}

impl FromRequestParts<AppState> for Authed {
    type Rejection = Unauthorized;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        state
            .auth
            .authenticate(header)
            .await
            .map(Authed)
            .map_err(|_| Unauthorized)
    }
}

/// The Java `PageReq` bound with `@Valid` from the query string, plus the `keyword` every list
/// endpoint accepts. Both are optional and their defaults are the field initializers.
#[derive(Debug, Clone)]
pub struct Paging {
    pub page: PageReq,
    pub keyword: Option<String>,
}

/// Everything arrives as text: a value that is not a number at all is a parameter error, not a
/// deserialization failure that would hide which field is wrong.
#[derive(Debug, Default, Deserialize)]
struct RawPaging {
    #[serde(default)]
    page: Option<String>,
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    keyword: Option<String>,
}

impl FromRequestParts<AppState> for Paging {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let raw = Query::<RawPaging>::from_request_parts(parts, state)
            .await
            .map_err(AppError::from)?
            .0;
        let mut violations = Vec::new();
        let page = bounded(raw.page, "page", 1, i64::MAX, &mut violations);
        let size = bounded(raw.size, "size", 10, 100, &mut violations);
        if !violations.is_empty() {
            // `@Min` / `@Max` on the bound object came out of the binder as one joined message.
            return Err(AppError::new(DatiError::new(
                ErrorCode::InvalidParameter,
                [violations.join("; ")],
            )));
        }
        Ok(Self {
            page: PageReq { page, size },
            keyword: raw.keyword,
        })
    }
}

fn bounded(
    raw: Option<String>,
    field: &str,
    default: i64,
    max: i64,
    violations: &mut Vec<String>,
) -> i64 {
    let Some(raw) = raw.filter(|value| !value.trim().is_empty()) else {
        return default;
    };
    let Ok(value) = raw.trim().parse::<i64>() else {
        violations.push(format!("{field}: must be a number"));
        return default;
    };
    if value < 1 {
        violations.push(format!("{field}: must be greater than or equal to 1"));
        return default;
    }
    if value > max {
        violations.push(format!("{field}: must be less than or equal to {max}"));
        return default;
    }
    value
}
