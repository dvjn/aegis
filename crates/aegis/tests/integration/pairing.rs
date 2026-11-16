//! `ags login`, `status` and `logout` against a running server, using the gateway in between.

use std::path::Path;

use ags::{Logout, Store};
use reqwest::{StatusCode, header, redirect::Policy};
use sea_orm::{ConnectionTrait, Database, DbBackend, Statement};

use crate::shared::gateway::Gateway;

const EMAIL: &str = "test@example.invalid";
const PASSWORD: &str = "Aegistest1!pass";

fn cookie_pairs(response: &reqwest::Response) -> Vec<String> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok()?.split(';').next().map(str::to_owned))
        .collect()
}

fn cookie_value<'a>(pairs: &'a [String], name: &str) -> &'a str {
    pairs
        .iter()
        .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
        .expect("the cookie is set")
}

/// Signs in on the web and approves the device page for `providers`.
pub(crate) async fn approve(base: String, page: String, providers: &[&str]) {
    let browser = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();
    let login_page = browser.get(format!("{base}/login")).send().await.unwrap();
    let csrf_cookie = cookie_pairs(&login_page);
    let signed_in = browser
        .post(format!("{base}/login"))
        .header(header::COOKIE, csrf_cookie.join("; "))
        .form(&[
            ("email", EMAIL),
            ("password", PASSWORD),
            ("csrf", cookie_value(&csrf_cookie, "aegis_csrf")),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(signed_in.status(), StatusCode::SEE_OTHER);
    let cookies = cookie_pairs(&signed_in);
    let mut form = vec![
        ("csrf", cookie_value(&cookies, "aegis_csrf")),
        ("decision", "allow"),
    ];
    form.extend(providers.iter().map(|provider| ("providers", *provider)));
    let approved = browser
        .post(page)
        .header(header::COOKIE, cookies.join("; "))
        .form(&form)
        .send()
        .await
        .unwrap();
    assert_eq!(approved.status(), StatusCode::OK);
}

async fn recorded_client(database: &Path) -> (String, String, Option<String>) {
    let database = Database::connect(format!("sqlite://{}?mode=ro", database.display()))
        .await
        .unwrap();
    let row = database
        .query_one_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT c.kind, c.name, r.key_id FROM gateway_requests r
            JOIN gateway_clients c ON c.id = r.client_id
            ORDER BY r.started_at DESC LIMIT 1",
        ))
        .await
        .unwrap()
        .expect("the request is recorded");
    (
        row.try_get("", "kind").unwrap(),
        row.try_get("", "name").unwrap(),
        row.try_get("", "key_id").unwrap(),
    )
}

#[tokio::test]
async fn a_paired_machine_reaches_its_providers_until_it_logs_out() {
    let gateway = Gateway::started().await;
    let base = gateway.base_url();
    let dir = std::env::temp_dir().join(format!("ags-pairing-{}", uuid::Uuid::new_v4()));
    let store = Store::at(dir.join("ags"));

    let mut approval = None;
    let status = ags::login(&store, &base, |request| {
        assert_eq!(request.code.len(), 14);
        approval = Some(tokio::spawn(approve(
            base.clone(),
            request.url.to_owned(),
            &["claude"],
        )));
    })
    .await
    .expect("login succeeds");
    approval.unwrap().await.unwrap();
    assert_eq!(status.server, base);
    assert_eq!(status.account, EMAIL);
    assert_eq!(status.providers, ["claude"]);
    assert_eq!(ags::status(&store).await.unwrap(), Some(status));

    let token = store.load().unwrap().unwrap().access_token;
    let granted = gateway
        .post_text_with_key(
            &gateway.claude_url(""),
            gateway.text_body(false),
            Some(token.clone()),
        )
        .await;
    assert_eq!(granted.status, Some(200), "{}", granted.body);
    let (kind, name, key_id) = recorded_client(&gateway.database_path()).await;
    assert_eq!(kind, "oauth");
    assert!(name.starts_with("ags"), "{name}");
    assert_eq!(key_id, None);
    let other = gateway
        .post_text_with_key(
            &gateway.provider_url("codex", "/responses", ""),
            gateway.text_body(false),
            Some(token.clone()),
        )
        .await;
    assert_eq!(other.status, Some(403), "{}", other.body);
    assert!(
        gateway.upstream.received().iter().all(|request| request
            .headers
            .values()
            .all(|value| !value.contains(&token))),
        "the machine's token never reaches the upstream"
    );

    // ags refreshes an expiring token itself and keeps the same connected app.
    let mut expiring = store.load().unwrap().unwrap();
    expiring.expires_at = 0;
    store.save(&expiring).unwrap();
    let refreshed = ags::current_login(&store).await.unwrap().unwrap();
    assert_ne!(refreshed.access_token, token);
    assert!(refreshed.expires_at > expiring.expires_at);
    let after_refresh = gateway
        .post_text_with_key(
            &gateway.claude_url(""),
            gateway.text_body(false),
            Some(refreshed.access_token.clone()),
        )
        .await;
    assert_eq!(after_refresh.status, Some(200), "{}", after_refresh.body);

    assert!(matches!(
        ags::logout(&store).await.unwrap(),
        Some(Logout::Revoked { server }) if server == base
    ));
    assert!(store.load().unwrap().is_none());
    assert!(ags::logout(&store).await.unwrap().is_none());
    let revoked = gateway
        .post_text_with_key(
            &gateway.claude_url(""),
            gateway.text_body(false),
            Some(refreshed.access_token),
        )
        .await;
    assert_eq!(revoked.status, Some(401), "{}", revoked.body);
    let _ = std::fs::remove_dir_all(dir);
}
