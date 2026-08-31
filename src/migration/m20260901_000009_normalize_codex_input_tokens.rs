use sea_orm::{ConnectionTrait, DbBackend, Statement};
use sea_orm_migration::prelude::*;
use serde_json::Value;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let database = manager.get_connection();
        let rows = database
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT u.request_id, u.raw_usage_json FROM gateway_usage u JOIN gateway_requests r ON r.id = u.request_id WHERE r.protocol = 'openai_responses' AND u.raw_usage_json IS NOT NULL",
            ))
            .await?;

        for row in rows {
            let request_id: String = row.try_get("", "request_id")?;
            let raw: String = row.try_get("", "raw_usage_json")?;
            // Re-deriving from the verbatim upstream usage object, rather than
            // subtracting from the stored column, is what makes this migration
            // safe to run twice: the source numbers never change, so a second
            // run writes the same value instead of subtracting again.
            let Ok(usage) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            let Some(upstream_input_tokens) = usage.get("input_tokens").and_then(Value::as_i64)
            else {
                continue;
            };
            let cached_tokens = usage
                .get("input_tokens_details")
                .and_then(|details| details.get("cached_tokens"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let uncached_input_tokens = std::cmp::max(upstream_input_tokens - cached_tokens, 0);

            database
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    "UPDATE gateway_usage SET input_tokens = ? WHERE request_id = ?",
                    [uncached_input_tokens.into(), request_id.into()],
                ))
                .await?;
        }
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
