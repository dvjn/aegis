use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(PolicyEvaluations::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(PolicyEvaluations::Id)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(PolicyEvaluations::RequestId)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(PolicyEvaluations::Policy)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(PolicyEvaluations::PolicyVersion)
                            .integer()
                            .not_null(),
                    )
                    .col(ColumnDef::new(PolicyEvaluations::Stage).string().not_null())
                    .col(
                        ColumnDef::new(PolicyEvaluations::Outcome)
                            .string()
                            .not_null(),
                    )
                    .col(ColumnDef::new(PolicyEvaluations::Severity).string())
                    .col(
                        ColumnDef::new(PolicyEvaluations::MatchCount)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(PolicyEvaluations::DurationMicros)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(ColumnDef::new(PolicyEvaluations::Metadata).text())
                    .col(
                        ColumnDef::new(PolicyEvaluations::CreatedAt)
                            .string()
                            .not_null(),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .from(PolicyEvaluations::Table, PolicyEvaluations::RequestId)
                            .to(GatewayRequests::Table, GatewayRequests::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("ix_policy_evaluations_request_id")
                    .table(PolicyEvaluations::Table)
                    .col(PolicyEvaluations::RequestId)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("ix_policy_evaluations_policy_created_at")
                    .table(PolicyEvaluations::Table)
                    .col(PolicyEvaluations::Policy)
                    .col(PolicyEvaluations::CreatedAt)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(PolicyEvaluations::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum PolicyEvaluations {
    Table,
    Id,
    RequestId,
    Policy,
    PolicyVersion,
    Stage,
    Outcome,
    Severity,
    MatchCount,
    DurationMicros,
    Metadata,
    CreatedAt,
}

#[derive(DeriveIden)]
enum GatewayRequests {
    Table,
    Id,
}
