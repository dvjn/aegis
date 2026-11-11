//! The terminal side: raw mode and the alternate screen (restored on exit and on panic), the
//! event loop, and carrying out effects, in the background or with the terminal handed over.
use super::super::{
    account_cmd,
    account_store::{self, AccountStore},
    auth, profile_cmd,
    profile_store::{Profile, ProfileStore},
    prompt, state, ui, wizard,
};
use super::{
    Options,
    app::{App, Confirm, Effect, Item, Link, Outcome, Route, Tab},
    data::{self, Data, Here, product_label},
    view,
};
use anyhow::{Context, Result, anyhow};
use crossterm::{
    cursor,
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    io::{self, Stderr},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinHandle};

/// The full-screen terminal; restores the normal screen when dropped.
struct Screen {
    terminal: Terminal<CrosstermBackend<Stderr>>,
}

fn enter() -> io::Result<()> {
    terminal::enable_raw_mode()?;
    execute!(io::stderr(), EnterAlternateScreen, cursor::Hide)
}

fn leave() -> io::Result<()> {
    terminal::disable_raw_mode()?;
    execute!(io::stderr(), LeaveAlternateScreen, cursor::Show)
}

impl Screen {
    fn open() -> io::Result<Self> {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = leave();
            previous(info);
        }));
        enter()?;
        let terminal = Terminal::new(CrosstermBackend::new(io::stderr()))?;
        Ok(Self { terminal })
    }

    /// Hands the terminal back for a vendor command.
    fn suspend(&mut self) -> io::Result<()> {
        leave()
    }

    fn resume(&mut self) -> io::Result<()> {
        enter()?;
        // A fresh terminal redraws everything on a cleared screen; `Terminal::clear` would ask
        // the terminal for the cursor position, which not every terminal answers.
        execute!(io::stderr(), terminal::Clear(terminal::ClearType::All))?;
        self.terminal = Terminal::new(CrosstermBackend::new(io::stderr()))?;
        Ok(())
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = leave();
    }
}

enum Msg {
    Link(Link, Vec<Route>),
    Device(String, String),
    Done(Result<Outcome>),
}

struct Runner {
    home: PathBuf,
    here: Here,
    options: Arc<Options>,
    tx: mpsc::UnboundedSender<Msg>,
    /// The running effect.
    task: Option<JoinHandle<()>>,
    /// The running connection check.
    check: Option<JoinHandle<()>>,
}

/// Runs the setup screen until the user quits.
pub async fn run(home: &Path, tab: Tab, options: Options) -> Result<()> {
    let here = Here::current()?;
    let data = Data::load(home, &here)?;
    let hint = data::server_hint(home, options.server.as_deref());
    let mut app = App::new(data, tab, hint);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut runner = Runner {
        home: home.to_owned(),
        here,
        options: Arc::new(options),
        tx,
        task: None,
        check: None,
    };
    if app.data.paired() {
        if runner.options.defaults.is_empty() {
            runner.check();
        } else {
            let defaults = runner.options.defaults.clone();
            if let Some(effect) = app.start(Effect::ApplyDefaults(defaults)) {
                runner.spawn(effect);
            }
        }
    }
    let mut screen = Screen::open()?;
    let result = runner.events(&mut app, &mut screen, &mut rx).await;
    for handle in [runner.task.take(), runner.check.take()]
        .into_iter()
        .flatten()
    {
        handle.abort();
    }
    drop(screen);
    result
}

impl Runner {
    async fn events(
        &mut self,
        app: &mut App,
        screen: &mut Screen,
        rx: &mut mpsc::UnboundedReceiver<Msg>,
    ) -> Result<()> {
        loop {
            screen.terminal.draw(|frame| view::render(frame, app))?;
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    Msg::Link(link, routes) => app.set_link(link, routes),
                    Msg::Device(url, code) => app.device(url, code),
                    Msg::Done(result) => {
                        self.task = None;
                        self.reload(app)?;
                        app.finish(result);
                        if app.data.paired() {
                            self.check();
                        }
                    }
                }
            }
            if event::poll(Duration::from_millis(80))?
                && let Event::Key(key) = event::read()?
                && key.kind != KeyEventKind::Release
                && let Some(effect) = app.key(key)
            {
                match effect {
                    Effect::Quit => return Ok(()),
                    Effect::Cancel => {
                        if let Some(task) = self.task.take() {
                            task.abort();
                        }
                    }
                    Effect::VendorAuth { account, logout } => {
                        let result = self.vendor_auth(screen, &account, logout).await;
                        self.reload(app)?;
                        app.finish(result);
                    }
                    effect => self.spawn(effect),
                }
            }
            app.tick = app.tick.wrapping_add(1);
        }
    }

    fn reload(&self, app: &mut App) -> Result<()> {
        app.set_data(Data::load(&self.home, &self.here)?);
        Ok(())
    }

    /// Checks the connection in the background and lists the routes.
    fn check(&mut self) {
        if let Some(check) = self.check.take() {
            check.abort();
        }
        let (home, tx) = (self.home.clone(), self.tx.clone());
        self.check = Some(tokio::spawn(async move {
            let (link, routes) = link(&home).await;
            let _ = tx.send(Msg::Link(link, routes));
        }));
    }

    fn spawn(&mut self, effect: Effect) {
        let (home, options, tx) = (self.home.clone(), self.options.clone(), self.tx.clone());
        self.task = Some(tokio::spawn(async move {
            let result = execute(&home, &options, &tx, effect).await;
            let _ = tx.send(Msg::Done(result));
        }));
    }

    /// Runs the vendor's login or logout with the terminal handed over.
    async fn vendor_auth(&self, screen: &mut Screen, id: &str, logout: bool) -> Result<Outcome> {
        let account = AccountStore::load(&self.home)?
            .get(id)
            .cloned()
            .context("the account no longer exists")?;
        let cli = account_store::cli(&account.vendor);
        let verb = if logout { "Signing out" } else { "Signing in" };
        screen.suspend()?;
        eprintln!(
            "\n{}\n",
            ui::muted(
                ui::Out::Stderr,
                format!("── {verb} {} with {cli} ──", account.name)
            )
        );
        let code = {
            let _shield = prompt::Shield::new();
            account_cmd::vendor_auth(&self.home, &account, logout).await
        };
        screen.resume()?;
        match code? {
            0 => Ok(Outcome::say(format!(
                "{} signed {}.",
                account.name,
                if logout { "out" } else { "in" }
            ))),
            code => Err(anyhow!("{cli} exited with status {code}")),
        }
    }
}

/// The connection state and, when connected, the routes this machine can use.
async fn link(home: &Path) -> (Link, Vec<Route>) {
    if !home.join("connection.json").exists() {
        return (Link::Unpaired, vec![]);
    }
    let checked = async {
        let config = auth::connected(home).await?;
        let available = auth::catalog(&config).await?.providers;
        Ok::<_, anyhow::Error>((config, available))
    }
    .await;
    match checked {
        Ok((config, available)) => {
            let routes = wizard::supported(&available)
                .into_iter()
                .map(|p| Route {
                    enabled: config.providers.iter().any(|c| c.id == p.id),
                    default: config.provider_defaults.get(&p.kind) == Some(&p.id),
                    id: p.id,
                    kind: p.kind,
                })
                .collect();
            (Link::Connected, routes)
        }
        Err(error) if error.downcast_ref::<auth::LoginRequired>().is_some() => {
            (Link::Expired, vec![])
        }
        Err(error) => (Link::Unreachable(short_error(&error)), vec![]),
    }
}

/// The innermost cause, which is what tells a connection problem apart.
fn short_error(error: &anyhow::Error) -> String {
    error
        .chain()
        .last()
        .map_or_else(|| error.to_string(), ToString::to_string)
}

async fn execute(
    home: &Path,
    options: &Options,
    tx: &mpsc::UnboundedSender<Msg>,
    effect: Effect,
) -> Result<Outcome> {
    let account_name = |id: &str| -> Result<String> {
        Ok(AccountStore::load(home)?
            .get(id)
            .context("the account no longer exists")?
            .name
            .clone())
    };
    let profile = |id: &str| -> Result<Profile> {
        ProfileStore::load(home)?
            .get(id)
            .cloned()
            .context("the profile no longer exists")
    };
    Ok(match effect {
        Effect::Quit | Effect::Cancel | Effect::VendorAuth { .. } => Outcome::default(),
        Effect::Pair { server, again } => {
            let previous = if again { state::load(home).ok() } else { None };
            pair(home, &server, previous, options, tx).await?;
            Outcome::say(if again {
                format!("Signed in to {server} again.")
            } else {
                format!("Signed in to {server}.")
            })
        }
        Effect::SwitchServer { server } => {
            let old = state::load(home)?.server;
            super::super::disconnect(home).await?;
            pair(home, &server, None, options, tx)
                .await
                .with_context(|| format!("disconnected from {old}, but pairing failed"))?;
            Outcome::say(format!(
                "Disconnected from {old} and signed in to {server}."
            ))
        }
        Effect::Disconnect => {
            let old = state::load(home)?.server;
            super::super::disconnect(home).await?;
            Outcome {
                message: format!("Disconnected from {old}; this machine's access was revoked."),
                focus: Some(Item::Server),
                then: None,
            }
        }
        Effect::SetRoutes(selected) => {
            super::super::configure_with(home, |config, available| {
                wizard::set_routes(config, available, &selected)
            })
            .await?;
            Outcome::say("Routes saved.")
        }
        Effect::ApplyDefaults(defaults) => {
            super::super::configure_with(home, |config, available| {
                wizard::configure(config, available, &defaults)
            })
            .await?;
            Outcome::say("Applied the --provider choices.")
        }
        Effect::CreateAccount {
            vendor,
            name,
            native,
            route,
        } => {
            let flag = format!("--{}", account_store::product(vendor));
            let retry = format!("ags account new {name} {flag}");
            let account =
                account_cmd::create(home, vendor, &name, native, Some(&route), &retry).await?;
            let product = product_label(vendor);
            let then = (!native).then(|| Confirm {
                prompt: format!("Sign in to {name} now?"),
                effect: Effect::VendorAuth {
                    account: account.id.clone(),
                    logout: false,
                },
            });
            Outcome {
                message: if native {
                    format!("Added {name}, using this machine's {product} sign-in.")
                } else {
                    format!("Added {product} account {name} on route {route}.")
                },
                focus: Some(Item::Account(account.id)),
                then,
            }
        }
        Effect::RenameAccount { account, name } => {
            let old = account_name(&account)?;
            account_cmd::rename(home, &old, &name)?;
            Outcome::say(format!("Renamed {old} to {name}."))
        }
        Effect::RemoveAccount { account } => {
            let account = AccountStore::load(home)?
                .get(&account)
                .cloned()
                .context("the account no longer exists")?;
            let detached = account_cmd::delete(home, &account, true).await?;
            let mut message = format!("Removed {}.", account.name);
            if !detached.is_empty() {
                message.push_str(&format!(" Detached it from {}.", detached.join(", ")));
            }
            Outcome::say(message)
        }
        Effect::SetAccount {
            profile: id,
            vendor,
            account,
        } => {
            let name = profile(&id)?.name;
            ProfileStore::update(home, |s| s.set_account(&id, vendor, account.as_deref()))?;
            let product = account_store::product(vendor);
            Outcome::say(match account {
                Some(account) => {
                    format!("{name} now uses {} for {product}.", account_name(&account)?)
                }
                None => format!("{name} no longer has a {product} account."),
            })
        }
        Effect::ToggleDefault { profile: id } => {
            let name = profile(&id)?.name;
            let now = ProfileStore::update(home, |s| {
                s.default = if s.default.as_deref() == Some(id.as_str()) {
                    None
                } else {
                    Some(id.clone())
                };
                Ok(s.default.is_some())
            })?;
            Outcome::say(if now {
                format!("{name} is now the default profile.")
            } else {
                format!("{name} is no longer the default; no profile applies by default.")
            })
        }
        Effect::ToggleHere { profile: id } => {
            let name = profile(&id)?.name;
            let here = Here::current()?;
            let set = ProfileStore::update(home, |s| {
                let had = s
                    .rules
                    .iter()
                    .any(|r| r.path == here.key && r.profile == id);
                if had {
                    s.clear_rule(&here.key);
                } else {
                    s.set_rule(&here.key, &id);
                }
                Ok(!had)
            })?;
            Outcome::say(if set {
                format!("{name} now applies under {}.", here.display)
            } else {
                format!("{name} no longer applies under {}.", here.display)
            })
        }
        Effect::RenameProfile { profile: id, name } => {
            let old = profile(&id)?.name;
            profile_cmd::rename(home, &old, &name)?;
            Outcome::say(format!("Renamed {old} to {name}."))
        }
        Effect::RemoveProfile { profile: id } => {
            let name = profile(&id)?.name;
            profile_cmd::remove(home, &name).await?;
            Outcome::say(format!("Removed profile {name}."))
        }
        Effect::CreateProfile { name } => {
            let created = Profile::new(&name);
            ProfileStore::update(home, |s| s.add(created.clone()))?;
            Outcome {
                message: format!("Created {name}; choose its accounts."),
                focus: Some(Item::Product(created.id, account_store::VENDORS[0].0)),
                then: None,
            }
        }
    })
}

/// Pairs with `server` (as the client of `previous` when signing in again there), keeps
/// `previous`'s routes or selects them, grants them, and saves the connection.
async fn pair(
    home: &Path,
    server: &str,
    previous: Option<state::Config>,
    options: &Options,
    tx: &mpsc::UnboundedSender<Msg>,
) -> Result<()> {
    state::write_private(&home.join("server"), server.as_bytes(), false)?;
    let previous = previous.filter(|c| c.server == server);
    let reuse = previous.as_ref().map(|c| c.client_id.clone());
    let device = tx.clone();
    let mut config = auth::pair_with(
        server,
        !options.no_browser,
        reuse.as_deref(),
        |url, code| {
            let _ = device.send(Msg::Device(url.to_owned(), code.to_owned()));
        },
    )
    .await?;
    match previous.filter(|c| !c.providers.is_empty()) {
        Some(old) => {
            config.providers = old.providers;
            config.provider_defaults = old.provider_defaults;
            config.cli_overrides = old.cli_overrides;
            config.defaults = old.defaults;
        }
        None => {
            let available = auth::catalog(&config).await?.providers;
            if options.defaults.is_empty() {
                let all: Vec<String> = wizard::supported(&available)
                    .into_iter()
                    .map(|p| p.id)
                    .collect();
                wizard::set_routes(&mut config, &available, &all)?;
            } else {
                wizard::configure(&mut config, &available, &options.defaults)?;
            }
        }
    }
    auth::enroll(&config).await?;
    let lock_home = home.to_owned();
    let _lock = tokio::task::spawn_blocking(move || state::lock(&lock_home)).await??;
    state::save(home, &config)?;
    Ok(())
}
