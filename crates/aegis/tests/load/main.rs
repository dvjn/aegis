//! A `harness = false` load coordinator with an isolated worker process per observation.

#[path = "../shared/mod.rs"]
mod shared;

mod coordinator;
mod model;
mod payload;
mod report;
mod scenarios;
mod worker;

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand, ValueEnum};

use model::Variant;

#[derive(Parser, Debug)]
#[command(
    about = "Compare two explicit Aegis binaries with isolated load observations",
    after_help = "The output directory receives results.json and summary.tsv. Each selected scenario runs as two balanced pairs in A/B then B/A order."
)]
struct Arguments {
    #[arg(long, help = "Baseline Aegis executable")]
    baseline: Option<PathBuf>,

    #[arg(long, help = "Candidate Aegis executable")]
    candidate: Option<PathBuf>,

    #[arg(long, help = "Directory for results.json and summary.tsv")]
    output: Option<PathBuf>,

    #[arg(long = "suite", help = "Select a suite. Repeatable.")]
    suites: Vec<String>,

    #[arg(
        long = "scenario",
        help = "Select suite::scenario, or a unique scenario name. Repeatable."
    )]
    scenarios: Vec<String>,

    #[arg(long, help = "List registered scenarios and exit")]
    list: bool,

    #[command(subcommand)]
    internal: Option<InternalCommand>,
}

#[derive(Subcommand, Debug)]
enum InternalCommand {
    #[command(name = "__worker", hide = true)]
    Worker {
        #[arg(long)]
        binary: PathBuf,
        #[arg(long)]
        suite: String,
        #[arg(long)]
        scenario: String,
        #[arg(long, value_enum)]
        variant: WorkerVariant,
        #[arg(long)]
        pair: u8,
        #[arg(long)]
        position: u8,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum WorkerVariant {
    Baseline,
    Candidate,
}

impl From<WorkerVariant> for Variant {
    fn from(value: WorkerVariant) -> Self {
        match value {
            WorkerVariant::Baseline => Self::Baseline,
            WorkerVariant::Candidate => Self::Candidate,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    #[cfg(not(target_os = "linux"))]
    bail!("the load harness requires Linux /proc process status metrics");

    let arguments = Arguments::parse();
    if let Some(InternalCommand::Worker {
        binary,
        suite,
        scenario,
        variant,
        pair,
        position,
    }) = arguments.internal
    {
        let observation =
            worker::run(&binary, &suite, &scenario, variant.into(), pair, position).await;
        let stdout = std::io::stdout();
        let mut output = stdout.lock();
        serde_json::to_writer(&mut output, &observation)?;
        writeln!(output)?;
        return Ok(());
    }

    if arguments.list {
        coordinator::list();
        return Ok(());
    }

    let Some(baseline) = arguments.baseline else {
        bail!("--baseline is required");
    };
    let Some(candidate) = arguments.candidate else {
        bail!("--candidate is required");
    };
    let Some(output) = arguments.output else {
        bail!("--output is required");
    };

    let invalid = coordinator::run(coordinator::Options {
        baseline,
        candidate,
        output,
        suites: arguments.suites,
        scenarios: arguments.scenarios,
    })?;
    if invalid > 0 {
        std::process::exit(1);
    }
    Ok(())
}
