use super::*;
use std::fs;

fn temp(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("ags-iso-{label}-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn account(vendor: &str, native: bool) -> Account {
    Account {
        id: "id".into(),
        name: "a".into(),
        vendor: vendor.into(),
        provider: "route".into(),
        native,
        created_at: 0,
        registered_with: None,
    }
}

#[test]
fn links_shared_items_but_never_credentials_or_local_overrides() {
    let source = temp("src");
    let target = temp("dst");
    for file in [
        "settings.json",
        "CLAUDE.md",
        ".credentials.json",
        "history.jsonl",
    ] {
        fs::write(source.join(file), file).unwrap();
    }
    for dir in ["skills", "projects", "sessions"] {
        fs::create_dir_all(source.join(dir)).unwrap();
    }
    fs::write(target.join("CLAUDE.md"), "local").unwrap();
    link_from(&source, &target, "claude").unwrap();
    link_from(&source, &target, "claude").unwrap();
    assert!(
        target
            .join("settings.json")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(target.join("skills").symlink_metadata().is_ok());
    assert_eq!(
        fs::read_to_string(target.join("CLAUDE.md")).unwrap(),
        "local"
    );
    for private in [".credentials.json", "history.jsonl", "projects", "sessions"] {
        assert!(
            target.join(private).symlink_metadata().is_err(),
            "{private}"
        );
    }
    let home = temp("home");
    let dir = home.join("accounts/id");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("CLAUDE.md"), "x").unwrap();
    assert_eq!(
        local_overrides(&account("anthropic", false), &home),
        vec!["CLAUDE.md"]
    );
    for path in [source, target, home] {
        let _ = fs::remove_dir_all(path);
    }
}

#[test]
fn environment_and_arguments_follow_the_vendor_and_native_accounts() {
    let home = Path::new("/ags");
    assert_eq!(
        env(&account("anthropic", false), home).unwrap(),
        vec![(
            "CLAUDE_CONFIG_DIR".to_owned(),
            "/ags/accounts/id".to_owned()
        )]
    );
    assert_eq!(
        env(&account("openai", false), home).unwrap(),
        vec![("CODEX_HOME".to_owned(), "/ags/accounts/id".to_owned())]
    );
    assert!(env(&account("openai", true), home).unwrap().is_empty());
    assert!(extra_args(&account("openai", true)).is_empty());
    assert!(extra_args(&account("anthropic", false)).is_empty());
    let mut args: Vec<OsString> = vec!["exec".into(), "--".into(), "x".into()];
    splice(&mut args, extra_args(&account("openai", false)));
    assert_eq!(args[1], "--config");
    assert_eq!(args[3], "--");
}

#[test]
fn signin_state_comes_from_the_account_directory() {
    let home = temp("signin");
    let claude = account("anthropic", false);
    let chatgpt = account("openai", false);
    let dir = home.join("accounts/id");
    fs::create_dir_all(&dir).unwrap();
    assert_eq!(signin(&account("anthropic", true), &home), SignIn::Native);
    assert_eq!(signin(&chatgpt, &home), SignIn::No);
    // Claude may use the macOS keychain, where the file proves nothing either way.
    let missing = if cfg!(target_os = "macos") {
        SignIn::Unknown
    } else {
        SignIn::No
    };
    assert_eq!(signin(&claude, &home), missing);
    // The other vendor's credential file does not count.
    fs::write(dir.join("auth.json"), "{}").unwrap();
    assert_eq!(signin(&chatgpt, &home), SignIn::Yes);
    assert_eq!(signin(&claude, &home), missing);
    fs::write(dir.join(".credentials.json"), "{}").unwrap();
    assert_eq!(signin(&claude, &home), SignIn::Yes);
    assert_eq!(SignIn::No.label(), "not signed in");
    let _ = fs::remove_dir_all(home);
}
