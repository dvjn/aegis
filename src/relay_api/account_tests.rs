use super::*;
use crate::api_keys::KeyStore;
use crate::{
    config::{ProviderConfig, ProviderKind},
    domain::{
        AuthConfig, ClientRegistration, DEVICE_GRANT_TYPE, DeviceAuthorizationRequest,
        DeviceTokenRequest, OAuthConfig, Password, bootstrap_superuser,
    },
    migration::Migrator,
    policies::Pipeline,
    telemetry::SqliteSink,
};
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement};
use sea_orm_migration::MigratorTrait;
use tokio::sync::mpsc::Receiver;
use tower::ServiceExt;

const ISSUER: &str = "https://aegis.example";
const CLAUDE_ACCOUNT: &str = "0195f3a0-0000-7000-8000-000000000001";
const CODEX_ACCOUNT: &str = "0195f3a0-0000-7000-8000-000000000002";
const PROFILE: &str = "0195f3a0-0000-7000-8000-000000000003";
const OTHER_PROFILE: &str = "0195f3a0-0000-7000-8000-000000000004";
const RUN: &str = "0195f3a0-0000-7000-8000-0000000000aa";

struct Harness {
    app: Router,
    db: DatabaseConnection,
    /// Headers of each upstream request.
    upstream: Receiver<HeaderMap>,
    /// Token of the first client; the second client's is `other`.
    token: String,
    other: String,
    backend: tokio::task::JoinHandle<()>,
}

async fn harness() -> Harness {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let (sent, received) = tokio::sync::mpsc::channel(16);
    let backend = tokio::spawn(async move {
        axum::serve(listener,Router::new().fallback(axum::routing::any(move |request:Request<Body>| {
            let sent=sent.clone(); async move {
                sent.send(request.headers().clone()).await.unwrap();
                Json(serde_json::json!({"model":"test-model","usage":{"input_tokens":8,"output_tokens":2}}))
            }
        }))).await.unwrap();
    });
    let db = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&db, None).await.unwrap();
    let password = Password::new("relay test passw0rd!".into()).unwrap();
    bootstrap_superuser(&db, b"pepper", "pepper", "relay@example.com", &password)
        .await
        .unwrap();
    let domain = Arc::new(
        Domain::new(
            db.clone(),
            AuthConfig {
                session_hmac_key: [3; 32],
                session_key_id: "session".into(),
                password_pepper: b"pepper".to_vec(),
                pepper_key_id: "pepper".into(),
                password_concurrency: 2,
                idle_lifetime: std::time::Duration::from_secs(3600),
                absolute_lifetime: std::time::Duration::from_secs(7200),
            },
            OAuthConfig {
                hmac_key: [4; 32],
                key_id: "oauth".into(),
            },
        )
        .await
        .unwrap(),
    );
    let login = domain
        .auth()
        .login("relay@example.com", &password, None)
        .await
        .unwrap();
    let principal = domain
        .auth()
        .authenticate(login.token(), None)
        .await
        .unwrap();
    let resource = format!("{ISSUER}{RESOURCE_PATH}");
    let mut tokens = vec![];
    for name in ["first", "second"] {
        let client = domain
            .oauth()
            .register_public_client(ClientRegistration {
                issuer: ISSUER.into(),
                redirect_uris: vec![],
                client_name: Some(name.into()),
                application_type: Some("native".into()),
                grant_types: vec![DEVICE_GRANT_TYPE.into(), "refresh_token".into()],
                response_types: vec![],
                scope: "relay:manage offline_access".into(),
            })
            .await
            .unwrap();
        let device = domain
            .oauth()
            .issue_device_authorization(DeviceAuthorizationRequest {
                client_id: client.client_id(),
                issuer: ISSUER,
                resource: &resource,
                scope: Some("relay:manage offline_access"),
            })
            .await
            .unwrap();
        domain
            .oauth()
            .decide_device_authorization(
                device.user_code(),
                &principal.identity,
                Some("relay:manage offline_access"),
            )
            .await
            .unwrap();
        let token = domain
            .oauth()
            .redeem_device_code(DeviceTokenRequest {
                device_code: device.device_code(),
                client_id: client.client_id(),
                issuer: ISSUER,
                resource: Some(&resource),
            })
            .await
            .unwrap();
        tokens.push(token.expose().to_owned());
    }
    let keys = KeyStore::new(db.clone());
    let gateway = Gateway::new(
        SqliteSink::new(db.clone()),
        keys.clone(),
        Pipeline::default(),
        vec![
            ProviderConfig {
                id: "claude".into(),
                kind: ProviderKind::ClaudeSubscription {
                    base_url: upstream.clone(),
                },
            },
            ProviderConfig {
                id: "codex".into(),
                kind: ProviderKind::CodexSubscription {
                    base_url: upstream.clone(),
                },
            },
            ProviderConfig {
                id: "codex2".into(),
                kind: ProviderKind::CodexSubscription {
                    base_url: upstream.clone(),
                },
            },
        ],
        1024,
    )
    .unwrap();
    let app = crate::app::router(
        domain.clone(),
        gateway,
        keys,
        crate::usage::UsageStore::new(db.clone()),
        OriginPolicy::new(Some(ISSUER.into())),
        tokio_util::sync::CancellationToken::new(),
    );
    let harness = Harness {
        app,
        db,
        upstream: received,
        other: tokens.pop().unwrap(),
        token: tokens.pop().unwrap(),
        backend,
    };
    // Each client may use claude and codex; only the first also has codex2.
    for (token, providers) in [
        (
            &harness.token,
            serde_json::json!(["claude", "codex", "codex2"]),
        ),
        (&harness.other, serde_json::json!(["claude", "codex"])),
    ] {
        let response = harness
            .api(
                "POST",
                "/api/relay/enroll",
                token,
                serde_json::json!({"providers": providers}),
            )
            .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
    harness
}

impl Harness {
    async fn api(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: serde_json::Value,
    ) -> Response {
        self.app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header(header::HOST, "aegis.example")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn delete(&self, kind: &str, token: &str, id: &str) -> StatusCode {
        self.api(
            "DELETE",
            &format!("/api/relay/{kind}/{id}"),
            token,
            serde_json::Value::Null,
        )
        .await
        .status()
    }

    async fn account(
        &self,
        token: &str,
        id: &str,
        vendor: &str,
        label: &str,
        provider: &str,
    ) -> StatusCode {
        self.api(
            "PUT",
            &format!("/api/relay/accounts/{id}"),
            token,
            serde_json::json!({"vendor": vendor, "label": label, "provider": provider}),
        )
        .await
        .status()
    }

    async fn profile(&self, token: &str, id: &str, label: &str) -> StatusCode {
        self.api(
            "PUT",
            &format!("/api/relay/profiles/{id}"),
            token,
            serde_json::json!({"label": label}),
        )
        .await
        .status()
    }

    /// One gateway request with the given extra headers.
    async fn send(&self, path: &str, token: &str, extra: &[(&str, &str)]) -> StatusCode {
        let mut request = Request::post(path)
            .header(header::HOST, "aegis.example")
            .header(header::AUTHORIZATION, "Bearer provider-oauth")
            .header(crate::ags_wire::ACCESS_TOKEN_HEADER, token)
            .header(header::CONTENT_TYPE, "application/json");
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        let response = self
            .app
            .clone()
            .oneshot(
                request
                    .body(Body::from(r#"{"model":"test-model","input":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        to_bytes(response.into_body(), 65536).await.unwrap();
        status
    }

    async fn value(&self, sql: &str) -> Option<String> {
        self.db
            .query_one_raw(Statement::from_string(DbBackend::Sqlite, sql))
            .await
            .unwrap()
            .and_then(|row| row.try_get_by_index::<Option<String>>(0).unwrap())
    }

    async fn wait_completed(&self, count: i64) {
        for _ in 0..200 {
            let done = self
                .value("SELECT CAST(COUNT(*) AS TEXT) FROM gateway_requests WHERE completed_at IS NOT NULL")
                .await
                .unwrap();
            if done.parse::<i64>().unwrap() >= count {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("requests did not complete");
    }
}

#[tokio::test]
async fn accounts_register_update_delete_and_stay_scoped_to_their_client() {
    let h = harness().await;
    let ok = StatusCode::NO_CONTENT;
    let bad = StatusCode::BAD_REQUEST;
    assert_eq!(
        h.account(&h.token, CLAUDE_ACCOUNT, "anthropic", "personal", "claude")
            .await,
        ok
    );
    assert_eq!(
        h.account(&h.token, CLAUDE_ACCOUNT, "anthropic", "  work  ", "claude")
            .await,
        ok
    );
    assert_eq!(
        h.value("SELECT COUNT(*) || label || vendor FROM relay_accounts")
            .await
            .as_deref(),
        Some("1workanthropic")
    );
    // Another client can neither take over nor delete the account.
    assert_eq!(
        h.account(&h.other, CLAUDE_ACCOUNT, "anthropic", "x", "claude")
            .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.delete("accounts", &h.other, CLAUDE_ACCOUNT).await,
        StatusCode::NOT_FOUND
    );
    // The vendor of an account cannot change.
    assert_eq!(
        h.account(&h.token, CLAUDE_ACCOUNT, "openai", "x", "codex")
            .await,
        bad
    );
    // A route the client was not granted, a provider the gateway lacks, a route of the wrong
    // kind for the vendor, unknown or reserved vendors, bad IDs and bad labels.
    assert_eq!(
        h.account(&h.other, OTHER_PROFILE, "openai", "x", "codex2")
            .await,
        bad,
        "codex2 is not granted to the second client"
    );
    for (vendor, provider) in [
        ("anthropic", "missing"),
        ("anthropic", "codex"),
        ("openai", "claude"),
        ("google", "claude"),
        ("vim", "claude"),
    ] {
        assert_eq!(
            h.account(&h.token, OTHER_PROFILE, vendor, "x", provider)
                .await,
            bad,
            "{vendor} on {provider}"
        );
    }
    assert_eq!(
        h.account(&h.token, "not-a-uuid", "anthropic", "x", "claude")
            .await,
        bad
    );
    for label in ["", "   ", &"x".repeat(65)] {
        assert_eq!(
            h.account(&h.token, OTHER_PROFILE, "anthropic", label, "claude")
                .await,
            bad
        );
    }
    let anonymous = h
        .app
        .clone()
        .oneshot(
            Request::put(format!("/api/relay/accounts/{CLAUDE_ACCOUNT}"))
                .header(header::HOST, "aegis.example")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"vendor":"anthropic","label":"a","provider":"claude"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    // Delete is soft; registering again revives the account.
    assert_eq!(h.delete("accounts", &h.token, CLAUDE_ACCOUNT).await, ok);
    assert_eq!(
        h.delete("accounts", &h.token, CLAUDE_ACCOUNT).await,
        StatusCode::NOT_FOUND
    );
    let path = "/providers/claude/v1/messages";
    assert_eq!(
        h.send(path, &h.token, &[("x-aegis-account", CLAUDE_ACCOUNT)])
            .await,
        StatusCode::FORBIDDEN,
        "a deleted account cannot carry traffic"
    );
    assert_eq!(
        h.account(&h.token, CLAUDE_ACCOUNT, "anthropic", "work", "claude")
            .await,
        ok
    );
    assert_eq!(h.value("SELECT deleted_at FROM relay_accounts").await, None);
    assert_eq!(
        h.send(path, &h.token, &[("x-aegis-account", CLAUDE_ACCOUNT)])
            .await,
        StatusCode::OK
    );
    let connection = h
        .api(
            "GET",
            "/api/relay/connection",
            &h.token,
            serde_json::Value::Null,
        )
        .await;
    let connection: Connection =
        serde_json::from_slice(&to_bytes(connection.into_body(), 65536).await.unwrap()).unwrap();
    assert_eq!(connection.capabilities, ["accounts"]);
    h.backend.abort();
}

#[tokio::test]
async fn profiles_register_rename_delete_and_stay_scoped_to_their_client() {
    let h = harness().await;
    let ok = StatusCode::NO_CONTENT;
    assert_eq!(h.profile(&h.token, PROFILE, "work").await, ok);
    assert_eq!(h.profile(&h.token, PROFILE, "job").await, ok);
    assert_eq!(
        h.value("SELECT COUNT(*) || label FROM relay_profiles")
            .await
            .as_deref(),
        Some("1job")
    );
    assert_eq!(
        h.profile(&h.other, PROFILE, "mine").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.delete("profiles", &h.other, PROFILE).await,
        StatusCode::NOT_FOUND
    );
    for (id, label) in [
        ("not-a-uuid", "x"),
        (OTHER_PROFILE, ""),
        (OTHER_PROFILE, &"x".repeat(65)[..]),
    ] {
        assert_eq!(
            h.profile(&h.token, id, label).await,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(h.delete("profiles", &h.token, PROFILE).await, ok);
    assert_eq!(
        h.delete("profiles", &h.token, PROFILE).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(h.profile(&h.token, PROFILE, "job").await, ok);
    assert_eq!(h.value("SELECT deleted_at FROM relay_profiles").await, None);
    h.backend.abort();
}

#[tokio::test]
async fn gateway_strips_attribution_validates_it_and_records_sessions() {
    let mut h = harness().await;
    let ok = StatusCode::NO_CONTENT;
    assert_eq!(
        h.account(
            &h.token,
            CLAUDE_ACCOUNT,
            "anthropic",
            "work-claude",
            "claude"
        )
        .await,
        ok
    );
    assert_eq!(
        h.account(&h.token, CODEX_ACCOUNT, "openai", "me-chatgpt", "codex")
            .await,
        ok
    );
    assert_eq!(h.profile(&h.token, PROFILE, "work").await, ok);
    assert_eq!(h.profile(&h.token, OTHER_PROFILE, "play").await, ok);
    let claude = "/providers/claude/v1/messages";
    let codex = "/providers/codex/responses";
    let stripped = |upstream: &HeaderMap| {
        !["x-aegis-account", "x-aegis-profile", "x-aegis-run"]
            .iter()
            .any(|name| upstream.contains_key(*name))
    };

    // Claude: headers stripped upstream, session recorded from the CLI's own header.
    let headers = [
        ("x-aegis-account", CLAUDE_ACCOUNT),
        ("x-aegis-profile", PROFILE),
        ("x-aegis-run", RUN),
        ("x-claude-code-session-id", "sess-claude_1"),
    ];
    assert_eq!(h.send(claude, &h.token, &headers).await, StatusCode::OK);
    let upstream = h.upstream.recv().await.unwrap();
    assert!(stripped(&upstream));
    assert_eq!(upstream["x-claude-code-session-id"], "sess-claude_1");

    // Codex: session-id (current) wins over the legacy session_id/conversation_id; the
    // upstream account is recorded and a change is flagged on the account.
    let mut headers = vec![
        ("x-aegis-account", CODEX_ACCOUNT),
        ("x-aegis-profile", OTHER_PROFILE),
        ("x-aegis-run", RUN),
        ("session-id", "codex-s1"),
        ("session_id", "legacy-s1"),
        ("conversation_id", "conv-1"),
        ("chatgpt-account-id", "acct-a"),
    ];
    assert_eq!(h.send(codex, &h.token, &headers).await, StatusCode::OK);
    let upstream = h.upstream.recv().await.unwrap();
    assert!(stripped(&upstream));
    assert_eq!(upstream["chatgpt-account-id"], "acct-a");
    h.wait_completed(2).await;
    let changed =
        format!("SELECT account_changed_at FROM relay_accounts WHERE id='{CODEX_ACCOUNT}'");
    assert_eq!(h.value(&changed).await, None);
    headers.pop();
    headers.push(("chatgpt-account-id", "acct-b"));
    assert_eq!(h.send(codex, &h.token, &headers).await, StatusCode::OK);
    h.upstream.recv().await.unwrap();
    assert_eq!(
        h.value(&format!(
            "SELECT observed_account FROM relay_accounts WHERE id='{CODEX_ACCOUNT}'"
        ))
        .await
        .as_deref(),
        Some("acct-b")
    );
    assert!(h.value(&changed).await.is_some());
    assert_eq!(
        h.value(&format!(
            "SELECT account_changed_at FROM relay_accounts WHERE id='{CLAUDE_ACCOUNT}'"
        ))
        .await,
        None
    );
    // An account needs no profile; conversation_id is the session fallback and invalid
    // identifiers are not recorded.
    let headers = [
        ("x-aegis-account", CODEX_ACCOUNT),
        ("conversation_id", "conv-2"),
        ("chatgpt-account-id", "bad value!"),
    ];
    assert_eq!(h.send(codex, &h.token, &headers).await, StatusCode::OK);
    h.upstream.recv().await.unwrap();
    // No attribution at all is allowed.
    assert_eq!(h.send(claude, &h.token, &[]).await, StatusCode::OK);
    h.upstream.recv().await.unwrap();
    h.wait_completed(5).await;
    assert_eq!(
        h.value("SELECT account_id||'/'||profile_id||'/'||run_id||'/'||session_id FROM gateway_requests WHERE endpoint LIKE '%v1/messages' AND session_id IS NOT NULL")
            .await
            .as_deref(),
        Some(format!("{CLAUDE_ACCOUNT}/{PROFILE}/{RUN}/sess-claude_1").as_str())
    );
    assert_eq!(
        h.value("SELECT group_concat(session_id||':'||COALESCE(upstream_account,'-')||':'||COALESCE(profile_id,'-'),',') FROM (SELECT * FROM gateway_requests WHERE endpoint LIKE '%/responses' ORDER BY started_at, seq)")
            .await
            .as_deref(),
        Some(format!("codex-s1:acct-a:{OTHER_PROFILE},codex-s1:acct-b:{OTHER_PROFILE},conv-2:-:-").as_str())
    );

    // Hourly buckets split by account and profile; the dashboard queries read them.
    let user = h
        .value("SELECT user_id FROM gateway_clients LIMIT 1")
        .await
        .unwrap();
    crate::usage_hourly::rebuild(&h.db).await.unwrap();
    assert_eq!(
        h.value("SELECT CAST(COUNT(DISTINCT account_id||'/'||profile_id) AS TEXT) FROM gateway_usage_hourly")
            .await
            .as_deref(),
        Some("4")
    );
    let usage = crate::usage::UsageStore::new(h.db.clone());
    let window = crate::usage::Range::Week.window(chrono::Utc::now());
    let user = uuid::Uuid::parse_str(&user).unwrap();
    let accounts = usage.by_account(user, window).await.unwrap();
    assert_eq!(accounts.len(), 3);
    let codex_usage = accounts.iter().find(|a| a.id == CODEX_ACCOUNT).unwrap();
    assert_eq!(
        (
            codex_usage.label.as_deref(),
            codex_usage.vendor.as_deref(),
            codex_usage.requests,
            codex_usage.account_changed
        ),
        (Some("me-chatgpt"), Some("openai"), 3, true)
    );
    let claude_usage = accounts.iter().find(|a| a.id == CLAUDE_ACCOUNT).unwrap();
    assert_eq!(
        (claude_usage.requests, claude_usage.account_changed),
        (1, false)
    );
    assert_eq!(
        accounts.iter().find(|a| a.id.is_empty()).unwrap().requests,
        1
    );
    let profiles = usage.by_profile(user, window).await.unwrap();
    assert_eq!(profiles.len(), 3);
    let by_id = |id: &str| profiles.iter().find(|p| p.id == id).unwrap();
    assert_eq!(
        (by_id(PROFILE).label.as_deref(), by_id(PROFILE).requests),
        (Some("work"), 1)
    );
    assert_eq!(by_id(OTHER_PROFILE).requests, 2);
    assert_eq!(by_id("").requests, 2);
    let sessions = usage.recent_sessions(user, window, 10).await.unwrap();
    assert_eq!(sessions.len(), 3);
    let session = sessions.iter().find(|s| s.id == "sess-claude_1").unwrap();
    assert_eq!(
        (
            session.profile_label.as_deref(),
            session.account_label.as_deref()
        ),
        (Some("work"), Some("work-claude"))
    );
    let session = sessions.iter().find(|s| s.id == "conv-2").unwrap();
    assert_eq!(
        (
            session.profile_label.as_deref(),
            session.account_label.as_deref()
        ),
        (None, Some("me-chatgpt"))
    );

    // Deleted rows still show by label.
    assert_eq!(h.delete("accounts", &h.token, CLAUDE_ACCOUNT).await, ok);
    assert_eq!(h.delete("profiles", &h.token, PROFILE).await, ok);
    let accounts = usage.by_account(user, window).await.unwrap();
    assert_eq!(
        accounts
            .iter()
            .find(|a| a.id == CLAUDE_ACCOUNT)
            .unwrap()
            .label
            .as_deref(),
        Some("work-claude")
    );
    assert_eq!(
        h.send(claude, &h.token, &[("x-aegis-account", CLAUDE_ACCOUNT)])
            .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.send(
            codex,
            &h.token,
            &[
                ("x-aegis-account", CODEX_ACCOUNT),
                ("x-aegis-profile", PROFILE)
            ]
        )
        .await,
        StatusCode::FORBIDDEN,
        "a deleted profile cannot carry traffic"
    );
    assert_eq!(h.profile(&h.token, PROFILE, "work").await, ok);
    assert_eq!(
        h.account(
            &h.token,
            CLAUDE_ACCOUNT,
            "anthropic",
            "work-claude",
            "claude"
        )
        .await,
        ok
    );

    // Rejected: foreign client's account or profile, route mismatch, malformed or unknown
    // IDs, profile or run without an account.
    let foreign = "0195f3a0-0000-7000-8000-0000000000f1";
    assert_eq!(
        h.account(&h.other, foreign, "anthropic", "theirs", "claude")
            .await,
        ok
    );
    let acct = ("x-aegis-account", CLAUDE_ACCOUNT);
    type Case<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)], StatusCode);
    let cases: &[Case] = &[
        (
            "foreign account",
            claude,
            &[("x-aegis-account", foreign)],
            StatusCode::FORBIDDEN,
        ),
        (
            "unknown account",
            claude,
            &[("x-aegis-account", "0195f3a0-0000-7000-8000-0000000000ff")],
            StatusCode::FORBIDDEN,
        ),
        (
            "malformed account",
            claude,
            &[("x-aegis-account", "nope")],
            StatusCode::FORBIDDEN,
        ),
        (
            "claude account on codex",
            codex,
            &[acct],
            StatusCode::FORBIDDEN,
        ),
        (
            "codex account on codex2",
            "/providers/codex2/responses",
            &[("x-aegis-account", CODEX_ACCOUNT)],
            StatusCode::FORBIDDEN,
        ),
        (
            "foreign profile",
            claude,
            &[
                acct,
                ("x-aegis-profile", "0195f3a0-0000-7000-8000-0000000000f2"),
            ],
            StatusCode::FORBIDDEN,
        ),
        (
            "malformed profile",
            claude,
            &[acct, ("x-aegis-profile", "nope")],
            StatusCode::FORBIDDEN,
        ),
        (
            "profile without account",
            claude,
            &[("x-aegis-profile", PROFILE)],
            StatusCode::BAD_REQUEST,
        ),
        (
            "run without account",
            claude,
            &[("x-aegis-run", RUN)],
            StatusCode::BAD_REQUEST,
        ),
        (
            "malformed run",
            claude,
            &[acct, ("x-aegis-run", "nope")],
            StatusCode::BAD_REQUEST,
        ),
    ];
    for (name, path, headers, expected) in cases {
        assert_eq!(h.send(path, &h.token, headers).await, *expected, "{name}");
    }
    // The other client cannot use this client's account or profile either.
    assert_eq!(
        h.send(claude, &h.other, &[acct]).await,
        StatusCode::FORBIDDEN
    );
    let foreign_profile = "0195f3a0-0000-7000-8000-0000000000f3";
    assert_eq!(h.profile(&h.other, foreign_profile, "theirs").await, ok);
    assert_eq!(
        h.send(
            claude,
            &h.token,
            &[acct, ("x-aegis-profile", foreign_profile)]
        )
        .await,
        StatusCode::FORBIDDEN
    );

    // API keys ignore and strip attribution headers.
    let user_key = KeyStore::new(h.db.clone())
        .create(user, "manual", &["claude".into()])
        .await
        .unwrap()
        .1;
    let response = h
        .app
        .clone()
        .oneshot(
            Request::post(claude)
                .header(header::HOST, "aegis.example")
                .header(header::AUTHORIZATION, "Bearer provider-oauth")
                .header("x-aegis-api-key", user_key)
                .header("x-aegis-account", CLAUDE_ACCOUNT)
                .header("x-aegis-profile", PROFILE)
                .header("x-aegis-run", "nope")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"model":"test-model","input":"hi"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let upstream = h.upstream.recv().await.unwrap();
    assert!(stripped(&upstream));
    h.wait_completed(6).await;
    assert_eq!(
        h.value("SELECT CAST(COUNT(*) AS TEXT) FROM gateway_requests WHERE account_id IS NOT NULL")
            .await
            .as_deref(),
        Some("4")
    );
    h.backend.abort();
}
