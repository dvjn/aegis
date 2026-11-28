use std::{
    ffi::{OsStr, OsString},
    io::{BufRead, IsTerminal},
    path::Path,
    process::{ExitCode, ExitStatus, Stdio},
};

use ags::{
    Approval, Cli, CliDefault, Credentials, EnvChanges, Home, LoggedOut, Logout, Need, NoProvider,
    NotLoggedIn, Offer, Pick, Started, Step, StepStatus, Store, TypeDefault, Unreachable, product,
    provider_text,
    shims::{self, Shell},
};
use anyhow::{Context, bail};
use clap::{Parser, Subcommand, ValueEnum};

/// Command-line client for Aegis.
#[derive(Parser)]
#[command(name = "ags", version, arg_required_else_help = true)]
struct Args {
    /// Use this provider for one launch, in place of the CLI's default
    #[arg(short, long, env = "AGS_PROVIDER")]
    provider: Option<String>,
    /// Launched by a shim, which may open the real CLI when Aegis can't be used
    #[arg(long, hide = true)]
    shim: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Pair this machine with an Aegis server
    Login {
        /// The server's URL, such as https://aegis.example
        url: String,
    },
    /// Revoke this machine's access and forget the server
    Logout,
    /// Show the server, account and providers of this machine
    Status,
    /// Sign in to each provider that needs it, or again to one provider
    Setup {
        /// Set up only this provider, again if it was set up before
        provider: Option<String>,
    },
    /// Set the provider a CLI uses, or list the provider of each CLI
    Use {
        #[arg(requires = "provider")]
        cli: Option<Cli>,
        provider: Option<String>,
    },
    /// Print the shell code that runs plain claude, codex and pi through ags
    Init { shell: Shell },
    /// Run Claude Code through Aegis
    #[command(disable_help_flag = true)]
    Claude {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Run Codex through Aegis
    #[command(disable_help_flag = true)]
    Codex {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Run pi through Aegis
    #[command(disable_help_flag = true)]
    Pi {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
}

enum Exit {
    Code(ExitCode),
    /// Ends as the launched CLI ended.
    As(ExitStatus),
}

/// How many leading arguments are ags's own: up to the CLI's name, after which nothing is parsed.
fn own_arguments(args: &[OsString]) -> usize {
    let mut index = 1;
    while let Some(arg) = args.get(index).and_then(|arg| arg.to_str()) {
        if Cli::from_str(arg, false).is_ok() {
            return index + 1;
        }
        match arg {
            "-p" | "--provider" => index += 2,
            "--shim" => index += 1,
            _ if arg.starts_with("--provider=") || (arg.starts_with("-p") && arg.len() > 2) => {
                index += 1;
            }
            _ => break,
        }
    }
    args.len()
}

fn try_parse(mut args: Vec<OsString>) -> Result<Args, clap::Error> {
    let theirs = args.split_off(own_arguments(&args).min(args.len()));
    let mut parsed = Args::try_parse_from(args)?;
    if let Command::Claude { args } | Command::Codex { args } | Command::Pi { args } =
        &mut parsed.command
    {
        *args = theirs;
    }
    Ok(parsed)
}

fn main() -> ExitCode {
    let args = try_parse(std::env::args_os().collect()).unwrap_or_else(|error| error.exit());
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return fail(&error.into()),
    };
    let exit = runtime.block_on(run(args));
    drop(runtime);
    match exit {
        Ok(Exit::Code(code)) => code,
        Ok(Exit::As(status)) => exit_as(status),
        Err(error) => fail(&error),
    }
}

fn fail(error: &anyhow::Error) -> ExitCode {
    eprintln!("ags: {error:#}");
    ExitCode::FAILURE
}

#[cfg(unix)]
fn exit_as(status: ExitStatus) -> ExitCode {
    use std::os::unix::process::ExitStatusExt;
    if let Some(signal) = status.signal() {
        // SAFETY: restores the default action, then ends this process by the CLI's signal.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
        return ExitCode::from(128_u8.saturating_add(u8::try_from(signal).unwrap_or(0)));
    }
    exit_code(status)
}

#[cfg(not(unix))]
fn exit_as(status: ExitStatus) -> ExitCode {
    match status.code() {
        Some(code) if u8::try_from(code).is_err() => std::process::exit(code),
        _ => exit_code(status),
    }
}

fn exit_code(status: ExitStatus) -> ExitCode {
    status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .map_or(ExitCode::FAILURE, ExitCode::from)
}

/// `program` replaces the real CLI on `PATH`.
async fn launch(
    store: &Store,
    credentials: &Credentials,
    cli: Cli,
    provider: Option<&str>,
    args: Vec<OsString>,
    program: Option<&OsStr>,
) -> anyhow::Result<Exit> {
    // An empty AGS_PROVIDER counts as unset.
    let provider = provider.filter(|id| !id.is_empty());
    if cli == Cli::Pi {
        return launch_pi(store, credentials, provider, args, program).await;
    }
    let offer = match ags::pick(store, cli, provider).await? {
        Pick::Use(offer) => offer,
        Pick::Ask(offers) => {
            if !interactive() {
                bail!(
                    "{cli} can run on several providers; choose one with `ags use {cli} <provider>`, or for one launch with `ags -p <provider> {cli}` or AGS_PROVIDER"
                );
            }
            let offer = ask(&format!("Which provider should {cli} use?"), offers)?;
            ags::remember(store, cli, &offer.id)?;
            eprintln!(
                "{cli} now uses {}. Change it with `ags use {cli} <provider>`.",
                offer.id
            );
            offer
        }
    };
    set_up_for_launch(credentials, cli, &offer).await?;
    warn(credentials, &offer.id, cli)?;
    let launched = ags::launch(store, credentials, cli, &offer, args, program).await?;
    if let Some(error) = launched.logged_out {
        eprintln!("ags: {error:#}");
    }
    Ok(Exit::As(launched.status))
}

/// Runs pi on every provider it can use, asking once for each type that has several.
async fn launch_pi(
    store: &Store,
    credentials: &Credentials,
    provider: Option<&str>,
    args: Vec<OsString>,
    program: Option<&OsStr>,
) -> anyhow::Result<Exit> {
    let mut plan = ags::plan_pi(store, provider).await?;
    if interactive() {
        let open: Vec<(String, Vec<Offer>)> = plan
            .types
            .iter()
            .filter_map(|(provider_type, default)| match default {
                TypeDefault::Ask(offers) => Some((provider_type.clone(), offers.clone())),
                _ => None,
            })
            .collect();
        for (provider_type, offers) in open {
            let offer = ask(
                &format!("Which {provider_type} provider should pi use?"),
                offers,
            )?;
            eprintln!(
                "pi now uses {} for {provider_type}. Change it with `ags use pi <provider>`.",
                offer.id
            );
            plan.choose(store, offer)?;
        }
    }
    if let Some(offer) = &plan.requested {
        set_up_for_launch(credentials, Cli::Pi, offer).await?;
    }
    for notice in credentials.pi_notices()? {
        eprintln!("ags: {notice}");
    }
    let launched = ags::launch_pi(store, credentials, &plan, args, program).await?;
    if let Some(error) = launched.logged_out {
        eprintln!("ags: {error:#}");
    }
    Ok(Exit::As(launched.status))
}

/// A shim's launch, which offers a fix or the real CLI without Aegis when Aegis can't be used.
async fn launch_from_shim(
    store: &Store,
    credentials: &Credentials,
    cli: Cli,
    provider: Option<&str>,
    args: Vec<OsString>,
) -> anyhow::Result<Exit> {
    let real = shims::program(store, cli)?;
    if bypassed() {
        return run_real(&real, args, &without_aegis(credentials, cli));
    }
    let chain = shims::chain();
    shims::check_depth(&chain, cli)?;
    // Directly inside a running launch of the same CLI, the real CLI runs in that launch.
    if chain.last().is_some_and(|last| last.cli == cli) {
        let mut changes = EnvChanges::default();
        changes.set(shims::LAUNCHED, shims::entered(&chain, cli));
        return run_real(&real, args, &changes);
    }
    let mut fixed = false;
    loop {
        let launched = launch(
            store,
            credentials,
            cli,
            provider,
            args.clone(),
            Some(real.as_os_str()),
        )
        .await;
        let error = match launched {
            Ok(exit) => return Ok(exit),
            Err(error) if error.is::<Started>() || error.is::<shims::TooDeep>() => {
                return Err(error);
            }
            Err(error) => error,
        };
        if !interactive() {
            eprintln!("ags: opening {cli} without Aegis: {error:#}");
            return run_real(&real, args, &without_aegis(credentials, cli));
        }
        eprintln!("ags: {error:#}");
        if !fixed {
            fixed = true;
            let _prompts = PromptsOnStderr::start();
            match fix(store, credentials, cli, &error).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(error) => eprintln!("ags: {error:#}"),
            }
        }
        if confirm(&format!("Open {cli} without Aegis?"))? {
            return run_real(&real, args, &without_aegis(credentials, cli));
        }
        return Ok(Exit::Code(ExitCode::FAILURE));
    }
}

fn bypassed() -> bool {
    std::env::var_os(shims::BYPASS).is_some_and(|value| !value.is_empty() && value != "0")
}

/// What `cli` run without Aegis changes in the environment ags has.
fn without_aegis(credentials: &Credentials, cli: Cli) -> EnvChanges {
    let mut changes = credentials.outside_launch();
    changes.remove(shims::BYPASS);
    changes.set(shims::LAUNCHED, shims::entered(&shims::chain(), cli));
    changes
}

/// Offers the login, pairing or setup that `error` says `cli` lacks; whether it was done.
async fn fix(
    store: &Store,
    credentials: &Credentials,
    cli: Cli,
    error: &anyhow::Error,
) -> anyhow::Result<bool> {
    let (server, again) = if error.is::<NotLoggedIn>() {
        if !confirm("Log in to Aegis now?")? {
            return Ok(false);
        }
        let url = read_answer("Server URL: ")?;
        if url.is_empty() {
            return Ok(false);
        }
        (url, false)
    } else if let Some(LoggedOut { server, .. }) = error.downcast_ref() {
        if !confirm(&format!("Log in to {server} again?"))? {
            return Ok(false);
        }
        (server.clone(), false)
    } else if error.is::<Unreachable>() {
        if !confirm("Set up this machine's providers now?")? {
            return Ok(false);
        }
        return setup(store, credentials, None).await;
    } else if let Some(NoProvider { server, .. }) = error.downcast_ref() {
        if !confirm(&format!(
            "Pair this machine with {server} again to grant {cli} a provider?"
        ))? {
            return Ok(false);
        }
        (server.clone(), true)
    } else {
        return Ok(false);
    };
    login(store, credentials, &server, again).await?;
    Ok(true)
}

/// Runs the real CLI in place of ags, with `changes` to its environment.
fn run_real(real: &Path, args: Vec<OsString>, changes: &EnvChanges) -> anyhow::Result<Exit> {
    let mut command = std::process::Command::new(real);
    command.args(args);
    changes.apply(&mut command);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        Err(error).with_context(|| format!("could not start {}", real.display()))
    }
    #[cfg(not(unix))]
    command
        .status()
        .map(Exit::As)
        .with_context(|| format!("could not start {}", real.display()))
}

/// While it lives, ags's stdout goes to stderr, keeping a piped stdout for the CLI's output.
struct PromptsOnStderr {
    #[cfg(unix)]
    stdout: Option<std::os::fd::OwnedFd>,
}

impl PromptsOnStderr {
    fn start() -> Self {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        #[cfg(unix)]
        {
            use std::os::fd::{AsFd, AsRawFd};
            let stdout = std::io::stdout().as_fd().try_clone_to_owned().ok();
            if stdout.is_some() {
                // SAFETY: both fds are this process's own, and stdout stays open as a copy of stderr.
                unsafe { libc::dup2(std::io::stderr().as_raw_fd(), std::io::stdout().as_raw_fd()) };
            }
            Self { stdout }
        }
        #[cfg(not(unix))]
        Self {}
    }
}

impl Drop for PromptsOnStderr {
    fn drop(&mut self) {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        #[cfg(unix)]
        if let Some(stdout) = &self.stdout {
            use std::os::fd::AsRawFd;
            // SAFETY: `stdout` is the copy taken at the start, open until this guard is dropped.
            unsafe { libc::dup2(stdout.as_raw_fd(), std::io::stdout().as_raw_fd()) };
        }
    }
}

/// Offers to set up what `cli` needs to run on `offer`, which the launch can't go without.
async fn set_up_for_launch(
    credentials: &Credentials,
    cli: Cli,
    offer: &Offer,
) -> anyhow::Result<()> {
    if let Some(need) = Need::of_launch(cli, &offer.provider_type)
        && !credentials.has(&offer.id, &offer.provider_type, need)?
    {
        let id = &offer.id;
        if !interactive() {
            bail!("{id} has no {need} yet; run `ags setup {id}` first");
        }
        eprintln!("{id} has no {need} yet.");
        let _prompts = PromptsOnStderr::start();
        let step = Step {
            provider: offer.id.clone(),
            provider_type: offer.provider_type.clone(),
            need,
            status: StepStatus::Missing,
        };
        if set_up(credentials, &step).await? != Outcome::Done {
            bail!("{id} has no {need}; set it up with `ags setup {id}`");
        }
    }
    Ok(())
}

/// Tells the user, once, about settings that keep `provider` from using its own sign-in in `cli`.
fn warn(credentials: &Credentials, provider: &str, cli: Cli) -> anyhow::Result<()> {
    for notice in credentials.notices(provider, cli)? {
        eprintln!("ags: {notice}");
    }
    Ok(())
}

fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// `ags setup`: walks the credentials that are missing, or every one of `only`.
async fn setup(
    store: &Store,
    credentials: &Credentials,
    only: Option<&str>,
) -> anyhow::Result<bool> {
    let steps = ags::setup_steps(store, credentials, only).await?;
    if let Some(id) = only
        && steps.is_empty()
    {
        println!("{id} needs nothing set up.");
        return Ok(true);
    }
    let redo = only.is_some();
    // `absent` sign-ins are for CLIs that are not installed.
    let (mut todo, mut absent, mut skipped) = (vec![], vec![], vec![]);
    for step in steps {
        match step.status {
            StepStatus::Done if !redo || !has_cli(store, &step) => {}
            StepStatus::Skipped if !redo => skipped.push(step),
            _ if !has_cli(store, &step) => absent.push(step),
            _ => todo.push(step),
        }
    }
    if !todo.is_empty() && !interactive() {
        bail!("setting up providers needs a terminal");
    }
    let mut failed = vec![];
    let mut waiting = vec![];
    for step in &absent {
        let Need::SignIn(cli) = step.need else {
            continue;
        };
        if interactive() {
            println!();
            println!("{} ({}): {}", step.provider, step.provider_type, step.need);
            let name = product(cli);
            if !confirm(&format!(
                "{name} is not installed. Sign in to it for {} once it is?",
                step.provider
            ))? {
                credentials.skip(&step.provider, step.need)?;
                skipped.push(step.clone());
                continue;
            }
            credentials.unskip(&step.provider, step.need)?;
        }
        waiting.push(step.clone());
    }
    for step in &todo {
        println!();
        println!("{} ({}): {}", step.provider, step.provider_type, step.need);
        let outcome = set_up(credentials, step).await.unwrap_or_else(|error| {
            eprintln!("ags: {error:#}");
            Outcome::Failed
        });
        match outcome {
            Outcome::Done | Outcome::Kept => {}
            Outcome::Skipped => skipped.push(step.clone()),
            Outcome::Failed => failed.push(step.clone()),
        }
    }
    if !todo.is_empty() || (!absent.is_empty() && interactive()) {
        println!();
    }
    let list = |steps: &[Step]| {
        steps
            .iter()
            .map(|step| format!("{} ({})", step.provider, step.need))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if !failed.is_empty() {
        match only {
            Some(id) => println!("Could not set up {id}. Run `ags setup {id}` to try again."),
            None => println!("Not set up: {}. Run `ags setup` to finish.", list(&failed)),
        }
    }
    if !skipped.is_empty() {
        println!(
            "Skipped: {}. Set one up with `ags setup <provider>`.",
            list(&skipped)
        );
    }
    for step in &waiting {
        if let Need::SignIn(cli) = step.need {
            println!(
                "Install {}, then run `ags setup {}` to sign in to it.",
                product(cli),
                step.provider
            );
        }
    }
    if failed.is_empty() && skipped.is_empty() && waiting.is_empty() {
        match only {
            Some(id) => println!("{id} is set up."),
            None if todo.is_empty() => {
                println!("Every provider is set up. Redo one with `ags setup <provider>`.");
            }
            None => println!("Every provider is set up."),
        }
    }
    Ok(failed.is_empty())
}

/// Whether the CLI a step signs in to is installed; other steps need no CLI.
fn has_cli(store: &Store, step: &Step) -> bool {
    match step.need {
        Need::SignIn(cli) => ags::installed(store, cli),
        _ => true,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Done,
    /// Declined, with the earlier credential still in place.
    Kept,
    Skipped,
    Failed,
}

/// Sets up one credential at the terminal. Declining a missing one is remembered.
async fn set_up(credentials: &Credentials, step: &Step) -> anyhow::Result<Outcome> {
    let (id, provider_type, need) = (&step.provider, &step.provider_type, step.need);
    let answered = match need {
        Need::SignIn(cli) => {
            let name = product(cli);
            if !confirm(&format!("Sign in to {name} for {id} now?"))? {
                None
            } else {
                warn(credentials, id, cli)?;
                credentials.sign_in(id, provider_type, cli, None).await?;
                if !credentials.has(id, provider_type, need)? {
                    eprintln!("ags: {id} is not signed in to {name}");
                    return Ok(Outcome::Failed);
                }
                Some(format!("{id} is signed in to {name}."))
            }
        }
        Need::BedrockKey => {
            let key = read_secret(&format!("Bedrock API key for {id}: "))?;
            if key.is_empty() {
                None
            } else {
                credentials.save_bedrock_key(id, &key)?;
                Some(format!("Saved the Bedrock API key for {id}."))
            }
        }
        Need::VertexAccount => {
            let path = read_answer(&format!("Service account key file for {id}: "))?;
            if path.is_empty() {
                None
            } else {
                credentials.save_vertex_account(id, std::path::Path::new(&path))?;
                Some(format!("Saved a copy of the service account key for {id}."))
            }
        }
    };
    match answered {
        Some(done) => {
            credentials.unskip(id, need)?;
            println!("{done}");
            Ok(Outcome::Done)
        }
        None if step.status == StepStatus::Done => Ok(Outcome::Kept),
        None => {
            credentials.skip(id, need)?;
            Ok(Outcome::Skipped)
        }
    }
}

/// Asks a yes-or-no question; an empty answer is yes.
fn confirm(question: &str) -> anyhow::Result<bool> {
    loop {
        let answer = read_answer(&format!("{question} [Y/n] "))?.to_ascii_lowercase();
        match answer.as_str() {
            "" | "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => {}
        }
    }
}

fn read_answer(prompt: &str) -> anyhow::Result<String> {
    eprint!("{prompt}");
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        bail!("no answer");
    }
    Ok(line.trim().to_owned())
}

/// Reads a line from the terminal without showing it. Ctrl-C restores the terminal before it ends ags.
#[cfg(unix)]
fn read_secret(prompt: &str) -> anyhow::Result<String> {
    use std::{io::Read, os::fd::AsRawFd};

    /// Puts the terminal back as it was, however reading ends.
    struct Restore(libc::c_int, libc::termios);
    impl Drop for Restore {
        fn drop(&mut self) {
            // SAFETY: the fd is stdin and the settings came from tcgetattr.
            unsafe { libc::tcsetattr(self.0, libc::TCSANOW, &raw const self.1) };
        }
    }

    let fd = std::io::stdin().as_raw_fd();
    // SAFETY: termios is plain data, filled in by tcgetattr before it is read.
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: fd is this process's stdin and `saved` outlives the call.
    if unsafe { libc::tcgetattr(fd, &raw mut saved) } != 0 {
        return read_answer(prompt);
    }
    let mut raw = saved;
    raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG);
    raw.c_cc[libc::VMIN] = 1;
    raw.c_cc[libc::VTIME] = 0;
    eprint!("{prompt}");
    let restore = Restore(fd, saved);
    // SAFETY: as above; `restore` puts the saved settings back.
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw const raw) };
    let mut line = Vec::new();
    let mut stdin = std::io::stdin().lock();
    let mut byte = [0_u8; 1];
    loop {
        if stdin.read(&mut byte)? == 0 {
            bail!("no answer");
        }
        match byte[0] {
            b'\r' | b'\n' => break,
            // Ctrl-C
            0x03 => {
                drop(restore);
                eprintln!();
                // SAFETY: raise only sends this process the signal Ctrl-C would have.
                unsafe { libc::raise(libc::SIGINT) };
                bail!("interrupted");
            }
            // Ctrl-D
            0x04 if line.is_empty() => bail!("no answer"),
            // Backspace and Delete remove a whole character.
            0x08 | 0x7f => while line.pop().is_some_and(|byte| byte & 0xc0 == 0x80) {},
            // Ctrl-U
            0x15 => line.clear(),
            byte if byte < 0x20 => {}
            byte => line.push(byte),
        }
    }
    drop(restore);
    eprintln!();
    Ok(String::from_utf8_lossy(&line).trim().to_owned())
}

#[cfg(not(unix))]
fn read_secret(prompt: &str) -> anyhow::Result<String> {
    read_answer(prompt)
}

/// Asks `question` at the terminal, for one of `offers`.
fn ask(question: &str, offers: Vec<Offer>) -> anyhow::Result<Offer> {
    eprintln!("{question}");
    for (number, offer) in (1..).zip(&offers) {
        eprintln!("  {number}. {} ({})", offer.id, offer.provider_type);
    }
    let mut line = String::new();
    loop {
        eprint!("Choose 1-{}: ", offers.len());
        line.clear();
        if std::io::stdin().lock().read_line(&mut line)? == 0 {
            bail!("no provider chosen");
        }
        let answer = line.trim();
        let chosen = answer
            .parse::<usize>()
            .ok()
            .and_then(|number| number.checked_sub(1))
            .or_else(|| offers.iter().position(|offer| offer.id == answer));
        if let Some(index) = chosen.filter(|index| *index < offers.len()) {
            return Ok(offers
                .into_iter()
                .nth(index)
                .expect("the index is in range"));
        }
    }
}

fn defaults_table(rows: &[CliDefault]) -> String {
    let mut lines = vec![["CLI", "PROVIDER", "TYPE"].map(str::to_owned)];
    for row in rows {
        let provider = row.provider.as_deref().unwrap_or("—");
        let provider_type = match (&row.for_type, &row.provider, &row.provider_type) {
            (Some(for_type), Some(_), Some(provider_type)) if provider_type == for_type => {
                for_type.clone()
            }
            (Some(for_type), Some(_), _) => format!("{for_type} (not granted)"),
            (Some(for_type), None, _) if !row.granted => format!("{for_type} (none granted)"),
            (Some(for_type), None, _) => for_type.clone(),
            (None, _, Some(provider_type)) => provider_type.clone(),
            (None, Some(_), None) => "not granted".to_owned(),
            (None, None, None) => String::new(),
        };
        lines.push([row.cli.to_string(), provider.to_owned(), provider_type]);
    }
    let width = |column: usize| {
        lines
            .iter()
            .map(|line| line[column].chars().count())
            .max()
            .unwrap_or(0)
    };
    let (first, second) = (width(0), width(1));
    lines
        .iter()
        .map(|[cli, provider, provider_type]| {
            format!("{cli:first$}  {provider:second$}  {provider_type}")
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn run(args: Args) -> anyhow::Result<Exit> {
    let store = Store::default_location()?;
    let store = &store;
    let credentials = &Credentials::new(store, Home::from_env());
    let provider = args.provider.as_deref();
    let launched = |cli, cli_args| async move {
        if args.shim {
            launch_from_shim(store, credentials, cli, provider, cli_args).await
        } else {
            launch(store, credentials, cli, provider, cli_args, None).await
        }
    };
    match args.command {
        Command::Claude { args } => return launched(Cli::Claude, args).await,
        Command::Codex { args } => return launched(Cli::Codex, args).await,
        Command::Pi { args } => return launched(Cli::Pi, args).await,
        Command::Init { shell } => {
            if matches!(store.load(), Ok(Some(_)))
                && let Err(error) = write_shims(store)
            {
                eprintln!("ags: {error:#}");
            }
            print!("{}", shims::init(shell, &shims::dir(store))?);
        }
        Command::Setup { provider } => {
            if !setup(store, credentials, provider.as_deref()).await? {
                return Ok(Exit::Code(ExitCode::FAILURE));
            }
        }
        Command::Use {
            cli: Some(cli),
            provider: Some(provider),
        } => {
            let offer = ags::use_provider(store, cli, &provider).await?;
            match cli {
                Cli::Pi => println!("pi now uses {provider} for {}.", offer.provider_type),
                _ => println!("{cli} now uses {provider}."),
            }
        }
        Command::Use { .. } => match ags::cli_defaults(store).await? {
            Some(rows) => println!("{}", defaults_table(&rows)),
            None => {
                println!("Not logged in. Run `ags login <URL>` to pair this machine.");
                return Ok(Exit::Code(ExitCode::FAILURE));
            }
        },
        Command::Login { url } => login(store, credentials, &url, false).await?,
        Command::Logout => {
            let logout = ags::logout(store).await;
            shims::remove(store)?;
            let logout = logout?;
            if logout.is_none() {
                println!("Not logged in.");
            } else if !logged_out(logout) {
                return Ok(Exit::Code(ExitCode::FAILURE));
            }
        }
        Command::Status => match ags::status(store).await? {
            Some(status) => {
                println!("{status}");
                shim_hint(store);
            }
            None => {
                println!("Not logged in. Run `ags login <URL>` to pair this machine.");
                return Ok(Exit::Code(ExitCode::FAILURE));
            }
        },
    }
    Ok(Exit::Code(ExitCode::SUCCESS))
}

/// `ags login`, or with `again` a new pairing that replaces the current one once approved.
async fn login(
    store: &Store,
    credentials: &Credentials,
    url: &str,
    again: bool,
) -> anyhow::Result<()> {
    let status = if again {
        let paired = ags::pair_again(store, url, show_approval).await?;
        if let Some(reason) = paired.kept {
            let server = &paired.status.server;
            eprintln!(
                "ags: could not revoke this machine's earlier access ({reason}); disconnect it under Connected apps on {server}/account"
            );
        }
        paired.status
    } else {
        ags::login(store, url, show_approval).await?
    };
    println!("Logged in to {} as {}.", status.server, status.account);
    println!("Providers: {}", provider_text(&status.providers));
    if let Err(error) = write_shims(store) {
        eprintln!("ags: {error:#}");
    }
    if interactive() {
        setup(store, credentials, None).await?;
    } else if ags::setup_steps(store, credentials, None)
        .await?
        .iter()
        .any(|step| step.status == StepStatus::Missing && has_cli(store, step))
    {
        println!("Run `ags setup` in a terminal to sign in to each provider.");
    }
    shim_hint(store);
    Ok(())
}

/// Writes the shims unless each already runs this ags.
fn write_shims(store: &Store) -> anyhow::Result<()> {
    let ags = std::env::current_exe().context("finding ags itself for its shims")?;
    if shims::current(store, &ags) {
        return Ok(());
    }
    shims::install(store, &ags)
}

/// Says how logging out went; whether the server revoked this machine's access.
fn logged_out(logout: Option<Logout>) -> bool {
    match logout {
        None => true,
        Some(Logout::Revoked { server }) => {
            println!("Logged out of {server}.");
            true
        }
        Some(Logout::Forgotten { server, reason }) => {
            println!("Logged out of {server}.");
            eprintln!(
                "ags: could not revoke this machine's access ({reason}); disconnect it under Connected apps on {server}/account"
            );
            false
        }
    }
}

/// Prints the line that puts the shims first on `PATH`, unless they already are.
fn shim_hint(store: &Store) {
    let dir = shims::dir(store);
    if !shims::written(&dir) || shims::active(&dir, std::env::var_os("PATH").as_deref()) {
        return;
    }
    match Shell::of_user() {
        Some(shell) => println!(
            "To run plain claude, codex and pi through ags, add this line to {} and open a new shell:\n  {}",
            shell.startup_file(),
            shell.init_line()
        ),
        None => {
            println!(
                "To run plain claude, codex and pi through ags, add the line for your shell to its startup file:"
            );
            for shell in Shell::value_variants() {
                println!("  {}", shell.init_line());
            }
        }
    }
}

fn show_approval(approval: &Approval<'_>) {
    println!("Approve this machine in your browser:");
    println!("  {}", approval.url);
    println!("Check that the page shows the code {}.", approval.code);
    open_browser(approval.url);
    println!("Waiting for approval...");
}

/// Best effort: the printed URL works when no browser opens.
fn open_browser(url: &str) {
    // ags checked that the page is on the server's origin; this keeps any other scheme out.
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return;
    }
    let mut command = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(windows) {
        // `cmd /C start` would interpret characters in the URL, so Windows users follow the link.
        return;
    } else if std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
    {
        std::process::Command::new("xdg-open")
    } else {
        return;
    };
    let _ = command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use ags::{Cli, CliDefault, shims::Shell};
    use clap::{CommandFactory, Parser, ValueEnum, error::ErrorKind};

    use super::{Args, Command, defaults_table, try_parse};

    const AGS_VERSION: &str = env!("CARGO_PKG_VERSION");

    fn parse(args: &[&str]) -> Result<Args, clap::Error> {
        try_parse(args.iter().map(OsString::from).collect())
    }

    fn launched(args: &[&str]) -> (Option<String>, Vec<String>) {
        let parsed = parse(args).unwrap();
        let (Command::Claude { args } | Command::Codex { args } | Command::Pi { args }) =
            parsed.command
        else {
            panic!("{args:?} is a launch");
        };
        let args = args.into_iter().map(|arg| arg.into_string().unwrap());
        (parsed.provider, args.collect())
    }

    #[test]
    fn everything_after_the_cli_reaches_it_unchanged() {
        assert_eq!(
            launched(&["ags", "claude", "--resume"]),
            (None, vec!["--resume".to_owned()])
        );
        let theirs = [
            "-p",
            "hi",
            "--help",
            "--version",
            "--",
            "-x",
            "claude",
            "use",
        ];
        let mut args = vec!["ags", "codex"];
        args.extend(theirs);
        assert_eq!(launched(&args).1, theirs);
        assert!(launched(&["ags", "pi"]).1.is_empty());
    }

    #[test]
    fn a_provider_for_one_launch_comes_before_the_cli() {
        for args in [
            ["ags", "-p", "aws", "claude", "--resume"].as_slice(),
            &["ags", "--provider", "aws", "claude", "--resume"],
            &["ags", "--provider=aws", "claude", "--resume"],
            &["ags", "-paws", "claude", "--resume"],
        ] {
            assert_eq!(
                launched(args),
                (Some("aws".to_owned()), vec!["--resume".to_owned()]),
                "{args:?}"
            );
        }
        assert_eq!(
            launched(&["ags", "-p", "pi", "pi"]).0.as_deref(),
            Some("pi")
        );
        let provider = Args::command()
            .get_arguments()
            .find(|arg| arg.get_id() == "provider")
            .and_then(|arg| arg.get_env().map(ToOwned::to_owned));
        assert_eq!(provider, Some("AGS_PROVIDER".into()));
    }

    #[test]
    fn each_cli_launches_through_its_own_command() {
        for cli in Cli::value_variants() {
            let parsed = parse(&["ags", &cli.to_string(), "--x"]).unwrap();
            let launched = match parsed.command {
                Command::Claude { .. } => Cli::Claude,
                Command::Codex { .. } => Cli::Codex,
                Command::Pi { .. } => Cli::Pi,
                _ => panic!("{cli} is a launch"),
            };
            assert_eq!(launched, *cli);
        }
    }

    #[test]
    fn only_the_known_clis_launch() {
        let unknown = parse(&["ags", "gemini", "--resume"]).err().unwrap();
        assert_eq!(unknown.kind(), ErrorKind::InvalidSubcommand);
        let not_usable = parse(&["ags", "use", "gemini", "aws"]).err().unwrap();
        assert_eq!(not_usable.kind(), ErrorKind::InvalidValue);
        assert!(parse(&["ags", "use", "claude"]).is_err());
        assert!(matches!(
            parse(&["ags", "use"]).unwrap().command,
            Command::Use {
                cli: None,
                provider: None
            }
        ));
    }

    #[test]
    fn use_lists_each_cli_with_its_provider_and_pis_for_each_type() {
        use aegis_wire::provider_type::{BEDROCK, CLAUDE_SUBSCRIPTION, CODEX_SUBSCRIPTION, VERTEX};
        let row =
            |cli, for_type: Option<&str>, provider: Option<&str>, provider_type: Option<&str>| {
                CliDefault {
                    cli,
                    for_type: for_type.map(str::to_owned),
                    provider: provider.map(str::to_owned),
                    provider_type: provider_type.map(str::to_owned),
                    granted: for_type != Some(VERTEX),
                }
            };
        let rows = [
            row(Cli::Claude, None, Some("claude"), Some(CLAUDE_SUBSCRIPTION)),
            row(Cli::Codex, None, None, None),
            row(Cli::Codex, None, Some("gone"), None),
            row(
                Cli::Pi,
                Some(CLAUDE_SUBSCRIPTION),
                Some("claude"),
                Some(CLAUDE_SUBSCRIPTION),
            ),
            row(Cli::Pi, Some(CODEX_SUBSCRIPTION), None, None),
            row(Cli::Pi, Some(BEDROCK), Some("gone"), None),
            row(Cli::Pi, Some(VERTEX), None, None),
        ];
        assert_eq!(
            defaults_table(&rows),
            "CLI     PROVIDER  TYPE\n\
             claude  claude    claude_subscription\n\
             codex   —\n\
             codex   gone      not granted\n\
             pi      claude    claude_subscription\n\
             pi      —         codex_subscription\n\
             pi      gone      bedrock (not granted)\n\
             pi      —         vertex (none granted)"
        );
    }

    #[test]
    fn prints_its_version() {
        let error = Args::try_parse_from(["ags", "--version"])
            .err()
            .expect("--version stops parsing");

        assert_eq!(error.kind(), ErrorKind::DisplayVersion);
        assert_eq!(error.to_string(), format!("ags {AGS_VERSION}\n"));
    }

    #[test]
    fn prints_help_when_run_without_arguments() {
        let error = Args::try_parse_from(["ags"])
            .err()
            .expect("a bare ags stops parsing");

        assert_eq!(
            error.kind(),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
        assert!(error.to_string().contains("Usage: ags"));
    }

    #[test]
    fn setup_walks_every_provider_or_redoes_one() {
        assert!(matches!(
            parse(&["ags", "setup"]).unwrap().command,
            Command::Setup { provider: None }
        ));
        assert!(matches!(
            parse(&["ags", "setup", "aws"]).unwrap().command,
            Command::Setup { provider: Some(provider) } if provider == "aws"
        ));
    }

    #[test]
    fn login_takes_the_server_url() {
        let cli = Args::try_parse_from(["ags", "login", "https://aegis.example"]).unwrap();
        assert!(matches!(cli.command, Command::Login { url } if url == "https://aegis.example"));
        assert!(Args::try_parse_from(["ags", "login"]).is_err());
    }

    #[test]
    fn a_shim_launches_like_ags_cli_and_passes_everything_on() {
        let parsed = parse(&["ags", "--shim", "claude", "--shim", "-p", "x"]).unwrap();
        assert!(parsed.shim);
        assert_eq!(
            launched(&["ags", "--shim", "claude", "--shim", "-p", "x"]),
            (None, ["--shim", "-p", "x"].map(str::to_owned).to_vec())
        );
        assert!(!parse(&["ags", "claude"]).unwrap().shim);
        let help = Args::command().render_long_help().to_string();
        assert!(!help.contains("--shim"), "{help}");
    }

    #[test]
    fn init_takes_a_known_shell() {
        assert!(matches!(
            parse(&["ags", "init", "fish"]).unwrap().command,
            Command::Init { shell: Shell::Fish }
        ));
        assert!(parse(&["ags", "init", "tcsh"]).is_err());
        assert!(parse(&["ags", "init"]).is_err());
    }

    #[test]
    fn shares_one_version_with_the_server_and_the_wire_contract() {
        let root: toml::Table = include_str!("../../../Cargo.toml")
            .parse()
            .expect("the workspace manifest parses");
        let workspace_version = root["workspace"]["package"]["version"]
            .as_str()
            .expect("the workspace states its version");
        let server: toml::Table = include_str!("../../aegis/Cargo.toml")
            .parse()
            .expect("the server manifest parses");

        assert_eq!(AGS_VERSION, workspace_version);
        assert_eq!(aegis_wire::VERSION, workspace_version);
        assert_eq!(
            server["package"]["version"]["workspace"].as_bool(),
            Some(true)
        );
    }

    #[test]
    fn has_a_valid_command_definition() {
        Args::command().debug_assert();
    }
}
