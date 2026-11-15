use sea_orm::ConnectionTrait;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Partial index: existing rows have no session, so building it is cheap on large tables.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        if !manager.has_column("gateway_requests", "session_id").await? {
            db.execute_unprepared("ALTER TABLE gateway_requests ADD COLUMN session_id TEXT")
                .await?;
        }
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS ix_gateway_requests_session ON gateway_requests(session_id) WHERE session_id IS NOT NULL",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS ix_gateway_requests_session")
            .await?;
        db.execute_unprepared("ALTER TABLE gateway_requests DROP COLUMN session_id")
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use sea_orm::{Database, DbBackend, Statement};
    use sea_orm_migration::MigratorTrait;

    #[tokio::test]
    async fn existing_requests_keep_their_history_and_have_no_client_session() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let this = Migration.name();
        let before = Migrator::migrations()
            .iter()
            .position(|migration| migration.name() == this)
            .expect("this migration is registered");
        Migrator::up(&db, Some(before as u32)).await.unwrap();
        db.execute_unprepared(
            "INSERT INTO gateway_requests(id,request_id,provider,protocol,method,endpoint,started_at,request_bytes) VALUES('old','old','claude','anthropic_messages','POST','/v1/messages','2026-01-01T00:00:00Z',7)",
        )
        .await
        .unwrap();

        Migrator::up(&db, None).await.unwrap();

        let row = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT session_id, request_bytes FROM gateway_requests WHERE id = 'old'",
            ))
            .await
            .unwrap()
            .unwrap();
        assert!(
            row.try_get::<Option<String>>("", "session_id")
                .unwrap()
                .is_none()
        );
        assert_eq!(row.try_get::<i64>("", "request_bytes").unwrap(), 7);
        let index = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'ix_gateway_requests_session'",
            ))
            .await
            .unwrap()
            .expect("the session column is indexed");
        let sql = index.try_get::<String>("", "sql").unwrap();
        assert!(
            sql.ends_with("gateway_requests(session_id) WHERE session_id IS NOT NULL"),
            "{sql}"
        );
        let plan = db
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                "EXPLAIN QUERY PLAN SELECT id FROM gateway_requests WHERE session_id = 'x'",
            ))
            .await
            .unwrap();
        assert!(
            plan.iter().any(|row| row
                .try_get::<String>("", "detail")
                .unwrap()
                .contains("ix_gateway_requests_session")),
            "a lookup by client session uses the partial index"
        );
    }
}
