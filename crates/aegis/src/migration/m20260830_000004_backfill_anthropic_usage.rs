use crate::providers::{Provider, extract_usage};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let database = manager.get_connection();
        let rows = database
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT r.id, p.response_body FROM gateway_requests r JOIN gateway_payloads p ON p.request_id = r.id WHERE r.protocol = 'anthropic_messages' AND p.response_body IS NOT NULL",
            ))
            .await?;

        for row in rows {
            let request_id: String = row.try_get("", "id")?;
            let body: Vec<u8> = row.try_get("", "response_body")?;
            let usage = extract_usage(Provider::Anthropic, &body);
            if usage.input_tokens.is_none() && usage.output_tokens.is_none() {
                continue;
            }
            database
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    "INSERT OR REPLACE INTO gateway_usage (request_id, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens, raw_usage_json) VALUES (?, ?, ?, ?, ?, ?, ?)",
                    [
                        request_id.into(),
                        usage.input_tokens.into(),
                        usage.output_tokens.into(),
                        usage.cache_read_tokens.into(),
                        usage.cache_write_tokens.into(),
                        usage.reasoning_tokens.into(),
                        usage.raw_json.into(),
                    ],
                ))
                .await?;
        }
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
