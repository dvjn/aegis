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
                "CREATE TABLE IF NOT EXISTS gateway_guardrails_hourly (
                    user_id TEXT NOT NULL,
                    hour TEXT NOT NULL,
                    provider TEXT NOT NULL,
                    key_id TEXT NOT NULL,
                    scanned INTEGER NOT NULL,
                    masked INTEGER NOT NULL,
                    matches INTEGER NOT NULL,
                    PRIMARY KEY (user_id, hour, provider, key_id)
                ) WITHOUT ROWID",
            )
            .await?;
        connection
            .execute_unprepared(
                "CREATE TABLE IF NOT EXISTS gateway_guardrail_detectors_hourly (
                    user_id TEXT NOT NULL,
                    hour TEXT NOT NULL,
                    detector TEXT NOT NULL,
                    matches INTEGER NOT NULL,
                    requests INTEGER NOT NULL,
                    PRIMARY KEY (user_id, hour, detector)
                ) WITHOUT ROWID",
            )
            .await?;
        connection
            .execute_unprepared(
                "CREATE TABLE IF NOT EXISTS gateway_guardrail_values_hourly (
                    user_id TEXT NOT NULL,
                    hour TEXT NOT NULL,
                    detector TEXT NOT NULL,
                    placeholder TEXT NOT NULL,
                    PRIMARY KEY (user_id, hour, detector, placeholder)
                ) WITHOUT ROWID",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let connection = manager.get_connection();
        for table in [
            "gateway_guardrail_values_hourly",
            "gateway_guardrail_detectors_hourly",
            "gateway_guardrails_hourly",
        ] {
            connection
                .execute_unprepared(&format!("DROP TABLE IF EXISTS {table}"))
                .await?;
        }
        Ok(())
    }
}
