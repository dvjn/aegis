use sea_orm::ConnectionTrait;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager
            .has_column("gateway_requests", "resolved_model")
            .await?
        {
            return Ok(());
        }
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE gateway_requests ADD COLUMN resolved_model TEXT")
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE gateway_requests DROP COLUMN resolved_model")
            .await?;
        Ok(())
    }
}
