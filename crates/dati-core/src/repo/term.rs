//! The `term` table (the glossary) and its `term_relation` bindings onto tables and fields.

use sqlx::any::AnyRow;

use crate::db::{Db, read_json_list};
use crate::error::{DatiError, ErrorCode, Result};
use crate::model::base::{Audit, PageReq, write_json_list};
use crate::model::semantic::{Term, TermRelation, TermRelationType};

use super::{Filter, Statement, count, flag, in_clause, keyword_match, optional, required};

const COLUMNS: &str = "id, created_by, created_at, updated_by, updated_at, name, description, \
     deleted, subject_id, aliases";

const TABLE: &str = "term";

const RELATIONS: &str = "term_relation";

/// The bound table's name and schema come along, the way the Java service joined them in.
const RELATION_COLUMNS: &str = "r.id, r.term_id, r.entity_type, r.table_id, r.field_name, \
     t.name AS table_name, t.table_schema AS bound_schema";

pub fn to_term(row: &AnyRow) -> Result<Term> {
    Ok(Term {
        id: required(row, "id")?,
        name: optional(row, "name"),
        description: optional(row, "description"),
        deleted: flag(row, "deleted"),
        subject_id: required(row, "subject_id")?,
        aliases: read_json_list(row, "aliases"),
        audit: Audit {
            created_by: optional(row, "created_by"),
            created_at: optional(row, "created_at"),
            updated_by: optional(row, "updated_by"),
            updated_at: optional(row, "updated_at"),
        },
    })
}

pub async fn insert(db: &Db, term: &Term) -> Result<String> {
    Statement::new(format!(
        "INSERT INTO {TABLE} (id, created_by, created_at, updated_by, updated_at, name, \
         description, deleted, subject_id, aliases) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    ))
    .bind(term.id.clone())
    .bind(term.audit.created_by.clone())
    .bind(term.audit.created_at.clone())
    .bind(term.audit.updated_by.clone())
    .bind(term.audit.updated_at.clone())
    .bind(term.name.clone())
    .bind(term.description.clone())
    .bind(term.deleted)
    .bind(term.subject_id.clone())
    .bind(write_json_list(&term.aliases))
    .execute(db)
    .await?;
    Ok(term.id.clone())
}

pub async fn update(db: &Db, term: &Term) -> Result<()> {
    Statement::new(format!(
        "UPDATE {TABLE} SET name = ?, description = ?, aliases = ?, updated_by = ?, updated_at = ? \
         WHERE id = ?"
    ))
    .bind(term.name.clone())
    .bind(term.description.clone())
    .bind(write_json_list(&term.aliases))
    .bind(term.audit.updated_by.clone())
    .bind(term.audit.updated_at.clone())
    .bind(term.id.clone())
    .execute(db)
    .await?;
    Ok(())
}

pub async fn find(db: &Db, id: &str) -> Result<Option<Term>> {
    let row = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE id = ? AND deleted = ?"
    ))
    .bind(id)
    .bind(false)
    .fetch_optional(db)
    .await?;
    row.as_ref().map(to_term).transpose()
}

/// `GET /v1/subjects/{id}/terms`: newest first inside one subject.
pub async fn page(
    db: &Db,
    subject_id: &str,
    keyword: Option<&str>,
    page_req: PageReq,
) -> Result<(Vec<Term>, i64)> {
    let mut filter = Filter::default()
        .add("deleted = ?", [false.into()])
        .add("subject_id = ?", [subject_id.into()]);
    if let Some(keyword) = keyword.filter(|v| !v.trim().is_empty()) {
        filter = filter.add(
            "LOWER(name) LIKE ?",
            [format!("%{}%", keyword.to_lowercase()).into()],
        );
    }
    let total = count(&filter.count(TABLE), db).await?;
    let rows = filter
        .select(COLUMNS, TABLE)
        .push_text(" ORDER BY updated_at DESC, id DESC LIMIT ? OFFSET ?")
        .bind(page_req.size)
        .bind(page_req.offset())
        .fetch_all(db)
        .await?;
    Ok((rows.iter().map(to_term).collect::<Result<Vec<_>>>()?, total))
}

/// The locate key of `UPSERT_TERM`: one name per subject.
pub async fn find_by_subject_and_name(
    db: &Db,
    subject_id: &str,
    name: &str,
) -> Result<Option<Term>> {
    let row = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE subject_id = ? AND name = ? AND deleted = ?"
    ))
    .bind(subject_id)
    .bind(name)
    .bind(false)
    .fetch_optional(db)
    .await?;
    row.as_ref().map(to_term).transpose()
}

pub async fn remove(db: &Db, id: &str) -> Result<bool> {
    let affected = Statement::new(format!("DELETE FROM {TABLE} WHERE id = ?"))
        .bind(id)
        .execute(db)
        .await?;
    Ok(affected > 0)
}

/// Ids to `{name, description}` pairs, so a search hit can label the terms it matched.
pub async fn briefs(db: &Db, ids: &[String]) -> Result<Vec<Term>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; ids.len()].join(", ");
    let mut statement = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE id IN ({placeholders})"
    ));
    for id in ids {
        statement = statement.bind(id.clone());
    }
    let rows = statement.fetch_all(db).await?;
    rows.iter().map(to_term).collect()
}

/// Every term of the given subjects: the glossary a scoped MCP service exposes in full.
pub async fn list_by_subjects(db: &Db, subject_ids: &[String]) -> Result<Vec<Term>> {
    if subject_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; subject_ids.len()].join(", ");
    let mut statement = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE subject_id IN ({placeholders}) AND deleted = ? \
         ORDER BY created_at ASC, id ASC"
    ));
    for id in subject_ids {
        statement = statement.bind(id.clone());
    }
    let rows = statement.bind(false).fetch_all(db).await?;
    rows.iter().map(to_term).collect()
}

/// The terms of the given subjects whose name, description or aliases carry one of the keywords:
/// the Java `TERM` documents of the search index, which were scoped by `entity.subjectId`.
pub async fn matching(db: &Db, subject_ids: &[String], keywords: &[String]) -> Result<Vec<Term>> {
    if subject_ids.is_empty() || keywords.is_empty() {
        return Ok(Vec::new());
    }
    let (clause, binds) = in_clause("subject_id", subject_ids);
    let (pattern, pattern_binds) = keyword_match(keywords, &["name", "description", "aliases"]);
    let mut statement = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE deleted = ? AND {clause} AND {pattern} \
         ORDER BY created_at ASC, id ASC"
    ));
    statement = statement.bind(false);
    for bind in binds.into_iter().chain(pattern_binds) {
        statement = statement.bind(bind);
    }
    let rows = statement.fetch_all(db).await?;
    rows.iter().map(to_term).collect()
}

// ---- relations --------------------------------------------------------------------------------

pub async fn insert_relation(db: &Db, relation: &TermRelation, audit: &Audit) -> Result<String> {
    Statement::new(format!(
        "INSERT INTO {RELATIONS} (id, created_by, created_at, updated_by, updated_at, term_id, \
         entity_type, table_id, field_name) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
    ))
    .bind(relation.id.clone())
    .bind(audit.created_by.clone())
    .bind(audit.created_at.clone())
    .bind(audit.updated_by.clone())
    .bind(audit.updated_at.clone())
    .bind(relation.term_id.clone())
    .bind(relation.entity_type.as_str())
    .bind(relation.table_id.clone())
    .bind(relation.field_name.clone())
    .execute(db)
    .await?;
    Ok(relation.id.clone())
}

pub fn to_relation(row: &AnyRow) -> Result<TermRelation> {
    let stored = required(row, "entity_type")?;
    let entity_type = TermRelationType::from_stored(&stored).ok_or_else(|| {
        tracing::error!(stored, "term relation has an unknown entity type");
        DatiError::new(ErrorCode::SmTermRelationInvalid, [stored])
    })?;
    Ok(TermRelation {
        id: required(row, "id")?,
        term_id: required(row, "term_id")?,
        entity_type,
        table_id: required(row, "table_id")?,
        field_name: optional(row, "field_name"),
        table_name: optional(row, "table_name"),
        schema: optional(row, "bound_schema"),
    })
}

/// Batch load: one query for a page of terms' bindings, so the list never fans out per row.
pub async fn list_relations(db: &Db, term_ids: &[String]) -> Result<Vec<TermRelation>> {
    if term_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; term_ids.len()].join(", ");
    let mut statement = Statement::new(format!(
        "SELECT {RELATION_COLUMNS} FROM {RELATIONS} r \
         LEFT JOIN table_info t ON t.id = r.table_id \
         WHERE r.term_id IN ({placeholders}) ORDER BY r.created_at ASC, r.id ASC"
    ));
    for id in term_ids {
        statement = statement.bind(id.clone());
    }
    let rows = statement.fetch_all(db).await?;
    rows.iter().map(to_relation).collect()
}

pub async fn find_relation_by_field(
    db: &Db,
    term_id: &str,
    table_id: &str,
    field_name: &str,
) -> Result<Option<TermRelation>> {
    let row = Statement::new(format!(
        "SELECT {RELATION_COLUMNS} FROM {RELATIONS} r \
         LEFT JOIN table_info t ON t.id = r.table_id \
         WHERE r.term_id = ? AND r.table_id = ? AND r.field_name = ?"
    ))
    .bind(term_id)
    .bind(table_id)
    .bind(field_name)
    .fetch_optional(db)
    .await?;
    row.as_ref().map(to_relation).transpose()
}

pub async fn remove_relations_by_term(db: &Db, term_id: &str) -> Result<u64> {
    let affected = Statement::new(format!("DELETE FROM {RELATIONS} WHERE term_id = ?"))
        .bind(term_id)
        .execute(db)
        .await?;
    Ok(affected)
}

pub async fn remove_relations_by_field(
    db: &Db,
    term_id: &str,
    table_id: &str,
    field_name: &str,
) -> Result<u64> {
    let affected = Statement::new(format!(
        "DELETE FROM {RELATIONS} WHERE term_id = ? AND table_id = ? AND field_name = ?"
    ))
    .bind(term_id)
    .bind(table_id)
    .bind(field_name)
    .execute(db)
    .await?;
    Ok(affected)
}

pub async fn remove_relations_by_table(db: &Db, term_id: &str, table_id: &str) -> Result<u64> {
    let affected = Statement::new(format!(
        "DELETE FROM {RELATIONS} WHERE term_id = ? AND table_id = ?"
    ))
    .bind(term_id)
    .bind(table_id)
    .execute(db)
    .await?;
    Ok(affected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::datasource::TableInfo;
    use crate::repo::table;
    use crate::time::new_id;

    fn term(subject: &str, name: &str, aliases: &[&str]) -> Term {
        Term {
            id: new_id(),
            name: Some(name.into()),
            description: Some("成交总额".into()),
            deleted: false,
            subject_id: subject.into(),
            aliases: aliases.iter().map(|v| v.to_string()).collect(),
            audit: Audit::new("u-1"),
        }
    }

    fn relation(term_id: &str, table_id: &str, field: Option<&str>) -> TermRelation {
        TermRelation {
            id: new_id(),
            term_id: term_id.into(),
            entity_type: if field.is_some() {
                TermRelationType::Field
            } else {
                TermRelationType::Table
            },
            table_id: table_id.into(),
            field_name: field.map(str::to_string),
            table_name: None,
            schema: None,
        }
    }

    async fn one_table(db: &Db) -> TableInfo {
        let row = TableInfo {
            id: new_id(),
            name: Some("orders".into()),
            description: None,
            deleted: false,
            data_source_id: "ds-1".into(),
            schema: Some("main".into()),
            aliases: Vec::new(),
            audit: Audit::new("u-1"),
        };
        table::insert(db, &row).await.unwrap();
        row
    }

    #[tokio::test]
    async fn a_term_lives_in_exactly_one_subject() {
        let db = Db::sqlite_temp().await;
        let row = term("s-1", "GMV", &["成交额"]);
        insert(&db, &row).await.unwrap();
        assert_eq!(find(&db, &row.id).await.unwrap().unwrap(), row);

        let (hits, total) = page(&db, "s-1", Some("gmv"), PageReq { page: 1, size: 10 })
            .await
            .unwrap();
        assert_eq!((total, hits.len()), (1, 1), "keyword search ignores case");
        assert_eq!(
            page(&db, "s-2", None, PageReq { page: 1, size: 10 })
                .await
                .unwrap()
                .0
                .len(),
            0
        );

        let mut edited = row.clone();
        edited.name = Some("GMV".into());
        edited.aliases = Vec::new();
        update(&db, &edited).await.unwrap();
        assert!(
            find(&db, &row.id)
                .await
                .unwrap()
                .unwrap()
                .aliases
                .is_empty()
        );

        assert!(remove(&db, &row.id).await.unwrap());
        assert!(find(&db, &row.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn relations_carry_the_bound_tables_label() {
        let db = Db::sqlite_temp().await;
        let table = one_table(&db).await;
        let row = term("s-1", "GMV", &[]);
        insert(&db, &row).await.unwrap();
        insert_relation(&db, &relation(&row.id, &table.id, None), &Audit::new("u-1"))
            .await
            .unwrap();
        insert_relation(
            &db,
            &relation(&row.id, &table.id, Some("amount")),
            &Audit::new("u-1"),
        )
        .await
        .unwrap();
        insert_relation(
            &db,
            &relation("ghost", "gone", Some("x")),
            &Audit::new("u-1"),
        )
        .await
        .unwrap();

        let all = list_relations(&db, std::slice::from_ref(&row.id))
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
        assert!(
            all.iter()
                .all(|r| r.table_name.as_deref() == Some("orders")),
            "the join labels both bindings"
        );
        assert!(all.iter().any(|r| r.entity_type == TermRelationType::Table));

        let found = find_relation_by_field(&db, &row.id, &table.id, "amount")
            .await
            .unwrap();
        assert_eq!(found.unwrap().entity_type, TermRelationType::Field);
        assert!(
            find_relation_by_field(&db, &row.id, &table.id, "missing")
                .await
                .unwrap()
                .is_none()
        );

        assert_eq!(
            remove_relations_by_field(&db, &row.id, &table.id, "amount")
                .await
                .unwrap(),
            1
        );
        assert_eq!(remove_relations_by_term(&db, &row.id).await.unwrap(), 1);
        assert!(list_relations(&db, &[row.id]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unknown_entity_type_is_a_bad_relation() {
        let db = Db::sqlite_temp().await;
        let sql = db.sql(
            "INSERT INTO term_relation (id, term_id, entity_type, table_id) VALUES (?, ?, ?, ?)",
        );
        sqlx::query(&sql)
            .bind(new_id())
            .bind("t-1")
            .bind("VIEW")
            .bind("tb-1")
            .execute(db.pool())
            .await
            .unwrap();
        let err = list_relations(&db, &["t-1".into()]).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::SmTermRelationInvalid);
        assert_eq!(err.status(), 400);
    }

    #[tokio::test]
    async fn briefs_and_subject_wide_listings_skip_missing_ids() {
        let db = Db::sqlite_temp().await;
        let first = term("s-1", "GMV", &[]);
        let second = term("s-2", "AOV", &[]);
        insert(&db, &first).await.unwrap();
        insert(&db, &second).await.unwrap();

        let found = briefs(&db, &[first.id.clone(), "gone".into()])
            .await
            .unwrap();
        assert_eq!(
            found.iter().map(|t| &t.id).collect::<Vec<_>>(),
            vec![&first.id]
        );
        assert!(briefs(&db, &[]).await.unwrap().is_empty());

        let in_scope = list_by_subjects(&db, &["s-1".into(), "s-2".into()])
            .await
            .unwrap();
        assert_eq!(in_scope.len(), 2);
        assert!(list_by_subjects(&db, &[]).await.unwrap().is_empty());
    }
}
