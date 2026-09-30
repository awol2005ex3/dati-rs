//! Password hashing. Spring's `BCryptPasswordEncoder()` used cost 10, so hashes from the Java
//! deployment verify unchanged here.

use crate::error::{DatiError, ErrorCode, Result};

pub const BCRYPT_COST: u32 = 10;

pub fn hash(raw: &str) -> Result<String> {
    bcrypt::hash(raw, BCRYPT_COST).map_err(|err| {
        tracing::error!(error = %err, "password hashing failed");
        DatiError::with_message(ErrorCode::InternalError, "Internal server error")
    })
}

/// `false` for a malformed stored hash rather than an error: the caller returns a generic 401.
pub fn verify(raw: &str, stored: &str) -> bool {
    match bcrypt::verify(raw, stored) {
        Ok(ok) => ok,
        Err(err) => {
            tracing::debug!(error = %err, "stored password hash is not verifiable");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_are_bcrypt_cost_10_and_verifiable() {
        let hashed = hash("demo123").unwrap();
        assert!(hashed.starts_with("$2b$10$"), "unexpected prefix: {hashed}");
        assert_eq!(hashed.len(), 60);
        assert!(verify("demo123", &hashed));
        assert!(!verify("demo124", &hashed));
    }

    #[test]
    fn accepts_the_dollar_2a_prefix_that_spring_security_writes() {
        // Spring's BCryptPasswordEncoder emits `$2a$`; for ASCII passwords under 72 bytes the
        // `$2a$` and `$2b$` variants agree, so re-labelling a locally made hash must still verify.
        let hashed = hash("demo123").unwrap();
        assert!(hashed.starts_with("$2b$10$"));
        let spring_style = hashed.replacen("$2b$", "$2a$", 1);
        assert!(verify("demo123", &spring_style));
        assert!(!verify("demo1234", &spring_style));
    }

    #[test]
    fn garbage_stored_hash_is_not_an_error() {
        assert!(!verify("anything", "not-a-bcrypt-hash"));
        assert!(!verify("anything", ""));
    }

    #[test]
    fn same_password_hashes_differently() {
        assert_ne!(hash("pw").unwrap(), hash("pw").unwrap());
    }
}
