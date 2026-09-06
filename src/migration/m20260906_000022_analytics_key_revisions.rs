use sea_orm::ConnectionTrait;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// A key is a shared dimension: one key names many requests. Marking every
// dependent request would put an unbounded fan-out on the capture writer's hot
// path, and the busiest keys are exactly the ones a rename would fan out over.
// Requests carry key_id, never the key name, so nothing request-owned changes
// when a key is renamed. One coalesced queue entry per key therefore carries
// the whole correction at constant cost, and gives each key its own revision so
// the destination guard can decide newest-wins per key rather than per snapshot.
//
// Existing keys are deliberately not seeded: an unchanged key needs no
// correction, and the request read path still observes its name.
const SCHEMA: &str = "
CREATE TABLE gateway_analytics_key_revisions (
    key_id TEXT PRIMARY KEY NOT NULL,
    revision INTEGER NOT NULL CHECK (typeof(revision) = 'integer' AND revision > 0),
    first_pending_revision INTEGER,
    changed_at TEXT NOT NULL,
    CHECK (first_pending_revision IS NULL OR first_pending_revision <= revision)
);
CREATE INDEX gateway_analytics_key_pending ON gateway_analytics_key_revisions
    (first_pending_revision, key_id) WHERE first_pending_revision IS NOT NULL;
";

// Deletion is not tracked. Analytics retains key names so already projected
// requests keep a label, and a key that no longer exists cannot be relabelled.
fn mark() -> String {
    "UPDATE gateway_analytics_clock SET revision = revision + 1 WHERE id = 1;
     INSERT INTO gateway_analytics_key_revisions
         (key_id, revision, first_pending_revision, changed_at)
     SELECT NEW.id, revision, revision, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
     FROM gateway_analytics_clock WHERE id = 1
     ON CONFLICT(key_id) DO UPDATE SET
         revision = excluded.revision,
         first_pending_revision = COALESCE(gateway_analytics_key_revisions.first_pending_revision, excluded.revision),
         changed_at = excluded.changed_at;"
        .to_owned()
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let connection = manager.get_connection();
        connection.execute_unprepared(SCHEMA).await?;
        connection
            .execute_unprepared(&format!(
                "CREATE TRIGGER analytics_gateway_keys_INSERT AFTER INSERT ON gateway_keys
                 BEGIN {} END;",
                mark()
            ))
            .await?;
        connection
            .execute_unprepared(&format!(
                "CREATE TRIGGER analytics_gateway_keys_UPDATE
                 AFTER UPDATE OF name, user_id ON gateway_keys
                 WHEN NEW.name IS NOT OLD.name OR NEW.user_id IS NOT OLD.user_id
                 BEGIN {} END;",
                mark()
            ))
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DROP TRIGGER analytics_gateway_keys_UPDATE;
                 DROP TRIGGER analytics_gateway_keys_INSERT;
                 DROP TABLE gateway_analytics_key_revisions;",
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::migration::Migrator;
    use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement};
    use sea_orm_migration::MigratorTrait;

    async fn database() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        db.execute_unprepared(
            "INSERT INTO gateway_analytics_generations VALUES('g',0,'2026-01-01T00:00:00.000Z');
             INSERT INTO users(id,email_normalized,email_display,role,status,auth_version,created_at,updated_at)
             VALUES('u','u@example.com','u@example.com','user','active',0,'2026-01-01','2026-01-01');
             INSERT INTO gateway_keys(id,user_id,name,allowed_providers,created_at)
             VALUES('k','u','old name','[]','2026-01-01T00:00:00.000Z');",
        )
        .await
        .unwrap();
        db
    }

    async fn key_revision(db: &DatabaseConnection) -> (i64, Option<i64>) {
        let row = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT r.revision, p.first_pending_revision FROM gateway_analytics_key_revisions r
             LEFT JOIN gateway_analytics_pending_keys p
                 ON p.key_id = r.key_id AND p.generation = 'g'
             WHERE r.key_id = 'k'",
            ))
            .await
            .unwrap()
            .unwrap();
        (
            row.try_get("", "revision").unwrap(),
            row.try_get("", "first_pending_revision").unwrap(),
        )
    }

    #[tokio::test]
    async fn renaming_a_key_revises_only_the_key_and_keeps_its_pending_boundary() {
        let db = database().await;
        let (created, pending) = key_revision(&db).await;
        assert_eq!(pending, Some(created));
        db.execute_unprepared("DELETE FROM gateway_analytics_pending_keys")
            .await
            .unwrap();
        db.execute_unprepared("UPDATE gateway_keys SET name = 'new name' WHERE id = 'k'")
            .await
            .unwrap();
        let (renamed, pending) = key_revision(&db).await;
        assert!(renamed > created);
        assert_eq!(pending, Some(renamed));
        db.execute_unprepared("UPDATE gateway_keys SET name = 'newer name' WHERE id = 'k'")
            .await
            .unwrap();
        let (again, pending) = key_revision(&db).await;
        assert!(again > renamed);
        assert_eq!(
            pending,
            Some(renamed),
            "an unacknowledged rename must keep the older pending boundary"
        );
        let requests = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT COUNT(*) n FROM gateway_analytics_revisions",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(requests.try_get::<i64>("", "n").unwrap(), 0);
    }

    #[tokio::test]
    async fn unrelated_key_columns_do_not_revise_the_key_dimension() {
        let db = database().await;
        let (created, _) = key_revision(&db).await;
        db.execute_unprepared("DELETE FROM gateway_analytics_pending_keys")
            .await
            .unwrap();
        db.execute_unprepared(
            "UPDATE gateway_keys SET revoked_at = '2026-01-02T00:00:00.000Z' WHERE id = 'k'",
        )
        .await
        .unwrap();
        db.execute_unprepared("UPDATE gateway_keys SET name = 'old name' WHERE id = 'k'")
            .await
            .unwrap();
        let (revision, pending) = key_revision(&db).await;
        assert_eq!(revision, created);
        assert_eq!(pending, None);
    }
}
