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
use sea_orm::{ConnectionTrait, Database};
use sea_orm_migration::MigratorTrait;
use tower::ServiceExt;

#[tokio::test]
async fn oauth_gateway_preserves_provider_auth_records_app_usage_and_checks_grants() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let (sent, mut received) = tokio::sync::mpsc::channel(8);
    let backend = tokio::spawn(async move {
        axum::serve(listener,Router::new().fallback(axum::routing::any(move |request:Request<Body>| {
            let sent=sent.clone(); async move {
                assert_eq!(request.headers()[header::AUTHORIZATION],"Bearer provider-oauth");
                assert!(!request.headers().contains_key(crate::ags_wire::ACCESS_TOKEN_HEADER));
                assert!(!request.headers().contains_key("x-aegis-api-key"));
                sent.send(()).await.unwrap();
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
    let issuer = "https://aegis.example";
    let resource = format!("{issuer}{RESOURCE_PATH}");
    let client = domain
        .oauth()
        .register_public_client(ClientRegistration {
            issuer: issuer.into(),
            redirect_uris: vec![],
            client_name: Some("AGS test".into()),
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
            issuer,
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
            issuer,
            resource: Some(&resource),
        })
        .await
        .unwrap();
    let keys = KeyStore::new(db.clone());
    let gateway = Gateway::new(
        SqliteSink::new(db.clone()),
        keys.clone(),
        Pipeline::default(),
        ["personal", "other"]
            .into_iter()
            .map(|id| ProviderConfig {
                id: id.into(),
                kind: ProviderKind::CodexSubscription {
                    base_url: upstream.clone(),
                },
            })
            .collect(),
        1024,
    )
    .unwrap();
    let state = RelayState {
        domain: domain.clone(),
        gateway,
        origin: OriginPolicy::new(Some(issuer.into())),
        artifacts: "/nonexistent".into(),
    };
    let app = crate::app::router(
        domain.clone(),
        state.gateway.clone(),
        keys.clone(),
        crate::usage::UsageStore::new(db.clone()),
        state.origin.clone(),
        tokio_util::sync::CancellationToken::new(),
    );
    let request = |method: &str, path: &str, value: serde_json::Value| {
        Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "aegis.example")
            .header(header::AUTHORIZATION, format!("Bearer {}", token.expose()))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .unwrap()
    };
    let installer = app
        .clone()
        .oneshot(
            Request::get("/install-mise.sh")
                .header(header::HOST, "aegis.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(installer.status(), StatusCode::OK);
    let script = String::from_utf8(
        to_bytes(installer.into_body(), 16384)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(script.contains("server='https://aegis.example'"));
    assert!(script.contains("http:ags["));
    assert!(!script.contains("github"));
    let anonymous = app
        .clone()
        .oneshot(
            Request::get("/api/relay/connection")
                .header(header::HOST, "aegis.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    // Exercise the actual device/refresh HTTP interface, not only the domain methods.
    let form_request = |path: &str, pairs: &[(&str, &str)]| {
        let mut form = url::form_urlencoded::Serializer::new(String::new());
        form.extend_pairs(pairs.iter().copied());
        Request::post(path)
            .header(header::HOST, "aegis.example")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(form.finish()))
            .unwrap()
    };
    let response = app
        .clone()
        .oneshot(form_request(
            "/oauth/device_authorization",
            &[
                ("client_id", client.client_id()),
                ("resource", &resource),
                ("scope", "relay:manage offline_access"),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let device: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
    domain
        .oauth()
        .decide_device_authorization(
            device["user_code"].as_str().unwrap(),
            &principal.identity,
            Some("relay:manage offline_access"),
        )
        .await
        .unwrap();
    let response = app
        .clone()
        .oneshot(form_request(
            "/oauth/token",
            &[
                ("grant_type", DEVICE_GRANT_TYPE),
                ("client_id", client.client_id()),
                ("resource", &resource),
                ("device_code", device["device_code"].as_str().unwrap()),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let issued: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
    let response = app
        .clone()
        .oneshot(form_request(
            "/oauth/token",
            &[
                ("grant_type", "refresh_token"),
                ("client_id", client.client_id()),
                ("resource", &resource),
                ("refresh_token", issued["refresh_token"].as_str().unwrap()),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let catalog = app
        .clone()
        .oneshot(request(
            "GET",
            "/api/relay/connection",
            serde_json::Value::Null,
        ))
        .await
        .unwrap();
    assert_eq!(catalog.status(), StatusCode::OK);
    let catalog: Connection =
        serde_json::from_slice(&to_bytes(catalog.into_body(), 65536).await.unwrap()).unwrap();
    assert!(catalog.providers.iter().any(|p| p.id == "personal"));
    let invalid = app
        .clone()
        .oneshot(request(
            "POST",
            "/api/relay/enroll",
            serde_json::json!({"providers":["missing"]}),
        ))
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(request(
                "POST",
                "/api/relay/enroll",
                serde_json::json!({"providers":["personal"]}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(
            to_bytes(response.into_body(), 1024)
                .await
                .unwrap()
                .is_empty(),
            "enrollment must not return a credential"
        );
    }
    let count = |sql: &'static str| {
        let db = db.clone();
        async move {
            db.query_one_raw(sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                sql,
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get::<i64>("", "n")
            .unwrap()
        }
    };
    assert_eq!(count("SELECT COUNT(*) n FROM gateway_keys").await, 0);
    assert_eq!(
        count("SELECT COUNT(*) n FROM gateway_key_versions").await,
        0
    );
    assert_eq!(
        count("SELECT COUNT(*) n FROM gateway_clients WHERE kind='oauth'").await,
        1
    );
    let gateway_request = |route: &str, credential: &str, oauth: bool| {
        let mut request = Request::post(format!("/providers/{route}/responses"))
            .header(header::HOST, "aegis.example")
            .header(header::AUTHORIZATION, "Bearer provider-oauth")
            .header(header::CONTENT_TYPE, "application/json");
        request = request.header(
            if oauth {
                crate::ags_wire::ACCESS_TOKEN_HEADER
            } else {
                "x-aegis-api-key"
            },
            credential,
        );
        request
            .body(Body::from(r#"{"model":"test-model","input":"hi"}"#))
            .unwrap()
    };
    let response = app
        .clone()
        .oneshot(gateway_request("personal", token.expose(), true))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 65536).await.unwrap();
    received.recv().await.unwrap();
    assert_eq!(
        app.clone()
            .oneshot(gateway_request("other", token.expose(), true))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.clone()
            .oneshot(gateway_request("personal", "invalid-token", true))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let mut mixed = gateway_request("personal", token.expose(), true);
    mixed
        .headers_mut()
        .insert("x-aegis-api-key", "ignored".parse().unwrap());
    assert_eq!(
        app.clone().oneshot(mixed).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    db.execute_unprepared("UPDATE oauth_access_tokens SET scope='usage:read'")
        .await
        .unwrap();
    assert_eq!(
        app.clone()
            .oneshot(gateway_request("personal", token.expose(), true))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    db.execute_unprepared("UPDATE oauth_access_tokens SET scope='relay:manage offline_access'")
        .await
        .unwrap();
    db.execute_unprepared("UPDATE oauth_access_tokens SET resource='https://aegis.example/mcp'")
        .await
        .unwrap();
    assert_eq!(
        app.clone()
            .oneshot(gateway_request("personal", token.expose(), true))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    db.execute_unprepared("UPDATE oauth_access_tokens SET resource='https://aegis.example/api/relay',expires_at='2000-01-01T00:00:00Z'").await.unwrap();
    assert_eq!(
        app.clone()
            .oneshot(gateway_request("personal", token.expose(), true))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    db.execute_unprepared("UPDATE oauth_access_tokens SET expires_at='2099-01-01T00:00:00Z'")
        .await
        .unwrap();
    let (_, manual) = keys
        .create(principal.user_id(), "manual", &["personal".into()])
        .await
        .unwrap();
    let response = app
        .clone()
        .oneshot(gateway_request("personal", &manual, false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), 65536).await.unwrap();
    received.recv().await.unwrap();
    for _ in 0..100 {
        if count("SELECT COUNT(*) n FROM gateway_requests WHERE completed_at IS NOT NULL").await
            == 2
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(count("SELECT COUNT(*) n FROM gateway_requests WHERE key_id IS NULL AND key_version_id IS NULL AND client_id IS NOT NULL").await,1);
    let usage = crate::usage::UsageStore::new(db.clone());
    let window = crate::usage::Range::Week.window(chrono::Utc::now());
    assert_eq!(
        usage
            .totals(principal.user_id(), window)
            .await
            .unwrap()
            .requests,
        2
    );
    let clients = usage.by_client(principal.user_id(), window).await.unwrap();
    assert_eq!(clients.len(), 2);
    let connected_usage = clients
        .iter()
        .find(|client| client.kind == "oauth")
        .unwrap();
    assert!(connected_usage.name.starts_with("AGS test"));
    assert_eq!(connected_usage.tokens, 10);
    let manual = clients
        .iter()
        .find(|client| client.kind == "api_key")
        .unwrap();
    assert_eq!(manual.name, "manual");
    assert_eq!(manual.tokens, 10);
    assert_eq!(
        keys.list_for_user(principal.user_id()).await.unwrap().len(),
        1
    );
    let connected = domain
        .oauth()
        .list_connected_clients(&principal, issuer)
        .await
        .unwrap();
    assert_eq!(connected.len(), 1);
    assert_eq!(connected[0].client_name.as_deref(), Some("AGS test"));
    domain
        .oauth()
        .revoke_client_grants(&principal, issuer, client.client_id())
        .await
        .unwrap();
    assert_eq!(
        app.clone()
            .oneshot(gateway_request("personal", token.expose(), true))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.clone()
            .oneshot(request(
                "GET",
                "/api/relay/connection",
                serde_json::Value::Null
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(!crate::domain::default_registration_scope(true).contains(SCOPE));
    assert!(!crate::domain::resource_scopes().contains(&SCOPE));
    backend.abort();
}
