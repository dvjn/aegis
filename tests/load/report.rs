//! https://github.com/testmoapp/junitxml and https://github.com/benchmark-action/github-action-benchmark

use std::path::Path;

use anyhow::Result;
use serde_json::json;

use crate::runner::{CaseRecord, Status};
use crate::shared::measure::{PeakSource, mib, mib_delta};

pub const SUITE: &str = "memory";

fn stage_name(label: &str, description: &str) -> String {
    let collapsed = label.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("{collapsed} [{description}]")
}

fn case_properties(record: &CaseRecord) -> Vec<(String, String)> {
    let mut properties = vec![
        ("status".to_string(), record.status.label().to_string()),
        ("seconds".to_string(), format!("{:.3}", record.seconds)),
    ];
    let mut push_memory = |name: &str, value: Option<u64>| {
        if let Some(kib) = value {
            properties.push((format!("{name} MiB"), format!("{:.1}", mib(kib))));
        }
    };
    push_memory("peak", record.peak_kib);
    push_memory("sampled peak", record.sampled_peak_kib);
    push_memory("rss before", record.before_kib);
    push_memory("rss after", record.after_kib);
    if let Some(source) = record.peak_source {
        properties.push(("peak source".to_string(), source.label().to_string()));
    }

    if let (Some(before), Some(peak)) = (record.before_kib, record.peak_kib) {
        properties.push((
            "growth MiB".to_string(),
            format!("{:+.1}", mib_delta(before, peak)),
        ));
    }
    if let (Some(before), Some(after)) = (record.before_kib, record.after_kib) {
        properties.push((
            "retained MiB".to_string(),
            format!("{:+.1}", mib_delta(before, after)),
        ));
    }
    if record.gateways > 0 {
        properties.push(("gateways".to_string(), record.gateways.to_string()));
    }

    for measurement in &record.measurements {
        let stage = stage_name(&measurement.label, &measurement.description);
        properties.push((
            format!("{stage} seconds"),
            format!("{:.3}", measurement.seconds),
        ));
        properties.push((
            format!("{stage} peak MiB"),
            format!("{:.1}", mib(measurement.peak_kib())),
        ));
        properties.push((
            format!("{stage} growth MiB"),
            format!(
                "{:+.1}",
                mib_delta(measurement.before_kib, measurement.peak_kib())
            ),
        ));
        properties.push((
            format!("{stage} peak source"),
            measurement.peak_source.label().to_string(),
        ));
        properties.push((
            format!("{stage} sampled peak MiB"),
            format!("{:.1}", mib(measurement.sampled_peak_kib)),
        ));
        properties.push((
            format!("{stage} sampled floor MiB"),
            format!("{:.1}", mib(measurement.sampled_floor_kib)),
        ));
        if let Some(after) = measurement.after_kib {
            properties.push((
                format!("{stage} retained MiB"),
                format!("{:+.1}", mib_delta(measurement.before_kib, after)),
            ));
        }
        if let Some(harness) = measurement.harness_peak_kib {
            properties.push((
                format!("{stage} harness peak MiB"),
                format!("{:.1}", mib(harness)),
            ));
        }
    }
    for metric in &record.metrics {
        properties.push((
            format!("{} {}", metric.name, metric.unit),
            format!("{:+.2}", metric.value),
        ));
    }
    properties
}

pub fn metric_lines(record: &CaseRecord) -> Vec<String> {
    case_properties(record)
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect()
}

fn escape(text: &str) -> String {
    text.chars()
        .map(|character| match character {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            // XML 1.0 forbids most control characters outright.
            control if control.is_control() && !matches!(control, '\n' | '\r' | '\t') => {
                String::new()
            }
            other => other.to_string(),
        })
        .collect()
}

pub fn write_junit(records: &[CaseRecord], path: &Path) -> Result<()> {
    let classname = format!("aegis.{SUITE}");
    let timestamp = chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S+00:00")
        .to_string();
    let failures = records
        .iter()
        .filter(|record| record.status == Status::Fail)
        .count();
    let errors = records
        .iter()
        .filter(|record| record.status == Status::Error)
        .count();
    let total_seconds: f64 = records.iter().map(|record| record.seconds).sum();

    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str(&format!(
        "<testsuites name=\"aegis\" tests=\"{}\" failures=\"{failures}\" errors=\"{errors}\" skipped=\"0\" time=\"{total_seconds:.3}\">\n",
        records.len()
    ));
    xml.push_str(&format!(
        "  <testsuite name=\"{classname}\" tests=\"{}\" failures=\"{failures}\" errors=\"{errors}\" skipped=\"0\" time=\"{total_seconds:.3}\" timestamp=\"{timestamp}\">\n",
        records.len()
    ));

    for record in records {
        let properties = case_properties(record);
        xml.push_str(&format!(
            "    <testcase name=\"{}\" classname=\"{classname}\" time=\"{:.3}\" timestamp=\"{timestamp}\" file=\"tests/load/cases.rs\">\n",
            escape(record.case),
            record.seconds
        ));
        xml.push_str("      <properties>\n");
        for (name, value) in &properties {
            xml.push_str(&format!(
                "        <property name=\"{}\" value=\"{}\"/>\n",
                escape(name),
                escape(value)
            ));
        }
        xml.push_str("      </properties>\n");

        if let Some(failure) = &record.failure {
            let tag = if record.status == Status::Fail {
                "failure"
            } else {
                "error"
            };
            let first_line = failure.lines().next().unwrap_or("failed");
            xml.push_str(&format!(
                "      <{tag} message=\"{}\" type=\"{}\">{}</{tag}>\n",
                escape(first_line),
                record.status.label(),
                escape(failure)
            ));
        }

        let mut system_out = vec![record.description.to_string()];
        system_out.extend(record.details.iter().cloned());
        system_out.push(String::new());
        system_out.extend(
            record
                .findings
                .iter()
                .map(|finding| format!("finding: {finding}")),
        );
        system_out.push(String::new());
        system_out.extend(
            properties
                .iter()
                .map(|(name, value)| format!("{name}={value}")),
        );
        xml.push_str(&format!(
            "      <system-out>{}</system-out>\n",
            escape(&system_out.join("\n"))
        ));
        xml.push_str("    </testcase>\n");
    }

    xml.push_str("  </testsuite>\n</testsuites>\n");
    std::fs::write(path, xml)?;
    Ok(())
}

pub fn write_benchmark(records: &[CaseRecord], path: &Path) -> Result<()> {
    let mut entries = Vec::new();
    for record in records {
        let case = format!("{SUITE}/{}", record.case);
        let worst = if record.environments.is_empty() {
            "no gateway".to_string()
        } else {
            record
                .environments
                .iter()
                .map(|row| row.description.clone())
                .collect::<Vec<_>>()
                .join("; ")
        };
        entries.push(json!({
            "name": format!("{case} wall clock"),
            "unit": "s",
            "value": round(record.seconds, 3),
            "extra": record.description,
        }));
        if let Some(peak) = record.peak_kib {
            entries.push(json!({
                "name": format!("{case} peak"),
                "unit": "MiB",
                "value": round(mib(peak), 1),
                "extra": format!(
                    "{worst}; {} peak",
                    record.peak_source.map(PeakSource::label).unwrap_or("no")
                ),
            }));
        }
        if let (Some(before), Some(peak)) = (record.before_kib, record.peak_kib) {
            entries.push(json!({
                "name": format!("{case} peak growth"),
                "unit": "MiB",
                "value": round(mib_delta(before, peak), 1),
                "extra": worst,
            }));
        }
        for measurement in &record.measurements {
            let stage = format!(
                "{case} {}",
                stage_name(&measurement.label, &measurement.description)
            );
            entries.push(json!({
                "name": format!("{stage} peak"),
                "unit": "MiB",
                "value": round(mib(measurement.peak_kib()), 1),
                "extra": measurement.description,
            }));
            entries.push(json!({
                "name": format!("{stage} growth"),
                "unit": "MiB",
                "value": round(mib_delta(measurement.before_kib, measurement.peak_kib()), 1),
                "extra": measurement.description,
            }));
        }
        for metric in &record.metrics {
            entries.push(json!({
                "name": format!("{case} {}", metric.name),
                "unit": metric.unit,
                "value": round(metric.value, 2),
                "extra": record.description,
            }));
        }
    }
    std::fs::write(path, serde_json::to_string_pretty(&entries)? + "\n")?;
    Ok(())
}

fn round(value: f64, places: i32) -> f64 {
    let scale = 10f64.powi(places);
    (value * scale).round() / scale
}
