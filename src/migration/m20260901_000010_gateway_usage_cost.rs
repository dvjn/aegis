use crate::{
    pricing::{Cost, cost},
    providers::Usage,
};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(GatewayUsage::Table)
                    .add_column(ColumnDef::new(GatewayUsage::CostNanodollars).big_integer())
                    .to_owned(),
            )
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(GatewayUsage::Table)
                    .add_column(ColumnDef::new(GatewayUsage::CostSource).string())
                    .to_owned(),
            )
            .await?;

        let database = manager.get_connection();
        let rows = database
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT u.request_id, r.requested_model, u.input_tokens, u.output_tokens, u.cache_read_tokens, u.cache_write_tokens \
                 FROM gateway_usage u JOIN gateway_requests r ON r.id = u.request_id",
            ))
            .await?;

        for row in rows {
            let request_id: String = row.try_get("", "request_id")?;
            let usage = Usage {
                input_tokens: row.try_get("", "input_tokens")?,
                output_tokens: row.try_get("", "output_tokens")?,
                cache_read_tokens: row.try_get("", "cache_read_tokens")?,
                cache_write_tokens: row.try_get("", "cache_write_tokens")?,
                reasoning_tokens: None,
                raw_json: None,
            };
            let model: Option<String> = row.try_get("", "requested_model")?;
            let Cost {
                nanodollars,
                source,
            } = cost(model.as_deref(), &usage);
            database
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    "UPDATE gateway_usage SET cost_nanodollars = ?, cost_source = ? WHERE request_id = ?",
                    [
                        nanodollars.into(),
                        source.as_str().into(),
                        request_id.into(),
                    ],
                ))
                .await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(GatewayUsage::Table)
                    .drop_column(GatewayUsage::CostSource)
                    .to_owned(),
            )
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(GatewayUsage::Table)
                    .drop_column(GatewayUsage::CostNanodollars)
                    .to_owned(),
            )
            .await
    }
}

#[derive(DeriveIden)]
enum GatewayUsage {
    Table,
    CostNanodollars,
    CostSource,
}
