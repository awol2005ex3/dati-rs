//! DatI semantic gateway core.
//!
//! Ports the domain, persistence, service and connector layers of the Java `dati-core` module
//! onto axum/sqlx, keeping the REST wire contract (snake_case JSON) and the MCP contract
//! (camelCase JSON-RPC) byte-compatible so the existing frontend and MCP hosts keep working.

pub mod auth;
pub mod config;
pub mod connector;
pub mod db;
pub mod error;
pub mod model;
pub mod repo;
pub mod service;
pub mod sql_analysis;
pub mod system_variable;
pub mod template;
pub mod time;
