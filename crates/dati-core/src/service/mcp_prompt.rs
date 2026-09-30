//! The prompts a service exposes through `prompts/list`: a text template plus its parameters.

use crate::auth::AuthUser;
use crate::db::Db;
use crate::error::{DatiError, ErrorCode, Result};
use crate::model::base::Audit;
use crate::model::mcp_prompt::{McpPrompt, McpPromptRequest, McpPromptView, PromptParameter};
use crate::model::permission::Permission;
use crate::repo::mcp_prompt;
use crate::template;
use crate::time::new_id;

use super::mcp_service::McpServiceService;

#[derive(Clone)]
pub struct McpPromptService {
    db: Db,
    services: McpServiceService,
}

impl McpPromptService {
    pub fn new(db: Db, services: McpServiceService) -> Self {
        Self { db, services }
    }

    /// `GET /v1/mcp-services/{id}/prompts`, newest first.
    pub async fn list(&self, caller: &AuthUser, service_id: &str) -> Result<Vec<McpPromptView>> {
        Ok(self
            .prompts(caller, service_id)
            .await?
            .iter()
            .map(McpPromptView::new)
            .collect())
    }

    /// The stored rows, for the publish snapshot and the endpoint.
    pub async fn prompts(&self, caller: &AuthUser, service_id: &str) -> Result<Vec<McpPrompt>> {
        self.services
            .authorize(caller, service_id, Permission::View)
            .await?;
        mcp_prompt::list(&self.db, service_id).await
    }

    /// The same read for a caller who was already checked, which is what the publish path is.
    pub async fn stored(&self, service_id: &str) -> Result<Vec<McpPrompt>> {
        mcp_prompt::list(&self.db, service_id).await
    }

    /// `POST /v1/mcp-services/{id}/prompts`. Java bound this body without `@Valid`, so an absent
    /// name or content reaches the store and the NOT NULL column answers; nothing else is checked
    /// before the template is.
    pub async fn create(
        &self,
        caller: &AuthUser,
        service_id: &str,
        request: &McpPromptRequest,
    ) -> Result<String> {
        self.services
            .authorize(caller, service_id, Permission::Edit)
            .await?;
        let name = request.name.clone().unwrap_or_default();
        if mcp_prompt::name_taken(&self.db, service_id, &name, None).await? {
            return Err(DatiError::new(ErrorCode::MsPromptNameExists, [name]));
        }
        let prompt = McpPrompt {
            id: new_id(),
            service_id: service_id.to_string(),
            name: Some(name),
            description: request.description.clone(),
            // The Java assembler read an absent flag as enabled and then ignored the request field
            // altogether, so a create cannot start out disabled.
            enabled: true,
            content: request.content.clone(),
            parameters: request.parameters.clone().unwrap_or_default(),
            audit: Audit::new(&caller.id),
        };
        validate_content_params(prompt.content.as_deref(), &prompt.parameters)?;
        mcp_prompt::insert(&self.db, &prompt).await
    }

    /// `PUT /v1/mcp-services/{id}/prompts/{promptId}`: every field moves only when the body names
    /// it, and `enabled` — unlike the tool update — keeps its stored value when the body omits it.
    pub async fn update(
        &self,
        caller: &AuthUser,
        service_id: &str,
        prompt_id: &str,
        request: &McpPromptRequest,
    ) -> Result<()> {
        self.services
            .authorize(caller, service_id, Permission::Edit)
            .await?;
        let mut prompt = mcp_prompt::find(&self.db, service_id, prompt_id)
            .await?
            .ok_or_else(|| DatiError::new(ErrorCode::MsPromptNotFound, [prompt_id]))?;

        if let Some(name) = &request.name
            && prompt.name.as_deref() != Some(name.as_str())
            && mcp_prompt::name_taken(&self.db, service_id, name, Some(prompt_id)).await?
        {
            return Err(DatiError::new(
                ErrorCode::MsPromptNameExists,
                [name.clone()],
            ));
        }
        // The Java service validated the body it was given, with the stored parameters filled in, so
        // an update that changes only the parameters is checked against the content already stored.
        let parameters = match &request.parameters {
            Some(parameters) => parameters.clone(),
            None => prompt.parameters.clone(),
        };
        validate_content_params(request.content.as_deref(), &parameters)?;

        if let Some(name) = &request.name {
            prompt.name = Some(name.clone());
        }
        if let Some(description) = &request.description {
            prompt.description = Some(description.clone());
        }
        if let Some(content) = &request.content {
            prompt.content = Some(content.clone());
        }
        if let Some(parameters) = &request.parameters {
            prompt.parameters = parameters.clone();
        }
        if let Some(enabled) = request.enabled {
            prompt.enabled = enabled;
        }
        prompt.audit.touch(&caller.id);
        mcp_prompt::update(&self.db, &prompt).await
    }

    /// `DELETE /v1/mcp-services/{id}/prompts/{promptId}`, which removes the row for good.
    pub async fn remove(&self, caller: &AuthUser, service_id: &str, prompt_id: &str) -> Result<()> {
        self.services
            .authorize(caller, service_id, Permission::Edit)
            .await?;
        if !mcp_prompt::remove(&self.db, service_id, prompt_id).await? {
            return Err(DatiError::new(ErrorCode::MsPromptNotFound, [prompt_id]));
        }
        Ok(())
    }
}

/// The template syntax plus the two directions of the parameter match. A content the body leaves out
/// is not validated at all, which is how the Java early return let a toggle through.
fn validate_content_params(content: Option<&str>, parameters: &[PromptParameter]) -> Result<()> {
    let Some(content) = content else {
        return Ok(());
    };
    let compiled = template::parse(content).map_err(|message| {
        DatiError::new(
            ErrorCode::MsTemplateSyntaxError,
            ["content", message.as_str()],
        )
    })?;

    let defined: Vec<&str> = parameters
        .iter()
        .filter_map(|parameter| parameter.name.as_deref())
        .filter(|name| !name.trim().is_empty())
        .collect();
    let used = compiled.variables();

    // Both lists keep the order the template or the body gave them; Java collected them through a
    // `HashSet`, whose order no client could predict.
    let unused: Vec<&str> = defined
        .iter()
        .copied()
        .filter(|name| !used.iter().any(|var| var == name))
        .collect();
    if !unused.is_empty() {
        return Err(DatiError::new(
            ErrorCode::MsPromptArgMismatch,
            [format!(
                "Unused parameter(s) not referenced in content: {}",
                unused.join(", ")
            )],
        ));
    }
    let undefined: Vec<String> = used
        .into_iter()
        .filter(|name| !defined.contains(&name.as_str()))
        .collect();
    if !undefined.is_empty() {
        return Err(DatiError::new(
            ErrorCode::MsPromptArgMismatch,
            [format!(
                "Unknown parameter(s) in content: {}",
                undefined.join(", ")
            )],
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::datasource::DataSourceService;
    use super::super::mcp_service::McpServiceService;
    use super::super::permission::PermissionService;
    use super::super::subject::SubjectService;
    use super::*;
    use crate::config::Config;
    use crate::model::base::Audit;
    use crate::model::datasource::{DataSource, DbType};
    use crate::model::mcp::{DataScopeItem, McpDataScopeType, McpServiceCreateRequest};
    use crate::repo::datasource;

    async fn store() -> (Db, McpServiceService, McpPromptService) {
        let db = Db::sqlite_temp().await;
        sqlx::query(
            "INSERT INTO users (id, name, password) \
                     VALUES ('u-1', 'alice', 'x'), ('u-2', 'bob', 'x')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        let config = Config::from_source(|_| None);
        let permissions = PermissionService::new(&config);
        let data_sources = DataSourceService::new(db.clone(), &config, permissions.clone());
        let subjects = SubjectService::new(db.clone(), data_sources.clone());
        let services = McpServiceService::new(db.clone(), data_sources.clone(), subjects, &config);
        let prompts = McpPromptService::new(db.clone(), services.clone());
        (db, services, prompts)
    }

    fn caller(id: &str, name: &str) -> AuthUser {
        AuthUser {
            id: id.to_string(),
            name: name.to_string(),
            display_name: None,
        }
    }

    async fn service(db: &Db, services: &McpServiceService, alice: &AuthUser) -> String {
        let source = DataSource {
            id: "d-1".into(),
            name: Some("warehouse".into()),
            description: None,
            deleted: false,
            connection_url: Some("sqlite:demo.db".into()),
            db_type: DbType::Sqlite,
            user_name: Some("none".into()),
            password: None,
            default_schema: Some("main".into()),
            audit: Audit::new("u-1"),
        };
        datasource::insert(db, &source).await.unwrap();
        services
            .create(
                alice,
                &McpServiceCreateRequest {
                    code: Some("sales".into()),
                    name: Some("销售服务".into()),
                    description: None,
                    data_scopes: vec![DataScopeItem {
                        id: None,
                        scope_type: Some(McpDataScopeType::DataSource),
                        reference_id: Some("d-1".into()),
                        reference_name: None,
                    }],
                },
            )
            .await
            .unwrap()
    }

    fn body(name: &str, content: &str, parameters: &[&str]) -> McpPromptRequest {
        McpPromptRequest {
            name: Some(name.into()),
            description: Some("周报".into()),
            enabled: None,
            content: Some(content.into()),
            parameters: Some(
                parameters
                    .iter()
                    .map(|parameter| PromptParameter {
                        name: Some((*parameter).into()),
                        ..Default::default()
                    })
                    .collect(),
            ),
        }
    }

    #[tokio::test]
    async fn a_prompt_starts_enabled_and_round_trips_its_parameters() {
        let (db, services, prompts) = store().await;
        let alice = caller("u-1", "alice");
        let id = service(&db, &services, &alice).await;

        let created = prompts
            .create(
                &alice,
                &id,
                &body("weekly", "统计 {{city}} 的订单", &["city"]),
            )
            .await
            .unwrap();
        let list = prompts.list(&alice, &id).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, created);
        assert!(
            list[0].enabled,
            "a create ignores the body flag and starts enabled"
        );
        assert_eq!(list[0].parameters[0].name.as_deref(), Some("city"));

        // Nothing was validated before the store, so a body with no content reaches the NOT NULL
        // column and the database answers — exactly like the Java `@Valid`-less binding.
        let err = prompts
            .create(
                &alice,
                &id,
                &McpPromptRequest {
                    name: Some("no_content".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::InternalError);
        assert_eq!(prompts.list(&alice, &id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn content_and_parameters_must_agree() {
        let (db, services, prompts) = store().await;
        let alice = caller("u-1", "alice");
        let id = service(&db, &services, &alice).await;

        // A declared parameter the content never uses.
        let err = prompts
            .create(&alice, &id, &body("weekly", "统计订单", &["city"]))
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::MsPromptArgMismatch);
        assert_eq!(
            err.message(),
            "Prompt parameter mismatch: Unused parameter(s) not referenced in content: city"
        );

        // A content variable the author never declared.
        let err = prompts
            .create(&alice, &id, &body("weekly", "统计 {{city}} 的订单", &[]))
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::MsPromptArgMismatch);
        assert_eq!(
            err.message(),
            "Prompt parameter mismatch: Unknown parameter(s) in content: city"
        );

        // A template that will not parse.
        let err = prompts
            .create(&alice, &id, &body("weekly", "统计 {{city", &["city"]))
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::MsTemplateSyntaxError);

        // A duplicate name in the same service.
        prompts
            .create(
                &alice,
                &id,
                &body("weekly", "统计 {{city}} 的订单", &["city"]),
            )
            .await
            .unwrap();
        let err = prompts
            .create(&alice, &id, &body("weekly", "另一段 {{city}}", &["city"]))
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::MsPromptNameExists);
        assert_eq!(
            err.message(),
            "Prompt name already exists in this service: weekly"
        );
    }

    #[tokio::test]
    async fn an_update_moves_only_the_named_fields() {
        let (db, services, prompts) = store().await;
        let alice = caller("u-1", "alice");
        let id = service(&db, &services, &alice).await;
        let prompt_id = prompts
            .create(
                &alice,
                &id,
                &body("weekly", "统计 {{city}} 的订单", &["city"]),
            )
            .await
            .unwrap();

        // Toggling `enabled` alone validates against the stored content and parameters, so it passes.
        prompts
            .update(
                &alice,
                &id,
                &prompt_id,
                &McpPromptRequest {
                    enabled: Some(false),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let stored = prompts.list(&alice, &id).await.unwrap().remove(0);
        assert!(!stored.enabled);
        assert_eq!(stored.name.as_deref(), Some("weekly"));

        // Changing the content is checked against the stored parameters: a variable they never
        // declared fails, even though the stored `city` is still used.
        let err = prompts
            .update(
                &alice,
                &id,
                &prompt_id,
                &McpPromptRequest {
                    content: Some("统计 {{city}} 和 {{region}} 的订单".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::MsPromptArgMismatch);
        assert_eq!(
            err.message(),
            "Prompt parameter mismatch: Unknown parameter(s) in content: region"
        );

        // The unknown prompt.
        let err = prompts
            .update(&alice, &id, "gone", &McpPromptRequest::default())
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::MsPromptNotFound);
    }

    #[tokio::test]
    async fn a_prompt_can_be_deleted_and_only_its_owner_reads_it() {
        let (db, services, prompts) = store().await;
        let alice = caller("u-1", "alice");
        let bob = caller("u-2", "bob");
        let id = service(&db, &services, &alice).await;
        let prompt_id = prompts
            .create(
                &alice,
                &id,
                &body("weekly", "统计 {{city}} 的订单", &["city"]),
            )
            .await
            .unwrap();

        assert_eq!(
            prompts.list(&bob, &id).await.unwrap_err().code(),
            ErrorCode::PermissionDenied
        );
        prompts.remove(&alice, &id, &prompt_id).await.unwrap();
        assert!(prompts.list(&alice, &id).await.unwrap().is_empty());
        assert_eq!(
            prompts
                .remove(&alice, &id, &prompt_id)
                .await
                .unwrap_err()
                .code(),
            ErrorCode::MsPromptNotFound
        );
    }
}
