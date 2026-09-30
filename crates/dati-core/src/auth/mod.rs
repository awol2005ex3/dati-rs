//! Authentication: password hashing, JWT issuance, API key generation and verification.

pub mod apikey;
pub mod jwt;
pub mod password;

pub use apikey::ApiKeySecret;
pub use jwt::{Jwt, JwtClaims};

use crate::error::{DatiError, ErrorCode};

/// The identity attached to a request, from either a JWT or an API key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthUser {
    pub id: String,
    pub name: String,
    pub display_name: Option<String>,
}

impl AuthUser {
    pub fn from_user(user: &crate::model::user::User) -> Self {
        Self {
            id: user.id.clone(),
            name: user.name.clone(),
            display_name: user.display_name.clone(),
        }
    }

    pub fn label(&self) -> String {
        self.display_name
            .clone()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| self.name.clone())
    }
}

/// Raised when the `Authorization` header carries nothing usable.
pub fn unauthorized() -> DatiError {
    DatiError::with_message(ErrorCode::AuthTokenInvalid, "Token invalid or expired")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_user_label_falls_back_to_login_name() {
        let user = AuthUser {
            id: "u-1".into(),
            name: "bob".into(),
            display_name: None,
        };
        assert_eq!(user.label(), "bob");
        let user = AuthUser {
            id: "u-1".into(),
            name: "bob".into(),
            display_name: Some("Bobby".into()),
        };
        assert_eq!(user.label(), "Bobby");
    }

    #[test]
    fn unauthorized_maps_to_401() {
        let err = unauthorized();
        assert_eq!(err.code(), ErrorCode::AuthTokenInvalid);
        assert_eq!(err.status(), 401);
        assert_eq!(err.code().code(), "AUTH002");
    }
}
