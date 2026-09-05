use sea_orm::ConnectionTrait;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Install tracking without scanning history. Historical facts receive revisions
// in bounded backfill transactions after tracking is enabled.
const SCHEMA: &str = "
CREATE TABLE gateway_analytics_clock (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    revision INTEGER NOT NULL CHECK (typeof(revision) = 'integer' AND revision >= 0),
    active_generation TEXT
);
INSERT INTO gateway_analytics_clock (id, revision) VALUES (1, 0);
CREATE TABLE gateway_analytics_revisions (
    request_id TEXT PRIMARY KEY NOT NULL,
    revision INTEGER NOT NULL CHECK (typeof(revision) = 'integer' AND revision > 0),
    first_pending_revision INTEGER,
    changed_at TEXT NOT NULL,
    deleted INTEGER NOT NULL CHECK (deleted IN (0, 1)),
    CHECK (first_pending_revision IS NULL OR first_pending_revision <= revision)
);
CREATE INDEX gateway_analytics_pending ON gateway_analytics_revisions
    (first_pending_revision, request_id) WHERE first_pending_revision IS NOT NULL;
";

// No foreign key to capture: deletion must leave a durable tombstone. Retaining
// first_pending_revision prevents a newer mutation from hiding older unfinished
// work behind a publication boundary.
fn mark(request: &str) -> String {
    format!(
        "UPDATE gateway_analytics_clock SET revision = revision + 1 WHERE id = 1;
         INSERT INTO gateway_analytics_revisions
             (request_id, revision, first_pending_revision, changed_at, deleted)
         SELECT {request}, revision, revision, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
             NOT EXISTS (SELECT 1 FROM gateway_requests WHERE id = {request})
         FROM gateway_analytics_clock WHERE id = 1
         ON CONFLICT(request_id) DO UPDATE SET
             revision = excluded.revision,
             first_pending_revision = COALESCE(gateway_analytics_revisions.first_pending_revision, excluded.revision),
             changed_at = excluded.changed_at,
             deleted = excluded.deleted;"
    )
}

const TRACKED: [(&str, &str); 3] = [
    ("gateway_requests", "id"),
    ("gateway_usage", "request_id"),
    ("gateway_request_metrics", "request_id"),
];

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let connection = manager.get_connection();
        connection.execute_unprepared(SCHEMA).await?;
        for (table, key) in TRACKED {
            for event in ["INSERT", "UPDATE", "DELETE"] {
                let row = if event == "DELETE" { "OLD" } else { "NEW" };
                connection
                    .execute_unprepared(&format!(
                        "CREATE TRIGGER analytics_{table}_{event} AFTER {event} ON {table}
                         BEGIN {} END;",
                        mark(&format!("{row}.{key}"))
                    ))
                    .await?;
            }
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let connection = manager.get_connection();
        for (table, _) in TRACKED {
            for event in ["INSERT", "UPDATE", "DELETE"] {
                connection
                    .execute_unprepared(&format!("DROP TRIGGER analytics_{table}_{event}"))
                    .await?;
            }
        }
        connection
            .execute_unprepared(
                "DROP TABLE gateway_analytics_revisions; DROP TABLE gateway_analytics_clock;",
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use sea_orm::{Database, DatabaseConnection, DbBackend, Statement, TransactionTrait};

    async fn database() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        db
    }

    async fn request(db: &impl ConnectionTrait) {
        db.execute_unprepared(
            "INSERT INTO gateway_requests
             (id, request_id, provider, protocol, method, endpoint, started_at, request_bytes)
             VALUES ('r', 'external', 'p', 'anthropic_messages', 'POST', '/messages', '2026-01-01T00:00:00.000Z', 0)",
        ).await.unwrap();
    }

    async fn revision(db: &DatabaseConnection) -> (i64, Option<i64>, bool) {
        let row = db.query_one_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT revision, first_pending_revision, deleted FROM gateway_analytics_revisions WHERE request_id = 'r'",
        )).await.unwrap().unwrap();
        (
            row.try_get("", "revision").unwrap(),
            row.try_get("", "first_pending_revision").unwrap(),
            row.try_get("", "deleted").unwrap(),
        )
    }

    #[tokio::test]
    async fn newer_revision_preserves_pending_boundary_and_rejects_stale_ack() {
        let db = database().await;
        request(&db).await;
        let (first, pending, deleted) = revision(&db).await;
        assert_eq!(pending, Some(first));
        assert!(!deleted);
        db.execute_unprepared("UPDATE gateway_requests SET http_status = 200 WHERE id = 'r'")
            .await
            .unwrap();
        let (second, pending, _) = revision(&db).await;
        assert!(second > first);
        assert_eq!(pending, Some(first));
        let result = db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE gateway_analytics_revisions SET first_pending_revision = NULL WHERE request_id = 'r' AND revision = ?",
            [first.into()],
        )).await.unwrap();
        assert_eq!(result.rows_affected(), 0);
        assert_eq!(revision(&db).await.1, Some(first));
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE gateway_analytics_revisions SET first_pending_revision = NULL WHERE request_id = 'r' AND revision = ?",
            [second.into()],
        )).await.unwrap();
        db.execute_unprepared("UPDATE gateway_requests SET http_status = 201 WHERE id = 'r'")
            .await
            .unwrap();
        let (third, pending, _) = revision(&db).await;
        assert!(third > second);
        assert_eq!(pending, Some(third));
    }

    #[tokio::test]
    async fn revision_exhaustion_rolls_back_the_source_mutation() {
        let db = database().await;
        request(&db).await;
        db.execute_unprepared(
            "UPDATE gateway_analytics_clock SET revision = 9223372036854775807 WHERE id = 1",
        )
        .await
        .unwrap();
        assert!(
            db.execute_unprepared("UPDATE gateway_requests SET http_status = 200 WHERE id = 'r'")
                .await
                .is_err()
        );
        let row = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT http_status FROM gateway_requests WHERE id = 'r'",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get::<Option<i64>>("", "http_status").unwrap(), None);
    }

    #[tokio::test]
    async fn rolled_back_capture_does_not_leave_pending_work() {
        let db = database().await;
        let tx = db.begin().await.unwrap();
        request(&tx).await;
        tx.rollback().await.unwrap();
        let row = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT COUNT(*) n FROM gateway_analytics_revisions",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get::<i64>("", "n").unwrap(), 0);
    }

    #[tokio::test]
    async fn file_backed_snapshot_keeps_facts_and_revision_together() {
        let fixture = crate::db::tests::FileDatabase::new().await;
        let db = &fixture.database;
        request(db).await;
        let reader = crate::db::reporting_connection(&fixture.url, db)
            .await
            .unwrap();
        let snapshot = reader.begin().await.unwrap();
        let sql = "SELECT a.revision, r.http_status FROM gateway_analytics_revisions a
                   JOIN gateway_requests r ON r.id = a.request_id WHERE r.id = 'r'";
        let before = snapshot
            .query_one_raw(Statement::from_string(DbBackend::Sqlite, sql))
            .await
            .unwrap()
            .unwrap();
        let first: i64 = before.try_get("", "revision").unwrap();
        db.execute_unprepared("UPDATE gateway_requests SET http_status = 200 WHERE id = 'r'")
            .await
            .unwrap();
        let during = snapshot
            .query_one_raw(Statement::from_string(DbBackend::Sqlite, sql))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(during.try_get::<i64>("", "revision").unwrap(), first);
        assert_eq!(
            during.try_get::<Option<i64>>("", "http_status").unwrap(),
            None
        );
        snapshot.commit().await.unwrap();
        assert!(revision(db).await.0 > first);
        reader.close().await.unwrap();
    }

    #[tokio::test]
    async fn deletion_retains_a_pending_tombstone() {
        let db = database().await;
        request(&db).await;
        let first = revision(&db).await.0;
        db.execute_unprepared("DELETE FROM gateway_requests WHERE id = 'r'")
            .await
            .unwrap();
        let (last, pending, deleted) = revision(&db).await;
        assert!(last > first);
        assert_eq!(pending, Some(first));
        assert!(deleted);
    }
}
