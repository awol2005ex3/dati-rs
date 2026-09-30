//! Shared application state: one metadata pool and the services built on it.

use std::sync::Arc;

use dati_core::config::Config;
use dati_core::db::Db;
use dati_core::service::acl::AclService;
use dati_core::service::auth::AuthService;
use dati_core::service::column::ColumnService;
use dati_core::service::datasource::DataSourceService;
use dati_core::service::mcp_endpoint::McpEndpoint;
use dati_core::service::mcp_exec::McpExecutors;
use dati_core::service::mcp_prompt::McpPromptService;
use dati_core::service::mcp_scope::ScopeValidator;
use dati_core::service::mcp_service::McpServiceService;
use dati_core::service::mcp_tool::McpToolService;
use dati_core::service::mcp_usage::{McpUsageService, UsageCollector};
use dati_core::service::mcp_version::McpVersionService;
use dati_core::service::permission::PermissionService;
use dati_core::service::search::SearchService;
use dati_core::service::subject::SubjectService;
use dati_core::service::table::TableService;
use dati_core::service::term::TermService;

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub config: Arc<Config>,
    pub auth: AuthService,
    pub permissions: PermissionService,
    pub acl: AclService,
    pub data_sources: DataSourceService,
    pub tables: TableService,
    pub columns: ColumnService,
    pub subjects: SubjectService,
    pub terms: TermService,
    pub search: SearchService,
    pub mcp_services: McpServiceService,
    pub mcp_tools: McpToolService,
    pub mcp_prompts: McpPromptService,
    pub mcp_versions: McpVersionService,
    pub scope: ScopeValidator,
    pub mcp_executors: McpExecutors,
    pub mcp_endpoint: McpEndpoint,
    /// The in-memory half of the usage statistics, which a request feeds and a background task
    /// writes out.
    pub usage: UsageCollector,
    /// The read and purge half of the same data, which the console's endpoints use.
    pub mcp_usage: McpUsageService,
}

impl AppState {
    pub fn new(db: Db, config: Config) -> Self {
        let config = Arc::new(config);
        let permissions = PermissionService::new(&config);
        let data_sources = DataSourceService::new(db.clone(), &config, permissions.clone());
        let tables = TableService::new(db.clone(), data_sources.clone());
        let columns = ColumnService::new(db.clone(), data_sources.clone(), tables.clone());
        let subjects = SubjectService::new(db.clone(), data_sources.clone());
        let terms = TermService::new(db.clone(), subjects.clone());
        let mcp_services =
            McpServiceService::new(db.clone(), data_sources.clone(), subjects.clone(), &config);
        let mcp_tools = McpToolService::new(db.clone(), mcp_services.clone());
        let mcp_prompts = McpPromptService::new(db.clone(), mcp_services.clone());
        let mcp_versions = McpVersionService::new(
            db.clone(),
            mcp_services.clone(),
            mcp_tools.clone(),
            mcp_prompts.clone(),
        );
        let search = SearchService::new(db.clone(), terms.clone());
        let scope = ScopeValidator::new(db.clone());
        let mcp_executors = McpExecutors::new(
            db.clone(),
            scope.clone(),
            mcp_services.clone(),
            data_sources.clone(),
            tables.clone(),
            columns.clone(),
            terms.clone(),
            search.clone(),
        );
        let usage = UsageCollector::new(
            db.clone(),
            config.mcp_stats_enabled,
            config.mcp_stats_batch_size,
        );
        let mcp_endpoint = McpEndpoint::new(
            mcp_services.clone(),
            mcp_executors.clone(),
            usage.clone(),
            &config,
        );
        Self {
            auth: AuthService::new(db.clone(), (*config).clone()),
            acl: AclService::new(db.clone(), permissions.clone()),
            data_sources,
            tables,
            columns,
            subjects,
            terms,
            search,
            mcp_services,
            mcp_tools,
            mcp_prompts,
            mcp_versions,
            scope,
            mcp_executors,
            mcp_endpoint,
            usage,
            mcp_usage: McpUsageService::new(db.clone(), config.mcp_stats_retention_days),
            db,
            config,
            permissions,
        }
    }
}
