//! Terms: the glossary entries of a subject, and the table and field bindings that ground them.
//!
//! This is the Java `TermService`. A term has no rights of its own: every operation is authorised
//! through its subject, which is how the Java `requireSubject(subjectId, …)` calls worked.

use std::collections::HashMap;

use crate::auth::AuthUser;
use crate::db::Db;
use crate::error::{DatiError, ErrorCode, Result};
use crate::model::base::{Audit, PageReq, PageResponse};
use crate::model::permission::Permission;
use crate::model::semantic::{
    CreateTermRequest, LinkTermRelationRequest, Term, TermBrief, TermRelation, TermRelationType,
    TermView, UpdateTermRequest,
};
use crate::repo::{subject, term};
use crate::time::new_id;

use super::datasource::author_names;
use super::subject::SubjectService;

#[derive(Clone)]
pub struct TermService {
    db: Db,
    subjects: SubjectService,
}

impl TermService {
    pub fn new(db: Db, subjects: SubjectService) -> Self {
        Self { db, subjects }
    }

    /// The stored row, or `SM002`.
    pub async fn load(&self, id: &str) -> Result<Term> {
        term::find(&self.db, id)
            .await?
            .ok_or_else(|| DatiError::new(ErrorCode::SmTermNotFound, [id]))
    }

    /// `PermissionService.requireSubject(term.subjectId, permission)`: the term resolves first, so
    /// a bad id reports `SM002` rather than a denial for a subject that was never named.
    async fn authorize_of(
        &self,
        caller: &AuthUser,
        row: &Term,
        permission: Permission,
    ) -> Result<()> {
        self.subjects
            .authorize(caller, &row.subject_id, permission)
            .await?;
        Ok(())
    }

    /// `POST /v1/subjects/{subjectId}/terms`: writing into a subject's glossary needs `EDIT`
    /// on that subject.
    pub async fn create(
        &self,
        caller: &AuthUser,
        subject_id: &str,
        request: &CreateTermRequest,
    ) -> Result<String> {
        request.validate()?;
        self.subjects
            .authorize(caller, subject_id, Permission::Edit)
            .await?;
        let row = Term {
            id: new_id(),
            name: request.name.clone(),
            description: request.description.clone(),
            deleted: false,
            subject_id: subject_id.to_string(),
            aliases: request.aliases.clone(),
            audit: Audit::new(&caller.id),
        };
        term::insert(&self.db, &row).await
    }

    /// `PUT /v1/terms/{id}`: the name must be supplied, the rest keeps its value when absent.
    pub async fn update(
        &self,
        caller: &AuthUser,
        id: &str,
        request: &UpdateTermRequest,
    ) -> Result<()> {
        request.validate()?;
        let mut current = self.load(id).await?;
        self.authorize_of(caller, &current, Permission::Edit)
            .await?;
        current.name = request.name.clone();
        if let Some(description) = request.description.clone() {
            current.description = Some(description);
        }
        if let Some(aliases) = request.aliases.clone() {
            current.aliases = aliases;
        }
        current.audit.touch(&caller.id);
        term::update(&self.db, &current).await
    }

    /// `DELETE /v1/terms/{id}`: the bindings go with the term.
    pub async fn remove(&self, caller: &AuthUser, id: &str) -> Result<()> {
        let row = self.load(id).await?;
        self.authorize_of(caller, &row, Permission::Edit).await?;
        term::remove_relations_by_term(&self.db, id).await?;
        term::remove(&self.db, id).await?;
        Ok(())
    }

    /// `GET /v1/terms/{id}`.
    pub async fn get(&self, caller: &AuthUser, id: &str) -> Result<TermView> {
        let row = self.load(id).await?;
        self.authorize_of(caller, &row, Permission::View).await?;
        let names = author_names(&self.db, [&row.audit]).await?;
        let relations = self.relations_of(&[id.to_string()]).await?;
        Ok(TermView::new(&row, &names)
            .with_relations(relations.get(id).map(Vec::as_slice).unwrap_or_default()))
    }

    /// `GET /v1/subjects/{subjectId}/terms`: a page of terms, each carrying its bindings, which are
    /// read in one query rather than one per row.
    pub async fn list(
        &self,
        caller: &AuthUser,
        subject_id: &str,
        keyword: Option<&str>,
        page: PageReq,
    ) -> Result<PageResponse<TermView>> {
        self.subjects
            .authorize(caller, subject_id, Permission::View)
            .await?;
        let (rows, total) = term::page(&self.db, subject_id, keyword, page).await?;
        let names = author_names(&self.db, rows.iter().map(|row| &row.audit)).await?;
        let ids: Vec<String> = rows.iter().map(|row| row.id.clone()).collect();
        let relations = self.relations_of(&ids).await?;
        Ok(PageResponse::new(
            rows.iter()
                .map(|row| {
                    TermView::new(row, &names).with_relations(
                        relations
                            .get(&row.id)
                            .map(Vec::as_slice)
                            .unwrap_or_default(),
                    )
                })
                .collect(),
            total,
            page.page,
            page.size,
        ))
    }

    /// `POST /v1/terms/{id}/relations`. A `FIELD` binding must name the field, and the table has to
    /// belong to the subject: the Java service checked both before inserting.
    pub async fn link(
        &self,
        caller: &AuthUser,
        term_id: &str,
        request: &LinkTermRelationRequest,
    ) -> Result<()> {
        request.validate()?;
        let row = self.load(term_id).await?;
        self.authorize_of(caller, &row, Permission::Edit).await?;
        let entity_type = request.entity_type.unwrap();
        let table_id = request.table_id.clone().unwrap_or_default();
        let field_name = request.field_name.clone();
        if entity_type == TermRelationType::Field && field_name.is_none() {
            return Err(DatiError::new(
                ErrorCode::InvalidParameter,
                ["fieldName is required"],
            ));
        }
        if !subject::contains(&self.db, &row.subject_id, &table_id).await? {
            return Err(DatiError::new(
                ErrorCode::SmTableNotInSubject,
                [&table_id, &row.subject_id],
            ));
        }
        if let Some(field_name) = field_name.as_deref()
            && term::find_relation_by_field(&self.db, term_id, &table_id, field_name)
                .await?
                .is_some()
        {
            return Err(DatiError::new(
                ErrorCode::InvalidParameter,
                ["Term relation already exists"],
            ));
        }
        let relation = TermRelation {
            id: new_id(),
            term_id: term_id.to_string(),
            entity_type,
            table_id,
            field_name,
            table_name: None,
            schema: None,
        };
        term::insert_relation(&self.db, &relation, &Audit::new(&caller.id)).await?;
        Ok(())
    }

    /// `DELETE /v1/terms/{id}/relations/{tableId}/{fieldName}`: without a field name every binding
    /// onto the table goes, and nothing is said when there was none to begin with.
    pub async fn unlink(
        &self,
        caller: &AuthUser,
        term_id: &str,
        table_id: &str,
        field_name: Option<&str>,
    ) -> Result<()> {
        let row = self.load(term_id).await?;
        self.authorize_of(caller, &row, Permission::Edit).await?;
        match field_name {
            None => {
                term::remove_relations_by_table(&self.db, term_id, table_id).await?;
            }
            Some(field_name) => {
                if term::find_relation_by_field(&self.db, term_id, table_id, field_name)
                    .await?
                    .is_none()
                {
                    // The Java `NOT_FOUND` template carries no placeholder, so this detail never
                    // reached the response body.
                    return Err(DatiError::new(
                        ErrorCode::NotFound,
                        ["Term relation not found"],
                    ));
                }
                term::remove_relations_by_field(&self.db, term_id, table_id, field_name).await?;
            }
        }
        Ok(())
    }

    /// The bindings of a batch of terms, grouped by term id.
    pub async fn relations_of(
        &self,
        term_ids: &[String],
    ) -> Result<HashMap<String, Vec<TermRelation>>> {
        let rows = term::list_relations(&self.db, term_ids).await?;
        let mut grouped: HashMap<String, Vec<TermRelation>> = HashMap::new();
        for relation in rows {
            grouped
                .entry(relation.term_id.clone())
                .or_default()
                .push(relation);
        }
        Ok(grouped)
    }

    /// Term ids to the lightweight `{name, description, subject_name}` the search reports.
    pub async fn briefs(&self, term_ids: &[String]) -> Result<Vec<TermBrief>> {
        let rows = term::briefs(&self.db, term_ids).await?;
        self.labelled(&rows).await
    }

    /// Every term of the given subjects, labelled the same way: the glossary a scoped MCP service
    /// hands out without a keyword search.
    pub async fn of_subjects(&self, subject_ids: &[String]) -> Result<Vec<TermBrief>> {
        let rows = term::list_by_subjects(&self.db, subject_ids).await?;
        self.labelled(&rows).await
    }

    async fn labelled(&self, rows: &[Term]) -> Result<Vec<TermBrief>> {
        let subject_ids: Vec<String> = rows.iter().map(|row| row.subject_id.clone()).collect();
        let names = self.subjects.name_map(&subject_ids).await?;
        Ok(rows
            .iter()
            .map(|row| {
                TermBrief::new(
                    row.name.as_deref(),
                    row.description.as_deref(),
                    names.get(&row.subject_id).map(String::as_str),
                )
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::datasource::{DataSource, DbType, TableInfo};
    use crate::model::semantic::CreateSubjectRequest;
    use crate::model::user::User;
    use crate::repo::{datasource, table, user};
    use crate::service::datasource::DataSourceService;
    use crate::service::permission::PermissionService;

    fn authed(id: &str, name: &str) -> AuthUser {
        AuthUser {
            id: id.into(),
            name: name.into(),
            display_name: None,
        }
    }

    async fn person(db: &Db, name: &str) -> AuthUser {
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
        authed(&id, name)
    }

    struct Fixture {
        db: Db,
        terms: TermService,
        subjects: SubjectService,
        caller: AuthUser,
        source_id: String,
    }

    async fn boot() -> Fixture {
        let db = Db::sqlite_temp().await;
        let config = Config::from_source(|_| None);
        let permissions = PermissionService::new(&config);
        let data_sources = DataSourceService::new(db.clone(), &config, permissions);
        let subjects = SubjectService::new(db.clone(), data_sources);
        let terms = TermService::new(db.clone(), subjects.clone());
        let caller = person(&db, "alice").await;
        let source = DataSource {
            id: new_id(),
            name: Some("warehouse".into()),
            description: None,
            deleted: false,
            connection_url: Some("sqlite:demo.db".into()),
            db_type: DbType::Sqlite,
            user_name: None,
            password: None,
            default_schema: Some("main".into()),
            audit: Audit::new(&caller.id),
        };
        datasource::insert(&db, &source).await.unwrap();
        Fixture {
            db,
            terms,
            subjects,
            caller,
            source_id: source.id,
        }
    }

    impl Fixture {
        async fn subject(&self, name: &str) -> String {
            self.subjects
                .create(
                    &self.caller,
                    &CreateSubjectRequest {
                        name: Some(name.into()),
                        description: None,
                        datasource_id: Some(self.source_id.clone()),
                        aliases: Vec::new(),
                    },
                )
                .await
                .unwrap()
        }

        async fn term(&self, subject_id: &str, name: &str) -> String {
            self.terms
                .create(
                    &self.caller,
                    subject_id,
                    &CreateTermRequest {
                        name: Some(name.into()),
                        description: Some("成交总额".into()),
                        aliases: vec!["GMV".into()],
                    },
                )
                .await
                .unwrap()
        }

        async fn table(&self, name: &str) -> TableInfo {
            let row = TableInfo {
                id: new_id(),
                name: Some(name.into()),
                description: None,
                deleted: false,
                data_source_id: self.source_id.clone(),
                schema: Some("main".into()),
                aliases: Vec::new(),
                audit: Audit::new(&self.caller.id),
            };
            table::insert(&self.db, &row).await.unwrap();
            row
        }
    }

    fn link(
        table_id: &str,
        entity_type: TermRelationType,
        field: Option<&str>,
    ) -> LinkTermRelationRequest {
        LinkTermRelationRequest {
            entity_type: Some(entity_type),
            table_id: Some(table_id.into()),
            field_name: field.map(str::to_string),
        }
    }

    #[tokio::test]
    async fn a_term_belongs_to_its_subject_and_carries_its_bindings() {
        let app = boot().await;
        let subject_id = app.subject("orders").await;
        let term_id = app.term(&subject_id, "gmv").await;

        let err = app.terms.load("nope").await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::SmTermNotFound);
        assert_eq!(err.status(), 404);

        let view = app.terms.get(&app.caller, &term_id).await.unwrap();
        assert_eq!(view.subject_id, subject_id);
        assert_eq!(view.created_user_name.as_deref(), Some("alice"));
        assert!(view.relations.is_empty());

        let table = app.table("orders").await;
        subject::link(&app.db, &subject_id, &table.id, &Audit::new("alice"))
            .await
            .unwrap();
        app.terms
            .link(
                &app.caller,
                &term_id,
                &link(&table.id, TermRelationType::Table, None),
            )
            .await
            .unwrap();
        app.terms
            .link(
                &app.caller,
                &term_id,
                &link(&table.id, TermRelationType::Field, Some("amount")),
            )
            .await
            .unwrap();

        let view = app.terms.get(&app.caller, &term_id).await.unwrap();
        assert_eq!(view.relations.len(), 2);
        assert!(
            view.relations
                .iter()
                .all(|r| r.table_name.as_deref() == Some("orders"))
        );
    }

    #[tokio::test]
    async fn linking_needs_a_field_and_a_bound_table() {
        let app = boot().await;
        let subject_id = app.subject("orders").await;
        let term_id = app.term(&subject_id, "gmv").await;
        let table = app.table("orders").await;

        let err = app
            .terms
            .link(
                &app.caller,
                &term_id,
                &link(&table.id, TermRelationType::Field, None),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidParameter);
        assert_eq!(
            err.message(),
            "Request parameter error: fieldName is required"
        );

        let err = app
            .terms
            .link(
                &app.caller,
                &term_id,
                &link(&table.id, TermRelationType::Table, None),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::SmTableNotInSubject);
        assert_eq!(err.status(), 400);
        assert_eq!(
            err.message(),
            format!("Table {} does not belong to subject {subject_id}", table.id),
            "the Java service reported the table id here, not its name"
        );

        subject::link(&app.db, &subject_id, &table.id, &Audit::new("alice"))
            .await
            .unwrap();
        app.terms
            .link(
                &app.caller,
                &term_id,
                &link(&table.id, TermRelationType::Table, None),
            )
            .await
            .unwrap();
        app.terms
            .link(
                &app.caller,
                &term_id,
                &link(&table.id, TermRelationType::Table, None),
            )
            .await
            .unwrap();
        app.terms
            .link(
                &app.caller,
                &term_id,
                &link(&table.id, TermRelationType::Field, Some("amount")),
            )
            .await
            .unwrap();
        let err = app
            .terms
            .link(
                &app.caller,
                &term_id,
                &link(&table.id, TermRelationType::Field, Some("amount")),
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.message(),
            "Request parameter error: Term relation already exists",
            "only field bindings are deduplicated, as in the Java service"
        );
        assert_eq!(
            app.terms
                .relations_of(std::slice::from_ref(&term_id))
                .await
                .unwrap()[&term_id]
                .len(),
            3,
            "a second TABLE binding is accepted, exactly like the original"
        );
    }

    #[tokio::test]
    async fn unlinking_reports_a_missing_field_binding() {
        let app = boot().await;
        let subject_id = app.subject("orders").await;
        let term_id = app.term(&subject_id, "gmv").await;
        let table = app.table("orders").await;
        subject::link(&app.db, &subject_id, &table.id, &Audit::new("alice"))
            .await
            .unwrap();
        app.terms
            .link(
                &app.caller,
                &term_id,
                &link(&table.id, TermRelationType::Field, Some("amount")),
            )
            .await
            .unwrap();

        let err = app
            .terms
            .unlink(&app.caller, &term_id, &table.id, Some("missing"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::NotFound);
        assert_eq!(err.status(), 404);
        assert_eq!(err.message(), "Resource not found");

        app.terms
            .unlink(&app.caller, &term_id, &table.id, None)
            .await
            .unwrap();
        assert!(
            app.terms
                .relations_of(std::slice::from_ref(&term_id))
                .await
                .unwrap()
                .is_empty()
        );
        app.terms
            .unlink(&app.caller, &term_id, &table.id, None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_page_labels_every_term_with_its_subject() {
        let app = boot().await;
        let subject_id = app.subject("orders").await;
        let first = app.term(&subject_id, "gmv").await;
        let second = app.term(&subject_id, "aov").await;
        let table = app.table("orders").await;
        subject::link(&app.db, &subject_id, &table.id, &Audit::new("alice"))
            .await
            .unwrap();
        app.terms
            .link(
                &app.caller,
                &first,
                &link(&table.id, TermRelationType::Table, None),
            )
            .await
            .unwrap();

        let page = app
            .terms
            .list(
                &app.caller,
                &subject_id,
                None,
                PageReq { page: 1, size: 10 },
            )
            .await
            .unwrap();
        assert_eq!(page.total, 2);
        let by_id: HashMap<String, Vec<usize>> = page
            .data
            .iter()
            .map(|row| (row.id.clone(), row.relations.iter().map(|_| 1).collect()))
            .collect();
        assert_eq!(by_id[&first].len(), 1);
        assert!(by_id[&second].is_empty());

        let briefs = app
            .terms
            .briefs(&[first.clone(), second.clone(), "gone".into()])
            .await
            .unwrap();
        assert_eq!(briefs.len(), 2);
        assert!(briefs.iter().all(|b| b.subject_name == "orders"));
        let scoped = app
            .terms
            .of_subjects(std::slice::from_ref(&subject_id))
            .await
            .unwrap();
        assert_eq!(scoped.len(), 2);

        app.terms.remove(&app.caller, &first).await.unwrap();
        assert!(app.terms.load(&first).await.is_err());
        assert!(
            term::list_relations(&app.db, &[first])
                .await
                .unwrap()
                .is_empty(),
            "the bindings went with the term"
        );
    }

    #[tokio::test]
    async fn rights_come_from_the_subject() {
        let app = boot().await;
        let subject_id = app.subject("orders").await;
        let term_id = app.term(&subject_id, "gmv").await;
        let stranger = person(&app.db, "carol").await;

        let err = app.terms.get(&stranger, &term_id).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::PermissionDenied);
        let err = app
            .terms
            .update(
                &stranger,
                &term_id,
                &UpdateTermRequest {
                    name: Some("hijacked".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::PermissionDenied);

        app.terms
            .update(
                &app.caller,
                &term_id,
                &UpdateTermRequest {
                    name: Some("gross-merchandise-value".into()),
                    aliases: Some(Vec::new()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let updated = app.terms.load(&term_id).await.unwrap();
        assert_eq!(updated.name.as_deref(), Some("gross-merchandise-value"));
        assert!(updated.aliases.is_empty());
        assert_eq!(updated.description.as_deref(), Some("成交总额"));
    }
}
