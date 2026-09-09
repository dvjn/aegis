//! Drain pending requests in start order, one hour at a time. Request stamps
//! are durable checkpoints; a restart resumes the first unfinished request.
use crate::{db::begin_immediate, tool_usage_hourly, usage_hourly};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement, TransactionTrait};
use std::time::{Duration, Instant};

const GENERATION: &str = "hourly_buckets_generation_v2";
const BATCH: usize = 32;

/// Invalidate in the same transaction that changes historical prices. The
/// worker discards prepared reads from an older generation before committing.
pub async fn invalidate(database: &impl ConnectionTrait) -> Result<(), DbErr> {
    database
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "DELETE FROM background_jobs WHERE name = ?",
            [GENERATION.into()],
        ))
        .await?;
    Ok(())
}

async fn generation(database: &impl ConnectionTrait) -> Result<Option<String>, DbErr> {
    database
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT completed_at FROM background_jobs WHERE name = ?",
            [GENERATION.into()],
        ))
        .await?
        .map(|r| r.try_get_by_index(0))
        .transpose()
}

async fn initialize(database: &DatabaseConnection) -> Result<(), DbErr> {
    if generation(database).await?.is_some() {
        return Ok(());
    }
    let tx = begin_immediate(database).await?;
    if generation(&tx).await?.is_none() {
        // Also repairs a deployment interrupted after writing only some of the
        // old migration's buckets. No payload scans happen while holding this lock.
        for sql in [
            "DELETE FROM gateway_tool_usage_hourly",
            "DELETE FROM gateway_tool_calls_seen",
            "DELETE FROM gateway_usage_hourly",
            "DELETE FROM gateway_guardrails_hourly",
            "DELETE FROM gateway_guardrail_detectors_hourly",
            "DELETE FROM gateway_guardrail_values_hourly",
            "UPDATE gateway_requests SET aggregated_at = NULL, tools_aggregated_at = NULL",
        ] {
            tx.execute_unprepared(sql).await?;
        }
        tx.execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO background_jobs (name, completed_at) VALUES (?, ?)",
            [
                GENERATION.into(),
                chrono::Utc::now()
                    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                    .into(),
            ],
        ))
        .await?;
        tracing::info!("hourly bucket rebuild initialized");
    }
    tx.commit().await
}

pub async fn run(database: &DatabaseConnection) {
    loop {
        let delay = match batch(database).await {
            Ok(0) => Duration::from_secs(1),
            Ok(_) => Duration::from_millis(100),
            Err(error) => {
                tracing::error!(%error, "hourly bucket batch failed; retrying in five seconds");
                Duration::from_secs(5)
            }
        };
        tokio::time::sleep(delay).await;
    }
}

pub(crate) async fn batch(database: &DatabaseConnection) -> Result<u64, DbErr> {
    initialize(database).await?;
    let rows = database
        .query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            format!(
                "SELECT id, strftime('%Y-%m-%dT%H:00:00.000Z', started_at) hour \
                 FROM gateway_requests WHERE completed_at IS NOT NULL \
                 AND (tools_aggregated_at IS NULL OR aggregated_at IS NULL) \
                 ORDER BY started_at, id LIMIT {BATCH}"
            ),
        ))
        .await?;
    let Some(first) = rows.first() else {
        return Ok(0);
    };
    let hour: String = first.try_get("", "hour")?;
    let started = Instant::now();
    let mut processed = 0;
    for row in rows {
        if row.try_get::<String>("", "hour")? != hour {
            break;
        }
        let id: String = row.try_get("", "id")?;
        if process(database, &id).await? {
            processed += 1;
        }
        // Release the connection and writer permit between every request.
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tracing::info!(%hour, processed, elapsed_ms = started.elapsed().as_millis(),
                   "hourly bucket batch committed");
    Ok(processed)
}

async fn process(database: &DatabaseConnection, id: &str) -> Result<bool, DbErr> {
    let snapshot = database.begin().await?;
    let expected = generation(&snapshot).await?;
    if expected.is_none() {
        snapshot.rollback().await?;
        return Ok(false);
    }
    let prepared = tool_usage_hourly::prepare(&snapshot, id).await?;
    snapshot.commit().await?;
    apply_prepared(database, id, expected, prepared).await
}

async fn apply_prepared(
    database: &DatabaseConnection,
    id: &str,
    expected: Option<String>,
    prepared: tool_usage_hourly::Prepared,
) -> Result<bool, DbErr> {
    let tx = begin_immediate(database).await?;
    if generation(&tx).await? != expected {
        tx.rollback().await?;
        return Ok(false);
    }
    let row = tx.query_one_raw(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT tools_aggregated_at FROM gateway_requests WHERE id = ? AND completed_at IS NOT NULL",
        [id.into()],
    )).await?;
    let Some(row) = row else {
        tx.rollback().await?;
        return Ok(false);
    };
    if row.try_get_by_index::<Option<String>>(0)?.is_none() {
        prepared.apply(&tx, id).await?;
    }
    usage_hourly::aggregate(&tx, Some(id)).await?;
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_writer_can_run_during_preparation_and_invalidation_discards_the_snapshot() {
        let path = std::env::temp_dir().join(format!("aegis-buckets-{}.db", uuid::Uuid::new_v4()));
        let database = crate::db::connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        database.execute_unprepared(
            "INSERT INTO gateway_requests(id,request_id,provider,protocol,method,endpoint,started_at,completed_at,request_bytes,response_bytes,client_disconnected) \
             VALUES('a','a','claude','anthropic_messages','POST','/v1/messages','2026-09-01T10:00:00Z','2026-09-01T10:01:00Z',0,0,FALSE)"
        ).await.unwrap();
        initialize(&database).await.unwrap();
        let snapshot = database.begin().await.unwrap();
        let expected = generation(&snapshot).await.unwrap();
        let prepared = tool_usage_hourly::prepare(&snapshot, "a").await.unwrap();
        // A read snapshot must not reserve SQLite's writer while payload joins run.
        tokio::time::timeout(Duration::from_secs(2), async {
            let tx = begin_immediate(&database).await.unwrap();
            invalidate(&tx).await.unwrap();
            tx.commit().await.unwrap();
        })
        .await
        .expect("WAL readers must allow a concurrent writer");
        snapshot.commit().await.unwrap();
        assert!(
            !apply_prepared(&database, "a", expected, prepared)
                .await
                .unwrap()
        );
        assert_eq!(batch(&database).await.unwrap(), 1);
        assert_eq!(batch(&database).await.unwrap(), 0);
        database.close().await.unwrap();
        let database = crate::db::connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        assert_eq!(
            batch(&database).await.unwrap(),
            0,
            "reopening resumes the durable checkpoint"
        );
        database.close().await.unwrap();
        std::fs::remove_file(path).unwrap();
    }
}
