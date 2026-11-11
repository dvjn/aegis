use super::*;
use crate::ags::account_store::Account;

fn fixture() -> (AccountStore, ProfileStore) {
    let mut accounts = AccountStore::default();
    let mut profiles = ProfileStore::default();
    for name in ["work-claude", "home-claude"] {
        accounts.accounts.push(Account {
            id: name.into(),
            name: name.into(),
            vendor: "anthropic".into(),
            provider: "route".into(),
            native: false,
            created_at: 0,
            registered_with: None,
        });
    }
    for (name, account) in [
        ("work", Some("work-claude")),
        ("home", Some("home-claude")),
        ("bare", None),
    ] {
        let mut p = Profile::new(name);
        if let Some(a) = account {
            p.accounts.insert("anthropic".into(), a.into());
        }
        profiles.add(p).unwrap();
    }
    (accounts, profiles)
}

fn id(profiles: &ProfileStore, name: &str) -> String {
    profiles.find(name).unwrap().id.clone()
}

fn pick(
    profiles: &ProfileStore,
    flag: Option<&str>,
    env: Option<&str>,
    cwd: &str,
) -> Option<(String, Source)> {
    resolve_profile(profiles, flag, env, Path::new(cwd))
        .unwrap()
        .map(|(p, s)| (p.name.clone(), s))
}

fn run<'a>(
    accounts: &'a AccountStore,
    profiles: &'a ProfileStore,
    vendor: &str,
    flag: Option<&str>,
    tty: bool,
) -> Result<Launch<'a>> {
    launch(
        profiles,
        accounts,
        vendor,
        false,
        flag,
        None,
        Path::new("/x"),
        tty,
    )
}

#[test]
fn nothing_configured_is_unprofiled() {
    let (accounts, profiles) = fixture();
    assert!(pick(&profiles, None, None, "/x").is_none());
    assert!(matches!(
        run(&accounts, &profiles, "anthropic", None, false).unwrap(),
        Launch::Unprofiled
    ));
}

#[test]
fn order_is_flag_then_env_then_longest_rule_then_default() {
    let (_, mut profiles) = fixture();
    profiles.default = Some(id(&profiles, "home"));
    let (work, bare) = (id(&profiles, "work"), id(&profiles, "bare"));
    profiles.set_rule("/w", &bare);
    profiles.set_rule("/w/proj", &work);
    assert_eq!(
        pick(&profiles, None, None, "/elsewhere").unwrap(),
        ("home".into(), Source::Default)
    );
    assert_eq!(
        pick(&profiles, None, None, "/w/x").unwrap(),
        ("bare".into(), Source::Rule("/w".into()))
    );
    assert_eq!(
        pick(&profiles, None, None, "/w/proj/x").unwrap(),
        ("work".into(), Source::Rule("/w/proj".into()))
    );
    assert_eq!(
        pick(&profiles, None, Some("home"), "/w/proj").unwrap(),
        ("home".into(), Source::Env)
    );
    assert_eq!(
        pick(&profiles, Some("bare"), Some("home"), "/w/proj").unwrap(),
        ("bare".into(), Source::Flag)
    );
    // An empty variable is unset.
    assert_eq!(
        pick(&profiles, None, Some(""), "/w/proj").unwrap().1,
        Source::Rule("/w/proj".into())
    );
}

#[test]
fn rules_match_whole_path_components() {
    let (_, mut profiles) = fixture();
    let work = id(&profiles, "work");
    profiles.set_rule("/a/b", &work);
    assert!(pick(&profiles, None, None, "/a/b/c").is_some());
    assert!(pick(&profiles, None, None, "/a/bc").is_none());
}

#[test]
fn unknown_flag_or_env_profile_names_the_fix() {
    let (_, profiles) = fixture();
    for (flag, env) in [(Some("nope"), None), (None, Some("nope"))] {
        let error = resolve_profile(&profiles, flag, env, Path::new("/x"))
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("ags profile new nope"), "{error}");
    }
}

#[test]
fn provider_forces_an_unprofiled_launch() {
    let (accounts, mut profiles) = fixture();
    profiles.default = Some(id(&profiles, "work"));
    let launched = launch(
        &profiles,
        &accounts,
        "anthropic",
        true,
        None,
        None,
        Path::new("/x"),
        false,
    )
    .unwrap();
    assert!(matches!(launched, Launch::Unprofiled));
}

#[test]
fn a_ready_launch_returns_the_profile_account_and_source() {
    let (accounts, mut profiles) = fixture();
    profiles.default = Some(id(&profiles, "work"));
    let Launch::Ready {
        profile,
        account,
        source,
    } = run(&accounts, &profiles, "anthropic", None, false).unwrap()
    else {
        panic!("ready expected");
    };
    assert_eq!(profile.name, "work");
    assert_eq!(account.name, "work-claude");
    assert_eq!(source, Source::Default);
}

#[test]
fn a_missing_vendor_account_fails_without_a_terminal_with_the_exact_command() {
    let (accounts, profiles) = fixture();
    let error = run(&accounts, &profiles, "openai", Some("work"), false)
        .err()
        .unwrap()
        .to_string();
    assert_eq!(
        error,
        "Profile work has no chatgpt account. Add one with `ags account new <name> --chatgpt`, then `ags profile set work chatgpt <name>`."
    );
    assert_eq!(error, missing_account_message("work", "openai"));
}

#[test]
fn a_terminal_is_asked_only_when_the_product_has_accounts_to_offer() {
    let (mut accounts, profiles) = fixture();
    // The profile has no claude account, but claude accounts exist: ask.
    assert!(matches!(
        run(&accounts, &profiles, "anthropic", Some("bare"), true).unwrap(),
        Launch::NeedsAccount { .. }
    ));
    // No chatgpt accounts at all: same error as without a terminal.
    assert!(run(&accounts, &profiles, "openai", Some("work"), true).is_err());
    accounts.accounts.clear();
    assert!(run(&accounts, &profiles, "anthropic", Some("bare"), true).is_err());
}

#[test]
fn a_dangling_account_reference_counts_as_missing() {
    let (accounts, mut profiles) = fixture();
    let work = id(&profiles, "work");
    profiles
        .set_account(&work, "anthropic", Some("gone"))
        .unwrap();
    assert!(run(&accounts, &profiles, "anthropic", Some("work"), false).is_err());
}

#[test]
fn paths_display_with_a_tilde_for_home() {
    let home = Path::new("/home/me");
    assert_eq!(
        display_path(Path::new("/home/me/work/x"), Some(home)),
        "~/work/x"
    );
    assert_eq!(display_path(Path::new("/home/me"), Some(home)), "~");
    assert_eq!(
        display_path(Path::new("/home/men/x"), Some(home)),
        "/home/men/x"
    );
    assert_eq!(display_path(Path::new("/srv/x"), None), "/srv/x");
    assert_eq!(
        Source::Rule("/home/me/w".into()).describe(Some(home)),
        "rule ~/w"
    );
    assert_eq!(Source::Flag.describe(None), "--profile");
    assert_eq!(Source::Env.describe(None), "AGS_PROFILE");
    assert_eq!(Source::Default.describe(None), "default");
}

#[test]
fn here_uses_the_git_root_or_the_directory_itself() {
    let base = std::env::temp_dir().join(format!("ags-here-{}", uuid::Uuid::new_v4()));
    let nested = base.join("repo/src/deep");
    std::fs::create_dir_all(&nested).unwrap();
    let base = base.canonicalize().unwrap();
    let nested = nested.canonicalize().unwrap();
    assert_eq!(here_root(&nested), nested);
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(base.join("repo"))
        .status();
    if init.is_ok_and(|s| s.success()) {
        assert_eq!(here_root(&nested), base.join("repo"));
    }
    let _ = std::fs::remove_dir_all(base);
}
