use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: &str = "aegis.load.v2";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Variant {
    Baseline,
    Candidate,
}

impl Variant {
    pub fn label(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Candidate => "candidate",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ScenarioInputs {
    pub protocol: String,
    pub target_request_bytes: Option<usize>,
    pub semantic_parts: Option<usize>,
    pub target_response_bytes: Option<usize>,
    pub request_count: usize,
    pub concurrency: usize,
    pub abandon_after_bytes: Option<usize>,
    pub mask_matches: Option<usize>,
    pub sse_frames: Option<usize>,
    pub usage_expected: Option<bool>,
    pub minimum_samples: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct MemoryMetrics {
    pub ready_rss_kib: Option<u64>,
    pub ready_stabilization_samples: u64,
    pub ready_stabilization_spread_kib: Option<u64>,
    pub sampled_workload_peak_kib: Option<u64>,
    pub peak_growth_kib: Option<i64>,
    pub settled_rss_kib: Option<u64>,
    pub settled_growth_kib: Option<i64>,
    pub settled_stabilization_samples: u64,
    pub settled_stabilization_spread_kib: Option<u64>,
    pub duration_ms: Option<u64>,
    pub sample_count: u64,
    pub vm_hwm_kib: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Observation {
    pub schema_version: String,
    pub suite: String,
    pub scenario: String,
    pub variant: Variant,
    pub pair: u8,
    pub position: u8,
    pub inputs: ScenarioInputs,
    pub actual_request_bytes: Vec<usize>,
    pub actual_response_bytes: Vec<usize>,
    pub valid: bool,
    pub invalid_reasons: Vec<String>,
    pub metrics: MemoryMetrics,
    pub response_statuses: Vec<u16>,
    pub upstream_requests: usize,
    pub telemetry_completed: usize,
    pub telemetry_disconnected: usize,
    pub stream_cleanup_verified: Option<bool>,
    pub child_reaped: bool,
    pub diagnostic: Option<String>,
}

impl Observation {
    pub fn new(
        suite: impl Into<String>,
        scenario: impl Into<String>,
        variant: Variant,
        pair: u8,
        position: u8,
        inputs: ScenarioInputs,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            suite: bounded(suite.into(), 128),
            scenario: bounded(scenario.into(), 128),
            variant,
            pair,
            position,
            inputs,
            actual_request_bytes: Vec::new(),
            actual_response_bytes: Vec::new(),
            valid: true,
            invalid_reasons: Vec::new(),
            metrics: MemoryMetrics::default(),
            response_statuses: Vec::new(),
            upstream_requests: 0,
            telemetry_completed: 0,
            telemetry_disconnected: 0,
            stream_cleanup_verified: None,
            child_reaped: false,
            diagnostic: None,
        }
    }

    pub fn invalidate(&mut self, reason: impl Into<String>) {
        self.valid = false;
        if self.invalid_reasons.len() < 20 {
            self.invalid_reasons.push(bounded(reason.into(), 512));
        }
    }

    pub fn set_diagnostic(&mut self, diagnostic: impl Into<String>) {
        self.diagnostic = Some(bounded(diagnostic.into(), 4096));
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct BinaryMetadata {
    pub path: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct HostMetadata {
    pub hostname: String,
    pub kernel_release: String,
    pub architecture: String,
    pub operating_system: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DeltaMetrics {
    pub ready_rss_kib: Option<i64>,
    pub sampled_workload_peak_kib: Option<i64>,
    pub peak_growth_kib: Option<i64>,
    pub settled_rss_kib: Option<i64>,
    pub settled_growth_kib: Option<i64>,
    pub duration_ms: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PairedDelta {
    pub suite: String,
    pub scenario: String,
    pub pair: u8,
    pub valid: bool,
    pub baseline_position: u8,
    pub candidate_position: u8,
    pub candidate_minus_baseline: DeltaMetrics,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScenarioSummary {
    pub suite: String,
    pub scenario: String,
    pub inputs: ScenarioInputs,
    pub valid_pairs: usize,
    pub total_pairs: usize,
    pub median_paired_delta: DeltaMetrics,
    pub paired_delta_spread: DeltaMetrics,
}

#[derive(Debug, Serialize)]
pub struct RunReport {
    pub schema_version: &'static str,
    pub baseline: BinaryMetadata,
    pub candidate: BinaryMetadata,
    pub host: HostMetadata,
    pub sample_interval_ms: u64,
    pub pairs_per_scenario: u8,
    pub observations: Vec<Observation>,
    pub paired_deltas: Vec<PairedDelta>,
    pub summaries: Vec<ScenarioSummary>,
}

fn bounded(mut value: String, limit: usize) -> String {
    if value.len() <= limit {
        return value;
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value.push_str("...[truncated]");
    value
}
