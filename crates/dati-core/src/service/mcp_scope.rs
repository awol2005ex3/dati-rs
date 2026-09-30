//! The service scope an MCP tool call runs inside: which data sources and tables the published
//! configuration may touch, and which subject a term write may name.
//!
//! The Java `ScopeValidator` walked the scope rows of the *active snapshot*, so every check here
//! reads the rows it was handed rather than the service's current draft. Matching is exact and
//! case-sensitive: a scope that names `PUBLIC.orders` does not cover `public.orders`.

use std::collections::HashSet;

use crate::db::Db;
use crate::model::datasource::TableInfo;
use crate::model::mcp::{McpDataScope, McpDataScopeType};
use crate::model::mcp_tool_data::{CallResult, ToolError, ToolFailure};
use crate::repo::{subject, table};
use crate::sql_analysis::TableRef;

/// A scope violation, with the English detail the Java exception put into the Chinese template.
fn out_of_scope(detail: impl AsRef<str>) -> ToolFailure {
    ToolFailure::new(ToolError::ScopeViolation, &[detail.as_ref()])
}

const NO_SCOPE: &str = "No data scope configured";

/// The tables and sources one scope list covers.
#[derive(Clone)]
pub struct ScopeValidator {
    db: Db,
}

impl ScopeValidator {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// The check a SQL tool runs: the source is covered, and every table the statement names is too.
    pub async fn validate(
        &self,
        items: &[McpDataScope],
        data_source_id: &str,
        table_refs: &[TableRef],
        default_schema: Option<&str>,
    ) -> CallResult<()> {
        self.require_data_source(items, data_source_id).await?;
        if table_refs.is_empty() {
            return Ok(());
        }
        let allowed = self.allowed_tables(items, data_source_id).await?;
        for reference in table_refs {
            if allowed.contains(reference) || resolves(reference, default_schema, &allowed) {
                continue;
            }
            return Err(
                out_of_scope(format!("Table {} not in scope", reference.qualified_name())).into(),
            );
        }
        Ok(())
    }

    /// The check a metadata write runs: only the source has to be covered, because writing a comment
    /// is not reading the table's data.
    pub async fn validate_data_source(
        &self,
        items: &[McpDataScope],
        data_source_id: &str,
    ) -> CallResult<()> {
        self.require_data_source(items, data_source_id).await
    }

    /// The id of the in-scope subject with this name. The first scope row that matches wins, which is
    /// what the Java loop did when a service listed the same subject twice.
    pub async fn resolve_subject(
        &self,
        items: &[McpDataScope],
        subject_name: &str,
    ) -> CallResult<String> {
        if items.is_empty() {
            return Err(out_of_scope(NO_SCOPE).into());
        }
        for item in items
            .iter()
            .filter(|item| item.scope_type == McpDataScopeType::Subject)
        {
            if let Some(row) = subject::find(&self.db, &item.reference_id).await?
                && row.name.as_deref() == Some(subject_name)
            {
                return Ok(row.id);
            }
        }
        Err(out_of_scope(format!("Subject {subject_name} not in service scope")).into())
    }

    async fn require_data_source(
        &self,
        items: &[McpDataScope],
        data_source_id: &str,
    ) -> CallResult<()> {
        if items.is_empty() {
            return Err(out_of_scope(NO_SCOPE).into());
        }
        if self.covered_sources(items).await?.contains(data_source_id) {
            return Ok(());
        }
        Err(out_of_scope(format!("Data source {data_source_id} not in service scope")).into())
    }

    /// A source is covered by a `DATA_SOURCE` row of its own, or by any subject that holds one of
    /// its tables.
    async fn covered_sources(&self, items: &[McpDataScope]) -> CallResult<HashSet<String>> {
        let mut covered = HashSet::new();
        for item in items {
            match item.scope_type {
                McpDataScopeType::DataSource => {
                    covered.insert(item.reference_id.clone());
                }
                McpDataScopeType::Subject => {
                    for row in self.subject_tables(&item.reference_id).await? {
                        covered.insert(row.data_source_id);
                    }
                }
            }
        }
        Ok(covered)
    }

    /// The tables the scope allows inside one source, as the SQL could have named them.
    async fn allowed_tables(
        &self,
        items: &[McpDataScope],
        data_source_id: &str,
    ) -> CallResult<HashSet<TableRef>> {
        let mut allowed = HashSet::new();
        for item in items {
            let rows = match item.scope_type {
                McpDataScopeType::DataSource if item.reference_id == data_source_id => {
                    table::list_by_datasource(&self.db, data_source_id).await?
                }
                McpDataScopeType::Subject => self.subject_tables(&item.reference_id).await?,
                _ => continue,
            };
            allowed.extend(
                rows.into_iter()
                    .filter(|row| row.data_source_id == data_source_id)
                    .map(|row| TableRef {
                        schema: row.schema,
                        name: row.name.unwrap_or_default(),
                    }),
            );
        }
        Ok(allowed)
    }

    async fn subject_tables(&self, subject_id: &str) -> CallResult<Vec<TableInfo>> {
        let ids = subject::table_ids(&self.db, subject_id).await?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(table::find_by_ids(&self.db, &ids).await?)
    }
}

/// A statement that named no schema may still mean an allowed table of the source's default schema,
/// and a statement that named one may still mean an allowed table the catalog stored without it.
fn resolves(
    reference: &TableRef,
    default_schema: Option<&str>,
    allowed: &HashSet<TableRef>,
) -> bool {
    if reference.schema.is_none()
        && let Some(schema) = default_schema
        && allowed.contains(&TableRef::qualified(schema, reference.name.clone()))
    {
        return true;
    }
    reference.schema.is_some() && allowed.contains(&TableRef::unqualified(&reference.name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::base::Audit;
    use crate::model::datasource::{DataSource, DbType};
    use crate::model::semantic::Subject;
    use crate::repo::datasource;

    fn scope(scope_type: McpDataScopeType, reference_id: &str) -> McpDataScope {
        McpDataScope {
            id: String::new(),
            service_id: "svc-1".to_string(),
            scope_type,
            reference_id: reference_id.to_string(),
            audit: Audit::default(),
        }
    }

    fn table_row(id: &str, name: &str, schema: Option<&str>) -> TableInfo {
        TableInfo {
            id: id.to_string(),
            name: Some(name.to_string()),
            description: None,
            deleted: false,
            data_source_id: "ds-1".to_string(),
            schema: schema.map(str::to_string),
            aliases: Vec::new(),
            audit: Audit::default(),
        }
    }

    /// One source, three tables of it, and a subject that holds only `orders`.
    async fn seed(db: &Db) {
        datasource::insert(
            db,
            &DataSource {
                id: "ds-1".to_string(),
                name: Some("仓库".to_string()),
                description: None,
                deleted: false,
                connection_url: Some("sqlite:seed.db".to_string()),
                db_type: DbType::Sqlite,
                user_name: None,
                password: None,
                default_schema: Some("public".to_string()),
                audit: Audit::default(),
            },
        )
        .await
        .unwrap();
        table::insert(db, &table_row("tbl-1", "orders", Some("public")))
            .await
            .unwrap();
        table::insert(db, &table_row("tbl-2", "legacy", None))
            .await
            .unwrap();
        table::insert(db, &table_row("tbl-3", "salaries", Some("public")))
            .await
            .unwrap();
        subject::insert(
            db,
            &Subject {
                id: "sub-1".to_string(),
                name: Some("交易".to_string()),
                description: None,
                deleted: false,
                datasource_id: "ds-1".to_string(),
                aliases: Vec::new(),
                audit: Audit::default(),
            },
        )
        .await
        .unwrap();
        subject::link(db, "sub-1", "tbl-1", &Audit::default())
            .await
            .unwrap();
    }

    async fn validator() -> ScopeValidator {
        let db = Db::sqlite_temp().await;
        seed(&db).await;
        ScopeValidator::new(db)
    }

    #[tokio::test]
    async fn a_source_scope_covers_every_table_of_its_source() {
        let validator = validator().await;
        let items = [scope(McpDataScopeType::DataSource, "ds-1")];

        // Every table the catalog knows for the source is covered, stored schema or none.
        for name in ["orders", "salaries", "legacy"] {
            validator
                .validate(
                    &items,
                    "ds-1",
                    &[TableRef::unqualified(name)],
                    Some("public"),
                )
                .await
                .unwrap();
        }
        validator.validate(&items, "ds-1", &[], None).await.unwrap();
        assert_eq!(
            validator
                .validate(
                    &items,
                    "ds-1",
                    &[TableRef::unqualified("missing")],
                    Some("public")
                )
                .await
                .unwrap_err()
                .to_string(),
            "数据源或表不在服务范围内：Table missing not in scope"
        );
        // A source the scope never named is refused before any table is looked at.
        assert_eq!(
            validator
                .validate(&items, "ds-2", &[], None)
                .await
                .unwrap_err()
                .to_string(),
            "数据源或表不在服务范围内：Data source ds-2 not in service scope"
        );
        assert_eq!(
            validator
                .validate(&[], "ds-1", &[], None)
                .await
                .unwrap_err()
                .to_string(),
            "数据源或表不在服务范围内：No data scope configured"
        );
    }

    #[tokio::test]
    async fn a_subject_scope_covers_only_its_own_tables() {
        let validator = validator().await;
        let items = [scope(McpDataScopeType::Subject, "sub-1")];

        // `orders` is linked to the subject; `salaries` is not, though both live in `ds-1`.
        validator
            .validate(
                &items,
                "ds-1",
                &[TableRef::unqualified("orders")],
                Some("public"),
            )
            .await
            .unwrap();
        assert_eq!(
            validator
                .validate(
                    &items,
                    "ds-1",
                    &[TableRef::unqualified("salaries")],
                    Some("public")
                )
                .await
                .unwrap_err()
                .to_string(),
            "数据源或表不在服务范围内：Table salaries not in scope"
        );
        // Without the source's default schema the bare name never reaches the stored `public.orders`.
        assert!(
            validator
                .validate(&items, "ds-1", &[TableRef::unqualified("orders")], None)
                .await
                .is_err()
        );
        // The subject still covers its whole source for a statement that names no table.
        validator
            .validate_data_source(&items, "ds-1")
            .await
            .unwrap();
        assert!(
            validator
                .validate_data_source(&items, "ds-2")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_schema_is_tried_in_both_directions_before_a_table_is_refused() {
        let validator = validator().await;
        let items = [scope(McpDataScopeType::DataSource, "ds-1")];

        // The catalog stores `public.orders`; the SQL said only `orders` against a `public` source.
        validator
            .validate(
                &items,
                "ds-1",
                &[TableRef::unqualified("orders")],
                Some("public"),
            )
            .await
            .unwrap();
        // The catalog stores no schema for `legacy`, so any schema the SQL named is forgiven.
        validator
            .validate(
                &items,
                "ds-1",
                &[TableRef::qualified("anything", "legacy")],
                None,
            )
            .await
            .unwrap();
        assert!(
            validator
                .validate(
                    &items,
                    "ds-1",
                    &[TableRef::qualified("other", "orders")],
                    None
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_subject_is_found_by_its_exact_name() {
        let validator = validator().await;
        let items = [
            scope(McpDataScopeType::DataSource, "ds-1"),
            scope(McpDataScopeType::Subject, "sub-1"),
        ];

        assert_eq!(
            validator.resolve_subject(&items, "交易").await.unwrap(),
            "sub-1"
        );
        assert_eq!(
            validator
                .resolve_subject(&items, "交易 ")
                .await
                .unwrap_err()
                .to_string(),
            "数据源或表不在服务范围内：Subject 交易  not in service scope"
        );
        assert_eq!(
            validator
                .resolve_subject(&[], "交易")
                .await
                .unwrap_err()
                .to_string(),
            "数据源或表不在服务范围内：No data scope configured"
        );
    }
}
