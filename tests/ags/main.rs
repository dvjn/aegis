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
        atomic::{AtomicBool, AtomicUsize, Ordering},
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
    artifacts_available: Arc<AtomicBool>,
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
        for name in ["claude", "codex", "pi", "agy"] {
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
        let installer =
            include_str!("../../src/ags/install.sh").replace("@SERVER@", &format!("'{server}'"));
        let mise_installer = include_str!("../../src/ags/install-mise.sh")
            .replace("@SERVER@", &format!("'{server}'"));
        let artifact = axum::body::Bytes::from(std::fs::read(env!("CARGO_BIN_EXE_ags")).unwrap());
        use sha2::{Digest, Sha256};
        let checksum: String = Sha256::digest(&artifact)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let device_url = format!("{server}/approve?user_code=TEST-CODE");
        let (tx, requests) = mpsc::channel(20);
        let refreshes = Arc::new(AtomicUsize::new(0));
        let counter = refreshes.clone();
        let registrations = Arc::new(AtomicUsize::new(0));
        let registered = registrations.clone();
        let catalog = if single {
            json!({"version":1,"providers":[{"id":"claude-main","kind":"claude_subscription"}],"capabilities":["accounts"]})
        } else {
            json!({"version":1,"providers":[{"id":"claude-main","kind":"claude_subscription"},{"id":"personal","kind":"codex_subscription"},{"id":"work","kind":"codex_subscription"},{"id":"google-personal","kind":"antigravity_subscription"}],"capabilities":["accounts"]})
        };
        let artifacts_available = Arc::new(AtomicBool::new(true));
        let available = artifacts_available.clone();
        let app = Router::new()
            .route("/install.sh",get(move || { let script = installer.clone(); async move { script } }))
            .route("/install-mise.sh",get(move || { let script = mise_installer.clone(); async move { script } }))
            .route("/downloads/ags/version",get(|| async { env!("CARGO_PKG_VERSION") }))
            .route("/.well-known/aegis-relay",get(|| async { Json(json!({"version":env!("CARGO_PKG_VERSION"),"protocol":1,"targets":["x86_64-unknown-linux-musl","aarch64-unknown-linux-musl"]})) }))
            .route("/downloads/ags/{version}/{target}/{file}",get(move |axum::extract::Path((version,_target,file)):axum::extract::Path<(String,String,String)>| {
                let artifact = artifact.clone(); let checksum = checksum.clone(); let available = available.clone();
                async move {
                    use axum::response::IntoResponse;
                    if !available.load(Ordering::SeqCst) || version != env!("CARGO_PKG_VERSION") { return axum::http::StatusCode::NOT_FOUND.into_response(); }
                    if file == "ags.sha256" { return format!("{checksum}  ags\n").into_response(); }
                    artifact.into_response()
                }
            }))
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
            .route("/api/relay/accounts/{id}",axum::routing::put(|| async { axum::http::StatusCode::NO_CONTENT }).delete(|| async { axum::http::StatusCode::NO_CONTENT }))
            .route("/api/relay/profiles/{id}",axum::routing::put(|| async { axum::http::StatusCode::NO_CONTENT }).delete(|| async { axum::http::StatusCode::NO_CONTENT }))
            .route("/api/relay/disconnect",post(|| async { axum::http::StatusCode::NO_CONTENT }))
            .route("/providers/{*path}",any(move |request: Request| { let tx = tx.clone(); async move {
                let (parts,body) = request.into_parts();
                assert!(!parts.headers.contains_key("x-aegis-api-key"));
                assert!(parts.headers["x-aegis-access-token"].to_str().unwrap().starts_with("mock-access"));
                assert_eq!(parts.headers["authorization"],"Bearer vendor-credential");
                let body = to_bytes(body,1024).await.unwrap();
                tx.send(json!({"path":parts.uri.to_string(),"body":String::from_utf8_lossy(&body),"aegis_token":parts.headers["x-aegis-access-token"].to_str().unwrap(),"account":parts.headers.get("x-aegis-account").and_then(|v| v.to_str().ok()),"profile":parts.headers.get("x-aegis-profile").and_then(|v| v.to_str().ok()),"run":parts.headers.get("x-aegis-run").and_then(|v| v.to_str().ok())})).await.unwrap();
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
            artifacts_available,
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
async fn mise_managed_wrappers_follow_the_active_ags_executable() {
    let mut f = Fixture::new().await;
    let binary = env!("CARGO_BIN_EXE_ags");
    let mise = f.root.join("vendor/mise");
    let script = format!(
        r#"#!/bin/sh
case "$1" in
  which) printf '%s\n' '{}' ;;
  exec)
    shift
    test "$1" = -- && shift
    test "$1" = ags && shift
    printf '%s\n' "$AGS_MISE_BINARY" > "$AGS_MISE_MARKER"
    exec "$AGS_MISE_BINARY" "$@"
    ;;
  *) exit 1 ;;
esac
"#,
        binary
    );
    std::fs::write(&mise, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&mise, std::fs::Permissions::from_mode(0o700)).unwrap();
    f.setup(true).await;
    let wrapper = std::fs::read_to_string(f.root.join("ags/shims/claude")).unwrap();
    assert!(wrapper.contains("mise exec -- ags run claude"));
    let next = f.root.join("ags-next");
    std::fs::copy(binary, &next).unwrap();
    let marker = f.root.join("mise-launched");
    let mut command = Command::new(f.root.join("ags/shims/claude"));
    f.environment(&mut command);
    let output = command
        .env("AGS_MISE_BINARY", &next)
        .env("AGS_MISE_MARKER", &marker)
        .arg("--version")
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(marker).unwrap().trim(),
        next.to_str().unwrap()
    );
    assert_eq!(
        f.requests.recv().await.unwrap()["path"],
        "/providers/claude-main/responses?trial=true"
    );
}

#[tokio::test]
async fn agy_wrapper_preserves_google_auth_arguments_and_private_proxy_lifetime() {
    let mut f = Fixture::new().await;
    f.setup(true).await;
    let mut command = Command::new(f.root.join("ags/shims/agy"));
    f.environment(&mut command);
    let output = command
        .args([
            "--model",
            "gemini-3.8-flash-low",
            "--print",
            "argument with spaces",
        ])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(data["args"][3], "argument with spaces");
    assert_eq!(data["aegis_auth_in_environment"], false);
    assert_eq!(
        f.requests.recv().await.unwrap()["path"],
        "/providers/google-personal/v1internal:streamGenerateContent?alt=sse"
    );
    assert!(
        reqwest::get(format!(
            "{}/v1internal:loadCodeAssist",
            data["endpoint"].as_str().unwrap()
        ))
        .await
        .is_err()
    );
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
    // Signing in again reuses the client, so its registered accounts and profiles still apply.
    assert_eq!(f.registrations.load(Ordering::SeqCst), 1);
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
async fn server_mise_installer_reports_missing_artifacts_before_installing() {
    let f = Fixture::new().await;
    f.artifacts_available.store(false, Ordering::SeqCst);
    let mise = f.root.join("vendor/mise");
    std::fs::write(&mise, "#!/bin/sh\nexit 99\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(mise, std::fs::Permissions::from_mode(0o700)).unwrap();
    let script = include_str!("../../src/ags/install-mise.sh")
        .replace("@SERVER@", &format!("'{}'", f.server));
    let path = f.root.join("install-mise.sh");
    std::fs::write(&path, script).unwrap();
    let mut command = Command::new("/bin/sh");
    f.environment(&mut command);
    let result = command.arg(path).output().await.unwrap();
    assert_eq!(result.status.code(), Some(1));
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(error.contains("ags download unavailable"), "{error}");
    assert!(error.contains("mise run ags:stage"), "{error}");
    assert!(!error.contains("Invalid server checksum"), "{error}");
}

#[tokio::test]
async fn server_mise_installer_configures_its_own_downloads_and_exact_version() {
    let f = Fixture::new().await;
    let log = f.root.join("mise-use");
    let mise = f.root.join("vendor/mise");
    let binary = env!("CARGO_BIN_EXE_ags");
    let mock = format!(
        r#"#!/bin/sh
case "$1" in
  install|use) shift; printf '%s\n' "$@" >> '{}' ;;
  which) printf '%s\n' '{}' ;;
  exec)
    shift
    while test "$1" != --; do shift; done
    shift
    test "$1" = ags && shift
    exec '{}' "$@"
    ;;
  *) exit 1 ;;
esac
"#,
        log.display(),
        binary,
        binary
    );
    std::fs::write(&mise, mock).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(mise, std::fs::Permissions::from_mode(0o700)).unwrap();
    let script = reqwest::get(format!("{}/install-mise.sh", f.server))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let path = f.root.join("install-mise.sh");
    std::fs::write(&path, script).unwrap();
    let mut command = Command::new("/bin/sh");
    f.environment(&mut command);
    let result = command
        .arg(path)
        .args([
            "--no-wrappers",
            "--no-browser",
            "--provider",
            "codex=personal",
            "--provider",
            "pi=work",
        ])
        .output()
        .await
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let arguments = std::fs::read_to_string(log).unwrap();
    assert!(arguments.contains("--force"));
    assert!(arguments.contains(&format!("url={}/downloads/ags/", f.server)));
    assert!(arguments.contains(&format!(
        "version_list_url={}/downloads/ags/version",
        f.server
    )));
    assert!(arguments.contains("checksum_url="));
    assert!(arguments.contains(&format!("@{}", env!("CARGO_PKG_VERSION"))));
    assert!(!arguments.contains("github"));
    assert!(f.root.join("ags/connection.json").is_file());
    assert!(!f.root.join("ags/.installed").exists());
}

#[tokio::test]
async fn server_installer_update_and_rollback_need_no_manually_copied_credentials() {
    let f = Fixture::new().await;
    let script = reqwest::get(format!("{}/install.sh", f.server))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let mut command = Command::new("/bin/sh");
    f.environment(&mut command);
    command
        .args([
            "-s",
            "--",
            "--no-browser",
            "--provider",
            "codex=personal",
            "--provider",
            "pi=work",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().unwrap();
    use tokio::io::AsyncWriteExt;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .await
        .unwrap();
    let result = child.wait_with_output().await.unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(f.root.join("ags/.installed").exists());
    let installed = f.root.join("ags/bin/ags");
    for args in [vec!["update"], vec!["update", "--rollback"]] {
        let mut command = Command::new(&installed);
        f.environment(&mut command);
        let result = command.args(args).output().await.unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let mut command = Command::new(installed);
    f.environment(&mut command);
    let result = command.arg("uninstall").output().await.unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!f.root.join("ags/bin/ags").exists());
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

async fn ok(output: std::process::Output) -> std::process::Output {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn json_file(path: std::path::PathBuf) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[tokio::test]
async fn profiles_share_accounts_and_attribute_requests_by_account_profile_and_run() {
    let mut f = Fixture::new().await;
    f.setup(true).await;
    // Profiles only reference existing accounts; `new` and `native` are rejected.
    for value in ["new", "native"] {
        let output = f
            .command()
            .args(["profile", "new", "work", "--claude", value])
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("ags account new <name> --claude"),
            "{stderr}"
        );
    }
    assert!(
        !f.root.join("ags/profiles.json").exists()
            || json_file(f.root.join("ags/profiles.json"))["profiles"]
                .as_array()
                .is_none_or(|p| p.is_empty()),
        "a rejected value must not create the profile"
    );
    let created = ok(f
        .command()
        .args(["account", "new", "work-claude", "--claude"])
        .output()
        .await
        .unwrap())
    .await;
    assert!(
        String::from_utf8_lossy(&created.stdout)
            .contains("Sign in with `ags account login work-claude`.")
    );
    // Two codex routes are granted, so a chatgpt account needs the route named.
    let output = f
        .command()
        .args(["account", "new", "work-chatgpt", "--chatgpt"])
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ags account new work-chatgpt --chatgpt --route <id>"),
        "{stderr}"
    );
    ok(f.command()
        .args([
            "account",
            "new",
            "work-chatgpt",
            "--chatgpt",
            "--route",
            "work",
        ])
        .output()
        .await
        .unwrap())
    .await;
    ok(f.command()
        .args([
            "profile",
            "new",
            "work",
            "--claude",
            "work-claude",
            "--chatgpt",
            "work-chatgpt",
        ])
        .output()
        .await
        .unwrap())
    .await;
    let output = f
        .command()
        .args(["profile", "set", "work", "chatgpt", "native"])
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no longer accepted"));

    let accounts = json_file(f.root.join("ags/accounts.json"));
    let id_of = |name: &str| {
        accounts["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"] == name)
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let (claude_id, codex_id) = (id_of("work-claude"), id_of("work-chatgpt"));
    let profile_id = json_file(f.root.join("ags/profiles.json"))["profiles"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Not signed in: the error carries the command that fixes it.
    let output = f
        .command()
        .args(["run", "claude", "-P", "work", "--", "hi"])
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("ags account login work-claude"));
    for (id, file) in [(&claude_id, ".credentials.json"), (&codex_id, "auth.json")] {
        std::fs::write(f.root.join("ags/accounts").join(id).join(file), "{}").unwrap();
    }

    let output = ok(f
        .command()
        .args(["run", "claude", "-P", "work", "--", "hi"])
        .output()
        .await
        .unwrap())
    .await;
    let launch: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        launch["config_dir"],
        f.root
            .join("ags/accounts")
            .join(&claude_id)
            .to_str()
            .unwrap()
    );
    let request = f.requests.recv().await.unwrap();
    assert_eq!(
        request["path"],
        "/providers/claude-main/responses?trial=true"
    );
    assert_eq!(request["account"], claude_id.as_str());
    assert_eq!(request["profile"], profile_id.as_str());
    assert!(uuid::Uuid::parse_str(request["run"].as_str().unwrap()).is_ok());
    let accounts = json_file(f.root.join("ags/accounts.json"));
    assert!(accounts["accounts"][0]["registered_with"].is_string());
    assert!(
        json_file(f.root.join("ags/profiles.json"))["profiles"][0]["registered_with"].is_string()
    );

    // AGS_PROFILE selects codex, whose account has its own CODEX_HOME and file credential store.
    let output = ok(f
        .command()
        .env("AGS_PROFILE", "work")
        .args(["run", "codex", "--", "exec", "prompt"])
        .output()
        .await
        .unwrap())
    .await;
    let launch: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        launch["config_dir"],
        f.root
            .join("ags/accounts")
            .join(&codex_id)
            .to_str()
            .unwrap()
    );
    assert!(
        launch["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "cli_auth_credentials_store=\"file\"")
    );
    let request = f.requests.recv().await.unwrap();
    assert_eq!(request["path"], "/providers/work/responses?trial=true");
    assert_eq!(request["account"], codex_id.as_str());
    assert_eq!(request["profile"], profile_id.as_str());

    // A directory rule selects the profile from a child directory only.
    let project = f.root.join("proj");
    std::fs::create_dir_all(project.join("src")).unwrap();
    ok(f.command()
        .current_dir(&project)
        .args(["profile", "use", "work", "--here"])
        .output()
        .await
        .unwrap())
    .await;
    let shown = ok(f
        .command()
        .current_dir(project.join("src"))
        .arg("profile")
        .output()
        .await
        .unwrap())
    .await;
    let shown = String::from_utf8_lossy(&shown.stdout).into_owned();
    assert!(
        shown.contains("Profile: work (rule ") && shown.contains("work-claude"),
        "{shown}"
    );
    ok(f.command()
        .current_dir(project.join("src"))
        .args(["run", "claude", "--", "hi"])
        .output()
        .await
        .unwrap())
    .await;
    let request = f.requests.recv().await.unwrap();
    assert_eq!(request["account"], claude_id.as_str());
    assert_eq!(request["profile"], profile_id.as_str());

    // Elsewhere nothing resolves, and --provider is always unprofiled: no attribution.
    for args in [
        vec!["run", "claude", "--", "hi"],
        vec!["run", "claude", "--provider", "claude-main", "--", "hi"],
    ] {
        ok(f.command().args(&args).output().await.unwrap()).await;
        let request = f.requests.recv().await.unwrap();
        assert!(
            request["account"].is_null()
                && request["profile"].is_null()
                && request["run"].is_null()
        );
    }
    let output = f
        .command()
        .args(["run", "pi", "-P", "work"])
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());

    let listed = ok(f
        .command()
        .args(["profile", "list"])
        .output()
        .await
        .unwrap())
    .await;
    let listed = String::from_utf8_lossy(&listed.stdout).into_owned();
    assert!(
        listed.contains("work-claude")
            && listed.contains("work-chatgpt")
            && listed.contains("rule "),
        "{listed}"
    );
    let listed = ok(f
        .command()
        .args(["account", "list"])
        .output()
        .await
        .unwrap())
    .await;
    assert!(String::from_utf8_lossy(&listed.stdout).contains("signed in"));

    // Removal is refused while a profile uses the account; --force detaches it and deletes its dir.
    let output = f
        .command()
        .args(["account", "rm", "work-claude"])
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("ags account rm work-claude --force"));
    ok(f.command()
        .args(["account", "rm", "work-claude", "--force"])
        .output()
        .await
        .unwrap())
    .await;
    assert!(!f.root.join("ags/accounts").join(&claude_id).exists());
    let output = f
        .command()
        .args(["run", "claude", "-P", "work", "--", "hi"])
        .output()
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stderr).contains("ags profile set work claude <name>"));
}

#[tokio::test]
async fn account_and_profile_commands_explain_empty_state_and_missing_input_without_a_terminal() {
    let f = Fixture::new().await;
    f.setup(false).await;
    let stdout =
        |output: std::process::Output| String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr =
        |output: std::process::Output| String::from_utf8_lossy(&output.stderr).into_owned();

    let listed = stdout(ok(f.command().args(["profile", "ls"]).output().await.unwrap()).await);
    assert!(
        listed.contains(
            "No profiles. Create one with `ags profile new <name>`, or run `ags setup` for a guided setup."
        ),
        "{listed}"
    );
    let listed = stdout(ok(f.command().args(["account", "ls"]).output().await.unwrap()).await);
    assert!(
        listed.contains(
            "No accounts. Create one with `ags account new <name> --claude` or `--chatgpt`, or run `ags setup` for a guided setup."
        ),
        "{listed}"
    );

    for (args, expected) in [
        (
            vec!["account", "new"],
            "a name and product are required: `ags account new <name> --claude`",
        ),
        (
            vec!["account", "new", "solo"],
            "choose a product: `ags account new solo --claude` or `--chatgpt`",
        ),
        (
            vec!["account", "new", "--claude"],
            "a name is required: `ags account new <name> --claude`",
        ),
    ] {
        let output = f.command().args(&args).output().await.unwrap();
        assert!(!output.status.success(), "{args:?}");
        let message = stderr(output);
        assert!(message.contains(expected), "{args:?}: {message}");
    }

    // A profile can be created with no accounts; launching with it names the exact fix.
    ok(f.command()
        .args(["profile", "new", "bare", "--default"])
        .output()
        .await
        .unwrap())
    .await;
    let output = f
        .command()
        .args(["run", "codex", "--", "hi"])
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    let message = stderr(output);
    assert!(
        message.contains(
            "Profile bare has no chatgpt account. Add one with `ags account new <name> --chatgpt`, then `ags profile set bare chatgpt <name>`."
        ),
        "{message}"
    );
}

#[tokio::test]
async fn setup_without_a_terminal_points_to_the_wizard_until_an_account_exists() {
    let f = Fixture::new().await;
    let setup = || {
        let mut cmd = f.command();
        cmd.args([
            "setup",
            &f.server,
            "--no-browser",
            "--no-wrappers",
            "--provider",
            "codex=personal",
            "--provider",
            "pi=work",
        ]);
        cmd
    };
    let hint = "Run `ags setup` in a terminal to set up accounts and profiles.";
    let output = ok(setup().output().await.unwrap()).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Connected. Run claude"), "{stdout}");
    assert!(stdout.contains(hint), "{stdout}");
    ok(f.command()
        .args(["account", "new", "work-claude", "--claude"])
        .output()
        .await
        .unwrap())
    .await;
    let output = ok(setup().output().await.unwrap()).await;
    assert!(!String::from_utf8_lossy(&output.stdout).contains(hint));
}
