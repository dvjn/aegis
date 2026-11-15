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
            .expect("the root manifest parses");
        let server_version = root["package"]["version"]
            .as_str()
            .expect("the server states its version");

        assert_eq!(AGS_VERSION, server_version);
        assert_eq!(aegis_wire::VERSION, server_version);
    }

    #[test]
    fn has_a_valid_command_definition() {
        Cli::command().debug_assert();
    }
}
