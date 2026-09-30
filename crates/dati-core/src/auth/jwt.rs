//! HS256 tokens carrying the same claims the Java `JwtTokenHelper` wrote: `sub`, `name`,
//! `display_name`, `iat`, `exp`.

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::auth::AuthUser;
use crate::config::Config;
use crate::error::{DatiError, ErrorCode, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JwtClaims {
    pub sub: String,
    pub name: String,
    pub display_name: Option<String>,
    pub iat: i64,
    pub exp: i64,
}

#[derive(Clone)]
pub struct Jwt {
    secret: Vec<u8>,
    expiration_seconds: i64,
}

impl Jwt {
    pub fn new(config: &Config) -> Self {
        Self {
            secret: config.jwt_secret.as_bytes().to_vec(),
            expiration_seconds: config.jwt_expiration_seconds.max(1),
        }
    }

    pub fn encoding_key(&self) -> jsonwebtoken::EncodingKey {
        jsonwebtoken::EncodingKey::from_secret(&self.secret)
    }

    pub fn decoding_key(&self) -> jsonwebtoken::DecodingKey {
        jsonwebtoken::DecodingKey::from_secret(&self.secret)
    }

    pub fn issue(&self, user: &AuthUser) -> Result<String> {
        let now = Utc::now().timestamp();
        let claims = JwtClaims {
            sub: user.id.clone(),
            name: user.name.clone(),
            display_name: user.display_name.clone(),
            iat: now,
            exp: now + self.expiration_seconds,
        };
        jsonwebtoken::encode(
            &jsonwebtoken::Header::default(),
            &claims,
            &self.encoding_key(),
        )
        .map_err(|err| {
            tracing::error!(error = %err, "cannot sign token");
            DatiError::with_message(ErrorCode::InternalError, "Internal server error")
        })
    }

    /// Any signature/validation failure collapses to a single 401, as the interceptor did.
    pub fn verify(&self, token: &str) -> Result<AuthUser> {
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
        validation.required_spec_claims = ["exp".to_string()].into_iter().collect();
        let token_data =
            jsonwebtoken::decode::<JwtClaims>(token.trim(), &self.decoding_key(), &validation)
                .map_err(|err| {
                    tracing::debug!(error = %err, "token rejected");
                    crate::auth::unauthorized()
                })?;
        let claims = token_data.claims;
        if claims.sub.is_empty() {
            return Err(crate::auth::unauthorized());
        }
        Ok(AuthUser {
            id: claims.sub,
            name: claims.name,
            display_name: claims.display_name,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(secret: &str) -> Jwt {
        let config = Config {
            jwt_secret: secret.into(),
            jwt_expiration_seconds: 3600,
            ..Config::from_source(|_key| None::<String>)
        };
        Jwt::new(&config)
    }

    fn user() -> AuthUser {
        AuthUser {
            id: "3f2b1f0c-0000-4000-8000-000000000001".into(),
            name: "alice".into(),
            display_name: Some("Alice".into()),
        }
    }

    #[test]
    fn round_trips_all_identity_claims() {
        let jwt = jwt("this-is-a-32-char-long-default-secret-key");
        let token = jwt.issue(&user()).unwrap();
        assert_eq!(token.split('.').count(), 3);
        let decoded = jwt.verify(&token).unwrap();
        assert_eq!(decoded, user());
    }

    #[test]
    fn claims_keep_the_java_names() {
        let jwt = jwt("this-is-a-32-char-long-default-secret-key");
        let token = jwt.issue(&user()).unwrap();
        let header = jsonwebtoken::decode_header(&token).unwrap();
        assert_eq!(header.alg, jsonwebtoken::Algorithm::HS256);
        let plain = base64_decode_middle(&token);
        let claims: serde_json::Value = serde_json::from_str(&plain).unwrap();
        assert_eq!(claims["sub"], user().id);
        assert_eq!(claims["name"], "alice");
        assert_eq!(claims["display_name"], "Alice");
        assert!(claims["exp"].as_i64().unwrap() > claims["iat"].as_i64().unwrap());
    }

    #[test]
    fn rejects_tampered_foreign_and_expired_tokens() {
        let signer = jwt("this-is-a-32-char-long-default-secret-key");
        let token = signer.issue(&user()).unwrap();
        let mut tampered = token.clone();
        tampered.pop();
        assert_eq!(
            signer.verify(&tampered).unwrap_err().code().code(),
            "AUTH002"
        );

        let other = jwt("a-completely-different-32-byte-secret!!");
        assert!(other.verify(&token).is_err());

        let expired = {
            let claims = JwtClaims {
                sub: user().id,
                name: "alice".into(),
                display_name: None,
                iat: 1_600_000_000,
                exp: 1_600_000_001,
            };
            jsonwebtoken::encode(
                &jsonwebtoken::Header::default(),
                &claims,
                &signer.encoding_key(),
            )
            .unwrap()
        };
        let err = signer.verify(&expired).unwrap_err();
        assert_eq!(err.status(), 401);
    }

    #[test]
    fn whitespace_around_the_token_is_tolerated() {
        let jwt = jwt("this-is-a-32-char-long-default-secret-key");
        let token = jwt.issue(&user()).unwrap();
        assert!(jwt.verify(&format!("  {token}\n")).is_ok());
    }

    fn base64_decode_middle(token: &str) -> String {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
        let payload = token.split('.').nth(1).expect("jwt payload");
        String::from_utf8(URL_SAFE_NO_PAD.decode(payload).expect("base64url payload")).unwrap()
    }
}
