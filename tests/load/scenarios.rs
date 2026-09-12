use std::time::Duration;

use anyhow::{Context, Result};
use axum::body::Bytes;
use futures_util::future::join_all;
use serde_json::Value;

use crate::model::ScenarioInputs;
use crate::payload::{Protocol, SemanticSpec, generate};
use crate::shared::gateway::{Gateway, RequestEncoding, Response, distinct_secrets};

const KIB: usize = 1024;
const MIB: usize = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_MINIMUM_SAMPLES: u64 = 12;

const ANTHROPIC_TYPICAL: SemanticSpec = SemanticSpec {
    protocol: Protocol::Anthropic,
    request_bytes: 384 * KIB,
    parts: 320,
    response_bytes: 4 * KIB,
};
const ANTHROPIC_DENSE: SemanticSpec = SemanticSpec {
    protocol: Protocol::Anthropic,
    request_bytes: 1536 * KIB,
    parts: 2_800,
    response_bytes: 48 * KIB,
};
const ANTHROPIC_LARGE: SemanticSpec = SemanticSpec {
    protocol: Protocol::Anthropic,
    request_bytes: 4 * MIB,
    parts: 800,
    response_bytes: 144 * KIB,
};
const ANTHROPIC_MAX: SemanticSpec = SemanticSpec {
    protocol: Protocol::Anthropic,
    request_bytes: 7 * MIB,
    parts: 1_100,
    response_bytes: 1536 * KIB,
};
const OPENAI_TYPICAL: SemanticSpec = SemanticSpec {
    protocol: Protocol::OpenAi,
    request_bytes: 144 * KIB,
    parts: 104,
    response_bytes: 288 * KIB,
};
const OPENAI_HEAVY: SemanticSpec = SemanticSpec {
    protocol: Protocol::OpenAi,
    request_bytes: 864 * KIB,
    parts: 824,
    response_bytes: 512 * KIB,
};
const OPENAI_MAX: SemanticSpec = SemanticSpec {
    protocol: Protocol::OpenAi,
    request_bytes: 1280 * KIB,
    parts: 700,
    response_bytes: 2304 * KIB,
};

#[derive(Clone, Copy, Debug)]
pub enum Workload {
    Semantic(SemanticSpec),
    MixedOverlap {
        concurrency: usize,
    },
    Cancellation {
        abandon_after_bytes: usize,
        concurrency: usize,
        stall_after_chunks: Option<usize>,
    },
    Masking {
        matches: usize,
    },
    SseStress {
        usage: bool,
        truncated: bool,
    },
    Soak,
}

#[derive(Clone, Copy, Debug)]
pub struct Scenario {
    pub suite: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub workload: Workload,
}

impl Scenario {
    pub fn key(self) -> String {
        format!("{}::{}", self.suite, self.name)
    }

    pub fn expected_requests(self) -> usize {
        match self.workload {
            Workload::Semantic(_) | Workload::SseStress { .. } => 1,
            Workload::MixedOverlap { concurrency } => concurrency,
            Workload::Cancellation { concurrency, .. } => concurrency,
            Workload::Masking { .. } => 2,
            Workload::Soak => 32,
        }
    }

    pub fn expected_disconnected(self) -> usize {
        match self.workload {
            Workload::Cancellation { concurrency, .. } => concurrency,
            _ => 0,
        }
    }

    pub fn timeout(self) -> Duration {
        match self.workload {
            Workload::Soak | Workload::MixedOverlap { concurrency: 192 } => {
                Duration::from_secs(300)
            }
            _ => Duration::from_secs(120),
        }
    }

    pub fn inputs(self) -> ScenarioInputs {
        let (protocol, request, parts, response) = match self.workload {
            Workload::Semantic(spec) => (
                spec.protocol.label(),
                Some(spec.request_bytes),
                Some(spec.parts),
                Some(spec.response_bytes),
            ),
            Workload::MixedOverlap { .. } => ("anthropic+openai", None, None, None),
            Workload::Cancellation {
                abandon_after_bytes,
                ..
            } => (
                Protocol::OpenAi.label(),
                Some(OPENAI_HEAVY.request_bytes),
                Some(OPENAI_HEAVY.parts),
                Some(abandon_after_bytes + 512 * KIB),
            ),
            Workload::Masking { .. } => ("anthropic+openai", None, None, Some(4 * KIB)),
            Workload::SseStress { .. } => (
                Protocol::Anthropic.label(),
                Some(ANTHROPIC_TYPICAL.request_bytes),
                Some(ANTHROPIC_TYPICAL.parts),
                Some(512 * KIB),
            ),
            Workload::Soak => (
                Protocol::Anthropic.label(),
                Some(ANTHROPIC_DENSE.request_bytes),
                Some(ANTHROPIC_DENSE.parts),
                Some(ANTHROPIC_DENSE.response_bytes),
            ),
        };
        let concurrency = match self.workload {
            Workload::MixedOverlap { concurrency } | Workload::Cancellation { concurrency, .. } => {
                concurrency
            }
            _ => 1,
        };
        ScenarioInputs {
            protocol: protocol.to_string(),
            target_request_bytes: request,
            semantic_parts: parts,
            target_response_bytes: response,
            request_count: self.expected_requests(),
            concurrency,
            abandon_after_bytes: match self.workload {
                Workload::Cancellation {
                    abandon_after_bytes,
                    ..
                } => Some(abandon_after_bytes),
                _ => None,
            },
            mask_matches: match self.workload {
                Workload::Masking { matches } => Some(matches),
                _ => None,
            },
            sse_frames: match self.workload {
                Workload::SseStress { .. } => Some(2_048),
                _ => None,
            },
            usage_expected: match self.workload {
                Workload::SseStress {
                    usage,
                    truncated: false,
                } => Some(usage),
                Workload::Cancellation { .. }
                | Workload::SseStress {
                    truncated: true, ..
                } => None,
                _ => Some(true),
            },
            minimum_samples: match self.workload {
                Workload::MixedOverlap { .. } | Workload::Soak => 20,
                _ => DEFAULT_MINIMUM_SAMPLES,
            },
        }
    }
}

#[derive(Debug, Default)]
pub struct WorkloadResult {
    pub statuses: Vec<Option<u16>>,
    pub errors: Vec<String>,
    pub request_bytes: Vec<usize>,
    pub response_bytes: Vec<usize>,
    pub correctness_failures: Vec<String>,
    pub stream_cleanup_verified: Option<bool>,
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        suite: "steady",
        name: "anthropic-typical",
        description: "typical Anthropic semantic request",
        workload: Workload::Semantic(ANTHROPIC_TYPICAL),
    },
    Scenario {
        suite: "steady",
        name: "openai-typical",
        description: "typical OpenAI semantic request with SSE response",
        workload: Workload::Semantic(OPENAI_TYPICAL),
    },
    Scenario {
        suite: "steady",
        name: "masking-4-matches",
        description: "four mutable secrets with signed fields preserved",
        workload: Workload::Masking { matches: 4 },
    },
    Scenario {
        suite: "scale",
        name: "anthropic-dense",
        description: "dense Anthropic semantic request",
        workload: Workload::Semantic(ANTHROPIC_DENSE),
    },
    Scenario {
        suite: "scale",
        name: "anthropic-large",
        description: "large Anthropic semantic request",
        workload: Workload::Semantic(ANTHROPIC_LARGE),
    },
    Scenario {
        suite: "scale",
        name: "anthropic-max",
        description: "observed maximum Anthropic semantic request",
        workload: Workload::Semantic(ANTHROPIC_MAX),
    },
    Scenario {
        suite: "scale",
        name: "openai-heavy",
        description: "heavy OpenAI semantic request with SSE response",
        workload: Workload::Semantic(OPENAI_HEAVY),
    },
    Scenario {
        suite: "scale",
        name: "openai-max",
        description: "observed maximum OpenAI semantic request with SSE response",
        workload: Workload::Semantic(OPENAI_MAX),
    },
    Scenario {
        suite: "scale",
        name: "overlap-32-representative",
        description: "32 production-mix requests accumulated at eight arrivals per second",
        workload: Workload::MixedOverlap { concurrency: 32 },
    },
    Scenario {
        suite: "scale",
        name: "overlap-96-tail",
        description: "96 production-mix requests accumulated at eight arrivals per second",
        workload: Workload::MixedOverlap { concurrency: 96 },
    },
    Scenario {
        suite: "scale",
        name: "overlap-192-observed-max",
        description: "192 production-mix requests accumulated at eight arrivals per second",
        workload: Workload::MixedOverlap { concurrency: 192 },
    },
    Scenario {
        suite: "scale",
        name: "masking-20-matches",
        description: "20 mutable secrets with signed fields preserved",
        workload: Workload::Masking { matches: 20 },
    },
    Scenario {
        suite: "scale",
        name: "sse-2048-frames",
        description: "512 KiB SSE response over 2,048 frames with final usage",
        workload: Workload::SseStress {
            usage: true,
            truncated: false,
        },
    },
    Scenario {
        suite: "soak",
        name: "anthropic-dense-sequential-32",
        description: "32 dense requests with stable prefixes and changing tails",
        workload: Workload::Soak,
    },
    Scenario {
        suite: "resilience",
        name: "cancel-8-after-256kib",
        description: "abandon eight OpenAI streams after 256 KiB",
        workload: Workload::Cancellation {
            abandon_after_bytes: 256 * KIB,
            concurrency: 8,
            stall_after_chunks: None,
        },
    },
    Scenario {
        suite: "resilience",
        name: "cancel-32-after-512kib",
        description: "abandon 32 OpenAI streams after 512 KiB",
        workload: Workload::Cancellation {
            abandon_after_bytes: 512 * KIB,
            concurrency: 32,
            stall_after_chunks: None,
        },
    },
    Scenario {
        suite: "resilience",
        name: "cancel-96-after-896kib",
        description: "abandon 96 OpenAI streams after 896 KiB",
        workload: Workload::Cancellation {
            abandon_after_bytes: 896 * KIB,
            concurrency: 96,
            stall_after_chunks: None,
        },
    },
    Scenario {
        suite: "resilience",
        name: "cancel-boundary-after-2mib",
        description: "abandon one OpenAI stream at the observed response boundary",
        workload: Workload::Cancellation {
            abandon_after_bytes: 2 * MIB,
            concurrency: 1,
            stall_after_chunks: None,
        },
    },
    Scenario {
        suite: "resilience",
        name: "cancel-stalled-upstream",
        description: "abandon 16 OpenAI streams after 4 MiB while the upstream is stalled",
        workload: Workload::Cancellation {
            abandon_after_bytes: 4 * MIB,
            concurrency: 16,
            stall_after_chunks: Some(7),
        },
    },
    Scenario {
        suite: "resilience",
        name: "sse-usage-free",
        description: "complete SSE response without a usage frame",
        workload: Workload::SseStress {
            usage: false,
            truncated: false,
        },
    },
    Scenario {
        suite: "resilience",
        name: "sse-truncated",
        description: "upstream transport failure midway through SSE",
        workload: Workload::SseStress {
            usage: false,
            truncated: true,
        },
    },
];

pub fn all() -> &'static [Scenario] {
    SCENARIOS
}

pub fn find(suite: &str, name: &str) -> Option<&'static Scenario> {
    SCENARIOS
        .iter()
        .find(|scenario| scenario.suite == suite && scenario.name == name)
}

pub async fn run(
    gateway: &Gateway,
    upstream: &crate::shared::upstream::Upstream,
    scenario: &Scenario,
) -> Result<WorkloadResult> {
    match scenario.workload {
        Workload::Semantic(spec) => run_semantic(gateway, spec, 0).await,
        Workload::MixedOverlap { concurrency } => {
            run_mixed_overlap(gateway, concurrency, scenario.name).await
        }
        Workload::Cancellation {
            abandon_after_bytes,
            concurrency,
            stall_after_chunks,
        } => {
            run_cancellation(
                gateway,
                upstream,
                abandon_after_bytes,
                concurrency,
                stall_after_chunks,
            )
            .await
        }
        Workload::Masking { matches } => run_masking(gateway, upstream, matches).await,
        Workload::SseStress { usage, truncated } => run_sse_stress(gateway, usage, truncated).await,
        Workload::Soak => run_soak(gateway).await,
    }
}

async fn run_semantic(
    gateway: &Gateway,
    spec: SemanticSpec,
    tail: usize,
) -> Result<WorkloadResult> {
    let payload = generate(spec, tail)?;
    let url = provider_url(gateway, spec, &response_query(spec.response_bytes, 64));
    let response = gateway
        .post(
            &url,
            payload.body,
            RequestEncoding::Identity,
            REQUEST_TIMEOUT,
        )
        .await;
    let mut result = WorkloadResult {
        request_bytes: vec![payload.bytes],
        ..WorkloadResult::default()
    };
    record_response(&mut result, response);
    require_success(&mut result, spec.response_bytes);
    Ok(result)
}

async fn run_mixed_overlap(
    gateway: &Gateway,
    concurrency: usize,
    barrier: &str,
) -> Result<WorkloadResult> {
    const ARRIVAL_INTERVAL: Duration = Duration::from_millis(125);
    let anthropic = generate(ANTHROPIC_TYPICAL, 0)?;
    let openai = generate(OPENAI_TYPICAL, 0)?;
    let anthropic_count = (concurrency * 81 + 50) / 100;
    let mut requests = Vec::with_capacity(concurrency);
    let mut request_bytes = Vec::with_capacity(concurrency);
    for index in 0..concurrency {
        let (spec, payload) = if index < anthropic_count {
            (ANTHROPIC_TYPICAL, &anthropic)
        } else {
            (OPENAI_TYPICAL, &openai)
        };
        let query = format!(
            "{}&barrier={barrier}&participants={concurrency}&release_ms={}",
            response_query(spec.response_bytes, 32),
            index * ARRIVAL_INTERVAL.as_millis() as usize
        );
        requests.push((provider_url(gateway, spec, &query), payload.body.clone()));
        request_bytes.push(payload.bytes);
    }
    let responses = join_all(requests.into_iter().enumerate().map(
        |(index, (url, body))| async move {
            tokio::time::sleep(ARRIVAL_INTERVAL * index as u32).await;
            gateway
                .post(&url, body, RequestEncoding::Identity, REQUEST_TIMEOUT)
                .await
        },
    ))
    .await;
    let mut result = WorkloadResult {
        request_bytes,
        ..WorkloadResult::default()
    };
    for response in responses {
        record_response(&mut result, response);
    }
    require_success(&mut result, 1);
    Ok(result)
}

async fn run_cancellation(
    gateway: &Gateway,
    upstream: &crate::shared::upstream::Upstream,
    abandon_after_bytes: usize,
    concurrency: usize,
    stall_after_chunks: Option<usize>,
) -> Result<WorkloadResult> {
    let payload = generate(OPENAI_HEAVY, abandon_after_bytes)?;
    let (response_bytes, chunks) = if stall_after_chunks.is_some() {
        (abandon_after_bytes + MIB, 8)
    } else {
        (abandon_after_bytes + 512 * KIB, 128)
    };
    let stall = stall_after_chunks
        .map(|chunks| format!("&stall_after={chunks}"))
        .unwrap_or_default();
    let query =
        format!("mode=sse&bytes={response_bytes}&chunks={chunks}&delay_ms=2&usage=1{stall}");
    let url = provider_url(gateway, OPENAI_HEAVY, &query);
    let responses = join_all(
        (0..concurrency)
            .map(|_| gateway.post_and_abandon(&url, payload.body.clone(), abandon_after_bytes)),
    )
    .await;
    let mut result = WorkloadResult {
        request_bytes: vec![payload.bytes; concurrency],
        ..WorkloadResult::default()
    };
    for response in responses {
        let read = response.bytes_read;
        record_response(&mut result, response);
        if read < abandon_after_bytes {
            result.correctness_failures.push(format!(
                "abandoned after {read} bytes, below {abandon_after_bytes} byte target"
            ));
        }
    }
    if result.statuses.iter().any(|status| *status != Some(200)) {
        result.correctness_failures.push(format!(
            "cancellation response statuses were {:?}",
            result.statuses
        ));
    }
    let idle = upstream.wait_until_idle(CLEANUP_TIMEOUT).await;
    result.stream_cleanup_verified = Some(idle);
    if !idle {
        result
            .correctness_failures
            .push("upstream streams remained active after cancellation".into());
    }
    Ok(result)
}

async fn run_masking(
    gateway: &Gateway,
    upstream: &crate::shared::upstream::Upstream,
    matches: usize,
) -> Result<WorkloadResult> {
    let secrets = distinct_secrets(matches);
    let split = matches / 2;
    let (anthropic_spec, openai_spec) = if matches <= 4 {
        (ANTHROPIC_TYPICAL, OPENAI_TYPICAL)
    } else {
        (ANTHROPIC_DENSE, OPENAI_HEAVY)
    };
    let (anthropic, protected_thinking) = masking_body(anthropic_spec, &secrets[..split])?;
    let (openai, protected_encrypted) = masking_body(openai_spec, &secrets[split..])?;
    let bodies = [anthropic, openai];
    let urls = [
        gateway.claude_url("mode=sse&bytes=4096&chunks=8"),
        gateway.provider_url("codex", "/v1/responses", "mode=sse&bytes=4096&chunks=8"),
    ];
    let mut result = WorkloadResult::default();
    for (url, body) in urls.iter().zip(&bodies) {
        result.request_bytes.push(body.len());
        let response = gateway
            .post(
                url,
                body.clone(),
                RequestEncoding::Identity,
                REQUEST_TIMEOUT,
            )
            .await;
        record_response(&mut result, response);
    }
    require_success(&mut result, 4 * KIB);

    let forwarded = upstream.received();
    if forwarded.len() != 2 {
        result.correctness_failures.push(format!(
            "expected two retained masking requests, found {}",
            forwarded.len()
        ));
        return Ok(result);
    }
    for secret in &secrets {
        if forwarded
            .iter()
            .any(|request| request.body.contains(secret))
        {
            result
                .correctness_failures
                .push(format!("mutable secret survived masking: {secret}"));
        }
    }
    let anthropic: Value = serde_json::from_str(&forwarded[0].body)
        .context("parsing forwarded Anthropic masking body")?;
    let openai: Value = serde_json::from_str(&forwarded[1].body)
        .context("parsing forwarded OpenAI masking body")?;
    if anthropic["messages"][1]["content"][0]["thinking"] != protected_thinking {
        result
            .correctness_failures
            .push("signed Anthropic thinking changed during masking".into());
    }
    let encrypted = openai["input"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["type"] == "reasoning"))
        .and_then(|item| item["encrypted_content"].as_str());
    if encrypted != Some(&protected_encrypted) {
        result
            .correctness_failures
            .push("OpenAI encrypted_content changed during masking".into());
    }
    if !forwarded
        .iter()
        .all(|request| request.body.contains("AEGIS_MASKED"))
    {
        result
            .correctness_failures
            .push("mask placeholders were absent from a forwarded request".into());
    }
    Ok(result)
}

fn masking_body(spec: SemanticSpec, secrets: &[String]) -> Result<(Bytes, String)> {
    let generated = generate(spec, 0)?;
    let mut document: Value = serde_json::from_slice(&generated.body)?;
    let protected = match spec.protocol {
        Protocol::Anthropic => {
            let mutable = document["messages"][0]["content"][0]["text"]
                .as_str()
                .context("Anthropic masking text is missing")?;
            let replacement = masking_text(mutable.len(), secrets)?;
            document["messages"][0]["content"][0]["text"] = Value::String(replacement);
            document["messages"][1]["content"][0]["thinking"]
                .as_str()
                .context("Anthropic signed thinking is missing")?
                .to_string()
        }
        Protocol::OpenAi => {
            let items = document["input"]
                .as_array_mut()
                .context("OpenAI input is missing")?;
            let user = items
                .iter_mut()
                .find(|item| item["role"] == "user")
                .context("OpenAI user input is missing")?;
            let mutable = user["content"][0]["text"]
                .as_str()
                .context("OpenAI masking text is missing")?;
            let replacement = masking_text(mutable.len(), secrets)?;
            user["content"][0]["text"] = Value::String(replacement);
            items
                .iter()
                .find(|item| item["type"] == "reasoning")
                .and_then(|item| item["encrypted_content"].as_str())
                .context("OpenAI encrypted content is missing")?
                .to_string()
        }
    };
    let encoded = serde_json::to_vec(&document)?;
    if encoded.len() != spec.request_bytes {
        anyhow::bail!(
            "masking payload changed from {} to {} bytes",
            spec.request_bytes,
            encoded.len()
        );
    }
    Ok((Bytes::from(encoded), protected))
}

fn masking_text(length: usize, secrets: &[String]) -> Result<String> {
    let prefix = secrets.join(" ");
    if prefix.len() > length {
        anyhow::bail!(
            "{} bytes of secrets do not fit in a {length} byte text part",
            prefix.len()
        );
    }
    let mut text = prefix;
    text.push_str(&"m".repeat(length - text.len()));
    Ok(text)
}

async fn run_sse_stress(gateway: &Gateway, usage: bool, truncated: bool) -> Result<WorkloadResult> {
    let payload = generate(ANTHROPIC_TYPICAL, 0)?;
    let query = format!(
        "mode=sse&bytes={}&chunks=2048&usage={}&abort={}",
        512 * KIB,
        usize::from(usage),
        usize::from(truncated)
    );
    let response = gateway
        .post(
            &gateway.claude_url(&query),
            payload.body,
            RequestEncoding::Identity,
            REQUEST_TIMEOUT,
        )
        .await;
    let mut result = WorkloadResult {
        request_bytes: vec![payload.bytes],
        ..WorkloadResult::default()
    };
    let had_error = response.error.is_some();
    record_response(&mut result, response);
    if truncated {
        if !had_error {
            result
                .correctness_failures
                .push("truncated SSE completed without a transport error".into());
        }
        if result.statuses != [Some(200)] {
            result.correctness_failures.push(format!(
                "truncated SSE response status was {:?}",
                result.statuses
            ));
        }
    } else {
        require_success(&mut result, 512 * KIB);
    }
    Ok(result)
}

async fn run_soak(gateway: &Gateway) -> Result<WorkloadResult> {
    let mut result = WorkloadResult::default();
    for tail in 1..=32 {
        let payload = generate(ANTHROPIC_DENSE, tail)?;
        result.request_bytes.push(payload.bytes);
        let response = gateway
            .post(
                &gateway.claude_url(&response_query(ANTHROPIC_DENSE.response_bytes, 32)),
                payload.body,
                RequestEncoding::Identity,
                REQUEST_TIMEOUT,
            )
            .await;
        record_response(&mut result, response);
    }
    require_success(&mut result, ANTHROPIC_DENSE.response_bytes);
    Ok(result)
}

fn record_response(result: &mut WorkloadResult, response: Response) {
    result.statuses.push(response.status);
    result.response_bytes.push(response.bytes_read);
    if let Some(error) = response.error {
        result.errors.push(error);
    }
}

fn require_success(result: &mut WorkloadResult, minimum_bytes: usize) {
    for (index, status) in result.statuses.iter().enumerate() {
        if *status != Some(200) {
            result.correctness_failures.push(format!(
                "response {index} had status {status:?}, expected 200"
            ));
        }
    }
    for (index, bytes) in result.response_bytes.iter().enumerate() {
        if *bytes < minimum_bytes {
            result.correctness_failures.push(format!(
                "response {index} had {bytes} bytes, expected at least {minimum_bytes}"
            ));
        }
    }
    for error in &result.errors {
        result
            .correctness_failures
            .push(format!("request error: {error}"));
    }
}

fn response_query(bytes: usize, chunks: usize) -> String {
    format!("mode=sse&bytes={bytes}&chunks={chunks}&usage=1")
}

fn provider_url(gateway: &Gateway, spec: SemanticSpec, query: &str) -> String {
    match spec.protocol {
        Protocol::Anthropic => gateway.claude_url(query),
        Protocol::OpenAi => gateway.provider_url("codex", "/v1/responses", query),
    }
}
