use sea_orm::ConnectionTrait;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Only the singleton clock is updated. Historical facts are not scanned.
        manager.get_connection().execute_unprepared(
            "ALTER TABLE gateway_analytics_clock ADD COLUMN source_id TEXT;
             UPDATE gateway_analytics_clock SET source_id = lower(hex(randomblob(16))) WHERE id = 1;
             CREATE TRIGGER analytics_source_id_immutable BEFORE UPDATE OF source_id ON gateway_analytics_clock
             WHEN NEW.source_id IS NOT OLD.source_id OR NEW.source_id IS NULL OR NEW.source_id = '' BEGIN SELECT RAISE(ABORT, 'immutable analytics source identity'); END;
             CREATE TRIGGER analytics_source_clock_no_delete BEFORE DELETE ON gateway_analytics_clock
             WHEN OLD.id = 1 BEGIN SELECT RAISE(ABORT, 'analytics source clock cannot be deleted'); END;
             CREATE TRIGGER analytics_source_clock_no_insert BEFORE INSERT ON gateway_analytics_clock
             WHEN NEW.id = 1 BEGIN SELECT RAISE(ABORT, 'analytics source clock cannot be replaced'); END;
             CREATE INDEX gateway_analytics_revision_order ON gateway_analytics_revisions(revision, request_id);"
        ).await?;
        // Oldest pending time is intentionally unavailable: changed_at is not
        // first_pending_at, and initializing it would require a history scan.
        Ok(())
    }
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DROP INDEX gateway_analytics_revision_order;
             DROP TRIGGER analytics_source_clock_no_insert;
             DROP TRIGGER analytics_source_clock_no_delete;
             DROP TRIGGER analytics_source_id_immutable;
             ALTER TABLE gateway_analytics_clock DROP COLUMN source_id;",
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};

    #[tokio::test]
    async fn source_identity_rejects_delete_replace_null_and_empty() {
        let fixture = crate::db::tests::FileDatabase::new().await;
        let database = &fixture.database;
        let read = || {
            Statement::from_string(
                DbBackend::Sqlite,
                "SELECT source_id,active_generation FROM gateway_analytics_clock WHERE id=1",
            )
        };
        let source_id: String = database
            .query_one_raw(read())
            .await
            .unwrap()
            .unwrap()
            .try_get("", "source_id")
            .unwrap();
        for statement in [
            "UPDATE gateway_analytics_clock SET source_id=NULL WHERE id=1",
            "UPDATE gateway_analytics_clock SET source_id='' WHERE id=1",
            "DELETE FROM gateway_analytics_clock WHERE id=1",
            "INSERT OR REPLACE INTO gateway_analytics_clock(id,revision,active_generation,source_id) VALUES(1,0,NULL,'replacement')",
        ] {
            assert!(
                database.execute_unprepared(statement).await.is_err(),
                "{statement}"
            );
        }
        database
            .execute_unprepared("UPDATE gateway_analytics_clock SET revision=revision,active_generation='test' WHERE id=1")
            .await
            .unwrap();
        let row = database.query_one_raw(read()).await.unwrap().unwrap();
        assert_eq!(row.try_get::<String>("", "source_id").unwrap(), source_id);
        assert_eq!(
            row.try_get::<Option<String>>("", "active_generation")
                .unwrap()
                .as_deref(),
            Some("test")
        );
    }
}
