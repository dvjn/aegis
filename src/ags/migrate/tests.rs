use super::*;
use serde_json::json;

fn old_json() -> serde_json::Value {
    json!({
        "profiles": [
            {"id":"id-personal","name":"personal","cli":"claude","provider":"claude-main","adopted":true,"created_at":1,"registered_with":"client"},
            {"id":"id-alt","name":"alt","cli":"claude","provider":"claude-alt","adopted":false,"created_at":2},
            {"id":"id-work","name":"work","cli":"codex","provider":"codex-work","adopted":false,"created_at":3,"registered_with":"client"},
        ],
        "defaults": {"claude":"id-personal","codex":"id-work"}
    })
}

fn names(accounts: &AccountStore, profile: &Profile) -> BTreeMap<String, String> {
    profile
        .accounts
        .iter()
        .map(|(v, id)| (v.clone(), accounts.get(id).unwrap().name.clone()))
        .collect()
}

fn expect(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
        .collect()
}

#[test]
fn old_profiles_become_accounts_with_the_same_ids_and_shared_profiles() {
    let (accounts, profiles) = convert(parse_old(old_json().to_string().as_bytes()).unwrap());
    let account = |n: &str| accounts.find(n).unwrap();
    assert_eq!(accounts.accounts.len(), 3);
    assert_eq!(account("personal-claude").id, "id-personal");
    assert!(account("personal-claude").native);
    assert_eq!(
        account("personal-claude").registered_with.as_deref(),
        Some("client")
    );
    assert!(!account("alt-claude").native);
    assert_eq!(account("work-chatgpt").vendor, "openai");
    assert_eq!(account("work-chatgpt").provider, "codex-work");

    let profile = |n: &str| profiles.find(n).unwrap();
    assert_eq!(
        names(&accounts, profile("personal")),
        expect(&[("anthropic", "personal-claude"), ("openai", "work-chatgpt")])
    );
    assert_eq!(
        names(&accounts, profile("alt")),
        expect(&[("anthropic", "alt-claude")])
    );
    assert_eq!(
        names(&accounts, profile("work")),
        expect(&[("openai", "work-chatgpt")])
    );
    assert_eq!(profiles.default_profile().unwrap().name, "personal");
    assert!(profiles.rules.is_empty());
}

#[test]
fn the_same_name_for_claude_and_codex_becomes_one_profile() {
    let old = json!({"profiles":[
        {"id":"c","name":"main","cli":"claude","provider":"p","adopted":false,"created_at":0},
        {"id":"x","name":"main","cli":"codex","provider":"q","adopted":false,"created_at":0}
    ],"defaults":{"codex":"x"}});
    let (accounts, profiles) = convert(parse_old(old.to_string().as_bytes()).unwrap());
    assert_eq!(profiles.profiles.len(), 1);
    assert_eq!(
        names(&accounts, &profiles.profiles[0]),
        expect(&[("anthropic", "main-claude"), ("openai", "main-chatgpt")])
    );
    assert_eq!(profiles.default_profile().unwrap().name, "main");
}

#[test]
fn only_the_old_format_is_recognized() {
    assert!(parse_old(old_json().to_string().as_bytes()).is_some());
    assert!(parse_old(br#"{"profiles":[],"defaults":{}}"#).is_some());
    assert!(
        parse_old(
            br#"{"profiles":[{"id":"1","name":"a","accounts":{}}],"rules":[],"default":null}"#
        )
        .is_none()
    );
    assert!(parse_old(b"{}").is_none());
}

#[test]
fn migration_moves_directories_keeps_a_backup_and_is_idempotent() {
    let home = std::env::temp_dir().join(format!("ags-migrate-{}", uuid::Uuid::new_v4()));
    state::private_dir(&home).unwrap();
    let old = old_json().to_string();
    state::write_private(&home.join("profiles.json"), old.as_bytes(), false).unwrap();
    for id in ["id-alt", "id-work"] {
        let dir = home.join("profiles").join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("marker"), id).unwrap();
    }
    assert!(run(&home).unwrap());
    for id in ["id-alt", "id-work"] {
        assert_eq!(
            fs::read_to_string(account_store::dir(&home, id).join("marker")).unwrap(),
            id
        );
        assert!(!home.join("profiles").join(id).exists());
    }
    assert_eq!(
        fs::read_to_string(home.join("profiles.v2.json.bak")).unwrap(),
        old
    );
    let accounts = AccountStore::load(&home).unwrap();
    let profiles = ProfileStore::load(&home).unwrap();
    assert_eq!(accounts.accounts.len(), 3);
    assert_eq!(profiles.default_profile().unwrap().name, "personal");
    assert!(!run(&home).unwrap());
    assert_eq!(AccountStore::load(&home).unwrap().accounts.len(), 3);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn nothing_to_migrate_without_a_profiles_file() {
    let home = std::env::temp_dir().join(format!("ags-migrate-none-{}", uuid::Uuid::new_v4()));
    assert!(!run(&home).unwrap());
    assert!(!home.exists());
}
