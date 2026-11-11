use super::*;
use crate::ags::{
    profile_store::Profile,
    tui::data::{Data, sample},
};
use crossterm::event::{KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn press(app: &mut App, code: KeyCode) -> Option<Effect> {
    app.key(key(code))
}

fn ch(app: &mut App, c: char) -> Option<Effect> {
    press(app, KeyCode::Char(c))
}

fn typed(app: &mut App, text: &str) {
    for c in text.chars() {
        assert_eq!(ch(app, c), None);
    }
}

fn routes() -> Vec<Route> {
    ["claude", "codex"]
        .iter()
        .map(|id| Route {
            id: (*id).into(),
            kind: format!("{id}_subscription"),
            enabled: true,
            default: true,
        })
        .collect()
}

/// The sample data on `tab`, connected, with two routes.
fn app(tab: Tab) -> App {
    let mut app = App::new(sample(), tab, "https://aegis.example.com".into());
    app.set_link(Link::Connected, routes());
    app
}

fn unpaired() -> App {
    App::new(
        Data::default(),
        Tab::Aegis,
        "https://aegis.example.com".into(),
    )
}

fn hint_keys(app: &App) -> Vec<&'static str> {
    app.hints().into_iter().map(|h| h.0).collect()
}

fn error(app: &App) -> String {
    let message = app.message.clone().expect("a message");
    assert!(!message.ok, "{}", message.text);
    message.text
}

#[test]
fn tabs_switch_with_tab_numbers_and_arrows_and_wrap() {
    let mut app = app(Tab::Aegis);
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.tab, Tab::Accounts);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.tab, Tab::Profiles);
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.tab, Tab::Aegis);
    press(&mut app, KeyCode::BackTab);
    assert_eq!(app.tab, Tab::Profiles);
    press(&mut app, KeyCode::Left);
    assert_eq!(app.tab, Tab::Accounts);
    ch(&mut app, '1');
    assert_eq!(app.tab, Tab::Aegis);
    ch(&mut app, '3');
    assert_eq!(app.tab, Tab::Profiles);
}

#[test]
fn focus_moves_within_a_tab_and_stops_at_the_ends() {
    let mut app = app(Tab::Accounts);
    assert_eq!(app.focused(), Some(Item::Account("pc".into())));
    ch(&mut app, 'k');
    assert_eq!(app.focused(), Some(Item::Account("pc".into())));
    press(&mut app, KeyCode::Down);
    ch(&mut app, 'j');
    assert_eq!(app.focused(), Some(Item::Account("pg".into())));
    for _ in 0..5 {
        press(&mut app, KeyCode::Down);
    }
    assert_eq!(app.focused(), Some(Item::AddAccount));
    ch(&mut app, 'g');
    assert_eq!(app.focused(), Some(Item::Account("pc".into())));
    // Each tab keeps its own focus.
    ch(&mut app, 'G');
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.focused(), Some(Item::Profile("p".into())));
    press(&mut app, KeyCode::BackTab);
    assert_eq!(app.focused(), Some(Item::AddAccount));
}

#[test]
fn elements_follow_the_data() {
    let app = app(Tab::Aegis);
    assert_eq!(
        app.items(Tab::Aegis),
        [
            Item::Server,
            Item::SignedIn,
            Item::Route("claude".into()),
            Item::Route("codex".into()),
            Item::Disconnect
        ]
    );
    assert_eq!(
        app.items(Tab::Profiles),
        [
            Item::Profile("p".into()),
            Item::Product("p".into(), "anthropic"),
            Item::Product("p".into(), "openai"),
            Item::Profile("w".into()),
            Item::Product("w".into(), "anthropic"),
            Item::Product("w".into(), "openai"),
            Item::NewProfile,
        ]
    );
    assert_eq!(unpaired().items(Tab::Aegis), [Item::Server]);
    assert_eq!(unpaired().items(Tab::Accounts), [Item::AddAccount]);
}

#[test]
fn q_and_esc_quit_and_ctrl_c_quits_anywhere() {
    assert_eq!(ch(&mut app(Tab::Aegis), 'q'), Some(Effect::Quit));
    assert_eq!(
        press(&mut app(Tab::Profiles), KeyCode::Esc),
        Some(Effect::Quit)
    );
    let mut editing = unpaired();
    assert!(matches!(editing.mode, Mode::Edit(_)));
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(editing.key(ctrl_c), Some(Effect::Quit));
}

#[test]
fn an_unpaired_machine_starts_editing_the_server_and_enter_pairs() {
    let mut app = unpaired();
    assert_eq!(app.link, Link::Unpaired);
    assert_eq!(hint_keys(&app), ["enter", "esc"]);
    // Typing q edits the field instead of quitting.
    typed(&mut app, "/q");
    assert_eq!(
        app.edit_problem().as_deref().map(|p| p.contains("HTTPS")),
        Some(true)
    );
    assert_eq!(press(&mut app, KeyCode::Enter), None);
    assert!(error(&app).contains("HTTPS"));
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Backspace);
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        Some(Effect::Pair {
            server: "https://aegis.example.com".into(),
            again: false
        })
    );
    let busy = app.busy.clone().expect("busy");
    assert!(busy.pairing);
    // Keys wait while pairing; the code arrives; Esc cancels.
    assert_eq!(press(&mut app, KeyCode::Tab), None);
    assert_eq!(app.tab, Tab::Aegis);
    app.device("https://a/device".into(), "ABCD".into());
    assert_eq!(
        app.busy.as_ref().unwrap().device,
        Some(("https://a/device".into(), "ABCD".into()))
    );
    assert_eq!(press(&mut app, KeyCode::Esc), Some(Effect::Cancel));
    assert!(app.busy.is_none());
}

#[test]
fn esc_leaves_the_server_field_and_enter_still_pairs_with_the_hint() {
    let mut app = unpaired();
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(hint_keys(&app), ["↑↓", "enter", "e"]);
    assert!(matches!(
        press(&mut app, KeyCode::Enter),
        Some(Effect::Pair { again: false, .. })
    ));
}

#[test]
fn changing_the_server_asks_before_disconnecting() {
    let mut app = app(Tab::Aegis);
    press(&mut app, KeyCode::Enter);
    let Mode::Edit(edit) = &app.mode else {
        panic!("editing");
    };
    assert_eq!(edit.field.text, "https://aegis.example.com");
    // The same server changes nothing.
    assert_eq!(press(&mut app, KeyCode::Enter), None);
    assert_eq!(app.mode, Mode::Normal);
    press(&mut app, KeyCode::Enter);
    app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    typed(&mut app, "https://other.example.com");
    assert_eq!(press(&mut app, KeyCode::Enter), None);
    let Mode::Confirm(confirm) = &app.mode else {
        panic!("confirming");
    };
    assert!(
        confirm
            .prompt
            .contains("Disconnect from https://aegis.example.com")
    );
    assert_eq!(hint_keys(&app), ["y", "n"]);
    // Other keys wait for the answer.
    assert_eq!(press(&mut app, KeyCode::Tab), None);
    assert_eq!(
        ch(&mut app, 'y'),
        Some(Effect::SwitchServer {
            server: "https://other.example.com".into()
        })
    );
}

#[test]
fn signing_in_again_and_disconnecting() {
    let mut app = app(Tab::Aegis);
    press(&mut app, KeyCode::Down);
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        Some(Effect::Pair {
            server: "https://aegis.example.com".into(),
            again: true
        })
    );
    app.finish(Ok(Outcome::say("Signed in again.")));
    assert!(app.message.as_ref().unwrap().ok);
    ch(&mut app, 'G');
    assert_eq!(app.focused(), Some(Item::Disconnect));
    press(&mut app, KeyCode::Enter);
    assert_eq!(ch(&mut app, 'n'), None);
    assert_eq!(app.mode, Mode::Normal);
    press(&mut app, KeyCode::Enter);
    assert_eq!(ch(&mut app, 'y'), Some(Effect::Disconnect));
}

#[test]
fn routes_toggle_while_connected_and_one_stays_enabled() {
    let mut app = app(Tab::Aegis);
    app.focus(&Item::Route("claude".into()));
    assert_eq!(hint_keys(&app), ["↑↓", "space"]);
    assert_eq!(
        ch(&mut app, ' '),
        Some(Effect::SetRoutes(vec!["codex".into()]))
    );
    app.finish(Ok(Outcome::say("Routes saved.")));
    let mut only = routes();
    only[1].enabled = false;
    app.set_link(Link::Connected, only);
    assert_eq!(app.focused(), Some(Item::Route("claude".into())));
    assert_eq!(press(&mut app, KeyCode::Enter), None);
    assert!(error(&app).contains("at least one"));
    app.set_link(Link::Expired, routes());
    assert_eq!(ch(&mut app, ' '), None);
    assert!(error(&app).contains("while connected"));
}

#[test]
fn account_actions_depend_on_the_account() {
    let mut app = app(Tab::Accounts);
    // Native: no sign-in or sign-out here.
    assert_eq!(hint_keys(&app), ["↑↓", "native:", "r", "d", "a"]);
    assert_eq!(press(&mut app, KeyCode::Enter), None);
    assert!(error(&app).contains("sign in with claude itself"));
    assert_eq!(ch(&mut app, 'o'), None);
    // Not signed in: sign in, no sign-out.
    press(&mut app, KeyCode::Down);
    assert_eq!(hint_keys(&app), ["↑↓", "enter", "r", "d", "a"]);
    assert_eq!(ch(&mut app, 'o'), None);
    assert_eq!(
        ch(&mut app, 'i'),
        Some(Effect::VendorAuth {
            account: "wc".into(),
            logout: false
        })
    );
    // Signed in: sign in again and sign out.
    press(&mut app, KeyCode::Down);
    assert_eq!(hint_keys(&app), ["↑↓", "enter", "o", "r", "d", "a"]);
    assert_eq!(
        app.hints()[1].1,
        "sign in again",
        "the label says it signs in again"
    );
    assert_eq!(
        ch(&mut app, 'o'),
        Some(Effect::VendorAuth {
            account: "pg".into(),
            logout: true
        })
    );
    assert!(can_sign_out(false, SignIn::Unknown));
    assert!(!can_sign_out(true, SignIn::Native));
    assert!(!can_sign_out(false, SignIn::No));
}

#[test]
fn renaming_an_account_in_place() {
    let mut app = app(Tab::Accounts);
    press(&mut app, KeyCode::Down);
    ch(&mut app, 'r');
    assert_eq!(hint_keys(&app), ["enter", "esc"]);
    // Tab and q are text while editing.
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.tab, Tab::Accounts);
    app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    typed(&mut app, "personal-claude");
    assert_eq!(
        app.edit_problem().as_deref(),
        Some("account personal-claude already exists")
    );
    assert_eq!(press(&mut app, KeyCode::Enter), None);
    assert!(matches!(app.mode, Mode::Edit(_)));
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.mode, Mode::Normal);
    ch(&mut app, 'r');
    // Unchanged saves nothing.
    assert_eq!(press(&mut app, KeyCode::Enter), None);
    assert_eq!(app.mode, Mode::Normal);
    ch(&mut app, 'r');
    typed(&mut app, "2");
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        Some(Effect::RenameAccount {
            account: "wc".into(),
            name: "work-claude2".into()
        })
    );
    assert_eq!(app.busy.as_ref().unwrap().label, "Renaming work-claude…");
}

#[test]
fn removing_an_account_names_what_it_detaches_from() {
    let mut app = app(Tab::Accounts);
    press(&mut app, KeyCode::Down);
    ch(&mut app, 'd');
    let Mode::Confirm(confirm) = &app.mode else {
        panic!("confirming");
    };
    assert_eq!(
        confirm.prompt,
        "Remove work-claude, detach it from work, and delete its sign-in?"
    );
    assert_eq!(press(&mut app, KeyCode::Esc), None);
    assert_eq!(app.mode, Mode::Normal);
    app.focus(&Item::Account("pc".into()));
    press(&mut app, KeyCode::Delete);
    let Mode::Confirm(confirm) = &app.mode else {
        panic!("confirming");
    };
    assert_eq!(
        confirm.prompt,
        "Remove personal-claude and detach it from personal?"
    );
    assert_eq!(
        ch(&mut app, 'y'),
        Some(Effect::RemoveAccount {
            account: "pc".into()
        })
    );
}

fn form(app: &App) -> AccountForm {
    match &app.mode {
        Mode::Form(form) => form.clone(),
        other => panic!("not a form: {other:?}"),
    }
}

#[test]
fn the_add_form_offers_what_applies_and_suggests_a_name() {
    let mut app = app(Tab::Accounts);
    ch(&mut app, 'a');
    assert_eq!(app.focused(), Some(Item::AddAccount));
    // Claude already has its native account: no sign-in choice, one route.
    let f = form(&app);
    assert_eq!(f.vendor(), "anthropic");
    assert_eq!(f.rows(&app.data), [FormRow::Product, FormRow::Name]);
    assert_eq!(f.name.text, "claude");
    press(&mut app, KeyCode::Right);
    let f = form(&app);
    assert_eq!(f.vendor(), "openai");
    assert_eq!(
        f.rows(&app.data),
        [FormRow::Product, FormRow::SignIn, FormRow::Name]
    );
    assert_eq!(f.name.text, "chatgpt");
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Right);
    let f = form(&app);
    assert!(f.native);
    assert_eq!(f.name.text, "chatgpt");
    // Typing keeps the name from following the choices.
    press(&mut app, KeyCode::Down);
    typed(&mut app, "-x");
    press(&mut app, KeyCode::Up);
    press(&mut app, KeyCode::Left);
    let f = form(&app);
    assert!(!f.native);
    assert_eq!(f.name.text, "chatgpt-x");
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        Some(Effect::CreateAccount {
            vendor: "openai",
            name: "chatgpt-x".into(),
            native: false,
            route: "codex".into()
        })
    );
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn the_add_form_checks_the_name_and_routes() {
    let mut app = app(Tab::Accounts);
    ch(&mut app, 'a');
    press(&mut app, KeyCode::Down);
    for _ in 0..20 {
        press(&mut app, KeyCode::Backspace);
    }
    typed(&mut app, "work-claude");
    assert_eq!(
        app.form_problem().as_deref(),
        Some("account work-claude already exists")
    );
    assert_eq!(press(&mut app, KeyCode::Enter), None);
    assert!(error(&app).contains("already exists"));
    assert!(matches!(app.mode, Mode::Form(_)));
    // A second Claude route adds a route choice; none leaves nothing to bind.
    app.data
        .granted
        .push(("claude-2".into(), "claude_subscription".into()));
    assert!(form(&app).rows(&app.data).contains(&FormRow::Route));
    app.data.granted.clear();
    typed(&mut app, "-2");
    assert_eq!(press(&mut app, KeyCode::Enter), None);
    assert!(error(&app).contains("No Claude route"));
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.mode, Mode::Normal);
    // Unpaired machines cannot add accounts.
    let mut app = unpaired();
    press(&mut app, KeyCode::Esc);
    ch(&mut app, '2');
    assert_eq!(press(&mut app, KeyCode::Enter), None);
    assert!(error(&app).contains("Pair this machine"));
}

#[test]
fn a_new_account_offers_to_sign_in() {
    let mut app = app(Tab::Accounts);
    let then = Confirm {
        prompt: "Sign in to new now?".into(),
        effect: Effect::VendorAuth {
            account: "wc".into(),
            logout: false,
        },
    };
    app.finish(Ok(Outcome {
        message: "Added new.".into(),
        focus: Some(Item::Account("wc".into())),
        then: Some(then.clone()),
    }));
    assert_eq!(app.focused(), Some(Item::Account("wc".into())));
    assert_eq!(app.mode, Mode::Confirm(then.clone()));
    assert_eq!(ch(&mut app, 'y'), Some(then.effect));
    // A failure is shown and clears on the next key.
    app.finish(Err(anyhow::anyhow!("boom")));
    assert_eq!(error(&app), "boom");
    press(&mut app, KeyCode::Down);
    assert!(app.message.is_none());
}

#[test]
fn profile_rows_toggle_default_and_here_and_rename_and_remove() {
    let mut app = app(Tab::Profiles);
    assert_eq!(hint_keys(&app), ["↑↓", "d", "h", "r", "x", "n"]);
    assert_eq!(app.hints()[1].1, "unset default");
    assert_eq!(app.hints()[2].1, "use here");
    assert_eq!(
        ch(&mut app, 'd'),
        Some(Effect::ToggleDefault {
            profile: "p".into()
        })
    );
    app.finish(Ok(Outcome::default()));
    assert_eq!(
        ch(&mut app, 'h'),
        Some(Effect::ToggleHere {
            profile: "p".into()
        })
    );
    app.finish(Ok(Outcome::default()));
    app.focus(&Item::Profile("w".into()));
    assert_eq!(app.hints()[2].1, "stop using here");
    ch(&mut app, 'r');
    typed(&mut app, "s");
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        Some(Effect::RenameProfile {
            profile: "w".into(),
            name: "works".into()
        })
    );
    app.finish(Ok(Outcome::default()));
    ch(&mut app, 'x');
    assert_eq!(
        ch(&mut app, 'y'),
        Some(Effect::RemoveProfile {
            profile: "w".into()
        })
    );
}

#[test]
fn product_rows_choose_an_account_from_a_dropdown() {
    let mut app = app(Tab::Profiles);
    app.focus(&Item::Product("w".into(), "anthropic"));
    assert_eq!(hint_keys(&app), ["↑↓", "enter", "x", "n"]);
    press(&mut app, KeyCode::Enter);
    let Mode::Dropdown(dropdown) = &app.mode else {
        panic!("dropdown");
    };
    let labels: Vec<_> = dropdown.options.iter().map(|o| o.1.as_str()).collect();
    assert_eq!(labels, ["personal-claude", "work-claude", "None"]);
    assert_eq!(dropdown.selected, 1, "the current account");
    // Choosing the current one changes nothing; Esc closes.
    assert_eq!(press(&mut app, KeyCode::Enter), None);
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.mode, Mode::Normal);
    press(&mut app, KeyCode::Enter);
    ch(&mut app, 'k');
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        Some(Effect::SetAccount {
            profile: "w".into(),
            vendor: "anthropic",
            account: Some("pc".into())
        })
    );
    app.finish(Ok(Outcome::default()));
    press(&mut app, KeyCode::Enter);
    ch(&mut app, 'G');
    press(&mut app, KeyCode::Down);
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        Some(Effect::SetAccount {
            profile: "w".into(),
            vendor: "anthropic",
            account: None
        })
    );
    app.finish(Ok(Outcome::default()));
    // An empty product preselects the first account; x clears only a set product.
    app.focus(&Item::Product("w".into(), "openai"));
    assert_eq!(hint_keys(&app), ["↑↓", "enter", "n"]);
    assert_eq!(ch(&mut app, 'x'), None);
    press(&mut app, KeyCode::Enter);
    assert!(matches!(&app.mode, Mode::Dropdown(d) if d.selected == 0));
    press(&mut app, KeyCode::Esc);
    app.data.accounts.accounts.retain(|a| a.vendor != "openai");
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.mode, Mode::Normal);
    assert!(error(&app).contains("No ChatGPT accounts"));
}

#[test]
fn a_new_profile_is_named_in_place_then_its_products_get_the_focus() {
    let mut app = app(Tab::Profiles);
    ch(&mut app, 'n');
    assert_eq!(app.focused(), Some(Item::NewProfile));
    typed(&mut app, "work");
    assert_eq!(press(&mut app, KeyCode::Enter), None);
    assert!(error(&app).contains("already exists"));
    typed(&mut app, "2");
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        Some(Effect::CreateProfile {
            name: "work2".into()
        })
    );
    let mut data = app.data.clone();
    data.profiles.profiles.push(Profile::new("work2"));
    let id = data.profiles.profiles[2].id.clone();
    app.set_data(data);
    app.finish(Ok(Outcome {
        message: "Created work2.".into(),
        focus: Some(Item::Product(id.clone(), "anthropic")),
        then: None,
    }));
    assert_eq!(app.focused(), Some(Item::Product(id, "anthropic")));
}

#[test]
fn reloading_keeps_the_focus_on_the_same_element() {
    let mut app = app(Tab::Accounts);
    app.focus(&Item::Account("pg".into()));
    let mut data = app.data.clone();
    data.accounts.accounts.remove(0);
    app.set_data(data.clone());
    assert_eq!(app.focused(), Some(Item::Account("pg".into())));
    // A removed element leaves the focus nearby.
    data.accounts.accounts.retain(|a| a.id != "pg");
    app.set_data(data.clone());
    assert_eq!(app.focused(), Some(Item::AddAccount));
    // Disconnecting forgets the routes.
    data.server = None;
    app.set_data(data);
    assert_eq!(app.link, Link::Unpaired);
    assert!(app.routes.is_empty());
}

#[test]
fn help_opens_and_closes() {
    let mut app = app(Tab::Accounts);
    ch(&mut app, '?');
    assert!(app.help);
    assert_eq!(press(&mut app, KeyCode::Down), None);
    assert_eq!(app.focused(), Some(Item::Account("pc".into())));
    press(&mut app, KeyCode::Esc);
    assert!(!app.help);
    ch(&mut app, '?');
    assert_eq!(ch(&mut app, 'q'), Some(Effect::Quit));
}

#[test]
fn background_work_blocks_keys_until_it_finishes() {
    let mut app = app(Tab::Profiles);
    ch(&mut app, 'd');
    assert!(app.busy.is_some());
    assert_eq!(hint_keys(&app), Vec::<&str>::new());
    assert_eq!(ch(&mut app, 'd'), None);
    assert_eq!(
        press(&mut app, KeyCode::Esc),
        None,
        "only pairing can be cancelled"
    );
    app.finish(Ok(Outcome::say("Done.")));
    assert!(app.busy.is_none());
    assert_eq!(app.message.as_ref().unwrap().text, "Done.");
}

#[test]
fn text_fields_edit_at_the_cursor() {
    let mut field = Field::new("acd");
    field.key(key(KeyCode::Left));
    field.key(key(KeyCode::Left));
    field.key(key(KeyCode::Char('b')));
    assert_eq!((field.text.as_str(), field.cursor), ("abcd", 2));
    field.key(key(KeyCode::Delete));
    field.key(key(KeyCode::Backspace));
    assert_eq!((field.text.as_str(), field.cursor), ("ad", 1));
    field.key(key(KeyCode::End));
    field.key(key(KeyCode::Char('é')));
    field.key(key(KeyCode::Home));
    field.key(key(KeyCode::Backspace));
    assert_eq!((field.text.as_str(), field.cursor), ("adé", 0));
    field.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    field.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert_eq!(field, Field::default());
    assert!(!field.key(key(KeyCode::Enter)));
}
