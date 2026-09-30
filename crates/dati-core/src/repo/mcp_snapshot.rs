//! The `mcp_service_snapshot` table: one serialized configuration document per published version.

use sqlx::Row;
use sqlx::any::AnyRow;

use crate::db::Db;
use crate::error::{DatiError, ErrorCode, Result};
use crate::model::base::Audit;
use crate::model::mcp_version::{McpServiceSnapshot, SnapshotContent};

use super::{Statement, integer, optional, required};

const TABLE: &str = "mcp_service_snapshot";

const COLUMNS: &str = "id, created_by, created_at, updated_by, updated_at, service_id, \
     version_number, release_note, snapshot_content";

fn to_snapshot(row: &AnyRow) -> Result<McpServiceSnapshot> {
    let stored = required(row, "snapshot_content")?;
    let content: SnapshotContent = serde_json::from_str(&stored).map_err(|error| {
        tracing::error!(%error, "stored snapshot content is unreadable");
        DatiError::with_message(ErrorCode::InternalError, "Internal server error")
    })?;
    Ok(McpServiceSnapshot {
        id: required(row, "id")?,
        service_id: required(row, "service_id")?,
        version_number: integer(row, "version_number"),
        release_note: optional(row, "release_note"),
        content,
        audit: Audit {
            created_by: optional(row, "created_by"),
            created_at: optional(row, "created_at"),
            updated_by: optional(row, "updated_by"),
            updated_at: optional(row, "updated_at"),
        },
    })
}

/// Stores one version. The caller has built the content and picked the id; the version number is
/// the caller's too, since it is `MAX + 1` read under the same edit lock.
pub async fn insert(db: &Db, snapshot: &McpServiceSnapshot) -> Result<String> {
    let content = serde_json::to_string(&snapshot.content)
        .map_err(|_| DatiError::with_message(ErrorCode::InternalError, "Internal server error"))?;
    Statement::new(format!(
        "INSERT INTO {TABLE} (id, created_by, created_at, updated_by, updated_at, service_id, \
         version_number, release_note, snapshot_content) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
    ))
    .bind(snapshot.id.clone())
    .bind(snapshot.audit.created_by.clone())
    .bind(snapshot.audit.created_at.clone())
    .bind(snapshot.audit.updated_by.clone())
    .bind(snapshot.audit.updated_at.clone())
    .bind(snapshot.service_id.clone())
    .bind(snapshot.version_number)
    .bind(snapshot.release_note.clone())
    .bind(content)
    .execute(db)
    .await?;
    Ok(snapshot.id.clone())
}

/// `MAX(version_number)`, or `None` for a service that has never been published.
pub async fn max_version(db: &Db, service_id: &str) -> Result<Option<i64>> {
    let row = Statement::new(format!(
        "SELECT MAX(version_number) AS max_version FROM {TABLE} WHERE service_id = ?"
    ))
    .bind(service_id)
    .fetch_one(db)
    .await?;
    Ok(row
        .try_get::<Option<i64>, _>("max_version")
        .or_else(|_| {
            row.try_get::<Option<i32>, _>("max_version")
                .map(|value| value.map(i64::from))
        })
        .unwrap_or(None))
}

pub async fn find(
    db: &Db,
    service_id: &str,
    version_number: i64,
) -> Result<Option<McpServiceSnapshot>> {
    let row = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE service_id = ? AND version_number = ?"
    ))
    .bind(service_id)
    .bind(version_number)
    .fetch_optional(db)
    .await?;
    row.as_ref().map(to_snapshot).transpose()
}

/// The active snapshot, looked up by the id the service points at.
pub async fn find_by_id(db: &Db, id: &str) -> Result<Option<McpServiceSnapshot>> {
    let row = Statement::new(format!("SELECT {COLUMNS} FROM {TABLE} WHERE id = ?"))
        .bind(id)
        .fetch_optional(db)
        .await?;
    row.as_ref().map(to_snapshot).transpose()
}

/// `GET /{id}/snapshots`: newest first.
pub async fn list(db: &Db, service_id: &str) -> Result<Vec<McpServiceSnapshot>> {
    let rows = Statement::new(format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE service_id = ? ORDER BY version_number DESC"
    ))
    .bind(service_id)
    .fetch_all(db)
    .await?;
    rows.iter().map(to_snapshot).collect()
}

/// Deleting a service took its version history with it.
pub async fn remove_all(db: &Db, service_id: &str) -> Result<u64> {
    let affected = Statement::new(format!("DELETE FROM {TABLE} WHERE service_id = ?"))
        .bind(service_id)
        .execute(db)
        .await?;
    Ok(affected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::base::Audit;
    use crate::model::mcp_version::SnapshotContent;

    fn snapshot(
        id: &str,
        service_id: &str,
        version: i64,
        note: Option<&str>,
    ) -> McpServiceSnapshot {
        McpServiceSnapshot {
            id: id.to_string(),
            service_id: service_id.to_string(),
            version_number: version,
            release_note: note.map(str::to_string),
            content: SnapshotContent::default(),
            audit: Audit::new("u-1"),
        }
    }

    #[tokio::test]
    async fn a_snapshot_round_trips_and_versions_are_scoped_to_one_service() {
        let db = Db::sqlite_temp().await;
        assert_eq!(max_version(&db, "svc-1").await.unwrap(), None);

        insert(&db, &snapshot("s-2", "svc-1", 2, Some("second")))
            .await
            .unwrap();
        insert(&db, &snapshot("s-1", "svc-1", 1, None))
            .await
            .unwrap();
        insert(&db, &snapshot("x-1", "svc-2", 9, None))
            .await
            .unwrap();

        assert_eq!(max_version(&db, "svc-1").await.unwrap(), Some(2));
        let found = find(&db, "svc-1", 1).await.unwrap().unwrap();
        assert_eq!(found.id, "s-1");
        assert_eq!(found.release_note, None);
        assert!(find(&db, "svc-1", 3).await.unwrap().is_none());
        assert!(find(&db, "svc-2", 1).await.unwrap().is_none());
        assert_eq!(
            find_by_id(&db, "s-2")
                .await
                .unwrap()
                .unwrap()
                .version_number,
            2
        );

        // The list is newest first and only ever names one service.
        let versions: Vec<i64> = list(&db, "svc-1")
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.version_number)
            .collect();
        assert_eq!(versions, vec![2, 1]);

        assert_eq!(remove_all(&db, "svc-1").await.unwrap(), 2);
        assert!(list(&db, "svc-1").await.unwrap().is_empty());
        assert_eq!(
            list(&db, "svc-2").await.unwrap().len(),
            1,
            "another service is untouched"
        );
    }

    #[tokio::test]
    async fn unreadable_stored_content_is_an_internal_error() {
        let db = Db::sqlite_temp().await;
        sqlx::query(
            "INSERT INTO mcp_service_snapshot (id, service_id, version_number, snapshot_content) \
             VALUES ('s-1', 'svc-1', 1, 'not json')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        let err = find(&db, "svc-1", 1).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::InternalError);
    }
}
