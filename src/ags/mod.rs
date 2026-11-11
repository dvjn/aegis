mod account_cmd;
mod account_store;
mod agy;
mod auth;
mod install;
mod isolation;
mod migrate;
mod pi;
mod profile_cmd;
mod profile_store;
mod profiles;
mod prompt;
mod relay;
mod resolve;
mod state;
mod tui;
mod ui;
mod update;
mod wizard;

use account_store::AccountStore;
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::{collections::BTreeMap, ffi::OsString, fs, path::Path};

#[derive(Parser)]
#[command(
    name = "ags",
    version,
    about = "Run coding CLIs through Aegis with existing provider sign-ins"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Pair this machine with Aegis, then set up accounts and profiles (on a terminal, a
    /// full-screen setup that can be re-run any time).
    #[command(aliases = ["connect", "login"])]
    Setup {
        server: Option<String>,
        /// Open the setup screen on this tab.
        #[arg(long, value_enum)]
        step: Option<SetupStep>,
        #[arg(long)]
        no_wrappers: bool,
        #[arg(long)]
        no_browser: bool,
        /// Set a default without prompting, e.g. --provider codex=work.
        #[arg(long = "provider", value_name = "CLI=ID")]
        defaults: Vec<String>,
    },
    /// Select subscriptions, provider defaults, and optional CLI overrides.
    #[command(alias = "config")]
    Configure {
        #[arg(long = "provider", value_name = "CLI=ID")]
        defaults: Vec<String>,
    },
    /// Run one CLI instance with its own private relay.
    #[command(visible_aliases = ["r"])]
    Run {
        #[arg(value_parser=["claude", "codex", "pi", "agy"])]
        client: String,
        #[arg(long)]
        provider: Option<String>,
        /// Launch with a profile (its accounts' sign-ins and usage attribution).
        #[arg(short = 'P', long, conflicts_with = "provider")]
        profile: Option<String>,
        #[arg(last = true)]
        args: Vec<OsString>,
    },
    /// Profiles group accounts per product and apply by directory. Without a subcommand,
    /// shows the profile active here.
    #[command(visible_aliases = ["p"])]
    Profile {
        #[command(subcommand)]
        command: Option<profile_cmd::ProfileCommand>,
    },
    /// Manage accounts: one claude or chatgpt sign-in each.
    #[command(visible_aliases = ["a"])]
    Account {
        #[command(subcommand)]
        command: account_cmd::AccountCommand,
    },
    /// Show the server connection, routes, and the profile active here
    #[command(visible_aliases = ["st"])]
    Status,
    /// Same as status
    Doctor,
    /// Install the server's compatible release, retaining the previous executable.
    Update {
        #[arg(long)]
        rollback: bool,
    },
    /// Revoke this installation in Aegis and remove its local connection
    Disconnect {
        /// Forget local credentials when the server is unreachable; revoke separately in Aegis.
        #[arg(long)]
        offline: bool,
    },
    /// Remove wrappers and this installation; revokes access unless --offline is explicit.
    Uninstall {
        #[arg(long)]
        offline: bool,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum SetupStep {
    Aegis,
    Accounts,
    Profiles,
}

pub async fn main() -> Result<i32> {
    prompt::init();
    let cli = Cli::parse();
    let home = state::home()?;
    migrate::run(&home)?;
    match cli.command {
        Command::Setup {
            server,
            step,
            no_wrappers,
            no_browser,
            defaults,
        } => {
            if account_cmd::tty() {
                wizard_setup(&home, server, step, no_wrappers, no_browser, defaults).await?;
            } else {
                let options = SetupOptions {
                    no_wrappers,
                    no_browser,
                    first_launch: false,
                };
                setup(&home, server, &options, &defaults).await?;
            }
        }
        Command::Configure { defaults } => configure(&home, &defaults).await?,
        Command::Run {
            client,
            provider,
            profile,
            args,
        } => {
            return run(
                &home,
                &client,
                provider.as_deref(),
                profile.as_deref(),
                args,
            )
            .await;
        }
        Command::Profile { command } => return profile_cmd::dispatch(&home, command).await,
        Command::Account { command } => return account_cmd::dispatch(&home, command).await,
        Command::Status | Command::Doctor => {
            let config = auth::connected(&home).await?;
            let current = auth::catalog(&config).await?;
            println!("Server: {}\nConnection: active", config.server);
            for cli in profiles::CLIENTS {
                let kind = profiles::kind(cli).unwrap_or("");
                let providers = if *cli == "pi" {
                    config
                        .providers
                        .iter()
                        .filter(|p| profiles::compatible("pi", p))
                        .map(|p| format!("{} (ags-{})", p.id, p.id))
                        .collect::<Vec<_>>()
                        .join(", ")
                } else {
                    config
                        .provider_for(cli, kind)
                        .unwrap_or("not configured")
                        .to_owned()
                };
                let executable = profiles::resolve(cli, &home)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "not installed".into());
                println!("{cli}: {providers} ({executable})");
            }
            profile_cmd::show(&home)?;
            for provider in &config.providers {
                if !current
                    .providers
                    .iter()
                    .any(|p| p.id == provider.id && p.kind == provider.kind)
                {
                    println!(
                        "Provider {} is unavailable; run ags configure.",
                        provider.id
                    );
                }
            }
            let shims = home.join("shims");
            if !std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .any(|p| p == shims)
            {
                println!("Wrapper directory is not in PATH: {}", shims.display());
            }
            println!(
                "Shell aliases/functions and absolute executable paths can bypass PATH wrappers."
            );
        }
        Command::Disconnect { offline } => {
            if offline {
                let _lock = state::lock(&home)?;
                fs::remove_file(home.join("connection.json"))?;
                eprintln!("Local connection removed. Revoke the AGS client in Aegis when online.");
            } else {
                disconnect(&home).await?;
                println!("Disconnected. Server access was revoked.");
            }
        }
        Command::Update { rollback } => update::update(&home, rollback).await?,
        Command::Uninstall { offline } => {
            if home.join("connection.json").exists() {
                if offline {
                    fs::remove_file(home.join("connection.json"))?;
                    eprintln!(
                        "Local connection removed. Revoke the AGS client in Aegis when online."
                    );
                } else {
                    disconnect(&home).await?;
                    println!("Disconnected. Server access was revoked.");
                }
            }
            install::remove(&home)?;
            println!("AGS wrappers and managed PATH entry removed.");
        }
    }
    Ok(0)
}

const CONNECTED: &str = "Connected. Run claude, codex, pi, or agy through the wrappers, or use ags run <cli> -- <args>.";

/// `ags setup` on a terminal: the full-screen setup, then wrappers once paired.
async fn wizard_setup(
    home: &Path,
    server: Option<String>,
    step: Option<SetupStep>,
    no_wrappers: bool,
    no_browser: bool,
    defaults: Vec<String>,
) -> Result<()> {
    let was_paired = home.join("connection.json").exists();
    let tab = match step {
        Some(SetupStep::Accounts) => tui::Tab::Accounts,
        Some(SetupStep::Profiles) => tui::Tab::Profiles,
        Some(SetupStep::Aegis) | None => tui::Tab::Aegis,
    };
    let options = tui::Options {
        server,
        no_browser,
        defaults,
    };
    tui::run(home, tab, options).await?;
    if home.join("connection.json").exists() {
        let first = !was_paired;
        if !no_wrappers {
            install::wrappers(home, first)?;
        }
        if first {
            println!("{CONNECTED}");
        }
    }
    Ok(())
}

struct SetupOptions {
    no_wrappers: bool,
    no_browser: bool,
    /// The implicit setup before a first launch: no chatter.
    first_launch: bool,
}

/// The scripted setup: without a terminal, and before a first launch.
async fn setup(
    home: &Path,
    server: Option<String>,
    options: &SetupOptions,
    defaults: &[String],
) -> Result<()> {
    let SetupOptions {
        no_wrappers,
        no_browser,
        first_launch,
    } = *options;
    let lock_home = home.to_owned();
    let lock = tokio::task::spawn_blocking(move || state::lock(&lock_home)).await??;
    let mut old = if home.join("connection.json").exists() {
        Some(state::load(home)?)
    } else {
        None
    };
    let server = match server
        .or_else(|| old.as_ref().map(|c| c.server.clone()))
        .or_else(|| std::env::var("AGS_SERVER_URL").ok())
        .or_else(|| fs::read_to_string(home.join("server")).ok())
    {
        Some(server) => state::validate_server(&server)?,
        None => state::validate_server(&prompt("Aegis server URL: ")?)?,
    };
    if old.as_ref().is_some_and(|c| c.server != server) {
        bail!("disconnect the existing server before pairing another one");
    }
    state::write_private(&home.join("server"), server.as_bytes(), false)?;
    let reusable = if let Some(config) = old.as_mut() {
        let validation = async {
            if auth::renew(config).await? {
                state::save(home, config)?;
            }
            auth::catalog(config).await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        match validation {
            Ok(()) => true,
            Err(error) if error.downcast_ref::<auth::LoginRequired>().is_some() => {
                eprintln!("Aegis login expired or was revoked. Pairing again.");
                false
            }
            Err(error) => return Err(error),
        }
    } else {
        false
    };
    if reusable {
        drop(lock);
        if !first_launch {
            configure(home, defaults).await?;
        }
    } else {
        // Signing in again under the same client keeps its registered accounts and profiles.
        let reuse = old.as_ref().map(|c| c.client_id.as_str());
        let mut config = auth::pair(&server, !no_browser, reuse).await?;
        if let Some(old) = old {
            config.providers = old.providers;
            config.provider_defaults = old.provider_defaults;
            config.cli_overrides = old.cli_overrides;
            config.defaults = old.defaults;
        }
        let available = auth::catalog(&config).await?.providers;
        wizard::configure(&mut config, &available, defaults)?;
        auth::enroll(&config).await?;
        state::save(home, &config)?;
        drop(lock);
    }
    if !no_wrappers {
        install::wrappers(home, true)?;
    }
    if !first_launch {
        println!("{CONNECTED}");
        if AccountStore::load(home)?.accounts.is_empty() {
            println!("Run `ags setup` in a terminal to set up accounts and profiles.");
        }
    }
    Ok(())
}

fn prompt(message: &str) -> Result<String> {
    use prompt::Ask;
    Ok(demand::Input::new(message).ask()?)
}

/// `ags configure`.
async fn configure(home: &Path, defaults: &[String]) -> Result<()> {
    configure_with(home, |config, available| {
        wizard::configure(config, available, defaults)
    })
    .await
}

/// Changes the provider selection with `choose`, then grants it on the server and saves it.
async fn configure_with(
    home: &Path,
    choose: impl FnOnce(&mut state::Config, &[crate::ags_wire::Provider]) -> Result<()>,
) -> Result<()> {
    let mut config = auth::connected(home).await?;
    let available = auth::catalog(&config).await?.providers;
    choose(&mut config, &available)?;
    // Refresh again after user interaction, before changing server grants.
    let _ = auth::connected(home).await?;
    let lock_home = home.to_owned();
    let _lock = tokio::task::spawn_blocking(move || state::lock(&lock_home)).await??;
    let mut latest = state::load(home)?;
    if latest.client_id != config.client_id || latest.server != config.server {
        bail!("connection changed while configuring; try again");
    }
    latest.providers = config.providers;
    latest.provider_defaults = config.provider_defaults;
    latest.cli_overrides = config.cli_overrides;
    latest.defaults = config.defaults;
    auth::enroll(&latest).await?;
    state::save(home, &latest)?;
    Ok(())
}

async fn disconnect(home: &Path) -> Result<()> {
    let config = auth::connected(home).await?;
    auth::disconnect(&config).await?;
    let _lock = state::lock(home)?;
    let latest = state::load(home)?;
    if latest.client_id != config.client_id || latest.server != config.server {
        bail!("connection changed while disconnecting; the newer connection was preserved");
    }
    fs::remove_file(home.join("connection.json"))?;
    Ok(())
}

async fn run(
    home: &Path,
    client: &str,
    selected: Option<&str>,
    profile: Option<&str>,
    args: Vec<OsString>,
) -> Result<i32> {
    let executable = profiles::resolve(client, home)?;
    if !home.join("connection.json").exists() {
        let options = SetupOptions {
            no_wrappers: true,
            no_browser: false,
            first_launch: true,
        };
        setup(home, None, &options, &[]).await?;
    }
    let config = auth::connected(home).await?;
    let token_source = auth::TokenSource::new(home);
    let mut relays = Vec::new();
    let profiled = account_store::vendor_for_cli(client).is_some();
    if !profiled && profile.is_some() {
        bail!("{client} does not support profiles; run it without --profile");
    }
    let launched = if profiled {
        profile_cmd::prepare_launch(home, &config, client, profile, selected.is_some()).await?
    } else {
        None
    };
    let plan = if client == "pi" {
        let mut endpoints = BTreeMap::new();
        for provider in &config.providers {
            if !profiles::compatible("pi", provider) {
                continue;
            }
            let relay =
                relay::Relay::start(&config.server, &provider.id, token_source.clone(), None)
                    .await?;
            endpoints.insert(provider.id.clone(), relay.url.clone());
            relays.push(relay);
        }
        pi::plan(&config, &endpoints, selected, args, home)?
    } else {
        let kind = profiles::kind(client).context("unsupported CLI")?;
        let id = match &launched {
            Some(l) => l.account.provider.as_str(),
            None => selected
                .or_else(|| config.provider_for(client, kind))
                .context("no default provider; run ags configure")?,
        };
        let provider =
            config
                .providers
                .iter()
                .find(|p| p.id == id)
                .with_context(|| match &launched {
                    Some(l) => format!(
                        "account {} uses route {id}, which is no longer granted; run ags configure",
                        l.account.name
                    ),
                    None => "provider is not enabled; run ags configure".to_owned(),
                })?;
        let attribution = launched.as_ref().and_then(|l| l.attribution.clone());
        let relay =
            relay::Relay::start(&config.server, &provider.id, token_source, attribution).await?;
        let mut plan = profiles::plan(client, provider, &relay.url, args, home)?;
        if let Some(l) = &launched {
            plan.env.extend(isolation::env(&l.account, home)?);
            isolation::splice(&mut plan.args, isolation::extra_args(&l.account));
        }
        relays.push(relay);
        plan
    };
    let mut command = tokio::process::Command::new(executable);
    command
        .args(&plan.args)
        .envs(plan.env.iter().map(|(k, v)| (k, v)))
        .kill_on_drop(true);
    // Register before spawning: normal terminal SIGINT reaches both processes.
    #[cfg(unix)]
    let (mut interrupt, mut terminate, mut hangup) = {
        use tokio::signal::unix::{SignalKind, signal};
        (
            signal(SignalKind::interrupt())?,
            signal(SignalKind::terminate())?,
            signal(SignalKind::hangup())?,
        )
    };
    let mut child = command.spawn().context("could not launch vendor CLI")?;
    #[cfg(unix)]
    let status = loop {
        tokio::select! {
            status = child.wait() => break status?,
            _ = interrupt.recv() => {}, // Child shares the terminal foreground group.
            _ = terminate.recv() => {
                if let Some(pid) = child.id() { unsafe { libc::kill(pid as i32, libc::SIGTERM); } }
                break match tokio::time::timeout(std::time::Duration::from_secs(10), child.wait()).await {
                    Ok(status) => status?, Err(_) => { child.kill().await?; child.wait().await? }
                };
            },
            _ = hangup.recv() => {
                if let Some(pid) = child.id() { unsafe { libc::kill(pid as i32, libc::SIGHUP); } }
            },
        }
    };
    #[cfg(not(unix))]
    let status = child.wait().await?;
    drop(plan);
    drop(relays);
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        Ok(status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)))
    }
    #[cfg(not(unix))]
    {
        Ok(status.code().unwrap_or(1))
    }
}

#[cfg(test)]
mod tests;
