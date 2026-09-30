//! Accounts, credentials and sessions: the logic behind `/v1/auth`, `/v1/auth/api-keys` and
//! `/v1/users/search`.

use crate::auth::apikey::{ApiKeySecret, bearer_token};
use crate::auth::{AuthUser, Jwt, password, unauthorized};
use crate::config::Config;
use crate::db::Db;
use crate::error::{DatiError, ErrorCode, Result};
use crate::model::base::Audit;
use crate::model::user::{
    ApiKey, ApiKeyCreateRequest, ApiKeyView, CreatedApiKey, LoginRequest, LoginResponse,
    RegisterRequest, User, UserBrief,
};
use crate::repo::{api_key, user as users};
use crate::time::{new_id, plus_days};

/// The only authentication type the Java `LoginRequest.type` accepted when local auth was enabled.
pub const LOCAL_LOGIN_TYPE: &str = "local";

/// The picker in the grant dialog lists matches; the Java query had no cap, this one bounds the
/// payload instead of returning an unbounded user list.
const USER_SEARCH_LIMIT: i64 = 100;

#[derive(Clone)]
pub struct AuthService {
    db: Db,
    config: Config,
    jwt: Jwt,
}

impl AuthService {
    pub fn new(db: Db, config: Config) -> Self {
        let jwt = Jwt::new(&config);
        Self { db, config, jwt }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    /// `POST /v1/auth/login`. Unknown user and wrong password answer identically, so the endpoint
    /// cannot be used to enumerate names.
    pub async fn login(&self, request: &LoginRequest) -> Result<LoginResponse> {
        request.validate()?;
        let kind = request.r#type.clone().unwrap_or_default();
        if kind != LOCAL_LOGIN_TYPE {
            return Err(DatiError::new(ErrorCode::AuthTypeUnsupported, [kind]));
        }
        let name = request.name.clone().unwrap_or_default();
        let stored = users::find_by_name(&self.db, &name)
            .await?
            .ok_or_else(login_failed)?;
        if !password::verify(
            &request.password.clone().unwrap_or_default(),
            &stored.password,
        ) {
            return Err(login_failed());
        }
        Ok(LoginResponse {
            token: self.jwt.issue(&AuthUser::from_user(&stored))?,
        })
    }

    /// `POST /v1/auth/register`. Anyone may register: administrators come from configuration only,
    /// so the first account is an ordinary one exactly as in the Java service.
    pub async fn register(&self, request: &RegisterRequest) -> Result<String> {
        request.validate()?;
        let name = request.name.clone().unwrap_or_default();
        if users::find_by_name(&self.db, &name).await?.is_some() {
            return Err(DatiError::new(ErrorCode::AuthUserExists, [&name]));
        }
        let user = User {
            id: new_id(),
            name,
            password: password::hash(&request.password.clone().unwrap_or_default())?,
            display_name: request.display_name.clone(),
            audit: Audit::timestamped(),
        };
        users::insert(&self.db, &user).await?;
        Ok(user.id)
    }

    /// Creates the configured administrators on a store that has no account for them yet, so the
    /// single shipped binary has something to log in with. Accounts that exist are left untouched:
    /// that is what lets an operator change the seeded password and keep it across restarts.
    pub async fn ensure_admins(&self) -> Result<Vec<String>> {
        let mut created = Vec::new();
        for name in &self.config.admin_users {
            if users::find_by_name(&self.db, name).await?.is_some() {
                continue;
            }
            let user = User {
                id: new_id(),
                name: name.clone(),
                password: password::hash(&self.config.admin_password)?,
                display_name: None,
                audit: Audit::timestamped(),
            };
            users::insert(&self.db, &user).await?;
            created.push(name.clone());
        }
        Ok(created)
    }

    /// Resolves an `Authorization: Bearer …` header into an identity. A key is recognised by its
    /// `sk_` prefix, so it never reaches the JWT verifier, matching the two Java providers.
    pub async fn authenticate(&self, header: Option<&str>) -> Result<AuthUser> {
        let Some(token) = bearer_token(header) else {
            return Err(unauthorized());
        };
        if !ApiKeySecret::looks_like_api_key(token) {
            return self.jwt.verify(token);
        }
        let key = api_key::find_by_hash(
            &self.db,
            &ApiKeySecret::from_plaintext(token.to_string()).hash,
        )
        .await?
        .ok_or_else(unauthorized)?;
        let now = crate::time::now_iso();
        if key.expired(&now) {
            return Err(unauthorized());
        }
        let owner = users::find_by_id(&self.db, &key.user_id)
            .await?
            .ok_or_else(unauthorized)?;
        api_key::touch_used(&self.db, &key.id, &now).await;
        Ok(AuthUser::from_user(&owner))
    }

    /// `POST /v1/auth/api-keys`: the plaintext is returned here and never stored or shown again.
    pub async fn create_api_key(
        &self,
        caller: &AuthUser,
        request: &ApiKeyCreateRequest,
    ) -> Result<CreatedApiKey> {
        request.validate()?;
        let days = ApiKeySecret::validate_expiry_days(request.expires_in_days)?;
        let secret = ApiKeySecret::generate();
        let key = ApiKey {
            id: new_id(),
            user_id: caller.id.clone(),
            name: request.name.clone().unwrap_or_default().trim().to_string(),
            key_hash: secret.hash,
            key_mask: secret.mask,
            expires_at: plus_days(days),
            last_used_at: None,
            audit: Audit::timestamped(),
        };
        api_key::insert(&self.db, &key).await?;
        Ok(CreatedApiKey {
            id: key.id.clone(),
            name: key.name,
            key: secret.plaintext,
            key_mask: key.key_mask,
            expires_at: key.expires_at,
        })
    }

    pub async fn list_api_keys(&self, caller: &AuthUser) -> Result<Vec<ApiKeyView>> {
        Ok(api_key::list_for_user(&self.db, &caller.id)
            .await?
            .iter()
            .map(ApiKeyView::from)
            .collect())
    }

    /// `DELETE /v1/auth/api-keys/{id}`: a missing key is a no-op, another user's key is forbidden.
    pub async fn delete_api_key(&self, caller: &AuthUser, id: &str) -> Result<()> {
        match api_key::find_by_id(&self.db, id).await? {
            None => Ok(()),
            Some(key) if key.user_id == caller.id => {
                api_key::delete(&self.db, id).await?;
                Ok(())
            }
            Some(_) => Err(DatiError::with_message(
                ErrorCode::AuthKeyForbidden,
                ErrorCode::AuthKeyForbidden.template(),
            )),
        }
    }

    /// `GET /v1/users/search`: blank input answers `[]` rather than dumping the user table.
    pub async fn search_users(&self, keyword: &str) -> Result<Vec<UserBrief>> {
        let keyword = keyword.trim();
        if keyword.is_empty() {
            return Ok(Vec::new());
        }
        Ok(users::search(&self.db, keyword, USER_SEARCH_LIMIT)
            .await?
            .iter()
            .map(User::brief)
            .collect())
    }
}

fn login_failed() -> DatiError {
    DatiError::with_message(
        ErrorCode::AuthLoginFailed,
        ErrorCode::AuthLoginFailed.template(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_ADMIN_PASSWORD;

    async fn service() -> AuthService {
        AuthService::new(
            Db::sqlite_temp().await,
            Config::from_source(|_key| None::<String>),
        )
    }

    fn login(name: &str, password: &str) -> LoginRequest {
        LoginRequest {
            r#type: Some(LOCAL_LOGIN_TYPE.to_string()),
            name: Some(name.to_string()),
            password: Some(password.to_string()),
        }
    }

    fn register(name: &str, password: &str) -> RegisterRequest {
        RegisterRequest {
            name: Some(name.to_string()),
            password: Some(password.to_string()),
            display_name: Some(name.to_string()),
        }
    }

    async fn service_from(pairs: &[(&str, &str)]) -> AuthService {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        AuthService::new(
            Db::sqlite_temp().await,
            Config::from_source(|key| map.get(key).cloned()),
        )
    }

    #[tokio::test]
    async fn seeding_creates_the_configured_administrators_once() {
        let service = service().await;
        assert_eq!(
            service.ensure_admins().await.unwrap(),
            vec!["admin".to_string()]
        );
        assert!(service.ensure_admins().await.unwrap().is_empty());
        assert!(
            service
                .login(&login("admin", DEFAULT_ADMIN_PASSWORD))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn seeding_leaves_an_existing_account_alone() {
        let service = service().await;
        service
            .register(&register("admin", "personal"))
            .await
            .unwrap();
        assert!(service.ensure_admins().await.unwrap().is_empty());
        // The operator's own password survives; the built-in default never takes the account over.
        assert!(service.login(&login("admin", "personal")).await.is_ok());
        assert!(
            service
                .login(&login("admin", DEFAULT_ADMIN_PASSWORD))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn seeding_follows_admin_users_and_admin_password() {
        let service =
            service_from(&[("ADMIN_USERS", "root,ops"), ("ADMIN_PASSWORD", "s3cret")]).await;
        assert_eq!(
            service.ensure_admins().await.unwrap(),
            vec!["root".to_string(), "ops".to_string()]
        );
        assert!(service.login(&login("root", "s3cret")).await.is_ok());
        assert!(service.login(&login("ops", "s3cret")).await.is_ok());
        assert!(service.config().is_admin("ops"));
    }

    #[tokio::test]
    async fn registration_then_login_issues_a_verifiable_token() {
        let service = service().await;
        let id = service
            .register(&register("alice", "wonderland"))
            .await
            .unwrap();
        assert_eq!(id.len(), 36);

        let response = service.login(&login("alice", "wonderland")).await.unwrap();
        let user = service
            .authenticate(Some(&format!("Bearer {}", response.token)))
            .await
            .unwrap();
        assert_eq!(user.id, id);
        assert_eq!(user.name, "alice");
        assert_eq!(user.display_name.as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn wrong_password_and_unknown_user_are_the_same_answer() {
        let service = service().await;
        service
            .register(&register("alice", "wonderland"))
            .await
            .unwrap();
        for request in [login("alice", "nope"), login("nobody", "wonderland")] {
            let err = service.login(&request).await.unwrap_err();
            assert_eq!(err.code(), ErrorCode::AuthLoginFailed);
            assert_eq!(err.status(), 401);
            assert_eq!(err.message(), "Authentication failed: invalid credentials");
        }
    }

    #[tokio::test]
    async fn duplicate_names_blanks_and_unknown_types_are_rejected() {
        let service = service().await;
        service
            .register(&register("alice", "wonderland"))
            .await
            .unwrap();

        let err = service
            .register(&register("alice", "elsewhere"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::AuthUserExists);
        assert_eq!(err.status(), 409);
        assert_eq!(err.message(), "Username already exists: alice");

        let blank = LoginRequest {
            r#type: Some(LOCAL_LOGIN_TYPE.to_string()),
            name: Some("  ".to_string()),
            password: None,
        };
        assert_eq!(
            service.login(&blank).await.unwrap_err().message(),
            "Request parameter error: name: must not be blank; password: must not be blank"
        );

        let mut foreign = login("alice", "wonderland");
        foreign.r#type = Some("ldap".to_string());
        let err = service.login(&foreign).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::AuthTypeUnsupported);
        assert_eq!(err.message(), "Unsupported authentication type: ldap");
    }

    #[tokio::test]
    async fn api_keys_are_plain_once_masked_everywhere_and_revocable() {
        let service = service().await;
        service
            .register(&register("alice", "wonderland"))
            .await
            .unwrap();
        let token = service
            .login(&login("alice", "wonderland"))
            .await
            .unwrap()
            .token;
        let caller = service
            .authenticate(Some(&format!("Bearer {token}")))
            .await
            .unwrap();

        let created = service
            .create_api_key(
                &caller,
                &ApiKeyCreateRequest {
                    name: Some("cli".to_string()),
                    expires_in_days: Some(7),
                },
            )
            .await
            .unwrap();
        assert!(created.key.starts_with("sk_"));
        assert_eq!(created.key_mask.matches('*').count(), 3);
        assert!(created.key.len() > created.key_mask.len());

        let user = service
            .authenticate(Some(&format!("Bearer {}", created.key)))
            .await
            .unwrap();
        assert_eq!(user.id, caller.id);

        let listed = service.list_api_keys(&caller).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].key_mask, created.key_mask);
        assert!(
            !serde_json::to_string(&listed)
                .unwrap()
                .contains(&created.key),
            "the plaintext is shown once, at creation"
        );
        assert!(
            listed[0].last_used_at.is_some(),
            "the login above recorded a use"
        );

        service.delete_api_key(&caller, &created.id).await.unwrap();
        assert!(service.list_api_keys(&caller).await.unwrap().is_empty());
        assert!(
            service
                .authenticate(Some(&format!("Bearer {}", created.key)))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn keys_belong_to_their_owner_and_expiry_is_a_short_list() {
        let service = service().await;
        service
            .register(&register("alice", "wonderland"))
            .await
            .unwrap();
        service.register(&register("bob", "thepool")).await.unwrap();
        let alice = principal(&service, "alice").await;
        let bob = principal(&service, "bob").await;

        let created = service
            .create_api_key(
                &alice,
                &ApiKeyCreateRequest {
                    name: Some("k".into()),
                    expires_in_days: None,
                },
            )
            .await
            .unwrap();
        let err = service.delete_api_key(&bob, &created.id).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::AuthKeyForbidden);
        assert_eq!(err.status(), 403);
        assert!(service.delete_api_key(&bob, "never-existed").await.is_ok());

        let err = service
            .create_api_key(
                &alice,
                &ApiKeyCreateRequest {
                    name: Some("k".into()),
                    expires_in_days: Some(15),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::AuthKeyInvalidExpiry);
        assert_eq!(err.status(), 400);
        assert_eq!(
            service
                .create_api_key(
                    &alice,
                    &ApiKeyCreateRequest {
                        name: Some(String::new()),
                        expires_in_days: None
                    }
                )
                .await
                .unwrap_err()
                .code(),
            ErrorCode::InvalidParameter
        );
    }

    #[tokio::test]
    async fn garbage_credentials_and_search_edges() {
        let service = service().await;
        service
            .register(&register("alice", "wonderland"))
            .await
            .unwrap();
        for header in [None, Some(""), Some("Basic abc"), Some("Bearer not-a-jwt")] {
            assert_eq!(
                service.authenticate(header).await.unwrap_err().code(),
                ErrorCode::AuthTokenInvalid,
                "{header:?} must not authenticate"
            );
        }
        assert_eq!(service.search_users("   ").await.unwrap().len(), 0);
        assert_eq!(service.search_users("LIC").await.unwrap().len(), 1);
        assert_eq!(service.search_users("nobody").await.unwrap().len(), 0);
    }

    async fn principal(service: &AuthService, name: &str) -> AuthUser {
        let token = service
            .login(&login(
                name,
                if name == "alice" {
                    "wonderland"
                } else {
                    "thepool"
                },
            ))
            .await
            .unwrap()
            .token;
        service
            .authenticate(Some(&format!("Bearer {token}")))
            .await
            .unwrap()
    }
}
