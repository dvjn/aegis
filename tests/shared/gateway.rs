use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use axum::body::Bytes;
use serde_json::json;

use super::measure::MIB;
use super::upstream::Upstream;

pub const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;
pub const DEFAULT_CAPTURE_BYTES: usize = 16 * MIB;

const API_KEY_HEADER: &str = "x-aegis-api-key";
const CLAUDE_MODEL: &str = "claude-sonnet-4-5-20250929";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const CLI_TIMEOUT: Duration = Duration::from_secs(30);
const TERMINATE_TIMEOUT: Duration = Duration::from_secs(10);

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

#[derive(Clone, Debug)]
pub struct GatewayOptions {
    pub guardrails: GuardrailsMode,
    pub max_capture_bytes: usize,
    pub keep_workdir: bool,
}

impl Default for GatewayOptions {
    fn default() -> Self {
        Self {
            guardrails: GuardrailsMode::Mask,
            max_capture_bytes: DEFAULT_CAPTURE_BYTES,
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
    pub exit_status: Option<i32>,
    pub child_reaped: bool,
    pub kept_workdir: Option<PathBuf>,
}

pub struct Gateway {
    options: GatewayOptions,
    workdir: PathBuf,
    port: u16,
    child: Mutex<Child>,
    pid: u32,
    exit: Mutex<Option<i32>>,
    shutdown: Mutex<Option<Shutdown>>,
    api_key: String,
    client: reqwest::Client,
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
        Self::start_with_binary(Path::new(env!("CARGO_BIN_EXE_aegis")), upstream, options)
    }

    pub fn start_with_binary(
        binary: &Path,
        upstream: &Upstream,
        options: GatewayOptions,
    ) -> Result<Self> {
        install_crypto_provider();
        let workdir = std::env::temp_dir().join(format!("aegis-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(workdir.join("data"))?;
        let mut cleanup = WorkdirCleanup::new(workdir.clone());

        let root_key = workdir.join("data/root.key");
        std::fs::write(&root_key, random_bytes(32)?)?;
        set_owner_only(&root_key)?;

        let port = free_port()?;
        std::fs::write(
            workdir.join("aegis.toml"),
            config_file(port, upstream.base_url(), &options),
        )?;
        let api_key = provision_credentials(binary, &workdir)?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()?;
        let child = spawn_gateway(binary, &workdir)?;
        let pid = child.id();
        let gateway = Self {
            options,
            workdir,
            port,
            child: Mutex::new(child),
            pid,
            exit: Mutex::new(None),
            shutdown: Mutex::new(None),
            api_key,
            client,
            upstream: upstream.clone(),
        };
        cleanup.disarm();

        if let Err(error) = gateway.wait_until_ready() {
            let tail = gateway.log_tail(20);
            gateway.shut_down();
            return Err(error.context(format!("gateway did not become ready; log tail:\n{tail}")));
        }
        Ok(gateway)
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn pid(&self) -> u32 {
        self.pid
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
            let status = self.child.lock().expect("child lock").try_wait().ok()?;
            *exit = status.map(exit_code);
        }
        *exit
    }

    fn wait_until_ready(&self) -> Result<()> {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while Instant::now() < deadline {
            if let Some(status) = self.exit_status() {
                bail!("gateway exited early with status {status}");
            }
            if health_ready(self.port) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        bail!("gateway health did not report ready within {STARTUP_TIMEOUT:?}")
    }

    pub fn log_tail(&self, lines: usize) -> String {
        let Ok(log) = std::fs::read_to_string(self.workdir.join("gateway.log")) else {
            return "(no gateway log)".to_string();
        };
        let collected = log.lines().collect::<Vec<_>>();
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
            Err(error) => failed_response(started, error),
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
            Err(error) => failed_response(started, error),
        }
    }

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
            Err(error) => failed_response(started, error),
        }
    }

    pub fn shut_down(&self) -> Shutdown {
        if let Some(done) = self.shutdown.lock().expect("shutdown lock").clone() {
            return done;
        }
        self.terminate();
        let exit_status = *self.exit.lock().expect("exit lock");
        let kept_workdir = if self.options.keep_workdir {
            Some(self.workdir.clone())
        } else {
            let _ = std::fs::remove_dir_all(&self.workdir);
            None
        };
        let done = Shutdown {
            exit_status,
            child_reaped: exit_status.is_some()
                && !Path::new(&format!("/proc/{}", self.pid)).exists(),
            kept_workdir,
        };
        *self.shutdown.lock().expect("shutdown lock") = Some(done.clone());
        done
    }

    fn terminate(&self) {
        if self.exit_status().is_some() {
            return;
        }
        unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGTERM) };
        let deadline = Instant::now() + TERMINATE_TIMEOUT;
        while Instant::now() < deadline {
            if self.exit_status().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let mut child = self.child.lock().expect("child lock");
        let _ = child.kill();
        if let Ok(status) = child.wait() {
            *self.exit.lock().expect("exit lock") = Some(exit_code(status));
        }
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.shut_down();
    }
}

struct WorkdirCleanup {
    path: PathBuf,
    armed: bool,
}

impl WorkdirCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for WorkdirCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

fn failed_response(started: Instant, error: reqwest::Error) -> Response {
    Response {
        status: None,
        body: String::new(),
        bytes_read: 0,
        seconds: started.elapsed().as_secs_f64(),
        error: Some(error.to_string()),
    }
}

fn health_ready(port: u16) -> bool {
    let address = ([127, 0, 0, 1], port).into();
    let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_millis(250)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    if stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut response = String::new();
    stream.read_to_string(&mut response).is_ok()
        && response.starts_with("HTTP/1.1 200")
        && response.contains("\"status\":\"ok\"")
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
    let password = workdir.join("data/password");
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
    let mut child = Command::new(binary)
        .args(arguments)
        .current_dir(workdir)
        .env("AEGIS_CONFIG", "./aegis.toml")
        .env_remove("DATABASE_URL")
        .env_remove("HTTP_ADDR")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running aegis {}", arguments.join(" ")))?;
    let deadline = Instant::now() + CLI_TIMEOUT;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("aegis {} exceeded {CLI_TIMEOUT:?}", arguments.join(" "));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let output = child.wait_with_output()?;
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

fn spawn_gateway(binary: &Path, workdir: &Path) -> Result<Child> {
    let log = std::fs::File::create(workdir.join("gateway.log"))?;
    Ok(Command::new(binary)
        .arg("serve")
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
    let mut buffer = vec![0; count];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buffer)?;
    Ok(buffer)
}

fn set_owner_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| -status.signal().unwrap_or_default())
}
