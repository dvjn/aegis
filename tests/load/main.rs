//! A `harness = false` target: `main` gets argv, and `CARGO_BIN_EXE_aegis` is built in this profile.

#[path = "../shared/mod.rs"]
mod shared;

mod analysis;
mod cases;
mod context;
mod harness;
mod report;
mod runner;

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, ValueEnum};

use cases::Profile;
use harness::Request;
use runner::Status;
use shared::gateway::PeakMode;
use shared::upstream::Upstream;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ProfileArgument {
    Quick,
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum PeakModeArgument {
    Maxrss,
    Cgroup,
}

#[derive(Parser, Debug)]
#[command(
    about = "Memory and OOM load suite for the aegis gateway",
    long_about = "Drives synthetic traffic through a real `aegis serve` against a fake upstream, \
and reports the gateway's peak memory per dimension, a fitted cost model per axis, and the \
in-flight payload that gets it killed under a memory cap.",
    after_help = "ENVIRONMENT
  AEGIS_LOAD_PROFILE         quick (default) | full
  AEGIS_LOAD_PEAK_MODE       maxrss (default) | cgroup
  AEGIS_LOAD_OOM_MEMORY_MAX  a systemd size, 500M by default
  AEGIS_LOAD_BENCHMARK_DIR   write one benchmark fragment per case here

A nextest run has no argv to spend on flags, so it takes these instead. The
matching flag overrides the variable."
)]
struct Arguments {
    #[arg(long = "case", help = "Run only this case. Repeatable.")]
    cases: Vec<String>,

    #[arg(long = "list-cases", help = "List the cases and exit.")]
    list: bool,

    #[arg(long, value_enum, help = "How many values each dimension sweeps.")]
    profile: Option<ProfileArgument>,

    #[arg(long, value_enum, help = "Which exact peak figure to report.")]
    peak_mode: Option<PeakModeArgument>,

    #[arg(long, help = "The memory cap for the OOM bisection case.")]
    oom_memory_max: Option<String>,

    #[arg(long, help = "Keep each case's working directory and print the path.")]
    keep: bool,

    #[arg(long, help = "Print the detail lines for passing cases too.")]
    verbose: bool,

    #[arg(long, help = "Write JUnit XML to this path.")]
    junit: Option<PathBuf>,

    #[arg(long, help = "Write benchmark JSON to this path.")]
    benchmark_json: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // reqwest is built on rustls-no-provider, so building a Client panics
    // until a provider is installed.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let all = cases::all();
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match Request::parse(&arguments) {
        Some(Request::List { ignored_only }) => {
            harness::list(&all, ignored_only);
            Ok(())
        }
        Some(Request::Run { name }) => {
            if harness::run_one(&all, &name).await? != Status::Pass {
                std::process::exit(1);
            }
            Ok(())
        }
        None => run_every_case(&all).await,
    }
}

async fn run_every_case(all: &[cases::Case]) -> Result<()> {
    let arguments = Arguments::parse();

    let selected: Vec<&cases::Case> = all
        .iter()
        .filter(|case| {
            arguments.cases.is_empty() || arguments.cases.iter().any(|name| name == case.name)
        })
        .collect();
    if selected.is_empty() {
        bail!("no cases matched");
    }
    if arguments.list {
        for case in selected {
            println!("memory/{}: {}", case.name, case.description);
        }
        return Ok(());
    }

    let mut settings = harness::settings_from_environment()?;
    if let Some(profile) = arguments.profile {
        settings.profile = match profile {
            ProfileArgument::Quick => Profile::Quick,
            ProfileArgument::Full => Profile::Full,
        };
    }
    if let Some(limit) = &arguments.oom_memory_max {
        settings.oom_memory_max = limit.clone();
    }
    let peak_mode = match arguments.peak_mode {
        Some(PeakModeArgument::Maxrss) => PeakMode::MaxRss,
        Some(PeakModeArgument::Cgroup) => PeakMode::Cgroup,
        None => harness::peak_mode_from_environment()?,
    };

    let upstream = Upstream::start().await;
    println!(
        "fake upstream on {}, aegis binary {}, profile {}, peaks from {}",
        upstream.base_url(),
        env!("CARGO_BIN_EXE_aegis"),
        match settings.profile {
            Profile::Quick => "quick",
            Profile::Full => "full",
        },
        match peak_mode {
            PeakMode::MaxRss => "ru_maxrss",
            PeakMode::Cgroup => "cgroup memory.peak",
        }
    );

    let mut records = Vec::new();
    for case in selected {
        records.push(
            runner::run_case(
                case,
                &settings,
                &upstream,
                peak_mode,
                arguments.keep,
                arguments.verbose,
            )
            .await,
        );
    }
    let failed = runner::print_summary(&records);

    if let Some(path) = &arguments.junit {
        report::write_junit(&records, path)?;
        println!("junit xml written to {}", path.display());
    }
    if let Some(path) = &arguments.benchmark_json {
        report::write_benchmark(&records, path)?;
        println!("benchmark json written to {}", path.display());
    }

    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}
