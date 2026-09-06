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
                "CREATE TABLE IF NOT EXISTS gateway_tool_usage_hourly (
                    user_id TEXT NOT NULL,
                    hour TEXT NOT NULL,
                    block_type TEXT NOT NULL,
                    label TEXT NOT NULL,
                    skill TEXT NOT NULL,
                    calls INTEGER NOT NULL,
                    bytes REAL NOT NULL,
                    cost_nanodollars REAL NOT NULL,
                    PRIMARY KEY (user_id, hour, block_type, label, skill)
                ) WITHOUT ROWID",
            )
            .await?;
        connection
            .execute_unprepared(
                "CREATE TABLE IF NOT EXISTS gateway_tool_calls_seen (
                    user_id TEXT NOT NULL,
                    block_type TEXT NOT NULL,
                    call_id TEXT NOT NULL,
                    PRIMARY KEY (user_id, block_type, call_id)
                ) WITHOUT ROWID",
            )
            .await?;
        if !manager
            .has_column("gateway_requests", "tools_aggregated_at")
            .await?
        {
            connection
                .execute_unprepared(
                    "ALTER TABLE gateway_requests ADD COLUMN tools_aggregated_at TEXT",
                )
                .await?;
        }
        connection
            .execute_unprepared(
                "CREATE INDEX IF NOT EXISTS ix_gateway_requests_tools_unaggregated \
                 ON gateway_requests (id) WHERE tools_aggregated_at IS NULL",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let connection = manager.get_connection();
        connection
            .execute_unprepared("DROP INDEX IF EXISTS ix_gateway_requests_tools_unaggregated")
            .await?;
        connection
            .execute_unprepared("ALTER TABLE gateway_requests DROP COLUMN tools_aggregated_at")
            .await?;
        connection
            .execute_unprepared("DROP TABLE IF EXISTS gateway_tool_calls_seen")
            .await?;
        connection
            .execute_unprepared("DROP TABLE IF EXISTS gateway_tool_usage_hourly")
            .await?;
        Ok(())
    }
}
