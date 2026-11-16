use sea_orm::ConnectionTrait;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if !manager.has_column("gateway_requests", "launch_id").await? {
            manager
                .get_connection()
                .execute_unprepared("ALTER TABLE gateway_requests ADD COLUMN launch_id TEXT")
                .await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE gateway_requests DROP COLUMN launch_id")
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
    async fn existing_requests_keep_their_history_and_have_no_launch() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let this = Migration.name();
        let before = Migrator::migrations()
            .iter()
            .position(|migration| migration.name() == this)
            .expect("this migration is registered");
        Migrator::up(&db, Some(before as u32)).await.unwrap();
        db.execute_unprepared(
            "INSERT INTO gateway_requests(id,request_id,provider,protocol,method,endpoint,started_at,request_bytes,session_id) VALUES('old','old','claude','anthropic_messages','POST','/v1/messages','2026-01-01T00:00:00Z',7,'session')",
        )
        .await
        .unwrap();

        Migrator::up(&db, None).await.unwrap();

        let row = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT launch_id, session_id, request_bytes FROM gateway_requests WHERE id = 'old'",
            ))
            .await
            .unwrap()
            .unwrap();
        assert!(
            row.try_get::<Option<String>>("", "launch_id")
                .unwrap()
                .is_none()
        );
        assert_eq!(row.try_get::<String>("", "session_id").unwrap(), "session");
        assert_eq!(row.try_get::<i64>("", "request_bytes").unwrap(), 7);
    }
}
