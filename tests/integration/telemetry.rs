//! Assertions against the gateway's sqlite telemetry.
//!
//! Rows are written off the request path, so nothing is visible the instant the
//! client response completes and every read has to poll. Tables and columns are
//! discovered from the live schema rather than hardcoded.

use std::{path::Path, time::Duration};

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
