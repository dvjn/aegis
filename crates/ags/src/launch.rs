//! Picks the provider a CLI runs on, and runs it through its own proxy.
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fmt,
    process::ExitStatus,
};

use aegis_wire::{Machine, provider_type::VERTEX};
use anyhow::{Result, anyhow, bail};
use tokio::process::Command;

use clap::ValueEnum;

use crate::{
    Cli, Credentials, LoggedOut, Login, Store,
    clis::{Upstream, stray_model_regions},
    current_login,
    defaults::{self, Remembered},
    forget, pi,
    proxy::{Proxy, Route},
    server::Server,
    shims::{self, Entered},
};

/// A provider granted to this machine that a CLI can run on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub id: String,
    pub provider_type: String,
    /// A Bedrock provider's AWS region or a Vertex provider's location, when the server names one.
    pub region: Option<String>,
}

impl Offer {
    fn of(machine: &Machine, id: &str, provider_type: &str) -> Self {
        Self {
            id: id.to_owned(),
            provider_type: provider_type.to_owned(),
            region: machine
                .provider_regions
                .as_ref()
                .and_then(|regions| regions.get(id))
                .or_else(|| machine.provider_locations.get(id))
                .cloned(),
        }
    }

    /// Why `cli` can't run on this provider of `machine`, with `path` as the value of `PATH`.
    fn refusal(&self, machine: &Machine, cli: Cli, path: Option<&OsStr>) -> Option<String> {
        cli.refusal(
            &self.id,
            &self.provider_type,
            self.region.as_deref(),
            machine.provider_regions.is_some(),
            path,
        )
    }
}

pub enum Pick {
    Use(Offer),
    /// Several providers fit and none is remembered.
    Ask(Vec<Offer>),
}

/// A CLI's remembered provider; `None` asks on its first launch.
pub struct CliDefault {
    pub cli: Cli,
    /// The provider type this default is for, for pi, which keeps one per type.
    pub for_type: Option<String>,
    pub provider: Option<String>,
    /// `None` when the provider is no longer granted to this machine.
    pub provider_type: Option<String>,
    /// Whether this machine has a provider of `for_type` that the CLI can run on.
    pub granted: bool,
}

/// What pi's own provider of one type uses during a launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeDefault {
    Use(Offer),
    /// Several providers fit and none is remembered.
    Ask(Vec<Offer>),
    /// No provider of the type is granted.
    None,
}

/// The providers of one pi launch.
pub struct PiPlan {
    pub(crate) server: String,
    /// Every provider pi can use, each of which pi offers under its own ID.
    pub(crate) offers: Vec<Offer>,
    /// What pi's own provider of each type uses, in the order of pi's types.
    pub types: Vec<(String, TypeDefault)>,
    /// The provider named for this launch, which its type uses.
    pub requested: Option<Offer>,
    /// pi's remembered provider of each type.
    remembered: BTreeMap<String, String>,
}

impl PiPlan {
    #[cfg(test)]
    pub(crate) fn of(
        server: String,
        offers: Vec<Offer>,
        types: Vec<(String, TypeDefault)>,
        requested: Option<Offer>,
    ) -> Self {
        Self {
            server,
            offers,
            types,
            requested,
            remembered: BTreeMap::new(),
        }
    }

    /// Remembers `offer` as pi's provider of its type, and uses it for this launch.
    pub fn choose(&mut self, store: &Store, offer: Offer) -> Result<()> {
        self.remembered
            .insert(offer.provider_type.clone(), offer.id.clone());
        defaults::set(
            store.dir(),
            Cli::Pi,
            Remembered::ByType(self.remembered.clone()),
        )?;
        if let Some((_, default)) = self
            .types
            .iter_mut()
            .find(|(provider_type, _)| *provider_type == offer.provider_type)
        {
            *default = TypeDefault::Use(offer);
        }
        Ok(())
    }
}

pub(crate) fn not_logged_in() -> anyhow::Error {
    NotLoggedIn.into()
}

/// This machine is paired with no server.
#[derive(Debug)]
pub struct NotLoggedIn;

impl fmt::Display for NotLoggedIn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not logged in; run `ags login <URL>` first")
    }
}

impl std::error::Error for NotLoggedIn {}

/// No provider granted to this machine is one a CLI can run on.
#[derive(Debug)]
pub struct NoProvider {
    pub cli: Cli,
    pub server: String,
}

impl fmt::Display for NoProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cli = self.cli;
        write!(
            f,
            "this machine has no provider that {cli} can run on ({}); run `ags logout`, then `ags login {}` to grant one",
            cli.provider_types().join(" or "),
            self.server
        )
    }
}

impl std::error::Error for NoProvider {}

/// The CLI started, then ags could not wait for it to end.
#[derive(Debug)]
pub struct Started(std::io::Error);

impl fmt::Display for Started {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "waiting for the CLI: {}", self.0)
    }
}

impl std::error::Error for Started {}

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

fn offers(machine: &Machine, cli: Cli, path: Option<&OsStr>) -> Vec<Offer> {
    machine
        .providers
        .iter()
        .filter_map(|id| {
            let offer = Offer::of(machine, id, machine.provider_types.get(id)?);
            offer.refusal(machine, cli, path).is_none().then_some(offer)
        })
        .collect()
}

fn offer(
    machine: &Machine,
    server: &str,
    cli: Cli,
    id: &str,
    path: Option<&OsStr>,
) -> Result<Offer> {
    if !machine.providers.iter().any(|granted| granted == id) {
        bail!(
            "{id} is not granted to this machine; run `ags logout`, then `ags login {server}` to grant it"
        );
    }
    let Some(provider_type) = machine.provider_types.get(id) else {
        bail!("{server} no longer has a provider named {id}");
    };
    let offer = Offer::of(machine, id, provider_type);
    if let Some(refusal) = offer.refusal(machine, cli, path) {
        bail!(refusal);
    }
    Ok(offer)
}

fn path() -> Option<OsString> {
    std::env::var_os("PATH")
}

/// The provider for one launch of `cli`: `requested`, else the remembered one, else the only fit.
pub async fn pick(store: &Store, cli: Cli, requested: Option<&str>) -> Result<Pick> {
    let (login, machine) = granted(store).await?.ok_or_else(not_logged_in)?;
    if let Some(id) = requested {
        return offer(&machine, &login.server, cli, id, path().as_deref()).map(Pick::Use);
    }
    let mut offers = offers(&machine, cli, path().as_deref());
    // A remembered provider that no longer fits is asked about again.
    if let Some(Remembered::One(remembered)) = defaults::load(store.dir())?.get(&cli.to_string())
        && let Some(index) = offers.iter().position(|offer| &offer.id == remembered)
    {
        return Ok(Pick::Use(offers.swap_remove(index)));
    }
    match offers.len() {
        0 => Err(NoProvider {
            cli,
            server: login.server,
        }
        .into()),
        1 => Ok(Pick::Use(offers.remove(0))),
        _ => Ok(Pick::Ask(offers)),
    }
}

/// Remembers a provider that `pick` offered.
pub fn remember(store: &Store, cli: Cli, provider: &str) -> Result<()> {
    defaults::set(store.dir(), cli, Remembered::One(provider.to_owned()))
}

/// pi's remembered provider of each type.
fn pi_defaults(remembered: Option<&Remembered>, machine: &Machine) -> BTreeMap<String, String> {
    match remembered {
        Some(Remembered::ByType(by_type)) => by_type.clone(),
        Some(Remembered::One(id)) => machine
            .provider_types
            .get(id)
            .map(|provider_type| BTreeMap::from([(provider_type.clone(), id.clone())]))
            .unwrap_or_default(),
        None => BTreeMap::new(),
    }
}

/// `ags use <cli> <provider>`: checks the provider, then remembers it, for pi as the provider of
/// its type.
pub async fn use_provider(store: &Store, cli: Cli, provider: &str) -> Result<Offer> {
    let (login, machine) = granted(store).await?.ok_or_else(not_logged_in)?;
    let offer = offer(&machine, &login.server, cli, provider, path().as_deref())?;
    match cli {
        Cli::Pi => {
            let key = cli.to_string();
            let mut by_type = pi_defaults(defaults::load(store.dir())?.get(&key), &machine);
            by_type.insert(offer.provider_type.clone(), offer.id.clone());
            defaults::set(store.dir(), cli, Remembered::ByType(by_type))?;
        }
        _ => remember(store, cli, provider)?,
    }
    Ok(offer)
}

/// Every CLI's remembered provider, and pi's of each type; `None` when this machine is not
/// logged in.
pub async fn cli_defaults(store: &Store) -> Result<Option<Vec<CliDefault>>> {
    let Some((_, machine)) = granted(store).await? else {
        return Ok(None);
    };
    let remembered = defaults::load(store.dir())?;
    let granted_type = |id: &String| {
        machine
            .providers
            .contains(id)
            .then(|| machine.provider_types.get(id).cloned())
            .flatten()
    };
    let mut rows = vec![];
    for &cli in Cli::value_variants() {
        let key = cli.to_string();
        if cli == Cli::Pi {
            let by_type = pi_defaults(remembered.get(&key), &machine);
            let offers = offers(&machine, cli, path().as_deref());
            for provider_type in cli.provider_types() {
                let provider = by_type.get(*provider_type).cloned();
                rows.push(CliDefault {
                    cli,
                    for_type: Some((*provider_type).to_owned()),
                    provider_type: provider.as_ref().and_then(granted_type),
                    provider,
                    granted: offers
                        .iter()
                        .any(|offer| offer.provider_type == *provider_type),
                });
            }
            continue;
        }
        let provider = match remembered.get(&key) {
            Some(Remembered::One(id)) => Some(id.clone()),
            _ => None,
        };
        rows.push(CliDefault {
            cli,
            for_type: None,
            provider_type: provider.as_ref().and_then(granted_type),
            provider,
            granted: true,
        });
    }
    Ok(Some(rows))
}

/// pi's providers for one launch: `requested` for its type, else each type's remembered provider,
/// else its only one.
pub async fn plan_pi(store: &Store, requested: Option<&str>) -> Result<PiPlan> {
    let (login, machine) = granted(store).await?.ok_or_else(not_logged_in)?;
    let path = path();
    let requested = requested
        .map(|id| offer(&machine, &login.server, Cli::Pi, id, path.as_deref()))
        .transpose()?;
    let offers = offers(&machine, Cli::Pi, path.as_deref());
    if offers.is_empty() {
        return Err(NoProvider {
            cli: Cli::Pi,
            server: login.server,
        }
        .into());
    }
    let remembered = pi_defaults(
        defaults::load(store.dir())?.get(&Cli::Pi.to_string()),
        &machine,
    );
    Ok(pi_plan(login.server, offers, remembered, requested))
}

fn pi_plan(
    server: String,
    offers: Vec<Offer>,
    remembered: BTreeMap<String, String>,
    requested: Option<Offer>,
) -> PiPlan {
    let types = Cli::Pi
        .provider_types()
        .iter()
        .map(|&provider_type| {
            let fits: Vec<&Offer> = offers
                .iter()
                .filter(|offer| offer.provider_type == provider_type)
                .collect();
            let chosen = requested
                .as_ref()
                .filter(|offer| offer.provider_type == provider_type)
                .or_else(|| {
                    let id = remembered.get(provider_type)?;
                    fits.iter().copied().find(|offer| &offer.id == id)
                });
            let default = match (chosen, fits.as_slice()) {
                (Some(offer), _) => TypeDefault::Use(offer.clone()),
                (None, [offer]) => TypeDefault::Use((*offer).clone()),
                (None, []) => TypeDefault::None,
                (None, _) => TypeDefault::Ask(fits.into_iter().cloned().collect()),
            };
            (provider_type.to_owned(), default)
        })
        .collect();
    PiPlan {
        server,
        offers,
        types,
        requested,
        remembered,
    }
}

/// How a launched CLI ended.
pub struct Launched {
    pub status: ExitStatus,
    /// The server logged this machine out during the launch, and its login is forgotten.
    pub logged_out: Option<anyhow::Error>,
}

/// Runs `cli` with `args` on `offer`, in the provider's own config dir, until it exits.
/// `program` replaces the real CLI on `PATH`.
pub async fn launch(
    store: &Store,
    credentials: &Credentials,
    cli: Cli,
    offer: &Offer,
    args: Vec<OsString>,
    program: Option<&OsStr>,
) -> Result<Launched> {
    let chain = shims::chain();
    shims::check_depth(&chain, cli)?;
    let login = current_login(store).await?.ok_or_else(not_logged_in)?;
    let server = login.server.clone();
    let program = resolve(store, cli, program)?;
    let isolation = credentials.isolate(&offer.id, &offer.provider_type, cli)?;
    // A random UUID carries nothing about the launch, such as when it started.
    let launch_id = uuid::Uuid::new_v4().to_string();
    let base_path = cli.base_path(&offer.provider_type);
    let route = Route::provider(String::new(), &offer.id, base_path);
    let proxy = Proxy::start(store.dir(), login, vec![route], &launch_id).await?;
    let project = match offer.provider_type.as_str() {
        VERTEX => Some(credentials.vertex_project(&offer.id)?),
        _ => None,
    };
    let upstream = Upstream {
        id: &offer.id,
        provider_type: &offer.provider_type,
        region: offer.region.as_deref(),
        project: project.as_deref(),
    };
    let setup = cli.setup(
        upstream,
        &format!("{}{base_path}", proxy.origin()),
        proxy.secret(),
        store.dir(),
        |name| std::env::var(name).ok(),
    )?;
    let setup_notes = setup.notes;
    let mut command = Command::new(&program);
    outside(&mut command, credentials, &chain, cli);
    command
        .args(isolation.args)
        .args(setup.args)
        .args(args)
        .envs(isolation.env)
        .envs(credentials.cloud_env(&offer.id, &offer.provider_type)?)
        .envs(setup.env)
        .envs(no_proxy(|name| std::env::var(name).ok()))
        .kill_on_drop(true);
    for name in &isolation.unset {
        command.env_remove(name);
    }
    if cli == Cli::Claude
        && offer.provider_type == VERTEX
        && let Some(location) = &offer.region
    {
        for (name, note) in stray_model_regions(&offer.id, location, std::env::vars_os()) {
            command.env_remove(name);
            eprintln!("ags: {note}");
        }
    }
    for note in &setup_notes {
        eprintln!("ags: {note}");
    }
    run(store, &server, command, &program, proxy).await
}

/// Starts `command` free of any launch this one runs inside, as a launch of `cli` in `chain`.
fn outside(command: &mut Command, credentials: &Credentials, chain: &[Entered], cli: Cli) {
    credentials.outside_launch().apply(command.as_std_mut());
    command.env(shims::LAUNCHED, shims::entered(chain, cli));
}

fn resolve(store: &Store, cli: Cli, program: Option<&OsStr>) -> Result<OsString> {
    match program {
        Some(program) => Ok(program.to_owned()),
        None => Ok(shims::program(store, cli)?.into_os_string()),
    }
}

/// Runs pi with `args` on the providers of `plan`, in the dir of pi launches, until it exits.
/// `program` replaces the real pi on `PATH`.
pub async fn launch_pi(
    store: &Store,
    credentials: &Credentials,
    plan: &PiPlan,
    args: Vec<OsString>,
    program: Option<&OsStr>,
) -> Result<Launched> {
    let chain = shims::chain();
    shims::check_depth(&chain, Cli::Pi)?;
    let login = current_login(store).await?.ok_or_else(not_logged_in)?;
    let server = login.server.clone();
    let program = resolve(store, Cli::Pi, program)?;
    let isolation = credentials.isolate_pi()?;
    let (pi, routes) = pi::Launch::new(plan, credentials, |name| std::env::var(name).ok())?;
    let launch_id = uuid::Uuid::new_v4().to_string();
    let proxy = Proxy::start(store.dir(), login, routes, &launch_id).await?;
    let setup = pi.setup(proxy.origin(), proxy.secret(), store.dir())?;
    let mut command = Command::new(&program);
    outside(&mut command, credentials, &chain, Cli::Pi);
    command
        .args(setup.args)
        .args(args)
        .envs(isolation.env)
        .envs(setup.env)
        .envs(no_proxy(|name| std::env::var(name).ok()))
        .kill_on_drop(true);
    for name in &isolation.unset {
        command.env_remove(name);
    }
    for note in &setup.notes {
        eprintln!("ags: {note}");
    }
    run(store, &server, command, &program, proxy).await
}

/// Runs `command` until it exits, then ends `proxy` and checks what it learned about the login.
async fn run(
    store: &Store,
    server: &str,
    mut command: Command,
    program: &OsStr,
    proxy: Proxy,
) -> Result<Launched> {
    die_with_parent(&mut command);
    let status = wait(command, program).await;
    let ended = proxy.end();
    let status = status?;
    let logged_out = match ended.logged_out {
        Some(error) => Some(error),
        None if ended.refused => logged_out_of(store, server).await,
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
            status = child.wait() => return status.map_err(|error| Started(error).into()),
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
            status = child.wait() => return status.map_err(|error| Started(error).into()),
            interrupted = tokio::signal::ctrl_c() => interrupted?,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(id, value)| ((*id).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn cloud_machine(regions: Option<&[(&str, &str)]>) -> Machine {
        Machine {
            account: "me@example.com".into(),
            name: "ags".into(),
            providers: vec!["aws".into(), "gcp".into(), "vpce".into()],
            provider_types: map(&[("aws", "bedrock"), ("gcp", "vertex"), ("vpce", "bedrock")]),
            provider_regions: regions.map(map),
            provider_locations: map(&[("gcp", "us-east5")]),
        }
    }

    /// A `PATH` value whose one dir holds an executable `gcloud`.
    #[cfg(unix)]
    fn gcloud() -> (std::path::PathBuf, OsString) {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::store::tests::scratch();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("gcloud"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(dir.join("gcloud"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        (dir.clone(), dir.into_os_string())
    }

    #[cfg(unix)]
    #[test]
    fn codex_is_offered_bedrock_on_a_regional_endpoint_and_vertex_with_gcloud() {
        let machine = cloud_machine(Some(&[("aws", "eu-west-1")]));
        let (dir, path) = gcloud();
        let ids = |cli, path: Option<&OsStr>| {
            offers(&machine, cli, path)
                .into_iter()
                .map(|offer| (offer.id, offer.region))
                .collect::<Vec<_>>()
        };
        let aws = ("aws".to_owned(), Some("eu-west-1".to_owned()));
        let gcp = ("gcp".to_owned(), Some("us-east5".to_owned()));
        assert_eq!(ids(Cli::Codex, Some(&path)), [aws.clone(), gcp.clone()]);
        assert_eq!(ids(Cli::Codex, None), std::slice::from_ref(&aws));
        assert_eq!(
            ids(Cli::Claude, None),
            [aws, gcp, ("vpce".to_owned(), None)]
        );
        let server = "https://aegis.example";
        let refused = |machine: &Machine, id, path| {
            offer(machine, server, Cli::Codex, id, path)
                .unwrap_err()
                .to_string()
        };
        assert_eq!(
            refused(&machine, "vpce", Some(&path)),
            "codex can't run on vpce, a bedrock provider without a regional endpoint"
        );
        assert_eq!(
            refused(&machine, "gcp", None),
            "codex can't run on gcp: Codex signs in to Vertex with gcloud, which is not on PATH; install the Google Cloud CLI"
        );
        let old = cloud_machine(None);
        assert_eq!(
            refused(&old, "aws", Some(&path)),
            "codex can't run on aws until the Aegis server is updated to name its region"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn pi_offer(id: &str, provider_type: &str) -> Offer {
        Offer {
            id: id.into(),
            provider_type: provider_type.into(),
            region: None,
        }
    }

    #[test]
    fn pi_uses_for_each_type_the_named_then_the_remembered_then_the_only_provider() {
        use aegis_wire::provider_type::{BEDROCK, CLAUDE_SUBSCRIPTION};
        let offers = vec![
            pi_offer("work", CLAUDE_SUBSCRIPTION),
            pi_offer("aws", BEDROCK),
            pi_offer("aws2", BEDROCK),
            pi_offer("gcp", VERTEX),
        ];
        let types = |plan: PiPlan| {
            plan.types
                .into_iter()
                .map(|(provider_type, default)| {
                    let default = match default {
                        TypeDefault::Use(offer) => offer.id,
                        TypeDefault::Ask(offers) => {
                            let ids: Vec<String> =
                                offers.into_iter().map(|offer| offer.id).collect();
                            format!("ask {}", ids.join(" "))
                        }
                        TypeDefault::None => "none".to_owned(),
                    };
                    format!("{provider_type}={default}")
                })
                .collect::<Vec<_>>()
        };
        let server = || "https://aegis.example".to_owned();
        assert_eq!(
            types(pi_plan(server(), offers.clone(), BTreeMap::new(), None)),
            [
                "claude_subscription=work",
                "codex_subscription=none",
                "bedrock=ask aws aws2",
                "vertex=gcp"
            ]
        );
        let remembered = BTreeMap::from([
            (BEDROCK.to_owned(), "aws2".to_owned()),
            (VERTEX.to_owned(), "gone".to_owned()),
        ]);
        assert_eq!(
            types(pi_plan(server(), offers.clone(), remembered.clone(), None)),
            [
                "claude_subscription=work",
                "codex_subscription=none",
                "bedrock=aws2",
                "vertex=gcp"
            ]
        );
        let requested = Some(pi_offer("aws", BEDROCK));
        assert_eq!(
            types(pi_plan(server(), offers, remembered, requested))[2],
            "bedrock=aws"
        );
    }

    #[test]
    fn a_choice_for_one_type_is_remembered_beside_the_others() {
        use aegis_wire::provider_type::BEDROCK;
        let dir = crate::store::tests::scratch();
        let store = Store::at(&dir);
        let mut machine = cloud_machine(None);
        let earlier = Remembered::One("gcp".into());
        let remembered = pi_defaults(Some(&earlier), &machine);
        assert_eq!(
            remembered,
            BTreeMap::from([(VERTEX.to_owned(), "gcp".to_owned())])
        );
        machine.providers.retain(|id| id != "gcp");
        let offers = offers(&machine, Cli::Pi, None);
        let mut plan = pi_plan("https://aegis.example".into(), offers, remembered, None);
        assert!(matches!(&plan.types[2].1, TypeDefault::Ask(offers) if offers.len() == 2));
        plan.choose(&store, pi_offer("vpce", BEDROCK)).unwrap();
        assert_eq!(plan.types[2].1, TypeDefault::Use(pi_offer("vpce", BEDROCK)));
        assert_eq!(
            defaults::load(&dir).unwrap()["pi"],
            Remembered::ByType(BTreeMap::from([
                (BEDROCK.to_owned(), "vpce".to_owned()),
                (VERTEX.to_owned(), "gcp".to_owned()),
            ]))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

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
