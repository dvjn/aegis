use super::*;
use crate::ags::account_store::tests::account;

#[test]
fn profiles_list_as_a_tree_without_empty_products() {
    let mut accounts = AccountStore::default();
    let claude = account("personal-claude", "anthropic", true);
    let chatgpt = account("work-chatgpt", "openai", false);
    let mut personal = Profile::new("personal");
    personal
        .accounts
        .insert("anthropic".into(), claude.id.clone());
    personal
        .accounts
        .insert("openai".into(), chatgpt.id.clone());
    let mut work = Profile::new("work");
    work.accounts.insert("openai".into(), chatgpt.id.clone());
    accounts.add(claude).unwrap();
    accounts.add(chatgpt).unwrap();
    let out = tree(
        &[personal.clone(), work],
        &accounts,
        |p| {
            if p.id == personal.id {
                vec!["default".into()]
            } else {
                vec![]
            }
        },
        ui::Out::Stdout,
    );
    assert_eq!(
        out,
        "personal  (default)
├─ claude   personal-claude
└─ chatgpt  work-chatgpt
work
└─ chatgpt  work-chatgpt
"
    );
}

fn store_with(accounts: &[(&str, &str)]) -> AccountStore {
    let mut store = AccountStore::default();
    for (name, vendor) in accounts {
        store.add(account(name, vendor, false)).unwrap();
    }
    store
}

#[test]
fn values_name_an_existing_account_of_the_product_or_none() {
    let store = store_with(&[("work-claude", "anthropic"), ("work-chatgpt", "openai")]);
    assert!(
        account_for_value(&store, "p", "anthropic", "none")
            .unwrap()
            .is_none()
    );
    let found = account_for_value(&store, "p", "anthropic", "work-claude")
        .unwrap()
        .unwrap();
    assert_eq!(found.name, "work-claude");
    let wrong = account_for_value(&store, "p", "anthropic", "work-chatgpt")
        .unwrap_err()
        .to_string();
    assert!(
        wrong.contains("is a chatgpt account, not claude"),
        "{wrong}"
    );
    assert!(account_for_value(&store, "p", "anthropic", "missing").is_err());
}

#[test]
fn new_and_native_are_rejected_with_the_command_that_replaces_them() {
    let store = store_with(&[]);
    let error = account_for_value(&store, "work", "openai", "new")
        .unwrap_err()
        .to_string();
    assert!(error.contains("no longer accepted"), "{error}");
    assert!(
        error.contains("`ags account new <name> --chatgpt`"),
        "{error}"
    );
    assert!(
        error.contains("`ags profile set work chatgpt <name>`"),
        "{error}"
    );
    let error = account_for_value(&store, "work", "anthropic", "native")
        .unwrap_err()
        .to_string();
    assert!(error.contains("--claude --native"), "{error}");
}

#[test]
fn a_product_without_accounts_gets_a_one_line_hint() {
    assert_eq!(
        no_accounts_hint("claude"),
        "No claude accounts yet; add one with `ags account new <name> --claude`."
    );
}

#[test]
fn the_current_account_is_preselected_else_none() {
    let a = account("a", "anthropic", false);
    let b = account("b", "anthropic", false);
    let list = [&a, &b];
    assert_eq!(initial_choice(&list, Some(&b.id)), b.id);
    assert_eq!(initial_choice(&list, Some("gone")), "skip");
    assert_eq!(initial_choice(&list, None), "skip");
}

#[test]
fn where_options_offer_everywhere_here_and_only_with_the_flag() {
    let items = where_items("~/x");
    assert_eq!(
        items.iter().map(|i| i.0).collect::<Vec<_>>(),
        [Where::Default, Where::Here, Where::Neither]
    );
    assert_eq!(items[1].1, "This directory (~/x)");
}
