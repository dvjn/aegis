mod agy;
mod auth;
mod install;
mod relay;
mod pi;
mod profiles;
mod state;
mod update;
mod wizard;

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
    /// Pair this machine, select subscriptions, and configure defaults and CLI overrides.
    #[command(aliases = ["connect", "login"])]
    Setup {
        server: Option<String>,
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
    Run {
        #[arg(value_parser=["claude", "codex", "pi", "agy"])]
        client: String,
        #[arg(long)]
        provider: Option<String>,
        #[arg(last = true)]
        args: Vec<OsString>,
    },
    Status,
    Doctor,
    /// Install the server's compatible release, retaining the previous executable.
    Update {
        #[arg(long)]
        rollback: bool,
    },
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

pub async fn main() -> Result<i32> {
    wizard::init_theme();
    let cli = Cli::parse();
    let home = state::home()?;
    match cli.command {
        Command::Setup {
            server,
            no_wrappers,
            no_browser,
            defaults,
        } => {
            setup(&home, server, no_wrappers, no_browser, &defaults, false).await?;
        }
        Command::Configure { defaults } => configure(&home, &defaults).await?,
        Command::Run {
            client,
            provider,
            args,
        } => return run(&home, &client, provider.as_deref(), args).await,
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
                }
            }
            install::remove(&home)?;
            println!("AGS wrappers and managed PATH entry removed.");
        }
    }
    Ok(0)
}

async fn setup(
    home: &Path,
    server: Option<String>,
    no_wrappers: bool,
    no_browser: bool,
    defaults: &[String],
    first_launch: bool,
) -> Result<()> {
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
        let mut config = auth::pair(&server, !no_browser).await?;
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
        install::wrappers(home)?;
    }
    if !first_launch {
        println!(
            "Connected. Run claude, codex, pi, or agy through the wrappers, or use ags run <cli> -- <args>."
        );
    }
    Ok(())
}

fn prompt(message: &str) -> Result<String> {
    Ok(cliclack::input(message).interact()?)
}

async fn configure(home: &Path, defaults: &[String]) -> Result<()> {
    let mut config = auth::connected(home).await?;
    let available = auth::catalog(&config).await?.providers;
    wizard::configure(&mut config, &available, defaults)?;
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
    println!("Disconnected. Server access was revoked.");
    Ok(())
}

async fn run(
    home: &Path,
    client: &str,
    selected: Option<&str>,
    args: Vec<OsString>,
) -> Result<i32> {
    let executable = profiles::resolve(client, home)?;
    if !home.join("connection.json").exists() {
        setup(home, None, true, false, &[], true).await?;
    }
    let config = auth::connected(home).await?;
    let token_source = auth::TokenSource::new(home);
    let mut relays = Vec::new();
    let plan = if client == "pi" {
        let mut endpoints = BTreeMap::new();
        for provider in &config.providers {
            if !profiles::compatible("pi", provider) {
                continue;
            }
            let relay =
                relay::Relay::start(&config.server, &provider.id, token_source.clone()).await?;
            endpoints.insert(provider.id.clone(), relay.url.clone());
            relays.push(relay);
        }
        pi::plan(&config, &endpoints, selected, args, home)?
    } else {
        let kind = profiles::kind(client).context("unsupported CLI")?;
        let id = selected
            .or_else(|| config.provider_for(client, kind))
            .context("no default provider; run ags configure")?;
        let provider = config
            .providers
            .iter()
            .find(|p| p.id == id)
            .context("provider is not enabled; run ags configure")?;
        let relay = relay::Relay::start(&config.server, &provider.id, token_source).await?;
        let plan = profiles::plan(client, provider, &relay.url, args, home)?;
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
