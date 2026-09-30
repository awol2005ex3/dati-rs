//! Runtime configuration, read from the environment the same way the Java app read it.

use std::env;

use crate::error::{DatiError, ErrorCode, Result};

/// Password `ADMIN_PASSWORD` falls back to, so a first start has a known credential to log in with.
pub const DEFAULT_ADMIN_PASSWORD: &str = "admin123";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaKind {
    Sqlite,
    Mysql,
    Postgres,
}

impl MetaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::Mysql => "mysql",
            Self::Postgres => "postgres",
        }
    }

    pub fn from_url(url: &str) -> Option<Self> {
        // sqlite URLs are commonly written without `//` (`sqlite:db/dati.db`), so split on the scheme only.
        let scheme = url
            .split(':')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        match scheme.as_str() {
            "sqlite" => Some(Self::Sqlite),
            "mysql" | "mariadb" => Some(Self::Mysql),
            "postgres" | "postgresql" => Some(Self::Postgres),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub port: u16,
    /// Connection URL of the metadata store ("知识库").
    pub db_url: String,
    pub db_kind: MetaKind,
    pub jwt_secret: String,
    pub jwt_expiration_seconds: i64,
    pub admin_users: Vec<String>,
    /// Password of the administrator account created on first start. Java had no such account:
    /// its operators registered themselves, which a single shipped binary cannot assume.
    pub admin_password: String,
    /// Full URL prefix advertised for MCP endpoints; empty keeps the relative `/{code}/mcp`.
    pub mcp_endpoint_base_url: String,
    pub mcp_allowed_origins: Vec<String>,
    /// `dati.mcp.stats.*`: the collector that buffers every `tools/call`, and the two jobs that
    /// write it out and drop the rows that aged past the retention window.
    pub mcp_stats_enabled: bool,
    pub mcp_stats_batch_size: usize,
    pub mcp_stats_flush_interval_seconds: u64,
    pub mcp_stats_retention_days: i64,
    pub column_value_sample_limit: i64,
    pub column_value_length_limit: i64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self::from_source(|key| env::var(key).ok()))
    }

    pub fn from_source(mut lookup: impl FnMut(&str) -> Option<String>) -> Self {
        let db_url = lookup("DATI_DB_URL")
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "sqlite:db/dati.db?mode=rwc".to_string());
        let db_kind = MetaKind::from_url(&db_url).unwrap_or(MetaKind::Sqlite);

        Self {
            port: lookup("PORT")
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(8085),
            db_url,
            db_kind,
            jwt_secret: lookup("JWT_SECRET")
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "this-is-a-32-char-long-default-secret-key".to_string()),
            jwt_expiration_seconds: lookup("JWT_EXPIRATION_SECONDS")
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(604_800),
            admin_users: split_list(&lookup("ADMIN_USERS").unwrap_or_else(|| "admin".to_string())),
            admin_password: lookup("ADMIN_PASSWORD")
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_ADMIN_PASSWORD.to_string()),
            mcp_endpoint_base_url: lookup("MCP_ENDPOINT_BASE_URL").unwrap_or_default(),
            mcp_allowed_origins: split_list(&lookup("MCP_ALLOWED_ORIGINS").unwrap_or_default()),
            mcp_stats_enabled: lookup("MCP_STATS_ENABLED")
                .map(|v| {
                    !matches!(
                        v.trim().to_ascii_lowercase().as_str(),
                        "false" | "0" | "no" | "off"
                    )
                })
                .unwrap_or(true),
            mcp_stats_batch_size: lookup("MCP_STATS_BATCH_SIZE")
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(100),
            mcp_stats_flush_interval_seconds: lookup("MCP_STATS_FLUSH_INTERVAL_SECONDS")
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(10),
            mcp_stats_retention_days: lookup("MCP_STATS_RETENTION_DAYS")
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(15),
            column_value_sample_limit: lookup("COLUMN_VALUE_SAMPLE_LIMIT")
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(1000),
            column_value_length_limit: lookup("COLUMN_VALUE_LENGTH_LIMIT")
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(256),
        }
    }

    pub fn is_admin(&self, name: &str) -> bool {
        self.admin_users.iter().any(|u| u == name)
    }

    /// Rejects a metadata-store URL we cannot talk to, before any side effect happens.
    pub fn validate(&self) -> Result<()> {
        if MetaKind::from_url(&self.db_url).is_none() {
            return Err(DatiError::new(
                ErrorCode::InvalidParameter,
                [format!(
                    "unsupported DATI_DB_URL scheme, expected sqlite/mysql/postgres: {}",
                    self.db_url
                )],
            ));
        }
        if self.jwt_secret.len() < 32 {
            return Err(DatiError::new(
                ErrorCode::InvalidParameter,
                ["JWT_SECRET must be at least 32 bytes for HS256"],
            ));
        }
        Ok(())
    }
}

fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToString::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(pairs: &[(&str, &str)]) -> Config {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        Config::from_source(|k| map.get(k).cloned())
    }

    #[test]
    fn defaults_match_the_java_application_yaml() {
        let c = cfg(&[]);
        assert_eq!(c.port, 8085);
        assert_eq!(c.db_kind, MetaKind::Sqlite);
        assert_eq!(c.jwt_expiration_seconds, 604_800);
        assert_eq!(c.admin_users, vec!["admin".to_string()]);
        assert_eq!(c.admin_password, DEFAULT_ADMIN_PASSWORD);
        assert_eq!(c.column_value_sample_limit, 1000);
        assert_eq!(c.column_value_length_limit, 256);
        assert!(c.mcp_allowed_origins.is_empty());
        assert!(c.mcp_stats_enabled);
        assert_eq!(c.mcp_stats_batch_size, 100);
        assert_eq!(c.mcp_stats_flush_interval_seconds, 10);
        assert_eq!(c.mcp_stats_retention_days, 15);
    }

    #[test]
    fn the_stats_switch_only_turns_off_for_a_false() {
        assert!(!cfg(&[("MCP_STATS_ENABLED", " off ")]).mcp_stats_enabled);
        assert!(!cfg(&[("MCP_STATS_ENABLED", "FALSE")]).mcp_stats_enabled);
        assert!(cfg(&[("MCP_STATS_ENABLED", "")]).mcp_stats_enabled);
        assert_eq!(
            cfg(&[("MCP_STATS_BATCH_SIZE", "nope")]).mcp_stats_batch_size,
            100
        );
    }

    #[test]
    fn db_url_scheme_selects_dialect() {
        assert_eq!(MetaKind::Sqlite, MetaKind::from_url("sqlite:a.db").unwrap());
        assert_eq!(
            MetaKind::Mysql,
            MetaKind::from_url("mysql://u@h/db").unwrap()
        );
        assert_eq!(
            MetaKind::Postgres,
            MetaKind::from_url("postgresql://h/db").unwrap()
        );
        assert!(MetaKind::from_url("oracle://h/db").is_none());
    }

    #[test]
    fn admin_users_splits_on_commas() {
        let c = cfg(&[("ADMIN_USERS", " root , ops ")]);
        assert_eq!(c.admin_users, vec!["root".to_string(), "ops".to_string()]);
        assert!(c.is_admin("root"));
        assert!(!c.is_admin("admin"));
    }

    #[test]
    fn admin_password_is_kept_as_given_and_blank_falls_back() {
        assert_eq!(
            cfg(&[("ADMIN_PASSWORD", "s3cret")]).admin_password,
            "s3cret"
        );
        assert_eq!(
            cfg(&[("ADMIN_PASSWORD", "   ")]).admin_password,
            DEFAULT_ADMIN_PASSWORD
        );
    }

    #[test]
    fn rejects_unknown_scheme_and_short_secret() {
        let c = cfg(&[("DATI_DB_URL", "oracle://h/db")]);
        assert_eq!(
            c.validate().unwrap_err().code(),
            ErrorCode::InvalidParameter
        );
        let c = cfg(&[("JWT_SECRET", "short")]);
        assert!(c.validate().is_err());
        assert!(cfg(&[]).validate().is_ok());
    }
}
