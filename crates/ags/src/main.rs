use std::process::{ExitCode, Stdio};

use ags::{Approval, Logout, Store, provider_text};
use clap::{Parser, Subcommand};

/// Command-line client for Aegis.
#[derive(Parser)]
#[command(name = "ags", version, arg_required_else_help = true)]
struct Cli {
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
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return fail(&error.into()),
    };
    match runtime.block_on(run(cli.command)) {
        Ok(code) => code,
        Err(error) => fail(&error),
    }
}

fn fail(error: &anyhow::Error) -> ExitCode {
    eprintln!("ags: {error:#}");
    ExitCode::FAILURE
}

async fn run(command: Command) -> anyhow::Result<ExitCode> {
    let store = Store::default_location()?;
    match command {
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
                return Ok(ExitCode::FAILURE);
            }
        },
        Command::Status => match ags::status(&store).await? {
            Some(status) => println!("{status}"),
            None => {
                println!("Not logged in. Run `ags login <URL>` to pair this machine.");
                return Ok(ExitCode::FAILURE);
            }
        },
    }
    Ok(ExitCode::SUCCESS)
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
    use clap::{CommandFactory, Parser, error::ErrorKind};

    use super::{Cli, Command};

    const AGS_VERSION: &str = env!("CARGO_PKG_VERSION");

    #[test]
    fn prints_its_version() {
        let error = Cli::try_parse_from(["ags", "--version"])
            .err()
            .expect("--version stops parsing");

        assert_eq!(error.kind(), ErrorKind::DisplayVersion);
        assert_eq!(error.to_string(), format!("ags {AGS_VERSION}\n"));
    }

    #[test]
    fn prints_help_when_run_without_arguments() {
        let error = Cli::try_parse_from(["ags"])
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
        let cli = Cli::try_parse_from(["ags", "login", "https://aegis.example"]).unwrap();
        assert!(matches!(cli.command, Command::Login { url } if url == "https://aegis.example"));
        assert!(Cli::try_parse_from(["ags", "login"]).is_err());
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
        Cli::command().debug_assert();
    }
}
