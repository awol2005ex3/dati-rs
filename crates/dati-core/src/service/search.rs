//! Metadata search: the Java `SemanticSearchService` rebuilt on relational tables.
//!
//! The Java version asked Elasticsearch for `SemanticSearchDocument`s of type `TABLE`, `FIELD`,
//! `FIELD_VALUE` and `TERM`. Those four document kinds are now four real tables, so a hit is the
//! union of the matching rows inside the caller's scope, resolved to whole tables and grouped
//! under their data sources. The response shape is the one the MCP tool has always returned.

use std::collections::{HashMap, HashSet};

use crate::db::Db;
use crate::error::Result;
use crate::model::datasource::{ColumnInfo, ColumnValue, DataSource, TableInfo};
use crate::model::semantic::{SearchColumn, SearchDataSource, SearchTable, SemanticSearchResult};
use crate::repo::{column, column_value, datasource, table, term};

use super::term::TermService;

/// How many tables one response carries: the Java search index asked for 50 documents.
const MAX_RESULTS: usize = 50;
/// How many sample values a column reports.
const SAMPLE_VALUE_LIMIT: usize = 5;
/// How many dictionary rows a search read takes.
const VALUE_QUERY_MAX: i64 = 5000;

/// A `(table, column)` pair, the key the value dictionaries are grouped by.
type ColumnKey = (String, String);

#[derive(Clone)]
pub struct SearchService {
    db: Db,
    terms: TermService,
}

impl SearchService {
    pub fn new(db: Db, terms: TermService) -> Self {
        Self { db, terms }
    }

    /// Search `keywords` inside the tables of `datasource_ids` and of the subjects in
    /// `subject_ids`. Neither keywords nor scope means no results, as in the Java index query.
    pub async fn search(
        &self,
        keywords: &[String],
        datasource_ids: &[String],
        subject_ids: &[String],
    ) -> Result<SemanticSearchResult> {
        let keywords = usable_keywords(keywords);
        if keywords.is_empty() || (datasource_ids.is_empty() && subject_ids.is_empty()) {
            return Ok(SemanticSearchResult::default());
        }

        let scoped = table::list_in_scope(&self.db, datasource_ids, subject_ids).await?;
        let scoped_ids: Vec<String> = scoped.iter().map(|row| row.id.clone()).collect();

        let mut hits = HitList::default();
        for row in &scoped {
            if mentioned(&keywords, row) {
                hits.add(&row.id);
            }
        }
        for hit in column::matching(&self.db, &scoped_ids, &keywords).await? {
            hits.add(&hit.table_id);
        }
        let matched_values = column_value::matching(&self.db, &scoped_ids, &keywords).await?;
        for value in &matched_values {
            hits.add(&value.table_id);
        }

        let term_ids: Vec<String> = term::matching(&self.db, subject_ids, &keywords)
            .await?
            .iter()
            .map(|row| row.id.clone())
            .collect();
        // A hit term's own tables join the response even when they sit outside the scope: the
        // Java service added every relation table id without re-checking it.
        for relation in term::list_relations(&self.db, &term_ids).await? {
            hits.add(&relation.table_id);
        }

        let table_ids = hits.capped();
        let data_sources = self.groups(&table_ids, &matched_values).await?;
        let terms = self.terms.briefs(&term_ids).await?;
        Ok(SemanticSearchResult {
            data_sources,
            terms,
        })
    }

    /// The hit tables with their columns and value dictionaries, grouped under their source.
    async fn groups(
        &self,
        table_ids: &[String],
        matched_values: &[ColumnValue],
    ) -> Result<Vec<SearchDataSource>> {
        if table_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = ordered(table::find_by_ids(&self.db, table_ids).await?, table_ids);
        let columns = column::list_by_tables(&self.db, table_ids).await?;
        let dictionary = column_value::list_by_tables(&self.db, table_ids, VALUE_QUERY_MAX).await?;

        let mut columns_of: HashMap<String, Vec<&ColumnInfo>> = HashMap::new();
        for column in &columns {
            columns_of
                .entry(column.table_id.clone())
                .or_default()
                .push(column);
        }
        let mut samples: HashMap<ColumnKey, Vec<String>> = HashMap::new();
        for value in &dictionary {
            let entry = samples
                .entry((value.table_id.clone(), value.column_name.clone()))
                .or_default();
            if entry.len() < SAMPLE_VALUE_LIMIT {
                entry.push(value.value.clone());
            }
        }
        let mut matched: HashMap<ColumnKey, Vec<String>> = HashMap::new();
        for value in matched_values {
            matched
                .entry((value.table_id.clone(), value.column_name.clone()))
                .or_default()
                .push(value.value.clone());
        }

        let mut sources_of: Vec<String> = Vec::new();
        let mut tables_of: HashMap<String, Vec<SearchTable>> = HashMap::new();
        for row in &rows {
            let table = SearchTable {
                table_id: row.id.clone(),
                schema: row.schema.clone(),
                table_name: row.name.clone(),
                description: row.description.clone(),
                aliases: row.aliases.clone(),
                data_source_id: row.data_source_id.clone(),
                columns: columns_of
                    .get(&row.id)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .map(|column| SearchColumn {
                        name: column.name.clone(),
                        column_type: column.column_type.clone(),
                        comment: column.description.clone(),
                        aliases: column.aliases.clone(),
                        sample_values: sample_values(
                            &row.id,
                            column.name.as_deref().unwrap_or_default(),
                            &matched,
                            &samples,
                        ),
                    })
                    .collect(),
            };
            if !tables_of.contains_key(&row.data_source_id) {
                sources_of.push(row.data_source_id.clone());
            }
            tables_of
                .entry(row.data_source_id.clone())
                .or_default()
                .push(table);
        }

        let briefs: HashMap<String, DataSource> = datasource::find_by_ids(&self.db, &sources_of)
            .await?
            .into_iter()
            .map(|source| (source.id.clone(), source))
            .collect();
        Ok(sources_of
            .into_iter()
            .map(|id| {
                let brief = briefs.get(&id);
                SearchDataSource {
                    data_source_name: brief
                        .and_then(|source| source.name.clone())
                        .unwrap_or_else(|| id.clone()),
                    db_type: brief.map(|source| source.db_type.as_str().to_string()),
                    default_schema: brief.and_then(|source| source.default_schema.clone()),
                    description: brief.and_then(|source| source.description.clone()),
                    tables: tables_of.remove(&id).unwrap_or_default(),
                    data_source_id: id,
                }
            })
            .collect())
    }
}

/// The matched values first, then the plain dictionary up to five. A column whose values alone
/// already fill the quota keeps them all, which is what the Java merge did.
fn sample_values(
    table_id: &str,
    column_name: &str,
    matched: &HashMap<ColumnKey, Vec<String>>,
    samples: &HashMap<ColumnKey, Vec<String>>,
) -> Vec<String> {
    let key = || (table_id.to_string(), column_name.to_string());
    let mut values = matched.get(&key()).cloned().unwrap_or_default();
    for value in samples.get(&key()).map(Vec::as_slice).unwrap_or_default() {
        if values.len() >= SAMPLE_VALUE_LIMIT {
            break;
        }
        if !values.contains(value) {
            values.push(value.clone());
        }
    }
    values
}

/// Only the keywords that carry something to match on; the Java query joined them with a space,
/// and a blank one could only ever widen the search.
fn usable_keywords(keywords: &[String]) -> Vec<String> {
    keywords
        .iter()
        .map(|keyword| keyword.trim())
        .filter(|keyword| !keyword.is_empty())
        .map(str::to_string)
        .collect()
}

/// The Java `TABLE` documents carried the table name and its aliases as keywords and its
/// description as the second matched field.
fn mentioned(keywords: &[String], row: &TableInfo) -> bool {
    let mut haystack = Vec::new();
    for value in [&row.name, &row.description].into_iter().flatten() {
        haystack.push(value.to_lowercase());
    }
    haystack.extend(row.aliases.iter().map(|alias| alias.to_lowercase()));
    keywords.iter().any(|keyword| {
        haystack
            .iter()
            .any(|value| value.contains(&keyword.to_lowercase()))
    })
}

/// `IN` reads come back in whatever order the database likes; the hit order is the one the
/// matching produced, so the response is restated against it.
fn ordered(rows: Vec<TableInfo>, order: &[String]) -> Vec<TableInfo> {
    let rank: HashMap<&str, usize> = order
        .iter()
        .enumerate()
        .map(|(index, id)| (id.as_str(), index))
        .collect();
    let mut rows = rows;
    rows.sort_by_key(|row| rank.get(row.id.as_str()).copied().unwrap_or(usize::MAX));
    rows
}

#[derive(Default)]
struct HitList {
    ids: Vec<String>,
    seen: HashSet<String>,
}

impl HitList {
    fn add(&mut self, id: &str) {
        if !id.is_empty() && self.seen.insert(id.to_string()) {
            self.ids.push(id.to_string());
        }
    }

    fn capped(self) -> Vec<String> {
        let mut ids = self.ids;
        ids.truncate(MAX_RESULTS);
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthUser;
    use crate::config::Config;
    use crate::db::Db;
    use crate::model::base::Audit;
    use crate::model::datasource::DbType;
    use crate::model::semantic::{
        CreateSubjectRequest, CreateTermRequest, LinkTermRelationRequest, TermRelationType,
    };
    use crate::model::user::User;
    use crate::repo::{subject, user};
    use crate::service::datasource::DataSourceService;
    use crate::service::permission::PermissionService;
    use crate::service::subject::SubjectService;
    use crate::service::term::TermService;
    use crate::time::new_id;

    struct Store {
        db: Db,
        source_id: String,
        other_source_id: String,
        subject_id: String,
        orders_id: String,
    }

    async fn insert_person(db: &Db, name: &str) -> AuthUser {
        let id = new_id();
        user::insert(
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

    async fn source(db: &Db, caller: &AuthUser, name: &str) -> String {
        let id = new_id();
        datasource::insert(
            db,
            &DataSource {
                id: id.clone(),
                name: Some(name.into()),
                description: Some("数仓".into()),
                deleted: false,
                connection_url: Some("sqlite:demo.db".into()),
                db_type: DbType::Sqlite,
                user_name: None,
                password: None,
                default_schema: Some("main".into()),
                audit: Audit::new(&caller.id),
            },
        )
        .await
        .unwrap();
        id
    }

    async fn registered_table(db: &Db, caller: &AuthUser, source_id: &str, name: &str) -> String {
        let id = new_id();
        table::insert(
            db,
            &TableInfo {
                id: id.clone(),
                name: Some(name.into()),
                description: Some("订单表".into()),
                deleted: false,
                data_source_id: source_id.into(),
                schema: Some("main".into()),
                aliases: vec!["交易".into()],
                audit: Audit::new(&caller.id),
            },
        )
        .await
        .unwrap();
        id
    }

    async fn column_of(db: &Db, caller: &AuthUser, table_id: &str, name: &str) {
        column::insert(
            db,
            &ColumnInfo {
                id: new_id(),
                name: Some(name.into()),
                description: Some("金额".into()),
                deleted: false,
                table_id: table_id.into(),
                column_type: Some("TEXT".into()),
                aliases: Vec::new(),
                extract_value_enabled: true,
                audit: Audit::new(&caller.id),
            },
        )
        .await
        .unwrap();
    }

    async fn dictionary(db: &Db, caller: &AuthUser, table_id: &str, values: &[(&str, &str)]) {
        for (sort_no, (value, synonyms)) in values.iter().enumerate() {
            column_value::insert(
                db,
                &ColumnValue {
                    id: new_id(),
                    table_id: table_id.into(),
                    column_name: "status".into(),
                    value: (*value).into(),
                    synonyms: if synonyms.is_empty() {
                        Vec::new()
                    } else {
                        vec![(*synonyms).to_string()]
                    },
                    sort_no: sort_no as i64,
                    audit: Audit::new(&caller.id),
                },
            )
            .await
            .unwrap();
        }
    }

    /// One warehouse table with a status dictionary, one foreign table nobody may find, and a
    /// subject whose term points back at the warehouse table.
    async fn boot() -> (Store, SearchService) {
        let db = Db::sqlite_temp().await;
        let caller = insert_person(&db, "alice").await;
        let source_id = source(&db, &caller, "warehouse").await;
        let other_source_id = source(&db, &caller, "crm").await;
        let orders_id = registered_table(&db, &caller, &source_id, "orders").await;
        registered_table(&db, &caller, &other_source_id, "orders_archive").await;
        column_of(&db, &caller, &orders_id, "status").await;
        dictionary(
            &db,
            &caller,
            &orders_id,
            &[("paid", "已支付"), ("refund", ""), ("cod", "")],
        )
        .await;

        let config = Config::from_source(|_| None);
        let permissions = PermissionService::new(&config);
        let data_sources = DataSourceService::new(db.clone(), &config, permissions);
        let subjects = SubjectService::new(db.clone(), data_sources);
        let terms = TermService::new(db.clone(), subjects.clone());
        let subject_id = subjects
            .create(
                &caller,
                &CreateSubjectRequest {
                    name: Some("交易域".into()),
                    description: None,
                    datasource_id: Some(source_id.clone()),
                    aliases: Vec::new(),
                },
            )
            .await
            .unwrap();
        subject::link(&db, &subject_id, &orders_id, &Audit::new("alice"))
            .await
            .unwrap();
        let term_id = terms
            .create(
                &caller,
                &subject_id,
                &CreateTermRequest {
                    name: Some("成交总额".into()),
                    description: Some("一段时间的成交额".into()),
                    aliases: vec!["GMV".into()],
                },
            )
            .await
            .unwrap();
        terms
            .link(
                &caller,
                &term_id,
                &LinkTermRelationRequest {
                    entity_type: Some(TermRelationType::Table),
                    table_id: Some(orders_id.clone()),
                    field_name: None,
                },
            )
            .await
            .unwrap();

        let store = Store {
            db,
            source_id,
            other_source_id,
            subject_id,
            orders_id,
        };
        let search = SearchService::new(store.db.clone(), terms);
        (store, search)
    }

    fn keywords(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[tokio::test]
    async fn a_search_without_keywords_or_scope_finds_nothing() {
        let (store, search) = boot().await;
        let scope = [store.source_id.clone()];
        assert!(
            search
                .search(&[], &scope, &[])
                .await
                .unwrap()
                .data_sources
                .is_empty(),
            "the Java index returned no documents for an empty query"
        );
        let found = search
            .search(&keywords(&["orders"]), &[], &[])
            .await
            .unwrap();
        assert!(found.data_sources.is_empty() && found.terms.is_empty());
    }

    #[tokio::test]
    async fn a_table_hit_reports_its_source_and_columns() {
        let (store, search) = boot().await;
        let found = search
            .search(
                &keywords(&["orders"]),
                std::slice::from_ref(&store.source_id),
                std::slice::from_ref(&store.subject_id),
            )
            .await
            .unwrap();

        assert_eq!(found.data_sources.len(), 1);
        let group = &found.data_sources[0];
        assert_eq!(group.data_source_id, store.source_id);
        assert_eq!(group.data_source_name, "warehouse");
        assert_eq!(group.db_type.as_deref(), Some("SQLITE"));
        assert_eq!(group.description.as_deref(), Some("数仓"));
        assert_eq!(group.tables.len(), 1, "the other source is out of scope");
        let table = &group.tables[0];
        assert_eq!(table.table_id, store.orders_id);
        assert_eq!(table.table_name.as_deref(), Some("orders"));
        assert_eq!(table.aliases, ["交易"]);
        assert_eq!(table.columns.len(), 1);
        assert_eq!(table.columns[0].comment.as_deref(), Some("金额"));
        assert_eq!(table.columns[0].sample_values, ["paid", "refund", "cod"]);
    }

    #[tokio::test]
    async fn a_value_hit_leads_with_the_value_that_matched() {
        let (store, search) = boot().await;
        let scope = [store.source_id.clone()];

        let found = search
            .search(&keywords(&["refund"]), &scope, &[])
            .await
            .unwrap();
        let values = &found.data_sources[0].tables[0].columns[0].sample_values;
        assert_eq!(values, &["refund".to_string(), "paid".into(), "cod".into()]);

        let by_subject = store.subject_id.clone();
        let found = search
            .search(&keywords(&["已支付"]), &[scope[0].clone()], &[by_subject])
            .await
            .unwrap();
        let hit = &found.data_sources[0].tables[0];
        assert_eq!(
            hit.columns[0].sample_values[0], "paid",
            "a synonym matched, but the dictionary reports the canonical value"
        );
        assert_eq!(found.terms, vec![], "the keyword matched no glossary entry");
    }

    #[tokio::test]
    async fn a_term_hit_labels_its_subject_and_pulls_in_its_tables() {
        let (store, search) = boot().await;
        let found = search
            .search(
                &keywords(&["GMV"]),
                &[],
                std::slice::from_ref(&store.subject_id),
            )
            .await
            .unwrap();

        assert_eq!(found.terms.len(), 1);
        assert_eq!(found.terms[0].name, "成交总额");
        assert_eq!(found.terms[0].subject_name, "交易域");
        assert_eq!(found.terms[0].description, "一段时间的成交额");
        assert_eq!(found.data_sources[0].tables[0].table_id, store.orders_id);

        let none = search
            .search(
                &keywords(&["GMV"]),
                std::slice::from_ref(&store.other_source_id),
                &[],
            )
            .await
            .unwrap();
        assert!(
            none.terms.is_empty(),
            "glossary hits are scoped by subject, never by data source"
        );
    }
}
