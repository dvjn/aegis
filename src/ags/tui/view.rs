//! Drawing the setup screen. Only the Aegis accent, muted grey, and status colors are set;
//! plain text keeps the terminal's own color, and nothing sets a background, so the screen
//! reads on light and dark terminals alike.
use super::super::{account_store::VENDORS, isolation::SignIn, ui, wizard};
use super::app::{AccountForm, App, Busy, EditTarget, Field, FormRow, Item, Link, Mode, Tab};
use super::data::{native_label, product_label};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Paragraph},
};

const ACCENT: Color = Color::Rgb(47, 111, 237);
const MUTED: Color = Color::Rgb(121, 130, 143);
const OK: Color = Color::Rgb(63, 185, 80);
const AMBER: Color = Color::Rgb(227, 179, 65);
const ERROR: Color = Color::Rgb(239, 107, 115);

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Width of the label column on the Aegis step and in the form.
const LABEL: usize = 12;

fn fg(color: Color) -> Style {
    Style::new().fg(color)
}

fn muted(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), fg(MUTED))
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

fn pad(text: &str, width: usize) -> String {
    format!("{text:<width$}")
}

/// The body: lines, and which line holds the focus (for scrolling and popups).
struct Body {
    lines: Vec<Line<'static>>,
    focus: Option<usize>,
}

impl Body {
    fn new() -> Self {
        Self {
            lines: vec![Line::default()],
            focus: None,
        }
    }

    fn blank(&mut self) {
        self.lines.push(Line::default());
    }

    fn text(&mut self, spans: Vec<Span<'static>>) {
        let mut all = vec![Span::raw("   ")];
        all.extend(spans);
        self.lines.push(Line::from(all));
    }

    /// A focusable row: a `›` marker and the accent on its label when focused.
    fn row(&mut self, focused: bool, label: Span<'static>, rest: Vec<Span<'static>>) {
        self.item(focused, None, label, rest);
    }

    /// [`Body::row`] with `lead` (a status mark that keeps its color) before the label.
    fn item(
        &mut self,
        focused: bool,
        lead: Option<Span<'static>>,
        label: Span<'static>,
        rest: Vec<Span<'static>>,
    ) {
        let marker = if focused {
            Span::styled(" › ", fg(ACCENT).add_modifier(Modifier::BOLD))
        } else {
            Span::raw("   ")
        };
        let label = if focused {
            let style = label.style.fg(label.style.fg.unwrap_or(ACCENT));
            Span::styled(label.content, style.add_modifier(Modifier::BOLD))
        } else {
            label
        };
        if focused {
            self.focus = Some(self.lines.len());
        }
        let mut spans = vec![marker];
        spans.extend(lead);
        spans.push(label);
        spans.extend(rest);
        self.lines.push(Line::from(spans));
    }
}

/// `[text]` with a block cursor.
fn field(field: &Field) -> Vec<Span<'static>> {
    let chars: Vec<char> = field.text.chars().collect();
    let before: String = chars[..field.cursor.min(chars.len())].iter().collect();
    let at = chars
        .get(field.cursor)
        .map_or(" ".to_owned(), char::to_string);
    let after: String = chars.iter().skip(field.cursor + 1).collect();
    vec![
        muted("["),
        Span::raw(before),
        Span::styled(at, Style::new().add_modifier(Modifier::REVERSED)),
        Span::raw(after),
        muted("]"),
    ]
}

fn problem(text: Option<String>) -> Vec<Span<'static>> {
    match text {
        Some(text) => vec![Span::styled(format!("  ✗ {text}"), fg(ERROR))],
        None => vec![],
    }
}

/// `‹ A │ B ›` with the chosen option in the accent.
fn selector(options: &[&str], chosen: usize, focused: bool) -> Vec<Span<'static>> {
    let mut spans = vec![muted("‹ ")];
    for (i, option) in options.iter().enumerate() {
        if i > 0 {
            spans.push(muted(" │ "));
        }
        spans.push(if i == chosen {
            let style = fg(ACCENT).add_modifier(Modifier::BOLD);
            let style = if focused {
                style.add_modifier(Modifier::UNDERLINED)
            } else {
                style
            };
            Span::styled((*option).to_owned(), style)
        } else {
            muted(*option)
        });
    }
    spans.push(muted(" ›"));
    spans
}

pub fn render(frame: &mut Frame, app: &App) {
    let [header, rule, body, rule2, message, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    render_header(frame, app, header);
    let line = muted("─".repeat(usize::from(frame.area().width)));
    frame.render_widget(Paragraph::new(Line::from(line.clone())), rule);
    frame.render_widget(Paragraph::new(Line::from(line)), rule2);
    let content = match app.tab {
        Tab::Aegis => aegis(app),
        Tab::Accounts => accounts(app),
        Tab::Profiles => profiles(app),
    };
    let height = usize::from(body.height);
    let offset = content
        .focus
        .map_or(0, |f| (f + 2).saturating_sub(height))
        .min(content.lines.len().saturating_sub(1));
    frame.render_widget(
        Paragraph::new(content.lines.clone()).scroll((offset as u16, 0)),
        body,
    );
    if let (Mode::Dropdown(_), Some(focus)) = (&app.mode, content.focus) {
        dropdown(frame, app, body, focus.saturating_sub(offset));
    }
    frame.render_widget(Paragraph::new(message_line(app)), message);
    frame.render_widget(
        Paragraph::new(footer_line(app, usize::from(footer.width))),
        footer,
    );
    if app.help {
        help(frame);
    }
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![
        Span::styled(" ◆ ", fg(ACCENT)),
        Span::styled("aegis setup", bold()),
        Span::raw("    "),
    ];
    for tab in Tab::ALL {
        let style = if tab == app.tab {
            fg(ACCENT).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::new()
        };
        spans.push(Span::raw(" "));
        spans.push(Span::styled(tab.title(), style));
        spans.push(Span::raw("  "));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    let host = app
        .data
        .server
        .as_deref()
        .map(|s| s.split("://").nth(1).unwrap_or(s).to_owned())
        .unwrap_or_default();
    let (dot, color, label) = match &app.link {
        Link::Unpaired => ("○", MUTED, "not paired".to_owned()),
        Link::Checking => ("◌", MUTED, format!("checking · {host}")),
        Link::Connected => ("●", OK, format!("connected · {host}")),
        Link::Expired => ("●", AMBER, format!("login expired · {host}")),
        Link::Unreachable(_) => ("●", ERROR, format!("unreachable · {host}")),
    };
    let status = Line::from(vec![
        Span::styled(format!("{dot} "), fg(color)),
        Span::styled(label, fg(if color == OK { MUTED } else { color })),
        Span::raw(" "),
    ])
    .alignment(Alignment::Right);
    // Only where it fits beside the tabs.
    let tabs_width = 48;
    if area.width > tabs_width + status.width() as u16 {
        frame.render_widget(Paragraph::new(status), area);
    }
}

fn editing<'a>(app: &'a App, target: &EditTarget) -> Option<&'a Field> {
    match &app.mode {
        Mode::Edit(edit) if &edit.target == target => Some(&edit.field),
        _ => None,
    }
}

fn hint(focused: bool, text: &str) -> Vec<Span<'static>> {
    if focused {
        vec![muted(format!("   {text}"))]
    } else {
        vec![]
    }
}

fn aegis(app: &App) -> Body {
    let mut body = Body::new();
    let focused = app.focused();
    let is = |item: &Item| focused.as_ref() == Some(item) && app.mode == Mode::Normal;
    let label = |text: &str| Span::raw(pad(text, LABEL));
    let server_focused = focused == Some(Item::Server);
    if let Some(edit) = editing(app, &EditTarget::Server) {
        let mut rest = field(edit);
        rest.extend(problem(app.edit_problem()));
        body.row(true, label("Server"), rest);
    } else {
        let server = app
            .data
            .server
            .clone()
            .unwrap_or_else(|| app.server_hint.clone());
        let mut rest = vec![Span::raw(server)];
        let note = if app.data.paired() {
            "enter: change"
        } else {
            "enter: pair · e: edit"
        };
        rest.extend(hint(is(&Item::Server), note));
        body.row(server_focused, label("Server"), rest);
    }
    if let Some(busy) = app.busy.as_ref().filter(|b| b.pairing) {
        pairing(&mut body, app, busy);
        return body;
    }
    if !app.data.paired() {
        body.text(vec![
            Span::raw(" ".repeat(LABEL)),
            muted("Enter pairs this machine; approve it in your browser."),
        ]);
        return body;
    }
    let (state, color) = match &app.link {
        Link::Connected => ("yes".to_owned(), OK),
        Link::Checking => ("checking…".to_owned(), MUTED),
        Link::Expired => ("login expired or revoked".to_owned(), AMBER),
        Link::Unreachable(error) => (format!("unknown: {error}"), ERROR),
        Link::Unpaired => ("no".to_owned(), MUTED),
    };
    let mut rest = vec![Span::styled(state, fg(color))];
    rest.extend(hint(is(&Item::SignedIn), "enter: sign in again"));
    body.row(is(&Item::SignedIn), label("Signed in"), rest);
    body.blank();
    body.text(vec![Span::styled(
        "Routes",
        fg(MUTED).add_modifier(Modifier::BOLD),
    )]);
    if app.routes.is_empty() {
        let note = match app.link {
            Link::Connected => "The server has no subscriptions AGS can use.",
            Link::Checking => "…",
            _ => "Shown once Aegis can be reached.",
        };
        body.text(vec![muted(note)]);
    }
    let width = app.routes.iter().map(|r| r.id.len()).max().unwrap_or(0) + 3;
    for route in &app.routes {
        let item = Item::Route(route.id.clone());
        let check = if route.enabled {
            Span::styled("[✓] ", fg(OK))
        } else {
            muted("[ ] ")
        };
        let mut rest = vec![muted(wizard::kind_label(&route.kind).to_owned())];
        if route.enabled && route.default {
            rest.push(muted(" · default"));
        }
        rest.extend(hint(is(&item), "space: toggle"));
        body.item(
            is(&item),
            Some(check),
            Span::raw(pad(&route.id, width)),
            rest,
        );
    }
    body.blank();
    let focused = is(&Item::Disconnect);
    body.row(
        focused,
        Span::raw("Disconnect"),
        hint(focused, "enter: revoke this machine's access"),
    );
    body
}

fn pairing(body: &mut Body, app: &App, busy: &Busy) {
    let spinner = SPINNER[app.tick % SPINNER.len()];
    body.blank();
    if let Some((url, code)) = &busy.device {
        body.text(vec![
            Span::raw(pad("Open", LABEL)),
            Span::styled(url.clone(), fg(ACCENT).add_modifier(Modifier::UNDERLINED)),
        ]);
        body.text(vec![
            Span::raw(pad("Code", LABEL)),
            Span::styled(code.clone(), bold()),
        ]);
        body.blank();
    }
    body.text(vec![
        Span::raw(" ".repeat(LABEL)),
        Span::styled(format!("{spinner} "), fg(ACCENT)),
        Span::raw(busy.label.clone()),
        muted("   esc cancels"),
    ]);
}

fn accounts(app: &App) -> Body {
    let mut body = Body::new();
    let focused = app.focused();
    let accounts = &app.data.accounts.accounts;
    if !app.data.paired() {
        body.text(vec![muted(
            "Pair this machine with Aegis first (step 1) to add accounts.",
        )]);
        body.blank();
    }
    if accounts.is_empty() {
        body.text(vec![Span::raw("No accounts yet.")]);
        body.text(vec![muted(
            "An account is one Claude or ChatGPT sign-in; profiles pick from them.",
        )]);
        body.blank();
    }
    let width = accounts
        .iter()
        .map(|a| a.name.len())
        .max()
        .unwrap_or(0)
        .max(10)
        + 3;
    for account in accounts {
        let item = Item::Account(account.id.clone());
        let is_focused = focused.as_ref() == Some(&item) && !matches!(app.mode, Mode::Form(_));
        let signin = app.data.signin(&account.id);
        let (dot, color) = match signin {
            SignIn::Yes | SignIn::Native => ("●", OK),
            SignIn::No => ("○", AMBER),
            SignIn::Unknown => ("?", MUTED),
        };
        let state = if account.native {
            native_label(&account.vendor).to_owned()
        } else {
            signin.label().to_owned()
        };
        let state_style = match signin {
            SignIn::No => fg(AMBER),
            SignIn::Unknown => fg(MUTED),
            _ => Style::new(),
        };
        let label = match editing(app, &EditTarget::Account(account.id.clone())) {
            Some(_) => Span::raw(""),
            None => Span::raw(pad(&account.name, width)),
        };
        let mut rest = vec![];
        if let Some(edit) = editing(app, &EditTarget::Account(account.id.clone())) {
            rest.extend(field(edit));
            rest.extend(problem(app.edit_problem()));
        } else {
            rest.push(Span::raw(pad(product_label(&account.vendor), 10)));
            rest.push(Span::styled(pad(&state, 24), state_style));
            let users = app.data.used_by(&account.id);
            if !users.is_empty() {
                rest.push(muted(format!("used by {}", users.join(", "))));
            }
        }
        body.item(
            is_focused,
            Some(Span::styled(format!("{dot} "), fg(color))),
            label,
            rest,
        );
    }
    match &app.mode {
        Mode::Form(form) => account_form(&mut body, app, form),
        _ => {
            let is_focused = focused == Some(Item::AddAccount);
            body.row(is_focused, Span::raw("+ Add account"), vec![]);
        }
    }
    body
}

fn account_form(body: &mut Body, app: &App, form: &AccountForm) {
    body.text(vec![Span::styled("+ New account", bold())]);
    let products: Vec<&str> = VENDORS.iter().map(|v| product_label(v.0)).collect();
    let routes = app.data.routes_for(form.vendor());
    for row in form.rows(&app.data) {
        let focused = form.row == row;
        let (label, rest) = match row {
            FormRow::Product => ("Product", selector(&products, form.vendor, focused)),
            FormRow::SignIn => (
                "Sign-in",
                selector(
                    &["Separate", "This machine's existing sign-in"],
                    usize::from(form.native),
                    focused,
                ),
            ),
            FormRow::Name => {
                let mut rest = if focused {
                    field(&form.name)
                } else {
                    vec![muted("["), Span::raw(form.name.text.clone()), muted("]")]
                };
                rest.extend(problem(app.form_problem()));
                ("Name", rest)
            }
            FormRow::Route => {
                let names: Vec<&str> = routes.iter().map(String::as_str).collect();
                ("Route", selector(&names, form.route, focused))
            }
        };
        let mut spans = vec![Span::raw("  ")];
        spans.extend(rest);
        body.row(
            focused,
            Span::raw(format!("  {}", pad(label, LABEL - 2))),
            spans,
        );
    }
    if routes.is_empty() {
        body.text(vec![Span::styled(
            format!(
                "    No {} route is granted to this machine; enable one on the Aegis step.",
                product_label(form.vendor())
            ),
            fg(AMBER),
        )]);
    } else if routes.len() == 1 {
        body.text(vec![muted(format!("    Route {}", routes[0]))]);
    }
}

fn profiles(app: &App) -> Body {
    let mut body = Body::new();
    let focused = app.focused();
    let normal = !matches!(app.mode, Mode::Edit(_));
    let data = &app.data;
    if data.profiles.profiles.is_empty() {
        body.text(vec![Span::raw("No profiles yet.")]);
        body.text(vec![muted(
            "A profile picks one account per product and applies by directory or as the default.",
        )]);
        body.blank();
    }
    let account_width = data
        .accounts
        .accounts
        .iter()
        .map(|a| a.name.len())
        .max()
        .unwrap_or(0);
    let names = data.profiles.profiles.iter().map(|p| p.name.len()).max();
    let width = names.unwrap_or(0).max(13 + account_width).max(16) + 4;
    for profile in &data.profiles.profiles {
        let item = Item::Profile(profile.id.clone());
        let is_focused = focused.as_ref() == Some(&item);
        let mut notes: Vec<Span<'static>> = vec![];
        let mut note = |span: Span<'static>| {
            if !notes.is_empty() {
                notes.push(muted(" · "));
            }
            notes.push(span);
        };
        if data.profiles.default.as_deref() == Some(profile.id.as_str()) {
            note(Span::styled("default", fg(ACCENT)));
        }
        if data.active.as_deref() == Some(profile.id.as_str()) {
            note(Span::styled("here", fg(OK)));
        }
        for rule in data
            .profiles
            .rules
            .iter()
            .filter(|r| r.profile == profile.id)
        {
            let path = super::super::resolve::show_path(std::path::Path::new(&rule.path));
            note(muted(ui::short_path(&path)));
        }
        match editing(app, &EditTarget::Profile(profile.id.clone())) {
            Some(edit) => {
                let mut rest = field(edit);
                rest.extend(problem(app.edit_problem()));
                body.row(true, Span::raw(""), rest);
            }
            None => body.row(
                is_focused && normal,
                Span::styled(pad(&profile.name, width), bold()),
                notes,
            ),
        }
        for &(vendor, product, _) in VENDORS {
            let item = Item::Product(profile.id.clone(), vendor);
            let is_focused = focused.as_ref() == Some(&item) && normal;
            let account = profile.accounts.get(vendor);
            let mut rest = vec![];
            match account.and_then(|id| data.accounts.get(id)) {
                Some(a) => {
                    rest.push(Span::raw(a.name.clone()));
                    if data.signin(&a.id) == SignIn::No {
                        rest.push(Span::styled("  ○ not signed in", fg(AMBER)));
                    }
                }
                None if account.is_some() => rest.push(muted("? (missing account)")),
                None => rest.push(muted("—")),
            }
            if is_focused && app.mode == Mode::Normal {
                rest.extend(hint(true, "enter: choose"));
            }
            body.row(
                is_focused,
                Span::styled(format!("   {}", pad(product, 10)), fg(MUTED)),
                rest,
            );
        }
    }
    match editing(app, &EditTarget::NewProfile) {
        Some(edit) => {
            let mut rest = field(edit);
            rest.extend(problem(
                (!edit.text.is_empty())
                    .then(|| app.edit_problem())
                    .flatten(),
            ));
            body.row(true, Span::raw("+ "), rest);
        }
        None => body.row(
            focused == Some(Item::NewProfile),
            Span::raw("+ New profile"),
            vec![],
        ),
    }
    body
}

fn dropdown(frame: &mut Frame, app: &App, body: Rect, line: usize) {
    let Mode::Dropdown(dropdown) = &app.mode else {
        return;
    };
    let names = dropdown
        .options
        .iter()
        .map(|o| o.1.chars().count())
        .max()
        .unwrap_or(4);
    let width = (names + 22).max(28) as u16;
    let height = dropdown.options.len() as u16 + 2;
    let x = body.x + 16;
    let below = body.y + line as u16 + 1;
    let y = if below + height <= body.y + body.height {
        below
    } else {
        (body.y + line as u16).saturating_sub(height)
    };
    let area = Rect::new(x, y, width, height).intersection(frame.area());
    let lines: Vec<Line> = dropdown
        .options
        .iter()
        .enumerate()
        .map(|(i, (id, name))| {
            let chosen = i == dropdown.selected;
            let marker = if chosen {
                Span::styled("› ", fg(ACCENT).add_modifier(Modifier::BOLD))
            } else {
                Span::raw("  ")
            };
            let style = if chosen {
                fg(ACCENT).add_modifier(Modifier::BOLD)
            } else if id.is_none() {
                fg(MUTED)
            } else {
                Style::new()
            };
            let state = id.as_deref().map(|id| app.data.signin(id).label());
            let mut spans = vec![marker, Span::styled(pad(name, names), style)];
            if let Some(state) = state {
                spans.push(muted(format!("  {state}")));
            }
            Line::from(spans)
        })
        .collect();
    // Clear to the right edge too, so no text of the rows underneath peeks out beside it.
    let strip = Rect::new(
        area.x,
        area.y,
        body.right().saturating_sub(area.x),
        area.height,
    );
    frame.render_widget(Clear, strip.intersection(frame.area()));
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(fg(MUTED)),
        ),
        area,
    );
}

fn message_line(app: &App) -> Line<'static> {
    if let Some(busy) = &app.busy {
        let spinner = SPINNER[app.tick % SPINNER.len()];
        return Line::from(vec![
            Span::styled(format!(" {spinner} "), fg(ACCENT)),
            Span::raw(busy.label.clone()),
        ]);
    }
    if let Mode::Confirm(confirm) = &app.mode {
        return Line::from(vec![
            Span::styled(" ? ", fg(AMBER).add_modifier(Modifier::BOLD)),
            Span::raw(confirm.prompt.clone()),
            Span::raw("  "),
            Span::styled("y/n", fg(ACCENT).add_modifier(Modifier::BOLD)),
        ]);
    }
    match &app.message {
        Some(message) if message.ok => Line::from(vec![
            Span::styled(" ✓ ", fg(OK)),
            Span::raw(message.text.clone()),
        ]),
        Some(message) => Line::from(vec![
            Span::styled(" ✗ ", fg(ERROR)),
            Span::styled(message.text.clone(), fg(ERROR)),
        ]),
        None => Line::default(),
    }
}

/// The keys for the focused element, then the general ones that fit in `width`.
fn footer_line(app: &App, width: usize) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    let key = |spans: &mut Vec<Span<'static>>, k: &str, d: &str| {
        if !k.is_empty() {
            spans.push(Span::styled(k.to_owned(), bold()));
            spans.push(Span::raw(" "));
        }
        spans.push(muted(d.to_owned()));
        spans.push(Span::raw("  "));
    };
    for (k, d) in app.hints() {
        key(&mut spans, k, &d);
    }
    if app.mode == Mode::Normal && app.busy.is_none() && !app.help {
        let general = [("tab", "next step"), ("?", "help"), ("q", "quit")];
        let used: usize = spans.iter().map(Span::width).sum();
        // Drop the least needed first when space runs out: tab, then help.
        let fits = |items: &[(&str, &str)]| {
            used + 3
                + items
                    .iter()
                    .map(|(k, d)| k.len() + d.len() + 3)
                    .sum::<usize>()
                <= width
        };
        let shown = (0..general.len())
            .find(|&skip| fits(&general[skip..]))
            .map_or(&general[2..], |skip| &general[skip..]);
        spans.push(muted("·  "));
        for (k, d) in shown {
            key(&mut spans, k, d);
        }
    }
    Line::from(spans)
}

type Section = (&'static str, &'static [(&'static str, &'static str)]);

/// The help overlay's two columns.
const HELP: [&[Section]; 2] = [
    &[
        (
            "Everywhere",
            &[
                ("tab 1 2 3 ← →", "switch step"),
                ("↑ ↓  j k", "move"),
                ("enter", "main action"),
                ("?", "this help"),
                ("q  esc", "quit"),
            ],
        ),
        (
            "Aegis",
            &[("enter", "pair or change"), ("space", "toggle a route")],
        ),
        (
            "Editing",
            &[("enter", "save"), ("esc", "cancel"), ("y  n", "answer")],
        ),
    ],
    &[
        (
            "Accounts",
            &[
                ("enter  i", "sign in"),
                ("o", "sign out"),
                ("r", "rename"),
                ("d  del", "remove"),
                ("a", "add"),
            ],
        ),
        (
            "Profiles",
            &[
                ("enter", "choose account"),
                ("d", "default on/off"),
                ("h", "use here on/off"),
                ("r", "rename"),
                ("x  del", "remove or clear"),
                ("n", "new profile"),
            ],
        ),
    ],
];

fn help_column(sections: &[Section]) -> Vec<Line<'static>> {
    let mut lines = vec![];
    for (i, (section, keys)) in sections.iter().enumerate() {
        if i > 0 {
            lines.push(Line::default());
        }
        lines.push(Line::from(Span::styled(
            format!(" {section}"),
            fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
        for (k, d) in *keys {
            lines.push(Line::from(vec![
                Span::styled(format!("   {}", pad(k, 15)), bold()),
                muted(*d),
            ]));
        }
    }
    lines
}

fn help(frame: &mut Frame) {
    let columns = HELP.map(help_column);
    let rows = columns.iter().map(Vec::len).max().unwrap_or(0) as u16;
    let area = frame.area();
    let width = 76.min(area.width.saturating_sub(2));
    let height = (rows + 2).min(area.height.saturating_sub(2));
    let popup = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(fg(MUTED))
        .title(Span::styled(
            " Keys ",
            fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(muted(" ? or esc closes "));
    let inner = block.inner(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(inner);
    let [first, second] = columns;
    frame.render_widget(Paragraph::new(first), left);
    frame.render_widget(Paragraph::new(second), right);
}

#[cfg(test)]
mod tests;
