//! Puts two runs of the benchmark JSON the suite already writes side by side.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::report::SUITE;

/// The entries `report::write_benchmark` names after the whole case rather than
/// after a series within it.
const CASE_WIDE_SUFFIXES: [&str; 3] = ["wall clock", "peak", "peak growth"];

#[derive(Clone, Debug, Deserialize)]
pub struct Entry {
    name: String,
    unit: String,
    value: f64,
}

pub fn read(path: &Path) -> Result<Vec<Entry>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the run at {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing the run at {}", path.display()))
}

pub fn name_from_path(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// `memory/concurrency_matrix concurrency 16 [guardrails off] peak` becomes
/// `concurrency 16 peak`, and `memory/request_size_matrix peak` becomes
/// `request_size peak`.
fn label(name: &str) -> String {
    let name = name.strip_prefix(&format!("{SUITE}/")).unwrap_or(name);
    let (case, series) = name.split_once(' ').unwrap_or((name, ""));
    let case = case.trim_end_matches("_matrix");
    let series = collapse_brackets(series);
    if series.is_empty() || CASE_WIDE_SUFFIXES.contains(&series.as_str()) {
        return format!("{case} {series}").trim().to_string();
    }
    series
}

fn collapse_brackets(text: &str) -> String {
    let mut kept = String::new();
    let mut depth = 0usize;
    for character in text.chars() {
        match character {
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            other if depth == 0 => kept.push(other),
            _ => {}
        }
    }
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn format_value(value: f64, unit: &str) -> String {
    let places = if unit == "count" { 0 } else { 1 };
    format!("{value:.places$} {unit}")
}

fn percentage_change(baseline: f64, current: f64) -> Option<String> {
    (baseline != 0.0).then(|| format!("({:+.0}%)", (current - baseline) / baseline * 100.0))
}

struct Row {
    label: String,
    baseline: String,
    current: String,
}

fn rows(baseline: &[Entry], current: &[Entry]) -> (Vec<Row>, Vec<String>) {
    let mut rows = Vec::new();
    let mut absent = Vec::new();
    let only_in = |run: &str, name: &str| format!("{} is only in the {run}", label(name));

    for entry in current {
        let value = format_value(entry.value, &entry.unit);
        match baseline.iter().find(|other| other.name == entry.name) {
            Some(other) => rows.push(Row {
                label: label(&entry.name),
                baseline: format_value(other.value, &other.unit),
                current: match percentage_change(other.value, entry.value) {
                    Some(change) => format!("{value} {change}"),
                    None => value,
                },
            }),
            None if baseline.is_empty() => rows.push(Row {
                label: label(&entry.name),
                baseline: String::new(),
                current: value,
            }),
            None => absent.push(only_in("current run", &entry.name)),
        }
    }
    for entry in baseline {
        if !current.iter().any(|other| other.name == entry.name) {
            absent.push(only_in("baseline", &entry.name));
        }
    }
    (rows, absent)
}

pub fn render(
    baseline: &[Entry],
    current: &[Entry],
    baseline_name: &str,
    current_name: &str,
) -> String {
    let (rows, absent) = rows(baseline, current);
    if rows.is_empty() {
        return "no metrics to compare\n".to_string();
    }

    let header = Row {
        label: String::new(),
        baseline: if baseline.is_empty() {
            String::new()
        } else {
            baseline_name.to_string()
        },
        current: current_name.to_string(),
    };
    let all: Vec<&Row> = std::iter::once(&header).chain(rows.iter()).collect();
    let label_width = all.iter().map(|row| row.label.len()).max().unwrap_or(0) + 2;
    let baseline_width = all.iter().map(|row| row.baseline.len()).max().unwrap_or(0);
    let baseline_width = if baseline_width == 0 {
        0
    } else {
        baseline_width + 2
    };

    let mut out = String::new();
    for row in all {
        let line = format!(
            "{:label_width$}{:baseline_width$}{}",
            row.label, row.baseline, row.current
        );
        out.push_str(line.trim_end());
        out.push('\n');
    }
    for note in absent {
        out.push('\n');
        out.push_str(&note);
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

pub fn compare(
    current_path: &Path,
    current_name: Option<String>,
    baseline_path: Option<&Path>,
    baseline_name: Option<String>,
) -> Result<String> {
    let current = read(current_path)?;
    let baseline = match baseline_path {
        Some(path) => read(path)?,
        None => Vec::new(),
    };
    let named = |given: Option<String>, path: Option<&Path>, fallback: &str| {
        given.unwrap_or_else(|| {
            path.map(name_from_path)
                .unwrap_or_else(|| fallback.to_string())
        })
    };
    Ok(render(
        &baseline,
        &current,
        &named(baseline_name, baseline_path, "baseline"),
        &named(current_name, Some(current_path), "current"),
    ))
}
