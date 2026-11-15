use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(ModelPrices::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(ModelPrices::Model)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(ModelPrices::InputPerToken)
                            .double()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(ModelPrices::OutputPerToken)
                            .double()
                            .not_null(),
                    )
                    .col(ColumnDef::new(ModelPrices::CacheReadPerToken).double())
                    .col(ColumnDef::new(ModelPrices::CacheWritePerToken).double())
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(ModelPriceSnapshots::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(ModelPriceSnapshots::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(ModelPriceSnapshots::FetchedAt)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(ModelPriceSnapshots::SourceUrl)
                            .string()
                            .not_null(),
                    )
                    .col(ColumnDef::new(ModelPriceSnapshots::Etag).string())
                    .col(
                        ColumnDef::new(ModelPriceSnapshots::Sha256)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(ModelPriceSnapshots::ModelCount)
                            .integer()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(ModelPriceSnapshots::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(ModelPrices::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum ModelPrices {
    Table,
    Model,
    InputPerToken,
    OutputPerToken,
    CacheReadPerToken,
    CacheWritePerToken,
}

#[derive(DeriveIden)]
enum ModelPriceSnapshots {
    Table,
    Id,
    FetchedAt,
    SourceUrl,
    Etag,
    Sha256,
    ModelCount,
}
