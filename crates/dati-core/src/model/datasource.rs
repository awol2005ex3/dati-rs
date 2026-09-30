//! Data sources and the table / column metadata registered from them.

use serde::{Deserialize, Serialize};

use crate::error::{DatiError, Result};

use super::base::Audit;

/// Databases DatI can connect to. Wire values match the Java `DbType` enum names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DbType {
    #[serde(rename = "MYSQL")]
    MySql,
    #[serde(rename = "POSTGRESQL")]
    PostgreSql,
    #[serde(rename = "MARIADB")]
    Mariadb,
    #[serde(rename = "DORIS")]
    Doris,
    #[serde(rename = "SQLITE")]
    Sqlite,
}

impl DbType {
    pub fn from_stored(value: &str) -> Option<Self> {
        match value {
            "MYSQL" => Some(Self::MySql),
            "POSTGRESQL" => Some(Self::PostgreSql),
            "MARIADB" => Some(Self::Mariadb),
            "DORIS" => Some(Self::Doris),
            "SQLITE" => Some(Self::Sqlite),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MySql => "MYSQL",
            Self::PostgreSql => "POSTGRESQL",
            Self::Mariadb => "MARIADB",
            Self::Doris => "DORIS",
            Self::Sqlite => "SQLITE",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::MySql => "MySQL",
            Self::PostgreSql => "PostgreSQL",
            Self::Mariadb => "MariaDB",
            Self::Doris => "Apache Doris",
            Self::Sqlite => "SQLite",
        }
    }

    pub const fn default_port(self) -> u16 {
        match self {
            Self::MySql | Self::Mariadb => 3306,
            Self::PostgreSql => 5432,
            Self::Doris => 9030,
            Self::Sqlite => 0,
        }
    }

    /// UI placeholder only: the backend stores whatever URL the client supplied. These are the
    /// driver URLs `sqlx` connects with, not JDBC ones — the server never loads a JDBC driver.
    pub const fn connection_url_template(self) -> &'static str {
        match self {
            Self::MySql => "mysql://localhost:3306/database_name",
            Self::PostgreSql => "postgresql://localhost:5432/database_name",
            Self::Mariadb => "mariadb://localhost:3306/database_name",
            Self::Doris => "mysql://localhost:9030/database_name",
            Self::Sqlite => "sqlite:/path/to/database.db",
        }
    }

    /// Doris and MariaDB speak the MySQL wire protocol, so they share its connector.
    pub const fn is_mysql_protocol(self) -> bool {
        matches!(self, Self::MySql | Self::Mariadb | Self::Doris)
    }

    /// For MySQL-family databases the user-facing "schema" is the catalog (the database itself).
    pub const fn schema_is_catalog(self) -> bool {
        self.is_mysql_protocol()
    }

    pub fn all() -> Vec<Self> {
        [
            Self::MySql,
            Self::PostgreSql,
            Self::Mariadb,
            Self::Doris,
            Self::Sqlite,
        ]
        .to_vec()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataSource {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub deleted: bool,
    pub connection_url: Option<String>,
    pub db_type: DbType,
    pub user_name: Option<String>,
    /// Stored as the driver consumes it; see `credential` for the at-rest encoding.
    pub password: Option<String>,
    /// Detected by connecting, never taken from the client.
    pub default_schema: Option<String>,
    pub audit: Audit,
}

impl DataSource {
    pub fn owner(&self) -> Option<&str> {
        self.audit.created_by.as_deref()
    }
}

/// The connection parameters a connector needs, parsed from the stored URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionSpec {
    pub db_type: DbType,
    pub url: String,
    pub user: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableInfo {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub deleted: bool,
    pub data_source_id: String,
    pub schema: Option<String>,
    pub aliases: Vec<String>,
    pub audit: Audit,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ColumnInfo {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub deleted: bool,
    pub table_id: String,
    pub column_type: Option<String>,
    pub aliases: Vec<String>,
    pub extract_value_enabled: bool,
    pub audit: Audit,
}

/// A sampled/enumerated value of a column, with its business synonyms.
/// Replaces the Elasticsearch `FIELD_VALUE` documents of the Java version; `sort_no` keeps the
/// dictionary in the order it was built, which a second-precision timestamp cannot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ColumnValue {
    pub id: String,
    pub table_id: String,
    pub column_name: String,
    pub value: String,
    pub synonyms: Vec<String>,
    pub sort_no: i64,
    pub audit: Audit,
}

/// A table as reported by the live database, before it is registered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveTable {
    pub name: String,
    pub comment: Option<String>,
}

/// A column as reported by the live database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveColumn {
    pub name: String,
    pub type_name: String,
    pub comment: Option<String>,
}

/// `POST /v1/data-sources` and `POST /v1/data-sources/test-connection` bodies: the Java
/// `DataSource` entity was posted straight back, so the client sends the connection details
/// and gets the stored id in return.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DataSourceInput {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "type")]
    pub db_type: Option<DbType>,
    #[serde(default)]
    pub connection_url: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    /// Ignored on create: the stored schema is always what the connector reported.
    #[serde(default)]
    pub default_schema: Option<String>,
}

impl DataSourceInput {
    /// Bean Validation on the Java entity: `@NotNull type`, `@NotBlank` on the connection URL and on
    /// the username. The test-connection endpoint posted the same shape without validation.
    pub fn validate(&self) -> Result<()> {
        let mut fields = Vec::new();
        if self.db_type.is_none() {
            fields.push(("type", "must not be null"));
        }
        if self
            .connection_url
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
        {
            fields.push(("connectionUrl", "must not be blank"));
        }
        if self
            .username
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
        {
            fields.push(("username", "must not be blank"));
        }
        if !fields.is_empty() {
            return Err(DatiError::validation(&fields));
        }
        Ok(())
    }

    pub fn spec(&self) -> Option<ConnectionSpec> {
        let db_type = self.db_type?;
        Some(ConnectionSpec {
            db_type,
            url: self.connection_url.clone().unwrap_or_default(),
            user: self.username.clone(),
            password: self.password.clone(),
        })
    }
}

/// `PUT /v1/data-sources/{id}` body; every field is optional and a null keeps the stored value.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateDataSourceInput {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "type")]
    pub db_type: Option<DbType>,
    #[serde(default)]
    pub connection_url: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub default_schema: Option<String>,
}

/// What the REST layer sends for one registered data source. The password never appears: the
/// Java `DatasourceVO` had no such field.
#[derive(Debug, Clone, Serialize)]
pub struct DataSourceView {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub created_by: Option<String>,
    pub created_at: Option<String>,
    pub updated_by: Option<String>,
    pub updated_at: Option<String>,
    pub created_user_name: Option<String>,
    pub updated_user_name: Option<String>,
    pub connection_url: Option<String>,
    pub username: Option<String>,
    #[serde(rename = "type")]
    pub db_type: DbType,
    pub default_schema: Option<String>,
}

impl DataSourceView {
    pub fn new(source: &DataSource, names: &std::collections::HashMap<String, String>) -> Self {
        let (created_user_name, updated_user_name) =
            super::base::author_names(&source.audit, names);
        Self {
            id: source.id.clone(),
            name: source.name.clone(),
            description: source.description.clone(),
            created_by: source.audit.created_by.clone(),
            created_at: source.audit.created_at.clone(),
            updated_by: source.audit.updated_by.clone(),
            updated_at: source.audit.updated_at.clone(),
            created_user_name,
            updated_user_name,
            connection_url: source.connection_url.clone(),
            username: source.user_name.clone(),
            db_type: source.db_type,
            default_schema: source.default_schema.clone(),
        }
    }
}

/// `POST /v1/data-sources/{id}/tables/batch` item: which live table to register.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AddTableRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub schema: Option<String>,
}

impl AddTableRequest {
    /// Both fields carried `@NotBlank` on the Java side. Reported as pairs so a batch request can
    /// name the offending element: `tables[3].name`.
    pub fn violations(&self) -> Vec<(&'static str, &'static str)> {
        let mut fields = Vec::new();
        if self
            .name
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
        {
            fields.push(("name", "must not be blank"));
        }
        if self
            .schema
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
        {
            fields.push(("schema", "must not be blank"));
        }
        fields
    }

    pub fn validate(&self) -> Result<()> {
        let violations = self.violations();
        if violations.is_empty() {
            Ok(())
        } else {
            Err(DatiError::validation(&violations))
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TableInfoView {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub created_by: Option<String>,
    pub created_at: Option<String>,
    pub updated_by: Option<String>,
    pub updated_at: Option<String>,
    pub created_user_name: Option<String>,
    pub updated_user_name: Option<String>,
    pub schema: Option<String>,
    pub datasource_id: String,
    pub aliases: Vec<String>,
}

impl TableInfoView {
    pub fn new(table: &TableInfo, names: &std::collections::HashMap<String, String>) -> Self {
        let (created_user_name, updated_user_name) = super::base::author_names(&table.audit, names);
        Self {
            id: table.id.clone(),
            name: table.name.clone(),
            description: table.description.clone(),
            created_by: table.audit.created_by.clone(),
            created_at: table.audit.created_at.clone(),
            updated_by: table.audit.updated_by.clone(),
            updated_at: table.audit.updated_at.clone(),
            created_user_name,
            updated_user_name,
            schema: table.schema.clone(),
            datasource_id: table.data_source_id.clone(),
            aliases: table.aliases.clone(),
        }
    }
}

/// `PUT /v1/data-sources/{id}/tables/{tableId}` body: the editable half of a `TableInfoVO`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TableUpdateRequest {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub aliases: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ColumnInfoView {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub created_by: Option<String>,
    pub created_at: Option<String>,
    pub updated_by: Option<String>,
    pub updated_at: Option<String>,
    pub created_user_name: Option<String>,
    pub updated_user_name: Option<String>,
    pub table_id: String,
    pub column_type: Option<String>,
    pub aliases: Vec<String>,
    pub extract_value_enabled: bool,
}

impl ColumnInfoView {
    pub fn new(column: &ColumnInfo, names: &std::collections::HashMap<String, String>) -> Self {
        let (created_user_name, updated_user_name) =
            super::base::author_names(&column.audit, names);
        Self {
            id: column.id.clone(),
            name: column.name.clone(),
            description: column.description.clone(),
            created_by: column.audit.created_by.clone(),
            created_at: column.audit.created_at.clone(),
            updated_by: column.audit.updated_by.clone(),
            updated_at: column.audit.updated_at.clone(),
            created_user_name,
            updated_user_name,
            table_id: column.table_id.clone(),
            column_type: column.column_type.clone(),
            aliases: column.aliases.clone(),
            extract_value_enabled: column.extract_value_enabled,
        }
    }
}

/// `PUT .../columns/{id}` body; a null field keeps the stored value.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ColumnUpdateRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub column_type: Option<String>,
    #[serde(default)]
    pub aliases: Option<Vec<String>>,
    #[serde(default)]
    pub extract_value_enabled: Option<bool>,
}

/// One row of the column value dictionary: the value plus its business synonyms.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ColumnValueItem {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub synonyms: Vec<String>,
}

/// `PUT .../columns/{columnId}/values` body: rows to upsert and rows to drop.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ColumnValueListRequest {
    #[serde(default)]
    pub values: Vec<ColumnValueItem>,
    #[serde(default)]
    pub deleted_ids: Vec<String>,
}

/// `GET .../columns/{columnId}/values` row.
#[derive(Debug, Clone, Serialize)]
pub struct ColumnValueView {
    pub id: String,
    pub value: String,
    pub synonyms: Vec<String>,
}

impl From<&ColumnValue> for ColumnValueView {
    fn from(value: &ColumnValue) -> Self {
        Self {
            id: value.id.clone(),
            value: value.value.clone(),
            synonyms: value.synonyms.clone(),
        }
    }
}

/// One entry of `GET /v1/system/config`: a database the console may offer in the data source
/// picker. The Java `DatabaseTypeVO`, whose `type` was the enum name.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct DatabaseTypeVo {
    #[serde(rename = "type")]
    pub db_type: &'static str,
    pub label: &'static str,
    pub default_port: u16,
    pub connection_url_template: &'static str,
}

impl From<DbType> for DatabaseTypeVo {
    fn from(db_type: DbType) -> Self {
        Self {
            db_type: db_type.as_str(),
            label: db_type.label(),
            default_port: db_type.default_port(),
            connection_url_template: db_type.connection_url_template(),
        }
    }
}

/// `GET /v1/system/config`: the sampling limits the value dictionaries obey, plus the databases
/// the server can connect to. The Java `SystemConfigResponse`.
#[derive(Debug, Clone, Serialize)]
pub struct SystemConfigResponse {
    pub column_value_sample_limit: i64,
    pub column_value_length_limit: i64,
    pub supported_database_types: Vec<DatabaseTypeVo>,
}

impl SystemConfigResponse {
    pub fn new(column_value_sample_limit: i64, column_value_length_limit: i64) -> Self {
        Self {
            column_value_sample_limit,
            column_value_length_limit,
            supported_database_types: DbType::all()
                .into_iter()
                .map(DatabaseTypeVo::from)
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;

    #[test]
    fn wire_names_match_the_java_enum() {
        assert_eq!(
            serde_json::to_value(DbType::all()).unwrap(),
            serde_json::json!(["MYSQL", "POSTGRESQL", "MARIADB", "DORIS", "SQLITE"])
        );
        assert_eq!(
            serde_json::from_str::<DbType>("\"POSTGRESQL\"").unwrap(),
            DbType::PostgreSql
        );
        assert_eq!(DbType::from_stored("DORIS"), Some(DbType::Doris));
        assert_eq!(DbType::from_stored("CLICKHOUSE"), None);
        assert_eq!(DbType::from_stored("UNKNOWN"), None);
    }

    #[test]
    fn mysql_family_types_share_the_connector() {
        for db_type in [DbType::MySql, DbType::Mariadb, DbType::Doris] {
            assert!(db_type.is_mysql_protocol(), "{db_type:?}");
            assert!(db_type.schema_is_catalog(), "{db_type:?}");
        }
        assert!(!DbType::PostgreSql.schema_is_catalog());
        assert!(!DbType::Sqlite.is_mysql_protocol());
        assert_eq!(DbType::Doris.default_port(), 9030);
        assert_eq!(DbType::Doris.label(), "Apache Doris");
    }

    #[test]
    fn templates_are_the_urls_the_connectors_accept() {
        assert_eq!(
            DbType::MySql.connection_url_template(),
            "mysql://localhost:3306/database_name"
        );
        assert_eq!(
            DbType::PostgreSql.connection_url_template(),
            "postgresql://localhost:5432/database_name"
        );
        assert!(
            DbType::Doris
                .connection_url_template()
                .starts_with("mysql://localhost:9030/")
        );
        assert!(
            DbType::Sqlite
                .connection_url_template()
                .starts_with("sqlite:")
        );
    }

    #[test]
    fn the_picker_offers_the_databases_the_port_connects_to() {
        assert_eq!(
            serde_json::to_value(SystemConfigResponse::new(1000, 256)).unwrap(),
            serde_json::json!({
                "column_value_sample_limit": 1000,
                "column_value_length_limit": 256,
                "supported_database_types": [
                    {
                        "type": "MYSQL",
                        "label": "MySQL",
                        "default_port": 3306,
                        "connection_url_template": "mysql://localhost:3306/database_name"
                    },
                    {
                        "type": "POSTGRESQL",
                        "label": "PostgreSQL",
                        "default_port": 5432,
                        "connection_url_template": "postgresql://localhost:5432/database_name"
                    },
                    {
                        "type": "MARIADB",
                        "label": "MariaDB",
                        "default_port": 3306,
                        "connection_url_template": "mariadb://localhost:3306/database_name"
                    },
                    {
                        "type": "DORIS",
                        "label": "Apache Doris",
                        "default_port": 9030,
                        "connection_url_template": "mysql://localhost:9030/database_name"
                    },
                    {
                        "type": "SQLITE",
                        "label": "SQLite",
                        "default_port": 0,
                        "connection_url_template": "sqlite:/path/to/database.db"
                    }
                ]
            })
        );
    }

    fn blank_data_source() -> DataSource {
        DataSource {
            id: "ds-1".into(),
            name: None,
            description: None,
            deleted: false,
            connection_url: None,
            db_type: DbType::Sqlite,
            user_name: None,
            password: None,
            default_schema: None,
            audit: Audit::default(),
        }
    }

    #[test]
    fn data_source_keeps_soft_delete_and_detected_schema_separate() {
        let ds = blank_data_source();
        assert!(!ds.deleted);
        assert!(ds.default_schema.is_none());
        assert!(ds.owner().is_none());
        assert_eq!(ds.db_type, DbType::Sqlite);
    }

    fn input(fields: &[(&str, serde_json::Value)]) -> DataSourceInput {
        let mut object = serde_json::Map::new();
        for (key, value) in fields {
            object.insert((*key).to_string(), value.clone());
        }
        serde_json::from_value(serde_json::Value::Object(object)).unwrap()
    }

    #[test]
    fn creating_a_data_source_needs_type_url_and_user() {
        let missing = input(&[]).validate().unwrap_err();
        assert_eq!(missing.code(), ErrorCode::InvalidParameter);
        assert_eq!(
            missing.message(),
            "Request parameter error: type: must not be null; connectionUrl: must not be blank; username: must not be blank"
        );
        let blank = input(&[
            ("type", serde_json::json!("SQLITE")),
            ("connection_url", serde_json::json!("   ")),
            ("username", serde_json::json!("root")),
        ])
        .validate()
        .unwrap_err();
        assert!(
            blank
                .message()
                .ends_with("connectionUrl: must not be blank")
        );
        input(&[
            ("type", serde_json::json!("SQLITE")),
            ("connection_url", serde_json::json!("sqlite:a.db")),
            ("username", serde_json::json!("none")),
        ])
        .validate()
        .unwrap();
    }

    #[test]
    fn an_input_becomes_the_spec_the_connector_dials() {
        let parsed = input(&[
            ("type", serde_json::json!("MYSQL")),
            ("connection_url", serde_json::json!("mysql://h:3306/dw")),
            ("username", serde_json::json!("root")),
            ("password", serde_json::json!("secret")),
        ]);
        assert_eq!(
            parsed.spec().unwrap(),
            ConnectionSpec {
                db_type: DbType::MySql,
                url: "mysql://h:3306/dw".into(),
                user: Some("root".into()),
                password: Some("secret".into()),
            }
        );
        assert!(DataSourceInput::default().spec().is_none());
    }

    #[test]
    fn registering_a_table_needs_both_name_and_schema() {
        let request = AddTableRequest {
            name: Some("orders".into()),
            schema: None,
        };
        let err = request.validate().unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidParameter);
        assert!(err.message().ends_with("schema: must not be blank"));
        assert!(
            AddTableRequest {
                name: Some("orders".into()),
                schema: Some("dati".into()),
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn data_source_view_never_leaks_the_password() {
        let mut source = blank_data_source();
        source.password = Some("hunter2".into());
        source.connection_url = Some("sqlite:a.db".into());
        source.user_name = Some("nobody".into());
        source.db_type = DbType::MySql;
        let mut names = std::collections::HashMap::new();
        names.insert("u-1".to_string(), "Alice".to_string());
        source.audit.created_by = Some("u-1".into());
        let json = serde_json::to_value(DataSourceView::new(&source, &names)).unwrap();
        assert!(json.get("password").is_none(), "{json}");
        assert_eq!(json["type"], "MYSQL");
        assert_eq!(json["username"], "nobody");
        assert_eq!(json["created_user_name"], "Alice");
        assert!(json["updated_user_name"].is_null());
        assert_eq!(json["connection_url"], "sqlite:a.db");
    }

    #[test]
    fn value_requests_default_to_empty_collections() {
        let request: ColumnValueListRequest =
            serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(request.values.is_empty());
        assert!(request.deleted_ids.is_empty());
        let request: ColumnValueListRequest = serde_json::from_value(serde_json::json!({
            "values": [{"value": "paid", "synonyms": ["已支付"]}],
            "deleted_ids": ["v-1"],
        }))
        .unwrap();
        assert_eq!(request.values[0].value, "paid");
        assert_eq!(request.values[0].synonyms, vec!["已支付".to_string()]);
        assert_eq!(request.deleted_ids, vec!["v-1".to_string()]);
    }
}
