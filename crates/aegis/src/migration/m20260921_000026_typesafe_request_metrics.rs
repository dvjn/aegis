use sea_orm::ConnectionTrait;
use sea_orm_migration::prelude::*;

const COLUMNS: [(&str, &str); 3] = [
    ("evidence_bytes", "INTEGER NOT NULL DEFAULT 0"),
    ("judgment_bytes", "INTEGER NOT NULL DEFAULT 0"),
    ("questions_asked", "INTEGER NOT NULL DEFAULT 0"),
];

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let connection = manager.get_connection();
        for (column, definition) in COLUMNS {
            if manager
                .has_column("gateway_request_metrics", column)
                .await?
            {
                continue;
            }
            connection
                .execute_unprepared(&format!(
                    "ALTER TABLE gateway_request_metrics ADD COLUMN {column} {definition}"
                ))
                .await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let connection = manager.get_connection();
        for (column, _) in COLUMNS.iter().rev() {
            connection
                .execute_unprepared(&format!(
                    "ALTER TABLE gateway_request_metrics DROP COLUMN {column}"
                ))
                .await?;
        }
        Ok(())
    }
}
