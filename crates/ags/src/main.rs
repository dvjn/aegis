use std::{
    ffi::OsString,
    io::{BufRead, IsTerminal},
    process::{ExitCode, ExitStatus, Stdio},
};

use ags::{Approval, Cli, CliDefault, Logout, Offer, Pick, Store, provider_text};
use anyhow::bail;
use clap::{Parser, Subcommand, ValueEnum};

/// Command-line client for Aegis.
#[derive(Parser)]
#[command(name = "ags", version, arg_required_else_help = true)]
struct Args {
    /// Use this provider for one launch, in place of the CLI's default
    #[arg(short, long, env = "AGS_PROVIDER")]
    provider: Option<String>,
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
    /// Set the provider a CLI uses, or list the provider of each CLI
    Use {
        #[arg(requires = "provider")]
        cli: Option<Cli>,
        provider: Option<String>,
    },
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

async fn launch(
    store: &Store,
    cli: Cli,
    provider: Option<String>,
    args: Vec<OsString>,
) -> anyhow::Result<Exit> {
    // An empty AGS_PROVIDER counts as unset.
    let provider = provider.as_deref().filter(|id| !id.is_empty());
    let offer = match ags::pick(store, cli, provider).await? {
        Pick::Use(offer) => offer,
        Pick::Ask(offers) => {
            let offer = ask(cli, offers)?;
            ags::remember(store, cli, &offer.id)?;
            eprintln!(
                "{cli} now uses {}. Change it with `ags use {cli} <provider>`.",
                offer.id
            );
            offer
        }
    };
    let launched = ags::launch(store, cli, &offer, args, None).await?;
    if let Some(error) = launched.logged_out {
        eprintln!("ags: {error:#}");
    }
    Ok(Exit::As(launched.status))
}

fn ask(cli: Cli, offers: Vec<Offer>) -> anyhow::Result<Offer> {
    if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
        bail!(
            "{cli} can run on several providers; choose one with `ags use {cli} <provider>`, or for one launch with `ags -p <provider> {cli}` or AGS_PROVIDER"
        );
    }
    eprintln!("Which provider should {cli} use?");
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
        let provider_type = match (&row.provider, &row.provider_type) {
            (_, Some(provider_type)) => provider_type,
            (Some(_), None) => "not granted",
            (None, None) => "",
        };
        lines.push([&row.cli.to_string(), provider, provider_type].map(str::to_owned));
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
    match args.command {
        Command::Claude { args: cli_args } => {
            return launch(&store, Cli::Claude, args.provider, cli_args).await;
        }
        Command::Codex { args: cli_args } => {
            return launch(&store, Cli::Codex, args.provider, cli_args).await;
        }
        Command::Pi { args: cli_args } => {
            return launch(&store, Cli::Pi, args.provider, cli_args).await;
        }
        Command::Use {
            cli: Some(cli),
            provider: Some(provider),
        } => {
            ags::use_provider(&store, cli, &provider).await?;
            println!("{cli} now uses {provider}.");
        }
        Command::Use { .. } => match ags::cli_defaults(&store).await? {
            Some(rows) => println!("{}", defaults_table(&rows)),
            None => {
                println!("Not logged in. Run `ags login <URL>` to pair this machine.");
                return Ok(Exit::Code(ExitCode::FAILURE));
            }
        },
        Command::Login { url } => {
            let status = ags::login(&store, &url, show_approval).await?;
            println!("Logged in to {} as {}.", status.server, status.account);
            println!("Providers: {}", provider_text(&status.providers));
        }
        Command::Logout => match ags::logout(&store).await? {
            None => println!("Not logged in."),
            Some(Logout::Revoked { server }) => println!("Logged out of {server}."),
            Some(Logout::Forgotten { server, reason }) => {
                println!("Logged out of {server}.");
                eprintln!(
                    "ags: could not revoke this machine's access ({reason}); disconnect it under Connected apps on {server}/account"
                );
                return Ok(Exit::Code(ExitCode::FAILURE));
            }
        },
        Command::Status => match ags::status(&store).await? {
            Some(status) => println!("{status}"),
            None => {
                println!("Not logged in. Run `ags login <URL>` to pair this machine.");
                return Ok(Exit::Code(ExitCode::FAILURE));
            }
        },
    }
    Ok(Exit::Code(ExitCode::SUCCESS))
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

    use ags::{Cli, CliDefault};
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
    fn use_lists_each_cli_with_its_provider() {
        let rows = [
            CliDefault {
                cli: Cli::Claude,
                provider: Some("claude".into()),
                provider_type: Some(aegis_wire::provider_type::CLAUDE_SUBSCRIPTION.into()),
            },
            CliDefault {
                cli: Cli::Codex,
                provider: None,
                provider_type: None,
            },
            CliDefault {
                cli: Cli::Pi,
                provider: Some("gone".into()),
                provider_type: None,
            },
        ];
        assert_eq!(
            defaults_table(&rows),
            "CLI     PROVIDER  TYPE\n\
             claude  claude    claude_subscription\n\
             codex   —\n\
             pi      gone      not granted"
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
    fn login_takes_the_server_url() {
        let cli = Args::try_parse_from(["ags", "login", "https://aegis.example"]).unwrap();
        assert!(matches!(cli.command, Command::Login { url } if url == "https://aegis.example"));
        assert!(Args::try_parse_from(["ags", "login"]).is_err());
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
