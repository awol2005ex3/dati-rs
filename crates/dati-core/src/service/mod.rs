//! Domain services: the business rules that sit between the HTTP layer and the repositories.

pub mod acl;
pub mod auth;
pub mod column;
pub mod datasource;
pub mod mcp_endpoint;
pub mod mcp_exec;
pub mod mcp_prompt;
pub mod mcp_scope;
pub mod mcp_service;
pub mod mcp_tool;
pub mod mcp_usage;
pub mod mcp_version;
pub mod permission;
pub mod search;
pub mod subject;
pub mod table;
pub mod template_preview;
pub mod term;
