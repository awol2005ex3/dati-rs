//! API keys: `sk_` + 43 base64url characters, stored only as a SHA-256 digest.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::error::{DatiError, ErrorCode, Result};

pub const KEY_PREFIX: &str = "sk_";
pub const RANDOM_BYTES: usize = 32;
pub const ALLOWED_EXPIRY_DAYS: [i64; 5] = [7, 30, 90, 180, 365];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeySecret {
    /// Shown to the client exactly once.
    pub plaintext: String,
    pub hash: String,
    pub mask: String,
}

impl ApiKeySecret {
    pub fn generate() -> Self {
        let mut bytes = [0u8; RANDOM_BYTES];
        rand::rng().fill_bytes(&mut bytes);
        Self::from_plaintext(format!("{KEY_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes)))
    }

    pub fn from_plaintext(plaintext: String) -> Self {
        let body = plaintext.trim();
        let mask = match body.get(KEY_PREFIX.len()..) {
            Some(rest) if rest.chars().count() > 8 => {
                let head: String = rest.chars().take(4).collect();
                let tail: String = rest.chars().rev().take(4).collect::<String>();
                let tail: String = tail.chars().rev().collect();
                format!("{KEY_PREFIX}{head}***{tail}")
            }
            _ => format!("{KEY_PREFIX}***"),
        };
        Self {
            plaintext: body.to_string(),
            hash: digest(body),
            mask,
        }
    }

    /// A bearer token is only looked up as a key when it carries the `sk_` prefix; anything else
    /// is a JWT, exactly like the two Java authentication providers.
    pub fn looks_like_api_key(value: &str) -> bool {
        value.trim().starts_with(KEY_PREFIX)
    }

    pub fn is_expired(expires_at: Option<&str>, now: &str) -> bool {
        expires_at.is_some_and(|expires| now > expires)
    }

    pub fn validate_expiry_days(days: Option<i64>) -> Result<Option<i64>> {
        match days {
            None => Ok(None),
            Some(value) if ALLOWED_EXPIRY_DAYS.contains(&value) => Ok(Some(value)),
            Some(_) => Err(DatiError::with_message(
                ErrorCode::AuthKeyInvalidExpiry,
                ErrorCode::AuthKeyInvalidExpiry.template(),
            )),
        }
    }
}

pub fn digest(plaintext: &str) -> String {
    hex::encode(Sha256::digest(plaintext.as_bytes()))
}

/// Pulls the credential out of an `Authorization: Bearer <token>` header.
pub fn bearer_token(header: Option<&str>) -> Option<&str> {
    let value = header?.trim();
    let rest = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?;
    let rest = rest.trim();
    (!rest.is_empty()).then_some(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_match_the_java_format() {
        let key = ApiKeySecret::generate();
        assert!(key.plaintext.starts_with("sk_"));
        assert_eq!(key.plaintext.len(), 3 + 43);
        assert_eq!(key.hash.len(), 64);
        assert!(key.hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(key.mask.matches('*').count(), 3);
        assert_eq!(&key.mask[..3], "sk_");
        let body = &key.plaintext[KEY_PREFIX.len()..];
        let hidden = &body[4..body.len() - 4];
        assert!(!key.mask.contains(hidden) && hidden.len() > 20);
    }

    #[test]
    fn hashing_matches_sha256_lowercase_hex() {
        // Published SHA-256 vectors.
        assert_eq!(
            digest(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            digest("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_ne!(digest("sk_demo"), digest("SK_DEMO"));
    }

    #[test]
    fn masks_never_reveal_the_middle() {
        let key = ApiKeySecret::from_plaintext(
            "sk_abcd1234efgh5678ijkl9012mnop3456qrst7890uvwx1234".into(),
        );
        assert_eq!(key.mask, "sk_abcd***1234");
        assert_eq!(key.hash, digest(&key.plaintext));
    }

    #[test]
    fn expiry_boundary_is_strict() {
        assert!(ApiKeySecret::is_expired(
            Some("2026-01-01T00:00:00Z"),
            "2026-01-01T00:00:01Z"
        ));
        assert!(!ApiKeySecret::is_expired(
            Some("2026-01-01T00:00:00Z"),
            "2026-01-01T00:00:00Z"
        ));
        assert!(!ApiKeySecret::is_expired(None, "2999-01-01T00:00:00Z"));
    }

    #[test]
    fn only_the_published_expiry_intervals_are_accepted() {
        for days in ALLOWED_EXPIRY_DAYS {
            assert_eq!(
                ApiKeySecret::validate_expiry_days(Some(days)).unwrap(),
                Some(days)
            );
        }
        assert_eq!(ApiKeySecret::validate_expiry_days(None).unwrap(), None);
        for days in [0, 1, 15, 60, 366, -7] {
            let err = ApiKeySecret::validate_expiry_days(Some(days)).unwrap_err();
            assert_eq!(err.code(), ErrorCode::AuthKeyInvalidExpiry);
            assert_eq!(err.status(), 400);
        }
    }

    #[test]
    fn bearer_parsing_requires_the_scheme() {
        assert_eq!(bearer_token(Some("Bearer abc123")), Some("abc123"));
        assert_eq!(bearer_token(Some("  Bearer   abc123 ")), Some("abc123"));
        assert_eq!(bearer_token(Some("Basic abc123")), None);
        assert_eq!(bearer_token(Some("Bearer ")), None);
        assert_eq!(bearer_token(None), None);
        assert!(ApiKeySecret::looks_like_api_key("sk_something"));
        assert!(!ApiKeySecret::looks_like_api_key("eyJhbGciOi"));
    }
}
