use std::panic::AssertUnwindSafe;
use std::time::Instant;

use futures_util::FutureExt;

use crate::cases::{Case, Settings};
use crate::context::{CaseContext, GatewayRow, Measurement, Metric};
use crate::shared::gateway::PeakMode;
use crate::shared::measure::{PeakSource, mib, mib_delta};
use crate::shared::upstream::Upstream;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    Error,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Error => "error",
        }
    }
}

pub struct CaseRecord {
    pub case: &'static str,
    pub description: &'static str,
    pub status: Status,
    pub failure: Option<String>,
    pub seconds: f64,
    pub details: Vec<String>,
    pub findings: Vec<String>,
    pub measurements: Vec<Measurement>,
    pub metrics: Vec<Metric>,
    pub before_kib: Option<u64>,
    pub peak_kib: Option<u64>,
    pub peak_source: Option<PeakSource>,
    pub sampled_peak_kib: Option<u64>,
    pub after_kib: Option<u64>,
    pub gateways: usize,
    pub environments: Vec<GatewayRow>,
}

pub async fn run_case(
    case: &Case,
    settings: &Settings,
    upstream: &Upstream,
    peak_mode: PeakMode,
    keep_workdirs: bool,
    verbose: bool,
) -> CaseRecord {
    let mut context = CaseContext::new(upstream, peak_mode, keep_workdirs);
    let started = Instant::now();
    let outcome = AssertUnwindSafe((case.run)(&mut context, settings))
        .catch_unwind()
        .await;
    let seconds = started.elapsed().as_secs_f64();

    let (status, failure) = match outcome {
        Ok(Ok(())) => (Status::Pass, None),
        Ok(Err(error)) => (Status::Fail, Some(format!("{error:#}"))),
        Err(panic) => (Status::Error, Some(describe_panic(&panic))),
    };

    let rows = context.settle();
    if let Some(failure) = &failure {
        context.log(format!("  failure: {failure}"));
        context.log("  gateway log tail:");
        let tail = context.gateway_tail();
        for line in tail.lines() {
            context.log(format!("    {line}"));
        }
    }

    let worst = rows.iter().max_by_key(|row| row.headline_kib()).cloned();
    let record = CaseRecord {
        case: case.name,
        description: case.description,
        status,
        failure,
        seconds,
        details: context.details.clone(),
        findings: context.findings.clone(),
        measurements: context.measurements.clone(),
        metrics: context.metrics.clone(),
        before_kib: worst.as_ref().map(|row| row.before_kib),
        peak_kib: worst.as_ref().and_then(|row| row.peak_kib),
        peak_source: worst.as_ref().map(|row| row.peak_source),
        sampled_peak_kib: worst.as_ref().map(|row| row.sampled_peak_kib),
        after_kib: worst.as_ref().and_then(|row| row.after_kib),
        gateways: rows.len(),
        environments: rows,
    };

    println!(
        "{:5} memory/{}  {:.2}s  {}",
        record.status.label().to_uppercase(),
        record.case,
        record.seconds,
        format_memory(&record)
    );
    if verbose || record.status != Status::Pass {
        for line in &record.details {
            println!("{line}");
        }
    }
    for finding in &record.findings {
        println!("  -> {finding}");
    }
    for kept in &context.kept_workdirs {
        println!("  kept workdir: {}", kept.display());
    }
    record
}

fn describe_panic(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = panic.downcast_ref::<&str>() {
        format!("panicked: {message}")
    } else if let Some(message) = panic.downcast_ref::<String>() {
        format!("panicked: {message}")
    } else {
        "panicked".to_string()
    }
}

fn format_memory(record: &CaseRecord) -> String {
    let Some(before) = record.before_kib else {
        return "memory n/a".to_string();
    };
    let headline = match record.peak_kib {
        Some(value) => format!("{:.1}", mib(value)),
        None => "n/a".to_string(),
    };
    let growth = match record.peak_kib {
        Some(value) => format!("{:+.1}", mib_delta(before, value)),
        None => "n/a".to_string(),
    };
    let retained = match record.after_kib {
        Some(value) => format!("{:+.1}", mib_delta(before, value)),
        None => "n/a".to_string(),
    };
    let scope = if record.gateways == 1 {
        String::new()
    } else {
        format!(" worst of {} gateways", record.gateways)
    };
    format!(
        "peak {headline} ({}) sampled peak {} before {:.1} after {} growth {growth} retained {retained} (MiB){scope}",
        record
            .peak_source
            .map(PeakSource::label)
            .unwrap_or("unmeasured"),
        optional(record.sampled_peak_kib),
        mib(before),
        optional(record.after_kib),
    )
}

fn optional(value: Option<u64>) -> String {
    match value {
        Some(kib) => format!("{:.1}", mib(kib)),
        None => "n/a".to_string(),
    }
}

pub fn print_summary(records: &[CaseRecord]) -> usize {
    let header = format!(
        "{:44}{:6} {:>7} {:>8} {:>8} {:>9} {:>8} {:>8} {:>9}",
        "case", "status", "secs", "before", "peak", "sampled*", "after", "growth", "retained"
    );
    println!("\n{header}");
    println!("{}", "-".repeat(header.len()));
    for record in records {
        let columns = [
            column(record.before_kib, 8),
            column(record.peak_kib, 8),
            column(record.sampled_peak_kib, 9),
            column(record.after_kib, 8),
        ];
        let growth = match (record.before_kib, record.peak_kib) {
            (Some(before), Some(peak)) => format!("{:+8.1}", mib_delta(before, peak)),
            _ => format!("{:>8}", "-"),
        };
        let retained = match (record.before_kib, record.after_kib) {
            (Some(before), Some(after)) => format!("{:+9.1}", mib_delta(before, after)),
            _ => format!("{:>9}", "-"),
        };
        println!(
            "{:44}{:6} {:7.2} {} {growth} {retained}",
            truncate(record.case, 43),
            record.status.label(),
            record.seconds,
            columns.join(" ")
        );
    }
    println!(
        "\nMiB. peak is a kernel high-water mark for the gateway, read once it has exited; sampled* is max-of-samples and only a lower bound.\nCases that start several gateways report the worst one, not a sum; per-gateway figures are in the detail lines."
    );

    let failed: Vec<&CaseRecord> = records
        .iter()
        .filter(|record| record.status != Status::Pass)
        .collect();
    let total_seconds: f64 = records.iter().map(|record| record.seconds).sum();
    println!(
        "\n{} cases, {} passed, {} failed, {total_seconds:.1}s total",
        records.len(),
        records.len() - failed.len(),
        failed.len()
    );
    for record in &failed {
        println!("  failed: memory/{}", record.case);
    }
    failed.len()
}

fn column(value: Option<u64>, width: usize) -> String {
    match value {
        Some(kib) => format!("{:>width$.1}", mib(kib)),
        None => format!("{:>width$}", "-"),
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        format!("{text:limit$} ")
    } else {
        format!("{} ", &text[..limit])
    }
}
