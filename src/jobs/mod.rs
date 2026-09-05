mod payload_facts_backfill;
mod payload_resplit;
mod request_metrics_rollup;
mod requested_model_backfill;

use crate::telemetry::timestamp;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement};
use std::time::Instant;

/// Each job reads what the one before it wrote, so the chain stops at the
/// first failure instead of recording a pass over half-converted rows.
pub fn spawn(database: DatabaseConnection) {
    tokio::spawn(async move {
        if !run(&database, payload_resplit::NAME, payload_resplit::run).await {
            return;
        }
        if !run(
            &database,
            requested_model_backfill::NAME,
            requested_model_backfill::run,
        )
        .await
        {
            return;
        }
        if let Err(error) =
            crate::pricing::backfill_costs(&database, &crate::pricing::active_map()).await
        {
            tracing::error!(%error, "failed to price requests after the model backfill");
            return;
        }
        if !run(
            &database,
            payload_facts_backfill::NAME,
            payload_facts_backfill::run,
        )
        .await
        {
            return;
        }
        run(
            &database,
            request_metrics_rollup::NAME,
            request_metrics_rollup::run,
        )
        .await;
    });
}

async fn run(
    database: &DatabaseConnection,
    name: &str,
    job: impl AsyncFnOnce(&DatabaseConnection) -> Result<u64, DbErr>,
) -> bool {
    match completed(database, name).await {
        Ok(true) => return true,
        Ok(false) => {}
        Err(error) => {
            tracing::error!(%error, name, "failed to read the background job state");
            return false;
        }
    }
    tracing::info!(name, "background job started");
    let started = Instant::now();
    let processed = match job(database).await {
        Ok(processed) => processed,
        Err(error) => {
            tracing::error!(%error, name, "background job failed");
            return false;
        }
    };
    if let Err(error) = record(database, name).await {
        tracing::error!(%error, name, "failed to record the completed background job");
        return false;
    }
    tracing::info!(
        name,
        processed,
        elapsed_ms = started.elapsed().as_millis(),
        "background job finished"
    );
    true
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
    crate::db::writer(database)
        .await?
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

    #[tokio::test]
    async fn a_failed_job_is_not_recorded_and_reports_the_failure() {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&database, None).await.unwrap();

        let failed = run(&database, "failing", async |_| {
            Err(DbErr::Custom("boom".to_owned()))
        })
        .await;
        assert!(!failed);
        assert!(!completed(&database, "failing").await.unwrap());

        assert!(run(&database, "passing", async |_| Ok(1)).await);
        assert!(completed(&database, "passing").await.unwrap());
        assert!(
            run(&database, "passing", async |_| Err(DbErr::Custom(
                "boom".to_owned()
            )))
            .await,
            "a recorded job is skipped without running"
        );
    }
}
