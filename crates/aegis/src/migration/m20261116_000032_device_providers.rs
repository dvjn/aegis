use sea_orm::ConnectionTrait;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if !manager
            .has_column("oauth_device_authorizations", "providers")
            .await?
        {
            manager
                .get_connection()
                .execute_unprepared(
                    "ALTER TABLE oauth_device_authorizations ADD COLUMN providers TEXT",
                )
                .await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE oauth_device_authorizations DROP COLUMN providers")
            .await?;
        Ok(())
    }
}
