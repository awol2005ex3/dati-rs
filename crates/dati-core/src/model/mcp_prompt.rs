//! The prompts of an MCP service: a text template with its declared parameters, served to an MCP
//! client through `prompts/list`.

use serde::{Deserialize, Serialize};

use super::base::Audit;

/// One declared argument. `required` is a plain boolean, so an omitted key reads as `false` exactly
/// like the Java field initializer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PromptParameter {
    pub name: Option<String>,
    pub description: Option<String>,
    pub required: bool,
}

/// A stored prompt. The parameters live in one JSON column, as they did in the Java `McpPromptPO`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpPrompt {
    pub id: String,
    pub service_id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub enabled: bool,
    pub content: Option<String>,
    pub parameters: Vec<PromptParameter>,
    pub audit: Audit,
}

/// The `POST` and `PUT` body. Every field is optional because the Java controller bound it without
/// `@Valid`: an update sends the fields it wants to move, and the rest keep their stored value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct McpPromptRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub enabled: Option<bool>,
    pub content: Option<String>,
    pub parameters: Option<Vec<PromptParameter>>,
}

/// The `McpPromptVO`: unlike the service view it carries no audit columns at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct McpPromptView {
    pub id: String,
    pub service_id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub enabled: bool,
    pub content: Option<String>,
    pub parameters: Vec<PromptParameter>,
}

impl McpPromptView {
    pub fn new(prompt: &McpPrompt) -> Self {
        Self {
            id: prompt.id.clone(),
            service_id: prompt.service_id.clone(),
            name: prompt.name.clone(),
            description: prompt.description.clone(),
            enabled: prompt.enabled,
            content: prompt.content.clone(),
            parameters: prompt.parameters.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prompt_body_reads_the_shape_the_frontend_sends() {
        let body: McpPromptRequest = serde_json::from_value(serde_json::json!({
            "name": "weekly_report",
            "description": "周报",
            "content": "统计 {{city:全国}} 本周订单 {{#if limit}}LIMIT {{limit}}{{/if}}",
            "parameters": [{"name": "city"}, {"name": "limit", "required": true, "description": "d"}],
        }))
        .unwrap();
        assert_eq!(
            body.enabled, None,
            "an absent toggle means leave the stored value alone"
        );
        assert_eq!(body.parameters.as_deref().unwrap().len(), 2);
        assert_eq!(
            body.parameters.as_deref().unwrap()[1],
            PromptParameter {
                name: Some("limit".into()),
                description: Some("d".into()),
                required: true,
            }
        );
        assert!(!body.parameters.as_deref().unwrap()[0].required);

        // A partial update leaves `parameters` null rather than empty; the difference is load-bearing.
        let partial: McpPromptRequest =
            serde_json::from_value(serde_json::json!({"enabled": false})).unwrap();
        assert_eq!(partial.parameters, None);
        assert_eq!(partial.name, None);
    }

    #[test]
    fn the_view_serializes_the_stored_prompt() {
        let prompt = McpPrompt {
            id: "p-1".into(),
            service_id: "svc-1".into(),
            name: Some("weekly_report".into()),
            description: None,
            enabled: true,
            content: Some("SELECT 1".into()),
            parameters: vec![PromptParameter {
                name: Some("city".into()),
                description: None,
                required: false,
            }],
            audit: Audit::new("u-1"),
        };
        let json = serde_json::to_value(McpPromptView::new(&prompt)).unwrap();
        assert_eq!(json["service_id"], "svc-1");
        assert_eq!(json["enabled"], true);
        assert_eq!(json["parameters"][0]["name"], "city");
        assert_eq!(json["parameters"][0]["required"], false);
        assert!(
            json.get("created_at").is_none(),
            "the Java VO has no audit fields"
        );
        assert_eq!(json.as_object().unwrap().len(), 7);
    }
}
