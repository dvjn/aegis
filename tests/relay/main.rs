//! Black-box AGS tests: real launcher processes, private relays, and fake vendor CLIs.
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::Request,
    routing::{any, get, post},
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::{net::TcpListener, process::Command, sync::mpsc};

struct Fixture {
    root: PathBuf,
    server: String,
    backend: tokio::task::JoinHandle<()>,
    requests: mpsc::Receiver<Value>,
    refreshes: Arc<AtomicUsize>,
    registrations: Arc<AtomicUsize>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.backend.abort();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Fixture {
    async fn new() -> Self {
        Self::with_single_provider(false).await
    }

    async fn with_single_provider(single: bool) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let root = std::env::temp_dir().join(format!("ags-e2e-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("vendor")).unwrap();
        std::fs::write(root.join(".bashrc"), "# existing shell settings\n").unwrap();
        let script = include_str!("vendor.py");
        for name in ["claude", "codex", "pi"] {
            let path = root.join("vendor").join(name);
            std::fs::write(&path, script).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        std::fs::write(root.join("vendor/xdg-open"), "#!/bin/sh\nexit 0\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                root.join("vendor/xdg-open"),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        let device_url = format!("{server}/approve?user_code=TEST-CODE");
        let (tx, requests) = mpsc::channel(20);
        let refreshes = Arc::new(AtomicUsize::new(0));
        let counter = refreshes.clone();
        let registrations = Arc::new(AtomicUsize::new(0));
        let registered = registrations.clone();
        let catalog = if single {
            json!({"version":1,"providers":[{"id":"claude-main","kind":"claude_subscription"}]})
        } else {
            json!({"version":1,"providers":[{"id":"claude-main","kind":"claude_subscription"},{"id":"personal","kind":"codex_subscription"},{"id":"work","kind":"codex_subscription"}]})
        };
        let app = Router::new()
            .route("/oauth/register",post(move || {let registered=registered.clone();async move {registered.fetch_add(1,Ordering::SeqCst);Json(json!({"client_id":"ags-test"}))}}))
            .route("/oauth/device_authorization",post(move || { let url = device_url.clone(); async move { Json(json!({"device_code":"test-device","user_code":"TEST-CODE","verification_uri_complete":url,"expires_in":30,"interval":1})) } }))
            .route("/oauth/token",post(move |body: String| { let counter = counter.clone(); async move {
                use axum::response::IntoResponse;
                if body.contains("rejected-refresh") {
                    return (axum::http::StatusCode::BAD_REQUEST,Json(json!({"error":"invalid_grant"}))).into_response();
                }
                let token=if body.contains("refresh_token") {format!("mock-access-{}",counter.fetch_add(1,Ordering::SeqCst)+1)} else {"mock-access".into()};
                Json(json!({"access_token":token,"refresh_token":"mock-refresh","expires_in":3600})).into_response()
            } }))
            .route("/api/relay/connection",get(move || {let catalog=catalog.clone();async move {Json(catalog)}}))
            .route("/api/relay/enroll",post(|| async { axum::http::StatusCode::NO_CONTENT }))
            .route("/api/relay/disconnect",post(|| async { axum::http::StatusCode::NO_CONTENT }))
            .route("/providers/{*path}",any(move |request: Request| { let tx = tx.clone(); async move {
                let (parts,body) = request.into_parts();
                assert!(!parts.headers.contains_key("x-aegis-api-key"));
                assert!(parts.headers["x-aegis-access-token"].to_str().unwrap().starts_with("mock-access"));
                assert_eq!(parts.headers["authorization"],"Bearer vendor-credential");
                let body = to_bytes(body,1024).await.unwrap();
                tx.send(json!({"path":parts.uri.to_string(),"body":String::from_utf8_lossy(&body),"aegis_token":parts.headers["x-aegis-access-token"].to_str().unwrap()})).await.unwrap();
                Body::from("data: OK\n\n")
            } }));
        let backend = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            root,
            server,
            backend,
            requests,
            refreshes,
            registrations,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ags"));
        self.environment(&mut command);
        command
    }

    fn environment(&self, command: &mut Command) {
        command
            .env("HOME", &self.root)
            .env("AGS_HOME", self.root.join("ags"))
            .env("SHELL", "/bin/bash")
            .env("PI_CODING_AGENT_DIR", self.root.join("pi-agent"))
            .env(
                "PATH",
                format!(
                    "{}:{}:/usr/bin:/bin",
                    self.root.join("ags/shims").display(),
                    self.root.join("vendor").display()
                ),
            )
            .current_dir(&self.root);
    }

    async fn setup(&self, wrappers: bool) {
        let mut cmd = self.command();
        cmd.args([
            "setup",
            &self.server,
            "--no-browser",
            "--provider",
            "codex=personal",
            "--provider",
            "pi=work",
        ]);
        if !wrappers {
            cmd.arg("--no-wrappers");
        }
        let output = cmd.output().await.unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let state: Value =
            serde_json::from_slice(&std::fs::read(self.root.join("ags/connection.json")).unwrap())
                .unwrap();
        assert!(
            state.get("key").is_none(),
            "OAuth pairing must not store a gateway API key"
        );
        assert!(
            !self.root.join("data").exists(),
            "AGS must not initialize a server database"
        );
    }
}

#[tokio::test]
async fn wrappers_have_private_lifetimes_and_preserve_args_auth_and_provider_defaults() {
    let mut f = Fixture::new().await;
    f.setup(true).await;
    f.setup(true).await;
    let rc = std::fs::read_to_string(f.root.join(".bashrc")).unwrap();
    assert_eq!(rc.matches("# >>> AGS").count(), 1);
    let mut first = Command::new(f.root.join("ags/shims/claude"));
    f.environment(&mut first);
    first.args(["argument with spaces", "--exit-7"]);
    let mut second = Command::new(f.root.join("ags/shims/codex"));
    f.environment(&mut second);
    second.args(["exec", "another prompt"]);
    let (a, b) = tokio::join!(first.output(), second.output());
    let a = a.unwrap();
    let b = b.unwrap();
    assert_eq!(
        a.status.code(),
        Some(7),
        "{}",
        String::from_utf8_lossy(&a.stderr)
    );
    assert!(b.status.success(), "{}", String::from_utf8_lossy(&b.stderr));
    let a: Value = serde_json::from_slice(&a.stdout).unwrap();
    let b: Value = serde_json::from_slice(&b.stdout).unwrap();
    assert_eq!(a["args"][0], "argument with spaces");
    assert_ne!(a["endpoint"], b["endpoint"]);
    assert_eq!(a["aegis_auth_in_environment"], false);
    let mut routes = vec![
        f.requests.recv().await.unwrap()["path"]
            .as_str()
            .unwrap()
            .to_owned(),
        f.requests.recv().await.unwrap()["path"]
            .as_str()
            .unwrap()
            .to_owned(),
    ];
    routes.sort();
    assert_eq!(
        routes,
        vec![
            "/providers/claude-main/responses?trial=true",
            "/providers/personal/responses?trial=true"
        ]
    );
    for endpoint in [
        a["endpoint"].as_str().unwrap(),
        b["endpoint"].as_str().unwrap(),
    ] {
        assert!(
            reqwest::get(format!("{endpoint}/responses")).await.is_err(),
            "relay survived its CLI"
        );
    }
    let output = f
        .command()
        .args(["run", "pi", "--provider", "personal", "--", "prompt"])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let pi: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        !Path::new(pi["extension"].as_str().unwrap()).exists(),
        "temporary extension was not removed"
    );
    assert_eq!(
        f.requests.recv().await.unwrap()["path"],
        "/providers/personal/responses?trial=true"
    );
    let output = f.command().args(["uninstall"]).output().await.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!f.root.join("ags/shims/claude").exists());
    assert!(
        std::fs::read_to_string(f.root.join(".bashrc"))
            .unwrap()
            .contains("# existing shell settings")
    );
    assert!(!f.root.join("ags/connection.json").exists());
}

#[tokio::test]
async fn setup_repairs_revoked_login_without_requiring_manual_state_deletion() {
    let f = Fixture::new().await;
    f.setup(false).await;
    let path = f.root.join("ags/connection.json");
    let mut state: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    state["refresh_token"] = json!("rejected-refresh");
    state["expires_at"] = json!(0);
    std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    f.setup(false).await;
    assert_eq!(f.registrations.load(Ordering::SeqCst), 2);
    let state: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(state["refresh_token"], "mock-refresh");
    assert_eq!(state["cli_overrides"]["pi"]["codex_subscription"], "work");
}

#[tokio::test]
async fn pi_proxies_every_enabled_subscription_concurrently_and_keeps_native_config() {
    let mut f = Fixture::new().await;
    f.setup(false).await;
    let agent = f.root.join("pi-agent");
    std::fs::create_dir_all(&agent).unwrap();
    let models = r#"{"providers":{"openai-codex":{"baseUrl":"https://old.example/providers/work/backend-api/codex","headers":{"x-aegis-api-key":"$OLD_KEY"}}}}"#;
    std::fs::write(agent.join("models.json"), models).unwrap();
    let output = f
        .command()
        .args(["run", "pi", "--", "--all-providers"])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(data["pi_config"]["defaults"]["openai-codex"], "work");
    let routes = data["pi_config"]["routes"].as_array().unwrap();
    assert_eq!(routes.len(), 3);
    assert_eq!(
        routes
            .iter()
            .map(|r| r["endpoint"].as_str().unwrap())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3
    );
    let mut paths = Vec::new();
    for _ in routes {
        paths.push(
            f.requests.recv().await.unwrap()["path"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    paths.sort();
    assert_eq!(
        paths,
        vec![
            "/providers/claude-main/responses?trial=true",
            "/providers/personal/responses?trial=true",
            "/providers/work/responses?trial=true"
        ]
    );
    assert_eq!(
        std::fs::read_to_string(agent.join("models.json")).unwrap(),
        models
    );
    for route in routes {
        assert_eq!(
            route["alias"],
            format!("ags-{}", route["id"].as_str().unwrap())
        );
        assert!(
            reqwest::get(format!("{}/responses", route["endpoint"].as_str().unwrap()))
                .await
                .is_err()
        );
    }
    assert_eq!(data["aegis_auth_in_environment"], false);
}

#[tokio::test]
async fn concurrent_launches_rotate_oauth_once_and_uninstall_preserves_edited_wrappers() {
    let mut f = Fixture::new().await;
    f.setup(true).await;
    let path = f.root.join("ags/connection.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["expires_at"] = json!(0);
    std::fs::write(&path, config.to_string()).unwrap();
    let mut a = f.command();
    a.args(["run", "claude"]);
    let mut b = f.command();
    b.args(["run", "codex"]);
    let (a, b) = tokio::join!(a.output(), b.output());
    assert!(a.unwrap().status.success());
    assert!(b.unwrap().status.success());
    assert_eq!(f.refreshes.load(Ordering::SeqCst), 1);
    f.requests.recv().await.unwrap();
    f.requests.recv().await.unwrap();
    let wrapper = f.root.join("ags/shims/claude");
    std::fs::write(&wrapper, "#!/bin/sh\n# Managed by AGS\n# user edit\n").unwrap();
    let output = f
        .command()
        .args(["uninstall", "--offline"])
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(
        std::fs::read_to_string(wrapper)
            .unwrap()
            .contains("# user edit")
    );
}

#[tokio::test]
async fn first_launch_runs_one_wizard_then_opens_each_cli() {
    let mut f = Fixture::with_single_provider(true).await;
    let mut a = f.command();
    a.env("AGS_SERVER_URL", &f.server)
        .args(["run", "claude", "--", "first"]);
    let mut b = f.command();
    b.env("AGS_SERVER_URL", &f.server)
        .args(["run", "claude", "--", "second"]);
    let (a, b) = tokio::join!(a.output(), b.output());
    for output in [a.unwrap(), b.unwrap()] {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(f.registrations.load(Ordering::SeqCst), 1);
    f.requests.recv().await.unwrap();
    f.requests.recv().await.unwrap();
    assert!(f.root.join("ags/connection.json").exists());
    assert!(
        !f.root.join("ags/shims").exists(),
        "first-use login does not modify shell wrappers"
    );
    assert_eq!(
        std::fs::read_to_string(f.root.join("ags/server")).unwrap(),
        f.server
    );
}

#[tokio::test]
async fn an_open_cli_refreshes_oauth_before_its_next_request() {
    let mut f = Fixture::new().await;
    f.setup(false).await;
    let output = f
        .command()
        .args(["run", "claude", "--", "--refresh-during-run"])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let first = f.requests.recv().await.unwrap();
    let second = f.requests.recv().await.unwrap();
    assert_eq!(first["aegis_token"], "mock-access");
    assert_eq!(second["aegis_token"], "mock-access-1");
    assert_eq!(f.refreshes.load(Ordering::SeqCst), 1);
}
