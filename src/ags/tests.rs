use super::*;
use crate::ags_wire::Provider;

#[test]
fn agy_uses_subscription_proxy_and_rejects_direct_api_settings() {
    let provider = Provider {
        id: "google-personal".into(),
        kind: "antigravity_subscription".into(),
    };
    let args = vec![
        "--model".into(),
        "gemini-3.8-flash-low".into(),
        "--print".into(),
        "prompt with spaces".into(),
    ];
    let plan = profiles::plan(
        "agy",
        &provider,
        "http://127.0.0.1:1234/private",
        args.clone(),
        Path::new("/unused"),
    )
    .unwrap();
    assert_eq!(plan.args, args);
    assert_eq!(
        plan.env,
        vec![(
            "CLOUD_CODE_URL".into(),
            "http://127.0.0.1:1234/private".into()
        )]
    );
    assert!(
        profiles::plan(
            "agy",
            &provider,
            "http://localhost",
            vec!["--settings".into(), r#"{"modelProvider":"gemini"}"#.into()],
            Path::new("/unused")
        )
        .is_err()
    );
}

#[tokio::test]
async fn reading_a_legacy_connection_removes_the_unused_key_without_repairing() {
    let home = std::env::temp_dir().join(format!("ags-legacy-state-{}", uuid::Uuid::new_v4()));
    state::private_dir(&home).unwrap();
    state::write_private(
        &home.join("connection.json"),
        serde_json::to_string(&serde_json::json!({
            "server":"https://aegis.example","client_id":"existing-app","access_token":"access",
            "refresh_token":"refresh","expires_at":state::now()+3600,"key":"obsolete-key",
            "providers":[{"id":"work","kind":"codex_subscription"}],"defaults":{"codex":"work"}
        }))
        .unwrap()
        .as_bytes(),
        false,
    )
    .unwrap();
    let config = auth::connected(&home).await.unwrap();
    assert_eq!(config.client_id, "existing-app");
    assert_eq!(config.access_token, "access");
    assert_eq!(config.defaults["codex"], "work");
    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.join("connection.json")).unwrap()).unwrap();
    assert!(stored.get("key").is_none());
    assert!(stored.get("version").is_none());
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn explicit_defaults_support_multiple_routes_for_one_subscription_type() {
    let providers = vec![
        Provider {
            id: "personal".into(),
            kind: "codex_subscription".into(),
        },
        Provider {
            id: "work".into(),
            kind: "codex_subscription".into(),
        },
    ];
    let mut config: state::Config = serde_json::from_value(serde_json::json!({
        "server":"https://aegis.example","client_id":"app","access_token":"access",
        "refresh_token":"refresh","expires_at":state::now()+3600,"providers":[],"defaults":{}
    }))
    .unwrap();
    wizard::configure(
        &mut config,
        &providers,
        &["codex=work".into(), "pi=personal".into()],
    )
    .unwrap();
    assert_eq!(
        config.provider_for("codex", "codex_subscription"),
        Some("work")
    );
    assert_eq!(
        config.provider_for("pi", "codex_subscription"),
        Some("personal")
    );
    assert_eq!(config.providers.len(), 2);
    assert!(wizard::configure(&mut config, &providers, &["claude=work".into()]).is_err());
}

#[test]
fn codex_launch_overrides_are_temporary_and_keep_vendor_sign_in() {
    let provider = Provider {
        id: "work".into(),
        kind: "codex_subscription".into(),
    };
    let plan = profiles::plan(
        "codex",
        &provider,
        "http://127.0.0.1:1234/private",
        vec!["exec".into(), "prompt with spaces".into()],
        Path::new("/unused"),
    )
    .unwrap();
    assert_eq!(plan.args[0], "exec");
    assert_eq!(plan.args[1], "prompt with spaces");
    assert!(
        plan.args
            .iter()
            .any(|a| a == "model_providers.ags.requires_openai_auth=true")
    );
    assert!(
        plan.args
            .iter()
            .any(|a| a == "model_providers.ags.supports_websockets=false")
    );
    assert!(plan.env.is_empty());
    assert!(
        profiles::plan(
            "agy",
            &provider,
            "http://localhost",
            vec![],
            Path::new("/unused")
        )
        .is_err()
    );
}

#[test]
fn server_validation_rejects_embedded_credentials_and_remote_plaintext() {
    for value in [
        "http://remote.example",
        "https://key@server.example",
        "https://server.example/path",
        "https://server.example?secret=x",
    ] {
        assert!(state::validate_server(value).is_err());
    }
    assert_eq!(
        state::validate_server("https://aegis.example/").unwrap(),
        "https://aegis.example"
    );
    assert!(state::validate_server("http://127.0.0.1:8765").is_ok());
}

#[test]
fn short_aliases_parse_like_the_full_commands() {
    use clap::Parser;
    for args in [
        &["ags", "p", "c", "work", "--claude", "new"][..],
        &["ags", "p", "s", "work", "chatgpt", "none"],
        &["ags", "p", "u", "work", "--here"],
        &["ags", "p", "ls"],
        &["ags", "a", "in", "work-claude"],
        &["ags", "a", "rm", "work-claude", "--force"],
        &["ags", "r", "claude", "-P", "work", "--", "-p", "hi"],
        &["ags", "st"],
    ] {
        assert!(Cli::try_parse_from(args).is_ok(), "{args:?}");
    }
}
