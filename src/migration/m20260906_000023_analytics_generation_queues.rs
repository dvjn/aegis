use sea_orm::ConnectionTrait;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// A revision row records what a fact looks like now; a queue row records that a
// generation still owes work for it. Splitting them lets a rebuilding generation
// hold its own backlog, so acknowledging the active generation cannot drain it.
//
// Fan-out is bounded by the registered generation count, which is one in the
// ordinary case and small even during a rebuild. An unregistered generation
// selects no rows, so it accumulates nothing and cannot be resumed from a queue
// it never had.
const SCHEMA: &str = "
CREATE TABLE gateway_analytics_generations (
    generation TEXT PRIMARY KEY NOT NULL CHECK (generation <> ''),
    registered_revision INTEGER NOT NULL
        CHECK (typeof(registered_revision) = 'integer' AND registered_revision >= 0),
    registered_at TEXT NOT NULL
);
CREATE TABLE gateway_analytics_pending_requests (
    generation TEXT NOT NULL
        REFERENCES gateway_analytics_generations (generation) ON DELETE CASCADE,
    request_id TEXT NOT NULL,
    first_pending_revision INTEGER NOT NULL
        CHECK (typeof(first_pending_revision) = 'integer' AND first_pending_revision > 0),
    first_pending_at TEXT NOT NULL,
    PRIMARY KEY (generation, request_id)
) WITHOUT ROWID;
CREATE INDEX gateway_analytics_pending_request_order ON gateway_analytics_pending_requests
    (generation, first_pending_revision, request_id);
CREATE TABLE gateway_analytics_pending_keys (
    generation TEXT NOT NULL
        REFERENCES gateway_analytics_generations (generation) ON DELETE CASCADE,
    key_id TEXT NOT NULL,
    first_pending_revision INTEGER NOT NULL
        CHECK (typeof(first_pending_revision) = 'integer' AND first_pending_revision > 0),
    first_pending_at TEXT NOT NULL,
    PRIMARY KEY (generation, key_id)
) WITHOUT ROWID;
CREATE INDEX gateway_analytics_pending_key_order ON gateway_analytics_pending_keys
    (generation, first_pending_revision, key_id);
";

// Work already owed belongs to whichever generation is active, and only that
// generation may acknowledge it. changed_at is the closest durable record of
// when the row entered the queue; it understates the age of a row re-dirtied
// since, which no longer happens once the queue owns the timestamp.
const ADOPT_ACTIVE_GENERATION: &str = "
INSERT INTO gateway_analytics_generations (generation, registered_revision, registered_at)
SELECT active_generation, revision, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
FROM gateway_analytics_clock WHERE id = 1 AND active_generation IS NOT NULL;
INSERT INTO gateway_analytics_pending_requests
    (generation, request_id, first_pending_revision, first_pending_at)
SELECT g.generation, r.request_id, r.first_pending_revision, r.changed_at
FROM gateway_analytics_generations g, gateway_analytics_revisions r
WHERE r.first_pending_revision IS NOT NULL;
INSERT INTO gateway_analytics_pending_keys
    (generation, key_id, first_pending_revision, first_pending_at)
SELECT g.generation, k.key_id, k.first_pending_revision, k.changed_at
FROM gateway_analytics_generations g, gateway_analytics_key_revisions k
WHERE k.first_pending_revision IS NOT NULL;
";

// The rename re-parses every trigger in the schema, and the capture triggers
// name these tables in their bodies, so only legacy_alter_table lets the rebuilt
// table take the original name.
// https://sqlite.org/lang_altertable.html#alter_table_rename
const DROP_PENDING_COLUMNS: &str = "
PRAGMA legacy_alter_table = ON;
CREATE TABLE gateway_analytics_revisions_rebuilt (
    request_id TEXT PRIMARY KEY NOT NULL,
    revision INTEGER NOT NULL CHECK (typeof(revision) = 'integer' AND revision > 0),
    changed_at TEXT NOT NULL,
    deleted INTEGER NOT NULL CHECK (deleted IN (0, 1))
);
INSERT INTO gateway_analytics_revisions_rebuilt
SELECT request_id, revision, changed_at, deleted FROM gateway_analytics_revisions;
DROP TABLE gateway_analytics_revisions;
ALTER TABLE gateway_analytics_revisions_rebuilt RENAME TO gateway_analytics_revisions;
CREATE TABLE gateway_analytics_key_revisions_rebuilt (
    key_id TEXT PRIMARY KEY NOT NULL,
    revision INTEGER NOT NULL CHECK (typeof(revision) = 'integer' AND revision > 0),
    changed_at TEXT NOT NULL
);
INSERT INTO gateway_analytics_key_revisions_rebuilt
SELECT key_id, revision, changed_at FROM gateway_analytics_key_revisions;
DROP TABLE gateway_analytics_key_revisions;
ALTER TABLE gateway_analytics_key_revisions_rebuilt RENAME TO gateway_analytics_key_revisions;
PRAGMA legacy_alter_table = OFF;
CREATE INDEX gateway_analytics_revision_order ON gateway_analytics_revisions (revision, request_id);
";

const TRACKED: [(&str, &str); 3] = [
    ("gateway_requests", "id"),
    ("gateway_usage", "request_id"),
    ("gateway_request_metrics", "request_id"),
];

fn drop_capture_triggers() -> String {
    let mut sql = String::new();
    for (table, _) in TRACKED {
        for event in ["INSERT", "UPDATE", "DELETE"] {
            sql.push_str(&format!("DROP TRIGGER analytics_{table}_{event};\n"));
        }
    }
    sql.push_str(
        "DROP TRIGGER analytics_gateway_keys_UPDATE;
         DROP TRIGGER analytics_gateway_keys_INSERT;",
    );
    sql
}

// Conflicting inserts do nothing at all, so both the revision that first made
// the row pending and the instant it happened survive every later re-dirty.
// Acknowledgement deletes the row, which clears them together.
fn enqueue(queue: &str, column: &str, id: &str) -> String {
    format!(
        "INSERT INTO {queue} (generation, {column}, first_pending_revision, first_pending_at)
         SELECT g.generation, {id}, c.revision, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         FROM gateway_analytics_generations g, gateway_analytics_clock c WHERE c.id = 1
         ON CONFLICT(generation, {column}) DO NOTHING;"
    )
}

fn mark_request(request: &str) -> String {
    format!(
        "UPDATE gateway_analytics_clock SET revision = revision + 1 WHERE id = 1;
         INSERT INTO gateway_analytics_revisions (request_id, revision, changed_at, deleted)
         SELECT {request}, revision, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
             NOT EXISTS (SELECT 1 FROM gateway_requests WHERE id = {request})
         FROM gateway_analytics_clock WHERE id = 1
         ON CONFLICT(request_id) DO UPDATE SET
             revision = excluded.revision,
             changed_at = excluded.changed_at,
             deleted = excluded.deleted;
         {}",
        enqueue("gateway_analytics_pending_requests", "request_id", request)
    )
}

fn mark_key() -> String {
    format!(
        "UPDATE gateway_analytics_clock SET revision = revision + 1 WHERE id = 1;
         INSERT INTO gateway_analytics_key_revisions (key_id, revision, changed_at)
         SELECT NEW.id, revision, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         FROM gateway_analytics_clock WHERE id = 1
         ON CONFLICT(key_id) DO UPDATE SET
             revision = excluded.revision,
             changed_at = excluded.changed_at;
         {}",
        enqueue("gateway_analytics_pending_keys", "key_id", "NEW.id")
    )
}

fn create_capture_triggers(request_body: &dyn Fn(&str) -> String, key_body: &str) -> String {
    let mut sql = String::new();
    for (table, key) in TRACKED {
        for event in ["INSERT", "UPDATE", "DELETE"] {
            let row = if event == "DELETE" { "OLD" } else { "NEW" };
            sql.push_str(&format!(
                "CREATE TRIGGER analytics_{table}_{event} AFTER {event} ON {table}
                 BEGIN {} END;\n",
                request_body(&format!("{row}.{key}"))
            ));
        }
    }
    sql.push_str(&format!(
        "CREATE TRIGGER analytics_gateway_keys_INSERT AFTER INSERT ON gateway_keys
         BEGIN {key_body} END;
         CREATE TRIGGER analytics_gateway_keys_UPDATE
         AFTER UPDATE OF name, user_id ON gateway_keys
         WHEN NEW.name IS NOT OLD.name OR NEW.user_id IS NOT OLD.user_id
         BEGIN {key_body} END;\n"
    ));
    sql
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let connection = manager.get_connection();
        connection
            .execute_unprepared(&drop_capture_triggers())
            .await?;
        connection.execute_unprepared(SCHEMA).await?;
        connection
            .execute_unprepared(ADOPT_ACTIVE_GENERATION)
            .await?;
        connection.execute_unprepared(DROP_PENDING_COLUMNS).await?;
        connection
            .execute_unprepared(&create_capture_triggers(&mark_request, &mark_key()))
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let connection = manager.get_connection();
        connection
            .execute_unprepared(&drop_capture_triggers())
            .await?;
        // One column cannot hold several generations' claims, so the oldest
        // surviving claim is restored and the rest are dropped with the queues.
        connection
            .execute_unprepared(
                "PRAGMA legacy_alter_table = ON;
                 CREATE TABLE gateway_analytics_revisions_rebuilt (
                     request_id TEXT PRIMARY KEY NOT NULL,
                     revision INTEGER NOT NULL
                         CHECK (typeof(revision) = 'integer' AND revision > 0),
                     first_pending_revision INTEGER,
                     changed_at TEXT NOT NULL,
                     deleted INTEGER NOT NULL CHECK (deleted IN (0, 1)),
                     CHECK (first_pending_revision IS NULL OR first_pending_revision <= revision)
                 );
                 INSERT INTO gateway_analytics_revisions_rebuilt
                 SELECT r.request_id, r.revision,
                     (SELECT MIN(p.first_pending_revision) FROM gateway_analytics_pending_requests p
                      WHERE p.request_id = r.request_id),
                     r.changed_at, r.deleted
                 FROM gateway_analytics_revisions r;
                 DROP TABLE gateway_analytics_revisions;
                 ALTER TABLE gateway_analytics_revisions_rebuilt
                     RENAME TO gateway_analytics_revisions;
                 CREATE TABLE gateway_analytics_key_revisions_rebuilt (
                     key_id TEXT PRIMARY KEY NOT NULL,
                     revision INTEGER NOT NULL
                         CHECK (typeof(revision) = 'integer' AND revision > 0),
                     first_pending_revision INTEGER,
                     changed_at TEXT NOT NULL,
                     CHECK (first_pending_revision IS NULL OR first_pending_revision <= revision)
                 );
                 INSERT INTO gateway_analytics_key_revisions_rebuilt
                 SELECT k.key_id, k.revision,
                     (SELECT MIN(p.first_pending_revision) FROM gateway_analytics_pending_keys p
                      WHERE p.key_id = k.key_id),
                     k.changed_at
                 FROM gateway_analytics_key_revisions k;
                 DROP TABLE gateway_analytics_key_revisions;
                 ALTER TABLE gateway_analytics_key_revisions_rebuilt
                     RENAME TO gateway_analytics_key_revisions;
                 PRAGMA legacy_alter_table = OFF;
                 CREATE INDEX gateway_analytics_revision_order
                     ON gateway_analytics_revisions (revision, request_id);
                 CREATE INDEX gateway_analytics_pending ON gateway_analytics_revisions
                     (first_pending_revision, request_id) WHERE first_pending_revision IS NOT NULL;
                 CREATE INDEX gateway_analytics_key_pending ON gateway_analytics_key_revisions
                     (first_pending_revision, key_id) WHERE first_pending_revision IS NOT NULL;
                 DROP TABLE gateway_analytics_pending_keys;
                 DROP TABLE gateway_analytics_pending_requests;
                 DROP TABLE gateway_analytics_generations;",
            )
            .await?;
        connection
            .execute_unprepared(&create_capture_triggers(
                &coalescing_mark_request,
                COALESCING_MARK_KEY,
            ))
            .await?;
        Ok(())
    }
}

fn coalescing_mark_request(request: &str) -> String {
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

const COALESCING_MARK_KEY: &str = "
UPDATE gateway_analytics_clock SET revision = revision + 1 WHERE id = 1;
INSERT INTO gateway_analytics_key_revisions
    (key_id, revision, first_pending_revision, changed_at)
SELECT NEW.id, revision, revision, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
FROM gateway_analytics_clock WHERE id = 1
ON CONFLICT(key_id) DO UPDATE SET
    revision = excluded.revision,
    first_pending_revision = COALESCE(gateway_analytics_key_revisions.first_pending_revision, excluded.revision),
    changed_at = excluded.changed_at;
";

#[cfg(test)]
mod tests {
    use crate::migration::Migrator;
    use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement};
    use sea_orm_migration::MigratorTrait;

    /// Everything except this migration, so the queues meet a source that has
    /// already been capturing under the previous schema.
    async fn previous_schema() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let before = u32::try_from(Migrator::migrations().len() - 1).unwrap();
        Migrator::up(&db, Some(before)).await.unwrap();
        db.execute_unprepared(
            "UPDATE gateway_analytics_clock SET active_generation = 'live' WHERE id = 1;
             INSERT INTO users(id,email_normalized,email_display,role,status,auth_version,created_at,updated_at)
             VALUES('u','u@example.com','u@example.com','user','active',0,'2026-01-01','2026-01-01');
             INSERT INTO gateway_keys(id,user_id,name,allowed_providers,created_at)
             VALUES('k','u','agent','[]','2026-01-01T00:00:00.000Z');
             INSERT INTO gateway_requests
             (id,request_id,provider,protocol,method,endpoint,started_at,request_bytes)
             VALUES('owed','external','p','anthropic_messages','POST','/messages','2026-01-01T00:00:00.000Z',0),
                   ('done','external','p','anthropic_messages','POST','/messages','2026-01-01T00:00:00.000Z',0);
             UPDATE gateway_analytics_revisions SET first_pending_revision = NULL WHERE request_id = 'done';
             UPDATE gateway_analytics_key_revisions SET first_pending_revision = NULL;",
        )
        .await
        .unwrap();
        db
    }

    async fn rows(db: &DatabaseConnection, query: &str) -> Vec<String> {
        db.query_all_raw(Statement::from_string(DbBackend::Sqlite, query))
            .await
            .unwrap()
            .iter()
            .map(|row| row.try_get::<String>("", "value").unwrap())
            .collect()
    }

    #[tokio::test]
    async fn outstanding_work_moves_into_the_active_generation_and_settled_work_does_not() {
        let db = previous_schema().await;
        let owed: i64 = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT first_pending_revision value FROM gateway_analytics_revisions
                 WHERE request_id = 'owed'",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "value")
            .unwrap();
        Migrator::up(&db, None).await.unwrap();
        assert_eq!(
            rows(
                &db,
                "SELECT generation value FROM gateway_analytics_generations"
            )
            .await,
            ["live"]
        );
        assert_eq!(
            rows(
                &db,
                "SELECT request_id || ':' || first_pending_revision value
                 FROM gateway_analytics_pending_requests WHERE generation = 'live'"
            )
            .await,
            [format!("owed:{owed}")]
        );
        assert!(
            rows(
                &db,
                "SELECT key_id value FROM gateway_analytics_pending_keys"
            )
            .await
            .is_empty()
        );
    }

    #[tokio::test]
    async fn a_source_with_no_active_generation_registers_none_and_queues_nothing() {
        let db = previous_schema().await;
        db.execute_unprepared("UPDATE gateway_analytics_clock SET active_generation = NULL")
            .await
            .unwrap();
        Migrator::up(&db, None).await.unwrap();
        assert!(
            rows(
                &db,
                "SELECT generation value FROM gateway_analytics_generations"
            )
            .await
            .is_empty()
        );
        db.execute_unprepared("UPDATE gateway_requests SET http_status = 200 WHERE id = 'owed'")
            .await
            .unwrap();
        assert!(
            rows(
                &db,
                "SELECT request_id value FROM gateway_analytics_pending_requests"
            )
            .await
            .is_empty()
        );
    }

    #[tokio::test]
    async fn reverting_restores_the_coalescing_columns_and_keeps_capture_working() {
        let db = previous_schema().await;
        Migrator::up(&db, None).await.unwrap();
        Migrator::down(&db, Some(1)).await.unwrap();
        assert_eq!(
            rows(
                &db,
                "SELECT request_id || ':' || COALESCE(CAST(first_pending_revision AS TEXT), 'none') value
                 FROM gateway_analytics_revisions ORDER BY request_id"
            )
            .await,
            ["done:none", "owed:2"]
        );
        db.execute_unprepared("UPDATE gateway_requests SET http_status = 200 WHERE id = 'done'")
            .await
            .unwrap();
        assert_eq!(
            rows(
                &db,
                "SELECT COALESCE(CAST(first_pending_revision AS TEXT), 'none') value
                 FROM gateway_analytics_revisions WHERE request_id = 'done'"
            )
            .await
            .len(),
            1
        );
    }
}
