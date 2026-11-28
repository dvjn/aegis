//! `ags` launches a stub CLI that calls its provider through the per-launch proxy and a running server.
#![cfg(unix)]

use std::{
    ffi::OsString,
    os::unix::{fs::PermissionsExt, process::ExitStatusExt},
    path::{Path, PathBuf},
};

use ags::{Cli, Credentials, Home, Need, Offer, Pick, Step, StepStatus, Store, TypeDefault};
use sea_orm::{ConnectionTrait, Database, DbBackend, Statement};

use crate::{pairing::approve, shared::gateway::Gateway};

/// Writes an executable `name` in `dir` that runs `body` with `$out` set to `dir`.
fn stub(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nout=\"$(dirname \"$0\")\"\ncall() {{ curl -sS -o /dev/null -w '%{{http_code}}' \"$@\"; }}\n{body}\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap_or_else(|error| panic!("{name}: {error}"))
}

fn offer(id: &str, provider_type: &str) -> Offer {
    Offer {
        id: id.to_owned(),
        provider_type: provider_type.to_owned(),
        region: None,
    }
}

async fn launches(database: &Path) -> Vec<(Option<String>, String, String)> {
    let database = Database::connect(format!("sqlite://{}?mode=ro", database.display()))
        .await
        .unwrap();
    database
        .query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT r.launch_id, r.provider, c.kind FROM gateway_requests r
            JOIN gateway_clients c ON c.id = r.client_id ORDER BY r.seq",
        ))
        .await
        .unwrap()
        .iter()
        .map(|row| {
            (
                row.try_get("", "launch_id").unwrap(),
                row.try_get("", "provider").unwrap(),
                row.try_get("", "kind").unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_launched_cli_reaches_its_provider_only_through_its_own_proxy() {
    let gateway = Gateway::started().await;
    let base = gateway.base_url();
    let dir = std::env::temp_dir().join(format!("ags-launch-{}", uuid::Uuid::new_v4()));
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let clis = dir.join("clis");
    std::fs::create_dir_all(&clis).unwrap();
    stub(
        &clis,
        "claude",
        r#"case "$*" in *'auth status --json') [ -e "$out/signed-in" ] && echo '{"loggedIn":true,"authMethod":"claude.ai"}' ;; esac"#,
    );
    let store = Store::at(dir.join("ags"));
    let credentials = Credentials::new(&store, Home::at(&dir.join("home")).with_path(&clis));
    let mut approval = None;
    ags::login(&store, &base, |request| {
        approval = Some(tokio::spawn(approve(
            base.clone(),
            request.url.to_owned(),
            &["claude", "codex"],
        )));
    })
    .await
    .expect("login succeeds");
    approval.unwrap().await.unwrap();
    let token = store.load().unwrap().unwrap().access_token;

    // One provider fits Claude Code, so it is used without asking; pi uses each for its type.
    let Pick::Use(claude) = ags::pick(&store, Cli::Claude, None).await.unwrap() else {
        panic!("claude has one provider");
    };
    assert_eq!(claude, offer("claude", "claude_subscription"));
    let plan = ags::plan_pi(&store, None).await.unwrap();
    assert_eq!(
        plan.types,
        [
            (
                "claude_subscription".to_owned(),
                TypeDefault::Use(offer("claude", "claude_subscription"))
            ),
            (
                "codex_subscription".to_owned(),
                TypeDefault::Use(offer("codex", "codex_subscription"))
            ),
            ("bedrock".to_owned(), TypeDefault::None),
            ("vertex".to_owned(), TypeDefault::None),
        ]
    );
    let used = ags::use_provider(&store, Cli::Pi, "codex").await.unwrap();
    assert_eq!(used, offer("codex", "codex_subscription"));
    for (cli, provider, error) in [
        (
            Cli::Claude,
            "codex",
            "claude can't run on codex, a codex_subscription provider".to_owned(),
        ),
        (
            Cli::Claude,
            "typesafe",
            format!(
                "typesafe is not granted to this machine; run `ags logout`, then `ags login {base}` to grant it"
            ),
        ),
    ] {
        let refused = ags::pick(&store, cli, Some(provider)).await.err().unwrap();
        assert_eq!(refused.to_string(), error);
    }

    // Each granted subscription needs a sign-in for every CLI that runs on it.
    let step = |provider: &str, provider_type: &str, need| Step {
        provider: provider.to_owned(),
        provider_type: provider_type.to_owned(),
        need,
        status: StepStatus::Missing,
    };
    let steps = ags::setup_steps(&store, &credentials, None).await.unwrap();
    assert_eq!(
        steps,
        [
            step("claude", "claude_subscription", Need::SignIn(Cli::Claude)),
            step("claude", "claude_subscription", Need::SignIn(Cli::Pi)),
            step("codex", "codex_subscription", Need::SignIn(Cli::Codex)),
            step("codex", "codex_subscription", Need::SignIn(Cli::Pi)),
        ]
    );
    // Claude Code says it is signed in, in the user's own dir.
    std::fs::write(clis.join("signed-in"), "").unwrap();
    credentials
        .skip("claude", "claude_subscription", Need::SignIn(Cli::Pi))
        .unwrap();
    let steps = ags::setup_steps(&store, &credentials, Some("claude"))
        .await
        .unwrap();
    let statuses: Vec<_> = steps.iter().map(|step| step.status).collect();
    assert_eq!(statuses, [StepStatus::Done, StepStatus::Skipped]);
    credentials
        .unskip("claude", "claude_subscription", Need::SignIn(Cli::Pi))
        .unwrap();
    let steps = ags::setup_steps(&store, &credentials, Some("claude"))
        .await
        .unwrap();
    assert_eq!(steps[1].status, StepStatus::Missing);
    let refused = ags::setup_steps(&store, &credentials, Some("typesafe"))
        .await
        .err()
        .unwrap();
    assert!(refused.to_string().starts_with("typesafe is not granted"));

    std::fs::write(bin.join("body"), gateway.text_body(false)).unwrap();
    let claude_code = stub(
        &bin,
        "claude",
        r#"printf '%s\n' "$@" > "$out/args"
printf '%s' "$ANTHROPIC_BASE_URL" > "$out/base"
printf '%s' "${CLAUDE_CONFIG_DIR-}${CODEX_HOME-}${PI_CODING_AGENT_DIR-}" > "$out/config"
if [ "$1" = --settings ]; then cp "$2" "$out/settings"; fi
secret="$(printf '%s\n' "$ANTHROPIC_CUSTOM_HEADERS" | grep '^x-ags-launch-secret: ')"
printf '%s' "$secret" > "$out/secret"
call -X POST "$ANTHROPIC_BASE_URL/v1/messages" -H 'content-type: application/json' \
  -H "$secret" -H 'authorization: Bearer vendor-sign-in' -H 'x-aegis-api-key: forged' \
  -H 'x-aegis-launch-id: forged' --data-binary "@$out/body" > "$out/allowed"
call -H 'x-ags-launch-secret: 0123456789abcdef0123456789abcdef' "$ANTHROPIC_BASE_URL/v1/models" > "$out/wrong-secret"
call "$ANTHROPIC_BASE_URL/v1/models" > "$out/no-secret"
call -H "$secret" -H 'origin: http://page.example' "$ANTHROPIC_BASE_URL/v1/models" > "$out/from-a-page"
tr '\0' ' ' < /proc/$$/cmdline > "$out/cmdline"
exit 7"#,
    );
    let args = ["--resume", "-p", "--", "two words", "--help"];
    let launched = ags::launch(
        &store,
        &credentials,
        Cli::Claude,
        &claude,
        args.iter().map(OsString::from).collect(),
        Some(claude_code.as_os_str()),
    )
    .await
    .unwrap();

    assert_eq!(
        launched.status.code(),
        Some(7),
        "the CLI's exit code comes back"
    );
    assert!(launched.logged_out.is_none());
    let given = read(&bin, "args");
    let given: Vec<&str> = given.lines().collect();
    assert_eq!(given[0], "--settings", "{given:?}");
    let settings = Path::new(given[1]);
    assert!(settings.starts_with(store.dir()), "{}", settings.display());
    assert!(!settings.exists(), "the launch's settings go with it");
    assert_eq!(given[2..], args);
    // The dirs the CLI gets are whatever this process has, which ags passes on as they are.
    let own_dirs: String = ["CLAUDE_CONFIG_DIR", "CODEX_HOME", "PI_CODING_AGENT_DIR"]
        .map(|name| std::env::var(name).unwrap_or_default())
        .concat();
    assert_eq!(
        read(&bin, "config"),
        own_dirs,
        "Claude Code runs in the user's own dir"
    );
    let proxy = read(&bin, "base");
    let settings: serde_json::Value = serde_json::from_str(&read(&bin, "settings")).unwrap();
    assert_eq!(settings["env"]["ANTHROPIC_BASE_URL"], proxy.as_str());
    assert!(proxy.starts_with("http://127.0.0.1:"), "{proxy}");
    assert!(!proxy.contains(&token));
    let secret = read(&bin, "secret");
    let secret = secret.trim_start_matches("x-ags-launch-secret: ");
    assert_eq!(secret.len(), 32, "{secret}");
    assert!(!proxy.contains(secret), "the base URL carries no secret");
    assert!(
        !read(&bin, "cmdline").contains(secret),
        "nor does the command line"
    );
    assert_eq!(read(&bin, "allowed"), "200");
    assert_eq!(read(&bin, "wrong-secret"), "403");
    assert_eq!(read(&bin, "no-secret"), "403");
    assert_eq!(read(&bin, "from-a-page"), "403");
    let received = gateway.upstream.received();
    let upstream = received.last().expect("the upstream saw the request");
    assert_eq!(upstream.path, "/v1/messages");
    assert_eq!(upstream.headers["authorization"], "Bearer vendor-sign-in");
    assert!(!upstream.headers.contains_key("x-aegis-api-key"));
    assert!(!upstream.headers.contains_key("x-aegis-launch-id"));
    assert!(!upstream.headers.contains_key("x-ags-launch-secret"));
    assert!(
        upstream
            .headers
            .values()
            .all(|value| !value.contains(&token))
    );
    let closed = reqwest::Client::new()
        .get(format!("{proxy}/v1/models"))
        .send()
        .await;
    assert!(closed.is_err(), "the proxy ends with its launch");

    // pi adds only `/responses` to a base URL ending in `/codex`; the gateway gets `/responses`.
    let pi = stub(
        &bin,
        "pi",
        r#"printf '%s\n' "$@" > "$out/pi-args"
printf '%s' "${CLAUDE_CONFIG_DIR-}${CODEX_HOME-}${PI_CODING_AGENT_DIR-}" > "$out/pi-config"
base_url() { printf '%s' "$AGS_PI_ROUTES" | grep -o "\"id\":\"$1\"[^}]*" | sed 's/.*"baseUrl":"\([^"]*\)".*/\1/'; }
codex="$(base_url openai-codex)"
printf '%s' "$codex" > "$out/pi-codex"
call -X POST "$codex/responses" -H 'content-type: application/json' \
  -H "x-ags-launch-secret: $AGS_LAUNCH_SECRET" \
  --data '{"model":"gpt-test","input":"hi"}' > "$out/pi-status"
call -X POST "$(base_url amazon-bedrock)/model/m/converse" \
  -H "x-ags-launch-secret: $AGS_LAUNCH_SECRET" > "$out/pi-ungranted"
kill -TERM $$"#,
    );
    let plan = ags::plan_pi(&store, None).await.unwrap();
    let launched = ags::launch_pi(
        &store,
        &credentials,
        &plan,
        vec!["--print".into()],
        Some(pi.as_os_str()),
    )
    .await
    .unwrap();

    assert_eq!(
        launched.status.signal(),
        Some(libc::SIGTERM),
        "the CLI's signal comes back"
    );
    let pi_args = read(&bin, "pi-args");
    let pi_args: Vec<_> = pi_args.lines().collect();
    assert_eq!(pi_args[0], "--extension");
    assert!(pi_args[1].ends_with("pi-extension.js"));
    assert_eq!(pi_args[2..], ["--print"]);
    assert!(
        read(&bin, "pi-codex").ends_with("/codex/codex"),
        "{}",
        read(&bin, "pi-codex")
    );
    assert_eq!(
        read(&bin, "pi-config"),
        own_dirs,
        "pi runs in the user's own dir"
    );
    assert_eq!(read(&bin, "pi-status"), "200");
    assert_eq!(read(&bin, "pi-ungranted"), "403");
    let received = gateway.upstream.received();
    assert_eq!(received.last().unwrap().path, "/responses");

    let recorded = launches(&gateway.database_path()).await;
    assert_eq!(recorded.len(), 2, "{recorded:?}");
    let (claude_launch, pi_launch) = (&recorded[0], &recorded[1]);
    assert_eq!(
        (claude_launch.1.as_str(), claude_launch.2.as_str()),
        ("claude", "oauth")
    );
    assert_eq!(
        (pi_launch.1.as_str(), pi_launch.2.as_str()),
        ("codex", "oauth")
    );
    let claude_id = claude_launch.0.as_deref().expect("the launch is recorded");
    let pi_id = pi_launch.0.as_deref().expect("the launch is recorded");
    assert!(uuid::Uuid::parse_str(claude_id).is_ok(), "{claude_id}");
    assert_ne!(claude_id, pi_id, "each launch has its own ID");

    // The server revokes the machine; a launch learns it from a 401 and forgets the login.
    let revoked = Database::connect(format!(
        "sqlite://{}?mode=rw",
        gateway.database_path().display()
    ))
    .await
    .unwrap();
    revoked
        .execute_unprepared(
            "UPDATE oauth_access_tokens SET revoked_at = '2026-01-01T00:00:00Z';
            UPDATE oauth_refresh_token_families SET revoked_at = '2026-01-01T00:00:00Z';",
        )
        .await
        .unwrap();
    let launched = ags::launch(
        &store,
        &credentials,
        Cli::Claude,
        &claude,
        vec![],
        Some(claude_code.as_os_str()),
    )
    .await
    .unwrap();
    assert_eq!(launched.status.code(), Some(7));
    assert_eq!(read(&bin, "allowed"), "401");
    let logged_out = launched
        .logged_out
        .expect("the launch says it was logged out");
    assert_eq!(
        logged_out.to_string(),
        format!("{base} logged this machine out; run `ags login {base}` to pair it again")
    );
    assert!(store.load().unwrap().is_none(), "the login is forgotten");

    let _ = std::fs::remove_dir_all(dir);
}
