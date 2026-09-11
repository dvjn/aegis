use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};

use crate::model::{DeltaMetrics, Observation, PairedDelta, RunReport, ScenarioSummary, Variant};

pub fn comparisons(observations: &[Observation]) -> (Vec<PairedDelta>, Vec<ScenarioSummary>) {
    let mut groups = BTreeMap::<(&str, &str), Vec<&Observation>>::new();
    for observation in observations {
        groups
            .entry((&observation.suite, &observation.scenario))
            .or_default()
            .push(observation);
    }

    let mut pairs = Vec::new();
    let mut summaries = Vec::new();
    for ((suite, scenario), observations) in groups {
        let mut scenario_pairs = Vec::new();
        for pair in 1..=2 {
            let baseline = observations.iter().find(|observation| {
                observation.pair == pair && observation.variant == Variant::Baseline
            });
            let candidate = observations.iter().find(|observation| {
                observation.pair == pair && observation.variant == Variant::Candidate
            });
            let (Some(baseline), Some(candidate)) = (baseline, candidate) else {
                continue;
            };
            scenario_pairs.push(PairedDelta {
                suite: suite.to_string(),
                scenario: scenario.to_string(),
                pair,
                valid: baseline.valid && candidate.valid,
                baseline_position: baseline.position,
                candidate_position: candidate.position,
                candidate_minus_baseline: deltas(baseline, candidate),
            });
        }
        let valid = scenario_pairs
            .iter()
            .filter(|pair| pair.valid)
            .collect::<Vec<_>>();
        let median_paired_delta = aggregate(&valid, median);
        let paired_delta_spread = aggregate(&valid, spread);
        let inputs = observations
            .first()
            .expect("scenario group is not empty")
            .inputs
            .clone();
        summaries.push(ScenarioSummary {
            suite: suite.to_string(),
            scenario: scenario.to_string(),
            inputs,
            valid_pairs: valid.len(),
            total_pairs: scenario_pairs.len(),
            median_paired_delta,
            paired_delta_spread,
        });
        pairs.extend(scenario_pairs);
    }
    summaries.sort_by_key(|summary| (suite_rank(&summary.suite), summary.scenario.clone()));
    pairs.sort_by_key(|pair| (suite_rank(&pair.suite), pair.scenario.clone(), pair.pair));
    (pairs, summaries)
}

fn suite_rank(suite: &str) -> u8 {
    match suite {
        "steady" => 0,
        "soak" => 1,
        "scale" => 2,
        "resilience" => 3,
        _ => 4,
    }
}

pub fn write(report: &RunReport, output: &Path) -> Result<()> {
    std::fs::create_dir_all(output)
        .with_context(|| format!("creating output directory {}", output.display()))?;
    let json_path = output.join("results.json");
    std::fs::write(&json_path, serde_json::to_vec_pretty(report)?)
        .with_context(|| format!("writing {}", json_path.display()))?;
    let tsv_path = output.join("summary.tsv");
    std::fs::write(&tsv_path, tsv(report))
        .with_context(|| format!("writing {}", tsv_path.display()))?;
    let markdown_path = output.join("summary.md");
    std::fs::write(&markdown_path, markdown(report))
        .with_context(|| format!("writing {}", markdown_path.display()))?;
    println!("wrote {}", json_path.display());
    println!("wrote {}", tsv_path.display());
    println!("wrote {}", markdown_path.display());
    Ok(())
}

pub fn print_summary(report: &RunReport) {
    println!(
        "\n{:<38} {:>7} {:>15} {:>15} {:>12}",
        "scenario", "pairs", "peak growth", "settled growth", "duration"
    );
    for summary in &report.summaries {
        println!(
            "{:<38} {:>3}/{:<3} {:>+15} {:>+15} {:>+12}",
            format!("{}::{}", summary.suite, summary.scenario),
            summary.valid_pairs,
            summary.total_pairs,
            optional(summary.median_paired_delta.peak_growth_kib),
            optional(summary.median_paired_delta.settled_growth_kib),
            optional(summary.median_paired_delta.duration_ms),
        );
    }
    let invalid = report
        .observations
        .iter()
        .filter(|observation| !observation.valid)
        .count();
    for observation in report
        .observations
        .iter()
        .filter(|observation| !observation.valid)
    {
        println!(
            "invalid {}::{} {} pair {}: {}",
            observation.suite,
            observation.scenario,
            observation.variant.label(),
            observation.pair,
            observation.invalid_reasons.join("; ")
        );
    }
    println!(
        "\n{} observations, {} valid, {} invalid",
        report.observations.len(),
        report.observations.len() - invalid,
        invalid
    );
}

fn tsv(report: &RunReport) -> String {
    let mut output = String::from(
        "schema_version\tsuite\tscenario\tprotocol\ttarget_request_bytes\tsemantic_parts\ttarget_response_bytes\trequest_count\tconcurrency\tvalid_pairs\ttotal_pairs\tmedian_ready_delta_kib\tspread_ready_delta_kib\tmedian_peak_delta_kib\tspread_peak_delta_kib\tmedian_peak_growth_delta_kib\tspread_peak_growth_delta_kib\tmedian_settled_delta_kib\tspread_settled_delta_kib\tmedian_settled_growth_delta_kib\tspread_settled_growth_delta_kib\tmedian_duration_delta_ms\tspread_duration_delta_ms\n",
    );
    for summary in &report.summaries {
        let median = &summary.median_paired_delta;
        let spread = &summary.paired_delta_spread;
        writeln!(
            output,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            report.schema_version,
            plain(&summary.suite),
            plain(&summary.scenario),
            plain(&summary.inputs.protocol),
            optional(summary.inputs.target_request_bytes),
            optional(summary.inputs.semantic_parts),
            optional(summary.inputs.target_response_bytes),
            summary.inputs.request_count,
            summary.inputs.concurrency,
            summary.valid_pairs,
            summary.total_pairs,
            optional(median.ready_rss_kib),
            optional(spread.ready_rss_kib),
            optional(median.sampled_workload_peak_kib),
            optional(spread.sampled_workload_peak_kib),
            optional(median.peak_growth_kib),
            optional(spread.peak_growth_kib),
            optional(median.settled_rss_kib),
            optional(spread.settled_rss_kib),
            optional(median.settled_growth_kib),
            optional(spread.settled_growth_kib),
            optional(median.duration_ms),
            optional(spread.duration_ms),
        )
        .expect("writing to String");
    }
    output
}

fn markdown(report: &RunReport) -> String {
    let mut output = String::new();
    for suite in ["steady", "soak", "scale", "resilience"] {
        let summaries = report
            .summaries
            .iter()
            .filter(|summary| summary.suite == suite)
            .collect::<Vec<_>>();
        if summaries.is_empty() {
            continue;
        }
        writeln!(
            output,
            "<details>\n<summary><strong>{}</strong> · {} scenarios</summary>\n",
            title(suite),
            summaries.len(),
        )
        .expect("writing to String");
        output.push_str("| Scenario | Baseline | Candidate | Change |\n");
        output.push_str("| --- | ---: | ---: | ---: |\n");
        for summary in summaries {
            let observations = report.observations.iter().filter(|observation| {
                observation.valid
                    && observation.suite == summary.suite
                    && observation.scenario == summary.scenario
            });
            let baseline = observations
                .clone()
                .filter(|observation| observation.variant == Variant::Baseline)
                .collect::<Vec<_>>();
            let candidate = observations
                .filter(|observation| observation.variant == Variant::Candidate)
                .collect::<Vec<_>>();
            let baseline_peak = observation_median(&baseline, |metrics| metrics.peak_growth_kib);
            let candidate_peak = observation_median(&candidate, |metrics| metrics.peak_growth_kib);
            let baseline_settled =
                observation_median(&baseline, |metrics| metrics.settled_growth_kib);
            let candidate_settled =
                observation_median(&candidate, |metrics| metrics.settled_growth_kib);
            let peak_delta = summary.median_paired_delta.peak_growth_kib;
            let settled_delta = summary.median_paired_delta.settled_growth_kib;
            if materially_different(peak_delta, settled_delta) {
                markdown_row(
                    &mut output,
                    &format!("{} peak", title(&summary.scenario)),
                    baseline_peak,
                    candidate_peak,
                    peak_delta,
                );
                markdown_row(
                    &mut output,
                    &format!("{} settled", title(&summary.scenario)),
                    baseline_settled,
                    candidate_settled,
                    settled_delta,
                );
            } else {
                markdown_row(
                    &mut output,
                    &title(&summary.scenario),
                    baseline_settled,
                    candidate_settled,
                    settled_delta,
                );
            }
        }
        output.push_str("\n</details>\n\n");
    }
    let failed = report
        .observations
        .iter()
        .filter(|observation| !observation.valid)
        .count();
    if failed > 0 {
        writeln!(output, "{failed} runs failed measurement checks.").expect("writing to String");
    }
    output
}

fn markdown_row(
    output: &mut String,
    scenario: &str,
    baseline: Option<i64>,
    candidate: Option<i64>,
    delta: Option<i64>,
) {
    writeln!(
        output,
        "| {scenario} | {} | {} | {} · {} |",
        mib_value(baseline),
        mib_value(candidate),
        mib_delta(delta),
        percent_delta(delta, baseline),
    )
    .expect("writing to String");
}

fn materially_different(peak: Option<i64>, settled: Option<i64>) -> bool {
    let (Some(peak), Some(settled)) = (peak, settled) else {
        return true;
    };
    let difference = (peak - settled).abs();
    difference > 1024 && difference * 10 > peak.abs().max(settled.abs())
}

fn percent_delta(delta: Option<i64>, baseline: Option<i64>) -> String {
    match (delta, baseline) {
        (Some(delta), Some(baseline)) if baseline != 0 => {
            format!("{:+.1}%", delta as f64 * 100.0 / baseline as f64)
        }
        _ => "-".to_string(),
    }
}

fn observation_median(
    observations: &[&Observation],
    field: fn(&crate::model::MemoryMetrics) -> Option<i64>,
) -> Option<i64> {
    median(
        &observations
            .iter()
            .filter_map(|observation| field(&observation.metrics))
            .collect::<Vec<_>>(),
    )
}

fn title(value: &str) -> String {
    let mut title = value.replace('-', " ");
    for (from, to) in [
        ("anthropic", "Anthropic"),
        ("openai", "OpenAI"),
        ("sse", "SSE"),
        ("kib", "KiB"),
        ("mib", "MiB"),
    ] {
        title = title.replace(from, to);
    }
    if let Some(first) = title.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    title
}

fn mib_value(value: Option<i64>) -> String {
    value
        .map(|value| format!("{:.2} MiB", value as f64 / 1024.0))
        .unwrap_or_else(|| "-".to_string())
}

fn mib_delta(value: Option<i64>) -> String {
    value
        .map(|value| format!("{:+.2} MiB", value as f64 / 1024.0))
        .unwrap_or_else(|| "-".to_string())
}

fn deltas(baseline: &Observation, candidate: &Observation) -> DeltaMetrics {
    DeltaMetrics {
        ready_rss_kib: difference(
            baseline.metrics.ready_rss_kib,
            candidate.metrics.ready_rss_kib,
        ),
        sampled_workload_peak_kib: difference(
            baseline.metrics.sampled_workload_peak_kib,
            candidate.metrics.sampled_workload_peak_kib,
        ),
        peak_growth_kib: difference_signed(
            baseline.metrics.peak_growth_kib,
            candidate.metrics.peak_growth_kib,
        ),
        settled_rss_kib: difference(
            baseline.metrics.settled_rss_kib,
            candidate.metrics.settled_rss_kib,
        ),
        settled_growth_kib: difference_signed(
            baseline.metrics.settled_growth_kib,
            candidate.metrics.settled_growth_kib,
        ),
        duration_ms: difference(baseline.metrics.duration_ms, candidate.metrics.duration_ms),
    }
}

fn difference(baseline: Option<u64>, candidate: Option<u64>) -> Option<i64> {
    Some(candidate? as i64 - baseline? as i64)
}

fn difference_signed(baseline: Option<i64>, candidate: Option<i64>) -> Option<i64> {
    Some(candidate? - baseline?)
}

fn aggregate(pairs: &[&PairedDelta], operation: fn(&[i64]) -> Option<i64>) -> DeltaMetrics {
    let values = |field: fn(&DeltaMetrics) -> Option<i64>| {
        pairs
            .iter()
            .filter_map(|pair| field(&pair.candidate_minus_baseline))
            .collect::<Vec<_>>()
    };
    DeltaMetrics {
        ready_rss_kib: operation(&values(|delta| delta.ready_rss_kib)),
        sampled_workload_peak_kib: operation(&values(|delta| delta.sampled_workload_peak_kib)),
        peak_growth_kib: operation(&values(|delta| delta.peak_growth_kib)),
        settled_rss_kib: operation(&values(|delta| delta.settled_rss_kib)),
        settled_growth_kib: operation(&values(|delta| delta.settled_growth_kib)),
        duration_ms: operation(&values(|delta| delta.duration_ms)),
    }
}

fn median(values: &[i64]) -> Option<i64> {
    let mut values = values.to_vec();
    values.sort_unstable();
    match values.len() {
        0 => None,
        length if length % 2 == 1 => Some(values[length / 2]),
        length => Some((values[length / 2 - 1] + values[length / 2]) / 2),
    }
}

fn spread(values: &[i64]) -> Option<i64> {
    Some(values.iter().max()? - values.iter().min()?)
}

fn optional<T: ToString>(value: Option<T>) -> String {
    value.map(|value| value.to_string()).unwrap_or_default()
}

fn plain(value: &str) -> String {
    value.replace(['\t', '\n', '\r'], " ")
}
