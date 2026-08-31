use super::{ModelPrice, PriceMap};
use crate::config::PricingConfig;
use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement, TransactionTrait};
use std::collections::HashMap;

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
    let transaction = database.begin().await?;
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
}
