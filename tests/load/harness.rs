//! https://nexte.st/docs/design/custom-test-harnesses

use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::cases::{Case, Profile, Settings};
use crate::report;
use crate::runner::{self, Status};
use crate::shared::gateway::PeakMode;
use crate::shared::upstream::Upstream;

const PROFILE_VARIABLE: &str = "AEGIS_LOAD_PROFILE";
const PEAK_MODE_VARIABLE: &str = "AEGIS_LOAD_PEAK_MODE";
const OOM_MEMORY_MAX_VARIABLE: &str = "AEGIS_LOAD_OOM_MEMORY_MAX";
const BENCHMARK_DIR_VARIABLE: &str = "AEGIS_LOAD_BENCHMARK_DIR";

const DEFAULT_OOM_MEMORY_MAX: &str = "500M";

pub enum Request {
    List { ignored_only: bool },
    Run { name: String },
}

impl Request {
    pub fn parse(arguments: &[String]) -> Option<Self> {
        let flag = |name: &str| arguments.iter().any(|argument| argument == name);
        if flag("--list") {
            return Some(Self::List {
                ignored_only: flag("--ignored"),
            });
        }
        if !flag("--exact") {
            return None;
        }
        let name = arguments
            .iter()
            .find(|argument| !argument.starts_with('-'))?;
        Some(Self::Run { name: name.clone() })
    }
}

pub fn settings_from_environment() -> Result<Settings> {
    let profile = match std::env::var(PROFILE_VARIABLE).as_deref() {
        Ok("full") => Profile::Full,
        Ok("quick") | Err(_) => Profile::Quick,
        Ok(other) => bail!("{PROFILE_VARIABLE} must be quick or full, not {other:?}"),
    };
    Ok(Settings {
        profile,
        oom_memory_max: std::env::var(OOM_MEMORY_MAX_VARIABLE)
            .unwrap_or_else(|_| DEFAULT_OOM_MEMORY_MAX.to_string()),
    })
}

pub fn peak_mode_from_environment() -> Result<PeakMode> {
    match std::env::var(PEAK_MODE_VARIABLE).as_deref() {
        Ok("cgroup") => Ok(PeakMode::Cgroup),
        Ok("maxrss") | Err(_) => Ok(PeakMode::MaxRss),
        Ok(other) => bail!("{PEAK_MODE_VARIABLE} must be maxrss or cgroup, not {other:?}"),
    }
}

pub fn list(cases: &[Case], ignored_only: bool) {
    if ignored_only {
        return;
    }
    for case in cases {
        println!("{}: test", case.name);
    }
}

pub async fn run_one(cases: &[Case], name: &str) -> Result<Status> {
    let Some(case) = cases.iter().find(|case| case.name == name) else {
        bail!("no such case: {name}");
    };
    let settings = settings_from_environment()?;
    let peak_mode = peak_mode_from_environment()?;

    let upstream = Upstream::start().await;
    let record = runner::run_case(case, &settings, &upstream, peak_mode, false, true).await;
    for line in report::metric_lines(&record) {
        println!("{line}");
    }

    if let Some(directory) = std::env::var_os(BENCHMARK_DIR_VARIABLE) {
        let directory = PathBuf::from(directory);
        std::fs::create_dir_all(&directory)?;
        let fragment = directory.join(format!("{}.json", case.name));
        report::write_benchmark(std::slice::from_ref(&record), &fragment)?;
        println!("benchmark fragment written to {}", fragment.display());
    }
    Ok(record.status)
}
