use sea_orm::ConnectionTrait;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// SQLite cannot attach a CHECK through ALTER TABLE ADD COLUMN, so the singleton
// clock is rebuilt. The guard triggers are dropped first: the insert guard would
// otherwise abort the row copy. The capture triggers on the tracked tables still
// name the clock while it is dropped, and only legacy_alter_table stops the
// rename from re-parsing them.
// https://sqlite.org/lang_altertable.html#alter_table_rename
const OWNERSHIP_TRIGGERS: &str = "
CREATE TRIGGER analytics_source_id_immutable BEFORE UPDATE OF source_id ON gateway_analytics_clock
WHEN NEW.source_id IS NOT OLD.source_id OR NEW.source_id IS NULL OR NEW.source_id = '' BEGIN SELECT RAISE(ABORT, 'immutable analytics source identity'); END;
CREATE TRIGGER analytics_source_clock_no_delete BEFORE DELETE ON gateway_analytics_clock
WHEN OLD.id = 1 BEGIN SELECT RAISE(ABORT, 'analytics source clock cannot be deleted'); END;
CREATE TRIGGER analytics_source_clock_no_insert BEFORE INSERT ON gateway_analytics_clock
WHEN NEW.id = 1 BEGIN SELECT RAISE(ABORT, 'analytics source clock cannot be replaced'); END;
";

const DROP_OWNERSHIP_TRIGGERS: &str = "
DROP TRIGGER analytics_source_clock_no_insert;
DROP TRIGGER analytics_source_clock_no_delete;
DROP TRIGGER analytics_source_id_immutable;
";

fn rebuild(columns: &str, select: &str) -> String {
    format!(
        "{DROP_OWNERSHIP_TRIGGERS}
         CREATE TABLE gateway_analytics_clock_rebuilt (
             id INTEGER PRIMARY KEY CHECK (id = 1),
             revision INTEGER NOT NULL CHECK (typeof(revision) = 'integer' AND revision >= 0),
             active_generation TEXT,
             source_id TEXT{columns}
         );
         INSERT INTO gateway_analytics_clock_rebuilt
         SELECT id, revision, active_generation, source_id{select} FROM gateway_analytics_clock;
         DROP TABLE gateway_analytics_clock;
         PRAGMA legacy_alter_table = ON;
         ALTER TABLE gateway_analytics_clock_rebuilt RENAME TO gateway_analytics_clock;
         PRAGMA legacy_alter_table = OFF;
         {OWNERSHIP_TRIGGERS}"
    )
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(&rebuild(
                ",
             epoch INTEGER NOT NULL DEFAULT 0 CHECK (typeof(epoch) = 'integer' AND epoch >= 0)",
                ", 0",
            ))
            .await?;
        Ok(())
    }
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(&rebuild("", ""))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};

    #[tokio::test]
    async fn rebuilt_clock_keeps_its_singleton_row_guards_and_rejects_a_negative_epoch() {
        let fixture = crate::db::tests::FileDatabase::new().await;
        let database = &fixture.database;
        let row = database
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT id, epoch, source_id FROM gateway_analytics_clock",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get::<i64>("", "id").unwrap(), 1);
        assert_eq!(row.try_get::<i64>("", "epoch").unwrap(), 0);
        assert!(!row.try_get::<String>("", "source_id").unwrap().is_empty());
        for statement in [
            "UPDATE gateway_analytics_clock SET epoch = -1 WHERE id = 1",
            "UPDATE gateway_analytics_clock SET epoch = 'one' WHERE id = 1",
            "UPDATE gateway_analytics_clock SET source_id = '' WHERE id = 1",
            "DELETE FROM gateway_analytics_clock WHERE id = 1",
            "INSERT INTO gateway_analytics_clock (id, revision, epoch) VALUES (1, 0, 0)",
        ] {
            assert!(
                database.execute_unprepared(statement).await.is_err(),
                "{statement}"
            );
        }
        database
            .execute_unprepared("UPDATE gateway_analytics_clock SET epoch = epoch + 1 WHERE id = 1")
            .await
            .unwrap();
    }
}
