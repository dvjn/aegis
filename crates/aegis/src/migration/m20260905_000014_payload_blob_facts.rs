use sea_orm::ConnectionTrait;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let database = manager.get_connection();
        database
            .execute_unprepared(
                "CREATE TABLE IF NOT EXISTS gateway_payload_blob_facts (
                    blob_id TEXT NOT NULL,
                    ordinal INTEGER NOT NULL,
                    block_type TEXT NOT NULL CHECK (block_type IN ('tool_definition', 'tool_use', 'tool_result', 'text', 'thinking', 'image', 'reasoning', 'message')),
                    tool_name TEXT,
                    mcp_server TEXT,
                    skill_name TEXT,
                    tool_use_id TEXT,
                    is_error INTEGER,
                    cache_ttl TEXT,
                    PRIMARY KEY (blob_id, ordinal),
                    FOREIGN KEY (blob_id) REFERENCES gateway_payload_blobs(id) ON DELETE CASCADE
                )",
            )
            .await?;
        database
            .execute_unprepared(
                "CREATE INDEX IF NOT EXISTS idx_gateway_payload_blob_facts_tool_name ON gateway_payload_blob_facts(tool_name)",
            )
            .await?;
        database
            .execute_unprepared(
                "CREATE INDEX IF NOT EXISTS idx_gateway_payload_blob_facts_tool_use_id ON gateway_payload_blob_facts(tool_use_id)",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS gateway_payload_blob_facts")
            .await?;
        Ok(())
    }
}
