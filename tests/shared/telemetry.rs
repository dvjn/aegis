//! Assertions against the gateway's sqlite telemetry.
//!
//! Rows are written off the request path, so nothing is visible the instant the
//! client response completes and every read has to poll. Tables and columns are
//! discovered from the live schema rather than hardcoded.

use std::{
    path::Path,
    time::{Duration, Instant},
};

use sea_orm::{ConnectionTrait, Database, DatabaseBackend, Statement};

const POLL_ATTEMPTS: usize = 80;
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Polls every usage-bearing table until one holds a row matching `expected`.
/// Returns the table it was found in.
pub async fn wait_for_usage_row(database: &Path, expected: &[(&str, i64)]) -> Option<String> {
    let url = format!("sqlite://{}?mode=ro", database.display());
    for _ in 0..POLL_ATTEMPTS {
        if let Ok(connection) = Database::connect(&url).await {
            for table in usage_tables(&connection).await {
                if matching_rows(&connection, &table, expected).await > 0 {
                    return Some(table);
                }
            }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    None
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TelemetryState {
    pub completed: usize,
    pub disconnected: usize,
    pub usage_rows: usize,
}

pub async fn wait_for_background_jobs(database: &Path, expected: usize, timeout: Duration) -> bool {
    let url = format!("sqlite://{}?mode=ro", database.display());
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(connection) = Database::connect(&url).await {
            let completed = connection
                .query_one_raw(Statement::from_string(
                    DatabaseBackend::Sqlite,
                    "SELECT COUNT(*) AS completed FROM background_jobs".to_string(),
                ))
                .await
                .ok()
                .flatten()
                .and_then(|row| row.try_get::<i64>("", "completed").ok())
                .unwrap_or_default();
            if completed >= expected as i64 {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

pub async fn wait_for_telemetry(
    database: &Path,
    expected_completed: usize,
    expected_disconnected: usize,
    timeout: Duration,
) -> TelemetryState {
    let url = format!("sqlite://{}?mode=ro", database.display());
    let deadline = Instant::now() + timeout;
    let mut latest = TelemetryState::default();
    while Instant::now() < deadline {
        if let Ok(connection) = Database::connect(&url).await {
            latest = telemetry_state(&connection).await;
            if latest.completed >= expected_completed
                && latest.disconnected >= expected_disconnected
            {
                return latest;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    latest
}

async fn telemetry_state(connection: &impl ConnectionTrait) -> TelemetryState {
    let request = connection
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS completed, COALESCE(SUM(client_disconnected), 0) AS disconnected FROM gateway_requests WHERE completed_at IS NOT NULL".to_string(),
        ))
        .await
        .ok()
        .flatten();
    let completed = request
        .as_ref()
        .and_then(|row| row.try_get::<i64>("", "completed").ok())
        .unwrap_or_default()
        .max(0) as usize;
    let disconnected = request
        .as_ref()
        .and_then(|row| row.try_get::<i64>("", "disconnected").ok())
        .unwrap_or_default()
        .max(0) as usize;
    let usage_rows = connection
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS rows FROM gateway_usage".to_string(),
        ))
        .await
        .ok()
        .flatten()
        .and_then(|row| row.try_get::<i64>("", "rows").ok())
        .unwrap_or_default()
        .max(0) as usize;
    TelemetryState {
        completed,
        disconnected,
        usage_rows,
    }
}

async fn query_column(connection: &impl ConnectionTrait, sql: String, column: &str) -> Vec<String> {
    connection
        .query_all_raw(Statement::from_string(DatabaseBackend::Sqlite, sql))
        .await
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.try_get::<String>("", column).ok())
                .collect()
        })
        .unwrap_or_default()
}

async fn usage_tables(connection: &impl ConnectionTrait) -> Vec<String> {
    let names = query_column(
        connection,
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'".into(),
        "name",
    )
    .await;

    let mut found = Vec::new();
    for name in names {
        let columns = query_column(
            connection,
            format!(r#"PRAGMA table_info("{name}")"#),
            "name",
        )
        .await;
        if columns.iter().any(|column| column == "input_tokens") {
            found.push(name);
        }
    }
    found
}

async fn matching_rows(
    connection: &impl ConnectionTrait,
    table: &str,
    expected: &[(&str, i64)],
) -> i64 {
    let predicate = expected
        .iter()
        .map(|(column, value)| format!(r#""{column}" = {value}"#))
        .collect::<Vec<_>>()
        .join(" AND ");
    let sql = format!(r#"SELECT COUNT(*) AS matches FROM "{table}" WHERE {predicate}"#);

    connection
        .query_one_raw(Statement::from_string(DatabaseBackend::Sqlite, sql))
        .await
        .ok()
        .flatten()
        .and_then(|row| row.try_get::<i64>("", "matches").ok())
        .unwrap_or(0)
}
