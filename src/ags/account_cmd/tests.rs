use super::*;
use crate::ags::{account_store::tests::account, profile_store::Profile};

fn stores() -> (AccountStore, ProfileStore, Account) {
    let mut accounts = AccountStore::default();
    let shared = account("me-chatgpt", "openai", false);
    accounts.add(shared.clone()).unwrap();
    let mut profiles = ProfileStore::default();
    for name in ["personal", "work"] {
        let mut p = Profile::new(name);
        p.accounts.insert("openai".into(), shared.id.clone());
        profiles.add(p).unwrap();
    }
    (accounts, profiles, shared)
}

#[test]
fn removal_is_refused_while_profiles_use_the_account() {
    let (_, mut profiles, shared) = stores();
    let error = detach_or_refuse(&mut profiles, &shared, false)
        .unwrap_err()
        .to_string();
    assert!(error.contains("used by profiles personal, work"), "{error}");
    assert!(
        error.contains("`ags account rm me-chatgpt --force`"),
        "{error}"
    );
    assert_eq!(
        profiles.using(&shared.id).len(),
        2,
        "refusal must not change anything"
    );
}

#[test]
fn force_detaches_the_account_from_every_profile() {
    let (_, mut profiles, shared) = stores();
    assert_eq!(
        detach_or_refuse(&mut profiles, &shared, true).unwrap(),
        ["personal", "work"]
    );
    assert!(profiles.using(&shared.id).is_empty());
    assert!(profiles.profiles.iter().all(|p| p.accounts.is_empty()));
}

#[test]
fn an_unused_account_can_be_removed_without_force() {
    let (_, mut profiles, _) = stores();
    let loose = account("loose", "anthropic", false);
    assert!(
        detach_or_refuse(&mut profiles, &loose, false)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn two_profiles_share_one_account() {
    let (accounts, profiles, shared) = stores();
    let ids: Vec<_> = profiles
        .profiles
        .iter()
        .map(|p| p.accounts["openai"].clone())
        .collect();
    assert_eq!(ids, [shared.id.clone(), shared.id.clone()]);
    assert_eq!(
        used_by(&profiles, accounts.find("me-chatgpt").unwrap()),
        "personal, work"
    );
    assert_eq!(used_by(&profiles, &account("x", "openai", false)), "-");
}

#[test]
fn new_accounts_get_a_free_default_name() {
    let mut store = AccountStore::default();
    assert_eq!(default_name(&store, "claude"), "claude");
    store.add(account("claude", "anthropic", false)).unwrap();
    assert_eq!(default_name(&store, "claude"), "claude-2");
    store.add(account("claude-2", "anthropic", false)).unwrap();
    assert_eq!(default_name(&store, "claude"), "claude-3");
}

#[test]
fn native_is_offered_only_until_one_exists_and_when_its_directory_does() {
    let mut store = AccountStore::default();
    assert!(offer_native(&store, "anthropic", true));
    assert!(!offer_native(&store, "anthropic", false));
    store.add(account("claude", "anthropic", true)).unwrap();
    assert!(!offer_native(&store, "anthropic", true));
    assert!(offer_native(&store, "openai", true));
}

#[test]
fn account_names_are_checked_for_syntax_and_collisions() {
    let (accounts, _, _) = stores();
    assert!(name_problem(&accounts, "work-claude").is_none());
    assert!(name_problem(&accounts, "has space").is_some());
    assert!(
        name_problem(&accounts, "me-chatgpt")
            .unwrap()
            .contains("already exists")
    );
}
