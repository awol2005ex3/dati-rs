//! Column and envelope shapes shared by every persisted resource.
//!
//! JSON is snake_case on the wire everywhere, matching the Java project's global Jackson
//! naming strategy, so model fields are named exactly as they serialize.

use serde::{Deserialize, Serialize};

/// Auditing columns present on all tables (the Java `BasePO`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Audit {
    pub created_by: Option<String>,
    pub created_at: Option<String>,
    pub updated_by: Option<String>,
    pub updated_at: Option<String>,
}

impl Audit {
    pub fn new(who: &str) -> Self {
        let now = crate::time::now_iso();
        Self {
            created_by: Some(who.to_string()),
            created_at: Some(now.clone()),
            updated_by: Some(who.to_string()),
            updated_at: Some(now),
        }
    }

    /// Stamps an update; the original creator is left untouched.
    pub fn touch(&mut self, who: &str) {
        self.updated_by = Some(who.to_string());
        self.updated_at = Some(crate::time::now_iso());
    }

    /// The two author ids a page needs resolved to display names.
    pub fn authors(&self) -> impl Iterator<Item = &str> {
        self.created_by
            .as_deref()
            .into_iter()
            .chain(self.updated_by.as_deref())
    }

    /// Rows written without an authenticated author (register, API keys) carry timestamps only,
    /// which is what Hibernate's `@CreationTimestamp` produced on the Java side.
    pub fn timestamped() -> Self {
        let now = crate::time::now_iso();
        Self {
            created_by: None,
            created_at: Some(now.clone()),
            updated_by: None,
            updated_at: Some(now),
        }
    }
}

/// Resolves the two audit authors of a response payload into display names, the Java
/// `BaseAssembler.fillUserInfo` step every list endpoint ran.
pub fn author_names(
    audit: &Audit,
    names: &std::collections::HashMap<String, String>,
) -> (Option<String>, Option<String>) {
    (
        audit
            .created_by
            .as_deref()
            .and_then(|id| names.get(id))
            .cloned(),
        audit
            .updated_by
            .as_deref()
            .and_then(|id| names.get(id))
            .cloned(),
    )
}

/// Name/description/soft-delete columns (the Java `BaseResourcePO`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resource {
    pub name: Option<String>,
    pub description: Option<String>,
    pub deleted: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct IdResponse {
    pub id: String,
}

impl IdResponse {
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }
}

/// The Java `PageResponse`: 1-based pages, no sort parameter is accepted anywhere.
#[derive(Debug, Clone, Serialize)]
pub struct PageResponse<T> {
    pub data: Vec<T>,
    pub total: i64,
    pub page: i64,
    pub size: i64,
    pub total_pages: i64,
}

impl<T> PageResponse<T> {
    pub fn new(data: Vec<T>, total: i64, page: i64, size: i64) -> Self {
        let total_pages = if size > 0 {
            total.div_euclid(size) + i64::from(total.rem_euclid(size) != 0)
        } else {
            0
        };
        Self {
            data,
            total,
            page,
            size,
            total_pages,
        }
    }

    pub fn map<U>(self, f: impl Fn(T) -> U) -> PageResponse<U> {
        PageResponse {
            data: self.data.into_iter().map(f).collect(),
            total: self.total,
            page: self.page,
            size: self.size,
            total_pages: self.total_pages,
        }
    }
}

/// Query parameters accepted by list endpoints: `page` is 1-based, `size` capped at 100.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageReq {
    pub page: i64,
    pub size: i64,
}

impl PageReq {
    pub const fn offset(self) -> i64 {
        (self.page - 1) * self.size
    }
}

/// Encodes a `Vec<String>` for storage, or `None` when empty so columns stay clean.
pub fn write_json_list(values: &[String]) -> Option<String> {
    if values.is_empty() {
        return None;
    }
    Some(serde_json::to_string(values).unwrap_or_else(|_| "[]".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_response_reports_total_pages_like_the_java_helper() {
        let page = PageResponse::new(vec![1, 2], 42, 1, 10);
        assert_eq!(page.total_pages, 5);
        assert_eq!(PageResponse::new(Vec::<u8>::new(), 0, 1, 10).total_pages, 0);
        assert_eq!(PageResponse::new(Vec::<u8>::new(), 5, 1, 0).total_pages, 0);
        assert_eq!(PageResponse::new(Vec::<u8>::new(), 5, 1, 5).total_pages, 1);
    }

    #[test]
    fn page_offsets_are_one_indexed() {
        assert_eq!(PageReq { page: 1, size: 10 }.offset(), 0);
        assert_eq!(PageReq { page: 3, size: 10 }.offset(), 20);
    }

    #[test]
    fn json_lists_round_trip_through_text() {
        assert_eq!(write_json_list(&[]), None);
        let stored = write_json_list(&["a".to_string(), "b".to_string()]).unwrap();
        assert_eq!(stored, r#"["a","b"]"#);
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&stored).unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn audit_touch_keeps_the_creator() {
        let mut audit = Audit::new("u-1");
        let created_by = audit.created_by.clone();
        audit.touch("u-2");
        assert_eq!(audit.created_by, created_by);
        assert_eq!(audit.updated_by.as_deref(), Some("u-2"));
        assert_eq!(audit.created_at, audit.updated_at);
    }
}
