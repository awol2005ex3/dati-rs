//! The executors behind `tools/call`: what each of the eight tool slots actually does.
//!
//! Java wired one `ToolExecutor` bean per [`McpToolType`] into a `Map` and dispatched on it. One
//! struct with a match over the same eight arms replaces that map, because Rust has no container of
//! beans to inject; everything else — the order of the guards, the failures each step raises, and
//! which failures abort the call versus land in one item of a batch result — is the Java behavior.
//!
//! Two seams the Java classes used are worth naming, since they decide who gets to answer:
//! - A `ToolFailure` (the Java `ToolExecuteException`) is the agent's problem: the endpoint turns it
//!   into an errored tool result, and inside a batch write only the one item that failed reports it.
//! - A `DatiError` is the gateway's problem: it aborts the whole call as a JSON-RPC internal error,
//!   which is exactly how a `DataIntegrityViolationException` from a permission check escaped the
//!   Java `catch (ToolExecuteException)` arm.
//!
//! The metadata writes go through the same `TableService`/`ColumnService`/`TermService` the admin
//! REST layer uses, so an agent writing a comment needs the `EDIT` right on the owning source or
//! subject that a human does — the Java executors made the same calls.

use std::collections::{HashMap, HashSet};

use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::auth::AuthUser;
use crate::connector::{SqlRun, StatementOutcome};
use crate::db::Db;
use crate::error::{DatiError, ErrorCode};
use crate::model::base::Audit;
use crate::model::datasource::{ColumnUpdateRequest, DataSource, TableUpdateRequest};
use crate::model::mcp::{McpDataScope, McpDataScopeType, McpToolType};
use crate::model::mcp_args::{
    ExecuteSqlArgs, GetTableInfoArgs, ListTablesArgs, SearchArgs, UpdateColumnArg,
    UpdateColumnInfoArgs, UpdateTableArg, UpdateTableInfoArgs, UpsertTermArg, UpsertTermArgs,
};
use crate::model::mcp_audit::MetadataAuditLog;
use crate::model::mcp_tool::{ExecuteSqlConfig, ParamSqlConfig, ToolConfig};
use crate::model::mcp_tool_data::{
    CallError, CallResult, ColumnDef, DataSourceDef, EntityUpdateResult, StatementResult, TableDef,
    TermDef, ToolData, ToolError, ToolFailure, UpdateError, value_map,
};
use crate::model::semantic::{CreateTermRequest, TermBrief, UpdateTermRequest};
use crate::repo::{column, column_value, datasource, mcp_audit, table, term};
use crate::service::column::ColumnService;
use crate::service::datasource::DataSourceService;
use crate::service::mcp_scope::ScopeValidator;
use crate::service::mcp_service::McpServiceService;
use crate::service::search::SearchService;
use crate::service::table::TableService;
use crate::service::term::TermService;
use crate::sql_analysis::{self, SqlOperationType};
use crate::time::new_id;
use crate::{system_variable, template};

/// How many sample values a schema report gives per column, and how many dictionary rows the read
/// that feeds it takes — the two constants the Java `TableMetadataService` shared with the search.
const SAMPLE_VALUE_LIMIT: usize = 5;
const VALUE_QUERY_MAX: i64 = 5000;

/// One `tools/call` as the executors see it. `arguments` is the raw JSON object the client sent; a
/// prebuilt slot binds it into its record, a custom `PARAMETERIZED_SQL` tool reads it as-is.
pub struct ToolContext<'a> {
    pub service_id: &'a str,
    pub tool_type: McpToolType,
    pub config: &'a ToolConfig,
    pub arguments: Map<String, Value>,
    pub scope_items: &'a [McpDataScope],
    pub caller: &'a AuthUser,
}

/// A prebuilt tool's argument contract: the shape Jackson mapped onto, plus the constraints it
/// checked afterwards — which every record already exposes as `violations`.
trait Record: Sized + DeserializeOwned {
    fn checked(&self) -> Vec<String>;
}

macro_rules! record {
    ($($type:ty),+) => {
        $(
            impl Record for $type {
                fn checked(&self) -> Vec<String> {
                    self.violations()
                }
            }
        )+
    };
}

record!(
    SearchArgs,
    GetTableInfoArgs,
    ExecuteSqlArgs,
    UpdateTableInfoArgs,
    UpdateColumnInfoArgs,
    UpsertTermArgs
);

/// `LIST_TABLES` has no fields to check, only the unknown ones `deny_unknown_fields` rejects.
impl Record for ListTablesArgs {
    fn checked(&self) -> Vec<String> {
        Vec::new()
    }
}

/// The Java `ToolParameterBinder`: map, then validate. A record that cannot be read is a
/// `参数不合法：Parse JSON failed` — Jackson's own message named the offending property in a text no
/// client parsed, so the port keeps the one detail that is stable.
fn bind<T: Record>(arguments: &Map<String, Value>) -> CallResult<T> {
    let record: T = serde_json::from_value(Value::Object(arguments.clone()))
        .map_err(|_| ToolFailure::new(ToolError::ParamInvalid, &["Parse JSON failed"]))?;
    let mut violations = record.checked();
    if violations.is_empty() {
        return Ok(record);
    }
    violations.sort();
    Err(ToolFailure::new(ToolError::ParamInvalid, &[&violations.join("; ")]).into())
}

/// The eight arms of `tools/call`.
#[derive(Clone)]
pub struct McpExecutors {
    db: Db,
    scope: ScopeValidator,
    services: McpServiceService,
    data_sources: DataSourceService,
    tables: TableService,
    columns: ColumnService,
    terms: TermService,
    search: SearchService,
}

impl McpExecutors {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Db,
        scope: ScopeValidator,
        services: McpServiceService,
        data_sources: DataSourceService,
        tables: TableService,
        columns: ColumnService,
        terms: TermService,
        search: SearchService,
    ) -> Self {
        Self {
            db,
            scope,
            services,
            data_sources,
            tables,
            columns,
            terms,
            search,
        }
    }

    pub async fn execute(&self, ctx: &ToolContext<'_>) -> CallResult<ToolData> {
        match ctx.tool_type {
            McpToolType::SearchMetadata => self.search_metadata(ctx).await,
            McpToolType::GetTableInfo => self.get_table_info(ctx).await,
            McpToolType::ListTables => self.list_tables(ctx).await,
            McpToolType::ExecuteSql => self.execute_sql(ctx).await,
            McpToolType::UpdateTableInfo => self.update_table_info(ctx).await,
            McpToolType::UpdateColumnInfo => self.update_column_info(ctx).await,
            McpToolType::UpsertTerm => self.upsert_term(ctx).await,
            McpToolType::ParameterizedSql => self.parameterized_sql(ctx).await,
        }
    }

    /// `SEARCH_METADATA`: the scoped keyword search, reshaped for the agent. An empty scope answers
    /// with the keywords echoed back and nothing else.
    async fn search_metadata(&self, ctx: &ToolContext<'_>) -> CallResult<ToolData> {
        let args: SearchArgs = bind(&ctx.arguments)?;
        let keywords: Vec<String> = args
            .keywords
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .collect();
        if ctx.scope_items.is_empty() {
            return Ok(ToolData::SearchHit {
                keywords,
                data_sources: Vec::new(),
                terms: Vec::new(),
            });
        }

        let sources = self.resolved_sources(ctx).await?;
        let subjects = subject_ids(ctx.scope_items);
        let result = self.search.search(&keywords, &sources, &subjects).await?;
        Ok(ToolData::SearchHit {
            keywords,
            data_sources: result
                .data_sources
                .into_iter()
                .map(|group| DataSourceDef {
                    id: group.data_source_id,
                    name: Some(group.data_source_name),
                    db_type: group.db_type,
                    default_schema: group.default_schema,
                    description: group.description,
                    tables: group
                        .tables
                        .into_iter()
                        .map(|table| TableDef {
                            table: table.table_name,
                            schema: table.schema,
                            description: table.description,
                            aliases: table.aliases,
                            columns: Some(
                                table
                                    .columns
                                    .into_iter()
                                    .map(|column| ColumnDef {
                                        name: column.name,
                                        column_type: column.column_type,
                                        comment: column.comment,
                                        aliases: column.aliases,
                                        sample_values: column.sample_values,
                                    })
                                    .collect(),
                            ),
                        })
                        .collect(),
                })
                .collect(),
            terms: result.terms.into_iter().map(term_def).collect(),
        })
    }

    /// `GET_TABLE_INFO`: one schema report per requested table. A table the metadata store does not
    /// have is silently absent from the answer, which is what the Java `ifPresent` did.
    async fn get_table_info(&self, ctx: &ToolContext<'_>) -> CallResult<ToolData> {
        let args: GetTableInfoArgs = bind(&ctx.arguments)?;
        let mut entries = Vec::new();
        for reference in args.tables.unwrap_or_default() {
            // The locate fields are unvalidated by design; a missing id reads as the Java `null`
            // did and fails the scope check with that word in the message.
            let source_id = reference.data_source_id.as_deref().unwrap_or("null");
            let table_name = reference.table.as_deref().unwrap_or("null");
            self.scope
                .validate(ctx.scope_items, source_id, &[], None)
                .await?;
            if let Some(meta) = self
                .table_meta(source_id, reference.schema.as_deref(), table_name)
                .await?
            {
                entries.push(TableDef {
                    table: reference.table,
                    schema: reference.schema,
                    description: meta.description,
                    aliases: meta.aliases,
                    columns: Some(meta.columns),
                });
            }
        }
        Ok(ToolData::TableMetadata { tables: entries })
    }

    /// `LIST_TABLES`: the whole inventory of the scope, table-level only.
    async fn list_tables(&self, ctx: &ToolContext<'_>) -> CallResult<ToolData> {
        let _args: ListTablesArgs = bind(&ctx.arguments)?;
        if ctx.scope_items.is_empty() {
            return Ok(ToolData::TableList {
                data_sources: Vec::new(),
                terms: Vec::new(),
            });
        }

        let subjects = subject_ids(ctx.scope_items);
        let glossary = if subjects.is_empty() {
            Vec::new()
        } else {
            self.terms.of_subjects(&subjects).await?
        };

        let sources = self.resolved_sources(ctx).await?;
        if sources.is_empty() {
            return Ok(ToolData::TableList {
                data_sources: Vec::new(),
                terms: glossary.into_iter().map(term_def).collect(),
            });
        }

        let briefs: HashMap<String, DataSource> = datasource::find_by_ids(&self.db, &sources)
            .await?
            .into_iter()
            .map(|source| (source.id.clone(), source))
            .collect();
        let mut groups = Vec::new();
        for source_id in &sources {
            let rows = table::list_by_datasource(&self.db, source_id).await?;
            if rows.is_empty() {
                continue;
            }
            let brief = briefs.get(source_id);
            groups.push(DataSourceDef {
                id: source_id.clone(),
                name: Some(
                    brief
                        .and_then(|source| source.name.clone())
                        .unwrap_or_else(|| source_id.clone()),
                ),
                db_type: brief.map(|source| source.db_type.as_str().to_string()),
                default_schema: brief.and_then(|source| source.default_schema.clone()),
                description: brief.and_then(|source| source.description.clone()),
                tables: rows
                    .iter()
                    .map(|row| TableDef {
                        table: row.name.clone(),
                        schema: row.schema.clone(),
                        description: row.description.clone(),
                        aliases: row.aliases.clone(),
                        columns: None,
                    })
                    .collect(),
            });
        }
        Ok(ToolData::TableList {
            data_sources: groups,
            terms: glossary.into_iter().map(term_def).collect(),
        })
    }

    /// `EXECUTE_SQL`: analyze, apply the slot's policy, check the scope, run.
    async fn execute_sql(&self, ctx: &ToolContext<'_>) -> CallResult<ToolData> {
        let args: ExecuteSqlArgs = bind(&ctx.arguments)?;
        let source_id = args.data_source_id.unwrap_or_default();
        let sql = args.sql.unwrap_or_default();
        let config = self.execute_config(ctx)?;

        let analysis = sql_analysis::analyze(&sql);
        if let Some(detail) = config.sql_policy.violation(&analysis) {
            return Err(ToolFailure::new(ToolError::SqlPolicyViolation, &[&detail]).into());
        }

        let source = self.require_source(&source_id).await?;
        self.scope
            .validate(
                ctx.scope_items,
                &source_id,
                &analysis.tables,
                source.default_schema.as_deref(),
            )
            .await?;

        let results = self
            .run(
                &source,
                &sql,
                Vec::new(),
                &analysis,
                config.timeout,
                config.max_rows,
            )
            .await?;
        Ok(ToolData::SqlExecution {
            executed_sql: sql,
            results,
            bindings: None,
        })
    }

    /// `PARAMETERIZED_SQL` — the one custom-tool type. The template is authored at config time, so
    /// no SQL policy runs here; the scope check still does, on the rendered statement.
    async fn parameterized_sql(&self, ctx: &ToolContext<'_>) -> CallResult<ToolData> {
        let config = self.param_config(ctx)?;
        let template_text = config.sql_template.clone().ok_or_else(|| {
            CallError::Internal(DatiError::with_message(
                ErrorCode::InternalError,
                "custom tool has no sql_template",
            ))
        })?;
        let compiled = template::parse(&template_text).map_err(internal)?;

        let mut params = ctx.arguments.clone();
        // The server's own variables win, which is why an author cannot spoof `_user.id`.
        params.extend(system_variable::resolve(Some(ctx.caller)));
        let values = template::values_of(&params);
        let prepared = template::render_sql(&compiled, &values).map_err(internal)?;
        let bound: Vec<Value> = prepared
            .bindings
            .iter()
            .map(|binding| binding.value.clone())
            .collect();

        let analysis = sql_analysis::analyze(&prepared.sql);
        let source = self
            .require_source(config.data_source_id.as_deref().unwrap_or_default())
            .await?;
        self.scope
            .validate(
                ctx.scope_items,
                config.data_source_id.as_deref().unwrap_or_default(),
                &analysis.tables,
                source.default_schema.as_deref(),
            )
            .await?;

        let results = self
            .run(
                &source,
                &prepared.sql,
                bound.clone(),
                &analysis,
                config.timeout,
                config.max_rows,
            )
            .await?;
        Ok(ToolData::SqlExecution {
            executed_sql: prepared.sql,
            results,
            bindings: Some(bound),
        })
    }

    /// `UPDATE_TABLE_INFO`: description and aliases of up to twenty tables, each independent.
    async fn update_table_info(&self, ctx: &ToolContext<'_>) -> CallResult<ToolData> {
        let args: UpdateTableInfoArgs = bind(&ctx.arguments)?;
        let mut results = Vec::new();
        for item in first_of(args.tables.unwrap_or_default(), |item| {
            locate_key(&[
                item.data_source_id.as_deref(),
                item.schema.as_deref(),
                item.table.as_deref(),
            ])
        }) {
            let entity = qualified_name(&[item.schema.as_deref(), item.table.as_deref()]);
            let outcome = match self.update_one_table(ctx, &item).await {
                Ok(result) => result,
                Err(CallError::Failure(failure)) => failed("TABLE", entity, &failure),
                Err(internal) => return Err(internal),
            };
            results.push(outcome);
        }
        Ok(ToolData::MetadataUpdate { results })
    }

    async fn update_one_table(
        &self,
        ctx: &ToolContext<'_>,
        item: &UpdateTableArg,
    ) -> CallResult<EntityUpdateResult> {
        let source_id = item.data_source_id.as_deref().unwrap_or("null");
        let table_name = item.table.as_deref().unwrap_or("null");
        self.scope
            .validate_data_source(ctx.scope_items, source_id)
            .await?;
        let target = self
            .resolve_table(source_id, item.schema.as_deref(), table_name)
            .await?
            .ok_or_else(|| {
                ToolFailure::new(
                    ToolError::EntityNotFound,
                    &[&format!("table {source_id}.{table_name}")],
                )
            })?;

        let old = value_map(target.description.as_deref(), &target.aliases);
        // An omitted write field keeps what is stored, because the Java executor merged before it
        // wrote and `TableService::update` then writes whatever it is handed.
        let description = item
            .description
            .clone()
            .or(target.description.clone())
            .filter(|text| !text.is_empty());
        let aliases = aliases_of(item.aliases.clone(), &target.aliases);
        self.tables
            .update(
                ctx.caller,
                &target.table_id,
                &TableUpdateRequest {
                    description: description.clone(),
                    aliases: Some(aliases.clone()),
                },
            )
            .await?;

        let new_value = value_map(description.as_deref(), &aliases);
        self.audit(
            ctx,
            &AuditTarget {
                entity_type: "TABLE",
                entity_id: &target.table_id,
                entity_name: table_name,
                change_type: "UPDATE",
            },
            Some(&old),
            &new_value,
        )
        .await?;
        Ok(success(
            "TABLE",
            qualified_name(&[item.schema.as_deref(), item.table.as_deref()]),
            old,
            new_value,
        ))
    }

    /// `UPDATE_COLUMN_INFO`: the same shape, one level down. Here the merge happens in the service,
    /// which is null-guarded, so the executor only reports the merged state afterwards.
    async fn update_column_info(&self, ctx: &ToolContext<'_>) -> CallResult<ToolData> {
        let args: UpdateColumnInfoArgs = bind(&ctx.arguments)?;
        let mut results = Vec::new();
        for item in first_of(args.columns.unwrap_or_default(), |item| {
            locate_key(&[
                item.data_source_id.as_deref(),
                item.schema.as_deref(),
                item.table.as_deref(),
                item.column.as_deref(),
            ])
        }) {
            let entity = qualified_name(&[
                item.schema.as_deref(),
                item.table.as_deref(),
                item.column.as_deref(),
            ]);
            let outcome = match self.update_one_column(ctx, &item).await {
                Ok(result) => result,
                Err(CallError::Failure(failure)) => failed("COLUMN", entity, &failure),
                Err(internal) => return Err(internal),
            };
            results.push(outcome);
        }
        Ok(ToolData::MetadataUpdate { results })
    }

    async fn update_one_column(
        &self,
        ctx: &ToolContext<'_>,
        item: &UpdateColumnArg,
    ) -> CallResult<EntityUpdateResult> {
        let source_id = item.data_source_id.as_deref().unwrap_or("null");
        let table_name = item.table.as_deref().unwrap_or("null");
        let column_name = item.column.as_deref().unwrap_or("null");
        self.scope
            .validate_data_source(ctx.scope_items, source_id)
            .await?;
        let target = self
            .resolve_column(source_id, item.schema.as_deref(), table_name, column_name)
            .await?
            .ok_or_else(|| {
                ToolFailure::new(
                    ToolError::EntityNotFound,
                    &[&format!("column {source_id}.{table_name}.{column_name}")],
                )
            })?;

        let old = value_map(target.description.as_deref(), &target.aliases);
        self.columns
            .update(
                ctx.caller,
                &target.column_id,
                &ColumnUpdateRequest {
                    description: item.description.clone(),
                    aliases: item.aliases.clone().map(flatten),
                    ..ColumnUpdateRequest::default()
                },
            )
            .await?;
        let description = item
            .description
            .as_deref()
            .or(target.description.as_deref())
            .filter(|text| !text.is_empty());
        let aliases = aliases_of(item.aliases.clone(), &target.aliases);
        let new_value = value_map(description, &aliases);

        self.audit(
            ctx,
            &AuditTarget {
                entity_type: "COLUMN",
                entity_id: &target.column_id,
                entity_name: column_name,
                change_type: "UPDATE",
            },
            Some(&old),
            &new_value,
        )
        .await?;
        Ok(success(
            "COLUMN",
            qualified_name(&[
                item.schema.as_deref(),
                item.table.as_deref(),
                item.column.as_deref(),
            ]),
            old,
            new_value,
        ))
    }

    /// `UPSERT_TERM`: create the term when the subject has none by that name, else enrich it. A
    /// rename is not offered — that stays an admin action.
    async fn upsert_term(&self, ctx: &ToolContext<'_>) -> CallResult<ToolData> {
        let args: UpsertTermArgs = bind(&ctx.arguments)?;
        let mut results = Vec::new();
        for item in first_of(args.terms.unwrap_or_default(), |item| {
            locate_key(&[item.subject_name.as_deref(), item.name.as_deref()])
        }) {
            let entity = qualified_name(&[item.subject_name.as_deref(), item.name.as_deref()]);
            let outcome = match self.upsert_one_term(ctx, &item).await {
                Ok(result) => result,
                Err(CallError::Failure(failure)) => failed("TERM", entity, &failure),
                Err(internal) => return Err(internal),
            };
            results.push(outcome);
        }
        Ok(ToolData::MetadataUpdate { results })
    }

    async fn upsert_one_term(
        &self,
        ctx: &ToolContext<'_>,
        item: &UpsertTermArg,
    ) -> CallResult<EntityUpdateResult> {
        let subject_name = item.subject_name.as_deref().unwrap_or("null");
        let name = item.name.as_deref().unwrap_or("null");
        let subject_id = self
            .scope
            .resolve_subject(ctx.scope_items, subject_name)
            .await?;
        let entity = qualified_name(&[Some(subject_name), Some(name)]);

        let existing = term::find_by_subject_and_name(&self.db, &subject_id, name).await?;
        let Some(row) = existing else {
            let request = CreateTermRequest {
                name: item.name.clone(),
                description: item.description.clone(),
                aliases: aliases_of(item.aliases.clone(), &[]),
            };
            let created = self.terms.create(ctx.caller, &subject_id, &request).await?;
            let new_value = value_map(request.description.as_deref(), &request.aliases);
            self.audit(
                ctx,
                &AuditTarget {
                    entity_type: "TERM",
                    entity_id: &created,
                    entity_name: name,
                    change_type: "CREATE",
                },
                None,
                &new_value,
            )
            .await?;
            return Ok(EntityUpdateResult {
                entity_type: "TERM",
                entity,
                success: true,
                change_type: Some("CREATE"),
                old: None,
                new_value: Some(new_value),
                error: None,
            });
        };

        let old = value_map(row.description.as_deref(), &row.aliases);
        let description = item
            .description
            .clone()
            .or(row.description.clone())
            .filter(|text| !text.is_empty());
        let aliases = aliases_of(item.aliases.clone(), &row.aliases);
        self.terms
            .update(
                ctx.caller,
                &row.id,
                &UpdateTermRequest {
                    name: Some(row.name.clone().unwrap_or_else(|| name.to_string())),
                    description: description.clone(),
                    aliases: Some(aliases.clone()),
                },
            )
            .await?;
        let new_value = value_map(description.as_deref(), &aliases);
        self.audit(
            ctx,
            &AuditTarget {
                entity_type: "TERM",
                entity_id: &row.id,
                entity_name: name,
                change_type: "UPDATE",
            },
            Some(&old),
            &new_value,
        )
        .await?;
        Ok(success("TERM", entity, old, new_value))
    }

    // ── shared steps ──

    /// The scope's data sources, read from the service's live scope rows the way
    /// `McpServiceDataScopeService.getResolvedDataSourceIds` did.
    async fn resolved_sources(&self, ctx: &ToolContext<'_>) -> CallResult<Vec<String>> {
        Ok(self
            .services
            .resolved_data_source_ids(ctx.service_id)
            .await?)
    }

    fn execute_config<'a>(&self, ctx: &'a ToolContext<'a>) -> CallResult<&'a ExecuteSqlConfig> {
        ctx.config.as_execute_sql().ok_or_else(|| {
            CallError::Internal(DatiError::with_message(
                ErrorCode::InternalError,
                "execute_sql slot has no EXECUTE_SQL config",
            ))
        })
    }

    fn param_config<'a>(&self, ctx: &'a ToolContext<'a>) -> CallResult<&'a ParamSqlConfig> {
        ctx.config.as_param_sql().ok_or_else(|| {
            CallError::Internal(DatiError::with_message(
                ErrorCode::InternalError,
                "custom tool has no PARAMETERIZED_SQL config",
            ))
        })
    }

    /// `getDataSourceInternal`: resolution only, never rights — the scope is the permission here. A
    /// missing source is the agent's `数据源不存在`, whose template drops the id it was handed.
    async fn require_source(&self, source_id: &str) -> CallResult<DataSource> {
        datasource::find(&self.db, source_id)
            .await?
            .ok_or_else(|| ToolFailure::new(ToolError::DataSourceNotFound, &[source_id]).into())
    }

    /// One statement, one outcome. Java looped over a JDBC result chain; `sqlx::Any` hands back one
    /// result per call, so a script of several statements is answered with the last one it ran.
    async fn run(
        &self,
        source: &DataSource,
        sql: &str,
        bindings: Vec<Value>,
        analysis: &sql_analysis::SqlAnalysisResult,
        timeout: i32,
        max_rows: i32,
    ) -> CallResult<Vec<StatementResult>> {
        let spec = DataSourceService::connection_of(source);
        let run = SqlRun {
            sql,
            bindings,
            reads: matches!(
                analysis.op_type,
                SqlOperationType::Select | SqlOperationType::Metadata
            ),
            timeout_secs: timeout,
            max_rows,
        };
        let outcome = self
            .data_sources
            .connectors()
            .run_sql(&spec, &run)
            .await
            .map_err(|error| ToolFailure::new(ToolError::SqlExecutionError, &[&error.reason()]))?;
        Ok(vec![match outcome {
            StatementOutcome::Select { columns, rows, .. } => {
                StatementResult::select(columns, rows)
            }
            StatementOutcome::Write { affected_rows } => {
                StatementResult::write(affected_rows as u64)
            }
        }])
    }

    /// A table plus its columns with sample values, as `TableMetadataService.getTableMeta` built it:
    /// the first row of the source whose name matches, an absent schema matching any.
    async fn table_meta(
        &self,
        source_id: &str,
        schema: Option<&str>,
        name: &str,
    ) -> Result<Option<TableMeta>, CallError> {
        let Some(row) = table::list_by_datasource(&self.db, source_id)
            .await?
            .into_iter()
            .find(|row| {
                row.name.as_deref() == Some(name) && matches_schema(row.schema.as_deref(), schema)
            })
        else {
            return Ok(None);
        };
        let columns = column::list_by_table(&self.db, &row.id).await?;
        let dictionary =
            column_value::list_by_tables(&self.db, std::slice::from_ref(&row.id), VALUE_QUERY_MAX)
                .await?;
        let mut samples: HashMap<String, Vec<String>> = HashMap::new();
        for value in &dictionary {
            let entry = samples.entry(value.column_name.clone()).or_default();
            if entry.len() < SAMPLE_VALUE_LIMIT {
                entry.push(value.value.clone());
            }
        }
        Ok(Some(TableMeta {
            description: row.description.clone(),
            aliases: row.aliases.clone(),
            columns: columns
                .iter()
                .map(|column| ColumnDef {
                    name: column.name.clone(),
                    column_type: column.column_type.clone(),
                    comment: column.description.clone(),
                    aliases: column.aliases.clone(),
                    sample_values: column
                        .name
                        .as_deref()
                        .and_then(|name| samples.get(name))
                        .cloned()
                        .unwrap_or_default(),
                })
                .collect(),
        }))
    }

    async fn resolve_table(
        &self,
        source_id: &str,
        schema: Option<&str>,
        name: &str,
    ) -> Result<Option<TableTarget>, CallError> {
        Ok(table::list_by_datasource(&self.db, source_id)
            .await?
            .into_iter()
            .find(|row| {
                row.name.as_deref() == Some(name) && matches_schema(row.schema.as_deref(), schema)
            })
            .map(|row| TableTarget {
                table_id: row.id,
                description: row.description,
                aliases: row.aliases,
            }))
    }

    async fn resolve_column(
        &self,
        source_id: &str,
        schema: Option<&str>,
        table_name: &str,
        column_name: &str,
    ) -> Result<Option<ColumnTarget>, CallError> {
        let Some(table) = self.resolve_table(source_id, schema, table_name).await? else {
            return Ok(None);
        };
        Ok(column::list_by_table(&self.db, &table.table_id)
            .await?
            .into_iter()
            .find(|column| column.name.as_deref() == Some(column_name))
            .map(|column| ColumnTarget {
                column_id: column.id,
                description: column.description,
                aliases: column.aliases,
            }))
    }

    /// One row of `mcp_metadata_audit_log` per changed entity. Nothing reads it back through an
    /// endpoint, which is how the Java version used it too.
    async fn audit(
        &self,
        ctx: &ToolContext<'_>,
        target: &AuditTarget<'_>,
        old: Option<&Map<String, Value>>,
        new_value: &Map<String, Value>,
    ) -> Result<(), CallError> {
        mcp_audit::insert(
            &self.db,
            &MetadataAuditLog {
                id: new_id(),
                audit: Audit::new(&ctx.caller.id),
                service_id: ctx.service_id.to_string(),
                tool_type: ctx.tool_type.as_str().to_string(),
                entity_type: target.entity_type.to_string(),
                entity_id: target.entity_id.to_string(),
                entity_name: target.entity_name.to_string(),
                change_type: target.change_type.to_string(),
                old_value: old.map(json_text),
                new_value: Some(json_text(new_value)),
            },
        )
        .await
        .map_err(CallError::Internal)
    }
}

/// Which entity an audit row describes.
struct AuditTarget<'a> {
    entity_type: &'a str,
    entity_id: &'a str,
    entity_name: &'a str,
    change_type: &'a str,
}

/// A `{description, aliases}` document as the audit log stores it: the JSON text, keys in the order
/// the map holds them.
fn json_text(map: &Map<String, Value>) -> String {
    Value::Object(map.clone()).to_string()
}

/// A term as the agent sees it: `TermBrief` holds plain strings, `TermDef` the Java record's
/// nullables.
fn term_def(brief: TermBrief) -> TermDef {
    TermDef {
        name: Some(brief.name),
        description: Some(brief.description),
        subject_name: Some(brief.subject_name),
    }
}

/// The `SUBJECT` rows of a scope, deduplicated in the order they were listed.
fn subject_ids(items: &[McpDataScope]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for item in items
        .iter()
        .filter(|item| item.scope_type == McpDataScopeType::Subject)
    {
        if !ids.contains(&item.reference_id) {
            ids.push(item.reference_id.clone());
        }
    }
    ids
}

/// The located table of `GET_TABLE_INFO`: only the three fields the tool reports.
struct TableMeta {
    description: Option<String>,
    aliases: Vec<String>,
    columns: Vec<ColumnDef>,
}

struct TableTarget {
    table_id: String,
    description: Option<String>,
    aliases: Vec<String>,
}

struct ColumnTarget {
    column_id: String,
    description: Option<String>,
    aliases: Vec<String>,
}

/// The Java `MetadataEntityResolver` match: a requested schema must be the stored one, no schema
/// asks for any.
fn matches_schema(stored: Option<&str>, requested: Option<&str>) -> bool {
    requested.is_none() || requested == stored
}

/// `QualifiedName.of`: non-blank parts joined by a dot, so a table without a schema is just its
/// name and a term is `subject.term`.
fn qualified_name(parts: &[Option<&str>]) -> String {
    parts
        .iter()
        .flatten()
        .filter(|part| !part.trim().is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(".")
}

/// The in-call dedupe key: a NUL join, because a table name may contain a dot or a space and two
/// different entities must not collapse into one key.
fn locate_key(parts: &[Option<&str>]) -> String {
    parts
        .iter()
        .map(|part| part.unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\u{0}")
}

/// Keeps the first item of each key, in the order the client listed them.
fn first_of<T>(items: Vec<T>, mut key: impl FnMut(&T) -> String) -> Vec<T> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();
    for item in items {
        if seen.insert(key(&item)) {
            unique.push(item);
        }
    }
    unique
}

/// A write that succeeded, reported with the pair it changed.
fn success(
    entity_type: &'static str,
    entity: String,
    old: Map<String, Value>,
    new_value: Map<String, Value>,
) -> EntityUpdateResult {
    EntityUpdateResult {
        entity_type,
        entity,
        success: true,
        change_type: Some("UPDATE"),
        old: Some(old),
        new_value: Some(new_value),
        error: None,
    }
}

/// A write the item could not complete: the failure text and category, and nothing else.
fn failed(entity_type: &'static str, entity: String, failure: &ToolFailure) -> EntityUpdateResult {
    EntityUpdateResult {
        entity_type,
        entity,
        success: false,
        change_type: None,
        old: None,
        new_value: None,
        error: Some(UpdateError::of(failure)),
    }
}

/// `null` means keep, a list means replace — including with nothing at all, which is how an agent
/// clears an alias set.
fn aliases_of(submitted: Option<Vec<Option<String>>>, current: &[String]) -> Vec<String> {
    submitted.map(flatten).unwrap_or_else(|| current.to_vec())
}

fn flatten(values: Vec<Option<String>>) -> Vec<String> {
    values.into_iter().flatten().collect()
}

/// A failure the agent is not allowed to see: the Java handler answered those as a JSON-RPC
/// internal error, so they carry a `DatiError` rather than a `ToolFailure`.
fn internal(message: String) -> CallError {
    CallError::Internal(DatiError::with_message(ErrorCode::InternalError, message))
}

#[cfg(test)]
mod tests {
    use sqlx::Row;

    use super::*;
    use crate::config::Config;
    use crate::model::datasource::{ColumnInfo, ColumnValue, DbType, TableInfo};
    use crate::model::semantic::Subject;
    use crate::model::user::User;
    use crate::repo::{column, column_value, mcp_service, subject, user as user_repo};
    use crate::service::permission::PermissionService;
    use crate::service::subject::SubjectService;
    use crate::time::new_id;
    use serde_json::json;

    const SERVICE: &str = "svc-exec";

    /// How the boot scope covers the seeded warehouse: not at all, by its source, or by the one
    /// subject that holds `orders`.
    enum Want {
        NoScope,
        BySource,
        BySubject,
    }

    struct Harness {
        db: Db,
        caller: AuthUser,
        executors: McpExecutors,
        source_id: String,
        scope_items: Vec<McpDataScope>,
    }

    fn scope(kind: McpDataScopeType, reference_id: &str) -> McpDataScope {
        McpDataScope {
            id: String::new(),
            service_id: SERVICE.to_string(),
            scope_type: kind,
            reference_id: reference_id.to_string(),
            audit: Audit::default(),
        }
    }

    /// A throwaway SQLite file the connector reads as the external data source, holding `orders`.
    async fn warehouse() -> String {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir
            .path()
            .join("warehouse.db")
            .to_string_lossy()
            .replace('\\', "/");
        std::mem::forget(dir);
        let url = format!("sqlite:{path}?mode=rwc");
        sqlx::any::install_default_drivers();
        let pool = sqlx::any::AnyPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("open warehouse");
        sqlx::query("CREATE TABLE orders (id INTEGER, status TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        for status in ["paid", "refunded", "paid"] {
            sqlx::query("INSERT INTO orders (id, status) VALUES (1, ?)")
                .bind(status)
                .execute(&pool)
                .await
                .unwrap();
        }
        url
    }

    async fn person(db: &Db, name: &str) -> AuthUser {
        let id = new_id();
        user_repo::insert(
            db,
            &User {
                id: id.clone(),
                name: name.into(),
                password: "hash".into(),
                display_name: None,
                audit: Audit::new(name),
            },
        )
        .await
        .unwrap();
        AuthUser {
            id,
            name: name.into(),
            display_name: None,
        }
    }

    /// The warehouse source, its `orders` table (schema-free) and `status` column, a two-value
    /// dictionary, and a `交易域` subject holding the table.
    async fn boot(want: Want) -> Harness {
        let db = Db::sqlite_temp().await;
        let caller = person(&db, "alice").await;
        let url = warehouse().await;

        let source = DataSource {
            id: new_id(),
            name: Some("warehouse".into()),
            description: Some("数仓".into()),
            deleted: false,
            connection_url: Some(url),
            db_type: DbType::Sqlite,
            user_name: None,
            password: None,
            default_schema: None,
            audit: Audit::new(&caller.id),
        };
        let source_id = source.id.clone();
        datasource::insert(&db, &source).await.unwrap();

        let orders_id = table::insert(
            &db,
            &TableInfo {
                id: new_id(),
                name: Some("orders".into()),
                description: Some("订单表".into()),
                deleted: false,
                data_source_id: source_id.clone(),
                schema: None,
                aliases: vec!["交易".into()],
                audit: Audit::new(&caller.id),
            },
        )
        .await
        .unwrap();

        column::insert(
            &db,
            &ColumnInfo {
                id: new_id(),
                name: Some("status".into()),
                description: Some("订单状态".into()),
                deleted: false,
                table_id: orders_id.clone(),
                column_type: Some("TEXT".into()),
                aliases: Vec::new(),
                extract_value_enabled: true,
                audit: Audit::new(&caller.id),
            },
        )
        .await
        .unwrap();
        for (sort_no, value) in ["paid", "refunded"].iter().enumerate() {
            column_value::insert(
                &db,
                &ColumnValue {
                    id: new_id(),
                    table_id: orders_id.clone(),
                    column_name: "status".into(),
                    value: (*value).into(),
                    synonyms: Vec::new(),
                    sort_no: sort_no as i64,
                    audit: Audit::new(&caller.id),
                },
            )
            .await
            .unwrap();
        }

        let subject_id = subject::insert(
            &db,
            &Subject {
                id: new_id(),
                name: Some("交易域".into()),
                description: None,
                deleted: false,
                datasource_id: source_id.clone(),
                aliases: Vec::new(),
                audit: Audit::new(&caller.id),
            },
        )
        .await
        .unwrap();
        subject::link(&db, &subject_id, &orders_id, &Audit::new(&caller.id))
            .await
            .unwrap();

        // `alice` is an admin, so the metadata writes clear the same `EDIT` gate a human would.
        let config = Config::from_source(|key| (key == "ADMIN_USERS").then(|| "alice".to_string()));
        let permissions = PermissionService::new(&config);
        let data_sources = DataSourceService::new(db.clone(), &config, permissions);
        let tables = TableService::new(db.clone(), data_sources.clone());
        let columns = ColumnService::new(db.clone(), data_sources.clone(), tables.clone());
        let subjects = SubjectService::new(db.clone(), data_sources.clone());
        let terms = TermService::new(db.clone(), subjects.clone());
        let search = SearchService::new(db.clone(), terms.clone());
        let scope_validator = ScopeValidator::new(db.clone());
        let services = McpServiceService::new(db.clone(), data_sources.clone(), subjects, &config);
        let executors = McpExecutors::new(
            db.clone(),
            scope_validator,
            services,
            data_sources,
            tables,
            columns,
            terms,
            search,
        );

        let mut scope_items = Vec::new();
        let wanted = match want {
            Want::NoScope => Vec::new(),
            Want::BySource => vec![(McpDataScopeType::DataSource, source_id.clone())],
            Want::BySubject => vec![(McpDataScopeType::Subject, subject_id.clone())],
        };
        for (kind, reference_id) in wanted {
            let row = scope(kind, &reference_id);
            mcp_service::insert_scope(&db, SERVICE, &row, &Audit::new(&caller.id))
                .await
                .unwrap();
            scope_items.push(row);
        }

        Harness {
            db,
            caller,
            executors,
            source_id,
            scope_items,
        }
    }

    fn tool_ctx<'a>(
        tool_type: McpToolType,
        config: &'a ToolConfig,
        arguments: Value,
        scope_items: &'a [McpDataScope],
        caller: &'a AuthUser,
    ) -> ToolContext<'a> {
        ToolContext {
            service_id: SERVICE,
            tool_type,
            config,
            arguments: arguments.as_object().cloned().unwrap_or_default(),
            scope_items,
            caller,
        }
    }

    async fn audit_rows(db: &Db) -> Vec<(String, String)> {
        let rows = sqlx::query(&db.sql(
            "SELECT entity_type, change_type FROM mcp_metadata_audit_log WHERE service_id = ? ORDER BY entity_type",
        ))
        .bind(SERVICE)
        .fetch_all(db.pool())
        .await
        .unwrap();
        rows.iter()
            .map(|row| {
                (
                    row.get::<String, _>("entity_type"),
                    row.get::<String, _>("change_type"),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn an_empty_scope_echoes_the_search_keywords_and_nothing_else() {
        let h = boot(Want::NoScope).await;
        let config = ToolConfig::default_for(McpToolType::SearchMetadata);
        let ctx = tool_ctx(
            McpToolType::SearchMetadata,
            &config,
            json!({"keywords": ["orders"]}),
            &h.scope_items,
            &h.caller,
        );
        let data = h.executors.execute(&ctx).await.unwrap();
        assert_eq!(
            serde_json::to_value(&data).unwrap(),
            json!({"type": "SEARCH_HIT", "keywords": ["orders"], "data_sources": [], "terms": []})
        );
    }

    #[tokio::test]
    async fn a_scoped_search_reports_the_matching_tables() {
        let h = boot(Want::BySource).await;
        let config = ToolConfig::default_for(McpToolType::SearchMetadata);
        let ctx = tool_ctx(
            McpToolType::SearchMetadata,
            &config,
            json!({"keywords": ["orders"]}),
            &h.scope_items,
            &h.caller,
        );
        let data = h.executors.execute(&ctx).await.unwrap();
        let value = serde_json::to_value(&data).unwrap();
        let groups = value["data_sources"].as_array().unwrap();
        assert_eq!(groups.len(), 1, "only the scoped source is searched");
        assert_eq!(groups[0]["id"], h.source_id);
        assert_eq!(groups[0]["name"], "warehouse");
        assert_eq!(groups[0]["tables"][0]["table"], "orders");
    }

    #[tokio::test]
    async fn list_tables_returns_the_inventory_without_columns() {
        let h = boot(Want::BySource).await;
        let config = ToolConfig::default_for(McpToolType::ListTables);
        let ctx = tool_ctx(
            McpToolType::ListTables,
            &config,
            json!({}),
            &h.scope_items,
            &h.caller,
        );
        let value = serde_json::to_value(h.executors.execute(&ctx).await.unwrap()).unwrap();
        assert_eq!(value["type"], "TABLE_LIST");
        let group = &value["data_sources"][0];
        assert_eq!(group["tables"][0]["table"], "orders");
        assert!(
            group["tables"][0].get("columns").is_none(),
            "LIST_TABLES is table-level only"
        );
    }

    #[tokio::test]
    async fn get_table_info_reports_a_known_table_and_drops_an_unknown_one() {
        let h = boot(Want::BySource).await;
        let source = &h.source_id;
        let config = ToolConfig::default_for(McpToolType::GetTableInfo);
        let ctx = tool_ctx(
            McpToolType::GetTableInfo,
            &config,
            json!({"tables": [
                {"data_source_id": source, "table": "orders"},
                {"data_source_id": source, "table": "ghost"},
            ]}),
            &h.scope_items,
            &h.caller,
        );
        let value = serde_json::to_value(h.executors.execute(&ctx).await.unwrap()).unwrap();
        let tables = value["tables"].as_array().unwrap();
        assert_eq!(
            tables.len(),
            1,
            "a table the store lacks is silently absent"
        );
        assert_eq!(tables[0]["table"], "orders");
        assert_eq!(tables[0]["columns"][0]["name"], "status");
        assert_eq!(
            tables[0]["columns"][0]["sample_values"],
            json!(["paid", "refunded"])
        );
    }

    #[tokio::test]
    async fn execute_sql_refuses_a_write_the_policy_denies_before_touching_anything() {
        let h = boot(Want::BySource).await;
        let config = ToolConfig::ExecuteSql(ExecuteSqlConfig::default());
        let ctx = tool_ctx(
            McpToolType::ExecuteSql,
            &config,
            json!({"data_source_id": h.source_id, "sql": "DELETE FROM orders"}),
            &h.scope_items,
            &h.caller,
        );
        let err = h.executors.execute(&ctx).await.unwrap_err();
        assert_eq!(err.to_string(), "SQL 操作被策略禁止：DELETE not allowed");
    }

    #[tokio::test]
    async fn execute_sql_names_a_missing_data_source() {
        let h = boot(Want::BySource).await;
        let config = ToolConfig::ExecuteSql(ExecuteSqlConfig::default());
        let ctx = tool_ctx(
            McpToolType::ExecuteSql,
            &config,
            json!({"data_source_id": "nope", "sql": "SELECT 1"}),
            &h.scope_items,
            &h.caller,
        );
        // The DataSourceNotFound template drops the id it was handed, as the Java text did.
        assert_eq!(
            h.executors.execute(&ctx).await.unwrap_err().to_string(),
            "数据源不存在"
        );
    }

    #[tokio::test]
    async fn execute_sql_refuses_a_table_outside_the_subject_scope() {
        // Subject scope: only `orders` is linked, so a source-wide table the subject lacks is refused.
        let h = boot(Want::BySubject).await;
        let config = ToolConfig::ExecuteSql(ExecuteSqlConfig::default());
        let ctx = tool_ctx(
            McpToolType::ExecuteSql,
            &config,
            json!({"data_source_id": h.source_id, "sql": "SELECT * FROM salaries"}),
            &h.scope_items,
            &h.caller,
        );
        let err = h.executors.execute(&ctx).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "数据源或表不在服务范围内：Table salaries not in scope"
        );
    }

    #[tokio::test]
    async fn execute_sql_runs_a_select_and_reports_its_rows() {
        let h = boot(Want::BySource).await;
        let config = ToolConfig::ExecuteSql(ExecuteSqlConfig::default());
        let ctx = tool_ctx(
            McpToolType::ExecuteSql,
            &config,
            json!({"data_source_id": h.source_id, "sql": "SELECT id, status FROM orders"}),
            &h.scope_items,
            &h.caller,
        );
        let value = serde_json::to_value(h.executors.execute(&ctx).await.unwrap()).unwrap();
        assert_eq!(value["type"], "SQL_EXECUTION");
        assert_eq!(value["executed_sql"], "SELECT id, status FROM orders");
        assert_eq!(value["results"][0]["total_rows"], 3);
        assert_eq!(value["results"][0]["columns"], json!(["id", "status"]));
    }

    #[tokio::test]
    async fn a_parameterized_tool_renders_then_binds_and_echos_the_values() {
        let h = boot(Want::BySource).await;
        let config = ToolConfig::ParameterizedSql(ParamSqlConfig {
            data_source_id: Some(h.source_id.clone()),
            sql_template: Some("SELECT id FROM orders WHERE status = {{status}}".into()),
            ..ParamSqlConfig::default()
        });
        let ctx = tool_ctx(
            McpToolType::ParameterizedSql,
            &config,
            json!({"status": "paid"}),
            &h.scope_items,
            &h.caller,
        );
        let value = serde_json::to_value(h.executors.execute(&ctx).await.unwrap()).unwrap();
        assert_eq!(
            value["executed_sql"],
            "SELECT id FROM orders WHERE status = ?"
        );
        assert_eq!(value["bindings"], json!(["paid"]));
        assert_eq!(value["results"][0]["total_rows"], 2);
    }

    #[tokio::test]
    async fn a_metadata_batch_reports_success_and_failure_and_audits_only_the_write() {
        let h = boot(Want::BySource).await;
        let source = &h.source_id;
        let config = ToolConfig::default_for(McpToolType::UpdateTableInfo);
        let ctx = tool_ctx(
            McpToolType::UpdateTableInfo,
            &config,
            json!({"tables": [
                {"data_source_id": source, "table": "orders", "description": "改过的说明", "aliases": ["订单"]},
                {"data_source_id": source, "table": "ghost"},
                {"data_source_id": source, "table": "orders"},
            ]}),
            &h.scope_items,
            &h.caller,
        );
        let value = serde_json::to_value(h.executors.execute(&ctx).await.unwrap()).unwrap();
        let results = value["results"].as_array().unwrap();
        assert_eq!(
            results.len(),
            2,
            "the duplicated `orders` item is deduped away"
        );
        assert_eq!(results[0]["success"], true);
        assert_eq!(results[0]["new"]["description"], "改过的说明");
        assert_eq!(results[1]["success"], false);
        assert_eq!(
            results[1]["error"]["message"],
            format!("表/列/术语不存在：table {source}.ghost")
        );
        // Only the completed write reaches the audit log.
        assert_eq!(
            audit_rows(&h.db).await,
            vec![("TABLE".to_string(), "UPDATE".to_string())]
        );
        // The stored comment is the merged result a later read sees.
        assert_eq!(
            table::list_by_datasource(&h.db, source)
                .await
                .unwrap()
                .into_iter()
                .find(|row| row.name.as_deref() == Some("orders"))
                .unwrap()
                .description
                .as_deref(),
            Some("改过的说明")
        );
    }

    #[tokio::test]
    async fn upsert_term_creates_a_new_term_then_updates_it() {
        let h = boot(Want::BySubject).await;

        let config = ToolConfig::default_for(McpToolType::UpsertTerm);
        let arguments = json!({"terms": [{"subject_name": "交易域", "name": "客单价", "description": "每单金额"}]});
        let ctx = tool_ctx(
            McpToolType::UpsertTerm,
            &config,
            arguments.clone(),
            &h.scope_items,
            &h.caller,
        );
        let first = serde_json::to_value(h.executors.execute(&ctx).await.unwrap()).unwrap();
        assert_eq!(first["results"][0]["change_type"], "CREATE");
        assert_eq!(first["results"][0]["entity"], "交易域.客单价");

        // The second call finds the term and enriches it, so it reports UPDATE.
        let again = tool_ctx(
            McpToolType::UpsertTerm,
            &config,
            arguments,
            &h.scope_items,
            &h.caller,
        );
        let second = serde_json::to_value(h.executors.execute(&again).await.unwrap()).unwrap();
        assert_eq!(second["results"][0]["change_type"], "UPDATE");

        assert_eq!(
            audit_rows(&h.db).await,
            vec![
                ("TERM".to_string(), "CREATE".to_string()),
                ("TERM".to_string(), "UPDATE".to_string())
            ]
        );
    }
}
