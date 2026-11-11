//! The setup screen's state and key handling. Keys change the state and may return an
//! [`Effect`] for the runner to carry out; nothing here touches the disk or the network.
use super::super::{
    account_cmd,
    account_store::{self, VENDORS},
    isolation::SignIn,
    state,
};
use super::data::{Data, product_label};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Aegis,
    Accounts,
    Profiles,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::Aegis, Tab::Accounts, Tab::Profiles];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn title(self) -> &'static str {
        match self {
            Tab::Aegis => "Aegis",
            Tab::Accounts => "Accounts",
            Tab::Profiles => "Profiles",
        }
    }
}

/// The connection to Aegis, as last checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    Unpaired,
    Checking,
    Connected,
    /// The stored login expired or was revoked.
    Expired,
    /// The server could not be asked.
    Unreachable(String),
}

/// A server provider this machine can be granted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub id: String,
    pub kind: String,
    pub enabled: bool,
    /// The default route for its kind.
    pub default: bool,
}

/// A focusable element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    Server,
    SignedIn,
    Route(String),
    Disconnect,
    Account(String),
    AddAccount,
    Profile(String),
    /// A profile's account for a vendor.
    Product(String, &'static str),
    NewProfile,
}

/// A one-line text field; the cursor counts characters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Field {
    pub text: String,
    pub cursor: usize,
}

impl Field {
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            cursor: text.chars().count(),
        }
    }

    fn byte(&self, char_index: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_index)
            .map_or(self.text.len(), |(i, _)| i)
    }

    /// Edits for `key`; returns whether it was an editing key.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        let len = self.text.chars().count();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('u') if ctrl => {
                let at = self.byte(self.cursor);
                self.text.replace_range(..at, "");
                self.cursor = 0;
            }
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = len,
            KeyCode::Char(c) if !ctrl => {
                let at = self.byte(self.cursor);
                self.text.insert(at, c);
                self.cursor += 1;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let at = self.byte(self.cursor - 1);
                self.text.remove(at);
                self.cursor -= 1;
            }
            KeyCode::Delete if self.cursor < len => {
                let at = self.byte(self.cursor);
                self.text.remove(at);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditTarget {
    Server,
    Account(String),
    Profile(String),
    NewProfile,
}

/// An element being edited in place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub target: EditTarget,
    pub field: Field,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormRow {
    Product,
    SignIn,
    Name,
    Route,
}

/// The inline "add account" form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountForm {
    /// Index into [`VENDORS`].
    pub vendor: usize,
    pub native: bool,
    pub name: Field,
    /// The name was typed, so it no longer follows the product.
    pub touched: bool,
    pub route: usize,
    pub row: FormRow,
}

impl AccountForm {
    pub fn vendor(&self) -> &'static str {
        VENDORS[self.vendor].0
    }

    /// The rows shown for the current choices.
    pub fn rows(&self, data: &Data) -> Vec<FormRow> {
        let mut rows = vec![FormRow::Product];
        if data.offer_native(self.vendor()) {
            rows.push(FormRow::SignIn);
        }
        rows.push(FormRow::Name);
        if data.routes_for(self.vendor()).len() > 1 {
            rows.push(FormRow::Route);
        }
        rows
    }
}

/// A list of choices opened under a profile's product row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dropdown {
    pub profile: String,
    pub vendor: &'static str,
    /// (account ID or none, label).
    pub options: Vec<(Option<String>, String)>,
    pub selected: usize,
}

/// A yes/no question on the message line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Confirm {
    pub prompt: String,
    pub effect: Effect,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Edit(Edit),
    Form(AccountForm),
    Dropdown(Dropdown),
    Confirm(Confirm),
}

/// Something running in the background.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Busy {
    pub label: String,
    pub pairing: bool,
    /// Verification URL and user code, once the server sent them.
    pub device: Option<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub ok: bool,
    pub text: String,
}

/// Work for the runner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Quit,
    /// Stop the running pairing.
    Cancel,
    /// Pair with `server`; `again` signs in again as the current client, keeping its routes.
    Pair {
        server: String,
        again: bool,
    },
    /// Disconnect from the current server, then pair with `server`.
    SwitchServer {
        server: String,
    },
    Disconnect,
    SetRoutes(Vec<String>),
    /// Apply `--provider` choices from the command line.
    ApplyDefaults(Vec<String>),
    CreateAccount {
        vendor: &'static str,
        name: String,
        native: bool,
        route: String,
    },
    /// Runs the vendor's login or logout with the terminal handed over.
    VendorAuth {
        account: String,
        logout: bool,
    },
    RenameAccount {
        account: String,
        name: String,
    },
    RemoveAccount {
        account: String,
    },
    SetAccount {
        profile: String,
        vendor: &'static str,
        account: Option<String>,
    },
    ToggleDefault {
        profile: String,
    },
    ToggleHere {
        profile: String,
    },
    RenameProfile {
        profile: String,
        name: String,
    },
    RemoveProfile {
        profile: String,
    },
    CreateProfile {
        name: String,
    },
}

impl Effect {
    /// What the message line says while it runs in the background; `None` for effects the
    /// runner carries out itself.
    fn label(&self, data: &Data) -> Option<String> {
        let account = |id: &str| {
            data.accounts
                .get(id)
                .map_or_else(|| "the account".to_owned(), |a| a.name.clone())
        };
        let profile = |id: &str| {
            data.profile(id)
                .map_or_else(|| "the profile".to_owned(), |p| p.name.clone())
        };
        Some(match self {
            Effect::Quit | Effect::Cancel | Effect::VendorAuth { .. } => return None,
            Effect::Pair { .. } | Effect::SwitchServer { .. } => "Contacting Aegis…".into(),
            Effect::Disconnect => "Disconnecting…".into(),
            Effect::SetRoutes(_) | Effect::ApplyDefaults(_) => "Saving routes…".into(),
            Effect::CreateAccount { name, .. } => format!("Creating {name}…"),
            Effect::RenameAccount { account: id, .. } => format!("Renaming {}…", account(id)),
            Effect::RemoveAccount { account: id } => format!("Removing {}…", account(id)),
            Effect::SetAccount { profile: id, .. }
            | Effect::ToggleDefault { profile: id }
            | Effect::ToggleHere { profile: id } => format!("Saving {}…", profile(id)),
            Effect::RenameProfile { profile: id, .. } => format!("Renaming {}…", profile(id)),
            Effect::RemoveProfile { profile: id } => format!("Removing {}…", profile(id)),
            Effect::CreateProfile { name } => format!("Creating {name}…"),
        })
    }
}

/// How a finished effect went.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub message: String,
    /// Element to focus afterwards.
    pub focus: Option<Item>,
    /// A follow-up question.
    pub then: Option<Confirm>,
}

impl Outcome {
    pub fn say(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            ..Self::default()
        }
    }
}

pub struct App {
    pub data: Data,
    pub link: Link,
    pub routes: Vec<Route>,
    pub tab: Tab,
    focus: [usize; 3],
    pub mode: Mode,
    pub help: bool,
    pub message: Option<Message>,
    pub busy: Option<Busy>,
    /// Server offered for a first pairing.
    pub server_hint: String,
    /// Animation frame, advanced by the runner.
    pub tick: usize,
}

const ROUTE_HINT: &str = "Enable one on the Aegis step";

impl App {
    pub fn new(data: Data, tab: Tab, server_hint: String) -> Self {
        let link = if data.paired() {
            Link::Checking
        } else {
            Link::Unpaired
        };
        let mut app = Self {
            data,
            link,
            routes: vec![],
            tab,
            focus: [0; 3],
            mode: Mode::Normal,
            help: false,
            message: None,
            busy: None,
            server_hint,
            tick: 0,
        };
        if tab == Tab::Aegis && !app.data.paired() {
            app.edit_server();
        }
        app
    }

    // Elements and focus.

    pub fn items(&self, tab: Tab) -> Vec<Item> {
        match tab {
            Tab::Aegis => {
                if !self.data.paired() {
                    return vec![Item::Server];
                }
                let mut items = vec![Item::Server, Item::SignedIn];
                items.extend(self.routes.iter().map(|r| Item::Route(r.id.clone())));
                items.push(Item::Disconnect);
                items
            }
            Tab::Accounts => {
                let mut items: Vec<_> = self
                    .data
                    .accounts
                    .accounts
                    .iter()
                    .map(|a| Item::Account(a.id.clone()))
                    .collect();
                items.push(Item::AddAccount);
                items
            }
            Tab::Profiles => {
                let mut items = vec![];
                for p in &self.data.profiles.profiles {
                    items.push(Item::Profile(p.id.clone()));
                    for &(vendor, _, _) in VENDORS {
                        items.push(Item::Product(p.id.clone(), vendor));
                    }
                }
                items.push(Item::NewProfile);
                items
            }
        }
    }

    pub fn focused(&self) -> Option<Item> {
        let items = self.items(self.tab);
        let index = self.focus[self.tab.index()].min(items.len().saturating_sub(1));
        items.into_iter().nth(index)
    }

    pub fn focus(&mut self, item: &Item) {
        for tab in Tab::ALL {
            if let Some(i) = self.items(tab).iter().position(|x| x == item) {
                self.tab = tab;
                self.focus[tab.index()] = i;
            }
        }
    }

    fn step(&mut self, delta: isize) {
        let count = self.items(self.tab).len();
        let focus = &mut self.focus[self.tab.index()];
        let current = (*focus).min(count.saturating_sub(1));
        *focus = current
            .saturating_add_signed(delta)
            .min(count.saturating_sub(1));
    }

    fn switch(&mut self, delta: isize) {
        let index = (self.tab.index() as isize + delta).rem_euclid(3);
        self.tab = Tab::ALL[index as usize];
    }

    /// Replaces the data after a change on disk, keeping each tab's focus on the same element.
    pub fn set_data(&mut self, data: Data) {
        let before: Vec<_> = Tab::ALL
            .iter()
            .map(|&t| {
                let items = self.items(t);
                let index = self.focus[t.index()].min(items.len().saturating_sub(1));
                items.get(index).cloned()
            })
            .collect();
        self.data = data;
        if !self.data.paired() {
            self.link = Link::Unpaired;
            self.routes.clear();
        } else if self.link == Link::Unpaired {
            self.link = Link::Checking;
        }
        for tab in Tab::ALL {
            let items = self.items(tab);
            if let Some(i) = before[tab.index()]
                .as_ref()
                .and_then(|item| items.iter().position(|x| x == item))
            {
                self.focus[tab.index()] = i;
            } else {
                self.focus[tab.index()] =
                    self.focus[tab.index()].min(items.len().saturating_sub(1));
            }
        }
    }

    pub fn set_link(&mut self, link: Link, routes: Vec<Route>) {
        let focused = self.focused();
        self.link = link;
        self.routes = routes;
        if let Some(item) = focused
            && self.items(self.tab).contains(&item)
        {
            self.focus(&item);
        } else {
            self.step(0);
        }
    }

    /// The pairing's verification URL and code arrived.
    pub fn device(&mut self, url: String, code: String) {
        if let Some(busy) = self.busy.as_mut().filter(|b| b.pairing) {
            busy.device = Some((url, code));
            busy.label = "Waiting for approval…".into();
        }
    }

    /// Records the outcome of a background or suspended effect.
    pub fn finish(&mut self, result: anyhow::Result<Outcome>) {
        self.busy = None;
        match result {
            Ok(outcome) => {
                if let Some(item) = &outcome.focus {
                    self.focus(item);
                }
                self.message = (!outcome.message.is_empty()).then_some(Message {
                    ok: true,
                    text: outcome.message,
                });
                if let Some(confirm) = outcome.then {
                    self.mode = Mode::Confirm(confirm);
                }
            }
            Err(error) => self.fail(format!("{error:#}")),
        }
    }

    fn fail(&mut self, text: impl Into<String>) {
        self.message = Some(Message {
            ok: false,
            text: text.into(),
        });
    }

    /// Starts `effect`, marking the screen busy when it runs in the background.
    pub fn start(&mut self, effect: Effect) -> Option<Effect> {
        if let Some(label) = effect.label(&self.data) {
            self.busy = Some(Busy {
                label,
                pairing: matches!(effect, Effect::Pair { .. } | Effect::SwitchServer { .. }),
                device: None,
            });
        }
        Some(effect)
    }

    fn ask(&mut self, prompt: String, effect: Effect) {
        self.mode = Mode::Confirm(Confirm { prompt, effect });
    }

    // Keys.

    pub fn key(&mut self, key: KeyEvent) -> Option<Effect> {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Some(Effect::Quit);
        }
        if self.help {
            match key.code {
                KeyCode::Char('q') => return Some(Effect::Quit),
                KeyCode::Char('?') | KeyCode::Esc | KeyCode::Enter => self.help = false,
                _ => {}
            }
            return None;
        }
        if let Some(busy) = &self.busy {
            if busy.pairing && key.code == KeyCode::Esc {
                self.busy = None;
                self.message = Some(Message {
                    ok: false,
                    text: "Pairing cancelled.".into(),
                });
                return Some(Effect::Cancel);
            }
            return None;
        }
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => self.normal(key),
            Mode::Edit(edit) => self.edit_key(edit, key),
            Mode::Form(form) => self.form_key(form, key),
            Mode::Dropdown(dropdown) => self.dropdown_key(dropdown, key),
            Mode::Confirm(confirm) => match key.code {
                KeyCode::Char('y' | 'Y') => {
                    self.message = None;
                    self.start(confirm.effect)
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    self.message = None;
                    None
                }
                _ => {
                    self.mode = Mode::Confirm(confirm);
                    None
                }
            },
        }
    }

    fn normal(&mut self, key: KeyEvent) -> Option<Effect> {
        self.message = None;
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Some(Effect::Quit),
            KeyCode::Char('?') => self.help = true,
            KeyCode::Tab | KeyCode::Right => self.switch(1),
            KeyCode::BackTab | KeyCode::Left => self.switch(-1),
            KeyCode::Char(c @ '1'..='3') => self.tab = Tab::ALL[c as usize - '1' as usize],
            KeyCode::Up | KeyCode::Char('k') => self.step(-1),
            KeyCode::Down | KeyCode::Char('j') => self.step(1),
            KeyCode::Home | KeyCode::Char('g') => self.focus[self.tab.index()] = 0,
            KeyCode::End | KeyCode::Char('G') => self.step(isize::MAX),
            _ => return self.act(key),
        }
        None
    }

    /// A key for the focused element.
    fn act(&mut self, key: KeyEvent) -> Option<Effect> {
        let code = key.code;
        let enter = code == KeyCode::Enter;
        match self.tab {
            Tab::Accounts if code == KeyCode::Char('a') => return self.open_form(),
            Tab::Profiles if code == KeyCode::Char('n') => {
                self.new_profile();
                return None;
            }
            _ => {}
        }
        match self.focused()? {
            Item::Server if enter && !self.data.paired() => self.pair_hint(),
            Item::Server if enter || code == KeyCode::Char('e') => {
                self.edit_server();
                None
            }
            Item::SignedIn if enter => {
                let server = self.data.server.clone()?;
                self.start(Effect::Pair {
                    server,
                    again: true,
                })
            }
            Item::Route(id) if enter || code == KeyCode::Char(' ') => self.toggle_route(&id),
            Item::Disconnect if enter => {
                let server = self.data.server.clone()?;
                self.ask(
                    format!("Disconnect from {server} and revoke this machine's access?"),
                    Effect::Disconnect,
                );
                None
            }
            Item::Account(id) => self.account_key(&id, code),
            Item::AddAccount if enter => self.open_form(),
            Item::Profile(id) => self.profile_key(&id, code),
            Item::Product(profile, vendor) => match code {
                KeyCode::Enter => {
                    self.open_dropdown(&profile, vendor);
                    None
                }
                KeyCode::Char('x') | KeyCode::Delete => {
                    let current = self.data.profile(&profile)?.accounts.get(vendor);
                    current?;
                    self.start(Effect::SetAccount {
                        profile,
                        vendor,
                        account: None,
                    })
                }
                _ => None,
            },
            Item::NewProfile if enter => {
                self.new_profile();
                None
            }
            _ => None,
        }
    }

    fn account_key(&mut self, id: &str, code: KeyCode) -> Option<Effect> {
        let account = self.data.accounts.get(id)?.clone();
        let signin = self.data.signin(id);
        match code {
            KeyCode::Enter | KeyCode::Char('i') => {
                if account.native {
                    let cli = account_store::cli(&account.vendor);
                    self.fail(format!(
                        "{} uses this machine's own sign-in; sign in with {cli} itself.",
                        account.name
                    ));
                    return None;
                }
                self.start(Effect::VendorAuth {
                    account: account.id,
                    logout: false,
                })
            }
            KeyCode::Char('o') if can_sign_out(account.native, signin) => {
                self.start(Effect::VendorAuth {
                    account: account.id,
                    logout: true,
                })
            }
            KeyCode::Char('r') => {
                self.mode = Mode::Edit(Edit {
                    target: EditTarget::Account(account.id),
                    field: Field::new(&account.name),
                });
                None
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                let users = self.data.used_by(&account.id);
                let mut steps = vec![format!("Remove {}", account.name)];
                if !users.is_empty() {
                    steps.push(format!("detach it from {}", users.join(", ")));
                }
                if !account.native {
                    steps.push("delete its sign-in".to_owned());
                }
                let prompt = match steps.len() {
                    1 => format!("{}?", steps[0]),
                    2 => format!("{} and {}?", steps[0], steps[1]),
                    _ => format!("{}, {}, and {}?", steps[0], steps[1], steps[2]),
                };
                self.ask(
                    prompt,
                    Effect::RemoveAccount {
                        account: account.id,
                    },
                );
                None
            }
            _ => None,
        }
    }

    fn profile_key(&mut self, id: &str, code: KeyCode) -> Option<Effect> {
        let profile = self.data.profile(id)?.clone();
        let profile_id = profile.id.clone();
        match code {
            KeyCode::Enter | KeyCode::Char('d') => self.start(Effect::ToggleDefault {
                profile: profile_id,
            }),
            KeyCode::Char('h') => self.start(Effect::ToggleHere {
                profile: profile_id,
            }),
            KeyCode::Char('r') => {
                self.mode = Mode::Edit(Edit {
                    target: EditTarget::Profile(profile_id),
                    field: Field::new(&profile.name),
                });
                None
            }
            KeyCode::Char('x') | KeyCode::Delete => {
                self.ask(
                    format!("Remove profile {} and its rules?", profile.name),
                    Effect::RemoveProfile {
                        profile: profile_id,
                    },
                );
                None
            }
            _ => None,
        }
    }

    // Aegis.

    fn edit_server(&mut self) {
        let text = self
            .data
            .server
            .clone()
            .unwrap_or_else(|| self.server_hint.clone());
        self.focus(&Item::Server);
        self.mode = Mode::Edit(Edit {
            target: EditTarget::Server,
            field: Field::new(&text),
        });
    }

    fn pair_hint(&mut self) -> Option<Effect> {
        match state::validate_server(self.server_hint.trim()) {
            Ok(server) => self.start(Effect::Pair {
                server,
                again: false,
            }),
            Err(error) => {
                self.fail(error.to_string());
                None
            }
        }
    }

    fn toggle_route(&mut self, id: &str) -> Option<Effect> {
        if self.link != Link::Connected {
            self.fail("Routes can be changed while connected.");
            return None;
        }
        let selected: Vec<String> = self
            .routes
            .iter()
            .filter(|r| r.enabled != (r.id == id))
            .map(|r| r.id.clone())
            .collect();
        if selected.is_empty() {
            self.fail("Keep at least one route enabled.");
            return None;
        }
        self.start(Effect::SetRoutes(selected))
    }

    // Inline editing.

    /// Why the field being edited cannot be saved, if it cannot.
    pub fn edit_problem(&self) -> Option<String> {
        let Mode::Edit(edit) = &self.mode else {
            return None;
        };
        let text = edit.field.text.trim();
        match &edit.target {
            EditTarget::Server => state::validate_server(text).err().map(|e| e.to_string()),
            EditTarget::Account(id) => {
                let current = self.data.accounts.get(id).map(|a| a.name.as_str());
                if current == Some(text) {
                    return None;
                }
                account_cmd::name_problem(&self.data.accounts, text)
            }
            EditTarget::Profile(id) => {
                let current = self.data.profile(id).map(|p| p.name.as_str());
                if current == Some(text) {
                    return None;
                }
                self.data
                    .profiles
                    .check_new(text)
                    .err()
                    .map(|e| e.to_string())
            }
            EditTarget::NewProfile => self
                .data
                .profiles
                .check_new(text)
                .err()
                .map(|e| e.to_string()),
        }
    }

    fn edit_key(&mut self, mut edit: Edit, key: KeyEvent) -> Option<Effect> {
        match key.code {
            KeyCode::Esc => {
                self.message = None;
                None
            }
            KeyCode::Enter => {
                self.mode = Mode::Edit(edit.clone());
                if let Some(problem) = self.edit_problem() {
                    self.fail(problem);
                    return None;
                }
                self.mode = Mode::Normal;
                self.message = None;
                self.commit(edit)
            }
            _ => {
                edit.field.key(key);
                self.mode = Mode::Edit(edit);
                None
            }
        }
    }

    fn commit(&mut self, edit: Edit) -> Option<Effect> {
        let text = edit.field.text.trim().to_owned();
        match edit.target {
            EditTarget::Server => {
                let server = state::validate_server(&text).ok()?;
                match self.data.server.clone() {
                    None => {
                        self.server_hint = server.clone();
                        self.start(Effect::Pair {
                            server,
                            again: false,
                        })
                    }
                    Some(current) if current == server => None,
                    Some(current) => {
                        self.ask(
                            format!(
                                "Disconnect from {current} and revoke access, then pair with {server}?"
                            ),
                            Effect::SwitchServer { server },
                        );
                        None
                    }
                }
            }
            EditTarget::Account(id) => {
                let current = self.data.accounts.get(&id)?;
                if current.name == text {
                    return None;
                }
                self.start(Effect::RenameAccount {
                    account: id,
                    name: text,
                })
            }
            EditTarget::Profile(id) => {
                let current = self.data.profile(&id)?;
                if current.name == text {
                    return None;
                }
                self.start(Effect::RenameProfile {
                    profile: id,
                    name: text,
                })
            }
            EditTarget::NewProfile => self.start(Effect::CreateProfile { name: text }),
        }
    }

    fn new_profile(&mut self) {
        self.focus(&Item::NewProfile);
        self.mode = Mode::Edit(Edit {
            target: EditTarget::NewProfile,
            field: Field::default(),
        });
    }

    // The add-account form.

    fn open_form(&mut self) -> Option<Effect> {
        if !self.data.paired() {
            self.fail("Pair this machine with Aegis first (step 1).");
            return None;
        }
        self.focus(&Item::AddAccount);
        let mut form = AccountForm {
            vendor: 0,
            native: false,
            name: Field::default(),
            touched: false,
            route: 0,
            row: FormRow::Product,
        };
        self.refresh_form(&mut form);
        self.mode = Mode::Form(form);
        None
    }

    /// Keeps the form's choices valid and its suggested name current.
    fn refresh_form(&self, form: &mut AccountForm) {
        let vendor = form.vendor();
        if !self.data.offer_native(vendor) {
            form.native = false;
        }
        form.route = form
            .route
            .min(self.data.routes_for(vendor).len().saturating_sub(1));
        if !form.touched {
            let product = account_store::product(vendor);
            form.name = Field::new(&account_cmd::default_name(&self.data.accounts, product));
        }
        let rows = form.rows(&self.data);
        if !rows.contains(&form.row) {
            form.row = FormRow::Product;
        }
    }

    pub fn form_problem(&self) -> Option<String> {
        let Mode::Form(form) = &self.mode else {
            return None;
        };
        account_cmd::name_problem(&self.data.accounts, form.name.text.trim())
    }

    fn form_key(&mut self, mut form: AccountForm, key: KeyEvent) -> Option<Effect> {
        let rows = form.rows(&self.data);
        let at = rows.iter().position(|r| *r == form.row).unwrap_or(0);
        let cycle = |delta: isize| -> Option<isize> {
            match key.code {
                KeyCode::Left => Some(-delta),
                KeyCode::Right | KeyCode::Char(' ') => Some(delta),
                _ => None,
            }
        };
        match key.code {
            KeyCode::Esc => {
                self.message = None;
                return None;
            }
            KeyCode::Enter => {
                self.mode = Mode::Form(form.clone());
                return self.create(form);
            }
            KeyCode::Up | KeyCode::BackTab => form.row = rows[at.saturating_sub(1)],
            KeyCode::Down | KeyCode::Tab => form.row = rows[(at + 1).min(rows.len() - 1)],
            _ => match form.row {
                FormRow::Name => {
                    if form.name.key(key) {
                        form.touched = true;
                    }
                }
                FormRow::Product => {
                    if let Some(d) = cycle(1) {
                        form.vendor =
                            (form.vendor as isize + d).rem_euclid(VENDORS.len() as isize) as usize;
                        form.route = 0;
                    }
                }
                FormRow::SignIn => {
                    if cycle(1).is_some() {
                        form.native = !form.native;
                    }
                }
                FormRow::Route => {
                    let count = self.data.routes_for(form.vendor()).len().max(1) as isize;
                    if let Some(d) = cycle(1) {
                        form.route = (form.route as isize + d).rem_euclid(count) as usize;
                    }
                }
            },
        }
        self.message = None;
        self.refresh_form(&mut form);
        self.mode = Mode::Form(form);
        None
    }

    fn create(&mut self, form: AccountForm) -> Option<Effect> {
        let vendor = form.vendor();
        let routes = self.data.routes_for(vendor);
        let Some(route) = routes.get(form.route).cloned() else {
            self.fail(format!(
                "No {} route is granted to this machine. {ROUTE_HINT}.",
                product_label(vendor)
            ));
            return None;
        };
        if let Some(problem) = self.form_problem() {
            self.fail(problem);
            return None;
        }
        self.mode = Mode::Normal;
        self.message = None;
        self.start(Effect::CreateAccount {
            vendor,
            name: form.name.text.trim().to_owned(),
            native: form.native,
            route,
        })
    }

    // The account dropdown.

    fn open_dropdown(&mut self, profile: &str, vendor: &'static str) {
        let Some(current) = self.data.profile(profile).map(|p| p.accounts.get(vendor)) else {
            return;
        };
        let mut options: Vec<_> = self
            .data
            .accounts
            .for_vendor(vendor)
            .into_iter()
            .map(|a| (Some(a.id.clone()), a.name.clone()))
            .collect();
        if options.is_empty() {
            self.fail(format!(
                "No {} accounts yet; add one on the Accounts step.",
                product_label(vendor)
            ));
            return;
        }
        options.push((None, "None".to_owned()));
        // The current account, else the first one: choosing one is why the list was opened.
        let selected = options
            .iter()
            .position(|o| current.is_some() && o.0.as_ref() == current)
            .unwrap_or(0);
        self.mode = Mode::Dropdown(Dropdown {
            profile: profile.to_owned(),
            vendor,
            options,
            selected,
        });
    }

    fn dropdown_key(&mut self, mut dropdown: Dropdown, key: KeyEvent) -> Option<Effect> {
        let last = dropdown.options.len() - 1;
        match key.code {
            KeyCode::Esc => return None,
            KeyCode::Enter => {
                let account = dropdown.options[dropdown.selected].0.clone();
                let current = self
                    .data
                    .profile(&dropdown.profile)
                    .and_then(|p| p.accounts.get(dropdown.vendor))
                    .cloned();
                if account == current {
                    return None;
                }
                return self.start(Effect::SetAccount {
                    profile: dropdown.profile,
                    vendor: dropdown.vendor,
                    account,
                });
            }
            KeyCode::Up | KeyCode::Char('k') => {
                dropdown.selected = dropdown.selected.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                dropdown.selected = (dropdown.selected + 1).min(last)
            }
            _ => {}
        }
        self.mode = Mode::Dropdown(dropdown);
        None
    }

    // The footer.

    /// Keys valid right now, most specific first: (key, what it does).
    pub fn hints(&self) -> Vec<(&'static str, String)> {
        let h = |k: &'static str, d: &str| (k, d.to_owned());
        if self.help {
            return vec![h("?", "close help"), h("q", "quit")];
        }
        if let Some(busy) = &self.busy {
            return if busy.pairing {
                vec![h("esc", "cancel"), h("ctrl-c", "quit")]
            } else {
                vec![]
            };
        }
        match &self.mode {
            Mode::Confirm(_) => return vec![h("y", "yes"), h("n", "no")],
            Mode::Dropdown(_) => {
                return vec![h("↑↓", "choose"), h("enter", "select"), h("esc", "close")];
            }
            Mode::Form(form) => {
                let change = if form.row == FormRow::Name {
                    "type the name"
                } else {
                    "change"
                };
                let arrows = if form.row == FormRow::Name {
                    ""
                } else {
                    "←→"
                };
                let mut keys = vec![h("↑↓", "field")];
                if arrows.is_empty() {
                    keys.push(h("", change));
                } else {
                    keys.push(h(arrows, change));
                }
                keys.extend([h("enter", "create"), h("esc", "cancel")]);
                return keys;
            }
            Mode::Edit(edit) => {
                let save = match edit.target {
                    EditTarget::Server if !self.data.paired() => "pair",
                    EditTarget::NewProfile => "create",
                    _ => "save",
                };
                return vec![h("enter", save), h("esc", "cancel")];
            }
            Mode::Normal => {}
        }
        let mut keys = vec![h("↑↓", "move")];
        match self.focused() {
            Some(Item::Server) if !self.data.paired() => {
                keys.extend([h("enter", "pair"), h("e", "edit")]);
            }
            Some(Item::Server) => keys.push(h("enter", "change server")),
            Some(Item::SignedIn) => keys.push(h("enter", "sign in again")),
            Some(Item::Route(_)) if self.link == Link::Connected => {
                keys.push(h("space", "toggle"));
            }
            Some(Item::Disconnect) => keys.push(h("enter", "disconnect")),
            Some(Item::Account(id)) => {
                if let Some(a) = self.data.accounts.get(&id) {
                    let signin = self.data.signin(&id);
                    if a.native {
                        let cli = account_store::cli(&a.vendor);
                        keys.push(("native:", format!("managed by {cli}")));
                    } else if signin == SignIn::Yes {
                        keys.push(h("enter", "sign in again"));
                    } else {
                        keys.push(h("enter", "sign in"));
                    }
                    if can_sign_out(a.native, signin) {
                        keys.push(h("o", "sign out"));
                    }
                    keys.extend([h("r", "rename"), h("d", "remove")]);
                }
                keys.push(h("a", "add"));
            }
            Some(Item::AddAccount) => keys.push(h("enter", "add account")),
            Some(Item::Profile(id)) => {
                let default = self.data.profiles.default.as_deref() == Some(id.as_str());
                keys.push(h(
                    "d",
                    if default {
                        "unset default"
                    } else {
                        "make default"
                    },
                ));
                keys.push(h(
                    "h",
                    if self.data.rule_here(&id) {
                        "stop using here"
                    } else {
                        "use here"
                    },
                ));
                keys.extend([h("r", "rename"), h("x", "remove"), h("n", "new")]);
            }
            Some(Item::Product(profile, vendor)) => {
                keys.push(h("enter", "choose account"));
                if self
                    .data
                    .profile(&profile)
                    .is_some_and(|p| p.accounts.contains_key(vendor))
                {
                    keys.push(h("x", "clear"));
                }
                keys.push(h("n", "new profile"));
            }
            Some(Item::NewProfile) => keys.push(h("enter", "new profile")),
            _ => {}
        }
        keys
    }
}

/// Managed accounts that are (or may be) signed in can be signed out here.
pub fn can_sign_out(native: bool, signin: SignIn) -> bool {
    !native && matches!(signin, SignIn::Yes | SignIn::Unknown)
}

#[cfg(test)]
mod tests;
