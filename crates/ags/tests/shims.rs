//! Plain `claude`, `codex` and `pi` through ags's shims, with a stand-in Aegis server.
#![cfg(unix)]

use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    os::{
        fd::{FromRawFd, OwnedFd},
        unix::fs::PermissionsExt,
    },
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::json;

const AGS: &str = env!("CARGO_BIN_EXE_ags");
/// The real CLI says how it ran, runs `NEXT` in turn after a last `nested`, and exits with `EXIT`.
/// It leaves out the settings file a launch gives Claude Code, which the launch tests check.
const REAL: &str = "#!/bin/sh\n\
    if [ \"$1\" = --settings ]; then shift 2; fi\n\
    echo \"${0##*/} $* | base=${ANTHROPIC_BASE_URL-unset} launched=${AGS_LAUNCHED-unset} bypass=${AGS_BYPASS-unset}\"\n\
    for last in \"$@\"; do :; done\n\
    if [ \"$last\" = nested ] && [ -n \"$NEXT\" ]; then next=${NEXT%% *}; rest=${NEXT#\"$next\"}; NEXT=${rest# } \"$next\" nested; fi\n\
    exit \"${EXIT:-0}\"\n";
const NOT_LOGGED_IN: &str = "not logged in; run `ags login <URL>` first";

struct Fixture {
    root: PathBuf,
    store: ags::Store,
    shims: PathBuf,
    real: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "ags-shims-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let config = if cfg!(target_os = "macos") {
            root.join("home/Library/Application Support")
        } else {
            root.join("config")
        };
        let store = ags::Store::at(config.join("ags"));
        ags::shims::install(&store, Path::new(AGS)).unwrap();
        let real = root.join("real");
        for cli in ["claude", "codex", "pi"] {
            script(&real.join(cli), REAL);
        }
        Self {
            shims: ags::shims::dir(&store),
            store,
            root,
            real,
        }
    }

    /// `PATH` is `dirs`, and nothing of the user's reaches ags.
    fn command(&self, program: impl AsRef<std::ffi::OsStr>, dirs: &[&Path]) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("SHELL", "/bin/bash")
            .env("PATH", std::env::join_paths(dirs).unwrap())
            .stdin(Stdio::null());
        command
    }

    /// Plain `cli`, with the shims first on `PATH`.
    fn plain(&self, cli: &str) -> Command {
        self.command(self.shims.join(cli), &[&self.shims, &self.real])
    }

    fn log_in(&self, server: &str, token: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        self.store
            .save(&ags::Login {
                server: server.to_owned(),
                client_id: "client".into(),
                access_token: token.to_owned(),
                refresh_token: Some(format!("{token}-refresh")),
                expires_at: now + 3600,
                lifetime: 3600,
            })
            .unwrap();
    }

    fn token(&self) -> Option<String> {
        self.store.load().unwrap().map(|login| login.access_token)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A stand-in Aegis server; a device sign-in gets the token `new` at once, or is denied.
struct Aegis {
    url: String,
    /// The access tokens whose pairing was revoked.
    unpaired: Arc<Mutex<Vec<String>>>,
}

impl Aegis {
    fn start(machines: &[(&str, &[(&str, &str)])], approve: bool) -> Self {
        use axum::{
            Json, Router,
            extract::State,
            http::{HeaderMap, Method, StatusCode},
            response::{IntoResponse, Response},
            routing::{any, get, post},
        };
        type Machines = BTreeMap<String, Vec<(String, String)>>;
        type Shared = (Arc<Machines>, Arc<Mutex<Vec<String>>>);
        let machines: Machines = machines
            .iter()
            .map(|(token, providers)| {
                let providers = providers
                    .iter()
                    .map(|(id, kind)| ((*id).to_owned(), (*kind).to_owned()))
                    .collect();
                ((*token).to_owned(), providers)
            })
            .collect();
        let unpaired = Arc::new(Mutex::new(vec![]));
        let (sender, receiver) = std::sync::mpsc::channel();
        let revoked = unpaired.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let url = format!("http://{}", listener.local_addr().unwrap());
                sender.send(url.clone()).unwrap();
                let metadata = json!({
                    "issuer": url,
                    "authorization_endpoint": format!("{url}/oauth/authorize"),
                    "token_endpoint": format!("{url}/oauth/token"),
                    "device_authorization_endpoint": format!("{url}/oauth/device_authorization"),
                    "registration_endpoint": format!("{url}/oauth/register"),
                    "response_types_supported": [],
                    "grant_types_supported": [],
                    "token_endpoint_auth_methods_supported": [],
                    "code_challenge_methods_supported": [],
                    "scopes_supported": [],
                    "authorization_response_iss_parameter_supported": true,
                    "resource_parameter_supported": true,
                });
                let device = json!({
                    "device_code": "device",
                    "user_code": "ABCD-EFGH-IJKL",
                    "verification_uri": format!("{url}/oauth/device"),
                    "verification_uri_complete": format!("{url}/oauth/device?user_code=ABCD-EFGH-IJKL"),
                    "expires_in": 600,
                    "interval": 1,
                });
                let machine = move |State((machines, revoked)): State<Shared>,
                                    method: Method,
                                    headers: HeaderMap| async move {
                    let token = headers
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.strip_prefix("Bearer "))
                        .unwrap_or_default()
                        .to_owned();
                    let Some(providers) = machines.get(&token) else {
                        return StatusCode::UNAUTHORIZED.into_response();
                    };
                    if method == Method::DELETE {
                        revoked.lock().unwrap().push(token);
                        return StatusCode::NO_CONTENT.into_response();
                    }
                    Json(json!({
                        "account": "me@example.com",
                        "name": "ags",
                        "providers": providers.iter().map(|(id, _)| id).collect::<Vec<_>>(),
                        "provider_types": providers.iter().cloned().collect::<BTreeMap<_, _>>(),
                        "provider_regions": {},
                        "provider_locations": {},
                    }))
                    .into_response()
                };
                let token = move || async move {
                    let response: Response = if approve {
                        Json(json!({
                            "access_token": "new",
                            "token_type": "Bearer",
                            "expires_in": 3600,
                            "scope": "",
                            "refresh_token": "new-refresh",
                        }))
                        .into_response()
                    } else {
                        (
                            StatusCode::BAD_REQUEST,
                            Json(json!({"error": "access_denied"})),
                        )
                            .into_response()
                    };
                    response
                };
                let app = Router::new()
                    .route(
                        aegis_wire::METADATA_PATH,
                        get(move || async move { Json(metadata) }),
                    )
                    .route(
                        "/oauth/register",
                        post(|| async {
                            (
                                StatusCode::CREATED,
                                Json(json!({
                                    "client_id": "client",
                                    "client_id_issued_at": 0,
                                    "redirect_uris": [],
                                    "client_name": "ags",
                                    "token_endpoint_auth_method": "none",
                                    "grant_types": [],
                                    "response_types": [],
                                    "scope": "",
                                    "application_type": "native",
                                })),
                            )
                        }),
                    )
                    .route(
                        "/oauth/device_authorization",
                        post(move || async move { Json(device) }),
                    )
                    .route("/oauth/token", post(token))
                    .route(aegis_wire::MACHINE_PATH, any(machine))
                    .with_state((Arc::new(machines), revoked));
                axum::serve(listener, app).await.unwrap();
            });
        });
        Self {
            url: receiver.recv().unwrap(),
            unpaired,
        }
    }

    fn unpaired(&self) -> Vec<String> {
        self.unpaired.lock().unwrap().clone()
    }
}

fn script(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// `bytes` as text, with each launch's proxy port as `PORT` and no ags process IDs.
fn text(bytes: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(bytes).replace("\r\n", "\n");
    for cli in ["claude", "codex", "pi"] {
        let mut kept = String::new();
        let mut rest = text.as_str();
        while let Some(at) = rest.find(&format!("{cli}:")) {
            let (before, after) = rest.split_at(at + cli.len());
            kept.push_str(before);
            let digits = after[1..].len()
                - after[1..]
                    .trim_start_matches(|c: char| c.is_ascii_digit())
                    .len();
            rest = if digits > 0 {
                &after[1 + digits..]
            } else {
                kept.push(':');
                &after[1..]
            };
        }
        kept.push_str(rest);
        text = kept;
    }
    let mut out = String::new();
    let mut rest = text.as_str();
    while let Some(at) = rest.find("http://127.0.0.1:") {
        let (before, after) = rest.split_at(at + "http://127.0.0.1:".len());
        out.push_str(before);
        let digits = after.len() - after.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        out.push_str(if digits > 0 { "PORT" } else { "" });
        rest = &after[digits..];
    }
    out.push_str(rest);
    out
}

/// Runs `command` at a terminal that types `typed`; returns its output and what it showed.
fn at_terminal(mut command: Command, typed: &str) -> (Output, String) {
    // SAFETY: each call gets a valid fd or name from the one before it, and is checked.
    let (terminal, user) = unsafe {
        let terminal = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(terminal >= 0);
        assert_eq!(libc::grantpt(terminal), 0);
        assert_eq!(libc::unlockpt(terminal), 0);
        let user = libc::open(libc::ptsname(terminal), libc::O_RDWR | libc::O_NOCTTY);
        assert!(user >= 0);
        let mut settings: libc::termios = std::mem::zeroed();
        assert_eq!(libc::tcgetattr(user, &raw mut settings), 0);
        settings.c_lflag &= !libc::ECHO;
        assert_eq!(libc::tcsetattr(user, libc::TCSANOW, &raw const settings), 0);
        (
            fs::File::from(OwnedFd::from_raw_fd(terminal)),
            OwnedFd::from_raw_fd(user),
        )
    };
    let mut terminal = terminal;
    terminal.write_all(typed.as_bytes()).unwrap();
    command
        .stdin(user.try_clone().unwrap())
        .stderr(user)
        .stdout(Stdio::piped());
    let child = command.spawn().unwrap();
    drop(command);
    let output = child.wait_with_output().unwrap();
    let mut shown = vec![];
    // Reading ends with an error once no process holds the terminal open.
    let _ = terminal.read_to_end(&mut shown);
    (output, text(&shown))
}

#[test]
fn setup_asks_once_for_each_clis_own_sign_in_whichever_providers_share_it() {
    let f = Fixture::new();
    let aegis = Aegis::start(
        &[(
            "old",
            &[
                ("work", "claude_subscription"),
                ("home", "claude_subscription"),
            ],
        )],
        true,
    );
    f.log_in(&aegis.url, "old");
    // The real CLIs say they are not signed in, and their sign-in is declined.
    let mut setup = f.command(AGS, &[&f.real]);
    setup.arg("setup").env("EXIT", "1");
    let (output, shown) = at_terminal(setup, "n\nn\n");
    assert!(output.status.success(), "{shown}");
    assert_eq!(
        text(&output.stdout),
        "\nwork (claude_subscription): Claude Code sign-in\n\
         \nwork (claude_subscription): pi sign-in\n\
         \nSkipped: work (Claude Code sign-in), work (pi sign-in). Set one up with `ags setup <provider>`.\n"
    );
    assert_eq!(
        shown,
        "Sign in to Claude Code now? [Y/n] Sign in to pi now? [Y/n] "
    );
}

#[test]
fn a_logged_in_shim_launches_through_aegis_once_and_a_nested_one_runs_the_real_cli() {
    let f = Fixture::new();
    let aegis = Aegis::start(&[("old", &[("claude", "claude_subscription")])], true);
    f.log_in(&aegis.url, "old");
    let output = f
        .plain("claude")
        .arg("nested")
        .env("NEXT", "claude")
        .env("EXIT", "3")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "claude nested | base=http://127.0.0.1:PORT launched=claude bypass=unset\n\
         claude nested | base=http://127.0.0.1:PORT launched=claude,claude bypass=unset\n",
        "the CLI ran once, and the claude it ran is the real one inside the same launch"
    );
    assert!(!text(&output.stderr).contains("without Aegis"));
}

#[test]
fn another_cli_inside_a_launch_gets_its_own_launch_through_aegis() {
    let f = Fixture::new();
    let aegis = Aegis::start(
        &[(
            "old",
            &[
                ("claude", "claude_subscription"),
                ("codex", "codex_subscription"),
            ],
        )],
        true,
    );
    f.log_in(&aegis.url, "old");
    let output = f
        .plain("claude")
        .arg("nested")
        .env("NEXT", "codex claude codex")
        .output()
        .unwrap();
    let (stdout, stderr) = (text(&output.stdout), text(&output.stderr));
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3, "a fourth CLI is refused: {stdout}{stderr}");
    assert_eq!(
        lines[0],
        "claude nested | base=http://127.0.0.1:PORT launched=claude bypass=unset"
    );
    assert!(lines[1].starts_with("codex -c "), "{stdout}");
    assert!(
        lines[1].contains("model_providers.ags.base_url=\"http://127.0.0.1:PORT\""),
        "codex runs through its own proxy: {stdout}"
    );
    assert!(
        lines[1].ends_with(" nested | base=unset launched=claude,codex bypass=unset"),
        "nothing of claude's launch reaches codex: {stdout}"
    );
    assert_eq!(
        lines[2],
        "claude nested | base=http://127.0.0.1:PORT launched=claude,codex,claude bypass=unset",
        "claude inside codex gets its own launch"
    );
    assert!(
        stderr.ends_with(
            "ags: codex would be 4 CLIs deep through ags, which looks like a loop; set AGS_BYPASS=1 to run it anyway\n"
        ),
        "{stderr}"
    );
}

#[test]
fn a_wrapper_that_runs_ags_itself_ends_after_a_few_rounds() {
    let f = Fixture::new();
    let aegis = Aegis::start(&[("old", &[("claude", "claude_subscription")])], true);
    f.log_in(&aegis.url, "old");
    let wrapper = f.root.join("wrapper");
    script(
        &wrapper.join("claude"),
        &format!("#!/bin/sh\necho \"round $AGS_LAUNCHED\"\nexec '{AGS}' claude \"$@\"\n"),
    );
    let output = f.command(AGS, &[&wrapper]).arg("claude").output().unwrap();
    assert!(!output.status.success());
    assert_eq!(
        text(&output.stdout),
        "round claude\nround claude,claude\nround claude,claude,claude\n"
    );
    assert_eq!(
        text(&output.stderr),
        "ags: claude would be 4 CLIs deep through ags, which looks like a loop; set AGS_BYPASS=1 to run it anyway\n"
    );
}

#[test]
fn a_shim_inside_an_ended_launch_starts_afresh() {
    let f = Fixture::new();
    let aegis = Aegis::start(&[("old", &[("claude", "claude_subscription")])], true);
    f.log_in(&aegis.url, "old");
    let mut ended = Command::new("true").spawn().unwrap();
    let gone = ended.id();
    ended.wait().unwrap();
    let alive = std::process::id();
    let run = |pid: u32| {
        f.plain("claude")
            .env("AGS_LAUNCHED", format!("claude:{pid}"))
            .env("AGS_LAUNCH_SECRET", "stale")
            .env("ANTHROPIC_BASE_URL", "http://launch.example")
            .output()
            .unwrap()
    };
    assert_eq!(
        text(&run(alive).stdout),
        "claude  | base=http://launch.example launched=claude,claude bypass=unset\n",
        "inside a running launch of claude, the real claude runs in it"
    );
    assert_eq!(
        text(&run(gone).stdout),
        "claude  | base=http://127.0.0.1:PORT launched=claude bypass=unset\n",
        "an ended launch is left behind"
    );
}

#[test]
fn a_cli_that_runs_itself_by_name_ends_after_a_few_rounds() {
    let f = Fixture::new();
    let wrapper = f.root.join("wrapper");
    script(
        &wrapper.join("claude"),
        "#!/bin/sh\necho \"round $AGS_LAUNCHED\"\nexec claude \"$@\"\n",
    );
    let output = f
        .command(f.shims.join("claude"), &[&f.shims, &wrapper])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        text(&output.stdout),
        "round claude\nround claude,claude\nround claude,claude,claude\n"
    );
    assert_eq!(
        text(&output.stderr),
        format!(
            "ags: opening claude without Aegis: {NOT_LOGGED_IN}\n\
             ags: claude would be 4 CLIs deep through ags, which looks like a loop; set AGS_BYPASS=1 to run it anyway\n"
        )
    );
}

#[test]
fn without_a_terminal_a_shim_opens_the_real_cli_with_why() {
    let f = Fixture::new();
    let output = f.plain("claude").arg("--version").output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        text(&output.stdout),
        "claude --version | base=unset launched=claude bypass=unset\n"
    );
    assert_eq!(
        text(&output.stderr),
        format!("ags: opening claude without Aegis: {NOT_LOGGED_IN}\n")
    );

    let aegis = Aegis::start(
        &[
            ("codex-only", &[("codex", "codex_subscription")]),
            ("aws", &[("aws", "bedrock")]),
            ("none", &[]),
        ],
        true,
    );
    let url = &text(aegis.url.as_bytes());
    for (token, cli, why) in [
        (
            "codex-only",
            "claude",
            format!(
                "this machine has no provider that claude can run on (claude_subscription or bedrock or vertex); run `ags logout`, then `ags login {url}` to grant one"
            ),
        ),
        (
            "aws",
            "claude",
            "aws has no Bedrock API key yet; run `ags setup aws` first".to_owned(),
        ),
        (
            "none",
            "pi",
            format!(
                "this machine has no provider that pi can run on (claude_subscription or codex_subscription or bedrock or vertex); run `ags logout`, then `ags login {url}` to grant one"
            ),
        ),
    ] {
        f.log_in(&aegis.url, token);
        let output = f.plain(cli).arg("-x").output().unwrap();
        assert!(output.status.success(), "{cli} on {token}");
        assert_eq!(
            text(&output.stdout),
            format!("{cli} -x | base=unset launched={cli} bypass=unset\n"),
            "{cli} on {token}"
        );
        assert_eq!(
            text(&output.stderr),
            format!("ags: opening {cli} without Aegis: {why}\n"),
            "{cli} on {token}"
        );
        let explicit = f
            .command(AGS, &[&f.shims, &f.real])
            .args([cli, "-x"])
            .output()
            .unwrap();
        assert!(!explicit.status.success(), "{cli} on {token}");
        assert_eq!(text(&explicit.stdout), "", "ags {cli} keeps a plain error");
        assert_eq!(text(&explicit.stderr), format!("ags: {why}\n"));
    }
}

#[test]
fn ags_bypass_runs_the_real_cli_for_that_run_only_and_outside_any_launch() {
    let f = Fixture::new();
    let output = f
        .plain("claude")
        .args(["-p", "hi"])
        .env("AGS_BYPASS", "1")
        .env("ANTHROPIC_BASE_URL", "https://mine.example")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        text(&output.stdout),
        "claude -p hi | base=https://mine.example launched=claude bypass=unset\n",
        "outside a launch the user's own settings stay"
    );
    assert_eq!(text(&output.stderr), "");
    let output = f
        .plain("claude")
        .env("AGS_BYPASS", "1")
        .env("AGS_LAUNCHED", format!("codex:{}", std::process::id()))
        .env("ANTHROPIC_BASE_URL", "https://mine.example")
        .output()
        .unwrap();
    assert_eq!(
        text(&output.stdout),
        "claude  | base=https://mine.example launched=codex,claude bypass=unset\n",
        "a run without Aegis inside another one is not inside a launch"
    );

    let env = f.root.join("env/claude");
    script(
        &env,
        "#!/bin/sh\nfor name in AGS_LAUNCH_SECRET ANTHROPIC_BASE_URL ANTHROPIC_CUSTOM_HEADERS AWS_BEARER_TOKEN_BEDROCK CLAUDE_CONFIG_DIR AGS_LAUNCHED AGS_BYPASS; do eval \"echo $name=\\${$name-unset}\"; done\n",
    );
    let output = f
        .command(f.shims.join("claude"), &[&f.shims, env.parent().unwrap()])
        .env("AGS_BYPASS", "1")
        .env("AGS_LAUNCH_SECRET", "secret")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:1")
        .env("ANTHROPIC_CUSTOM_HEADERS", "x-ags-launch-secret: secret")
        .env("AWS_BEARER_TOKEN_BEDROCK", "ABSKkey")
        .env("CLAUDE_CONFIG_DIR", "/home/me/own-claude")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "AGS_LAUNCH_SECRET=unset\nANTHROPIC_BASE_URL=unset\nANTHROPIC_CUSTOM_HEADERS=unset\n\
         AWS_BEARER_TOKEN_BEDROCK=unset\nCLAUDE_CONFIG_DIR=/home/me/own-claude\n\
         AGS_LAUNCHED=claude\nAGS_BYPASS=unset\n",
        "nothing of the launch it ran in reaches it, and the user's own dir stays"
    );
}

#[test]
fn a_shim_never_runs_itself_or_another_shim() {
    let f = Fixture::new();
    let run = f.root.join("run");
    script(
        &run.join("claude"),
        "#!/bin/sh\n# Managed by AGS\nexec ags run claude -- \"$@\"\n",
    );
    let output = f
        .command(f.shims.join("claude"), &[&f.shims, &run])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        text(&output.stderr),
        "ags: could not find claude; install it or add it to PATH\n"
    );
    let output = f
        .command(f.shims.join("claude"), &[&f.shims, &run, &f.real])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        text(&output.stdout),
        "claude  | base=unset launched=claude bypass=unset\n"
    );
    let explicit = f
        .command(AGS, &[&f.shims, &f.real])
        .arg("claude")
        .output()
        .unwrap();
    assert!(!explicit.status.success());
    assert_eq!(text(&explicit.stdout), "", "ags claude runs no shim");
    assert_eq!(text(&explicit.stderr), format!("ags: {NOT_LOGGED_IN}\n"));
}

#[test]
fn at_a_terminal_a_shim_offers_to_log_in_then_to_open_the_real_cli() {
    let f = Fixture::new();
    let mut claude = f.plain("claude");
    claude.arg("--resume");
    let (output, shown) = at_terminal(claude, "n\ny\n");
    assert!(output.status.success(), "{shown}");
    assert_eq!(
        text(&output.stdout),
        "claude --resume | base=unset launched=claude bypass=unset\n"
    );
    assert_eq!(
        shown,
        format!(
            "ags: {NOT_LOGGED_IN}\n\
             Log in to Aegis now? [Y/n] Open claude without Aegis? [Y/n] "
        )
    );

    let (output, shown) = at_terminal(f.plain("claude"), "y\nnot-a-url\nn\n");
    assert!(!output.status.success(), "{shown}");
    assert_eq!(text(&output.stdout), "", "the real CLI did not run");
    assert_eq!(
        shown,
        format!(
            "ags: {NOT_LOGGED_IN}\n\
             Log in to Aegis now? [Y/n] Server URL: ags: give the server's full URL, such as https://aegis.example\n\
             Open claude without Aegis? [Y/n] "
        )
    );
}

#[test]
fn at_a_terminal_a_shim_logs_in_and_then_launches_through_aegis() {
    let f = Fixture::new();
    let aegis = Aegis::start(&[("new", &[("claude", "claude_subscription")])], true);
    let typed = format!("y\n{}\nn\nn\n", aegis.url);
    let (output, shown) = at_terminal(f.plain("claude"), &typed);
    assert!(output.status.success(), "{shown}");
    assert_eq!(
        text(&output.stdout),
        "claude  | base=http://127.0.0.1:PORT launched=claude bypass=unset\n",
        "stdout gets only the CLI's output"
    );
    let url = text(aegis.url.as_bytes());
    assert!(
        shown.starts_with(&format!(
            "ags: {NOT_LOGGED_IN}\n\
             Log in to Aegis now? [Y/n] Server URL: Approve this machine in your browser:\n\
             \x20 {url}/oauth/device?user_code=ABCD-EFGH-IJKL\n"
        )),
        "{shown}"
    );
    assert!(
        shown.contains("Logged in to http://127.0.0.1:PORT as me@example.com.\n"),
        "{shown}"
    );
    assert_eq!(f.token().as_deref(), Some("new"));
}

#[test]
fn pairing_again_for_a_provider_keeps_the_old_pairing_until_the_new_one_is_approved() {
    for approve in [false, true] {
        let f = Fixture::new();
        let aegis = Aegis::start(
            &[
                ("old", &[("codex", "codex_subscription")]),
                ("new", &[("claude", "claude_subscription")]),
            ],
            approve,
        );
        f.log_in(&aegis.url, "old");
        let (output, shown) = at_terminal(f.plain("claude"), "y\nn\nn\nn\nn\n");
        assert!(
            shown.contains("Pair this machine with http://127.0.0.1:PORT again to grant claude a provider? [Y/n] "),
            "{shown}"
        );
        if approve {
            assert!(output.status.success(), "{shown}");
            assert_eq!(
                text(&output.stdout),
                "claude  | base=http://127.0.0.1:PORT launched=claude bypass=unset\n"
            );
            assert_eq!(f.token().as_deref(), Some("new"));
            assert_eq!(aegis.unpaired(), ["old"]);
        } else {
            assert!(!output.status.success(), "{shown}");
            assert!(
                shown.ends_with("ags: the sign-in was denied\nOpen claude without Aegis? [Y/n] "),
                "{shown}"
            );
            assert_eq!(text(&output.stdout), "");
            assert_eq!(
                f.token().as_deref(),
                Some("old"),
                "the machine is as it was"
            );
            assert!(aegis.unpaired().is_empty());
        }
    }
}

#[test]
fn logout_removes_the_shims_even_when_the_server_cannot_be_told() {
    let f = Fixture::new();
    let output = f.command(AGS, &[&f.real]).arg("logout").output().unwrap();
    assert!(output.status.success());
    assert_eq!(text(&output.stdout), "Not logged in.\n");
    assert!(!f.shims.exists());

    ags::shims::install(&f.store, Path::new(AGS)).unwrap();
    f.log_in("http://127.0.0.1:9", "old");
    let output = f.command(AGS, &[&f.real]).arg("logout").output().unwrap();
    assert!(!output.status.success());
    assert!(
        text(&output.stderr).starts_with("ags: could not revoke this machine's access"),
        "{}",
        text(&output.stderr)
    );
    assert!(!f.shims.exists());
    assert!(f.token().is_none());
}

#[test]
fn init_rewrites_shims_that_run_another_ags() {
    let f = Fixture::new();
    ags::shims::install(&f.store, Path::new("/old/ags")).unwrap();
    let before = fs::read_to_string(f.shims.join("claude")).unwrap();
    let init = || {
        f.command(AGS, &[&f.real])
            .args(["init", "fish"])
            .output()
            .unwrap()
    };
    assert!(init().status.success());
    assert_eq!(
        fs::read_to_string(f.shims.join("claude")).unwrap(),
        before,
        "not while logged out"
    );
    f.log_in("http://127.0.0.1:9", "old");
    let output = init();
    assert_eq!(text(&output.stderr), "");
    assert!(ags::shims::current(&f.store, Path::new(AGS)));
    assert!(
        !fs::read_to_string(f.shims.join("pi"))
            .unwrap()
            .contains("/old/ags")
    );
}

#[test]
fn init_writes_missing_shims_while_logged_in() {
    let f = Fixture::new();
    ags::shims::remove(&f.store).unwrap();
    let init = || {
        f.command(AGS, &[&f.real])
            .args(["init", "bash"])
            .output()
            .unwrap()
    };
    let output = init();
    assert!(output.status.success());
    assert!(!f.shims.exists(), "not while logged out");

    f.log_in("http://127.0.0.1:9", "old");
    fs::create_dir_all(&f.shims).unwrap();
    fs::write(f.shims.join("codex"), "mine").unwrap();
    let output = init();
    assert!(output.status.success());
    assert!(text(&output.stdout).starts_with("_ags_path="));
    assert_eq!(
        text(&output.stderr),
        format!(
            "ags: ags wrote no shims: {} is not one of them; move it away, then run `ags init <shell>`\n",
            f.shims.join("codex").display()
        )
    );
    assert!(!f.shims.join("claude").exists());

    fs::remove_file(f.shims.join("codex")).unwrap();
    let output = init();
    assert_eq!(text(&output.stderr), "");
    assert!(ags::shims::written(&f.shims));
}

#[test]
fn the_init_line_puts_the_shims_first_in_bash_and_fish() {
    let f = Fixture::new();
    let find = |name: &str| {
        std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|dir| dir.join(name))
            .find(|shell| shell.is_file())
    };
    let expected = format!(
        "{}:{}claude  | base=unset launched=claude bypass=unset\n",
        f.shims.display(),
        f.real.display()
    );
    let bash = find("bash").unwrap();
    let output = f
        .command(&bash, &[&f.real])
        .args([
            "--norc",
            "-c",
            &format!(
                "eval \"$({AGS} init bash)\"; eval \"$({AGS} init bash)\"; printf %s \"$PATH\"; claude"
            ),
        ])
        .output()
        .unwrap();
    assert_eq!(text(&output.stdout), expected);
    assert_eq!(
        text(&output.stderr),
        format!("ags: opening claude without Aegis: {NOT_LOGGED_IN}\n")
    );
    if let Some(fish) = find("fish") {
        let output = f
            .command(&fish, &[&f.real])
            .args([
                "--no-config",
                "-c",
                &format!(
                    "{AGS} init fish | source; {AGS} init fish | source; printf %s (string join : $PATH); claude"
                ),
            ])
            .output()
            .unwrap();
        assert_eq!(text(&output.stdout), expected);
    }
}
