use super::*;
use std::path::PathBuf;

fn profile(name: &str) -> Profile {
    Profile::new(name)
}

#[test]
fn store_round_trips_and_missing_file_is_empty() {
    let home = std::env::temp_dir().join(format!("ags-profiles-{}", uuid::Uuid::new_v4()));
    assert!(ProfileStore::load(&home).unwrap().profiles.is_empty());
    ProfileStore::update(&home, |s| {
        let p = profile("work");
        s.default = Some(p.id.clone());
        s.add(p)
    })
    .unwrap();
    let store = ProfileStore::load(&home).unwrap();
    assert_eq!(store.default_profile().unwrap().name, "work");
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn profile_names_are_unique() {
    let mut store = ProfileStore::default();
    store.add(profile("work")).unwrap();
    assert!(store.add(profile("work")).is_err());
    assert!(store.add(profile("bad name")).is_err());
    assert!(store.find("missing").is_err());
}

#[test]
fn rules_match_by_path_component_and_the_longest_wins() {
    let mut store = ProfileStore::default();
    for (name, path) in [("a", "/a/b"), ("b", "/a/b/c"), ("c", "/")] {
        let p = profile(name);
        store.set_rule(path, &p.id);
        store.add(p).unwrap();
    }
    let name = |store: &ProfileStore, cwd: &str| {
        let rule = store.rule_for(&PathBuf::from(cwd))?;
        Some(store.get(&rule.profile)?.name.clone())
    };
    assert_eq!(name(&store, "/a/b"), Some("a".into()));
    assert_eq!(name(&store, "/a/b/x/y"), Some("a".into()));
    assert_eq!(name(&store, "/a/b/c/d"), Some("b".into()));
    // A string prefix is not a path prefix: /a/bc is not under /a/b.
    assert_eq!(name(&store, "/a/bc"), Some("c".into()));
    store.clear_rule("/");
    assert_eq!(name(&store, "/a/bc"), None);
    assert_eq!(name(&store, "/elsewhere"), None);
}

#[test]
fn setting_a_rule_twice_replaces_it() {
    let mut store = ProfileStore::default();
    store.set_rule("/x", "one");
    store.set_rule("/x", "two");
    assert_eq!(store.rules.len(), 1);
    assert_eq!(store.rules[0].profile, "two");
}

#[test]
fn removing_a_profile_drops_its_rules_and_default() {
    let mut store = ProfileStore::default();
    let p = profile("work");
    let id = p.id.clone();
    store.default = Some(id.clone());
    store.set_rule("/x", &id);
    store.add(p).unwrap();
    store.remove(&id).unwrap();
    assert!(store.default.is_none() && store.rules.is_empty());
}

#[test]
fn an_account_can_be_shared_and_detach_removes_it_everywhere() {
    let mut store = ProfileStore::default();
    for name in ["personal", "work"] {
        let mut p = profile(name);
        p.accounts.insert("openai".into(), "shared".into());
        p.accounts
            .insert("anthropic".into(), format!("{name}-claude"));
        store.add(p).unwrap();
    }
    let users: Vec<_> = store
        .using("shared")
        .iter()
        .map(|p| p.name.clone())
        .collect();
    assert_eq!(users, ["personal", "work"]);
    assert_eq!(store.using("work-claude").len(), 1);
    assert_eq!(store.detach("shared"), ["personal", "work"]);
    assert!(store.using("shared").is_empty());
    assert_eq!(store.using("work-claude").len(), 1);
}
