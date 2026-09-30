//! Error codes and the HTTP error body shared by the REST and MCP layers.

use std::fmt;

use serde::Serialize;

/// Business error codes ported from the Java `ErrorCode` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    InvalidParameter,
    NotFound,
    InternalError,
    MethodNotAllowed,

    DsConnectionFailed,
    DsNotFound,
    DsSqlError,
    DsSyncFailed,
    DsUnsupportedType,
    DsSchemaDetectionFailed,
    DsTableNotFound,

    SmSubjectNotFound,
    SmTermNotFound,
    SmTableNotInSubject,
    SmTermRelationInvalid,
    SmTableAlreadyAssociated,
    SmAssociationNotFound,

    FieldRequired,

    MsServiceNotFound,
    MsToolNotFound,
    MsToolNameExists,
    MsToolNameInvalid,
    MsPromptNotFound,
    MsPromptNameExists,
    MsPromptArgMismatch,
    MsTemplateSyntaxError,
    MsToolArgMismatch,
    MsServiceCodeExists,
    MsServiceCodeInvalid,
    MsServiceCodeRequired,
    MsSqlPolicyViolation,
    MsScopeError,
    MsToolDisabled,
    MsServiceStatusInvalid,
    MsServiceVersionNotFound,
    MsServiceDataScopeRequired,
    MsServiceDataScopeEmpty,
    MsTemplateQuotedVar,

    AuthLoginFailed,
    AuthTokenInvalid,
    AuthUserExists,
    AuthTypeUnsupported,
    AuthKeyForbidden,
    AuthKeyInvalidExpiry,

    PermissionDenied,
}

impl ErrorCode {
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidParameter => "CM001",
            Self::NotFound => "CM002",
            Self::InternalError => "CM003",
            Self::MethodNotAllowed => "CM004",
            Self::DsConnectionFailed => "DS001",
            Self::DsNotFound => "DS002",
            Self::DsSqlError => "DS003",
            Self::DsSyncFailed => "DS004",
            Self::DsUnsupportedType => "DS005",
            Self::DsSchemaDetectionFailed => "DS006",
            Self::DsTableNotFound => "DS007",
            Self::SmSubjectNotFound => "SM001",
            Self::SmTermNotFound => "SM002",
            Self::SmTableNotInSubject => "SM003",
            Self::SmTermRelationInvalid => "SM004",
            Self::SmTableAlreadyAssociated => "SM005",
            Self::SmAssociationNotFound => "SM006",
            Self::FieldRequired => "VAL002",
            Self::MsServiceNotFound => "MS001",
            Self::MsToolNotFound => "MS002",
            Self::MsToolNameExists => "MS003",
            Self::MsToolNameInvalid => "MS004",
            Self::MsPromptNotFound => "MS005",
            Self::MsPromptNameExists => "MS006",
            Self::MsPromptArgMismatch => "MS007",
            Self::MsTemplateSyntaxError => "MS008",
            Self::MsToolArgMismatch => "MS009",
            Self::MsServiceCodeExists => "MS010",
            Self::MsServiceCodeInvalid => "MS011",
            Self::MsServiceCodeRequired => "MS012",
            Self::MsSqlPolicyViolation => "MS013",
            Self::MsScopeError => "MS014",
            Self::MsToolDisabled => "MS015",
            Self::MsServiceStatusInvalid => "MS016",
            Self::MsServiceVersionNotFound => "MS017",
            Self::MsServiceDataScopeRequired => "MS018",
            Self::MsServiceDataScopeEmpty => "MS019",
            Self::MsTemplateQuotedVar => "MS020",
            Self::AuthLoginFailed => "AUTH001",
            Self::AuthTokenInvalid => "AUTH002",
            Self::AuthUserExists => "AUTH003",
            Self::AuthTypeUnsupported => "AUTH004",
            Self::AuthKeyForbidden => "AUTH005",
            Self::AuthKeyInvalidExpiry => "AUTH006",
            Self::PermissionDenied => "PM001",
        }
    }

    pub const fn status(self) -> u16 {
        match self {
            Self::InvalidParameter
            | Self::DsConnectionFailed
            | Self::DsSqlError
            | Self::DsUnsupportedType
            | Self::DsSchemaDetectionFailed
            | Self::SmTableNotInSubject
            | Self::SmTermRelationInvalid
            | Self::SmTableAlreadyAssociated
            | Self::FieldRequired
            | Self::MsToolNameInvalid
            | Self::MsPromptArgMismatch
            | Self::MsTemplateSyntaxError
            | Self::MsToolArgMismatch
            | Self::MsServiceCodeInvalid
            | Self::MsServiceCodeRequired
            | Self::MsScopeError
            | Self::MsToolDisabled
            | Self::MsServiceDataScopeRequired
            | Self::MsServiceDataScopeEmpty
            | Self::MsTemplateQuotedVar
            | Self::AuthTypeUnsupported
            | Self::AuthKeyInvalidExpiry => 400,
            Self::NotFound
            | Self::DsNotFound
            | Self::DsTableNotFound
            | Self::SmSubjectNotFound
            | Self::SmTermNotFound
            | Self::SmAssociationNotFound
            | Self::MsServiceNotFound
            | Self::MsToolNotFound
            | Self::MsPromptNotFound
            | Self::MsServiceVersionNotFound => 404,
            Self::MethodNotAllowed => 405,
            Self::DsSyncFailed | Self::InternalError => 500,
            Self::AuthLoginFailed | Self::AuthTokenInvalid => 401,
            Self::MsSqlPolicyViolation | Self::PermissionDenied => 403,
            Self::MsToolNameExists | Self::MsPromptNameExists | Self::MsServiceCodeExists => 409,
            Self::MsServiceStatusInvalid => 409,
            Self::AuthUserExists => 409,
            Self::AuthKeyForbidden => 403,
        }
    }

    /// Message template using `MessageFormat`-style `{0}`/`{1}` placeholders.
    pub const fn template(self) -> &'static str {
        match self {
            Self::InvalidParameter => "Request parameter error: {0}",
            Self::NotFound => "Resource not found",
            Self::InternalError => "Internal server error",
            Self::MethodNotAllowed => "Method not allowed: {0}",
            Self::DsConnectionFailed => "Database connection failed: {0}",
            Self::DsNotFound => "Data source not found: {0}",
            Self::DsSqlError => "SQL execution error: {0}",
            Self::DsSyncFailed => "Failed to sync metadata for data source: {0}",
            Self::DsUnsupportedType => "Unsupported database type: {0}",
            Self::DsSchemaDetectionFailed => "Failed to detect default schema: {0}",
            Self::DsTableNotFound => "Table not found: {0}",
            Self::SmSubjectNotFound => "Subject not found: {0}",
            Self::SmTermNotFound => "Term not found: {0}",
            Self::SmTableNotInSubject => "Table {0} does not belong to subject {1}",
            Self::SmTermRelationInvalid => "Invalid term relation: {0}",
            Self::SmTableAlreadyAssociated => "Table {0} is already associated with subject {1}",
            Self::SmAssociationNotFound => {
                "Association between subject {0} and table {1} not found"
            }
            Self::FieldRequired => "Field {0} is required",
            Self::MsServiceNotFound => "MCP service not found: {0}",
            Self::MsToolNotFound => "MCP tool not found: {0}",
            Self::MsToolNameExists => "Tool name already exists in this service: {0}",
            Self::MsToolNameInvalid => {
                "Tool name must be 1-128 chars, allowed: A-Z, a-z, 0-9, _, -, ."
            }
            Self::MsPromptNotFound => "MCP prompt not found: {0}",
            Self::MsPromptNameExists => "Prompt name already exists in this service: {0}",
            Self::MsPromptArgMismatch => "Prompt parameter mismatch: {0}",
            Self::MsTemplateSyntaxError => "Template syntax error in {0}: {1}",
            Self::MsToolArgMismatch => "Tool parameter mismatch: {0}",
            Self::MsServiceCodeExists => "Service code already exists: {0}",
            Self::MsServiceCodeInvalid => {
                "Service code must be 1-64 chars, lowercase letters, digits, hyphens and underscores only"
            }
            Self::MsServiceCodeRequired => "Service code is required",
            Self::MsSqlPolicyViolation => "SQL operation not allowed by policy: {0}",
            Self::MsScopeError => "Data source or table not in scope: {0}",
            Self::MsToolDisabled => "Tool is disabled: {0}",
            Self::MsServiceStatusInvalid => "Invalid status transition for MCP service: {0}",
            Self::MsServiceVersionNotFound => "MCP service version not found: {0}",
            Self::MsServiceDataScopeRequired => "Data scope is required when creating MCP service",
            Self::MsServiceDataScopeEmpty => "Data scope is empty, cannot publish MCP service: {0}",
            Self::MsTemplateQuotedVar => {
                "Template variable(s) {0} are inside string literals; strings are auto-quoted, remove the surrounding quotes"
            }
            Self::AuthLoginFailed => "Authentication failed: invalid credentials",
            Self::AuthTokenInvalid => "Token invalid or expired",
            Self::AuthUserExists => "Username already exists: {0}",
            Self::AuthTypeUnsupported => "Unsupported authentication type: {0}",
            Self::AuthKeyForbidden => "Cannot operate on API key of another user",
            Self::AuthKeyInvalidExpiry => "expiresInDays must be one of 7/30/90/180/365",
            Self::PermissionDenied => "No permission to access this resource",
        }
    }

    pub fn message(self, args: &[String]) -> String {
        // Mirrors MessageFormat through DatiException: an unsubstituted placeholder is kept verbatim.
        let mut out = self.template().to_string();
        for (i, arg) in args.iter().enumerate() {
            out = out.replace(&format!("{{{i}}}"), arg);
        }
        out
    }
}

/// An empty argument list for `DatiError::new`; the item type is not inferable from `[]`.
pub const NO_ARGS: [&str; 0] = [];

/// A failure that carries a business error code, rendered as the Java `ErrorResponse` body.
#[derive(Debug, Clone)]
pub struct DatiError {
    code: ErrorCode,
    message: String,
}

impl DatiError {
    pub fn new(code: ErrorCode, args: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        let args: Vec<String> = args.into_iter().map(|a| a.as_ref().to_string()).collect();
        Self {
            code,
            message: code.message(&args),
        }
    }

    pub fn with_message(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// `CM001` built from field-validation pairs, joined the way Spring's `BindException` did.
    pub fn validation(fields: &[(&str, &str)]) -> Self {
        let detail: Vec<String> = fields
            .iter()
            .map(|(field, reason)| format!("{field}: {reason}"))
            .collect();
        Self::new(ErrorCode::InvalidParameter, &[detail.join("; ")])
    }

    pub fn code(&self) -> ErrorCode {
        self.code
    }

    pub fn status(&self) -> u16 {
        self.code.status()
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn body(&self) -> ErrorResponse {
        ErrorResponse {
            code: self.code.code().to_string(),
            message: self.message.clone(),
            timestamp: crate::time::now_millis(),
        }
    }
}

impl fmt::Display for DatiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DatiError {}

impl From<sqlx::Error> for DatiError {
    fn from(err: sqlx::Error) -> Self {
        tracing::error!(error = %err, "database error");
        Self::with_message(ErrorCode::InternalError, "Internal server error")
    }
}

#[derive(Serialize)]
pub struct ErrorResponse {
    pub code: String,
    pub message: String,
    pub timestamp: i64,
}

pub type Result<T> = std::result::Result<T, DatiError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_only_provided_arguments() {
        assert_eq!(
            ErrorCode::DsNotFound.message(&["ds-1".into()]),
            "Data source not found: ds-1"
        );
        // Java left the literal placeholder in when no args were supplied.
        assert_eq!(
            ErrorCode::DsNotFound.message(&[]),
            "Data source not found: {0}"
        );
        assert_eq!(
            ErrorCode::SmTableNotInSubject.message(&["t1".into(), "s1".into()]),
            "Table t1 does not belong to subject s1"
        );
    }

    #[test]
    fn codes_and_statuses_match_java_contract() {
        assert_eq!(ErrorCode::AuthLoginFailed.code(), "AUTH001");
        assert_eq!(ErrorCode::AuthLoginFailed.status(), 401);
        assert_eq!(ErrorCode::PermissionDenied.code(), "PM001");
        assert_eq!(ErrorCode::PermissionDenied.status(), 403);
        assert_eq!(ErrorCode::MsSqlPolicyViolation.status(), 403);
        assert_eq!(ErrorCode::DsSyncFailed.status(), 500);
        assert_eq!(ErrorCode::MsServiceStatusInvalid.status(), 409);
        assert_eq!(ErrorCode::MsTemplateQuotedVar.code(), "MS020");
        assert_eq!(ErrorCode::FieldRequired.code(), "VAL002");
    }

    #[test]
    fn validation_errors_are_joined_like_spring() {
        let err =
            DatiError::validation(&[("name", "must not be blank"), ("size", "must be positive")]);
        assert_eq!(err.code(), ErrorCode::InvalidParameter);
        assert_eq!(
            err.message(),
            "Request parameter error: name: must not be blank; size: must be positive"
        );
    }

    #[test]
    fn error_body_uses_string_code_and_millis_timestamp() {
        let body = DatiError::new(ErrorCode::DsNotFound, ["x"]).body();
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["code"], "DS002");
        assert_eq!(json["message"], "Data source not found: x");
        assert!(json["timestamp"].is_i64());
        assert_eq!(json.as_object().unwrap().len(), 3);
    }
}
