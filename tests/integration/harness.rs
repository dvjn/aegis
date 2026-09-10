//! A disposable aegis instance: its own working directory, database, root key
//! and port, so a test never touches a real deployment and parallel tests
//! cannot collide.

use std::{
    fs, io,
    net::TcpListener as BlockingTcpListener,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::OnceLock,
    time::{Duration, Instant},
};

use serde_json::json;
use uuid::Uuid;

use crate::fake_upstream::FakeUpstream;

const API_KEY_HEADER: &str = "x-aegis-api-key";
const CLAUDE_MODEL: &str = "claude-sonnet-4-5-20250929";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

/// Fake credentials shaped to match the built-in detectors in
/// src/policies/secrets.rs, so the masking pipeline actually rewrites them.
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

#[derive(Clone, Copy)]
pub enum GuardrailsMode {
    Mask,
    Observe,
}

impl GuardrailsMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Mask => "mask",
            Self::Observe => "observe",
        }
    }
}

pub struct Response {
    pub status: Option<u16>,
    pub body: String,
    pub error: Option<String>,
}

impl Response {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).expect("response body was not JSON")
    }
}

pub struct Aegis {
    workdir: PathBuf,
    gateway: Child,
    base_url: String,
    api_key: String,
    client: reqwest::Client,
    pub upstream: FakeUpstream,
}

impl Aegis {
    pub async fn start() -> Self {
        Self::with_guardrails(GuardrailsMode::Mask).await
    }

    pub async fn with_guardrails(mode: GuardrailsMode) -> Self {
        install_crypto_provider();

        let workdir = std::env::temp_dir().join(format!("aegis-itest-{}", Uuid::new_v4()));
        fs::create_dir_all(workdir.join("data")).unwrap();
        write_root_key(&workdir.join("data/root.key"));

        let upstream = FakeUpstream::start().await;
        let gateway_port = free_port();
        fs::write(
            workdir.join("aegis.toml"),
            config_toml(gateway_port, upstream.base_url(), mode),
        )
        .unwrap();

        let api_key = provision_credentials(&workdir);
        let gateway = spawn_gateway(&workdir);
        let instance = Self {
            workdir,
            gateway,
            base_url: format!("http://127.0.0.1:{gateway_port}"),
            api_key,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .unwrap(),
            upstream,
        };
        instance.wait_for_gateway(gateway_port);
        instance
    }

    pub fn database_path(&self) -> PathBuf {
        self.workdir.join("data/aegis.db")
    }

    pub fn provider_url(&self, provider: &str, path: &str, query: &str) -> String {
        format!("{}/providers/{provider}{path}?{query}", self.base_url)
    }

    pub fn claude_url(&self, query: &str) -> String {
        self.provider_url("claude", "/v1/messages", query)
    }

    pub fn request_body(&self, include_secrets: bool) -> String {
        self.request_body_for(CLAUDE_MODEL, include_secrets)
    }

    pub fn request_body_for(&self, model: &str, include_secrets: bool) -> String {
        let mut content = "review this config".to_owned();
        if include_secrets {
            for (_, value) in fake_secrets() {
                content.push(' ');
                content.push_str(&value);
            }
        }
        json!({
            "model": model,
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": content}],
        })
        .to_string()
    }

    pub async fn post(&self, url: &str, body: String) -> Response {
        self.post_with_key(url, body, Some(self.api_key.clone()))
            .await
    }

    pub async fn post_with_key(&self, url: &str, body: String, key: Option<String>) -> Response {
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
                        body,
                        error: None,
                    },
                    Err(error) => Response {
                        status: Some(status),
                        body: String::new(),
                        error: Some(error.to_string()),
                    },
                }
            }
            Err(error) => Response {
                status: None,
                body: String::new(),
                error: Some(error.to_string()),
            },
        }
    }

    fn wait_for_gateway(&self, port: u16) {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while Instant::now() < deadline {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "gateway did not open port {port} within {STARTUP_TIMEOUT:?}\n{}",
            self.gateway_log()
        );
    }

    fn gateway_log(&self) -> String {
        fs::read_to_string(self.workdir.join("gateway.log")).unwrap_or_default()
    }
}

impl Drop for Aegis {
    fn drop(&mut self) {
        let _ = self.gateway.kill();
        let _ = self.gateway.wait();
        let _ = fs::remove_dir_all(&self.workdir);
    }
}

/// reqwest is built with `rustls-no-provider`, so a process that constructs a
/// client must install one itself.
fn install_crypto_provider() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn free_port() -> u16 {
    BlockingTcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn write_root_key(path: &Path) {
    let mut key = Vec::with_capacity(32);
    key.extend_from_slice(Uuid::new_v4().as_bytes());
    key.extend_from_slice(Uuid::new_v4().as_bytes());
    fs::write(path, &key).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn config_toml(gateway_port: u16, upstream_url: &str, mode: GuardrailsMode) -> String {
    format!(
        r#"http_addr = "127.0.0.1:{gateway_port}"
database_url = "sqlite://data/aegis.db?mode=rwc"
max_capture_bytes = 16777216

[guardrails]
enabled = true
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
        mode = mode.as_str()
    )
}

fn cli(workdir: &Path, arguments: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_aegis"))
        .args(arguments)
        .current_dir(workdir)
        .env("AEGIS_CONFIG", "./aegis.toml")
        .env_remove("DATABASE_URL")
        .env_remove("HTTP_ADDR")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "aegis {} failed: {}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn field_after(output: &str, prefix: &str) -> String {
    output
        .lines()
        .find_map(|line| line.strip_prefix(prefix))
        .unwrap_or_else(|| panic!("no {prefix:?} line in: {output:?}"))
        .trim()
        .to_owned()
}

fn provision_credentials(workdir: &Path) -> String {
    fs::write(workdir.join("data/password"), "Itest1!pass\n").unwrap();
    let created = cli(
        workdir,
        &[
            "bootstrap-user",
            "--email",
            "itest@example.invalid",
            "--password-file",
            "data/password",
        ],
    );
    let user = field_after(&created, "created user:");

    let minted = cli(
        workdir,
        &[
            "key",
            "create",
            "--user",
            &user,
            "--name",
            "itest",
            "--provider",
            "claude",
            "--provider",
            "codex",
        ],
    );
    field_after(&minted, "key:")
}

fn spawn_gateway(workdir: &Path) -> Child {
    let log = fs::File::create(workdir.join("gateway.log")).unwrap();
    Command::new(env!("CARGO_BIN_EXE_aegis"))
        .arg("serve")
        .current_dir(workdir)
        .env("AEGIS_CONFIG", "./aegis.toml")
        .env("RUST_LOG", "warn")
        .env_remove("DATABASE_URL")
        .env_remove("HTTP_ADDR")
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .map_err(io::Error::other)
        .unwrap()
}
