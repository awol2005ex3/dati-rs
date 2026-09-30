//! The argument records of the seven prebuilt tools, and the JSON Schema each one is published
//! under.
//!
//! In Java a single record carried both contracts: the schema was generated from its annotations by
//! victools, and the same annotations were what Hibernate checked. Here the two halves sit next to
//! each other for the same reason — [`input_schema`] is the shape the checker below enforces.
//!
//! Two details of the Java binder are reproduced deliberately:
//! - Every field deserializes to an `Option` or an empty collection. A missing key is *not* a parse
//!   failure; it is a `must not be null` violation, because Jackson leaves the record component null
//!   and lets the validator speak.
//! - An unknown key *is* a parse failure, since Jackson's `FAIL_ON_UNKNOWN_PROPERTIES` was left at
//!   its default. Both cases end up as `参数不合法：<detail>`, the detail being the literal
//!   `Parse JSON failed` for the first.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::model::mcp::McpToolType;

/// A blank check on a text field: `null` is `must not be null`, empty or whitespace is
/// `must not be blank`, and both fire together when the key was absent, as `@NotNull @NotBlank`
/// did in the Java record.
fn required_text(value: &Option<String>, out: &mut Vec<String>) {
    match value.as_deref() {
        None => {
            out.push("must not be null".to_string());
            out.push("must not be blank".to_string());
        }
        Some(text) if text.trim().is_empty() => out.push("must not be blank".to_string()),
        Some(_) => {}
    }
}

/// `@Size(max = n)` on a string: the message keeps the zero lower bound the annotation implied.
fn max_len(value: &Option<String>, max: usize, out: &mut Vec<String>) {
    if let Some(text) = value
        && text.chars().count() > max
    {
        out.push(format!("size must be between 0 and {max}"));
    }
}

/// `@Size(max = n)` on a list, including the element check `List<@Size(max = n) String>`.
fn max_items(values: &Option<Vec<Option<String>>>, max: usize, out: &mut Vec<String>) {
    let Some(values) = values else {
        return;
    };
    if values.len() > max {
        out.push(format!("size must be between 0 and {max}"));
    }
}

fn max_element_len(values: &Option<Vec<Option<String>>>, max: usize, out: &mut Vec<String>) {
    for value in values.as_deref().unwrap_or_default() {
        if let Some(text) = value
            && text.chars().count() > max
        {
            out.push(format!("size must be between 0 and {max}"));
        }
    }
}

/// `SEARCH_METADATA`: `@NotNull @Size(min = 1)` with `@NotBlank` on every element.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchArgs {
    #[serde(default)]
    pub keywords: Option<Vec<Option<String>>>,
}

impl SearchArgs {
    pub fn violations(&self) -> Vec<String> {
        let mut out = Vec::new();
        match &self.keywords {
            None => out.push("must not be null".to_string()),
            Some(keywords) if keywords.is_empty() => {
                // `@Size(min = 1)` leaves the upper bound at `Integer.MAX_VALUE`.
                out.push("size must be between 1 and 2147483647".to_string());
            }
            Some(keywords) => {
                for keyword in keywords.iter() {
                    required_text(keyword, &mut out);
                }
            }
        }
        out
    }
}

/// One `tables[]` entry. Shared by the schema reader and the two write tools.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableRefArg {
    #[serde(default)]
    pub data_source_id: Option<String>,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub table: Option<String>,
    /// Declared and validated by the schema, then never applied — `GET_TABLE_INFO` ignores it.
    #[serde(default)]
    pub fields: Option<Vec<Option<String>>>,
}

/// `GET_TABLE_INFO`: the list itself is checked, its items are not — the Java record has no `@Valid`
/// on `tables`, so a missing `data_source_id` reaches the executor and fails as a scope error.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetTableInfoArgs {
    #[serde(default)]
    pub tables: Option<Vec<TableRefArg>>,
}

impl GetTableInfoArgs {
    pub fn violations(&self) -> Vec<String> {
        let mut out = Vec::new();
        list_size(&self.tables, 1, 20, &mut out);
        out
    }
}

/// `LIST_TABLES` takes nothing at all; an argument object with any key is a parse failure.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListTablesArgs {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecuteSqlArgs {
    #[serde(default)]
    pub data_source_id: Option<String>,
    #[serde(default)]
    pub sql: Option<String>,
}

impl ExecuteSqlArgs {
    pub fn violations(&self) -> Vec<String> {
        let mut out = Vec::new();
        required_text(&self.data_source_id, &mut out);
        required_text(&self.sql, &mut out);
        out
    }
}

/// A `tables[]` entry of `UPDATE_TABLE_INFO`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateTableArg {
    #[serde(default)]
    pub data_source_id: Option<String>,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub table: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub aliases: Option<Vec<Option<String>>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateTableInfoArgs {
    #[serde(default)]
    pub tables: Option<Vec<UpdateTableArg>>,
}

impl UpdateTableInfoArgs {
    pub fn violations(&self) -> Vec<String> {
        let mut out = Vec::new();
        list_size(&self.tables, 1, 20, &mut out);
        // `@Valid` on the list: every item is checked, and an omitted write field keeps the current.
        for item in self.tables.as_deref().unwrap_or_default() {
            required_text(&item.data_source_id, &mut out);
            required_text(&item.table, &mut out);
            max_len(&item.description, 500, &mut out);
            max_items(&item.aliases, 20, &mut out);
            max_element_len(&item.aliases, 100, &mut out);
        }
        out
    }
}

/// A `columns[]` entry of `UPDATE_COLUMN_INFO`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateColumnArg {
    #[serde(default)]
    pub data_source_id: Option<String>,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub table: Option<String>,
    #[serde(default)]
    pub column: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub aliases: Option<Vec<Option<String>>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateColumnInfoArgs {
    #[serde(default)]
    pub columns: Option<Vec<UpdateColumnArg>>,
}

impl UpdateColumnInfoArgs {
    pub fn violations(&self) -> Vec<String> {
        let mut out = Vec::new();
        list_size(&self.columns, 1, 20, &mut out);
        for item in self.columns.as_deref().unwrap_or_default() {
            required_text(&item.data_source_id, &mut out);
            required_text(&item.table, &mut out);
            required_text(&item.column, &mut out);
            max_len(&item.description, 500, &mut out);
            max_items(&item.aliases, 20, &mut out);
            max_element_len(&item.aliases, 100, &mut out);
        }
        out
    }
}

/// A `terms[]` entry of `UPSERT_TERM`; the subject is found by name inside the service scope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpsertTermArg {
    #[serde(default)]
    pub subject_name: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub aliases: Option<Vec<Option<String>>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpsertTermArgs {
    #[serde(default)]
    pub terms: Option<Vec<UpsertTermArg>>,
}

impl UpsertTermArgs {
    pub fn violations(&self) -> Vec<String> {
        let mut out = Vec::new();
        list_size(&self.terms, 1, 20, &mut out);
        for item in self.terms.as_deref().unwrap_or_default() {
            required_text(&item.subject_name, &mut out);
            max_len(&item.subject_name, 200, &mut out);
            required_text(&item.name, &mut out);
            max_len(&item.name, 200, &mut out);
            max_len(&item.description, 500, &mut out);
            max_items(&item.aliases, 20, &mut out);
            max_element_len(&item.aliases, 100, &mut out);
        }
        out
    }
}

/// `@NotNull @Size(min = a, max = b)` on a collection field.
fn list_size<T>(values: &Option<Vec<T>>, min: usize, max: usize, out: &mut Vec<String>) {
    match values {
        None => out.push("must not be null".to_string()),
        Some(rows) if rows.len() < min || rows.len() > max => {
            out.push(format!("size must be between {min} and {max}"));
        }
        Some(_) => {}
    }
}

/// The `inputSchema` of one prebuilt tool, in the shape the Java generator emitted from the
/// records above: `additionalProperties: false` at every object level, the array bounds from the
/// `@Size`s, and the descriptions from the `@JsonPropertyDescription`s.
pub fn input_schema(tool_type: McpToolType) -> Value {
    match tool_type {
        McpToolType::SearchMetadata => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "keywords": {
                    "type": "array",
                    "minItems": 1,
                    "description": "List of search keywords or business terms. Matched using OR logic (feel free to provide multiple relevant terms or synonyms).",
                    "items": {"type": "string", "minLength": 1},
                }
            },
            "required": ["keywords"],
        }),
        McpToolType::GetTableInfo => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "tables": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 20,
                    "description": "Tables to describe (1-20)",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "data_source_id": {"type": "string", "description": "Data source ID"},
                            "schema": {"type": "string", "description": "Table schema (optional)"},
                            "table": {"type": "string", "description": "Table name"},
                            "fields": {
                                "type": "array",
                                "maxItems": 100,
                                "description": "Optional column name filter",
                                "items": {"type": "string", "maxLength": 100},
                            },
                        },
                        "required": ["data_source_id", "table"],
                    },
                }
            },
            "required": ["tables"],
        }),
        // An empty record: no properties, no required list.
        McpToolType::ListTables => json!({"type": "object", "additionalProperties": false}),
        McpToolType::ExecuteSql => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "data_source_id": {"type": "string", "description": "Data source ID"},
                "sql": {"type": "string", "description": "SQL statement to execute"},
            },
            "required": ["data_source_id", "sql"],
        }),
        McpToolType::UpdateTableInfo => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "tables": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 20,
                    "description": "Tables to update (1-20)",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "aliases": {
                                "type": "array",
                                "maxItems": 20,
                                "description": "Complete alias list, replaces existing (omit to keep current)",
                                "items": {"type": "string", "maxLength": 100},
                            },
                            "data_source_id": {"type": "string", "description": "Data source ID"},
                            "description": {
                                "type": "string",
                                "maxLength": 500,
                                "description": "New description (omit to keep current)",
                            },
                            "schema": {"type": "string", "description": "Table schema (optional)"},
                            "table": {"type": "string", "description": "Table name"},
                        },
                        "required": ["data_source_id", "table"],
                    },
                }
            },
            "required": ["tables"],
        }),
        McpToolType::UpdateColumnInfo => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "columns": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 20,
                    "description": "Columns to update (1-20)",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "aliases": {
                                "type": "array",
                                "maxItems": 20,
                                "description": "Complete alias list, replaces existing (omit to keep current)",
                                "items": {"type": "string", "maxLength": 100},
                            },
                            "column": {"type": "string", "description": "Column name"},
                            "data_source_id": {"type": "string", "description": "Data source ID"},
                            "description": {
                                "type": "string",
                                "maxLength": 500,
                                "description": "New description (omit to keep current)",
                            },
                            "schema": {"type": "string", "description": "Table schema (optional)"},
                            "table": {"type": "string", "description": "Table name"},
                        },
                        "required": ["column", "data_source_id", "table"],
                    },
                }
            },
            "required": ["columns"],
        }),
        McpToolType::UpsertTerm => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "terms": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 20,
                    "description": "Terms to upsert (1-20)",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "aliases": {
                                "type": "array",
                                "maxItems": 20,
                                "description": "Complete alias list, replaces existing (omit to keep current)",
                                "items": {"type": "string", "maxLength": 100},
                            },
                            "description": {
                                "type": "string",
                                "maxLength": 500,
                                "description": "Term description (omit to keep current)",
                            },
                            "name": {"type": "string", "maxLength": 200, "description": "Term name"},
                            "subject_name": {
                                "type": "string",
                                "maxLength": 200,
                                "description": "Subject name the term belongs to",
                            },
                        },
                        "required": ["name", "subject_name"],
                    },
                }
            },
            "required": ["terms"],
        }),
        // A custom tool's schema comes from its declared parameters, not from a record.
        McpToolType::ParameterizedSql => json!({"type": "object", "additionalProperties": false}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The binder's exact output: sorted messages, joined as the Java stream joined them.
    fn joined(messages: Vec<String>) -> String {
        let mut messages = messages;
        messages.sort();
        messages.join("; ")
    }

    #[test]
    fn a_missing_keyword_list_is_reported_as_the_two_java_violations() {
        let args: SearchArgs = serde_json::from_str("{}").unwrap();
        assert_eq!(joined(args.violations()), "must not be null");

        let args: SearchArgs = serde_json::from_str(r#"{"keywords":null}"#).unwrap();
        assert_eq!(args.violations(), vec!["must not be null".to_string()]);

        let args: SearchArgs = serde_json::from_str(r#"{"keywords":[]}"#).unwrap();
        assert_eq!(
            joined(args.violations()),
            "size must be between 1 and 2147483647"
        );

        let args: SearchArgs = serde_json::from_str(r#"{"keywords":["ok"," ",""]}"#).unwrap();
        assert_eq!(
            joined(args.violations()),
            "must not be blank; must not be blank"
        );
        assert_eq!(args.keywords.as_deref().unwrap()[1].as_deref(), Some(" "));
    }

    #[test]
    fn an_unknown_or_wrongly_typed_argument_key_is_a_parse_failure() {
        // `FAIL_ON_UNKNOWN_PROPERTIES` was left on, so `list_tables_and_terms` rejects any argument.
        serde_json::from_str::<ListTablesArgs>("{}").unwrap();
        serde_json::from_str::<ListTablesArgs>(r#"{"foo":1}"#).unwrap_err();
        // A keyword list given as a string never reaches the validator.
        serde_json::from_str::<SearchArgs>(r#"{"keywords":"订单"}"#).unwrap_err();
    }

    #[test]
    fn table_refs_are_checked_as_a_list_only() {
        let args: GetTableInfoArgs = serde_json::from_str(r#"{"tables":[]}"#).unwrap();
        assert_eq!(joined(args.violations()), "size must be between 1 and 20");

        let many = vec![TableRefArg::default(); 21];
        let args = GetTableInfoArgs { tables: Some(many) };
        assert_eq!(joined(args.violations()), "size must be between 1 and 20");

        // No `@Valid` on the Java record's list, so an empty item is the executor's problem.
        let args: GetTableInfoArgs = serde_json::from_str(r#"{"tables":[{}]}"#).unwrap();
        assert!(args.violations().is_empty());
    }

    #[test]
    fn a_missing_required_text_raises_both_of_its_annotations() {
        let args: ExecuteSqlArgs = serde_json::from_str("{}").unwrap();
        assert_eq!(
            joined(args.violations()),
            "must not be blank; must not be blank; must not be null; must not be null"
        );

        let args: ExecuteSqlArgs =
            serde_json::from_str(r#"{"data_source_id":"ds-1","sql":" "}"#).unwrap();
        assert_eq!(joined(args.violations()), "must not be blank");
    }

    #[test]
    fn the_write_tools_check_length_and_alias_bounds_per_item() {
        let long = "x".repeat(501);
        let args: UpdateTableInfoArgs = serde_json::from_str(&format!(
            r#"{{"tables":[{{"data_source_id":"ds-1","table":"t","description":"{long}",
               "aliases":["{}"]}}]}}"#,
            "y".repeat(101)
        ))
        .unwrap();
        assert_eq!(
            joined(args.violations()),
            "size must be between 0 and 100; size must be between 0 and 500"
        );

        // A valid item keeps its omitted fields as `None`, which means "leave the current value".
        let args: UpdateTableInfoArgs =
            serde_json::from_str(r#"{"tables":[{"data_source_id":"ds-1","table":"t"}]}"#).unwrap();
        assert!(args.violations().is_empty());
        assert_eq!(args.tables.as_ref().unwrap()[0].description, None);

        // One item with nothing in it: each of the three locate fields reports both of its checks.
        let args: UpdateColumnInfoArgs = serde_json::from_str(r#"{"columns":[{}]}"#).unwrap();
        assert_eq!(
            joined(args.violations()),
            "must not be blank; must not be blank; must not be blank; \
             must not be null; must not be null; must not be null"
        );
        let args: UpdateColumnInfoArgs = serde_json::from_str(
            r#"{"columns":[{"data_source_id":"ds-1","table":"t","column":"c"}]}"#,
        )
        .unwrap();
        assert!(args.violations().is_empty());

        // The list bound is checked before the items, so an empty list says only that.
        let args: UpsertTermArgs = serde_json::from_str(r#"{"terms":[]}"#).unwrap();
        assert_eq!(joined(args.violations()), "size must be between 1 and 20");
    }

    #[test]
    fn term_names_are_bounded_like_the_java_annotations() {
        let args: UpsertTermArgs = serde_json::from_str(r#"{"terms":[{"name":"GMV"}]}"#).unwrap();
        assert_eq!(
            joined(args.violations()),
            "must not be blank; must not be null"
        );

        let args: UpsertTermArgs = serde_json::from_str(&format!(
            r#"{{"terms":[{{"subject_name":"{}","name":"{}"}}]}}"#,
            "s".repeat(201),
            "n".repeat(201)
        ))
        .unwrap();
        assert_eq!(
            joined(args.violations()),
            "size must be between 0 and 200; size must be between 0 and 200"
        );
    }

    #[test]
    fn every_prebuilt_schema_is_an_object_that_forbids_extra_properties() {
        for tool_type in McpToolType::ALL {
            let schema = input_schema(tool_type);
            assert_eq!(schema["type"], "object", "{tool_type:?}");
            assert_eq!(
                schema["additionalProperties"], false,
                "{tool_type:?} forbids extra properties"
            );
        }
        // The list tool has nothing to declare; the search tool requires its keyword list.
        assert!(
            input_schema(McpToolType::ListTables)
                .get("required")
                .is_none()
        );
        assert_eq!(
            input_schema(McpToolType::SearchMetadata)["required"][0],
            "keywords"
        );
        assert_eq!(
            input_schema(McpToolType::GetTableInfo)["properties"]["tables"]["items"]["required"],
            json!(["data_source_id", "table"])
        );
        assert_eq!(
            input_schema(McpToolType::UpsertTerm)["properties"]["terms"]["items"]["properties"]["subject_name"]
                ["maxLength"],
            200
        );
    }
}
