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

/// One entry of the vendored or fetched price map. `provider` is LiteLLM's
/// `litellm_provider`; a map published before it was kept has none.
#[derive(Deserialize)]
struct Entry {
    #[serde(flatten)]
    price: ModelPrice,
    #[serde(default)]
    provider: Option<String>,
}

/// LiteLLM's model prices, keyed as LiteLLM keys them: first-party names
/// (`claude-haiku-4-5`, `gemini/gemini-3.8-flash`, `typesafe/jev-1.13.0`),
/// cloud-prefixed and regional keys (`bedrock/us-east-1/zai.glm-5`,
/// `bedrock_mantle/openai.gpt-5.6-luna`, `vertex_ai/claude-haiku-4-5@20251001`)
/// and Bedrock Converse model IDs (`zai.glm-4.7`). The same model can cost
/// differently on each cloud, so a request is priced within the cloud that
/// served it; see [`PriceMap::price_in`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PriceMap {
    models: HashMap<String, ModelPrice>,
    /// The provider of each priced key. A key without one is a configured
    /// override, which applies on every cloud.
    providers: HashMap<String, String>,
}

impl PriceMap {
    pub fn vendored() -> Self {
        Self::from_json(PRICE_MAP.as_bytes()).expect("vendored model_prices.json is valid")
    }

    pub fn from_json(body: &[u8]) -> serde_json::Result<Self> {
        let entries: HashMap<String, Entry> = serde_json::from_slice(body)?;
        let mut map = Self::default();
        for (model, entry) in entries {
            map.insert(model, entry.price, entry.provider);
        }
        Ok(map)
    }

    pub fn insert(&mut self, model: String, price: ModelPrice, provider: Option<String>) {
        match provider {
            Some(provider) => self.providers.insert(model.clone(), provider),
            None => self.providers.remove(&model),
        };
        self.models.insert(model, price);
    }

    pub fn model_count(&self) -> usize {
        self.models.len()
    }

    pub fn entries(&self) -> impl Iterator<Item = (&str, &ModelPrice, Option<&str>)> {
        self.models.iter().map(|(model, price)| {
            (
                model.as_str(),
                price,
                self.providers.get(model).map(String::as_str),
            )
        })
    }

    /// These prices over `base`: a model priced in both takes these prices,
    /// and a model only `base` prices keeps its price. An entry without a
    /// provider, from a map published before providers were kept, keeps the
    /// provider `base` knows for it.
    pub fn over(self, mut base: PriceMap) -> PriceMap {
        for (model, price) in self.models {
            let provider = self
                .providers
                .get(&model)
                .or_else(|| base.providers.get(&model))
                .cloned();
            base.insert(model, price, provider);
        }
        base
    }

    pub fn with_overrides(mut self, overrides: &[PriceOverride]) -> Self {
        for entry in overrides {
            self.insert(
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
                None,
            );
        }
        self
    }

    /// A first-party price, for a request whose cloud is unknown.
    pub fn price(&self, model: &str) -> Option<ModelPrice> {
        self.price_in(None, model)
    }

    /// The price of `model` on the cloud named by `scope`, looked up the way
    /// LiteLLM does: `bedrock/<region>`, `bedrock`, `vertex_ai`, `anthropic`,
    /// `openai`, `gemini` or `typesafe`.
    ///
    /// 1. A configured override for the exact model, on any cloud.
    /// 2. Within the cloud: its regional key, its prefixed keys, then a bare
    ///    key that cloud's provider prices (Bedrock Converse model IDs, and
    ///    Vertex's Gemini names).
    /// 3. The first-party price of the model inside a cloud ID, such as
    ///    `claude-haiku-4-5` for `us.anthropic.claude-haiku-4-5-20251001-v1:0`.
    pub fn price_in(&self, scope: Option<&str>, model: &str) -> Option<ModelPrice> {
        if !self.providers.contains_key(model)
            && let Some(price) = self.models.get(model)
        {
            return Some(*price);
        }
        let unprefixed = model.rsplit('/').next().unwrap_or(model);
        if let Some(scope) = scope {
            let (family, region) = match scope.split_once('/') {
                Some((family, region)) => (family, Some(region)),
                None => (scope, None),
            };
            for name in [model, unprefixed] {
                if let Some(price) = self.scoped(family, region, name) {
                    return Some(price);
                }
            }
        }
        let cloud = cloud_model_name(unprefixed);
        for candidate in [Some(model), Some(unprefixed), cloud.as_deref()]
            .into_iter()
            .flatten()
        {
            for name in [Some(candidate), strip_release_date(candidate)]
                .into_iter()
                .flatten()
            {
                if let Some(price) = self.first_party(name) {
                    return Some(price);
                }
            }
        }
        None
    }

    fn scoped(&self, family: &str, region: Option<&str>, name: &str) -> Option<ModelPrice> {
        let prefixed: Vec<String> = match family {
            "bedrock" => region
                .into_iter()
                .flat_map(|region| {
                    [
                        format!("bedrock/{region}/{name}"),
                        format!("bedrock_mantle/{region}/{name}"),
                    ]
                })
                .chain([format!("bedrock/{name}"), format!("bedrock_mantle/{name}")])
                .collect(),
            family => vec![format!("{family}/{name}")],
        };
        if let Some(price) = prefixed.iter().find_map(|key| self.models.get(key)) {
            return Some(*price);
        }
        let provider = self.providers.get(name)?;
        let in_family = match family {
            "bedrock" | "vertex_ai" => provider.starts_with(family),
            family => provider == family,
        };
        in_family.then(|| self.models[name])
    }

    /// A first-party price: Anthropic's or OpenAI's bare names, the prefixed
    /// Gemini and TypeSafe names, and entries without a provider, which are
    /// overrides or come from a map published before providers were kept.
    fn first_party(&self, name: &str) -> Option<ModelPrice> {
        let price = self.models.get(name);
        let first_party = self
            .providers
            .get(name)
            .is_none_or(|provider| provider == "anthropic" || provider == "openai");
        if let Some(price) = price.filter(|_| first_party) {
            return Some(*price);
        }
        ["gemini", "typesafe"]
            .iter()
            .find_map(|family| self.models.get(&format!("{family}/{name}")))
            .copied()
    }

    fn cost(&self, scope: Option<&str>, model: Option<&str>, usage: &Usage) -> Cost {
        calculate_cost(model.and_then(|model| self.price_in(scope, model)), usage)
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

#[cfg(test)]
pub fn price(model: &str) -> Option<ModelPrice> {
    active_map().price(model)
}

/// The price scope of a request whose stored row predates scopes, from its
/// format: only the first-party formats existed then.
pub const SCOPE_FROM_PROTOCOL: &str = "CASE r.protocol \
    WHEN 'anthropic_messages' THEN 'anthropic' \
    WHEN 'openai_responses' THEN 'openai' \
    WHEN 'typesafe_systemone' THEN 'typesafe' END";

/// The plain model name inside a cloud provider's model ID: Bedrock's
/// `eu.anthropic.claude-sonnet-4-6-v1:0` or `openai.gpt-5.6-sol`, and Vertex's
/// `claude-sonnet-4-5@20250929`.
fn cloud_model_name(model: &str) -> Option<String> {
    const REGIONS: [&str; 7] = ["us", "eu", "apac", "global", "us-gov", "jp", "au"];
    const VENDORS: [&str; 8] = [
        "anthropic",
        "openai",
        "amazon",
        "meta",
        "mistral",
        "cohere",
        "deepseek",
        "qwen",
    ];
    let mut name = model.split('@').next().unwrap_or(model);
    if let Some((region, rest)) = name.split_once('.')
        && REGIONS.contains(&region)
    {
        name = rest;
    }
    if let Some((vendor, rest)) = name.split_once('.')
        && VENDORS.contains(&vendor)
    {
        name = rest;
    }
    // Bedrock's version suffix: `-v1:0`, `-v2`, or a bare `:0`.
    let name = name.rsplit_once(':').map_or(name, |(head, tail)| {
        if tail.bytes().all(|byte| byte.is_ascii_digit()) {
            head
        } else {
            name
        }
    });
    let name = name.rsplit_once("-v").map_or(name, |(head, tail)| {
        if !tail.is_empty() && tail.bytes().all(|byte| byte.is_ascii_digit()) {
            head
        } else {
            name
        }
    });
    (name != model).then(|| name.to_owned())
}

fn strip_release_date(model: &str) -> Option<&str> {
    let (name, date) = model.rsplit_once('-')?;
    let is_release_date = date.len() == 8 && date.bytes().all(|byte| byte.is_ascii_digit());
    is_release_date.then_some(name)
}

/// The cost of `usage` on `model`, priced within the cloud `scope` names.
pub fn cost(scope: Option<&str>, model: Option<&str>, usage: &Usage) -> Cost {
    active_map().cost(scope, model, usage)
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
            "backfilled historical request costs; hourly rebuild queued"
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

    /// The input price `model` gets on the cloud `scope` names, from the
    /// vendored map.
    fn input_in(scope: Option<&str>, model: &str) -> Option<f64> {
        PriceMap::vendored()
            .price_in(scope, model)
            .map(|price| price.input)
    }

    #[test]
    fn a_model_is_priced_by_the_cloud_that_served_it() {
        // Bedrock: Converse model IDs, OpenAI-compatible (Mantle) keys and
        // the regional premium on a US inference profile.
        assert_eq!(
            input_in(Some("bedrock/us-east-1"), "zai.glm-4.7"),
            Some(6e-7)
        );
        assert_eq!(
            input_in(Some("bedrock/us-east-1"), "openai.gpt-5.6-luna"),
            Some(2.2e-7)
        );
        assert_eq!(
            input_in(
                Some("bedrock/us-east-1"),
                "us.anthropic.claude-haiku-4-5-20251001-v1:0"
            ),
            Some(1.1e-6)
        );
        // Vertex: its own prefixed keys and its bare Gemini names.
        assert_eq!(
            input_in(Some("vertex_ai"), "claude-haiku-4-5@20251001"),
            Some(1e-6)
        );
        assert_eq!(
            input_in(Some("vertex_ai"), "zai-org/glm-4.7-maas"),
            Some(6e-7)
        );
        assert_eq!(
            input_in(Some("vertex_ai"), "gemini-3.8-flash"),
            Some(7.5e-7)
        );
        // A first-party request never takes a cloud's bare key.
        assert_eq!(input_in(Some("anthropic"), "claude-haiku-4-5"), Some(1e-6));
        assert_eq!(input_in(Some("anthropic"), "zai.glm-4.7"), None);
        // A cloud model the cloud's own prices miss falls back to the
        // first-party price of the model inside its ID.
        assert_eq!(
            input_in(
                Some("bedrock/eu-west-1"),
                "eu.anthropic.claude-haiku-4-5-v1:0"
            ),
            Some(1e-6)
        );
    }

    /// Released servers fetch the published map and read it with this shape,
    /// looking models up by their bare names. They must keep pricing the
    /// models they serve.
    #[test]
    fn the_published_map_stays_readable_by_earlier_releases() {
        #[derive(serde::Deserialize)]
        struct Released {
            input: f64,
            #[allow(dead_code)]
            output: f64,
            #[allow(dead_code)]
            cache_read: Option<f64>,
            #[allow(dead_code)]
            cache_write: Option<f64>,
        }
        let released: HashMap<String, Released> =
            serde_json::from_str(PRICE_MAP).expect("earlier releases parse the map");
        for model in [
            "claude-opus-4-5",
            "claude-sonnet-4-5",
            "gpt-5-codex",
            "jev-1.13.0",
            "jev-latest",
        ] {
            assert!(
                released.get(model).is_some_and(|price| price.input > 0.0),
                "{model}"
            );
        }
    }

    #[test]
    fn fetched_prices_layer_over_the_vendored_ones() {
        let fetched = PriceMap::from_json(
            br#"{"claude-haiku-4-5":{"input":0.5,"output":1.0},"zai.glm-4.7":{"input":0.25,"output":1.0}}"#,
        )
        .unwrap();
        let effective = fetched.over(PriceMap::vendored());
        // A fetched entry without a provider keeps the vendored provider.
        assert_eq!(
            effective
                .price_in(Some("bedrock"), "zai.glm-4.7")
                .map(|p| p.input),
            Some(0.25)
        );
        assert_eq!(effective.price_in(Some("anthropic"), "zai.glm-4.7"), None);
        // A model only the vendored map prices keeps its price.
        assert_eq!(
            effective
                .price_in(Some("vertex_ai"), "gemini-3.8-flash")
                .map(|p| p.input),
            Some(7.5e-7)
        );
    }

    #[test]
    fn cloud_model_ids_resolve_to_their_plain_names() {
        for (id, name) in [
            (
                "eu.anthropic.claude-sonnet-4-6-v1:0",
                Some("claude-sonnet-4-6"),
            ),
            (
                "anthropic.claude-haiku-4-5-20251001-v1:0",
                Some("claude-haiku-4-5-20251001"),
            ),
            ("global.anthropic.claude-opus-4-7", Some("claude-opus-4-7")),
            ("openai.gpt-5.6-sol", Some("gpt-5.6-sol")),
            ("claude-sonnet-4-5@20250929", Some("claude-sonnet-4-5")),
            ("gpt-5.6-sol", None),
            ("claude-sonnet-4-6", None),
        ] {
            assert_eq!(super::cloud_model_name(id).as_deref(), name, "{id}");
        }
    }

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
            ..Default::default()
        }
    }

    #[test]
    fn every_jev_name_is_priced_and_charges_for_input_alone() {
        let map = PriceMap::vendored();
        for model in ["jev-1.13.0", "jev-latest", "jev-preview"] {
            let price = map.price(model).unwrap_or_else(|| panic!("{model}"));
            assert_eq!(price.input, 4.2e-8);
            assert_eq!(price.output, 0.0);
        }
        let cost = map.cost(
            Some("typesafe"),
            Some("jev-latest"),
            &usage(1_000_000, 0, 0, 1_000_000),
        );
        assert_eq!(cost.source, CostSource::Calculated);
        assert_eq!(
            cost.nanodollars,
            Some((0.042 * NANODOLLARS_PER_DOLLAR) as i64),
            "output tokens are free"
        );
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
        let cost = cost(
            Some("anthropic"),
            Some("claude-sonnet-4-5"),
            &usage(1_000, 2_000, 500, 300),
        );
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
        let cost = cost(Some("openai"), Some("gpt-5-codex"), &usage);
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
            let cost = cost(None, model, &usage(1_000, 0, 0, 100));
            assert_eq!(cost.nanodollars, None);
            assert_eq!(cost.source, CostSource::Unknown);
        }
    }

    #[test]
    fn a_priced_model_with_no_tokens_is_unknown_rather_than_free() {
        let cost = cost(None, Some("claude-sonnet-4-5"), &usage(0, 0, 0, 0));
        assert_eq!(cost.nanodollars, None);
        assert_eq!(cost.source, CostSource::Unknown);
    }
}
