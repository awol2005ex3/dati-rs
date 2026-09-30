//! The semantic layer: subjects (business domains), terms (glossary entries) and their bindings.

use serde::{Deserialize, Serialize};

use crate::error::{DatiError, Result};

use super::base::Audit;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TermRelationType {
    #[serde(rename = "TABLE")]
    Table,
    #[serde(rename = "FIELD")]
    Field,
}

impl TermRelationType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Table => "TABLE",
            Self::Field => "FIELD",
        }
    }

    pub fn from_stored(value: &str) -> Option<Self> {
        match value {
            "TABLE" => Some(Self::Table),
            "FIELD" => Some(Self::Field),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Subject {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub deleted: bool,
    pub datasource_id: String,
    pub aliases: Vec<String>,
    pub audit: Audit,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Term {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub deleted: bool,
    pub subject_id: String,
    pub aliases: Vec<String>,
    pub audit: Audit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermRelation {
    pub id: String,
    pub term_id: String,
    pub entity_type: TermRelationType,
    pub table_id: String,
    pub field_name: Option<String>,
    /// Resolved from `table_info` when the bindings are read; a dangling table id leaves both null,
    /// which is what the Java batch lookup produced for a table that no longer exists.
    pub table_name: Option<String>,
    pub schema: Option<String>,
}

/// A search hit grouped for the `search_tables_and_terms` tool response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermBrief {
    pub name: String,
    pub description: String,
    pub subject_name: String,
}

impl TermBrief {
    pub fn new(name: Option<&str>, description: Option<&str>, subject_name: Option<&str>) -> Self {
        Self {
            name: name.unwrap_or_default().to_string(),
            description: description.unwrap_or_default().to_string(),
            subject_name: subject_name.unwrap_or_default().to_string(),
        }
    }
}

/// The Java `SubjectVO`: a subject labelled with its data source and author names.
#[derive(Debug, Clone, Serialize)]
pub struct SubjectView {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub created_by: Option<String>,
    pub created_at: Option<String>,
    pub updated_by: Option<String>,
    pub updated_at: Option<String>,
    pub created_user_name: Option<String>,
    pub updated_user_name: Option<String>,
    pub datasource_id: String,
    pub datasource_name: Option<String>,
    pub aliases: Vec<String>,
}

impl SubjectView {
    pub fn new(subject: &Subject, names: &std::collections::HashMap<String, String>) -> Self {
        let (created_user_name, updated_user_name) =
            super::base::author_names(&subject.audit, names);
        Self {
            id: subject.id.clone(),
            name: subject.name.clone(),
            description: subject.description.clone(),
            created_by: subject.audit.created_by.clone(),
            created_at: subject.audit.created_at.clone(),
            updated_by: subject.audit.updated_by.clone(),
            updated_at: subject.audit.updated_at.clone(),
            created_user_name,
            updated_user_name,
            datasource_id: subject.datasource_id.clone(),
            datasource_name: None,
            aliases: subject.aliases.clone(),
        }
    }

    /// `SubjectAssembler.fillDatasourceInfo`: the name is looked up in bulk, so a deleted data
    /// source leaves it null rather than failing the page.
    pub fn with_datasource_name(mut self, name: Option<String>) -> Self {
        self.datasource_name = name;
        self
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TermRelationView {
    pub id: String,
    pub term_id: String,
    pub entity_type: TermRelationType,
    pub table_id: String,
    pub table_name: Option<String>,
    pub schema: Option<String>,
    pub field_name: Option<String>,
}

impl From<&TermRelation> for TermRelationView {
    fn from(relation: &TermRelation) -> Self {
        Self {
            id: relation.id.clone(),
            term_id: relation.term_id.clone(),
            entity_type: relation.entity_type,
            table_id: relation.table_id.clone(),
            table_name: relation.table_name.clone(),
            schema: relation.schema.clone(),
            field_name: relation.field_name.clone(),
        }
    }
}

/// The Java `TermVO`. A page of terms carries its bindings, which the assembler batch-filled.
#[derive(Debug, Clone, Serialize)]
pub struct TermView {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub created_by: Option<String>,
    pub created_at: Option<String>,
    pub updated_by: Option<String>,
    pub updated_at: Option<String>,
    pub created_user_name: Option<String>,
    pub updated_user_name: Option<String>,
    pub subject_id: String,
    pub aliases: Vec<String>,
    pub relations: Vec<TermRelationView>,
}

impl TermView {
    pub fn new(term: &Term, names: &std::collections::HashMap<String, String>) -> Self {
        let (created_user_name, updated_user_name) = super::base::author_names(&term.audit, names);
        Self {
            id: term.id.clone(),
            name: term.name.clone(),
            description: term.description.clone(),
            created_by: term.audit.created_by.clone(),
            created_at: term.audit.created_at.clone(),
            updated_by: term.audit.updated_by.clone(),
            updated_at: term.audit.updated_at.clone(),
            created_user_name,
            updated_user_name,
            subject_id: term.subject_id.clone(),
            aliases: term.aliases.clone(),
            relations: Vec::new(),
        }
    }

    pub fn with_relations(mut self, relations: &[TermRelation]) -> Self {
        self.relations = relations.iter().map(TermRelationView::from).collect();
        self
    }
}

/// One row of `GET /v1/subjects/{id}/available-tables`: a live table not yet bound to the subject.
#[derive(Debug, Clone, Serialize)]
pub struct AvailableTable {
    pub table_id: String,
    pub table_name: Option<String>,
    pub schema: Option<String>,
    pub description: Option<String>,
}

/// A column as the metadata search reports it. The Java `ColumnDef`.
#[derive(Debug, Clone, Serialize)]
pub struct SearchColumn {
    pub name: Option<String>,
    pub column_type: Option<String>,
    pub comment: Option<String>,
    pub aliases: Vec<String>,
    pub sample_values: Vec<String>,
}

/// A table as the metadata search reports it. The Java `TableMeta`.
#[derive(Debug, Clone, Serialize)]
pub struct SearchTable {
    pub table_id: String,
    pub schema: Option<String>,
    pub table_name: Option<String>,
    pub description: Option<String>,
    pub aliases: Vec<String>,
    pub data_source_id: String,
    pub columns: Vec<SearchColumn>,
}

/// Tables the search matched, grouped under the source they live in.
#[derive(Debug, Clone, Serialize)]
pub struct SearchDataSource {
    pub data_source_id: String,
    pub data_source_name: String,
    #[serde(rename = "type")]
    pub db_type: Option<String>,
    pub default_schema: Option<String>,
    pub description: Option<String>,
    pub tables: Vec<SearchTable>,
}

/// What a metadata search returns: the matched tables by source, plus the glossary hits.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SemanticSearchResult {
    pub data_sources: Vec<SearchDataSource>,
    pub terms: Vec<TermBrief>,
}

/// `POST /v1/subjects` body.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CreateSubjectRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub datasource_id: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// `PUT /v1/subjects/{id}` body: nothing is validated, and a field that is absent keeps its value.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateSubjectRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub aliases: Option<Vec<String>>,
}

/// `POST /v1/subjects/{id}/terms` body.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CreateTermRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// `PUT /v1/terms/{id}` body: the name is required, the rest keeps its value when absent.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateTermRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub aliases: Option<Vec<String>>,
}

/// `POST /v1/subjects/{id}/tables` body.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AddTableToSubjectRequest {
    #[serde(default)]
    pub table_id: Option<String>,
}

/// `POST /v1/terms/{id}/relations` body. `_` in `field_name` means "the whole table".
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LinkTermRelationRequest {
    #[serde(default)]
    pub entity_type: Option<TermRelationType>,
    #[serde(default)]
    pub table_id: Option<String>,
    #[serde(default)]
    pub field_name: Option<String>,
}

fn blank(value: &Option<String>) -> bool {
    value
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
        .is_empty()
}

impl CreateSubjectRequest {
    pub fn validate(&self) -> Result<()> {
        let mut fields = Vec::new();
        if blank(&self.name) {
            fields.push(("name", "must not be blank"));
        }
        if blank(&self.datasource_id) {
            fields.push(("datasourceId", "must not be blank"));
        }
        if fields.is_empty() {
            Ok(())
        } else {
            Err(DatiError::validation(&fields))
        }
    }
}

impl CreateTermRequest {
    pub fn validate(&self) -> Result<()> {
        let mut fields = Vec::new();
        if blank(&self.name) {
            fields.push(("name", "must not be blank"));
        }
        if fields.is_empty() {
            Ok(())
        } else {
            Err(DatiError::validation(&fields))
        }
    }
}

impl UpdateTermRequest {
    pub fn validate(&self) -> Result<()> {
        let mut fields = Vec::new();
        if blank(&self.name) {
            fields.push(("name", "must not be blank"));
        }
        if fields.is_empty() {
            Ok(())
        } else {
            Err(DatiError::validation(&fields))
        }
    }
}

impl AddTableToSubjectRequest {
    pub fn validate(&self) -> Result<()> {
        let mut fields = Vec::new();
        if blank(&self.table_id) {
            fields.push(("tableId", "must not be blank"));
        }
        if fields.is_empty() {
            Ok(())
        } else {
            Err(DatiError::validation(&fields))
        }
    }
}

impl LinkTermRelationRequest {
    pub fn validate(&self) -> Result<()> {
        let mut fields = Vec::new();
        if self.entity_type.is_none() {
            fields.push(("entityType", "must not be null"));
        }
        if blank(&self.table_id) {
            fields.push(("tableId", "must not be blank"));
        }
        if fields.is_empty() {
            Ok(())
        } else {
            Err(DatiError::validation(&fields))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relation_types_store_java_constant_names() {
        assert_eq!(TermRelationType::Table.as_str(), "TABLE");
        assert_eq!(
            TermRelationType::from_stored("FIELD"),
            Some(TermRelationType::Field)
        );
        assert_eq!(TermRelationType::from_stored("COLUMN"), None);
        assert_eq!(
            serde_json::to_value(TermRelationType::Field).unwrap(),
            serde_json::json!("FIELD")
        );
    }

    #[test]
    fn subject_and_term_start_unbound() {
        let subject = Subject::default();
        assert!(subject.aliases.is_empty());
        assert!(!subject.deleted);
        assert!(Term::default().subject_id.is_empty());
    }

    #[test]
    fn an_unbound_relation_has_no_table_label() {
        let relation = TermRelation {
            id: "r-1".into(),
            term_id: "t-1".into(),
            entity_type: TermRelationType::Field,
            table_id: "missing".into(),
            field_name: Some("status".into()),
            table_name: None,
            schema: None,
        };
        let view = TermRelationView::from(&relation);
        assert_eq!(view.entity_type, TermRelationType::Field);
        assert_eq!(view.field_name.as_deref(), Some("status"));
        assert!(view.table_name.is_none());
    }

    #[test]
    fn creating_a_subject_needs_a_name_and_a_source() {
        let err = CreateSubjectRequest::default().validate().unwrap_err();
        assert_eq!(
            err.message(),
            "Request parameter error: name: must not be blank; datasourceId: must not be blank"
        );
        CreateSubjectRequest {
            name: Some("订单域".into()),
            datasource_id: Some("ds-1".into()),
            ..Default::default()
        }
        .validate()
        .unwrap();

        let err = AddTableToSubjectRequest::default().validate().unwrap_err();
        assert_eq!(
            err.message(),
            "Request parameter error: tableId: must not be blank"
        );
        let err = LinkTermRelationRequest::default().validate().unwrap_err();
        assert_eq!(
            err.message(),
            "Request parameter error: entityType: must not be null; tableId: must not be blank"
        );
    }

    #[test]
    fn update_bodies_tell_absent_from_empty() {
        let untouched: UpdateSubjectRequest =
            serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(untouched.name.is_none());
        assert!(untouched.aliases.is_none());
        let cleared: UpdateSubjectRequest =
            serde_json::from_value(serde_json::json!({"aliases": []})).unwrap();
        assert_eq!(cleared.aliases, Some(Vec::new()));

        let err = UpdateTermRequest::default().validate().unwrap_err();
        assert_eq!(
            err.message(),
            "Request parameter error: name: must not be blank"
        );
    }

    #[test]
    fn subject_view_labels_its_source_and_authors() {
        let subject = Subject {
            id: "s-1".into(),
            name: Some("orders".into()),
            datasource_id: "ds-1".into(),
            aliases: vec!["trade".into()],
            audit: Audit::new("u-1"),
            ..Default::default()
        };
        let mut names = std::collections::HashMap::new();
        names.insert("u-1".to_string(), "Alice".to_string());
        let json = serde_json::to_value(SubjectView::new(&subject, &names)).unwrap();
        assert_eq!(json["created_user_name"], "Alice");
        assert!(json["datasource_name"].is_null());
        assert_eq!(json["datasource_id"], "ds-1");
        assert_eq!(json["aliases"], serde_json::json!(["trade"]));
    }

    #[test]
    fn search_result_groups_tables_under_their_source() {
        let result = SemanticSearchResult {
            data_sources: vec![SearchDataSource {
                data_source_id: "ds-1".into(),
                data_source_name: "warehouse".into(),
                db_type: Some("SQLITE".into()),
                default_schema: Some("main".into()),
                description: None,
                tables: vec![SearchTable {
                    table_id: "t-1".into(),
                    schema: Some("main".into()),
                    table_name: Some("orders".into()),
                    description: None,
                    aliases: Vec::new(),
                    data_source_id: "ds-1".into(),
                    columns: vec![SearchColumn {
                        name: Some("status".into()),
                        column_type: Some("TEXT".into()),
                        comment: None,
                        aliases: Vec::new(),
                        sample_values: vec!["paid".into()],
                    }],
                }],
            }],
            terms: vec![TermBrief::new(Some("GMV"), None, Some("orders"))],
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["data_sources"][0]["type"], "SQLITE");
        assert_eq!(
            json["data_sources"][0]["tables"][0]["columns"][0]["sample_values"][0],
            "paid"
        );
        assert_eq!(json["terms"][0]["subject_name"], "orders");
    }
}
