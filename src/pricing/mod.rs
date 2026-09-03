mod remote;
mod store;

use crate::{config::PriceOverride, providers::Usage};
pub use remote::spawn_refresh;
use serde::Deserialize;
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock, RwLock},
};
pub use store::load_effective_map;

pub const NANODOLLARS_PER_DOLLAR: f64 = 1e9;

const PRICE_MAP: &str = include_str!("model_prices.json");

const TOKENS_PER_MILLION: f64 = 1e6;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
pub struct ModelPrice {
    input: f64,
    output: f64,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CostSource {
    Calculated,
    Unknown,
}

impl CostSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Calculated => "calculated",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cost {
    pub nanodollars: Option<i64>,
    pub source: CostSource,
}

impl Cost {
    fn unknown() -> Self {
        Self {
            nanodollars: None,
            source: CostSource::Unknown,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PriceMap {
    models: HashMap<String, ModelPrice>,
}

impl PriceMap {
    pub fn vendored() -> Self {
        Self {
            models: serde_json::from_str(PRICE_MAP).expect("vendored model_prices.json is valid"),
        }
    }

    pub fn model_count(&self) -> usize {
        self.models.len()
    }

    pub fn entries(&self) -> impl Iterator<Item = (&str, &ModelPrice)> {
        self.models
            .iter()
            .map(|(model, price)| (model.as_str(), price))
    }

    pub fn with_overrides(mut self, overrides: &[PriceOverride]) -> Self {
        for entry in overrides {
            self.models.insert(
                entry.model.clone(),
                ModelPrice {
                    input: entry.input_per_mtok / TOKENS_PER_MILLION,
                    output: entry.output_per_mtok / TOKENS_PER_MILLION,
                    cache_read: entry
                        .cache_read_per_mtok
                        .map(|rate| rate / TOKENS_PER_MILLION),
                    cache_write: entry
                        .cache_write_per_mtok
                        .map(|rate| rate / TOKENS_PER_MILLION),
                },
            );
        }
        self
    }

    pub fn price(&self, model: &str) -> Option<ModelPrice> {
        let unprefixed = model.rsplit('/').next().unwrap_or(model);
        for candidate in [model, unprefixed] {
            if let Some(price) = self.models.get(candidate) {
                return Some(*price);
            }
            if let Some(price) =
                strip_release_date(candidate).and_then(|name| self.models.get(name))
            {
                return Some(*price);
            }
        }
        None
    }

    fn cost(&self, model: Option<&str>, usage: &Usage) -> Cost {
        calculate_cost(model.and_then(|model| self.price(model)), usage)
    }
}

fn calculate_cost(price: Option<ModelPrice>, usage: &Usage) -> Cost {
    let Some(price) = price else {
        return Cost::unknown();
    };
    let input = usage.input_tokens.unwrap_or(0);
    let cache_read = usage.cache_read_tokens.unwrap_or(0);
    let cache_write = usage.cache_write_tokens.unwrap_or(0);
    let output = usage.output_tokens.unwrap_or(0);
    if input == 0 && cache_read == 0 && cache_write == 0 && output == 0 {
        return Cost::unknown();
    }

    let dollars = input as f64 * price.input
        + cache_read as f64 * price.cache_read.unwrap_or(price.input)
        + cache_write as f64 * price.cache_write.unwrap_or(0.0)
        + output as f64 * price.output;

    Cost {
        nanodollars: Some((dollars * NANODOLLARS_PER_DOLLAR).round() as i64),
        source: CostSource::Calculated,
    }
}

fn active() -> &'static RwLock<Arc<PriceMap>> {
    static ACTIVE: OnceLock<RwLock<Arc<PriceMap>>> = OnceLock::new();
    ACTIVE.get_or_init(|| RwLock::new(Arc::new(PriceMap::vendored())))
}

pub fn active_map() -> Arc<PriceMap> {
    Arc::clone(&active().read().expect("price map lock is never poisoned"))
}

pub fn install(map: PriceMap) {
    *active().write().expect("price map lock is never poisoned") = Arc::new(map);
}

pub fn price(model: &str) -> Option<ModelPrice> {
    active_map().price(model)
}

fn strip_release_date(model: &str) -> Option<&str> {
    let (name, date) = model.rsplit_once('-')?;
    let is_release_date = date.len() == 8 && date.bytes().all(|byte| byte.is_ascii_digit());
    is_release_date.then_some(name)
}

pub fn cost(model: Option<&str>, usage: &Usage) -> Cost {
    calculate_cost(model.and_then(price), usage)
}

pub async fn backfill_costs(
    database: &sea_orm::DatabaseConnection,
    map: &PriceMap,
) -> anyhow::Result<()> {
    let stats = store::backfill_unknown_costs(database, map).await?;
    if stats.updated > 0 {
        tracing::info!(
            scanned = stats.scanned,
            updated = stats.updated,
            still_unpriced = stats.scanned - stats.updated,
            "backfilled historical request costs"
        );
    } else {
        tracing::debug!(
            scanned = stats.scanned,
            still_unpriced = stats.scanned,
            "historical request costs need no backfill"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: i64, cache_read: i64, cache_write: i64, output: i64) -> Usage {
        Usage {
            input_tokens: Some(input),
            cache_read_tokens: Some(cache_read),
            cache_write_tokens: Some(cache_write),
            output_tokens: Some(output),
            reasoning_tokens: None,
            raw_json: None,
        }
    }

    fn fetched_map(model: &str, input: f64) -> PriceMap {
        PriceMap {
            models: HashMap::from([(
                model.to_owned(),
                ModelPrice {
                    input,
                    output: input * 5.0,
                    cache_read: None,
                    cache_write: None,
                },
            )]),
        }
    }

    #[test]
    fn a_fetched_price_replaces_the_vendored_one() {
        let vendored = PriceMap::vendored()
            .price("claude-sonnet-4-5")
            .expect("vendored");
        let fetched = fetched_map("claude-sonnet-4-5", vendored.input * 2.0);
        assert_eq!(
            fetched.price("claude-sonnet-4-5").expect("fetched").input,
            vendored.input * 2.0
        );
    }

    #[test]
    fn an_override_replaces_a_fetched_price() {
        let overrides = [PriceOverride {
            model: "claude-sonnet-4-5".into(),
            input_per_mtok: 9.0,
            output_per_mtok: 36.0,
            cache_read_per_mtok: Some(0.9),
            cache_write_per_mtok: None,
        }];
        let map = fetched_map("claude-sonnet-4-5", 3e-06).with_overrides(&overrides);
        let price = map.price("claude-sonnet-4-5").expect("overridden");
        assert_eq!(price.input, 9.0 / TOKENS_PER_MILLION);
        assert_eq!(price.output, 36.0 / TOKENS_PER_MILLION);
        assert_eq!(price.cache_read, Some(0.9 / TOKENS_PER_MILLION));
        assert_eq!(price.cache_write, None);
    }

    #[test]
    fn an_override_for_an_unfetched_model_is_still_priced() {
        let overrides = [PriceOverride {
            model: "an-unlisted-model".into(),
            input_per_mtok: 1.0,
            output_per_mtok: 2.0,
            cache_read_per_mtok: None,
            cache_write_per_mtok: None,
        }];
        let map = PriceMap::vendored().with_overrides(&overrides);
        assert_eq!(
            map.price("an-unlisted-model").expect("overridden").input,
            1.0 / TOKENS_PER_MILLION
        );
    }

    #[test]
    fn the_vendored_map_parses_and_covers_current_models() {
        for model in [
            "claude-opus-4-5",
            "claude-sonnet-4-5",
            "claude-haiku-4-5",
            "gpt-5-codex",
        ] {
            assert!(price(model).is_some(), "{model} should be priced");
        }
    }

    #[test]
    fn model_names_resolve_through_dates_and_routing_prefixes() {
        let plain = price("claude-sonnet-4-5").expect("undated name");
        for name in [
            "claude-sonnet-4-5-20250929",
            "anthropic/claude-sonnet-4-5",
            "anthropic/claude-sonnet-4-5-20250929",
        ] {
            let resolved = price(name).unwrap_or_else(|| panic!("{name} should resolve"));
            assert_eq!(resolved.input, plain.input, "{name}");
        }
    }

    #[test]
    fn a_version_suffix_that_is_not_a_date_is_left_alone() {
        assert_eq!(strip_release_date("claude-sonnet-4-5"), None);
        assert_eq!(strip_release_date("gpt-5.1-codex-max"), None);
        assert_eq!(
            strip_release_date("claude-sonnet-4-5-20250929"),
            Some("claude-sonnet-4-5")
        );
    }

    #[test]
    fn anthropic_charges_every_token_class_separately() {
        let price = price("claude-sonnet-4-5").expect("priced");
        let cost = cost(Some("claude-sonnet-4-5"), &usage(1_000, 2_000, 500, 300));
        let expected = 1_000.0 * price.input
            + 2_000.0 * price.cache_read.expect("anthropic prices cache reads")
            + 500.0 * price.cache_write.expect("anthropic prices cache writes")
            + 300.0 * price.output;
        assert_eq!(
            cost.nanodollars,
            Some((expected * NANODOLLARS_PER_DOLLAR).round() as i64)
        );
        assert_eq!(cost.source, CostSource::Calculated);
    }

    #[test]
    fn codex_cached_tokens_are_billed_at_the_cache_rate_not_the_input_rate() {
        use crate::providers::{Provider, extract_usage};

        let usage = extract_usage(
            Provider::Codex,
            br#"{"usage":{"input_tokens":1000,"output_tokens":200,"input_tokens_details":{"cached_tokens":400}}}"#,
        );
        let price = price("gpt-5-codex").expect("priced");
        let cost = cost(Some("gpt-5-codex"), &usage);
        let expected = 600.0 * price.input
            + 400.0 * price.cache_read.expect("openai prices cache reads")
            + 200.0 * price.output;
        assert_eq!(
            cost.nanodollars,
            Some((expected * NANODOLLARS_PER_DOLLAR).round() as i64),
            "the 400 cached tokens must be billed once, at the cache rate"
        );
    }

    #[test]
    fn an_unpriced_or_absent_model_is_reported_as_unknown() {
        for model in [None, Some("not-a-real-model")] {
            let cost = cost(model, &usage(1_000, 0, 0, 100));
            assert_eq!(cost.nanodollars, None);
            assert_eq!(cost.source, CostSource::Unknown);
        }
    }

    #[test]
    fn a_priced_model_with_no_tokens_is_unknown_rather_than_free() {
        let cost = cost(Some("claude-sonnet-4-5"), &usage(0, 0, 0, 0));
        assert_eq!(cost.nanodollars, None);
        assert_eq!(cost.source, CostSource::Unknown);
    }
}
