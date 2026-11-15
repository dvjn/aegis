use clap::Parser;

/// Command-line client for Aegis.
#[derive(Parser)]
#[command(name = "ags", version, arg_required_else_help = true)]
struct Cli {}

fn main() {
    Cli::parse();
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser, error::ErrorKind};

    use super::Cli;

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
