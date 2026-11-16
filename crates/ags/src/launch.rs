//! Picks the provider a CLI runs on, and runs it through its own proxy.
use std::{
    ffi::{OsStr, OsString},
    process::ExitStatus,
};

use aegis_wire::Machine;
use anyhow::{Context, Result, anyhow, bail};
use tokio::process::Command;

use clap::ValueEnum;

use crate::{
    Cli, Credentials, LoggedOut, Login, Store, current_login, defaults, forget, proxy::Proxy,
    server::Server,
};

/// A provider granted to this machine that a CLI can run on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub id: String,
    pub provider_type: String,
}

pub enum Pick {
    Use(Offer),
    /// Several providers fit and none is remembered.
    Ask(Vec<Offer>),
}

/// A CLI's remembered provider; `None` asks on its first launch.
pub struct CliDefault {
    pub cli: Cli,
    pub provider: Option<String>,
    /// `None` when the provider is no longer granted to this machine.
    pub provider_type: Option<String>,
}

pub(crate) fn not_logged_in() -> anyhow::Error {
    anyhow!("not logged in; run `ags login <URL>` first")
}

pub(crate) async fn granted(store: &Store) -> Result<Option<(Login, Machine)>> {
    let Some(login) = current_login(store).await? else {
        return Ok(None);
    };
    let server = Server::discover(&login.server).await?;
    match server.machine(&login.access_token).await? {
        Some(machine) => Ok(Some((login, machine))),
        None => Err(forget(store, &login).await),
    }
}

fn offers(machine: &Machine, cli: Cli) -> Vec<Offer> {
    machine
        .providers
        .iter()
        .filter_map(|id| {
            let provider_type = machine.provider_types.get(id)?;
            cli.runs_on(provider_type).then(|| Offer {
                id: id.clone(),
                provider_type: provider_type.clone(),
            })
        })
        .collect()
}

fn offer(machine: &Machine, server: &str, cli: Cli, id: &str) -> Result<Offer> {
    if !machine.providers.iter().any(|granted| granted == id) {
        bail!(
            "{id} is not granted to this machine; run `ags logout`, then `ags login {server}` to grant it"
        );
    }
    let Some(provider_type) = machine.provider_types.get(id) else {
        bail!("{server} no longer has a provider named {id}");
    };
    if !cli.runs_on(provider_type) {
        bail!("{cli} can't run on {id}, a {provider_type} provider");
    }
    Ok(Offer {
        id: id.to_owned(),
        provider_type: provider_type.clone(),
    })
}

/// The provider for one launch of `cli`: `requested`, else the remembered one, else the only fit.
pub async fn pick(store: &Store, cli: Cli, requested: Option<&str>) -> Result<Pick> {
    let (login, machine) = granted(store).await?.ok_or_else(not_logged_in)?;
    if let Some(id) = requested {
        return offer(&machine, &login.server, cli, id).map(Pick::Use);
    }
    let mut offers = offers(&machine, cli);
    // A remembered provider that no longer fits is asked about again.
    if let Some(remembered) = defaults::load(store.dir())?.get(&cli.to_string())
        && let Some(index) = offers.iter().position(|offer| &offer.id == remembered)
    {
        return Ok(Pick::Use(offers.swap_remove(index)));
    }
    match offers.len() {
        0 => bail!(
            "this machine has no provider that {cli} can run on ({}); run `ags logout`, then `ags login {}` to grant one",
            cli.provider_types().join(" or "),
            login.server
        ),
        1 => Ok(Pick::Use(offers.remove(0))),
        _ => Ok(Pick::Ask(offers)),
    }
}

/// Remembers a provider that `pick` offered.
pub fn remember(store: &Store, cli: Cli, provider: &str) -> Result<()> {
    defaults::set(store.dir(), cli, provider)
}

/// `ags use <cli> <provider>`: checks the provider, then remembers it.
pub async fn use_provider(store: &Store, cli: Cli, provider: &str) -> Result<()> {
    let (login, machine) = granted(store).await?.ok_or_else(not_logged_in)?;
    offer(&machine, &login.server, cli, provider)?;
    remember(store, cli, provider)
}

/// Every CLI's remembered provider; `None` when this machine is not logged in.
pub async fn cli_defaults(store: &Store) -> Result<Option<Vec<CliDefault>>> {
    let Some((_, machine)) = granted(store).await? else {
        return Ok(None);
    };
    let remembered = defaults::load(store.dir())?;
    Ok(Some(
        Cli::value_variants()
            .iter()
            .map(|&cli| {
                let provider = remembered.get(&cli.to_string()).cloned();
                let provider_type = provider
                    .as_ref()
                    .filter(|id| machine.providers.contains(id))
                    .and_then(|id| machine.provider_types.get(id))
                    .cloned();
                CliDefault {
                    cli,
                    provider,
                    provider_type,
                }
            })
            .collect(),
    ))
}

/// How a launched CLI ended.
pub struct Launched {
    pub status: ExitStatus,
    /// The server logged this machine out during the launch, and its login is forgotten.
    pub logged_out: Option<anyhow::Error>,
}

/// Runs `cli` with `args` on `offer`, in the provider's own config dir, until it exits.
/// `program` replaces the CLI's name on `PATH`.
pub async fn launch(
    store: &Store,
    credentials: &Credentials,
    cli: Cli,
    offer: &Offer,
    args: Vec<OsString>,
    program: Option<&OsStr>,
) -> Result<Launched> {
    let login = current_login(store).await?.ok_or_else(not_logged_in)?;
    let server = login.server.clone();
    let isolation = credentials.isolate(&offer.id, &offer.provider_type, cli)?;
    // A random UUID carries nothing about the launch, such as when it started.
    let launch_id = uuid::Uuid::new_v4().to_string();
    let proxy = Proxy::start(
        store.dir(),
        login,
        &offer.id,
        cli.base_path(&offer.provider_type),
        &launch_id,
    )
    .await?;
    let setup = cli.setup(
        &offer.provider_type,
        proxy.base_url(),
        proxy.secret(),
        store.dir(),
        |name| std::env::var(name).ok(),
    )?;
    let name = OsString::from(cli.to_string());
    let program = program.unwrap_or(&name);
    let mut command = Command::new(program);
    command
        .args(isolation.args)
        .args(setup.args)
        .args(args)
        .envs(isolation.env)
        .envs(setup.env)
        .envs(no_proxy(|name| std::env::var(name).ok()))
        .kill_on_drop(true);
    for name in isolation.unset.iter().chain(setup.unset) {
        command.env_remove(name);
    }
    die_with_parent(&mut command);
    let status = wait(command, program).await;
    let ended = proxy.end();
    let status = status?;
    let logged_out = match ended.logged_out {
        Some(error) => Some(error),
        None if ended.refused => logged_out_of(store, &server).await,
        None => None,
    };
    Ok(Launched { status, logged_out })
}

/// After a 401, which a vendor may also send: whether the server still accepts this machine.
async fn logged_out_of(store: &Store, server: &str) -> Option<anyhow::Error> {
    let login = store.load().ok()??;
    if login.server != server {
        return None;
    }
    match granted(store).await {
        Err(error) if error.is::<LoggedOut>() => Some(error),
        _ => None,
    }
}

/// The CLI reaches the proxy directly, even when told to use an HTTP proxy.
pub(crate) fn no_proxy(inherited: impl Fn(&str) -> Option<String>) -> [(&'static str, String); 2] {
    let names = ["NO_PROXY", "no_proxy"];
    let inherited: Vec<String> = names.iter().filter_map(|name| inherited(name)).collect();
    let mut hosts: Vec<String> = Vec::new();
    for host in inherited
        .iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .chain(["127.0.0.1", "localhost"])
    {
        if !host.is_empty() && !hosts.iter().any(|known| known == host) {
            hosts.push(host.to_owned());
        }
    }
    let value = hosts.join(",");
    names.map(|name| (name, value.clone()))
}

/// The CLI ends if ags is killed, rather than running on without its proxy.
#[cfg(target_os = "linux")]
fn die_with_parent(command: &mut Command) {
    let parent = libc::pid_t::try_from(std::process::id()).unwrap_or(0);
    // SAFETY: the closure makes only async-signal-safe calls and does not allocate.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // ags may have died before the signal was armed.
            if libc::getppid() != parent {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn die_with_parent(_command: &mut Command) {}

pub(crate) fn spawn(mut command: Command, program: &OsStr) -> Result<tokio::process::Child> {
    command.spawn().map_err(|error| {
        let program = program.to_string_lossy();
        if error.kind() == std::io::ErrorKind::NotFound {
            anyhow!("could not find {program}; install it or add it to PATH")
        } else {
            anyhow::Error::new(error).context(format!("could not start {program}"))
        }
    })
}

/// The terminal sends interrupts to the CLI as well; ags outlives them to keep the proxy up.
#[cfg(unix)]
pub(crate) async fn wait(command: Command, program: &OsStr) -> Result<ExitStatus> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut quit = signal(SignalKind::quit())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let mut child = spawn(command, program)?;
    let pass = |child: &tokio::process::Child, signal: libc::c_int| {
        if let Some(pid) = child.id().and_then(|pid| libc::pid_t::try_from(pid).ok()) {
            // SAFETY: kill only sends a signal to the child, which this process has not reaped.
            unsafe { libc::kill(pid, signal) };
        }
    };
    loop {
        tokio::select! {
            status = child.wait() => return status.context("waiting for the CLI"),
            _ = interrupt.recv() => {}
            _ = quit.recv() => {}
            _ = terminate.recv() => pass(&child, libc::SIGTERM),
            _ = hangup.recv() => pass(&child, libc::SIGHUP),
        }
    }
}

#[cfg(not(unix))]
pub(crate) async fn wait(command: Command, program: &OsStr) -> Result<ExitStatus> {
    let mut child = spawn(command, program)?;
    loop {
        tokio::select! {
            status = child.wait() => return status.context("waiting for the CLI"),
            interrupted = tokio::signal::ctrl_c() => interrupted?,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cli_reaches_loopback_directly_whichever_spelling_its_proxy_uses() {
        let both = no_proxy(|name| {
            Some(match name {
                "NO_PROXY" => "corp.example, 127.0.0.1".to_owned(),
                _ => "corp.example,.internal".to_owned(),
            })
        });
        let expected = "corp.example,127.0.0.1,.internal,localhost".to_owned();
        assert_eq!(
            both,
            [("NO_PROXY", expected.clone()), ("no_proxy", expected)]
        );
        let none = no_proxy(|_| None);
        assert_eq!(none[0].1, "127.0.0.1,localhost");
    }
}
