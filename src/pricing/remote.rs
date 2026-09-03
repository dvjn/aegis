use super::{ModelPrice, PriceMap, install, store};
use crate::{config::PricingConfig, gateway::webpki_roots_tls_config};
use anyhow::{Context, Result, bail};
use sea_orm::DatabaseConnection;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, time::Duration};
use tokio_util::sync::CancellationToken;

const MINIMUM_PRICED_MODELS: usize = 50;
const CANARY_MODELS: [&str; 3] = ["claude-opus-4-5", "claude-sonnet-4-5", "gpt-5-codex"];

const MAXIMUM_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

const FIRST_REFRESH_DELAY: Duration = Duration::from_secs(5);

pub fn spawn_refresh(
    database: DatabaseConnection,
    pricing: PricingConfig,
    cancellation: CancellationToken,
) {
    if !pricing.enabled {
        tracing::info!("remote model price refresh is disabled");
        return;
    }
    tokio::spawn(async move {
        let client = match build_client() {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!(%error, "model price refresh disabled: HTTP client setup failed");
                return;
            }
        };
        let mut delay = FIRST_REFRESH_DELAY;
        loop {
            tokio::select! {
                () = cancellation.cancelled() => return,
                () = tokio::time::sleep(delay) => {}
            }
            if let Err(error) = refresh_once(&client, &database, &pricing).await {
                tracing::warn!(%error, "model price refresh failed, keeping the prices in use");
            }
            delay = Duration::from_secs(pricing.refresh_hours * 60 * 60);
        }
    });
}

fn build_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let same_origin = attempt.url().scheme() == "https"
                && attempt.previous().last().and_then(|url| url.host_str())
                    == attempt.url().host_str();
            if same_origin {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .tls_backend_preconfigured(webpki_roots_tls_config()?)
        .build()?)
}

async fn refresh_once(
    client: &reqwest::Client,
    database: &DatabaseConnection,
    pricing: &PricingConfig,
) -> Result<()> {
    let known_etag = store::latest_etag(database).await?;
    let Some(fetched) = fetch(client, &pricing.url, known_etag.as_deref()).await? else {
        tracing::debug!("upstream model prices are unchanged");
        return Ok(());
    };

    let candidate = parse(&fetched.body).context("remote model prices could not be parsed")?;
    let current = super::active_map();
    if let Some(reason) = adoption_rejection(&candidate, &current) {
        tracing::warn!(
            reason,
            fetched_models = candidate.model_count(),
            current_models = current.model_count(),
            "rejected the fetched model prices, keeping the prices in use"
        );
        return Ok(());
    }

    let sha256: String = Sha256::digest(&fetched.body)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    store::store_snapshot(
        database,
        &candidate,
        &pricing.url,
        fetched.etag.as_deref(),
        &sha256,
    )
    .await?;
    let models = candidate.model_count();
    install(candidate.with_overrides(&pricing.overrides));
    tracing::info!(models, "adopted fetched model prices");
    Ok(())
}

struct FetchedBody {
    body: Vec<u8>,
    etag: Option<String>,
}

async fn fetch(
    client: &reqwest::Client,
    url: &str,
    known_etag: Option<&str>,
) -> Result<Option<FetchedBody>> {
    let mut request = client.get(url).timeout(REQUEST_TIMEOUT);
    if let Some(etag) = known_etag {
        request = request.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    let response = request.send().await.context("failed to request prices")?;
    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(None);
    }
    let response = response
        .error_for_status()
        .context("prices request was refused")?;
    let etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let mut response = response;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.context("failed to read prices")? {
        if body.len() + chunk.len() > MAXIMUM_RESPONSE_BYTES {
            bail!("prices response exceeded {MAXIMUM_RESPONSE_BYTES} bytes");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Some(FetchedBody { body, etag }))
}

fn parse(body: &[u8]) -> Result<PriceMap> {
    let models: HashMap<String, ModelPrice> = serde_json::from_slice(body)?;
    Ok(PriceMap { models })
}

fn adoption_rejection(candidate: &PriceMap, current: &PriceMap) -> Option<String> {
    if candidate.model_count() < MINIMUM_PRICED_MODELS {
        return Some(format!(
            "only {} priced models, fewer than the {MINIMUM_PRICED_MODELS} required",
            candidate.model_count()
        ));
    }
    if candidate.model_count() * 2 < current.model_count() {
        return Some(format!(
            "shrank from {} to {} models",
            current.model_count(),
            candidate.model_count()
        ));
    }
    for canary in CANARY_MODELS {
        if candidate.price(canary).is_none() {
            return Some(format!("canary model {canary} is not priced"));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURATED: &str = r#"{
        "claude-sonnet-4-5": {
            "input": 3e-06,
            "output": 1.5e-05,
            "cache_read": 3e-07,
            "cache_write": 3.75e-06
        },
        "gpt-4o-mini-tts": {
            "input": 0.0000006,
            "output": 0.000012
        }
    }"#;

    fn map(models: &[(&str, f64)]) -> PriceMap {
        PriceMap {
            models: models
                .iter()
                .map(|&(model, input)| {
                    (
                        model.to_owned(),
                        ModelPrice {
                            input,
                            output: input * 4.0,
                            cache_read: None,
                            cache_write: None,
                        },
                    )
                })
                .collect(),
        }
    }

    fn map_of_size(size: usize) -> PriceMap {
        let mut models: Vec<(String, f64)> = (0..size.saturating_sub(CANARY_MODELS.len()))
            .map(|index| (format!("filler-model-{index}"), 1e-06))
            .collect();
        models.extend(
            CANARY_MODELS
                .iter()
                .map(|canary| ((*canary).to_owned(), 1e-06)),
        );
        let borrowed: Vec<(&str, f64)> = models
            .iter()
            .map(|(model, input)| (model.as_str(), *input))
            .collect();
        map(&borrowed)
    }

    #[test]
    fn parsing_reads_the_curated_price_map() {
        let parsed = parse(CURATED.as_bytes()).expect("curated sample should parse");
        assert_eq!(parsed.model_count(), 2);

        let sonnet = parsed.price("claude-sonnet-4-5").expect("present");
        assert_eq!(sonnet.input, 3e-06);
        assert_eq!(sonnet.output, 1.5e-05);
        assert_eq!(sonnet.cache_read, Some(3e-07));
        assert_eq!(sonnet.cache_write, Some(3.75e-06));

        let tts = parsed.price("gpt-4o-mini-tts").expect("present");
        assert_eq!(tts.input, 6e-07);
        assert_eq!(tts.cache_read, None);
        assert_eq!(tts.cache_write, None);
    }

    #[test]
    fn an_invalid_curated_price_map_is_rejected() {
        assert!(parse(b"[1, 2, 3]").is_err());
        assert!(parse(b"{\"model\": {\"input\": 0.1}}").is_err());
        assert!(parse(b"not json at all").is_err());
    }

    #[test]
    fn a_map_with_too_few_models_is_rejected_whole() {
        let candidate = map_of_size(MINIMUM_PRICED_MODELS - 1);
        let current = map_of_size(MINIMUM_PRICED_MODELS);
        let reason = adoption_rejection(&candidate, &current).expect("should be rejected");
        assert!(reason.contains("fewer than"), "{reason}");
    }

    #[test]
    fn a_map_that_lost_more_than_half_its_models_is_rejected_whole() {
        let candidate = map_of_size(60);
        let current = map_of_size(200);
        let reason = adoption_rejection(&candidate, &current).expect("should be rejected");
        assert!(reason.contains("shrank"), "{reason}");
    }

    #[test]
    fn a_map_missing_a_canary_model_is_rejected_whole() {
        let mut candidate = map_of_size(80);
        candidate.models.remove("gpt-5-codex");
        let current = map_of_size(80);
        let reason = adoption_rejection(&candidate, &current).expect("should be rejected");
        assert!(reason.contains("gpt-5-codex"), "{reason}");
    }

    #[test]
    fn the_vendored_map_passes_the_adoption_gate() {
        let vendored = PriceMap::vendored();
        assert_eq!(adoption_rejection(&vendored, &vendored), None);
    }
}
