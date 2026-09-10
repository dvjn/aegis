//! Peak memory of a child process, from `wait4` or from its scope's cgroup.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const SAMPLE_INTERVAL: Duration = Duration::from_millis(50);

pub const KIB: usize = 1024;
pub const MIB: usize = 1024 * 1024;

pub fn mib(kib: u64) -> f64 {
    kib as f64 / 1024.0
}

pub fn mib_delta(from: u64, to: u64) -> f64 {
    (to as f64 - from as f64) / 1024.0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeakSource {
    MaxRss,
    Cgroup,
    Sampled,
}

impl PeakSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::MaxRss => "ru_maxrss",
            Self::Cgroup => "cgroup",
            Self::Sampled => "sampled",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ChildExit {
    pub status: i32,
    pub max_rss_kib: u64,
}

pub fn reap(pid: u32, block: bool) -> Option<ChildExit> {
    let mut status: libc::c_int = 0;
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let flags = if block { 0 } else { libc::WNOHANG };
    // SAFETY: both out-parameters are live for the call.
    let reaped = unsafe { libc::wait4(pid as libc::pid_t, &mut status, flags, &mut usage) };
    if reaped != pid as libc::pid_t {
        return None;
    }
    Some(ChildExit {
        status: exit_status(status),
        max_rss_kib: max_rss_kib(usage.ru_maxrss),
    })
}

fn exit_status(status: libc::c_int) -> i32 {
    if libc::WIFSIGNALED(status) {
        -libc::WTERMSIG(status)
    } else {
        libc::WEXITSTATUS(status)
    }
}

/// getrusage(2): `ru_maxrss` is kilobytes on Linux and bytes on macOS and iOS.
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn max_rss_kib(value: libc::c_long) -> u64 {
    (value.max(0) as u64) / 1024
}

#[cfg(not(any(target_os = "macos", target_os = "ios")))]
fn max_rss_kib(value: libc::c_long) -> u64 {
    value.max(0) as u64
}

#[cfg(target_os = "linux")]
pub fn read_rss_kib(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|number| number.parse().ok())
}

#[cfg(not(target_os = "linux"))]
pub fn read_rss_kib(_pid: u32) -> Option<u64> {
    None
}

fn cgroup_path_of(pid: u32) -> Option<String> {
    let content = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    content.lines().find_map(|line| {
        let mut fields = line.splitn(3, ':');
        let hierarchy = fields.next()?;
        let _controllers = fields.next()?;
        let path = fields.next()?;
        (hierarchy == "0").then(|| path.to_string())
    })
}

fn process_children(pid: u32) -> Vec<u32> {
    let mut children = Vec::new();
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return children;
    };
    for task in tasks.flatten() {
        if let Ok(listed) = std::fs::read_to_string(task.path().join("children")) {
            children.extend(
                listed
                    .split_whitespace()
                    .filter_map(|entry| entry.parse::<u32>().ok()),
            );
        }
    }
    children
}

pub fn systemd_run_available() -> bool {
    let on_path = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|directory| directory.join("systemd-run").exists());
    on_path
        && std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|value| !value.is_empty())
        && Path::new(CGROUP_ROOT).join("cgroup.controllers").exists()
}

pub fn resolve_cgroup_dir(pid: u32) -> Option<PathBuf> {
    std::iter::once(pid)
        .chain(process_children(pid))
        .filter_map(cgroup_path_of)
        .filter(|path| path.contains(".scope"))
        .map(|path| Path::new(CGROUP_ROOT).join(path.trim_start_matches('/')))
        .find(|candidate| candidate.join("memory.peak").is_file())
}

#[derive(Debug)]
pub struct PeakTracker {
    cgroup_dir: Option<PathBuf>,
    last_peak_kib: AtomicU64,
    seen: AtomicBool,
}

impl PeakTracker {
    pub fn new(cgroup_dir: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            cgroup_dir,
            last_peak_kib: AtomicU64::new(0),
            seen: AtomicBool::new(false),
        })
    }

    pub fn has_cgroup(&self) -> bool {
        self.cgroup_dir.is_some()
    }

    pub fn peak_kib(&self) -> Option<u64> {
        let live = self
            .cgroup_dir
            .as_ref()
            .and_then(|directory| std::fs::read_to_string(directory.join("memory.peak")).ok())
            .and_then(|content| content.trim().parse::<u64>().ok());
        match live {
            Some(bytes) => {
                let kib = bytes / 1024;
                self.last_peak_kib.store(kib, Ordering::Relaxed);
                self.seen.store(true, Ordering::Relaxed);
                Some(kib)
            }
            None => self
                .seen
                .load(Ordering::Relaxed)
                .then(|| self.last_peak_kib.load(Ordering::Relaxed)),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SampledSeries {
    pub gateway: Vec<u64>,
    pub harness_peak_kib: Option<u64>,
}

impl SampledSeries {
    pub fn peak_or(&self, fallback: u64) -> u64 {
        self.gateway.iter().copied().max().unwrap_or(fallback)
    }

    pub fn floor_or(&self, fallback: u64) -> u64 {
        self.gateway.iter().copied().min().unwrap_or(fallback)
    }
}

pub struct Sampler {
    stopping: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<SampledSeries>>,
}

impl Sampler {
    pub fn start(pid: u32, peaks: Arc<PeakTracker>, harness_pid: Option<u32>) -> Self {
        let stopping = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stopping);
        let handle = std::thread::spawn(move || {
            let mut series = SampledSeries::default();
            while !stop_flag.load(Ordering::Relaxed) {
                if let Some(companion) = harness_pid.and_then(read_rss_kib) {
                    series.harness_peak_kib =
                        Some(series.harness_peak_kib.unwrap_or(0).max(companion));
                }
                peaks.peak_kib();
                match read_rss_kib(pid) {
                    Some(rss) => series.gateway.push(rss),
                    None => break,
                }
                std::thread::sleep(SAMPLE_INTERVAL);
            }
            series
        });
        Self {
            stopping,
            handle: Some(handle),
        }
    }

    pub fn stop(mut self) -> SampledSeries {
        self.stopping.store(true, Ordering::Relaxed);
        self.handle
            .take()
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default()
    }
}
