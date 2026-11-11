use super::*;
use crate::ags::tui::{
    app::Route,
    data::{Data, sample},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};

fn press(app: &mut App, code: KeyCode) {
    app.key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn connected(tab: Tab) -> App {
    let mut app = App::new(sample(), tab, String::new());
    let route = |id: &str, enabled| Route {
        id: id.into(),
        kind: format!("{id}_subscription"),
        enabled,
        default: enabled,
    };
    app.set_link(
        Link::Connected,
        vec![route("claude", true), route("codex", false)],
    );
    app
}

fn unpaired(tab: Tab) -> App {
    App::new(Data::default(), tab, "https://aegis.example.com".into())
}

/// The screen as text, trailing spaces trimmed.
fn screen(app: &App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal.draw(|frame| render(frame, app)).unwrap();
    let buffer = terminal.backend().buffer();
    let width = usize::from(buffer.area.width);
    buffer
        .content
        .chunks(width)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn has(screen: &str, lines: &[&str]) {
    for line in lines {
        assert!(
            screen
                .lines()
                .any(|l| l.trim_start() == *line || l.contains(line)),
            "missing {line:?} in\n{screen}"
        );
    }
}

#[test]
fn the_header_shows_the_steps_and_the_connection() {
    let screen = screen(&connected(Tab::Aegis));
    let first = screen.lines().next().unwrap();
    assert!(first.starts_with(" ◆ aegis setup     Aegis   Accounts   Profiles"));
    assert!(first.ends_with("● connected · aegis.example.com"));
    let mut app = connected(Tab::Aegis);
    app.set_link(Link::Expired, vec![]);
    assert!(
        self::screen(&app)
            .lines()
            .next()
            .unwrap()
            .ends_with("● login expired · aegis.example.com")
    );
    assert!(
        self::screen(&unpaired(Tab::Aegis))
            .lines()
            .next()
            .unwrap()
            .ends_with("○ not paired")
    );
}

#[test]
fn the_aegis_step_when_connected() {
    let screen = screen(&connected(Tab::Aegis));
    has(
        &screen,
        &[
            " › Server      https://aegis.example.com   enter: change",
            "   Signed in   yes",
            "   Routes",
            "   [✓] claude   Claude · default",
            "   [ ] codex    Codex",
            "   Disconnect",
            " ↑↓ move  enter change server  ·  tab next step  ? help  q quit",
        ],
    );
}

#[test]
fn the_aegis_step_when_expired_or_unpaired_or_pairing() {
    let mut app = connected(Tab::Aegis);
    app.set_link(Link::Expired, vec![]);
    has(
        &screen(&app),
        &[
            "Signed in   login expired or revoked",
            "Shown once Aegis can be reached.",
        ],
    );
    let mut app = unpaired(Tab::Aegis);
    has(
        &screen(&app),
        &[
            " › Server      [https://aegis.example.com ]",
            "Enter pairs this machine; approve it in your browser.",
            " enter pair  esc cancel",
        ],
    );
    press(&mut app, KeyCode::Enter);
    app.device(
        "https://aegis.example.com/device".into(),
        "ABCD-EFGH".into(),
    );
    has(
        &screen(&app),
        &[
            "Open        https://aegis.example.com/device",
            "Code        ABCD-EFGH",
            "⠋ Waiting for approval…   esc cancels",
            " esc cancel  ctrl-c quit",
        ],
    );
}

#[test]
fn the_accounts_step_lists_accounts_with_their_state() {
    let screen = screen(&connected(Tab::Accounts));
    has(
        &screen,
        &[
            " › ● personal-claude    Claude    your Claude Code login  used by personal",
            "   ○ work-claude        Claude    not signed in           used by work",
            "   ● personal-chatgpt   ChatGPT   signed in               used by personal",
            "   + Add account",
            "native: managed by claude",
        ],
    );
}

#[test]
fn the_accounts_step_when_empty_and_unpaired() {
    has(
        &screen(&unpaired(Tab::Accounts)),
        &[
            "Pair this machine with Aegis first (step 1) to add accounts.",
            "No accounts yet.",
            " › + Add account",
        ],
    );
}

#[test]
fn the_add_form_and_inline_rename() {
    let mut app = connected(Tab::Accounts);
    app.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
    press(&mut app, KeyCode::Right);
    has(
        &screen(&app),
        &[
            "+ New account",
            " ›   Product     ‹ Claude │ ChatGPT ›",
            "Sign-in     ‹ Separate │ This machine's existing sign-in ›",
            "Name        [chatgpt]",
            "Route codex",
            " ↑↓ field  ←→ change  enter create  esc cancel",
        ],
    );
    let mut app = connected(Tab::Accounts);
    press(&mut app, KeyCode::Down);
    app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
    press(&mut app, KeyCode::Backspace);
    has(
        &screen(&app),
        &[" › ○ [work-claud ]", " enter save  esc cancel"],
    );
}

#[test]
fn the_profiles_step_is_a_tree_with_notes() {
    let screen = screen(&connected(Tab::Profiles));
    has(
        &screen,
        &[
            " › personal",
            "default",
            "      claude    personal-claude",
            "      chatgpt   personal-chatgpt",
            "work                             here · /w/x",
            "      claude    work-claude  ○ not signed in",
            "      chatgpt   —",
            "   + New profile",
        ],
    );
}

#[test]
fn the_profiles_step_when_empty() {
    has(
        &screen(&unpaired(Tab::Profiles)),
        &["No profiles yet.", " › + New profile", " enter new profile"],
    );
}

#[test]
fn the_account_dropdown_opens_under_the_row() {
    let mut app = connected(Tab::Profiles);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    let screen = screen(&app);
    let lines: Vec<_> = screen.lines().collect();
    let row = lines
        .iter()
        .position(|l| l.contains("› ") && l.contains("claude    personal-claude"))
        .expect("focused row");
    assert!(lines[row + 1].contains("╭"), "{screen}");
    assert!(lines[row + 2].contains("│› personal-claude"), "{screen}");
    assert!(
        lines[row + 3].contains("│  work-claude      not signed in"),
        "{screen}"
    );
    assert!(lines[row + 4].contains("│  None"), "{screen}");
    has(&screen, &[" ↑↓ choose  enter select  esc close"]);
}

#[test]
fn questions_messages_and_help() {
    let mut app = connected(Tab::Profiles);
    app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    has(
        &screen(&app),
        &[
            " ? Remove profile personal and its rules?  y/n",
            " y yes  n no",
        ],
    );
    press(&mut app, KeyCode::Esc);
    app.finish(Err(anyhow::anyhow!("it broke")));
    has(&screen(&app), &[" ✗ it broke"]);
    app.finish(Ok(crate::ags::tui::app::Outcome::say("Saved.")));
    has(&screen(&app), &[" ✓ Saved."]);
    app.key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
    has(
        &screen(&app),
        &[
            "Keys",
            "Everywhere",
            "Accounts",
            "Profiles",
            "? or esc closes",
        ],
    );
}

#[test]
fn a_small_terminal_scrolls_to_the_focus() {
    let mut app = connected(Tab::Profiles);
    for i in 0..12 {
        app.data
            .profiles
            .profiles
            .push(crate::ags::profile_store::Profile::new(&format!(
                "extra{i}"
            )));
    }
    app.key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE));
    has(&screen(&app), &[" › + New profile"]);
}

#[test]
fn a_crowded_footer_drops_the_general_keys_but_keeps_quit() {
    let mut app = connected(Tab::Accounts);
    app.focus(&crate::ags::tui::app::Item::Account("pg".into()));
    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
    terminal.draw(|frame| render(frame, &app)).unwrap();
    let buffer = terminal.backend().buffer();
    let last: String = buffer.content[buffer.content.len() - 80..]
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert_eq!(
        last.trim_end(),
        " ↑↓ move  enter sign in again  o sign out  r rename  d remove  a add  ·  q quit"
    );
}
