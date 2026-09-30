//! `/v1/template/preview` and `/v1/template/extract`: what the custom-tool editor shows while an
//! author types. Neither endpoint looks at the service, so neither one checks a permission — the
//! Java controller had no ACL call in it, only the login every `/v1` path required.

use crate::auth::AuthUser;
use crate::error::{DatiError, ErrorCode, Result};
use crate::model::template::{
    TemplateExtractRequest, TemplateExtractResponse, TemplatePreviewRequest,
    TemplatePreviewResponse, TemplateRenderMode,
};
use crate::system_variable;
use crate::template::{self, values_of};

/// The preview of one template. The system variables are merged over the supplied values, so the
/// editor sees what the server would really bind rather than what the author typed for `_user.id`.
pub fn preview(
    caller: &AuthUser,
    request: &TemplatePreviewRequest,
) -> Result<TemplatePreviewResponse> {
    request.validate()?;
    let template = request.template.clone().unwrap_or_default();
    let compiled = template::parse(&template).map_err(bad_request)?;
    let mut values = request.values.clone().unwrap_or_default();
    values.extend(system_variable::resolve(Some(caller)));

    let rendered = match request.mode {
        // Anything but `TEXT` took the SQL path in the Java branch, and `validate` already rejected
        // a missing mode, so the fall-through is only there to keep the match total.
        Some(TemplateRenderMode::Text) => template::render_text(&compiled, &values_of(&values)),
        _ => {
            let prepared =
                template::render_sql(&compiled, &values_of(&values)).map_err(bad_request)?;
            template::to_display_sql(&prepared)
        }
    };
    Ok(TemplatePreviewResponse { rendered })
}

/// The variables a template reads, minus the five the server fills in, sorted so the editor's
/// checklist is reproducible.
pub fn extract(request: &TemplateExtractRequest) -> Result<TemplateExtractResponse> {
    request.validate()?;
    let template = request.template.clone().unwrap_or_default();
    let compiled = template::parse(&template).map_err(bad_request)?;
    let mut variables: Vec<String> = compiled
        .variables()
        .into_iter()
        .filter(|name| !system_variable::is_system_variable(name))
        .collect();
    variables.sort();
    Ok(TemplateExtractResponse { variables })
}

/// A template that does not parse is a parameter error, exactly like the Java `CM001` that wrapped
/// the parser message.
fn bad_request(message: String) -> DatiError {
    DatiError::new(ErrorCode::InvalidParameter, [message])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn caller() -> AuthUser {
        AuthUser {
            id: "u-1".into(),
            name: "alice".into(),
            display_name: Some("Alice".into()),
        }
    }

    fn request(
        mode: TemplateRenderMode,
        template: &str,
        values: serde_json::Value,
    ) -> TemplatePreviewRequest {
        TemplatePreviewRequest {
            mode: Some(mode),
            template: Some(template.into()),
            values: Some(values.as_object().unwrap().clone()),
        }
    }

    #[test]
    fn text_preview_inlines_values_and_blocks() {
        let response = preview(
            &caller(),
            &request(
                TemplateRenderMode::Text,
                "你好 {{name}}, {{#if city}}来自 {{city}}{{/if}}",
                json!({"name": "Bob", "city": "上海"}),
            ),
        )
        .unwrap();
        assert_eq!(response.rendered, "你好 Bob, 来自 上海");

        let response = preview(
            &caller(),
            &request(TemplateRenderMode::Text, "{{missing:默认}}", json!({})),
        )
        .unwrap();
        assert_eq!(response.rendered, "默认");
    }

    #[test]
    fn sql_preview_shows_the_bound_values() {
        let response = preview(
            &caller(),
            &request(
                TemplateRenderMode::Sql,
                "SELECT * FROM orders WHERE city = {{city}} AND amount > {{min}}",
                json!({"city": "上海", "min": "100"}),
            ),
        )
        .unwrap();
        assert_eq!(
            response.rendered,
            "SELECT * FROM orders WHERE city = '上海' AND amount > 100"
        );

        // The display form is display only: the statement the tool runs keeps its placeholders.
        let response = preview(
            &caller(),
            &request(TemplateRenderMode::Sql, "SELECT {{x}}", json!({"x": "a?b"})),
        )
        .unwrap();
        assert_eq!(response.rendered, "SELECT 'a?b'");
    }

    #[test]
    fn the_server_wins_over_a_supplied_system_variable() {
        let response = preview(
            &caller(),
            &request(
                TemplateRenderMode::Text,
                "{{_user.id}}/{{_user.name}}/{{_user.display_name}}",
                json!({"_user.id": "someone-else", "_user.name": ""}),
            ),
        )
        .unwrap();
        assert_eq!(response.rendered, "u-1/alice/Alice");

        let response = preview(
            &caller(),
            &request(TemplateRenderMode::Text, "{{_now}} {{_date}}", json!({})),
        )
        .unwrap();
        // `_now` renders as `yyyy-MM-dd HH:mm:ss`, so the separator is the space before `_date`, not
        // the first space in the string.
        let (now, date) = response.rendered.rsplit_once(' ').unwrap();
        assert_eq!(now.len(), 19);
        assert_eq!(date, &now[..10]);
    }

    #[test]
    fn a_broken_template_is_a_parameter_error() {
        let err = preview(
            &caller(),
            &request(TemplateRenderMode::Sql, "SELECT {{x", json!({})),
        )
        .unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidParameter);
        assert!(
            err.message().starts_with("Request parameter error: "),
            "{}",
            err.message()
        );

        let missing_values = TemplatePreviewRequest {
            mode: Some(TemplateRenderMode::Text),
            template: Some("a".into()),
            values: None,
        };
        let err = preview(&caller(), &missing_values).unwrap_err();
        assert_eq!(
            err.message(),
            "Request parameter error: values: must not be null"
        );
    }

    #[test]
    fn extract_lists_the_parameters_an_author_must_declare() {
        let response = extract(&TemplateExtractRequest {
            template: Some(
                "SELECT {{_now}} FROM t WHERE a = {{city}} {{#if limit}}LIMIT {{limit}}{{/if}} AND b = {{city}}"
                    .into(),
            ),
        })
        .unwrap();
        assert_eq!(
            response.variables,
            vec!["city".to_string(), "limit".to_string()]
        );

        let err = extract(&TemplateExtractRequest::default()).unwrap_err();
        assert_eq!(
            err.message(),
            "Request parameter error: template: must not be blank"
        );
    }
}
