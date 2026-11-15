use sea_orm::ConnectionTrait;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// The client and time index serves every lookup the client index did, so it replaces it.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS ix_gateway_requests_client_started_at ON gateway_requests(client_id, started_at)",
        )
        .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS ix_gateway_requests_client")
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS ix_gateway_requests_client ON gateway_requests(client_id)",
        )
        .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS ix_gateway_requests_client_started_at")
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

    async fn indexes(db: &impl ConnectionTrait) -> Vec<String> {
        db.query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'gateway_requests' AND name LIKE 'ix_gateway_requests_client%' ORDER BY name",
        ))
        .await
        .unwrap()
        .iter()
        .map(|row| row.try_get::<String>("", "name").unwrap())
        .collect()
    }

    #[tokio::test]
    async fn the_client_index_gains_the_start_time_and_reverts() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let this = Migration.name();
        let before = Migrator::migrations()
            .iter()
            .position(|migration| migration.name() == this)
            .expect("this migration is registered");
        Migrator::up(&db, Some(before as u32)).await.unwrap();
        assert_eq!(indexes(&db).await, ["ix_gateway_requests_client"]);

        Migrator::up(&db, Some(1)).await.unwrap();
        assert_eq!(
            indexes(&db).await,
            ["ix_gateway_requests_client_started_at"]
        );

        Migrator::down(&db, Some(1)).await.unwrap();
        assert_eq!(indexes(&db).await, ["ix_gateway_requests_client"]);
    }
}
