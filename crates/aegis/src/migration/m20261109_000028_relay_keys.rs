use sea_orm::ConnectionTrait;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for column in ["oauth_client_id", "oauth_issuer"] {
            if !manager.has_column("gateway_keys", column).await? {
                manager
                    .get_connection()
                    .execute_unprepared(&format!(
                        "ALTER TABLE gateway_keys ADD COLUMN {column} TEXT"
                    ))
                    .await?;
            }
        }
        manager.get_connection().execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS gateway_relay_client ON gateway_keys(user_id, oauth_client_id, oauth_issuer) WHERE revoked_at IS NULL AND oauth_client_id IS NOT NULL"
        ).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP INDEX gateway_relay_client")
            .await?;
        for column in ["oauth_client_id", "oauth_issuer"] {
            manager
                .get_connection()
                .execute_unprepared(&format!("ALTER TABLE gateway_keys DROP COLUMN {column}"))
                .await?;
        }
        Ok(())
    }
}
