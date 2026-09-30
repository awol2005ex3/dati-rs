//! Access-control vocabulary: who may view or edit which resource.

use serde::{Deserialize, Serialize};

use super::base::Audit;
use crate::error::{DatiError, ErrorCode, Result};

/// The group every authenticated user belongs to in this release.
pub const ALL_USERS: &str = "ALL_USERS";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ResourceType {
    #[serde(rename = "DATA_SOURCE")]
    DataSource,
    #[serde(rename = "SUBJECT")]
    Subject,
    #[serde(rename = "MCP_SERVICE")]
    McpService,
}

impl ResourceType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DataSource => "DATA_SOURCE",
            Self::Subject => "SUBJECT",
            Self::McpService => "MCP_SERVICE",
        }
    }

    /// The `{type}` path variable of `/v1/acls/{type}/{resourceId}` is the exact enum name.
    pub fn from_stored(value: &str) -> Option<Self> {
        match value {
            "DATA_SOURCE" => Some(Self::DataSource),
            "SUBJECT" => Some(Self::Subject),
            "MCP_SERVICE" => Some(Self::McpService),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PrincipalType {
    #[serde(rename = "USER")]
    User,
    #[serde(rename = "GROUP")]
    Group,
}

impl PrincipalType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "USER",
            Self::Group => "GROUP",
        }
    }

    pub fn from_stored(value: &str) -> Option<Self> {
        match value {
            "USER" => Some(Self::User),
            "GROUP" => Some(Self::Group),
            _ => None,
        }
    }
}

/// Grant levels; `Edit` implies `View`, exactly as the Java ordinal comparison did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Permission {
    #[serde(rename = "VIEW")]
    View,
    #[serde(rename = "EDIT")]
    Edit,
}

impl Permission {
    pub const fn rank(self) -> u8 {
        match self {
            Self::View => 0,
            Self::Edit => 1,
        }
    }

    pub const fn covers(self, required: Self) -> bool {
        self.rank() >= required.rank()
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::View => "VIEW",
            Self::Edit => "EDIT",
        }
    }

    pub fn from_stored(value: &str) -> Option<Self> {
        match value {
            "VIEW" => Some(Self::View),
            "EDIT" => Some(Self::Edit),
            _ => None,
        }
    }
}

/// `POST /v1/acls/{type}/{resourceId}` body. The enums arrive as their Java constant names.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GrantRequest {
    #[serde(default)]
    pub principal_type: Option<PrincipalType>,
    #[serde(default)]
    pub principal_id: Option<String>,
    #[serde(default)]
    pub permission: Option<Permission>,
}

impl GrantRequest {
    /// Field presence plus the group rules the Java service enforced before touching the database:
    /// the only addressable group is the public one, and it can never be given edit rights.
    pub fn validate(&self) -> Result<(PrincipalType, String, Permission)> {
        let mut fields = Vec::new();
        if self.principal_type.is_none() {
            fields.push(("principal_type", "must not be null"));
        }
        match self.principal_id.as_deref().map(str::trim) {
            None | Some("") => fields.push(("principal_id", "must not be blank")),
            Some(_) => {}
        }
        if self.permission.is_none() {
            fields.push(("permission", "must not be null"));
        }
        if !fields.is_empty() {
            return Err(DatiError::validation(&fields));
        }
        let principal_type = self.principal_type.unwrap();
        let principal_id = self
            .principal_id
            .clone()
            .unwrap_or_default()
            .trim()
            .to_string();
        let permission = self.permission.unwrap();
        if principal_type == PrincipalType::Group {
            if principal_id != ALL_USERS {
                return Err(DatiError::new(
                    ErrorCode::InvalidParameter,
                    ["principal_id"],
                ));
            }
            if permission != Permission::View {
                return Err(DatiError::new(ErrorCode::InvalidParameter, ["permission"]));
            }
        }
        Ok((principal_type, principal_id, permission))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AclEntry {
    pub id: String,
    pub resource_type: ResourceType,
    pub resource_id: String,
    pub principal_type: PrincipalType,
    pub principal_id: String,
    pub permission: Permission,
    pub audit: Audit,
}

/// What `GET /v1/acls/{type}/{resourceId}` returns; `principal_name` is resolved from the user table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AclEntryView {
    pub id: String,
    pub principal_type: PrincipalType,
    pub principal_id: String,
    pub principal_name: Option<String>,
    pub permission: Permission,
    pub created_by: Option<String>,
}

impl AclEntryView {
    pub fn new(entry: &AclEntry, principal_name: Option<String>) -> Self {
        Self {
            id: entry.id.clone(),
            principal_type: entry.principal_type,
            principal_id: entry.principal_id.clone(),
            principal_name,
            permission: entry.permission,
            created_by: entry.audit.created_by.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_covers_view() {
        assert!(Permission::Edit.covers(Permission::View));
        assert!(Permission::Edit.covers(Permission::Edit));
        assert!(!Permission::View.covers(Permission::Edit));
        assert!(Permission::View.covers(Permission::View));
    }

    #[test]
    fn resource_types_parse_the_acl_path() {
        assert_eq!(
            ResourceType::from_stored("DATA_SOURCE"),
            Some(ResourceType::DataSource)
        );
        assert_eq!(
            ResourceType::from_stored("MCP_SERVICE").map(ResourceType::as_str),
            Some("MCP_SERVICE")
        );
        assert_eq!(ResourceType::from_stored("data_source"), None);
        assert_eq!(ResourceType::from_stored("TABLE"), None);
    }

    #[test]
    fn enums_serialize_as_java_constant_names() {
        assert_eq!(
            serde_json::to_value(PrincipalType::Group).unwrap(),
            serde_json::json!("GROUP")
        );
        assert_eq!(
            serde_json::to_value(Permission::View).unwrap(),
            serde_json::json!("VIEW")
        );
        assert_eq!(
            serde_json::to_value(ResourceType::Subject).unwrap(),
            serde_json::json!("SUBJECT")
        );
    }
}
