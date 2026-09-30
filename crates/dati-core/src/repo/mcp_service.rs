//! The `mcp_service` table plus its `mcp_service_data_scope` rows: a published MCP endpoint and
//! the data sources it is allowed to talk about.

use sqlx::Row;
use sqlx::any::AnyRow;

use crate::db::Db;
use crate::error::{DatiError, ErrorCode, Result};
use crate::model::base::{Audit, PageReq};
use crate::model::mcp::{McpDataScope, McpDataScopeType, McpService, McpServiceStatus};
use crate::model::permission::ResourceType;
use crate::time::new_id;

use super::resource::Visibility;
use super::{Filter, Statement, count, flag, optional, required};

const COLUMNS: &str = "id, created_by, created_at, updated_by, updated_at, name, description, \
     deleted, code, status, active_version_id, active_version_number";

const TABLE: &str = "mcp_service";

const SCOPE: &str = "mcp_service_data_scope";

const SCOPE_COLUMNS: &str = "id, created_by, created_at, updated_by, updated_at, service_id, \
     scope_type, reference_id";

pub fn to_service(row: &AnyRow) -> Result<McpService> {
    let status = match optional(row, "status") {
        None => None,
        Some(stored) => Some(McpServiceStatus::from_stored(&stored).ok_or_else(|| {
            tracing::error!(stored, "mcp service has an unknown status");
            DatiError::new(ErrorCode::InternalError, [stored])
        })?),
    };
    Ok(McpService {
        id: required(row, "id")?,
        name: optional(row, "name"),
        description: optional(row, "description"),
        deleted: flag(row, "deleted"),
        code: required(row, "code")?,
        status,
        active_version_id: optional(row, "active_version_id"),
        active_version_number: row
            .try_get::<i64, _>("active_version_number")
            .or_else(|_| {
                row.try_get::<i32, _>("active_version_number")
                    .map(i64::from)
            })
            .ok(),
        audit: Audit {
            created_by: optional(row, "created_by"),
            created_at: optional(row, "created_at"),
            updated_by: optional(row, "updated_by"),
            updated_at: optional(row, "updated_at"),
        },
    })
}

pub async fn insert(db: &Db, service: &McpService) -> Result<String> {
    Statement::new(format!(
        "INSERT INTO {TABLE} (id, created_by, created_at, updated_by, updated_at, name, \
         description, deleted, code, status, active_version_id, active_version_number) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    ))
    .bind(service.id.clone())
    .bind(service.audit.created_by.clone())
    .bind(service.audit.created_at.clone())
    .bind(service.audit.updated_by.clone())
    .bind(service.audit.updated_at.clone())
    .bind(service.name.clone())
    .bind(service.description.clone())
    .bind(service.deleted)
    .bind(service.code.clone())
    .bind(service.status.map(|status| status.as_str().to_string()))
    .bind(service.active_version_id.clone())
    .bind(service.active_version_number)
    .execute(db)
    .await?;
    Ok(service.id.clone())
}

/// The stored row, or `MS001`.
pub async fn find(db: &Db, id: &str) -> Result<Option<McpService>> {
    let row = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE id = ? AND deleted = ?"
    ))
    .bind(id)
    .bind(false)
    .fetch_optional(db)
    .await?;
    row.as_ref().map(to_service).transpose()
}

pub async fn find_by_code(db: &Db, code: &str) -> Result<Option<McpService>> {
    let row = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE code = ? AND deleted = ?"
    ))
    .bind(code)
    .bind(false)
    .fetch_optional(db)
    .await?;
    row.as_ref().map(to_service).transpose()
}

/// A code already taken by a live service. Codes of soft-deleted services are free again, which is
/// what the Java `existsByCode` derived query did.
pub async fn code_taken(db: &Db, code: &str) -> Result<bool> {
    let statement = Statement::new(format!(
        "SELECT COUNT(*) AS total FROM {TABLE} WHERE code = ? AND deleted = ?"
    ))
    .bind(code)
    .bind(false);
    Ok(count(&statement, db).await? > 0)
}

/// Writes back the three columns the Java update copied: the code, the status and the published
/// version never move through this path.
pub async fn update_profile(db: &Db, service: &McpService) -> Result<()> {
    Statement::new(format!(
        "UPDATE {TABLE} SET name = ?, description = ?, updated_by = ?, updated_at = ? WHERE id = ?"
    ))
    .bind(service.name.clone())
    .bind(service.description.clone())
    .bind(service.audit.updated_by.clone())
    .bind(service.audit.updated_at.clone())
    .bind(service.id.clone())
    .execute(db)
    .await?;
    Ok(())
}

/// Writes back the release columns a publish or a state transition moves: the status, the active
/// snapshot's id and number, and the audit stamp. The profile and the code stay where they are.
pub async fn update_release(db: &Db, service: &McpService) -> Result<()> {
    Statement::new(format!(
        "UPDATE {TABLE} SET status = ?, active_version_id = ?, active_version_number = ?, \
         updated_by = ?, updated_at = ? WHERE id = ?"
    ))
    .bind(service.status.map(|status| status.as_str().to_string()))
    .bind(service.active_version_id.clone())
    .bind(service.active_version_number)
    .bind(service.audit.updated_by.clone())
    .bind(service.audit.updated_at.clone())
    .bind(service.id.clone())
    .execute(db)
    .await?;
    Ok(())
}

/// The profile half of a rollback: the version being restored writes its name, description and code
/// back onto the live service row, so the draft again reads as that snapshot.
pub async fn restore_basic(db: &Db, service: &McpService) -> Result<()> {
    Statement::new(format!(
        "UPDATE {TABLE} SET name = ?, description = ?, code = ?, updated_by = ?, updated_at = ? \
         WHERE id = ?"
    ))
    .bind(service.name.clone())
    .bind(service.description.clone())
    .bind(service.code.clone())
    .bind(service.audit.updated_by.clone())
    .bind(service.audit.updated_at.clone())
    .bind(service.id.clone())
    .execute(db)
    .await?;
    Ok(())
}

/// `GET /v1/mcp-services`: newest first. A keyword matches the name or the code loosely, or the id
/// exactly, and a non-admin only sees the services it owns or was granted.
pub async fn page(
    db: &Db,
    keyword: Option<&str>,
    status: Option<McpServiceStatus>,
    visibility: Visibility<'_>,
    page_req: PageReq,
) -> Result<(Vec<McpService>, i64)> {
    let mut filter = Filter::default().add("deleted = ?", [false.into()]);
    if let Some(keyword) = keyword.filter(|value| !value.trim().is_empty()) {
        let pattern = format!("%{}%", keyword.to_lowercase());
        filter = filter.add(
            "(LOWER(name) LIKE ? OR LOWER(code) LIKE ? OR id = ?)",
            [pattern.clone().into(), pattern.into(), keyword.into()],
        );
    }
    if let Some(status) = status {
        filter = filter.add("status = ?", [status.as_str().into()]);
    }
    filter = visibility.apply(filter, ResourceType::McpService);
    let total = count(&filter.count(TABLE), db).await?;
    let rows = filter
        .select(COLUMNS, TABLE)
        .push_text(" ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?")
        .bind(page_req.size)
        .bind(page_req.offset())
        .fetch_all(db)
        .await?;
    Ok((
        rows.iter().map(to_service).collect::<Result<Vec<_>>>()?,
        total,
    ))
}

/// The Java service deleted the whole configuration graph and then the row itself, in one
/// transaction; the caller removes the children first.
pub async fn remove(db: &Db, id: &str) -> Result<bool> {
    let affected = Statement::new(format!("DELETE FROM {TABLE} WHERE id = ?"))
        .bind(id)
        .execute(db)
        .await?;
    Ok(affected > 0)
}

// ---- data scope ----------------------------------------------------------------------------

fn to_scope(row: &AnyRow) -> Result<McpDataScope> {
    let stored = required(row, "scope_type")?;
    let scope_type = McpDataScopeType::from_stored(&stored).ok_or_else(|| {
        tracing::error!(stored, "data scope row has an unknown scope type");
        DatiError::new(ErrorCode::MsScopeError, [stored])
    })?;
    Ok(McpDataScope {
        id: required(row, "id")?,
        service_id: required(row, "service_id")?,
        scope_type,
        reference_id: required(row, "reference_id")?,
        audit: Audit {
            created_by: optional(row, "created_by"),
            created_at: optional(row, "created_at"),
            updated_by: optional(row, "updated_by"),
            updated_at: optional(row, "updated_at"),
        },
    })
}

/// Rows in whatever order the database picked: the Java query had no `ORDER BY`, and the resolved
/// data source list follows that order.
pub async fn scopes(db: &Db, service_id: &str) -> Result<Vec<McpDataScope>> {
    let rows = Statement::new(format!(
        "SELECT {SCOPE_COLUMNS} FROM {SCOPE} WHERE service_id = ?"
    ))
    .bind(service_id)
    .fetch_all(db)
    .await?;
    rows.iter().map(to_scope).collect()
}

pub async fn clear_scopes(db: &Db, service_id: &str) -> Result<u64> {
    let affected = Statement::new(format!("DELETE FROM {SCOPE} WHERE service_id = ?"))
        .bind(service_id)
        .execute(db)
        .await?;
    Ok(affected)
}

/// Every scope row of every listed service, used by the publish path and the scope label step.
pub async fn scopes_of(db: &Db, service_ids: &[String]) -> Result<Vec<McpDataScope>> {
    if service_ids.is_empty() {
        return Ok(Vec::new());
    }
    let (clause, binds) = super::in_clause("service_id", service_ids);
    let mut statement = Statement::new(format!(
        "SELECT {SCOPE_COLUMNS} FROM {SCOPE} WHERE {clause}"
    ));
    for bind in binds {
        statement = statement.bind(bind);
    }
    let rows = statement.fetch_all(db).await?;
    rows.iter().map(to_scope).collect()
}

/// A `PUT` replaces the whole scope, so each row is written with a fresh id.
pub async fn insert_scope(
    db: &Db,
    service_id: &str,
    scope: &McpDataScope,
    audit: &Audit,
) -> Result<()> {
    Statement::new(format!(
        "INSERT INTO {SCOPE} (id, created_by, created_at, updated_by, updated_at, service_id, \
         scope_type, reference_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
    ))
    .bind(new_id())
    .bind(audit.created_by.clone())
    .bind(audit.created_at.clone())
    .bind(audit.updated_by.clone())
    .bind(audit.updated_at.clone())
    .bind(service_id.to_string())
    .bind(scope.scope_type.as_str().to_string())
    .bind(scope.reference_id.clone())
    .execute(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::base::Audit;
    use crate::repo::resource::Visibility;
    use crate::time::new_id;

    async fn service(code: &str, owner: &str) -> McpService {
        McpService {
            id: new_id(),
            name: Some("销售服务".into()),
            description: Some("desc".into()),
            deleted: false,
            code: code.into(),
            status: Some(McpServiceStatus::Draft),
            active_version_id: None,
            active_version_number: None,
            audit: Audit::new(owner),
        }
    }

    #[tokio::test]
    async fn a_service_round_trips_through_the_store() {
        let db = Db::sqlite_temp().await;
        let row = service("sales", "u-1").await;
        insert(&db, &row).await.unwrap();

        let found = find(&db, &row.id).await.unwrap().unwrap();
        assert_eq!(found, row);
        assert_eq!(
            found.active_version_number, None,
            "an unpublished service has no version"
        );
        assert_eq!(
            find_by_code(&db, "sales").await.unwrap().unwrap().id,
            row.id
        );
        assert!(find_by_code(&db, "crm").await.unwrap().is_none());
        assert!(code_taken(&db, "sales").await.unwrap());
        assert!(find(&db, "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn only_the_profile_columns_follow_an_update() {
        let db = Db::sqlite_temp().await;
        let mut row = service("sales", "u-1").await;
        insert(&db, &row).await.unwrap();

        row.name = None;
        row.description = Some("changed".into());
        row.code = "ignored".into();
        row.status = Some(McpServiceStatus::Published);
        row.audit.touch("u-2");
        update_profile(&db, &row).await.unwrap();

        let found = find(&db, &row.id).await.unwrap().unwrap();
        assert!(found.name.is_none());
        assert_eq!(found.description.as_deref(), Some("changed"));
        assert_eq!(
            found.code, "sales",
            "the code never moves through an update"
        );
        assert_eq!(found.status, Some(McpServiceStatus::Draft));
        assert_eq!(found.audit.updated_by.as_deref(), Some("u-2"));
        assert_eq!(found.audit.created_by.as_deref(), Some("u-1"));
    }

    #[tokio::test]
    async fn paging_filters_on_keyword_status_and_visibility() {
        let db = Db::sqlite_temp().await;
        let mine = service("sales", "u-1").await;
        let mut theirs = service("CRM", "u-2").await;
        theirs.name = Some("客户关系".into());
        for row in [&mine, &theirs] {
            insert(&db, row).await.unwrap();
        }
        let page_req = PageReq { page: 1, size: 10 };

        let (rows, total) = page(&db, None, None, Visibility::All, page_req)
            .await
            .unwrap();
        assert_eq!(total, 2);
        assert_eq!(rows.len(), 2);

        // The Java query matched name OR code case-insensitively, plus an exact id.
        for (keyword, expected) in [
            ("SALES", &mine),
            ("crm", &theirs),
            ("销售", &mine),
            (mine.id.as_str(), &mine),
        ] {
            let (rows, total) = page(&db, Some(keyword), None, Visibility::All, page_req)
                .await
                .unwrap();
            assert_eq!(total, 1, "keyword {keyword}");
            assert_eq!(rows[0].id, expected.id, "keyword {keyword}");
        }

        let (rows, _) = page(
            &db,
            None,
            Some(McpServiceStatus::Published),
            Visibility::All,
            page_req,
        )
        .await
        .unwrap();
        assert!(rows.is_empty(), "both services are still DRAFT");

        let (rows, total) = page(&db, None, None, Visibility::User("u-1"), page_req)
            .await
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].id, mine.id);

        let (_, total) = page(&db, Some("nothing"), None, Visibility::All, page_req)
            .await
            .unwrap();
        assert_eq!(total, 0);
    }

    #[tokio::test]
    async fn a_deleted_service_frees_its_code() {
        let db = Db::sqlite_temp().await;
        let row = service("sales", "u-1").await;
        insert(&db, &row).await.unwrap();
        assert!(code_taken(&db, "sales").await.unwrap());

        assert!(remove(&db, &row.id).await.unwrap());
        assert!(!code_taken(&db, "sales").await.unwrap());
        assert!(find(&db, &row.id).await.unwrap().is_none());
        assert!(!remove(&db, &row.id).await.unwrap());
    }

    #[tokio::test]
    async fn a_scope_is_replaced_as_a_whole() {
        let db = Db::sqlite_temp().await;
        let row = service("sales", "u-1").await;
        insert(&db, &row).await.unwrap();
        let audit = Audit::new("u-1");
        let scope = |reference: &str| McpDataScope {
            id: String::new(),
            service_id: row.id.clone(),
            scope_type: McpDataScopeType::DataSource,
            reference_id: reference.into(),
            audit: audit.clone(),
        };

        insert_scope(&db, &row.id, &scope("d-1"), &audit)
            .await
            .unwrap();
        insert_scope(&db, &row.id, &scope("d-2"), &audit)
            .await
            .unwrap();
        assert_eq!(scopes(&db, &row.id).await.unwrap().len(), 2);

        assert_eq!(clear_scopes(&db, &row.id).await.unwrap(), 2);
        assert!(scopes(&db, &row.id).await.unwrap().is_empty());

        insert_scope(&db, &row.id, &scope("d-3"), &audit)
            .await
            .unwrap();
        let all = scopes_of(&db, &[row.id.clone(), "other".into()])
            .await
            .unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].reference_id, "d-3");
        assert!(scopes_of(&db, &[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_scope_row_with_an_unknown_type_is_reported() {
        let db = Db::sqlite_temp().await;
        sqlx::query(
            "INSERT INTO mcp_service_data_scope (id, service_id, scope_type, reference_id) \
             VALUES ('s-1', 'svc-1', 'TABLE', 't-1')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        let err = scopes(&db, "svc-1").await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::MsScopeError);
    }
}
