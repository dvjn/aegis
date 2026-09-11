use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

pub const KIB: usize = 1024;
pub const MIB: usize = 1024 * 1024;
pub const SAMPLE_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcStatus {
    pub vm_rss_kib: u64,
    pub vm_hwm_kib: u64,
}

#[cfg(target_os = "linux")]
pub fn read_proc_status(pid: u32) -> Result<ProcStatus> {
    let path = format!("/proc/{pid}/status");
    let content = std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
    parse_proc_status(&content).with_context(|| format!("parsing {path}"))
}

#[cfg(not(target_os = "linux"))]
pub fn read_proc_status(_pid: u32) -> Result<ProcStatus> {
    anyhow::bail!("process RSS sampling requires Linux /proc")
}

#[cfg(target_os = "linux")]
fn parse_proc_status(content: &str) -> Result<ProcStatus> {
    fn value(content: &str, field: &str) -> Option<u64> {
        content.lines().find_map(|line| {
            line.strip_prefix(field)
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|number| number.parse().ok())
        })
    }

    let vm_rss_kib = value(content, "VmRSS:").context("VmRSS is missing")?;
    let vm_hwm_kib = value(content, "VmHWM:").context("VmHWM is missing")?;
    Ok(ProcStatus {
        vm_rss_kib,
        vm_hwm_kib,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RssStability {
    pub rss_kib: u64,
    pub vm_hwm_kib: u64,
    pub sample_count: u64,
    pub spread_kib: u64,
    pub stable: bool,
}

pub async fn stabilize_rss(
    pid: u32,
    timeout: Duration,
    window_size: usize,
    tolerance_kib: u64,
) -> Result<RssStability> {
    let deadline = Instant::now() + timeout;
    let mut window = VecDeque::with_capacity(window_size);
    let mut sample_count = 0;
    let mut vm_hwm_kib = 0;
    loop {
        let status = read_proc_status(pid)?;
        sample_count += 1;
        vm_hwm_kib = vm_hwm_kib.max(status.vm_hwm_kib);
        if window.len() == window_size {
            window.pop_front();
        }
        window.push_back(status.vm_rss_kib);
        if window.len() == window_size {
            let spread_kib = spread(&window);
            if spread_kib <= tolerance_kib {
                return Ok(RssStability {
                    rss_kib: median(&window),
                    vm_hwm_kib,
                    sample_count,
                    spread_kib,
                    stable: true,
                });
            }
        }
        if Instant::now() >= deadline {
            return Ok(RssStability {
                rss_kib: median(&window),
                vm_hwm_kib,
                sample_count,
                spread_kib: spread(&window),
                stable: false,
            });
        }
        tokio::time::sleep(SAMPLE_INTERVAL).await;
    }
}

fn median(values: &VecDeque<u64>) -> u64 {
    let mut sorted = values.iter().copied().collect::<Vec<_>>();
    sorted.sort_unstable();
    sorted.get(sorted.len() / 2).copied().unwrap_or_default()
}

fn spread(values: &VecDeque<u64>) -> u64 {
    let minimum = values.iter().min().copied().unwrap_or_default();
    values.iter().max().copied().unwrap_or_default() - minimum
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RssSamples {
    pub peak_kib: Option<u64>,
    pub sample_count: u64,
    pub read_failures: u64,
}

pub struct RssSampler {
    stopping: Arc<AtomicBool>,
    sample_count: Arc<AtomicU64>,
    handle: Option<std::thread::JoinHandle<RssSamples>>,
}

impl RssSampler {
    pub fn start(pid: u32, ready_rss_kib: u64) -> Self {
        let stopping = Arc::new(AtomicBool::new(false));
        let sample_count = Arc::new(AtomicU64::new(1));
        let stop_flag = Arc::clone(&stopping);
        let shared_count = Arc::clone(&sample_count);
        let handle = std::thread::spawn(move || {
            let mut samples = RssSamples {
                peak_kib: Some(ready_rss_kib),
                sample_count: 1,
                read_failures: 0,
            };
            while !stop_flag.load(Ordering::Relaxed) {
                std::thread::sleep(SAMPLE_INTERVAL);
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                match read_proc_status(pid) {
                    Ok(status) => {
                        samples.peak_kib =
                            Some(samples.peak_kib.unwrap_or_default().max(status.vm_rss_kib));
                        samples.sample_count += 1;
                        shared_count.store(samples.sample_count, Ordering::Relaxed);
                    }
                    Err(_) => {
                        samples.read_failures += 1;
                        break;
                    }
                }
            }
            samples
        });
        Self {
            stopping,
            sample_count,
            handle: Some(handle),
        }
    }

    pub fn sample_count(&self) -> u64 {
        self.sample_count.load(Ordering::Relaxed)
    }

    pub async fn wait_for_samples(&self, minimum: u64, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.sample_count() >= minimum {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        self.sample_count() >= minimum
    }

    pub fn stop(mut self) -> RssSamples {
        self.stopping.store(true, Ordering::Relaxed);
        self.handle
            .take()
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default()
    }
}

impl Drop for RssSampler {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
