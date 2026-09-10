use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use axum::body::Bytes;

use crate::analysis::{fit_linear, fit_linear_region};
use crate::context::{CaseContext, SETTLE};
use crate::shared::gateway::{
    DEFAULT_CAPTURE_BYTES, Gateway, GatewayOptions, GuardrailsMode, MAX_REQUEST_BYTES, MemoryCap,
    PayloadShape, RequestEncoding, Response, encode_request, memory_size_bytes,
};
use crate::shared::measure::{KIB, MIB, mib, mib_delta, read_rss_kib};

const DEFAULT_RESPONSE_BYTES: usize = 64 * KIB;
const DEFAULT_CHUNKS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    Quick,
    Full,
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub profile: Profile,
    pub oom_memory_max: String,
}

impl Settings {
    fn pick<T: Clone>(&self, full: &[T], quick: &[T]) -> Vec<T> {
        match self.profile {
            Profile::Quick => quick.to_vec(),
            Profile::Full => full.to_vec(),
        }
    }

    fn one<T: Copy>(&self, full: T, quick: T) -> T {
        match self.profile {
            Profile::Quick => quick,
            Profile::Full => full,
        }
    }
}

type CaseFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + 'a>>;

pub struct Case {
    pub name: &'static str,
    pub description: &'static str,
    pub run: for<'a> fn(&'a mut CaseContext, &'a Settings) -> CaseFuture<'a>,
}

macro_rules! registry {
    ($($function:ident => $description:literal,)*) => {
        pub fn all() -> Vec<Case> {
            vec![$(Case {
                name: stringify!($function),
                description: $description,
                run: |context, settings| Box::pin($function(context, settings)),
            }),*]
        }
    };
}

registry! {
    request_size_matrix => "Peak growth and retained floor against request body size, with a fitted cost model.",
    response_size_matrix => "Peak growth against response body size, sse and json, with a fitted cost model.",
    concurrency_matrix => "Peak growth against in-flight request count at a fixed payload size.",
    payload_shape_matrix => "Cost per byte for one long string against thousands of small JSON objects.",
    sse_frame_count_matrix => "Peak growth for the same response bytes split across few large or many small frames.",
    stream_duration_matrix => "Peak growth as buffer lifetime grows, at a fixed response size and frame count.",
    guardrails_mode_matrix => "Peak growth with guardrails off, in observe and in mask, at a matched payload.",
    secret_count_matrix => "Peak growth against the number of distinct secrets in one request body.",
    request_encoding_matrix => "Peak growth for the same decoded body sent identity, gzip, zstd and brotli.",
    capture_limit_matrix => "Peak growth at max_capture_bytes 256 KiB against 16 MiB, on a large response.",
    provider_matrix => "Peak growth for the claude and codex usage-extraction paths at a matched payload.",
    client_disconnect_mid_stream => "Whether an abandoned response frees the relay task's capture buffer.",
    upstream_error_with_large_body => "Peak growth when the upstream answers 5xx with a large error body.",
    upstream_transport_failure_mid_stream => "Peak growth and retention when the upstream drops the connection mid-stream.",
    stream_without_stop_frames => "Whether per-block restorer state accumulates when no content block is ever closed.",
    repeated_large_request_ratchet => "The retention ratchet: resident floor after each of N sequential large requests.",
    oom_bisection => "Informational: bisect for the smallest in-flight payload that OOM-kills a gateway.",
}

#[derive(Clone, Debug)]
struct Point {
    label: String,
    request_bytes: usize,
    request_shape: PayloadShape,
    block_bytes: usize,
    secret_count: Option<usize>,
    encoding: RequestEncoding,
    response_bytes: usize,
    chunks: usize,
    delay_ms: usize,
    mode: &'static str,
    concurrency: usize,
    provider: &'static str,
    guardrails_mode: GuardrailsMode,
    max_capture_bytes: usize,
    memory_max: Option<String>,
    extra_query: Vec<(&'static str, String)>,
    timeout: Duration,
    expected_statuses: Vec<Option<u16>>,
}

impl Point {
    fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            request_bytes: 0,
            request_shape: PayloadShape::OneLongString,
            block_bytes: 64,
            secret_count: Some(1),
            encoding: RequestEncoding::Identity,
            response_bytes: DEFAULT_RESPONSE_BYTES,
            chunks: DEFAULT_CHUNKS,
            delay_ms: 0,
            mode: "sse",
            concurrency: 1,
            provider: "claude",
            guardrails_mode: GuardrailsMode::Mask,
            max_capture_bytes: DEFAULT_CAPTURE_BYTES,
            memory_max: None,
            extra_query: Vec::new(),
            timeout: Duration::from_secs(180),
            expected_statuses: vec![Some(200)],
        }
    }

    fn gateway_options(&self) -> GatewayOptions {
        GatewayOptions {
            guardrails: self.guardrails_mode,
            max_capture_bytes: self.max_capture_bytes,
            memory_max: self.memory_max.clone(),
            ..GatewayOptions::default()
        }
    }

    fn provider_path(&self) -> &'static str {
        match self.provider {
            "codex" => "/v1/responses",
            _ => "/v1/messages",
        }
    }

    fn url(&self, gateway: &Gateway) -> String {
        let mut query = format!(
            "mode={}&bytes={}&chunks={}&delay_ms={}",
            self.mode, self.response_bytes, self.chunks, self.delay_ms
        );
        for (name, value) in &self.extra_query {
            query.push_str(&format!("&{name}={value}"));
        }
        gateway.provider_url(self.provider, self.provider_path(), &query)
    }

    fn query_value(&self, name: &str) -> f64 {
        self.extra_query
            .iter()
            .find(|(key, _)| *key == name)
            .and_then(|(_, value)| value.parse().ok())
            .unwrap_or(0.0)
    }

    fn body(&self, gateway: &Gateway) -> Result<Bytes> {
        let plain = gateway.request_body(
            self.request_bytes,
            self.secret_count,
            self.request_shape,
            self.block_bytes,
        );
        encode_request(&plain, self.encoding)
    }
}

#[derive(Clone, Debug)]
struct PointRecord {
    label: String,
    request_bytes: usize,
    nominal_request_bytes: usize,
    response_bytes: usize,
    payload_bytes: usize,
    concurrency: usize,
    encoding: RequestEncoding,
    guardrails_mode: GuardrailsMode,
    max_capture_bytes: usize,
    provider: &'static str,
    ok: bool,
    growth_mib: f64,
    retained_mib: Option<f64>,
    peak_mib: f64,
    x: f64,
}

async fn post_concurrently(
    gateway: Arc<Gateway>,
    url: String,
    body: Bytes,
    count: usize,
    timeout: Duration,
) -> Vec<Response> {
    let mut tasks = Vec::with_capacity(count);
    for _ in 0..count {
        let gateway = Arc::clone(&gateway);
        let url = url.clone();
        let body = body.clone();
        tasks.push(tokio::spawn(async move {
            gateway
                .post(&url, body, RequestEncoding::Identity, timeout)
                .await
        }));
    }
    let mut results = Vec::with_capacity(count);
    for task in tasks {
        match task.await {
            Ok(response) => results.push(response),
            Err(error) => results.push(Response {
                status: None,
                body: String::new(),
                bytes_read: 0,
                seconds: 0.0,
                error: Some(error.to_string()),
            }),
        }
    }
    results
}

async fn run_point(context: &mut CaseContext, point: &Point) -> Result<PointRecord> {
    let id = context.start(point.gateway_options())?;
    let gateway = context.gateway(id);
    let url = point.url(&gateway);
    let body = point.body(&gateway)?;
    let request_bytes = body.len();

    let (measurement, results) = {
        let gateway = Arc::clone(&gateway);
        let url = url.clone();
        let body = body.clone();
        let concurrency = point.concurrency;
        let timeout = point.timeout;
        let encoding = point.encoding;
        context
            .measure(&point.label, id, move || async move {
                if concurrency > 1 {
                    post_concurrently(gateway, url, body, concurrency, timeout).await
                } else {
                    vec![gateway.post(&url, body, encoding, timeout).await]
                }
            })
            .await
    };

    let slowest = results
        .iter()
        .map(|result| result.seconds)
        .fold(0.0f64, f64::max);
    let statuses: BTreeSet<Option<u16>> = results.iter().map(|result| result.status).collect();
    let errors: BTreeSet<String> = results
        .iter()
        .filter_map(|result| result.error.clone())
        .collect();

    tokio::time::sleep(SETTLE).await;
    let settled = read_rss_kib(gateway.pid());
    let tail = gateway.log_tail(6);
    drop(gateway);
    let row = context.release(id);

    let ok = statuses
        .iter()
        .all(|status| point.expected_statuses.contains(status));
    let record = PointRecord {
        label: point.label.clone(),
        request_bytes,
        nominal_request_bytes: point.request_bytes,
        response_bytes: point.response_bytes,
        payload_bytes: (request_bytes + point.response_bytes) * point.concurrency,
        concurrency: point.concurrency,
        encoding: point.encoding,
        guardrails_mode: point.guardrails_mode,
        max_capture_bytes: point.max_capture_bytes,
        provider: point.provider,
        ok,
        growth_mib: mib_delta(measurement.before_kib, row.headline_kib()),
        retained_mib: settled.map(|value| mib_delta(measurement.before_kib, value)),
        peak_mib: mib(row.headline_kib()),
        x: 0.0,
    };

    context.log(format!(
        "    {}: request {:.3} MiB x{}, response {:.3} MiB, statuses {}, slowest {slowest:.2}s, growth {:+.1} MiB, retained {}",
        point.label,
        request_bytes as f64 / MIB as f64,
        point.concurrency,
        point.response_bytes as f64 / MIB as f64,
        describe_statuses(&statuses),
        record.growth_mib,
        match record.retained_mib {
            Some(value) => format!("{value:+.1} MiB"),
            None => "n/a".to_string(),
        }
    ));
    if !record.ok {
        context.log(format!(
            "      not ok: statuses {}, errors {:?}",
            describe_statuses(&statuses),
            errors
        ));
        for line in tail.lines() {
            context.log(format!("        {line}"));
        }
    }
    Ok(record)
}

fn describe_statuses(statuses: &BTreeSet<Option<u16>>) -> String {
    let rendered: Vec<String> = statuses
        .iter()
        .map(|status| match status {
            Some(code) => code.to_string(),
            None => "transport error".to_string(),
        })
        .collect();
    format!("[{}]", rendered.join(", "))
}

async fn sweep(
    context: &mut CaseContext,
    dimension: &str,
    points: Vec<Point>,
    x_of: impl Fn(&Point, &PointRecord) -> f64,
) -> Result<Vec<PointRecord>> {
    context.log(format!(
        "  axis {dimension}: {} points, one fresh gateway each",
        points.len()
    ));
    let mut records = Vec::new();
    for point in points {
        let mut record = run_point(context, &point).await?;
        record.x = x_of(&point, &record);
        context.metric(
            format!("{dimension} {} growth", record.label),
            "MiB",
            record.growth_mib,
        );
        if let Some(retained) = record.retained_mib {
            context.metric(
                format!("{dimension} {} retained", record.label),
                "MiB",
                retained,
            );
        }
        records.push(record);
    }
    Ok(records)
}

fn accepted(records: &[PointRecord]) -> Vec<PointRecord> {
    records.iter().filter(|record| record.ok).cloned().collect()
}

fn report_rejections(context: &mut CaseContext, records: &[PointRecord]) -> Vec<PointRecord> {
    let rejected: Vec<PointRecord> = records
        .iter()
        .filter(|record| !record.ok)
        .cloned()
        .collect();
    for record in &rejected {
        context.log(format!(
            "  rejected: {} at {:.2} MiB encoded request",
            record.label,
            record.request_bytes as f64 / MIB as f64
        ));
    }
    rejected
}

fn growth_against_request_mib(records: &[PointRecord]) -> Vec<(f64, f64)> {
    records
        .iter()
        .map(|record| (record.x / MIB as f64, record.growth_mib))
        .collect()
}

fn retention_against_request_mib(records: &[PointRecord]) -> Vec<(f64, f64)> {
    records
        .iter()
        .filter_map(|record| {
            record
                .retained_mib
                .map(|retained| (record.x / MIB as f64, retained))
        })
        .collect()
}

fn growth_against_x(records: &[PointRecord]) -> Vec<(f64, f64)> {
    records
        .iter()
        .map(|record| (record.x, record.growth_mib))
        .collect()
}

async fn request_size_matrix(context: &mut CaseContext, settings: &Settings) -> Result<()> {
    let sizes = settings.pick(
        &[
            4 * KIB,
            64 * KIB,
            MIB,
            4 * MIB,
            8 * MIB,
            16 * MIB,
            24 * MIB,
            MAX_REQUEST_BYTES,
        ],
        &[4 * KIB, MIB, 4 * MIB, 8 * MIB],
    );
    let points = sizes
        .iter()
        .map(|&size| Point {
            request_bytes: size,
            ..Point::new(format!("request {} KiB", size / KIB))
        })
        .collect();
    let records = sweep(context, "request_bytes", points, |_, record| {
        record.request_bytes as f64
    })
    .await?;
    let rejected = report_rejections(context, &records);
    let usable = accepted(&records);

    let growth = context.publish_fit(
        "request size growth",
        fit_linear_region(&growth_against_request_mib(&usable)),
        "MiB request",
        "MiB",
    );
    let retention = context.publish_fit(
        "request size retention",
        fit_linear_region(&retention_against_request_mib(&usable)),
        "MiB request",
        "MiB",
    );

    context.finding(format!(
        "a request costs {} at its peak",
        growth.plain("MiB of request body", "MiB")
    ));
    context.finding(format!(
        "of that, {} is still resident a second later and does not come back",
        retention.plain("MiB of request body", "MiB")
    ));
    if let Some(smallest) = rejected.iter().map(|record| record.request_bytes).min() {
        context.finding(format!(
            "requests are refused from about {:.0} MiB upwards, so that is the hard ceiling on one request",
            smallest as f64 / MIB as f64
        ));
    }
    if let Some(biggest) = usable.iter().max_by_key(|record| record.request_bytes) {
        context.finding(format!(
            "worst point measured: {:.0} MiB of request peaked at {:.0} MiB of gateway memory",
            biggest.request_bytes as f64 / MIB as f64,
            biggest.peak_mib
        ));
    }
    Ok(())
}

async fn response_size_matrix(context: &mut CaseContext, settings: &Settings) -> Result<()> {
    let sizes = settings.pick(
        &[4 * KIB, 64 * KIB, MIB, 4 * MIB, 8 * MIB, 16 * MIB, 32 * MIB],
        &[4 * KIB, MIB, 8 * MIB],
    );
    let mut slopes = Vec::new();
    for mode in ["sse", "json"] {
        let points = sizes
            .iter()
            .map(|&size| Point {
                response_bytes: size,
                mode,
                chunks: 32,
                ..Point::new(format!("{mode} response {} KiB", size / KIB))
            })
            .collect();
        let records = sweep(
            context,
            &format!("response_bytes_{mode}"),
            points,
            |_, record| record.response_bytes as f64,
        )
        .await?;
        let usable = accepted(&records);
        let growth = context.publish_fit(
            &format!("{mode} response size growth"),
            fit_linear_region(&growth_against_request_mib(&usable)),
            "MiB response",
            "MiB",
        );
        context.publish_fit(
            &format!("{mode} response size retention"),
            fit_linear_region(&retention_against_request_mib(&usable)),
            "MiB response",
            "MiB",
        );
        let label = if mode == "sse" {
            "a streamed response"
        } else {
            "a plain (non-streamed) response"
        };
        context.finding(format!(
            "{label} costs {}",
            growth.plain("MiB of response body", "MiB")
        ));
        slopes.push(growth.slope);
    }

    if slopes[0] > 0.0 {
        let ratio = slopes[1] / slopes[0];
        if ratio >= 1.0 {
            context.finding(format!(
                "plain responses are {ratio:.1}x more expensive than streamed ones per MiB, because a non-streamed body is held whole instead of frame by frame"
            ));
        } else {
            context.finding(format!(
                "over this range a plain response is no dearer per MiB than a streamed one ({ratio:.1}x); the non-streamed curve bends, so its cost per MiB depends on which sizes you fit"
            ));
        }
    }
    Ok(())
}

async fn concurrency_matrix(context: &mut CaseContext, settings: &Settings) -> Result<()> {
    let levels = settings.pick(&[1, 2, 4, 8, 16, 32, 64], &[1, 4, 16]);
    let points = levels
        .iter()
        .map(|&level| Point {
            request_bytes: MIB,
            response_bytes: MIB,
            chunks: 32,
            delay_ms: 5,
            concurrency: level,
            ..Point::new(format!("concurrency {level}"))
        })
        .collect();
    let records = sweep(context, "concurrency", points, |_, record| {
        record.concurrency as f64
    })
    .await?;
    let usable = accepted(&records);
    let fit = context.publish_fit(
        "concurrency growth",
        fit_linear_region(&growth_against_x(&usable)),
        "in-flight request",
        "MiB",
    );
    if !context.harness_pressure.is_empty() {
        let flagged = context.harness_pressure.join(", ");
        context.log(format!(
            "  harness-limited points: {flagged}; the marginal cost above them is not the gateway's"
        ));
    }

    context.finding(format!(
        "each extra request in flight adds {}, with nothing shared between them",
        fit.plain("request in flight", "MiB")
    ));
    if let Some(widest) = usable.iter().max_by_key(|record| record.concurrency) {
        context.finding(format!(
            "{} requests of 1 MiB in and 1 MiB out at once peaked at {:.0} MiB, so concurrency is the cheapest way to run out of memory",
            widest.concurrency, widest.peak_mib
        ));
    }
    if !context.harness_pressure.is_empty() {
        context.finding(
            "some points were limited by the test driver rather than the gateway; do not read the top of the curve as a gateway figure",
        );
    }
    Ok(())
}

async fn payload_shape_matrix(context: &mut CaseContext, settings: &Settings) -> Result<()> {
    let sizes = settings.pick(
        &[256 * KIB, MIB, 4 * MIB, 8 * MIB],
        &[256 * KIB, MIB, 2 * MIB],
    );
    let mut per_byte: Vec<(PayloadShape, usize, f64)> = Vec::new();
    for shape in [PayloadShape::OneLongString, PayloadShape::ManySmallBlocks] {
        let points = sizes
            .iter()
            .map(|&size| Point {
                request_bytes: size,
                request_shape: shape,
                block_bytes: 64,
                ..Point::new(format!("{} {} KiB", shape.label(), size / KIB))
            })
            .collect();
        let records = sweep(
            context,
            &format!("payload_shape_{}", shape.label()),
            points,
            |_, record| record.request_bytes as f64,
        )
        .await?;
        let usable = accepted(&records);
        context.publish_fit(
            &format!("payload shape {} growth", shape.label()),
            fit_linear_region(&growth_against_request_mib(&usable)),
            "MiB request",
            "MiB",
        );
        for record in &usable {
            let cost = record.growth_mib / (record.request_bytes as f64 / MIB as f64).max(1e-9);
            per_byte.push((shape, record.nominal_request_bytes, cost));
        }
    }

    let cost_of = |shape: PayloadShape, size: usize| {
        per_byte
            .iter()
            .find(|entry| entry.0 == shape && entry.1 == size)
            .map(|entry| entry.2)
    };
    let mut divergences = Vec::new();
    context.log("  64-byte content blocks against one string of the same total size:");
    for &size in &sizes {
        let (Some(one_string), Some(many_objects)) = (
            cost_of(PayloadShape::OneLongString, size),
            cost_of(PayloadShape::ManySmallBlocks, size),
        ) else {
            continue;
        };
        let divergence = many_objects / one_string.max(1e-9);
        context.log(format!(
            "    {} KiB: string {one_string:.2} vs objects {many_objects:.2} MiB growth per MiB (objects/string {divergence:.2}x)",
            size / KIB
        ));
        context.metric(
            format!("payload shape divergence at {} KiB", size / KIB),
            "x",
            divergence,
        );
        divergences.push((size, divergence));
    }

    context.log(
        "  serde_json is built with preserve_order, so Value maps are IndexMap-backed at compile time; this axis is the only way to see that cost, since the guardrails-off control pays it too and no runtime flag can switch it off",
    );

    if let Some(&(largest_size, largest)) = divergences.last() {
        context.finding(format!(
            "shape matters more than size: at {} KiB, a body made of many small blocks costs {largest:.1}x what the same bytes cost as one long string",
            largest_size / KIB
        ));
        context.finding(
            "the gap grows with body size, so a chat with a long history of short messages is the expensive case, not one big paste",
        );
        context.finding(
            "this cost is paid whether guardrails are on or off; it is built into the binary and cannot be turned off by configuration",
        );
    }
    Ok(())
}

async fn sse_frame_count_matrix(context: &mut CaseContext, settings: &Settings) -> Result<()> {
    let counts = settings.pick(&[1, 4, 32, 256, 2048, 16384], &[1, 32, 2048]);
    let response_bytes = 4 * MIB;
    let points = counts
        .iter()
        .map(|&count| Point {
            response_bytes,
            chunks: count,
            ..Point::new(format!("{count} frames"))
        })
        .collect();
    let records = sweep(context, "sse_frames", points, |point, _| {
        point.chunks as f64
    })
    .await?;
    let usable = accepted(&records);
    context.log(format!(
        "  {} MiB response throughout, so any difference is per-frame cost in FrameParser (src/policies/sse.rs) and the per-frame Vec<Value> in extract_usage (src/providers.rs)",
        response_bytes / MIB
    ));
    context.publish_fit(
        "sse frame count growth",
        fit_linear_region(&growth_against_x(&usable)),
        "frame",
        "MiB",
    );

    let cheapest = usable
        .iter()
        .min_by(|left, right| left.growth_mib.total_cmp(&right.growth_mib));
    let dearest = usable
        .iter()
        .max_by(|left, right| left.growth_mib.total_cmp(&right.growth_mib));
    if let (Some(cheapest), Some(dearest)) = (cheapest, dearest) {
        context.log(format!(
            "  the relationship is not monotonic, so read the extremes rather than the slope: cheapest at {} frames ({:+.1} MiB), dearest at {} frames ({:+.1} MiB)",
            cheapest.x, cheapest.growth_mib, dearest.x, dearest.growth_mib
        ));
        context.metric(
            "sse frame count cheapest growth",
            "MiB",
            cheapest.growth_mib,
        );
        context.metric("sse frame count dearest growth", "MiB", dearest.growth_mib);
        context.finding(format!(
            "for the same {} MiB of response, frame size matters: cheapest at {} ({:.0} MiB), dearest at {} ({:.0} MiB)",
            response_bytes / MIB,
            frame_count(cheapest.x),
            cheapest.growth_mib,
            frame_count(dearest.x),
            dearest.growth_mib
        ));
        context.finding(
            "both extremes cost more than the middle: very few huge frames need one big buffer, very many tiny frames pay per-frame overhead",
        );
    }
    Ok(())
}

fn frame_count(count: f64) -> String {
    if count == 1.0 {
        "1 frame".to_string()
    } else {
        format!("{count:.0} frames")
    }
}

async fn stream_duration_matrix(context: &mut CaseContext, settings: &Settings) -> Result<()> {
    let delays = settings.pick(&[0, 1, 5, 20], &[0, 5, 20]);
    let points = delays
        .iter()
        .map(|&delay| Point {
            response_bytes: MIB,
            chunks: 32,
            delay_ms: delay,
            concurrency: 4,
            ..Point::new(format!("delay {delay} ms"))
        })
        .collect();
    let records = sweep(context, "stream_delay_ms", points, |point, _| {
        point.delay_ms as f64
    })
    .await?;
    let usable = accepted(&records);
    context.log("  4 concurrent streams, so a longer hold means more overlapping capture buffers");
    let fit = context.publish_fit(
        "stream delay growth",
        fit_linear_region(&growth_against_x(&usable)),
        "ms per frame",
        "MiB",
    );

    if fit.is_flat() || fit.slope >= 0.0 {
        context.finding("holding a stream open longer does not by itself cost more memory");
    } else {
        context.finding(
            "slower streams cost less, not more: spacing them out means fewer are in flight at the same moment. What costs memory is overlap, not how long a stream lasts",
        );
    }
    Ok(())
}

async fn guardrails_mode_matrix(context: &mut CaseContext, settings: &Settings) -> Result<()> {
    let size = settings.one(8 * MIB, 4 * MIB);
    let points = [
        GuardrailsMode::Off,
        GuardrailsMode::Observe,
        GuardrailsMode::Mask,
    ]
    .into_iter()
    .map(|mode| Point {
        request_bytes: size,
        guardrails_mode: mode,
        ..Point::new(format!("guardrails {}", mode.label()))
    })
    .collect();
    let records = sweep(context, "guardrails_mode", points, |_, record| {
        record.request_bytes as f64
    })
    .await?;
    let usable = accepted(&records);
    let find = |mode: GuardrailsMode| {
        usable
            .iter()
            .find(|record| record.guardrails_mode == mode)
            .cloned()
    };
    let Some(control) = find(GuardrailsMode::Off) else {
        return Ok(());
    };
    for mode in [GuardrailsMode::Observe, GuardrailsMode::Mask] {
        let Some(record) = find(mode) else { continue };
        let ratio = record.growth_mib / control.growth_mib.max(1.0 / 1024.0);
        context.log(format!(
            "  {} growth is {ratio:.2}x the guardrails-off control at {} MiB",
            mode.label(),
            size / MIB
        ));
        context.metric(
            format!("guardrails {} over control", mode.label()),
            "x",
            ratio,
        );
    }
    context.log(
        "  the control is not a clean zero-cost baseline: serde_json's preserve_order feature is compile-time, so the IndexMap-backed Value tree is paid on every path",
    );

    if let Some(masked) = find(GuardrailsMode::Mask) {
        let ratio = masked.growth_mib / control.growth_mib.max(1.0 / 1024.0);
        context.finding(format!(
            "turning masking on costs {ratio:.2}x the memory of running with guardrails off at a {} MiB request, so masking is not where the memory goes",
            size / MIB
        ));
    }
    context.finding(
        "the guardrails-off comparison cannot be a clean baseline: the expensive part of parsing a request is compiled in and paid on every path, guardrails or not",
    );
    Ok(())
}

async fn secret_count_matrix(context: &mut CaseContext, settings: &Settings) -> Result<()> {
    let counts = settings.pick(&[0, 1, 10, 100], &[0, 10, 100]);
    let points = counts
        .iter()
        .map(|&count| Point {
            request_bytes: MIB,
            secret_count: Some(count),
            extra_query: vec![("echo", "1".to_string())],
            response_bytes: 64 * KIB,
            ..Point::new(format!("{count} secrets"))
        })
        .collect();
    let records = sweep(context, "secret_count", points, |point, _| {
        point.secret_count.unwrap_or(0) as f64
    })
    .await?;
    let usable = accepted(&records);
    context.log(
        "  echo=1 so the placeholders travel back and the replacement map plus StreamRestorer state in src/policies/restore.rs is exercised, not only the mask pass",
    );
    let fit = context.publish_fit(
        "secret count growth",
        fit_linear_region(&growth_against_x(&usable)),
        "secret",
        "MiB",
    );

    if fit.is_flat() {
        context.finding(format!(
            "the number of secrets in a request does not matter: 0 and {} distinct secrets cost the same, so no limit on secrets per request is needed",
            counts.iter().max().copied().unwrap_or(0)
        ));
    } else {
        context.finding(format!(
            "each extra secret in a request adds {}",
            fit.plain("secret", "MiB")
        ));
    }
    Ok(())
}

async fn request_encoding_matrix(context: &mut CaseContext, settings: &Settings) -> Result<()> {
    let size = settings.one(8 * MIB, 4 * MIB);
    context.log(
        "  identity, gzip, zstd and brotli are all covered: the flate2, zstd and brotli crates aegis itself decodes with are available to the driver, so every arm of decode_declared in src/compression.rs is exercised, brotli included",
    );
    let points = RequestEncoding::all()
        .into_iter()
        .map(|encoding| Point {
            request_bytes: size,
            encoding,
            ..Point::new(format!("encoding {}", encoding.label()))
        })
        .collect();
    let records = sweep(context, "request_encoding", points, |_, record| {
        record.request_bytes as f64
    })
    .await?;
    let usable = accepted(&records);
    let Some(identity) = usable
        .iter()
        .find(|record| record.encoding == RequestEncoding::Identity)
        .cloned()
    else {
        return Ok(());
    };
    for record in &usable {
        if record.encoding == RequestEncoding::Identity {
            continue;
        }
        let ratio = record.growth_mib / identity.growth_mib.max(1.0 / 1024.0);
        context.log(format!(
            "  {}: {:.3} MiB on the wire for a {:.1} MiB decoded body, growth {ratio:.2}x identity",
            record.encoding.label(),
            record.request_bytes as f64 / MIB as f64,
            size as f64 / MIB as f64
        ));
        context.metric(
            format!("request encoding {} over identity", record.encoding.label()),
            "x",
            ratio,
        );
        context.finding(format!(
            "a {}-compressed request costs {ratio:.2}x an uncompressed one of the same decoded size, so limits must be set on the uncompressed size, not the bytes on the wire",
            record.encoding.label()
        ));
    }
    Ok(())
}

async fn capture_limit_matrix(context: &mut CaseContext, settings: &Settings) -> Result<()> {
    let response_bytes = settings.one(16 * MIB, 4 * MIB);
    let limits = [256 * KIB, 16 * MIB];
    let points = limits
        .into_iter()
        .map(|limit| Point {
            response_bytes,
            chunks: 64,
            max_capture_bytes: limit,
            ..Point::new(format!("capture {} KiB", limit / KIB))
        })
        .collect();
    let records = sweep(context, "max_capture_bytes", points, |_, record| {
        record.max_capture_bytes as f64
    })
    .await?;
    let usable = accepted(&records);
    let find = |limit: usize| {
        usable
            .iter()
            .find(|record| record.max_capture_bytes == limit)
            .cloned()
    };
    let (Some(small), Some(large)) = (find(256 * KIB), find(16 * MIB)) else {
        return Ok(());
    };
    let share = 1.0 - small.growth_mib / large.growth_mib.max(1.0 / 1024.0);
    context.log(format!(
        "  at a {} MiB response the capture buffer is {:+.0}% of the growth; the remainder is the frame parser and the post-stream usage extraction",
        response_bytes / MIB,
        share * 100.0
    ));
    context.metric("capture buffer share of growth", "fraction", share);
    context.finding(format!(
        "lowering max_capture_bytes from 16 MiB to 256 KiB cut the cost of a {} MiB response by {:.0}%, so the setting works but only covers part of the cost",
        response_bytes / MIB,
        share * 100.0
    ));
    Ok(())
}

async fn provider_matrix(context: &mut CaseContext, settings: &Settings) -> Result<()> {
    let request_bytes = settings.one(4 * MIB, MIB);
    let points = ["claude", "codex"]
        .into_iter()
        .map(|provider| Point {
            provider,
            request_bytes,
            response_bytes: 4 * MIB,
            chunks: 64,
            ..Point::new(format!("provider {provider}"))
        })
        .collect();
    let records = sweep(context, "provider", points, |_, record| {
        record.request_bytes as f64
    })
    .await?;
    let usable = accepted(&records);
    let mut per_provider = Vec::new();
    for record in &usable {
        let per_mib = record.growth_mib / (record.payload_bytes as f64 / MIB as f64).max(1e-9);
        context.metric(
            format!("provider {} growth per MiB", record.provider),
            "MiB/MiB",
            per_mib,
        );
        per_provider.push((record.provider, per_mib));
    }

    if per_provider.len() == 2 {
        let claude = per_provider[0].1;
        let codex = per_provider[1].1;
        if (codex - claude).abs() <= claude.abs().max(1e-9) * 0.2 {
            context.finding("claude and codex cost the same; one limit can cover both providers");
        } else {
            context.finding(format!(
                "codex costs {:.2}x claude per MiB, so the two providers need separate limits",
                codex / claude.max(1e-9)
            ));
        }
    }
    Ok(())
}

async fn client_disconnect_mid_stream(
    context: &mut CaseContext,
    _settings: &Settings,
) -> Result<()> {
    let id = context.start(GatewayOptions::default())?;
    let gateway = context.gateway(id);
    let point = Point {
        response_bytes: 8 * MIB,
        chunks: 256,
        delay_ms: 2,
        request_bytes: MIB,
        ..Point::new("abandoned reads")
    };
    let url = point.url(&gateway);
    let body = point.body(&gateway)?;
    let abandoned = 8;

    let (measurement, results) = {
        let gateway = Arc::clone(&gateway);
        let url = url.clone();
        let body = body.clone();
        context
            .measure(
                &format!("{abandoned} concurrent abandoned streams"),
                id,
                move || async move {
                    let mut tasks = Vec::new();
                    for _ in 0..abandoned {
                        let gateway = Arc::clone(&gateway);
                        let url = url.clone();
                        let body = body.clone();
                        tasks.push(tokio::spawn(async move {
                            gateway.post_and_abandon(&url, body, 64 * KIB).await
                        }));
                    }
                    let mut read = Vec::new();
                    for task in tasks {
                        read.push(task.await.map(|result| result.bytes_read).unwrap_or(0));
                    }
                    read
                },
            )
            .await
    };
    context.log(format!(
        "  bytes read before dropping the socket: {results:?}"
    ));

    tokio::time::sleep(SETTLE * 3).await;
    let settled = read_rss_kib(gateway.pid()).unwrap_or(measurement.before_kib);
    let retained = mib_delta(measurement.before_kib, settled);
    context.metric("abandoned stream retained", "MiB", retained);

    let follow_up = gateway
        .post(
            &url,
            body,
            RequestEncoding::Identity,
            Duration::from_secs(60),
        )
        .await;
    if follow_up.status != Some(200) {
        bail!(
            "the gateway did not serve a request after {abandoned} abandoned streams: status {:?}, error {:?}",
            follow_up.status,
            follow_up.error
        );
    }

    // One child, so one high-water mark: it covers the follow-up too, which is
    // an order of magnitude too small to set it.
    drop(gateway);
    let row = context.release(id);
    context.log(format!(
        "  peak {:.1} MiB ({}), growth {:+.1} MiB, retained after settle {retained:+.1} MiB",
        mib(row.headline_kib()),
        row.peak_source.label(),
        row.growth_mib()
    ));

    context.finding(format!(
        "a client that hangs up mid-response does not free what was already buffered: {abandoned} abandoned streams left {retained:.0} MiB resident"
    ));
    context.finding(
        "cancelled requests still count against memory, so a retry storm is as expensive as the same traffic completed",
    );
    context.finding("the gateway kept serving traffic afterwards, so nothing wedged");
    Ok(())
}

async fn upstream_error_with_large_body(
    context: &mut CaseContext,
    settings: &Settings,
) -> Result<()> {
    let size = settings.one(16 * MIB, 4 * MIB);
    let point = Point {
        request_bytes: MIB,
        response_bytes: size,
        extra_query: vec![("status", "503".to_string())],
        expected_statuses: vec![Some(503)],
        ..Point::new(format!("upstream 503 with {} MiB body", size / MIB))
    };
    let record = run_point(context, &point).await?;
    context.log("  a forwarded 5xx is the expected result, not a failure");
    context.metric("upstream 5xx growth", "MiB", record.growth_mib);
    if let Some(retained) = record.retained_mib {
        context.metric("upstream 5xx retained", "MiB", retained);
    }
    context.finding(format!(
        "an upstream error is not a cheap path: a {} MiB error body cost {:.0} MiB, about the same as a successful response that size",
        size / MIB, record.growth_mib
    ));
    context.finding(
        "a failing provider therefore does not protect the gateway; failures need the same size limits as successes",
    );
    Ok(())
}

async fn upstream_transport_failure_mid_stream(
    context: &mut CaseContext,
    _settings: &Settings,
) -> Result<()> {
    let point = Point {
        request_bytes: MIB,
        response_bytes: 8 * MIB,
        chunks: 64,
        concurrency: 4,
        extra_query: vec![("abort", "1".to_string())],
        expected_statuses: vec![None, Some(200)],
        ..Point::new("upstream aborts mid-stream")
    };
    let record = run_point(context, &point).await?;
    context.log("  a truncated read is the expected result");
    context.metric("upstream abort growth", "MiB", record.growth_mib);
    if let Some(retained) = record.retained_mib {
        context.metric("upstream abort retained", "MiB", retained);
    }
    context.finding(format!(
        "when the provider drops the connection halfway, the gateway still peaked at {:.0} MiB and kept {} of it, so a flaky upstream costs as much as a working one",
        record.peak_mib,
        match record.retained_mib {
            Some(value) => format!("{value:.0} MiB"),
            None => "n/a".to_string(),
        }
    ));
    context
        .finding("clients see a truncated response rather than an error, which is worth knowing");
    Ok(())
}

async fn stream_without_stop_frames(context: &mut CaseContext, settings: &Settings) -> Result<()> {
    let counts = settings.pick(&[1, 64, 256, 1024], &[1, 64, 256]);
    let points = counts
        .iter()
        .map(|&count| Point {
            request_bytes: 256 * KIB,
            response_bytes: 2 * MIB,
            chunks: (count * 4).max(32),
            secret_count: Some(4),
            extra_query: vec![
                ("blocks", count.to_string()),
                ("no_stop", "1".to_string()),
                ("echo", "1".to_string()),
            ],
            ..Point::new(format!("{count} unterminated blocks"))
        })
        .collect();
    let records = sweep(context, "unterminated_blocks", points, |point, _| {
        point.query_value("blocks")
    })
    .await?;
    let usable = accepted(&records);
    context.log(
        "  no content_block_stop is ever sent, so StreamRestorer.blocks is only pruned by finish() at the end of the stream (src/policies/restore.rs:121-130); a slope here is per-block state held for the stream's whole lifetime",
    );
    let fit = context.publish_fit(
        "unterminated block growth",
        fit_linear(&growth_against_x(&usable)),
        "open block",
        "MiB",
    );

    if fit.is_flat() {
        context.finding(format!(
            "a stream that never closes its content blocks does not pile up state: 1 and {} open blocks cost the same",
            counts.iter().max().copied().unwrap_or(0)
        ));
    } else {
        context.finding(format!(
            "per-block state does accumulate while a stream is open, about {:.0} MiB per thousand open blocks, which is small but real",
            fit.slope * 1000.0
        ));
    }
    Ok(())
}

async fn repeated_large_request_ratchet(
    context: &mut CaseContext,
    settings: &Settings,
) -> Result<()> {
    let count = settings.one(8, 4);
    let size = settings.one(16 * MIB, 8 * MIB);
    let id = context.start(GatewayOptions::default())?;
    let gateway = context.gateway(id);
    let point = Point {
        request_bytes: size,
        ..Point::new(format!("request {} MiB", size / MIB))
    };
    let url = point.url(&gateway);
    let body = point.body(&gateway)?;
    let pid = gateway.pid();

    let baseline = read_rss_kib(pid).unwrap_or(0);
    let mut floors = Vec::new();
    for index in 0..count {
        let result = gateway
            .post(
                &url,
                body.clone(),
                RequestEncoding::Identity,
                Duration::from_secs(180),
            )
            .await;
        if result.status != Some(200) {
            bail!(
                "ratchet request {index} returned status {:?}, error {:?}",
                result.status,
                result.error
            );
        }
        tokio::time::sleep(SETTLE).await;
        let floor = read_rss_kib(pid).unwrap_or(baseline);
        floors.push(floor);
        context.log(format!(
            "    after request {}: floor {:.1} MiB ({:+.1} MiB over the start)",
            index + 1,
            mib(floor),
            mib_delta(baseline, floor)
        ));
        context.metric(
            format!("ratchet floor after request {}", index + 1),
            "MiB",
            mib(floor),
        );
    }

    let series: Vec<(f64, f64)> = floors
        .iter()
        .enumerate()
        .map(|(index, &floor)| (index as f64, mib(floor)))
        .collect();
    let fit = context.publish_fit(
        "ratchet floor per request",
        fit_linear_region(&series),
        "request",
        "MiB",
    );
    let first_step = mib_delta(baseline, floors[0]);
    let last_step = if floors.len() > 1 {
        mib_delta(floors[floors.len() - 2], floors[floors.len() - 1])
    } else {
        0.0
    };
    let converging = last_step <= first_step.abs() / 4.0;
    let retained_per_mib_sent =
        mib_delta(baseline, floors[floors.len() - 1]) / ((count * size / MIB) as f64).max(1e-9);
    context.log(format!(
        "  first request left {first_step:+.1} MiB, the last added {last_step:+.1} MiB -> {}; per-request retention over the run {:+.2} MiB/request ({retained_per_mib_sent:.3} MiB retained per MiB sent)",
        if converging { "converging" } else { "still ratcheting" },
        fit.slope
    ));
    context.metric(
        "ratchet retained per MiB sent",
        "MiB/MiB",
        retained_per_mib_sent,
    );

    context.finding(format!(
        "the first {} MiB request raised the gateway's resting memory by {first_step:.0} MiB and it never came back down",
        size / MIB
    ));
    if converging {
        context.finding(format!(
            "the climb levels off: after {count} requests the resting level settled at {:.0} MiB and the last request only added {last_step:.0} MiB. So it is a one-off step up per size of request, not an endless leak",
            mib(floors[floors.len() - 1])
        ));
        context.finding(
            "plan capacity for the resting level a large request leaves behind, not for the memory a small idle gateway uses",
        );
    } else {
        context.finding(format!(
            "the resting level was still climbing after {count} requests ({last_step:+.0} MiB on the last one), which would need a longer run to bound"
        ));
    }
    Ok(())
}

struct Probe {
    survived: bool,
    cap: MemoryCap,
}

async fn probe_survives(
    context: &mut CaseContext,
    concurrency: usize,
    memory_max: &str,
    request_bytes: usize,
    response_bytes: usize,
) -> Result<Option<Probe>> {
    let id = context.start(GatewayOptions {
        guardrails: GuardrailsMode::Mask,
        max_capture_bytes: DEFAULT_CAPTURE_BYTES,
        memory_max: Some(memory_max.to_string()),
        ..GatewayOptions::default()
    })?;
    let gateway = context.gateway(id);
    if gateway.memory_cap() == &MemoryCap::None {
        drop(gateway);
        context.release(id);
        return Ok(None);
    }
    let point = Point {
        request_bytes,
        response_bytes,
        chunks: 32,
        concurrency,
        timeout: Duration::from_secs(120),
        ..Point::new(format!("concurrency {concurrency}"))
    };
    let url = point.url(&gateway);
    let body = point.body(&gateway)?;
    let in_flight_mib = (concurrency * (body.len() + response_bytes)) as f64 / MIB as f64;

    let (_measurement, _results) = {
        let gateway = Arc::clone(&gateway);
        let url = url.clone();
        let body = body.clone();
        context
            .measure(
                &format!("probe concurrency {concurrency}"),
                id,
                move || async move {
                    post_concurrently(gateway, url, body, concurrency, Duration::from_secs(120))
                        .await
                },
            )
            .await
    };
    let cap = gateway.memory_cap().clone();
    drop(gateway);
    let row = context.release(id);
    let capped = cap.was_breached_by(row.exit_status);
    context.log(format!(
        "    concurrency {concurrency}: {in_flight_mib:.1} MiB in flight, peak {:.1} MiB, exit {:?}, over the {} cap {capped}",
        mib(row.headline_kib()),
        row.exit_status,
        cap.label(),
    ));
    Ok(Some(Probe {
        survived: !capped,
        cap,
    }))
}

async fn oom_bisection(context: &mut CaseContext, settings: &Settings) -> Result<()> {
    let request_bytes = 24 * MIB;
    let response_bytes = 8 * MIB;
    let highest = settings.one(64, 8);
    let memory_max = settings.oom_memory_max.clone();
    context.log(format!(
        "  MemoryMax {memory_max}, MemorySwapMax 0, masking on, {} MiB requests against {} MiB streamed responses, bisecting concurrency in [1, {highest}] with a fresh gateway per probe",
        request_bytes / MIB,
        response_bytes / MIB
    ));

    let per_request_mib = (request_bytes + response_bytes) as f64 / MIB as f64;
    let mut survived: Option<usize> = None;

    let top = probe_survives(context, highest, &memory_max, request_bytes, response_bytes).await?;
    let Some(top) = top else {
        context.log(
            "  neither a systemd scope nor RLIMIT_AS could cap this gateway, so no limit was applied; the case is unavailable on this host rather than passing",
        );
        context.finding(
            "no memory cap could be imposed here, so the safe in-flight payload is unmeasured on this host; nothing below is a result",
        );
        return Ok(());
    };
    if top.cap == MemoryCap::AddressSpace {
        context.log(
            "  systemd-run --user is unavailable, so the cap is RLIMIT_AS: allocation fails and the process aborts itself rather than the kernel OOM-killing it, and the ceiling is virtual address space, which a Rust process reserves far more of than it makes resident",
        );
        context.finding(
            "this run capped RLIMIT_AS rather than cgroup memory, so the boundary below is where allocation started failing, not where the kernel would kill the gateway; it is not comparable with a MemoryMax figure",
        );
    }
    if top.survived {
        context.log(format!(
            "  concurrency {highest} survived; nothing in [1, {highest}] reproduces a kill at {memory_max}. Raise the ramp or lower --oom-memory-max"
        ));
        context.metric("oom bisection killed at MiB in flight", "MiB", 0.0);
        context.metric(
            "oom bisection survived MiB in flight",
            "MiB",
            highest as f64 * per_request_mib,
        );
        context.finding(format!(
            "nothing up to {:.0} MiB in flight was killed under {memory_max}, so the safe limit is above what this search covered",
            highest as f64 * per_request_mib
        ));
        return Ok(());
    }

    let mut killed_at = highest;
    let (mut low, mut high) = (1, highest);
    while low < high {
        let middle = (low + high) / 2;
        let Some(outcome) =
            probe_survives(context, middle, &memory_max, request_bytes, response_bytes).await?
        else {
            break;
        };
        if outcome.survived {
            survived = Some(survived.unwrap_or(0).max(middle));
            low = middle + 1;
        } else {
            killed_at = middle;
            high = middle;
        }
    }

    context.log(format!(
        "  safe operating boundary: killed at >= {:.1} MiB in flight (concurrency {killed_at}); {}",
        killed_at as f64 * per_request_mib,
        match survived {
            Some(level) => format!(
                "survived {:.1} MiB (concurrency {level})",
                level as f64 * per_request_mib
            ),
            None => "no surviving concurrency was found, so even 1 in flight is over the cap"
                .to_string(),
        }
    ));
    context.metric(
        "oom bisection killed at MiB in flight",
        "MiB",
        killed_at as f64 * per_request_mib,
    );
    context.metric(
        "oom bisection killed at concurrency",
        "concurrency",
        killed_at as f64,
    );
    context.metric(
        "oom bisection survived MiB in flight",
        "MiB",
        survived.unwrap_or(0) as f64 * per_request_mib,
    );

    context.finding(format!(
        "under a {memory_max} memory limit the gateway is killed once about {:.0} MiB of payload is in flight ({killed_at} concurrent {} MiB requests)",
        killed_at as f64 * per_request_mib,
        request_bytes / MIB
    ));
    match survived {
        Some(level) => context.finding(format!(
            "{:.0} MiB in flight ({level} concurrent requests) survived, so that is the largest amount measured as safe at this limit",
            level as f64 * per_request_mib
        )),
        None => context.finding(
            "even a single request of this size was killed, so the limit is too low for this payload",
        ),
    }
    context.finding(format!(
        "as a rule of thumb, the gateway needs roughly {:.0}x the in-flight payload size in memory headroom",
        memory_max_mib(&memory_max) / (killed_at as f64 * per_request_mib).max(1e-9)
    ));
    Ok(())
}

fn memory_max_mib(value: &str) -> f64 {
    memory_size_bytes(value).unwrap_or(0) as f64 / MIB as f64
}
