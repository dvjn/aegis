use super::{ModelPrice, PriceMap};
use crate::config::PricingConfig;
use crate::providers::Usage;
use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};
use std::collections::HashMap;

const BACKFILL_BATCH_SIZE: u64 = 32;

#[derive(Debug, PartialEq, Eq)]
pub struct BackfillStats {
    pub scanned: u64,
    pub updated: u64,
}

pub async fn load_effective_map(
    database: &DatabaseConnection,
    pricing: &PricingConfig,
) -> Result<PriceMap> {
    let fetched = load_fetched(database).await?;
    let base = match fetched {
        Some(map) => map,
        None => PriceMap::vendored(),
    };
    Ok(base.with_overrides(&pricing.overrides))
}

pub async fn load_fetched(database: &DatabaseConnection) -> Result<Option<PriceMap>> {
    let rows = database
        .query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT model, input_per_token, output_per_token, cache_read_per_token, cache_write_per_token FROM model_prices",
        ))
        .await
        .context("failed to read stored model prices")?;

    let mut models = HashMap::with_capacity(rows.len());
    for row in rows {
        let model: String = row.try_get("", "model")?;
        models.insert(
            model,
            ModelPrice {
                input: row.try_get("", "input_per_token")?,
                output: row.try_get("", "output_per_token")?,
                cache_read: row.try_get("", "cache_read_per_token")?,
                cache_write: row.try_get("", "cache_write_per_token")?,
            },
        );
    }
    Ok((!models.is_empty()).then_some(PriceMap { models }))
}

pub async fn store_snapshot(
    database: &DatabaseConnection,
    map: &PriceMap,
    source_url: &str,
    etag: Option<&str>,
    sha256: &str,
) -> Result<()> {
    let transaction = crate::db::begin_immediate(database).await?;
    transaction
        .execute_raw(Statement::from_string(
            DbBackend::Sqlite,
            "DELETE FROM model_prices",
        ))
        .await?;
    for (model, price) in map.entries() {
        transaction
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO model_prices (model, input_per_token, output_per_token, cache_read_per_token, cache_write_per_token) VALUES (?, ?, ?, ?, ?)",
                [
                    model.into(),
                    price.input.into(),
                    price.output.into(),
                    price.cache_read.into(),
                    price.cache_write.into(),
                ],
            ))
            .await?;
    }
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO model_price_snapshots (fetched_at, source_url, etag, sha256, model_count) VALUES (?, ?, ?, ?, ?)",
            [
                Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true).into(),
                source_url.into(),
                etag.into(),
                sha256.into(),
                (map.model_count() as i64).into(),
            ],
        ))
        .await?;
    transaction.commit().await?;
    Ok(())
}

pub async fn latest_etag(database: &DatabaseConnection) -> Result<Option<String>> {
    let row = database
        .query_one_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT etag FROM model_price_snapshots ORDER BY id DESC LIMIT 1",
        ))
        .await?;
    match row {
        Some(row) => Ok(row.try_get("", "etag")?),
        None => Ok(None),
    }
}

pub async fn backfill_unknown_costs(
    database: &DatabaseConnection,
    map: &PriceMap,
) -> Result<BackfillStats> {
    let mut stats = BackfillStats {
        scanned: 0,
        updated: 0,
    };
    let mut cursor = String::new();

    loop {
        let rows = database
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                format!(
                    "SELECT u.request_id, r.requested_model, u.input_tokens, u.output_tokens, \
                     u.cache_read_tokens, u.cache_write_tokens \
                     FROM gateway_usage u JOIN gateway_requests r ON r.id = u.request_id \
                     WHERE u.cost_nanodollars IS NULL \
                     AND (u.cost_source IS NULL OR u.cost_source = 'unknown') \
                     AND u.request_id > ? ORDER BY u.request_id LIMIT {BACKFILL_BATCH_SIZE}"
                ),
                [cursor.clone().into()],
            ))
            .await
            .context("failed to read unpriced request usage")?;
        if rows.is_empty() {
            break;
        }

        stats.scanned += rows.len() as u64;
        cursor = rows
            .last()
            .expect("a non-empty batch has a final row")
            .try_get("", "request_id")?;

        let mut updates = Vec::new();
        for row in rows {
            let request_id: String = row.try_get("", "request_id")?;
            let model: Option<String> = row.try_get("", "requested_model")?;
            let usage = Usage {
                input_tokens: row.try_get("", "input_tokens")?,
                output_tokens: row.try_get("", "output_tokens")?,
                cache_read_tokens: row.try_get("", "cache_read_tokens")?,
                cache_write_tokens: row.try_get("", "cache_write_tokens")?,
                reasoning_tokens: None,
                raw_json: None,
            };
            if let Some(cost) = map.cost(model.as_deref(), &usage).nanodollars {
                updates.push((request_id, cost));
            }
        }
        if updates.is_empty() {
            continue;
        }

        let transaction = crate::db::begin_immediate(database).await?;
        for (request_id, cost) in updates {
            let result = transaction
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    "UPDATE gateway_usage SET cost_nanodollars = ?, cost_source = 'calculated' \
                     WHERE request_id = ? AND cost_nanodollars IS NULL \
                     AND (cost_source IS NULL OR cost_source = 'unknown')",
                    [cost.into(), request_id.into()],
                ))
                .await?;
            stats.updated += result.rows_affected();
        }
        transaction.commit().await?;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use sea_orm::Database;
    use sea_orm_migration::MigratorTrait;

    async fn database() -> DatabaseConnection {
        let database = Database::connect("sqlite::memory:")
            .await
            .expect("in-memory database");
        Migrator::up(&database, None).await.expect("migrations");
        database
    }

    #[tokio::test]
    async fn a_stored_map_comes_back_with_the_same_prices() {
        let database = database().await;
        let map = PriceMap::vendored();
        store_snapshot(
            &database,
            &map,
            "https://example.invalid/prices.json",
            Some("\"abc\""),
            "0".repeat(64).as_str(),
        )
        .await
        .expect("snapshot should store");

        let loaded = load_fetched(&database)
            .await
            .expect("snapshot should load")
            .expect("a stored snapshot exists");
        assert_eq!(loaded, map);
        assert_eq!(
            latest_etag(&database).await.expect("etag should load"),
            Some("\"abc\"".to_owned())
        );
    }

    #[tokio::test]
    async fn an_empty_table_reads_as_no_stored_snapshot() {
        let database = database().await;
        assert_eq!(load_fetched(&database).await.expect("read"), None);
        assert_eq!(latest_etag(&database).await.expect("read"), None);
    }

    #[tokio::test]
    async fn adopting_a_snapshot_replaces_every_earlier_price() {
        let database = database().await;
        store_snapshot(
            &database,
            &PriceMap::vendored(),
            "https://example.invalid/prices.json",
            None,
            "0".repeat(64).as_str(),
        )
        .await
        .expect("first snapshot");

        let mut models = HashMap::new();
        models.insert(
            "claude-sonnet-4-5".to_owned(),
            ModelPrice {
                input: 1e-6,
                output: 2e-6,
                cache_read: None,
                cache_write: None,
            },
        );
        let replacement = PriceMap { models };
        store_snapshot(
            &database,
            &replacement,
            "https://example.invalid/prices.json",
            None,
            "1".repeat(64).as_str(),
        )
        .await
        .expect("second snapshot");

        let loaded = load_fetched(&database)
            .await
            .expect("read")
            .expect("a stored snapshot exists");
        assert_eq!(loaded, replacement);
    }

    async fn insert_usage(
        database: &DatabaseConnection,
        id: &str,
        model: &str,
        input: i64,
        output: i64,
        cost: Option<i64>,
        source: &str,
    ) {
        database
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO gateway_requests \
                 (id, request_id, provider, protocol, method, endpoint, requested_model, \
                  started_at, request_bytes, response_bytes, client_disconnected) \
                 VALUES (?, ?, 'claude', 'anthropic_messages', 'POST', '/v1/messages', ?, \
                         '2026-09-01T00:00:00Z', 0, 0, FALSE)",
                [id.into(), id.into(), model.into()],
            ))
            .await
            .expect("request should insert");
        database
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO gateway_usage \
                 (request_id, input_tokens, output_tokens, cost_nanodollars, cost_source) \
                 VALUES (?, ?, ?, ?, ?)",
                [
                    id.into(),
                    input.into(),
                    output.into(),
                    cost.into(),
                    source.into(),
                ],
            ))
            .await
            .expect("usage should insert");
    }

    #[tokio::test]
    async fn backfill_prices_only_requests_that_are_still_unknown() {
        let database = database().await;
        insert_usage(
            &database,
            "a",
            "provider/claude-new-20260903",
            100,
            10,
            None,
            "unknown",
        )
        .await;
        insert_usage(&database, "b", "still-unknown", 100, 10, None, "unknown").await;
        insert_usage(&database, "c", "claude-new", 100, 10, Some(7), "calculated").await;

        let map = PriceMap {
            models: HashMap::from([(
                "claude-new".to_owned(),
                ModelPrice {
                    input: 1e-6,
                    output: 2e-6,
                    cache_read: None,
                    cache_write: None,
                },
            )]),
        };
        assert_eq!(
            backfill_unknown_costs(&database, &map)
                .await
                .expect("backfill should succeed"),
            BackfillStats {
                scanned: 2,
                updated: 1,
            }
        );

        let rows = database
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT request_id, cost_nanodollars, cost_source FROM gateway_usage ORDER BY request_id",
            ))
            .await
            .expect("costs should load");
        let costs: Vec<(String, Option<i64>, Option<String>)> = rows
            .iter()
            .map(|row| {
                Ok((
                    row.try_get("", "request_id")?,
                    row.try_get("", "cost_nanodollars")?,
                    row.try_get("", "cost_source")?,
                ))
            })
            .collect::<Result<_>>()
            .expect("cost rows should decode");
        assert_eq!(
            costs,
            vec![
                ("a".to_owned(), Some(120_000), Some("calculated".to_owned())),
                ("b".to_owned(), None, Some("unknown".to_owned())),
                ("c".to_owned(), Some(7), Some("calculated".to_owned())),
            ]
        );
        assert_eq!(
            backfill_unknown_costs(&database, &map)
                .await
                .expect("repeat backfill should succeed"),
            BackfillStats {
                scanned: 1,
                updated: 0,
            }
        );
    }
}
