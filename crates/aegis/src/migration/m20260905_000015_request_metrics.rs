use sea_orm::ConnectionTrait;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE IF NOT EXISTS gateway_request_metrics (
                    request_id TEXT PRIMARY KEY NOT NULL,
                    tool_definition_bytes INTEGER NOT NULL DEFAULT 0,
                    system_bytes INTEGER NOT NULL DEFAULT 0,
                    user_text_bytes INTEGER NOT NULL DEFAULT 0,
                    assistant_text_bytes INTEGER NOT NULL DEFAULT 0,
                    thinking_bytes INTEGER NOT NULL DEFAULT 0,
                    tool_use_bytes INTEGER NOT NULL DEFAULT 0,
                    tool_result_bytes INTEGER NOT NULL DEFAULT 0,
                    other_bytes INTEGER NOT NULL DEFAULT 0,
                    total_bytes INTEGER NOT NULL DEFAULT 0,
                    tools_offered INTEGER NOT NULL DEFAULT 0,
                    tools_invoked INTEGER NOT NULL DEFAULT 0,
                    tool_result_errors INTEGER NOT NULL DEFAULT 0,
                    cache_breakpoints INTEGER NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL,
                    FOREIGN KEY (request_id) REFERENCES gateway_requests(id) ON DELETE CASCADE
                )",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS gateway_request_metrics")
            .await?;
        Ok(())
    }
}
