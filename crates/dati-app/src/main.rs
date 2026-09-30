//! The single DatI executable: metadata store, admin API and the embedded frontend.

use std::error::Error;
use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::net::TcpListener;

use dati_app::{state::AppState, web};
use dati_core::config::{Config, DEFAULT_ADMIN_PASSWORD, MetaKind};
use dati_core::db::Db;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dati=info,dati_core=info,dati_app=info".into()),
        )
        .init();

    let config = Config::from_env()?;
    config.validate()?;
    prepare_sqlite_path(&config)?;
    let db = Db::connect(&config.db_url, config.db_kind).await?;
    let port = config.port;
    let state = AppState::new(db, config);
    seed_admins(&state).await;
    spawn_usage_tasks(&state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(%addr, "DatI listening");
    axum::serve(
        listener,
        web::app_router(state.clone()).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    // Whatever the scheduled flush had not written out yet still counts as history.
    state.usage.flush().await;
    Ok(())
}

/// Registers the configured administrators, so the shipped binary has an account to log into.
/// The Java service left that step to its operators, who ran a web app others had already filled;
/// a single executable starts from an empty store with nobody to register first.
async fn seed_admins(state: &AppState) {
    let created = match state.auth.ensure_admins().await {
        Ok(created) => created,
        Err(err) => {
            tracing::warn!(error = %err.message(), "could not create the initial administrator accounts");
            return;
        }
    };
    if created.is_empty() {
        return;
    }
    tracing::info!(
        users = %created.join(","),
        default_password = state.config.admin_password == DEFAULT_ADMIN_PASSWORD,
        "created the initial administrator accounts; they authenticate with ADMIN_PASSWORD"
    );
}

/// The two Spring scheduled tasks of the Java usage layer, as background loops: the collector's
/// fixed-delay flush (which returns at once when statistics are switched off) and the retention
/// purge that ran at three in the morning.
fn spawn_usage_tasks(state: &AppState) {
    let collector = state.usage.clone();
    let every = Duration::from_secs(state.config.mcp_stats_flush_interval_seconds.max(1));
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            collector.flush().await;
        }
    });

    let usage = state.mcp_usage.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(seconds_until_three_am())).await;
            match usage.cleanup().await {
                Ok(deleted) => tracing::info!(deleted, "cleaned up expired MCP usage data"),
                Err(err) => {
                    tracing::warn!(error = %err.message(), "failed to clean up expired MCP usage data")
                }
            }
        }
    });
}

/// The Java cron read the server's clock; every timestamp this port writes is UTC, so the purge
/// waits for 03:00 UTC.
fn seconds_until_three_am() -> u64 {
    const DAY: u64 = 24 * 60 * 60;
    const THREE_AM: u64 = 3 * 60 * 60;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default();
    let into_day = now % DAY;
    if into_day < THREE_AM {
        THREE_AM - into_day
    } else {
        DAY - into_day + THREE_AM
    }
}

/// A sqlite URL pointing into a directory that does not exist is a setup mistake we can fix here.
fn prepare_sqlite_path(config: &Config) -> Result<(), Box<dyn Error>> {
    if config.db_kind != MetaKind::Sqlite {
        return Ok(());
    }
    let path = config.db_url.strip_prefix("sqlite:").unwrap_or_default();
    let path = path.split_once('?').map_or(path, |(path, _)| path);
    if path.is_empty() || path == ":memory:" {
        return Ok(());
    }
    if let Some(parent) = std::path::Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

async fn shutdown_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        tracing::warn!(error = %err, "cannot install the shutdown handler");
    }
    tracing::info!("shutting down");
}
