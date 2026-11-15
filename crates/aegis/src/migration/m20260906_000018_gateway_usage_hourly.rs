use sea_orm::ConnectionTrait;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let connection = manager.get_connection();
        connection
            .execute_unprepared(
                "CREATE TABLE IF NOT EXISTS gateway_usage_hourly (
                    user_id TEXT NOT NULL,
                    hour TEXT NOT NULL,
                    provider TEXT NOT NULL,
                    requested_model TEXT NOT NULL,
                    key_id TEXT NOT NULL,
                    requests INTEGER NOT NULL,
                    succeeded INTEGER NOT NULL,
                    failed INTEGER NOT NULL,
                    input_tokens INTEGER NOT NULL,
                    cache_read_tokens INTEGER NOT NULL,
                    cache_write_tokens INTEGER NOT NULL,
                    output_tokens INTEGER NOT NULL,
                    reasoning_tokens INTEGER NOT NULL,
                    cost_nanodollars INTEGER NOT NULL,
                    unpriced INTEGER NOT NULL,
                    PRIMARY KEY (user_id, hour, provider, requested_model, key_id)
                ) WITHOUT ROWID",
            )
            .await?;
        if !manager
            .has_column("gateway_requests", "aggregated_at")
            .await?
        {
            connection
                .execute_unprepared("ALTER TABLE gateway_requests ADD COLUMN aggregated_at TEXT")
                .await?;
        }
        connection
            .execute_unprepared(
                "CREATE INDEX IF NOT EXISTS ix_gateway_requests_unaggregated \
                 ON gateway_requests (id) WHERE aggregated_at IS NULL",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let connection = manager.get_connection();
        connection
            .execute_unprepared("DROP INDEX IF EXISTS ix_gateway_requests_unaggregated")
            .await?;
        connection
            .execute_unprepared("ALTER TABLE gateway_requests DROP COLUMN aggregated_at")
            .await?;
        connection
            .execute_unprepared("DROP TABLE IF EXISTS gateway_usage_hourly")
            .await?;
        Ok(())
    }
}
