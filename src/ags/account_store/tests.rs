use super::*;

pub fn account(name: &str, vendor: &str, native: bool) -> Account {
    Account {
        id: uuid::Uuid::now_v7().to_string(),
        name: name.into(),
        vendor: vendor.into(),
        provider: "route".into(),
        native,
        created_at: 1,
        registered_with: None,
    }
}

#[test]
fn names_are_validated() {
    for ok in ["work", "a.b_c-1", &"x".repeat(64)] {
        validate_name("account", ok).unwrap();
    }
    for bad in ["", "has space", "a/b", "é", &"x".repeat(65)] {
        assert!(validate_name("account", bad).is_err(), "{bad}");
    }
}

#[test]
fn vendors_map_to_products_clis_and_route_kinds() {
    assert_eq!(vendor_for_cli("claude"), Some("anthropic"));
    assert_eq!(vendor_for_cli("codex"), Some("openai"));
    assert_eq!(vendor_for_cli("agy"), None);
    assert_eq!(product("openai"), "chatgpt");
    assert_eq!(vendor_for_product("chatgpt"), Some("openai"));
    assert_eq!(cli("openai"), "codex");
    assert_eq!(kind("anthropic"), Some("claude_subscription"));
    assert_eq!(kind("openai"), Some("codex_subscription"));
}

#[test]
fn store_round_trips_and_missing_file_is_empty() {
    let home = std::env::temp_dir().join(format!("ags-accounts-{}", uuid::Uuid::new_v4()));
    assert!(AccountStore::load(&home).unwrap().accounts.is_empty());
    AccountStore::update(&home, |s| s.add(account("work-claude", "anthropic", false))).unwrap();
    let store = AccountStore::load(&home).unwrap();
    assert_eq!(store.find("work-claude").unwrap().vendor, "anthropic");
    assert!(store.find("missing").is_err());
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn names_are_unique_and_collisions_get_numeric_suffixes() {
    let mut store = AccountStore::default();
    assert_eq!(store.unique_name("work-claude"), "work-claude");
    store
        .add(account("work-claude", "anthropic", false))
        .unwrap();
    assert_eq!(store.unique_name("work-claude"), "work-claude-2");
    store
        .add(account("work-claude-2", "anthropic", false))
        .unwrap();
    assert_eq!(store.unique_name("work-claude"), "work-claude-3");
    assert!(store.add(account("work-claude", "openai", false)).is_err());
}

#[test]
fn a_native_account_exists_at_most_once_per_vendor() {
    let mut store = AccountStore::default();
    store.add(account("claude", "anthropic", true)).unwrap();
    store.add(account("chatgpt", "openai", true)).unwrap();
    let error = store
        .add(account("other", "anthropic", true))
        .unwrap_err()
        .to_string();
    assert!(error.contains("already exists as claude"), "{error}");
    assert_eq!(store.native("openai").unwrap().name, "chatgpt");
}

#[test]
fn google_and_unknown_vendors_are_not_offered() {
    let mut store = AccountStore::default();
    assert!(store.add(account("g", "google", false)).is_err());
}

#[test]
fn rename_keeps_names_unique_and_forces_reregistration() {
    let mut store = AccountStore::default();
    let mut a = account("a", "anthropic", false);
    a.registered_with = Some("client".into());
    let id = a.id.clone();
    store.add(a).unwrap();
    store.add(account("b", "anthropic", false)).unwrap();
    assert!(store.rename(&id, "b").is_err());
    store.rename(&id, "c").unwrap();
    assert_eq!(store.get(&id).unwrap().name, "c");
    assert!(store.get(&id).unwrap().registered_with.is_none());
}
