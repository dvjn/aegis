use sea_orm::ConnectionTrait;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Requests captured before revision tracking existed carry no source facts and
// no queue entry, so nothing ever offers them to a projector. Enumerating them
// is bounded work that outlives a process, which needs a durable place to say
// how far it got.
//
// The frame is fixed when the checkpoint is created: ceiling_request_id is the
// greatest request ID present at that instant, so every request that existed
// then is at or below it, and every request created afterwards is covered by
// the capture triggers instead. cursor_request_id is the last ID enumeration
// examined, advanced in the same transaction that installs that page's facts.
// Resuming therefore neither repeats committed work nor skips a request.
//
// A request whose payloads are gone can never yield facts. It is recorded as a
// gap rather than installed with empty ones, which would assert that the
// request used no tools. Enumeration can finish with gaps outstanding; the
// baseline only counts as covered once an operator accepts them.
const SCHEMA: &str = "
CREATE TABLE gateway_analytics_backfill (
    generation TEXT PRIMARY KEY NOT NULL
        REFERENCES gateway_analytics_generations (generation) ON DELETE CASCADE,
    ceiling_request_id TEXT NOT NULL,
    cursor_request_id TEXT NOT NULL,
    started_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    visited INTEGER NOT NULL CHECK (typeof(visited) = 'integer' AND visited >= 0),
    installed INTEGER NOT NULL CHECK (typeof(installed) = 'integer' AND installed >= 0),
    live INTEGER NOT NULL CHECK (typeof(live) = 'integer' AND live >= 0),
    unavailable INTEGER NOT NULL CHECK (typeof(unavailable) = 'integer' AND unavailable >= 0),
    completed_at TEXT,
    gaps_accepted INTEGER NOT NULL CHECK (gaps_accepted IN (0, 1))
);
CREATE TABLE gateway_analytics_backfill_gaps (
    generation TEXT NOT NULL
        REFERENCES gateway_analytics_backfill (generation) ON DELETE CASCADE,
    request_id TEXT NOT NULL,
    reason TEXT NOT NULL,
    detected_at TEXT NOT NULL,
    PRIMARY KEY (generation, request_id)
) WITHOUT ROWID;
";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(SCHEMA).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DROP TABLE gateway_analytics_backfill_gaps;
                 DROP TABLE gateway_analytics_backfill;",
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::migration::Migrator;
    use sea_orm::{ConnectionTrait, Database, DbBackend, Statement};
    use sea_orm_migration::MigratorTrait;

    #[tokio::test]
    async fn retiring_a_generation_takes_its_checkpoint_and_gaps_with_it() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        db.execute_unprepared(
            "PRAGMA foreign_keys = ON;
             INSERT INTO gateway_analytics_generations VALUES ('g', 0, '2026-01-01T00:00:00.000Z');
             INSERT INTO gateway_analytics_backfill
             VALUES ('g', 'z', '', '2026-01-01', '2026-01-01', 1, 1, 0, 1, NULL, 0);
             INSERT INTO gateway_analytics_backfill_gaps
             VALUES ('g', 'r', 'payloads unavailable', '2026-01-01');
             DELETE FROM gateway_analytics_generations WHERE generation = 'g';",
        )
        .await
        .unwrap();
        let remaining = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT (SELECT COUNT(*) FROM gateway_analytics_backfill)
                      + (SELECT COUNT(*) FROM gateway_analytics_backfill_gaps) n",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(remaining.try_get::<i64>("", "n").unwrap(), 0);
    }

    #[tokio::test]
    async fn reverting_removes_the_checkpoint_tables() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        Migrator::down(&db, Some(1)).await.unwrap();
        assert!(
            db.execute_unprepared("SELECT 1 FROM gateway_analytics_backfill")
                .await
                .is_err()
        );
    }
}
