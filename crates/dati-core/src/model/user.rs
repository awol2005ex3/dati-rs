//! Accounts and API keys.

use serde::{Deserialize, Serialize};

use super::base::Audit;
use crate::error::{DatiError, Result};

/// `POST /v1/auth/login`. The Java DTO marks all three fields `@NotBlank`, and the auth type must
/// be one the providers support ("local"); both failures surface as `CM001`.
#[derive(Debug, Clone, Deserialize)]
pub struct LoginRequest {
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

impl LoginRequest {
    pub fn validate(&self) -> Result<()> {
        let mut fields = Vec::new();
        for (field, value) in [
            ("type", self.r#type.as_deref()),
            ("name", self.name.as_deref()),
            ("password", self.password.as_deref()),
        ] {
            if value.is_none_or(|v| v.trim().is_empty()) {
                fields.push((field, "must not be blank"));
            }
        }
        if fields.is_empty() {
            Ok(())
        } else {
            Err(DatiError::validation(&fields))
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LoginResponse {
    pub token: String,
}

/// `POST /v1/auth/register`; the display name is optional and unchecked, as in Java.
#[derive(Debug, Clone, Deserialize)]
pub struct RegisterRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
}

impl RegisterRequest {
    pub fn validate(&self) -> Result<()> {
        let mut fields = Vec::new();
        for (field, value) in [
            ("name", self.name.as_deref()),
            ("password", self.password.as_deref()),
        ] {
            if value.is_none_or(|v| v.trim().is_empty()) {
                fields.push((field, "must not be blank"));
            }
        }
        if fields.is_empty() {
            Ok(())
        } else {
            Err(DatiError::validation(&fields))
        }
    }
}

/// `POST /v1/auth/api-keys`. `expires_in_days` is nullable; the service rejects other values.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiKeyCreateRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub expires_in_days: Option<i64>,
}

impl ApiKeyCreateRequest {
    pub fn validate(&self) -> Result<()> {
        let mut fields = Vec::new();
        match self.name.as_deref().map(str::trim) {
            None | Some("") => fields.push(("name", "must not be blank")),
            Some(name) if name.chars().count() > 64 => {
                fields.push(("name", "size must be between 0 and 64"));
            }
            Some(_) => {}
        }
        if fields.is_empty() {
            Ok(())
        } else {
            Err(DatiError::validation(&fields))
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct User {
    pub id: String,
    pub name: String,
    pub password: String,
    pub display_name: Option<String>,
    pub audit: Audit,
}

impl User {
    /// The shape returned by `/v1/auth/me` and `/v1/users/search`.
    pub fn brief(&self) -> UserBrief {
        UserBrief {
            id: self.id.clone(),
            name: self.name.clone(),
            display_name: self.display_name.clone(),
        }
    }

    /// Label used wherever the UI shows a person: display name, else login name.
    pub fn label(&self) -> String {
        self.display_name
            .clone()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| self.name.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserBrief {
    pub id: String,
    pub name: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApiKey {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub key_hash: String,
    pub key_mask: String,
    pub expires_at: Option<String>,
    pub last_used_at: Option<String>,
    pub audit: Audit,
}

impl ApiKey {
    pub fn expired(&self, now: &str) -> bool {
        self.expires_at
            .as_deref()
            .is_some_and(|expires| now > expires)
    }
}

/// What `POST /v1/auth/api-keys` returns once: the plaintext key is never stored or re-sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CreatedApiKey {
    pub id: String,
    pub name: String,
    pub key: String,
    pub key_mask: String,
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiKeyView {
    pub id: String,
    pub name: String,
    pub key_mask: String,
    pub created_at: Option<String>,
    pub expires_at: Option<String>,
    pub last_used_at: Option<String>,
}

impl From<&ApiKey> for ApiKeyView {
    fn from(key: &ApiKey) -> Self {
        Self {
            id: key.id.clone(),
            name: key.name.clone(),
            key_mask: key.key_mask.clone(),
            created_at: key.audit.created_at.clone(),
            expires_at: key.expires_at.clone(),
            last_used_at: key.last_used_at.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(name: &str, display: Option<&str>) -> User {
        User {
            id: "u-1".into(),
            name: name.into(),
            password: "hash".into(),
            display_name: display.map(str::to_string),
            audit: Audit::default(),
        }
    }

    #[test]
    fn label_prefers_display_name() {
        assert_eq!(user("alice", Some("Alice")).label(), "Alice");
        assert_eq!(user("alice", None).label(), "alice");
        assert_eq!(user("alice", Some("")).label(), "alice");
    }

    #[test]
    fn expiry_compares_iso_instants() {
        let mut key = ApiKey {
            id: "k-1".into(),
            user_id: "u-1".into(),
            name: "cli".into(),
            key_hash: "h".into(),
            key_mask: "sk_ab***cd".into(),
            expires_at: Some("2026-01-01T00:00:00Z".into()),
            last_used_at: None,
            audit: Audit::default(),
        };
        assert!(key.expired("2026-09-01T00:00:00Z"));
        assert!(!key.expired("2025-12-31T23:59:59Z"));
        key.expires_at = None;
        assert!(!key.expired("2999-01-01T00:00:00Z"));
    }

    #[test]
    fn api_key_view_omits_the_hash() {
        let key = ApiKey {
            id: "k-1".into(),
            user_id: "u-1".into(),
            name: "cli".into(),
            key_hash: "secret-hash".into(),
            key_mask: "sk_ab***cd".into(),
            expires_at: None,
            last_used_at: None,
            audit: Audit::new("u-1"),
        };
        let json = serde_json::to_value(ApiKeyView::from(&key)).unwrap();
        assert_eq!(json["key_mask"], "sk_ab***cd");
        assert!(json.get("key_hash").is_none());
        assert!(json.get("user_id").is_none());
    }
}
