mod payload_resplit;

use crate::telemetry::timestamp;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement};
use std::time::Instant;

pub fn spawn(database: DatabaseConnection) {
    tokio::spawn(async move { run(&database).await });
}

async fn run(database: &DatabaseConnection) {
    let name = payload_resplit::NAME;
    match completed(database, name).await {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            tracing::error!(%error, name, "failed to read the background job state");
            return;
        }
    }
    tracing::info!(name, "background job started");
    let started = Instant::now();
    let converted = match payload_resplit::run(database).await {
        Ok(converted) => converted,
        Err(error) => {
            tracing::error!(%error, name, "background job failed");
            return;
        }
    };
    if let Err(error) = record(database, name).await {
        tracing::error!(%error, name, "failed to record the completed background job");
        return;
    }
    tracing::info!(
        name,
        converted,
        elapsed_ms = started.elapsed().as_millis(),
        "background job finished"
    );
}

async fn completed(database: &DatabaseConnection, name: &str) -> Result<bool, DbErr> {
    let row = database
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT completed_at FROM background_jobs WHERE name = ?",
            [name.into()],
        ))
        .await?;
    Ok(row.is_some())
}

async fn record(database: &DatabaseConnection, name: &str) -> Result<(), DbErr> {
    database
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT OR IGNORE INTO background_jobs (name, completed_at) VALUES (?, ?)",
            [name.into(), timestamp().into()],
        ))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use sea_orm::Database;
    use sea_orm_migration::MigratorTrait;

    #[tokio::test]
    async fn a_job_counts_as_done_only_once_it_is_recorded() {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&database, None).await.unwrap();

        assert!(!completed(&database, "example").await.unwrap());
        record(&database, "example").await.unwrap();
        assert!(completed(&database, "example").await.unwrap());
        assert!(!completed(&database, "other").await.unwrap());
    }
}
