use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::analysis::Fit;
use crate::shared::gateway::{Gateway, GatewayOptions, PeakMode};
use crate::shared::measure::{PeakSource, Sampler, mib, mib_delta, read_rss_kib};
use crate::shared::upstream::Upstream;

pub const SETTLE: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GatewayId(usize);

const HARNESS_PRESSURE_RATIO: f64 = 1.0;
const HARNESS_PRESSURE_FLOOR_KIB: u64 = 32 * 1024;

#[derive(Clone, Debug)]
pub struct Metric {
    pub name: String,
    pub unit: String,
    pub value: f64,
}

#[derive(Clone, Debug)]
pub struct Measurement {
    pub label: String,
    pub description: String,
    pub seconds: f64,
    pub before_kib: u64,
    pub exact_peak_kib: Option<u64>,
    pub peak_source: PeakSource,
    pub sampled_peak_kib: u64,
    pub sampled_floor_kib: u64,
    pub after_kib: Option<u64>,
    pub harness_peak_kib: Option<u64>,
    gateway: GatewayId,
}

impl Measurement {
    pub fn peak_kib(&self) -> u64 {
        self.exact_peak_kib.unwrap_or(self.sampled_peak_kib)
    }
}

#[derive(Clone, Debug)]
pub struct GatewayRow {
    pub description: String,
    pub before_kib: u64,
    pub peak_kib: Option<u64>,
    pub peak_source: PeakSource,
    pub sampled_peak_kib: u64,
    pub after_kib: Option<u64>,
    pub exit_status: Option<i32>,
}

impl GatewayRow {
    pub fn headline_kib(&self) -> u64 {
        self.peak_kib.unwrap_or(self.sampled_peak_kib)
    }

    pub fn growth_mib(&self) -> f64 {
        mib_delta(self.before_kib, self.headline_kib())
    }
}

struct Tracked {
    gateway: Arc<Gateway>,
    before_kib: u64,
    sampler: Sampler,
}

pub struct CaseContext {
    upstream: Upstream,
    peak_mode: PeakMode,
    keep_workdirs: bool,
    tracked: Vec<(GatewayId, Tracked)>,
    next_id: GatewayId,
    released: Vec<GatewayRow>,
    pub kept_workdirs: Vec<PathBuf>,
    pub details: Vec<String>,
    pub findings: Vec<String>,
    pub measurements: Vec<Measurement>,
    pub metrics: Vec<Metric>,
    pub harness_pressure: Vec<String>,
    last_tail: String,
}

impl CaseContext {
    pub fn new(upstream: &Upstream, peak_mode: PeakMode, keep_workdirs: bool) -> Self {
        Self {
            upstream: upstream.clone(),
            peak_mode,
            keep_workdirs,
            tracked: Vec::new(),
            next_id: GatewayId(0),
            released: Vec::new(),
            kept_workdirs: Vec::new(),
            details: Vec::new(),
            findings: Vec::new(),
            measurements: Vec::new(),
            metrics: Vec::new(),
            harness_pressure: Vec::new(),
            last_tail: "(no gateway started)".to_string(),
        }
    }

    pub fn log(&mut self, line: impl Into<String>) {
        self.details.push(line.into());
    }

    pub fn finding(&mut self, line: impl Into<String>) {
        self.findings.push(line.into());
    }

    pub fn metric(&mut self, name: impl Into<String>, unit: &str, value: f64) {
        self.metrics.push(Metric {
            name: name.into(),
            unit: unit.to_string(),
            value,
        });
    }

    pub fn publish_fit(&mut self, series: &str, fit: Fit, x_unit: &str, y_unit: &str) -> Fit {
        self.log(format!("  fit {series}: {}", fit.describe(x_unit, y_unit)));
        self.metric(format!("{series} intercept"), y_unit, fit.intercept);
        self.metric(
            format!("{series} slope"),
            &format!("{y_unit}/{x_unit}"),
            fit.slope,
        );
        self.metric(format!("{series} r_squared"), "r2", fit.r_squared);
        self.metric(format!("{series} points"), "count", fit.count() as f64);
        fit
    }

    fn position(&self, id: GatewayId) -> usize {
        self.tracked
            .iter()
            .position(|(tracked_id, _)| *tracked_id == id)
            .expect("gateway was already released")
    }

    pub fn start(&mut self, options: GatewayOptions) -> Result<GatewayId> {
        let gateway = Gateway::start(
            &self.upstream,
            GatewayOptions {
                peak_mode: self.peak_mode,
                keep_workdir: self.keep_workdirs,
                ..options
            },
        )?;
        for note in gateway.notes.clone() {
            self.log(format!("  note: {note}"));
        }
        self.last_tail = String::new();
        let pid = gateway.pid();
        let before_kib = read_rss_kib(pid).unwrap_or(0);
        let sampler = Sampler::start(pid, Arc::clone(gateway.peaks()), None);
        let id = self.next_id;
        self.next_id = GatewayId(id.0 + 1);
        self.tracked.push((
            id,
            Tracked {
                gateway: Arc::new(gateway),
                before_kib,
                sampler,
            },
        ));
        Ok(id)
    }

    pub fn gateway(&self, id: GatewayId) -> Arc<Gateway> {
        Arc::clone(&self.tracked[self.position(id)].1.gateway)
    }

    pub fn release(&mut self, id: GatewayId) -> GatewayRow {
        let slot = self.position(id);
        let (
            _id,
            Tracked {
                gateway,
                before_kib,
                sampler,
            },
        ) = self.tracked.remove(slot);
        let samples = sampler.stop();
        let row = self.retire(&gateway, before_kib, samples.peak_or(before_kib));
        self.backfill_peak(id, &row);
        self.released.push(row.clone());
        row
    }

    fn retire(&mut self, gateway: &Gateway, before_kib: u64, sampled_peak_kib: u64) -> GatewayRow {
        let after_kib = read_rss_kib(gateway.pid());
        let description = gateway.describe();
        self.last_tail = gateway.log_tail(20);
        let shutdown = gateway.shut_down();
        if let Some(kept) = shutdown.kept_workdir {
            self.kept_workdirs.push(kept);
        }
        GatewayRow {
            description,
            before_kib,
            peak_kib: shutdown.peak_kib,
            peak_source: shutdown.peak_source,
            sampled_peak_kib,
            after_kib,
            exit_status: shutdown.exit_status,
        }
    }

    fn backfill_peak(&mut self, id: GatewayId, row: &GatewayRow) {
        for measurement in &mut self.measurements {
            if measurement.gateway == id {
                measurement.exact_peak_kib = row.peak_kib;
                measurement.peak_source = row.peak_source;
            }
        }
    }

    pub async fn measure<Action, Fut, Output>(
        &mut self,
        label: &str,
        id: GatewayId,
        action: Action,
    ) -> (Measurement, Output)
    where
        Action: FnOnce() -> Fut,
        Fut: Future<Output = Output>,
    {
        let slot = self.position(id);
        let gateway = Arc::clone(&self.tracked[slot].1.gateway);
        let pid = gateway.pid();
        let before_kib = read_rss_kib(pid).unwrap_or(0);
        let harness_pid = std::process::id();
        let harness_before = read_rss_kib(harness_pid);

        let sampler = Sampler::start(pid, Arc::clone(gateway.peaks()), Some(harness_pid));
        let started = Instant::now();
        let output = action().await;
        let seconds = started.elapsed().as_secs_f64();
        let samples = sampler.stop();

        let sampled_peak_kib = samples.peak_or(before_kib);
        let after_kib = read_rss_kib(pid);
        self.log(format!(
            "  {label}: {seconds:6.2}s  before {:7.1}  sampled max {:7.1}  after {}  sampled growth {:+7.1}  retained {}",
            mib(before_kib),
            mib(sampled_peak_kib),
            optional_mib(after_kib),
            mib_delta(before_kib, sampled_peak_kib),
            optional_mib_delta(before_kib, after_kib),
        ));

        let measurement = Measurement {
            label: label.to_string(),
            description: gateway.describe(),
            seconds,
            before_kib,
            exact_peak_kib: None,
            peak_source: PeakSource::Sampled,
            sampled_peak_kib,
            sampled_floor_kib: samples.floor_or(before_kib),
            after_kib,
            harness_peak_kib: samples.harness_peak_kib,
            gateway: id,
        };
        self.measurements.push(measurement.clone());
        self.warn_on_harness_pressure(
            label,
            sampled_peak_kib.saturating_sub(before_kib),
            harness_before,
            samples.harness_peak_kib,
        );
        (measurement, output)
    }

    fn warn_on_harness_pressure(
        &mut self,
        label: &str,
        gateway_growth_kib: u64,
        harness_before: Option<u64>,
        harness_peak: Option<u64>,
    ) {
        let (Some(baseline), Some(peak)) = (harness_before, harness_peak) else {
            return;
        };
        let growth = peak.saturating_sub(baseline);
        if growth < HARNESS_PRESSURE_FLOOR_KIB {
            return;
        }
        if growth as f64 >= gateway_growth_kib as f64 * HARNESS_PRESSURE_RATIO {
            self.harness_pressure.push(label.to_string());
            self.log(format!(
                "    warning: the harness grew {:+.1} MiB against the gateway's {:+.1} MiB; treat this point as harness-limited",
                mib(growth),
                mib(gateway_growth_kib)
            ));
        }
    }

    pub fn settle(&mut self) -> Vec<GatewayRow> {
        let mut pending = Vec::new();
        for (id, tracked) in self.tracked.drain(..) {
            let samples = tracked.sampler.stop();
            pending.push((id, tracked.gateway, tracked.before_kib, samples));
        }
        if !pending.is_empty() {
            std::thread::sleep(SETTLE);
        }

        let mut rows = std::mem::take(&mut self.released);
        for (id, gateway, before_kib, samples) in pending {
            let row = self.retire(&gateway, before_kib, samples.peak_or(before_kib));
            self.backfill_peak(id, &row);
            rows.push(row);
        }

        for (index, row) in rows.iter().enumerate() {
            self.details.push(format!(
                "  gateway {} ({}): peak {} MiB ({}), sampled peak {:.1} MiB (lower bound), before {:.1} MiB, after {} MiB",
                index + 1,
                row.description,
                match row.peak_kib {
                    Some(value) => format!("{:.1}", mib(value)),
                    None => "n/a".to_string(),
                },
                row.peak_source.label(),
                mib(row.sampled_peak_kib),
                mib(row.before_kib),
                optional_mib(row.after_kib),
            ));
        }
        rows
    }

    pub fn gateway_tail(&self) -> String {
        if self.last_tail.is_empty() {
            match self.tracked.first() {
                Some((_id, tracked)) => tracked.gateway.log_tail(20),
                None => "(no gateway started)".to_string(),
            }
        } else {
            self.last_tail.clone()
        }
    }
}

pub fn optional_mib(value: Option<u64>) -> String {
    match value {
        Some(kib) => format!("{:7.1}", mib(kib)),
        None => "    n/a".to_string(),
    }
}

pub fn optional_mib_delta(from: u64, to: Option<u64>) -> String {
    match to {
        Some(kib) => format!("{:+7.1}", mib_delta(from, kib)),
        None => "    n/a".to_string(),
    }
}
