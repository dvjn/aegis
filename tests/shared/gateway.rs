use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use axum::body::Bytes;
use serde_json::json;

use super::measure::{
    ChildExit, KIB, MIB, PeakSource, PeakTracker, reap, resolve_cgroup_dir, systemd_run_available,
};
use super::upstream::Upstream;

/// `MAX_REQUEST_BYTES` in src/gateway/mod.rs, matched by `DefaultBodyLimit::max` in src/app.rs.
pub const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;

pub const DEFAULT_CAPTURE_BYTES: usize = 16 * MIB;

const API_KEY_HEADER: &str = "x-aegis-api-key";
const CLAUDE_MODEL: &str = "claude-sonnet-4-5-20250929";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const TERMINATE_TIMEOUT: Duration = Duration::from_secs(10);

/// Fake credentials matching the built-in detectors in src/policies/secrets.rs.
pub fn fake_secrets() -> [(&'static str, String); 3] {
    [
        ("github_token", format!("ghp_{}", "a".repeat(36))),
        (
            "anthropic_api_key",
            format!("sk-ant-api03-{}", "b".repeat(95)),
        ),
        ("aws_access_key_id", format!("AKIA{}", "C".repeat(16))),
    ]
}

pub fn distinct_secrets(count: usize) -> Vec<String> {
    (0..count).map(|index| format!("ghp_{index:036}")).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuardrailsMode {
    Off,
    Observe,
    Mask,
}

impl GuardrailsMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Observe => "observe",
            Self::Mask => "mask",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestEncoding {
    Identity,
    Gzip,
    Zstd,
    Brotli,
}

impl RequestEncoding {
    pub fn all() -> [Self; 4] {
        [Self::Identity, Self::Gzip, Self::Zstd, Self::Brotli]
    }

    pub fn header_value(self) -> Option<&'static str> {
        match self {
            Self::Identity => None,
            Self::Gzip => Some("gzip"),
            Self::Zstd => Some("zstd"),
            Self::Brotli => Some("br"),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::Gzip => "gzip",
            Self::Zstd => "zstd",
            Self::Brotli => "br",
        }
    }
}

/// Encoded so the declared header takes the `decode_declared` path in src/compression.rs.
pub fn encode_request(body: &[u8], encoding: RequestEncoding) -> Result<Bytes> {
    let encoded = match encoding {
        RequestEncoding::Identity => body.to_vec(),
        RequestEncoding::Gzip => {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder.write_all(body)?;
            encoder.finish()?
        }
        RequestEncoding::Zstd => zstd::stream::encode_all(body, 1)?,
        RequestEncoding::Brotli => {
            let mut encoded = Vec::new();
            let mut encoder = brotli::CompressorWriter::new(&mut encoded, 4096, 1, 22);
            encoder.write_all(body)?;
            drop(encoder);
            encoded
        }
    };
    Ok(Bytes::from(encoded))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadShape {
    OneLongString,
    ManySmallBlocks,
}

impl PayloadShape {
    pub fn label(self) -> &'static str {
        match self {
            Self::OneLongString => "string",
            Self::ManySmallBlocks => "objects",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeakMode {
    MaxRss,
    Cgroup,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemoryCap {
    None,
    Scope,
    AddressSpace,
}

impl MemoryCap {
    pub fn was_breached_by(&self, exit_status: Option<i32>) -> bool {
        match (self, exit_status) {
            (Self::Scope, Some(status)) => status == -libc::SIGKILL,
            (Self::AddressSpace, Some(status)) => status != 0,
            _ => false,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::None => "absent",
            Self::Scope => "MemoryMax",
            Self::AddressSpace => "RLIMIT_AS",
        }
    }
}

#[derive(Clone, Debug)]
pub struct GatewayOptions {
    pub guardrails: GuardrailsMode,
    pub max_capture_bytes: usize,
    pub memory_max: Option<String>,
    pub peak_mode: PeakMode,
    pub keep_workdir: bool,
}

impl Default for GatewayOptions {
    fn default() -> Self {
        Self {
            guardrails: GuardrailsMode::Mask,
            max_capture_bytes: DEFAULT_CAPTURE_BYTES,
            memory_max: None,
            peak_mode: PeakMode::MaxRss,
            keep_workdir: false,
        }
    }
}

pub struct Response {
    pub status: Option<u16>,
    pub body: String,
    pub bytes_read: usize,
    pub seconds: f64,
    pub error: Option<String>,
}

impl Response {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).expect("response body was not JSON")
    }
}

#[derive(Clone, Debug)]
pub struct Shutdown {
    pub peak_kib: Option<u64>,
    pub peak_source: PeakSource,
    pub exit_status: Option<i32>,
    pub kept_workdir: Option<PathBuf>,
}

pub struct Gateway {
    options: GatewayOptions,
    workdir: PathBuf,
    port: u16,
    child: Mutex<Child>,
    pid: u32,
    scope_unit: Option<String>,
    cap: MemoryCap,
    peaks: Arc<PeakTracker>,
    exit: Mutex<Option<ChildExit>>,
    shutdown: Mutex<Option<Shutdown>>,
    api_key: String,
    client: reqwest::Client,
    pub notes: Vec<String>,
    pub upstream: Upstream,
}

impl Gateway {
    pub async fn started() -> Self {
        Self::started_with(GuardrailsMode::Mask).await
    }

    pub async fn started_with(guardrails: GuardrailsMode) -> Self {
        let options = GatewayOptions {
            guardrails,
            ..GatewayOptions::default()
        };
        let upstream = Upstream::start().await;
        Self::start(&upstream, options).expect("gateway did not start")
    }

    pub fn start(upstream: &Upstream, options: GatewayOptions) -> Result<Self> {
        install_crypto_provider();
        let binary = Path::new(env!("CARGO_BIN_EXE_aegis"));
        let workdir = std::env::temp_dir().join(format!("aegis-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(workdir.join("data"))?;

        let root_key = workdir.join("data").join("root.key");
        std::fs::write(&root_key, random_bytes(32)?)?;
        set_owner_only(&root_key)?;

        let port = free_port()?;
        std::fs::write(
            workdir.join("aegis.toml"),
            config_file(port, upstream.base_url(), &options),
        )?;

        let api_key = provision_credentials(binary, &workdir)?;
        let mut notes = Vec::new();
        let scope = ScopePlan::decide(&options, &mut notes);
        let child = spawn_gateway(binary, &workdir, &scope)?;
        let pid = child.id();

        let mut gateway = Self {
            options,
            workdir,
            port,
            child: Mutex::new(child),
            pid,
            scope_unit: scope.unit.clone(),
            cap: scope.cap.clone(),
            peaks: PeakTracker::new(None),
            exit: Mutex::new(None),
            shutdown: Mutex::new(None),
            api_key,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .build()?,
            notes,
            upstream: upstream.clone(),
        };

        if let Err(error) = gateway.wait_for_port() {
            let tail = gateway.log_tail(20);
            gateway.attach_cgroup();
            gateway.shut_down();
            return Err(error.context(format!("gateway did not start; log tail:\n{tail}")));
        }
        gateway.attach_cgroup();
        Ok(gateway)
    }

    fn attach_cgroup(&mut self) {
        if self.scope_unit.is_none() {
            return;
        }
        let cgroup_dir = resolve_cgroup_dir(self.pid);
        if cgroup_dir.is_none() && self.options.peak_mode == PeakMode::Cgroup {
            self.notes
                .push("could not resolve the scope cgroup; reporting ru_maxrss instead".into());
        }
        self.peaks = PeakTracker::new(cgroup_dir);
        self.peaks.peak_kib();
    }

    pub fn describe(&self) -> String {
        let mut parts = vec![
            format!("guardrails {}", self.options.guardrails.label()),
            format!("max_capture {} KiB", self.options.max_capture_bytes / KIB),
        ];
        if let Some(limit) = &self.options.memory_max {
            parts.push(match self.cap {
                MemoryCap::Scope => format!("MemoryMax {limit}"),
                MemoryCap::AddressSpace => format!("RLIMIT_AS {limit}"),
                MemoryCap::None => format!("{limit} requested, uncapped"),
            });
        }
        parts.join(", ")
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn peaks(&self) -> &Arc<PeakTracker> {
        &self.peaks
    }

    pub fn memory_cap(&self) -> &MemoryCap {
        &self.cap
    }

    pub fn database_path(&self) -> PathBuf {
        self.workdir.join("data/aegis.db")
    }

    pub fn provider_url(&self, provider: &str, path: &str, query: &str) -> String {
        let separator = if query.is_empty() { "" } else { "?" };
        format!(
            "{}/providers/{provider}{path}{separator}{query}",
            self.base_url()
        )
    }

    pub fn claude_url(&self, query: &str) -> String {
        self.provider_url("claude", "/v1/messages", query)
    }

    pub fn exit_status(&self) -> Option<i32> {
        let mut exit = self.exit.lock().expect("exit lock");
        if exit.is_none() {
            *exit = reap(self.pid, false);
        }
        exit.map(|exit| exit.status)
    }

    fn wait_for_port(&self) -> Result<()> {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while Instant::now() < deadline {
            if let Some(status) = self.exit_status() {
                bail!("gateway exited early with status {status}");
            }
            if std::net::TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        bail!(
            "gateway did not open port {} within {STARTUP_TIMEOUT:?}",
            self.port
        )
    }

    pub fn log_tail(&self, lines: usize) -> String {
        let Ok(log) = std::fs::read_to_string(self.workdir.join("gateway.log")) else {
            return "(no gateway log)".to_string();
        };
        let collected: Vec<&str> = log.lines().collect();
        collected[collected.len().saturating_sub(lines)..].join("\n")
    }

    pub fn request_body(
        &self,
        padding_bytes: usize,
        secret_count: Option<usize>,
        shape: PayloadShape,
        block_bytes: usize,
    ) -> Bytes {
        let secrets = match secret_count {
            None => fake_secrets().into_iter().map(|(_, value)| value).collect(),
            Some(count) => distinct_secrets(count),
        };
        let mut preamble = String::from("review this config");
        for secret in &secrets {
            preamble.push(' ');
            preamble.push_str(secret);
        }

        let content = match shape {
            PayloadShape::OneLongString => {
                json!(format!("{preamble} {}", "y".repeat(padding_bytes)))
            }
            PayloadShape::ManySmallBlocks => {
                let filler = "y".repeat(block_bytes.max(1));
                // `{"type":"text","text":""},` as serialised here.
                const BLOCK_OVERHEAD: usize = 32;
                let per_block = block_bytes.max(1) + BLOCK_OVERHEAD;
                let mut blocks = vec![json!({ "type": "text", "text": preamble })];
                for _ in 0..(padding_bytes / per_block).max(1) {
                    blocks.push(json!({ "type": "text", "text": filler }));
                }
                json!(blocks)
            }
        };

        Bytes::from(serde_json::to_vec(&messages_document(CLAUDE_MODEL, content)).expect("body"))
    }

    pub fn text_body(&self, include_secrets: bool) -> String {
        self.text_body_for(CLAUDE_MODEL, include_secrets)
    }

    pub fn text_body_for(&self, model: &str, include_secrets: bool) -> String {
        let mut content = "review this config".to_owned();
        if include_secrets {
            for (_, value) in fake_secrets() {
                content.push(' ');
                content.push_str(&value);
            }
        }
        messages_document(model, json!(content)).to_string()
    }

    pub async fn post_text(&self, url: &str, body: String) -> Response {
        self.post_text_with_key(url, body, Some(self.api_key.clone()))
            .await
    }

    pub async fn post_text_with_key(
        &self,
        url: &str,
        body: String,
        key: Option<String>,
    ) -> Response {
        let started = Instant::now();
        let mut request = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json");
        if let Some(key) = key {
            request = request.header(API_KEY_HEADER, key);
        }
        match request.body(body).send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                match response.text().await {
                    Ok(body) => Response {
                        status: Some(status),
                        bytes_read: body.len(),
                        body,
                        seconds: started.elapsed().as_secs_f64(),
                        error: None,
                    },
                    Err(error) => Response {
                        status: Some(status),
                        body: String::new(),
                        bytes_read: 0,
                        seconds: started.elapsed().as_secs_f64(),
                        error: Some(error.to_string()),
                    },
                }
            }
            Err(error) => Response {
                status: None,
                body: String::new(),
                bytes_read: 0,
                seconds: started.elapsed().as_secs_f64(),
                error: Some(error.to_string()),
            },
        }
    }

    pub async fn post(
        &self,
        url: &str,
        body: Bytes,
        encoding: RequestEncoding,
        timeout: Duration,
    ) -> Response {
        let started = Instant::now();
        let mut request = self
            .client
            .post(url)
            .timeout(timeout)
            .header("content-type", "application/json")
            .header(API_KEY_HEADER, &self.api_key);
        if let Some(value) = encoding.header_value() {
            request = request.header("content-encoding", value);
        }
        match request.body(body).send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                match response.bytes().await {
                    Ok(payload) => Response {
                        status: Some(status),
                        body: String::new(),
                        bytes_read: payload.len(),
                        seconds: started.elapsed().as_secs_f64(),
                        error: (status != 200).then(|| format!("http {status}")),
                    },
                    Err(error) => Response {
                        status: Some(status),
                        body: String::new(),
                        bytes_read: 0,
                        seconds: started.elapsed().as_secs_f64(),
                        error: Some(error.to_string()),
                    },
                }
            }
            Err(error) => Response {
                status: None,
                body: String::new(),
                bytes_read: 0,
                seconds: started.elapsed().as_secs_f64(),
                error: Some(error.to_string()),
            },
        }
    }

    /// Dropping the socket is what sets `disconnected` on the relay task in src/gateway/mod.rs.
    pub async fn post_and_abandon(&self, url: &str, body: Bytes, read_bytes: usize) -> Response {
        use futures_util::StreamExt;

        let started = Instant::now();
        let sent = self
            .client
            .post(url)
            .timeout(Duration::from_secs(60))
            .header("content-type", "application/json")
            .header(API_KEY_HEADER, &self.api_key)
            .body(body)
            .send()
            .await;
        match sent {
            Ok(response) => {
                let status = response.status().as_u16();
                let mut stream = response.bytes_stream();
                let mut read = 0;
                while read < read_bytes {
                    match stream.next().await {
                        Some(Ok(chunk)) => read += chunk.len(),
                        Some(Err(_)) | None => break,
                    }
                }
                drop(stream);
                Response {
                    status: Some(status),
                    body: String::new(),
                    bytes_read: read,
                    seconds: started.elapsed().as_secs_f64(),
                    error: None,
                }
            }
            Err(error) => Response {
                status: None,
                body: String::new(),
                bytes_read: 0,
                seconds: started.elapsed().as_secs_f64(),
                error: Some(error.to_string()),
            },
        }
    }

    pub fn shut_down(&self) -> Shutdown {
        if let Some(done) = self.shutdown.lock().expect("shutdown lock").clone() {
            return done;
        }
        let cgroup_peak = self.peaks.peak_kib();
        self.terminate();
        self.stop_scope();

        let exit = *self.exit.lock().expect("exit lock");
        let (peak_kib, peak_source) = match self.options.peak_mode {
            PeakMode::Cgroup if cgroup_peak.is_some() => (cgroup_peak, PeakSource::Cgroup),
            _ => (exit.map(|exit| exit.max_rss_kib), PeakSource::MaxRss),
        };
        let kept_workdir = if self.options.keep_workdir {
            Some(self.workdir.clone())
        } else {
            let _ = std::fs::remove_dir_all(&self.workdir);
            None
        };

        let done = Shutdown {
            peak_kib,
            peak_source,
            exit_status: exit.map(|exit| exit.status),
            kept_workdir,
        };
        *self.shutdown.lock().expect("shutdown lock") = Some(done.clone());
        done
    }

    fn terminate(&self) {
        if self.exit_status().is_some() {
            return;
        }
        // SAFETY: signalling a pid this process owns.
        unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGTERM) };
        let deadline = Instant::now() + TERMINATE_TIMEOUT;
        while Instant::now() < deadline {
            if self.exit_status().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.lock().expect("child lock").kill();
        *self.exit.lock().expect("exit lock") = reap(self.pid, true);
    }

    fn stop_scope(&self) {
        let Some(unit) = &self.scope_unit else {
            return;
        };
        let _ = Command::new("systemctl")
            .args(["--user", "stop", unit])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.shut_down();
    }
}

fn messages_document(model: &str, content: serde_json::Value) -> serde_json::Value {
    json!({
        "model": model,
        "max_tokens": 1024,
        "messages": [{ "role": "user", "content": content }],
    })
}

fn install_crypto_provider() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

pub fn memory_size_bytes(value: &str) -> Option<u64> {
    let text = value.trim();
    let (digits, multiplier) = match text.chars().last().map(|last| last.to_ascii_uppercase()) {
        Some('K') => (&text[..text.len() - 1], KIB as f64),
        Some('M') => (&text[..text.len() - 1], MIB as f64),
        Some('G') => (&text[..text.len() - 1], (MIB * KIB) as f64),
        _ => (text, 1.0),
    };
    digits
        .trim()
        .parse::<f64>()
        .ok()
        .map(|number| (number * multiplier) as u64)
}

struct ScopePlan {
    unit: Option<String>,
    cap: MemoryCap,
    address_space_bytes: Option<u64>,
    memory_max: Option<String>,
}

impl ScopePlan {
    fn decide(options: &GatewayOptions, notes: &mut Vec<String>) -> Self {
        let wants_scope = options.peak_mode == PeakMode::Cgroup || options.memory_max.is_some();
        if !wants_scope {
            return Self {
                unit: None,
                cap: MemoryCap::None,
                address_space_bytes: None,
                memory_max: None,
            };
        }
        if systemd_run_available() {
            return Self {
                unit: Some(format!(
                    "aegis-gw-{}-{}.scope",
                    std::process::id(),
                    &uuid::Uuid::new_v4().simple().to_string()[..10]
                )),
                cap: match options.memory_max {
                    Some(_) => MemoryCap::Scope,
                    None => MemoryCap::None,
                },
                address_space_bytes: None,
                memory_max: options.memory_max.clone(),
            };
        }

        if options.peak_mode == PeakMode::Cgroup {
            notes.push("systemd-run --user unavailable; reporting ru_maxrss instead".to_string());
        }
        let address_space_bytes = options
            .memory_max
            .as_deref()
            .and_then(memory_size_bytes)
            .inspect(|_| {
                notes.push(
                    "systemd-run --user unavailable; capping RLIMIT_AS instead, which makes allocation fail rather than the kernel killing the process".to_string(),
                );
            });
        Self {
            unit: None,
            cap: match address_space_bytes {
                Some(_) => MemoryCap::AddressSpace,
                None => MemoryCap::None,
            },
            address_space_bytes,
            memory_max: None,
        }
    }
}

fn config_file(port: u16, upstream_url: &str, options: &GatewayOptions) -> String {
    let enabled = options.guardrails != GuardrailsMode::Off;
    let mode = match options.guardrails {
        GuardrailsMode::Off => GuardrailsMode::Observe,
        mode => mode,
    };
    format!(
        r#"http_addr = "127.0.0.1:{port}"
database_url = "sqlite://data/aegis.db?mode=rwc"
max_capture_bytes = {capture}

[guardrails]
enabled = {enabled}
mode = "{mode}"

[guardrails.secrets]
enabled = true

[pricing]
enabled = false

[[providers]]
id = "claude"
type = "claude_subscription"
base_url = "{upstream_url}"

[[providers]]
id = "codex"
type = "codex_subscription"
base_url = "{upstream_url}"
"#,
        capture = options.max_capture_bytes,
        mode = mode.label(),
    )
}

fn provision_credentials(binary: &Path, workdir: &Path) -> Result<String> {
    let password = workdir.join("data").join("password");
    std::fs::write(&password, "Aegistest1!pass\n")?;

    let created = run_cli(
        binary,
        workdir,
        &[
            "bootstrap-user",
            "--email",
            "test@example.invalid",
            "--password-file",
            "data/password",
        ],
    )?;
    let user_id = field_after(&created, "created user:")
        .ok_or_else(|| anyhow!("could not read user id from: {created}"))?;

    let minted = run_cli(
        binary,
        workdir,
        &[
            "key",
            "create",
            "--user",
            &user_id,
            "--name",
            "test",
            "--provider",
            "claude",
            "--provider",
            "codex",
        ],
    )?;
    field_after(&minted, "key:").ok_or_else(|| anyhow!("could not read api key from: {minted}"))
}

fn field_after(output: &str, prefix: &str) -> Option<String> {
    output.lines().find_map(|line| {
        line.trim()
            .strip_prefix(prefix)
            .map(|rest| rest.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

fn run_cli(binary: &Path, workdir: &Path, arguments: &[&str]) -> Result<String> {
    let output = Command::new(binary)
        .args(arguments)
        .current_dir(workdir)
        .env("AEGIS_CONFIG", "./aegis.toml")
        .env_remove("DATABASE_URL")
        .env_remove("HTTP_ADDR")
        .output()
        .with_context(|| format!("running aegis {}", arguments.join(" ")))?;
    if !output.status.success() {
        bail!(
            "aegis {} failed ({}): {}",
            arguments.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn spawn_gateway(binary: &Path, workdir: &Path, scope: &ScopePlan) -> Result<Child> {
    let log = std::fs::File::create(workdir.join("gateway.log"))?;
    let mut command = match &scope.unit {
        None => Command::new(binary),
        Some(unit) => {
            let mut command = Command::new("systemd-run");
            command.args([
                "--user",
                "--scope",
                "--collect",
                "--quiet",
                &format!("--unit={unit}"),
                "-p",
                "MemoryAccounting=yes",
            ]);
            if let Some(limit) = &scope.memory_max {
                // Without a swap cap the pages go to zram and nothing is killed.
                command.args(["-p", &format!("MemoryMax={limit}"), "-p", "MemorySwapMax=0"]);
            }
            command.arg("--").arg(binary);
            command
        }
    };
    command.arg("serve");

    if let Some(bytes) = scope.address_space_bytes {
        let limit = libc::rlimit {
            rlim_cur: bytes,
            rlim_max: bytes,
        };
        // SAFETY: setrlimit is async-signal-safe, which is all the closure may
        // call between fork and exec.
        unsafe {
            use std::os::unix::process::CommandExt;
            command.pre_exec(move || {
                if libc::setrlimit(libc::RLIMIT_AS, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    Ok(command
        .current_dir(workdir)
        .env("AEGIS_CONFIG", "./aegis.toml")
        .env("RUST_LOG", "warn")
        .env_remove("DATABASE_URL")
        .env_remove("HTTP_ADDR")
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()?)
}

pub fn free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

fn random_bytes(count: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut buffer = vec![0u8; count];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buffer)?;
    Ok(buffer)
}

fn set_owner_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}
