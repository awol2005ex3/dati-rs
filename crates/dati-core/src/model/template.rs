//! The bodies of `/v1/template/preview` and `/v1/template/extract`, the two endpoints the custom
//! tool editor calls while an author types a template.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{DatiError, Result};

/// Which renderer the preview runs: text inlines values, SQL keeps them as bindings and shows them
/// inlined only for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum TemplateRenderMode {
    #[serde(rename = "TEXT")]
    Text,
    #[serde(rename = "SQL")]
    Sql,
}

/// `POST /v1/template/preview`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TemplatePreviewRequest {
    pub mode: Option<TemplateRenderMode>,
    pub template: Option<String>,
    pub values: Option<Map<String, Value>>,
}

impl TemplatePreviewRequest {
    /// The Java `@NotNull mode`, `@NotBlank template`, `@NotNull values`, in field order.
    pub fn validate(&self) -> Result<()> {
        let mut fields = Vec::new();
        if self.mode.is_none() {
            fields.push(("mode", "must not be null"));
        }
        if self
            .template
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
        {
            fields.push(("template", "must not be blank"));
        }
        if self.values.is_none() {
            fields.push(("values", "must not be null"));
        }
        if fields.is_empty() {
            return Ok(());
        }
        Err(DatiError::validation(&fields))
    }
}

/// `POST /v1/template/extract`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TemplateExtractRequest {
    pub template: Option<String>,
}

impl TemplateExtractRequest {
    pub fn validate(&self) -> Result<()> {
        if self
            .template
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
        {
            return Err(DatiError::validation(&[("template", "must not be blank")]));
        }
        Ok(())
    }
}

/// The rendered text, or the display-only SQL with its values inlined.
#[derive(Debug, Clone, Serialize)]
pub struct TemplatePreviewResponse {
    pub rendered: String,
}

/// The variables an author still has to declare as parameters.
#[derive(Debug, Clone, Serialize)]
pub struct TemplateExtractResponse {
    pub variables: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preview_body_reports_every_missing_field_at_once() {
        let err = TemplatePreviewRequest::default().validate().unwrap_err();
        assert_eq!(
            err.message(),
            "Request parameter error: mode: must not be null; template: must not be blank; \
             values: must not be null"
        );

        let body: TemplatePreviewRequest = serde_json::from_value(serde_json::json!({
            "mode": "SQL",
            "template": "SELECT * FROM t WHERE c = {{city}}",
            "values": {"city": "上海"},
        }))
        .unwrap();
        body.validate().unwrap();
        assert_eq!(body.mode, Some(TemplateRenderMode::Sql));
        assert_eq!(
            body.values.as_ref().unwrap()["city"],
            Value::String("上海".into())
        );

        // An empty object is a value set; only an absent key is rejected.
        let body: TemplatePreviewRequest = serde_json::from_value(
            serde_json::json!({"mode": "TEXT", "template": "x", "values": {}}),
        )
        .unwrap();
        body.validate().unwrap();
        assert!(body.values.as_ref().unwrap().is_empty());

        serde_json::from_value::<TemplatePreviewRequest>(
            serde_json::json!({"mode": "JSON", "template": "x", "values": {}}),
        )
        .unwrap_err();

        let err = TemplateExtractRequest::default().validate().unwrap_err();
        assert_eq!(
            err.message(),
            "Request parameter error: template: must not be blank"
        );
    }
}
