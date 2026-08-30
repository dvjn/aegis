use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(GatewayRequests::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(GatewayRequests::Id)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(GatewayRequests::RequestId)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(GatewayRequests::Provider)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(GatewayRequests::Protocol)
                            .string()
                            .not_null(),
                    )
                    .col(ColumnDef::new(GatewayRequests::Method).string().not_null())
                    .col(
                        ColumnDef::new(GatewayRequests::Endpoint)
                            .string()
                            .not_null(),
                    )
                    .col(ColumnDef::new(GatewayRequests::RequestedModel).string())
                    .col(
                        ColumnDef::new(GatewayRequests::StartedAt)
                            .string()
                            .not_null(),
                    )
                    .col(ColumnDef::new(GatewayRequests::FirstByteAt).string())
                    .col(ColumnDef::new(GatewayRequests::CompletedAt).string())
                    .col(ColumnDef::new(GatewayRequests::HttpStatus).integer())
                    .col(
                        ColumnDef::new(GatewayRequests::RequestBytes)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(GatewayRequests::ResponseBytes)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(GatewayRequests::ClientDisconnected)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .col(ColumnDef::new(GatewayRequests::ErrorMessage).text())
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("ix_gateway_requests_started_at")
                    .table(GatewayRequests::Table)
                    .col(GatewayRequests::StartedAt)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("ix_gateway_requests_provider")
                    .table(GatewayRequests::Table)
                    .col(GatewayRequests::Provider)
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(GatewayPayloads::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(GatewayPayloads::RequestId)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(GatewayPayloads::RequestBody).binary())
                    .col(ColumnDef::new(GatewayPayloads::ResponseBody).binary())
                    .col(
                        ColumnDef::new(GatewayPayloads::ResponseTruncated)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_gateway_payloads_request")
                            .from(GatewayPayloads::Table, GatewayPayloads::RequestId)
                            .to(GatewayRequests::Table, GatewayRequests::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(GatewayUsage::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(GatewayUsage::RequestId)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(GatewayUsage::InputTokens).big_integer())
                    .col(ColumnDef::new(GatewayUsage::OutputTokens).big_integer())
                    .col(ColumnDef::new(GatewayUsage::CacheReadTokens).big_integer())
                    .col(ColumnDef::new(GatewayUsage::CacheWriteTokens).big_integer())
                    .col(ColumnDef::new(GatewayUsage::ReasoningTokens).big_integer())
                    .col(ColumnDef::new(GatewayUsage::RawUsageJson).text())
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_gateway_usage_request")
                            .from(GatewayUsage::Table, GatewayUsage::RequestId)
                            .to(GatewayRequests::Table, GatewayRequests::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(GatewayUsage::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(GatewayPayloads::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(GatewayRequests::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum GatewayRequests {
    Table,
    Id,
    RequestId,
    Provider,
    Protocol,
    Method,
    Endpoint,
    RequestedModel,
    StartedAt,
    FirstByteAt,
    CompletedAt,
    HttpStatus,
    RequestBytes,
    ResponseBytes,
    ClientDisconnected,
    ErrorMessage,
}

#[derive(DeriveIden)]
enum GatewayPayloads {
    Table,
    RequestId,
    RequestBody,
    ResponseBody,
    ResponseTruncated,
}

#[derive(DeriveIden)]
enum GatewayUsage {
    Table,
    RequestId,
    InputTokens,
    OutputTokens,
    CacheReadTokens,
    CacheWriteTokens,
    ReasoningTokens,
    RawUsageJson,
}
