use std::collections::BTreeSet;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::model::{BinaryMetadata, HostMetadata, Observation, RunReport, SCHEMA_VERSION, Variant};
use crate::report;
use crate::scenarios::{self, Scenario};
use crate::shared::measure::SAMPLE_INTERVAL;

const PAIRS_PER_SCENARIO: u8 = 2;
const WORKER_TIMEOUT: Duration = Duration::from_secs(480);

pub struct Options {
    pub baseline: PathBuf,
    pub candidate: PathBuf,
    pub output: PathBuf,
    pub suites: Vec<String>,
    pub scenarios: Vec<String>,
}

pub fn list() {
    for scenario in scenarios::all() {
        println!("{}\t{}", scenario.key(), scenario.description);
    }
}

pub fn run(options: Options) -> Result<usize> {
    let baseline_path = validate_binary(&options.baseline, "baseline")?;
    let candidate_path = validate_binary(&options.candidate, "candidate")?;
    let baseline = binary_metadata(&baseline_path)?;
    let candidate = binary_metadata(&candidate_path)?;
    let selected = select(&options.suites, &options.scenarios)?;
    let executable = std::env::current_exe().context("locating load worker executable")?;

    let mut observations = Vec::with_capacity(selected.len() * 4);
    for scenario in selected {
        println!("running {}", scenario.key());
        let orders = [
            [Variant::Baseline, Variant::Candidate],
            [Variant::Candidate, Variant::Baseline],
        ];
        for (pair_index, order) in orders.into_iter().enumerate() {
            for (position_index, variant) in order.into_iter().enumerate() {
                let pair = pair_index as u8 + 1;
                let position = position_index as u8 + 1;
                let binary = match variant {
                    Variant::Baseline => &baseline_path,
                    Variant::Candidate => &candidate_path,
                };
                let observation =
                    run_worker(&executable, binary, scenario, variant, pair, position);
                println!(
                    "  pair {pair} position {position} {:9} {}",
                    variant.label(),
                    if observation.valid {
                        "valid"
                    } else {
                        "invalid"
                    }
                );
                observations.push(observation);
            }
        }
    }

    let invalid = observations
        .iter()
        .filter(|observation| !observation.valid)
        .count();
    let (paired_deltas, summaries) = report::comparisons(&observations);
    let report = RunReport {
        schema_version: SCHEMA_VERSION,
        baseline,
        candidate,
        host: host_metadata(),
        sample_interval_ms: SAMPLE_INTERVAL.as_millis() as u64,
        pairs_per_scenario: PAIRS_PER_SCENARIO,
        observations,
        paired_deltas,
        summaries,
    };
    report::write(&report, &options.output)?;
    report::print_summary(&report);
    Ok(invalid)
}

fn select(suites: &[String], selectors: &[String]) -> Result<Vec<&'static Scenario>> {
    let known_suites = scenarios::all()
        .iter()
        .map(|scenario| scenario.suite)
        .collect::<BTreeSet<_>>();
    for suite in suites {
        if !known_suites.contains(suite.as_str()) {
            bail!("unknown suite {suite:?}");
        }
    }

    let mut selected_keys = BTreeSet::new();
    for selector in selectors {
        if let Some((suite, name)) = selector.split_once("::") {
            let Some(scenario) = scenarios::find(suite, name) else {
                bail!("unknown scenario {selector:?}");
            };
            selected_keys.insert((scenario.suite, scenario.name));
            continue;
        }
        let matches = scenarios::all()
            .iter()
            .filter(|scenario| scenario.name == selector)
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => bail!("unknown scenario {selector:?}"),
            [scenario] => {
                selected_keys.insert((scenario.suite, scenario.name));
            }
            _ => bail!("ambiguous scenario {selector:?}; use suite::scenario"),
        }
    }

    let selected = scenarios::all()
        .iter()
        .filter(|scenario| suites.is_empty() || suites.iter().any(|suite| suite == scenario.suite))
        .filter(|scenario| {
            selected_keys.is_empty() || selected_keys.contains(&(scenario.suite, scenario.name))
        })
        .collect::<Vec<_>>();
    if selected.is_empty() {
        bail!("suite and scenario filters selected no scenarios");
    }
    Ok(selected)
}

fn validate_binary(path: &Path, label: &str) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("resolving {label} binary {}", path.display()))?;
    let metadata = std::fs::metadata(&canonical)
        .with_context(|| format!("reading {label} binary {}", canonical.display()))?;
    if !metadata.is_file() {
        bail!("{label} binary is not a file: {}", canonical.display());
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        bail!("{label} binary is not executable: {}", canonical.display());
    }
    Ok(canonical)
}

fn binary_metadata(path: &Path) -> Result<BinaryMetadata> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    let digest = hash.finalize();
    let sha256 = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(BinaryMetadata {
        path: path.display().to_string(),
        sha256,
    })
}

fn host_metadata() -> HostMetadata {
    HostMetadata {
        hostname: read_trimmed("/proc/sys/kernel/hostname"),
        kernel_release: read_trimmed("/proc/sys/kernel/osrelease"),
        architecture: std::env::consts::ARCH.to_string(),
        operating_system: std::env::consts::OS.to_string(),
    }
}

fn read_trimmed(path: &str) -> String {
    std::fs::read_to_string(path)
        .map(|value| value.trim().chars().take(256).collect())
        .unwrap_or_else(|_| "unknown".to_string())
}

fn run_worker(
    executable: &Path,
    binary: &Path,
    scenario: &Scenario,
    variant: Variant,
    pair: u8,
    position: u8,
) -> Observation {
    match spawn_worker(executable, binary, scenario, variant, pair, position) {
        Ok(observation) => observation,
        Err(error) => {
            let mut observation = Observation::new(
                scenario.suite,
                scenario.name,
                variant,
                pair,
                position,
                scenario.inputs(),
            );
            observation.invalidate(format!("worker process failed: {error:#}"));
            observation
        }
    }
}

fn spawn_worker(
    executable: &Path,
    binary: &Path,
    scenario: &Scenario,
    variant: Variant,
    pair: u8,
    position: u8,
) -> Result<Observation> {
    let mut command = Command::new(executable);
    command
        .arg("__worker")
        .arg("--binary")
        .arg(binary)
        .arg("--suite")
        .arg(scenario.suite)
        .arg("--scenario")
        .arg(scenario.name)
        .arg("--variant")
        .arg(variant.label())
        .arg("--pair")
        .arg(pair.to_string())
        .arg("--position")
        .arg(position.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().context("spawning observation worker")?;
    let process_group = child.id() as libc::pid_t;
    let deadline = Instant::now() + WORKER_TIMEOUT;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            kill_process_group(process_group);
            let output = child.wait_with_output()?;
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("timed out after {WORKER_TIMEOUT:?}: {stderr}");
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    let output = child.wait_with_output()?;
    if process_group_exists(process_group) {
        kill_process_group(process_group);
        bail!("worker exited with a surviving child process");
    }
    if !output.status.success() {
        bail!(
            "exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    if output.stdout.len() > 64 * 1024 {
        bail!("emitted more than 64 KiB of JSON");
    }
    let observation: Observation =
        serde_json::from_slice(&output.stdout).context("decoding worker JSON")?;
    if observation.schema_version != SCHEMA_VERSION {
        bail!("worker emitted schema {:?}", observation.schema_version);
    }
    if observation.suite != scenario.suite
        || observation.scenario != scenario.name
        || observation.variant != variant
        || observation.pair != pair
        || observation.position != position
    {
        bail!("worker emitted mismatched observation identity");
    }
    Ok(observation)
}

fn process_group_exists(process_group: libc::pid_t) -> bool {
    let result = unsafe { libc::kill(-process_group, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn kill_process_group(process_group: libc::pid_t) {
    unsafe {
        libc::kill(-process_group, libc::SIGKILL);
    }
}
