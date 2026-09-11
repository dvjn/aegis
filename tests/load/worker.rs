use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::model::{Observation, ScenarioInputs, Variant};
use crate::scenarios::{self, Scenario};
use crate::shared::gateway::{Gateway, GatewayOptions};
use crate::shared::measure::{RssSampler, stabilize_rss};
use crate::shared::telemetry::{TelemetryState, wait_for_background_jobs, wait_for_telemetry};
use crate::shared::upstream::Upstream;

const TELEMETRY_TIMEOUT: Duration = Duration::from_secs(20);
const READY_STABILITY_TIMEOUT: Duration = Duration::from_secs(15);
const SETTLED_STABILITY_TIMEOUT: Duration = Duration::from_secs(20);
const SAMPLE_FLOOR_TIMEOUT: Duration = Duration::from_secs(2);
const READY_WINDOW: usize = 30;
const SETTLED_WINDOW: usize = 50;
const STABILITY_TOLERANCE_KIB: u64 = 512;
const STARTUP_JOB_COUNT: usize = 5;

pub async fn run(
    binary: &Path,
    suite: &str,
    scenario_name: &str,
    variant: Variant,
    pair: u8,
    position: u8,
) -> Observation {
    let Some(scenario) = scenarios::find(suite, scenario_name) else {
        let mut observation = Observation::new(
            suite,
            scenario_name,
            variant,
            pair,
            position,
            unknown_inputs(),
        );
        observation.invalidate("scenario is not registered");
        return observation;
    };
    let mut observation = Observation::new(
        suite,
        scenario_name,
        variant,
        pair,
        position,
        scenario.inputs(),
    );
    let upstream = Upstream::start().await;
    let gateway = match Gateway::start_with_binary(binary, &upstream, GatewayOptions::default()) {
        Ok(gateway) => gateway,
        Err(error) => {
            observation.invalidate(format!("starting {}: {error:#}", binary.display()));
            return observation;
        }
    };

    if let Err(error) = observe(&gateway, &upstream, scenario, &mut observation).await {
        observation.invalidate(format!("worker error: {error:#}"));
    }
    if !observation.valid {
        observation.set_diagnostic(gateway.log_tail(40));
    }
    let shutdown = gateway.shut_down();
    observation.child_reaped = shutdown.child_reaped;
    if !shutdown.child_reaped {
        observation.invalidate(format!(
            "Aegis child {} was not confirmed reaped, exit status {:?}",
            gateway.pid(),
            shutdown.exit_status
        ));
    }
    observation
}

async fn observe(
    gateway: &Gateway,
    upstream: &Upstream,
    scenario: &Scenario,
    observation: &mut Observation,
) -> Result<()> {
    if !wait_for_background_jobs(
        &gateway.database_path(),
        STARTUP_JOB_COUNT,
        READY_STABILITY_TIMEOUT,
    )
    .await
    {
        bail!(
            "{STARTUP_JOB_COUNT} startup jobs did not complete within {READY_STABILITY_TIMEOUT:?}"
        );
    }
    let warm = warm_up(gateway, upstream).await?;
    let ready = stabilize_rss(
        gateway.pid(),
        READY_STABILITY_TIMEOUT,
        READY_WINDOW,
        STABILITY_TOLERANCE_KIB,
    )
    .await
    .context("stabilizing ready RSS")?;
    observation.metrics.ready_rss_kib = Some(ready.rss_kib);
    observation.metrics.ready_stabilization_samples = ready.sample_count;
    observation.metrics.ready_stabilization_spread_kib = Some(ready.spread_kib);
    if !ready.stable {
        observation.invalidate(format!(
            "ready RSS did not stabilize within {READY_STABILITY_TIMEOUT:?}; last spread {} KiB",
            ready.spread_kib
        ));
    }

    let sampler = RssSampler::start(gateway.pid(), ready.rss_kib);
    let started = Instant::now();
    let result = tokio::time::timeout(
        scenario.timeout(),
        scenarios::run(gateway, upstream, scenario),
    )
    .await
    .map_err(|_| anyhow!("workload exceeded {:?}", scenario.timeout()))??;

    let expected_completed = warm.completed + scenario.expected_requests();
    let expected_disconnected = warm.disconnected + scenario.expected_disconnected();
    let telemetry = wait_for_telemetry(
        &gateway.database_path(),
        expected_completed,
        expected_disconnected,
        TELEMETRY_TIMEOUT,
    )
    .await;
    observation.telemetry_completed = telemetry.completed.saturating_sub(warm.completed);
    observation.telemetry_disconnected = telemetry.disconnected.saturating_sub(warm.disconnected);
    validate_telemetry(scenario, warm, telemetry, observation);

    let enough_samples = sampler
        .wait_for_samples(observation.inputs.minimum_samples, SAMPLE_FLOOR_TIMEOUT)
        .await;
    observation.metrics.duration_ms = Some(elapsed_millis(started));
    let samples = sampler.stop();
    observation.metrics.sampled_workload_peak_kib = samples.peak_kib;
    observation.metrics.sample_count = samples.sample_count;
    if !enough_samples || samples.sample_count < observation.inputs.minimum_samples {
        observation.invalidate(format!(
            "workload produced {} RSS samples, requires at least {}",
            samples.sample_count, observation.inputs.minimum_samples
        ));
    }
    if samples.read_failures > 0 {
        observation.invalidate(format!(
            "RSS sampler failed {} /proc reads",
            samples.read_failures
        ));
    }

    observation.actual_request_bytes = result.request_bytes.into_iter().take(256).collect();
    observation.actual_response_bytes = result.response_bytes.into_iter().take(256).collect();
    observation.response_statuses = result.statuses.into_iter().flatten().take(256).collect();
    observation.stream_cleanup_verified = result.stream_cleanup_verified;
    for failure in result.correctness_failures {
        observation.invalidate(failure);
    }
    validate_payload_sizes(observation);

    observation.upstream_requests = upstream.received_count();
    if observation.upstream_requests != scenario.expected_requests() {
        observation.invalidate(format!(
            "expected {} upstream requests, observed {}",
            scenario.expected_requests(),
            observation.upstream_requests
        ));
    }
    if let Some(status) = gateway.exit_status() {
        observation.invalidate(format!("Aegis exited before teardown with status {status}"));
    }

    let settled = stabilize_rss(
        gateway.pid(),
        SETTLED_STABILITY_TIMEOUT,
        SETTLED_WINDOW,
        STABILITY_TOLERANCE_KIB,
    )
    .await
    .context("stabilizing settled RSS")?;
    observation.metrics.settled_rss_kib = Some(settled.rss_kib);
    observation.metrics.settled_stabilization_samples = settled.sample_count;
    observation.metrics.settled_stabilization_spread_kib = Some(settled.spread_kib);
    observation.metrics.vm_hwm_kib = Some(settled.vm_hwm_kib.max(ready.vm_hwm_kib));
    if !settled.stable {
        observation.invalidate(format!(
            "settled RSS did not stabilize within {SETTLED_STABILITY_TIMEOUT:?}; last spread {} KiB",
            settled.spread_kib
        ));
    }
    finish_metrics(observation);
    Ok(())
}

async fn warm_up(gateway: &Gateway, upstream: &Upstream) -> Result<TelemetryState> {
    let response = gateway
        .post_text(
            &gateway.claude_url("mode=sse&bytes=4096&chunks=8&usage=1"),
            gateway.text_body(false),
        )
        .await;
    if response.status != Some(200) || response.error.is_some() || response.bytes_read < 4096 {
        bail!(
            "warm-up failed: status {:?}, bytes {}, error {:?}",
            response.status,
            response.bytes_read,
            response.error
        );
    }
    let telemetry = wait_for_telemetry(&gateway.database_path(), 1, 0, TELEMETRY_TIMEOUT).await;
    if telemetry.completed < 1 || telemetry.usage_rows < 1 {
        bail!("warm-up telemetry did not complete: {telemetry:?}");
    }
    if !upstream.wait_until_idle(Duration::from_secs(5)).await {
        bail!("warm-up upstream stream remained active");
    }
    upstream.reset();
    Ok(telemetry)
}

fn validate_telemetry(
    scenario: &Scenario,
    warm: TelemetryState,
    telemetry: TelemetryState,
    observation: &mut Observation,
) {
    if observation.telemetry_completed < scenario.expected_requests() {
        observation.invalidate(format!(
            "telemetry completed {} of {} workload requests within {TELEMETRY_TIMEOUT:?}",
            observation.telemetry_completed,
            scenario.expected_requests()
        ));
    }
    if observation.telemetry_disconnected < scenario.expected_disconnected() {
        observation.invalidate(format!(
            "telemetry recorded {} disconnected requests, expected at least {}",
            observation.telemetry_disconnected,
            scenario.expected_disconnected()
        ));
    }
    match observation.inputs.usage_expected {
        Some(true) => {
            let usage = telemetry.usage_rows.saturating_sub(warm.usage_rows);
            if usage < scenario.expected_requests() {
                observation.invalidate(format!(
                    "telemetry recorded {usage} usage rows for {} requests",
                    scenario.expected_requests()
                ));
            }
        }
        Some(false) if telemetry.usage_rows != warm.usage_rows => observation.invalidate(format!(
            "usage-free workload added {} usage rows",
            telemetry.usage_rows.saturating_sub(warm.usage_rows)
        )),
        _ => {}
    }
}

fn validate_payload_sizes(observation: &mut Observation) {
    if observation.actual_request_bytes.len() != observation.inputs.request_count {
        observation.invalidate(format!(
            "recorded {} request sizes for {} requests",
            observation.actual_request_bytes.len(),
            observation.inputs.request_count
        ));
    }
    if let Some(target) = observation.inputs.target_request_bytes
        && observation
            .actual_request_bytes
            .iter()
            .any(|bytes| *bytes != target)
    {
        observation.invalidate(format!(
            "generated request size differed from {target} byte target"
        ));
    }
    if observation.actual_response_bytes.len() != observation.inputs.request_count {
        observation.invalidate(format!(
            "recorded {} response sizes for {} requests",
            observation.actual_response_bytes.len(),
            observation.inputs.request_count
        ));
    }
}

fn finish_metrics(observation: &mut Observation) {
    if let (Some(ready), Some(peak)) = (
        observation.metrics.ready_rss_kib,
        observation.metrics.sampled_workload_peak_kib,
    ) {
        observation.metrics.peak_growth_kib = Some(peak as i64 - ready as i64);
    }
    if let (Some(ready), Some(settled)) = (
        observation.metrics.ready_rss_kib,
        observation.metrics.settled_rss_kib,
    ) {
        observation.metrics.settled_growth_kib = Some(settled as i64 - ready as i64);
    }

    let required = [
        observation.metrics.ready_rss_kib,
        observation.metrics.sampled_workload_peak_kib,
        observation.metrics.settled_rss_kib,
        observation.metrics.vm_hwm_kib,
    ];
    if required.iter().any(Option::is_none) {
        observation.invalidate("one or more required RSS metrics are missing");
    }
}

fn unknown_inputs() -> ScenarioInputs {
    ScenarioInputs {
        protocol: "unknown".into(),
        target_request_bytes: None,
        semantic_parts: None,
        target_response_bytes: None,
        request_count: 0,
        concurrency: 0,
        abandon_after_bytes: None,
        mask_matches: None,
        sse_frames: None,
        usage_expected: None,
        minimum_samples: 0,
    }
}

fn elapsed_millis(started: Instant) -> u64 {
    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}
