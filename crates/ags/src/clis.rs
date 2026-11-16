//! The coding CLIs ags launches: the provider types each runs on, and how each reaches the proxy.
//!
//! Each CLI gets the proxy's secret from its environment and sends it as a header, never on its
//! command line, which every local user can read.
use std::{ffi::OsString, fmt, path::Path};

use aegis_wire::provider_type::{CLAUDE_SUBSCRIPTION, CODEX_SUBSCRIPTION};
use anyhow::{Result, bail};
use clap::ValueEnum;

use crate::proxy::SECRET_HEADER;

/// Carries the proxy's secret to the CLI.
const SECRET_ENV: &str = "AGS_LAUNCH_SECRET";
pub(crate) const PI_EXTENSION_NAME: &str = "pi-extension.js";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum Cli {
    Claude,
    Codex,
    Pi,
}

impl Cli {
    /// The provider types this CLI can run on.
    pub fn provider_types(self) -> &'static [&'static str] {
        match self {
            Self::Claude => &[CLAUDE_SUBSCRIPTION],
            Self::Codex => &[CODEX_SUBSCRIPTION],
            Self::Pi => &[CLAUDE_SUBSCRIPTION, CODEX_SUBSCRIPTION],
        }
    }

    pub fn runs_on(self, provider_type: &str) -> bool {
        self.provider_types().contains(&provider_type)
    }

    /// The path the CLI adds after its base URL that the gateway does not expect.
    pub(crate) fn base_path(self, provider_type: &str) -> &'static str {
        match (self, provider_type) {
            // pi appends only `/responses` to a base URL that ends in `/codex`.
            (Self::Pi, CODEX_SUBSCRIPTION) => "/codex",
            _ => "",
        }
    }

    /// Points the CLI at the proxy's `base_url` for a provider of `provider_type`.
    /// `inherited` reads the environment ags was started with.
    pub(crate) fn setup(
        self,
        provider_type: &str,
        base_url: &str,
        secret: &str,
        dir: &Path,
        inherited: impl Fn(&str) -> Option<String>,
    ) -> Result<Setup> {
        let mut setup = Setup::default();
        match (self, provider_type) {
            (Self::Claude, CLAUDE_SUBSCRIPTION) => {
                // A launch from inside another launch inherits that launch's secret.
                let mut headers: Vec<String> = inherited("ANTHROPIC_CUSTOM_HEADERS")
                    .unwrap_or_default()
                    .lines()
                    .filter(|line| {
                        !line.split_once(':').is_some_and(|(name, _)| {
                            name.trim().eq_ignore_ascii_case(SECRET_HEADER)
                        })
                    })
                    .map(str::to_owned)
                    .collect();
                headers.push(format!("{SECRET_HEADER}: {secret}"));
                let headers = headers.join("\n");
                setup.env.extend([
                    ("ANTHROPIC_BASE_URL", base_url.to_owned()),
                    ("ANTHROPIC_CUSTOM_HEADERS", headers),
                ]);
                // Each of these would send Claude Code to a cloud instead of its base URL.
                setup.unset = &[
                    "CLAUDE_CODE_USE_BEDROCK",
                    "CLAUDE_CODE_USE_VERTEX",
                    "CLAUDE_CODE_USE_FOUNDRY",
                ];
            }
            (Self::Codex, CODEX_SUBSCRIPTION) => {
                let base_url = toml_string(base_url);
                setup.args = [
                    "model_provider=\"ags\"".to_owned(),
                    "model_providers.ags.name=\"Aegis\"".to_owned(),
                    format!("model_providers.ags.base_url={base_url}"),
                    "model_providers.ags.wire_api=\"responses\"".to_owned(),
                    "model_providers.ags.requires_openai_auth=true".to_owned(),
                    format!(
                        "model_providers.ags.env_http_headers={{ {} = {} }}",
                        toml_string(SECRET_HEADER),
                        toml_string(SECRET_ENV)
                    ),
                    // Aegis forwards HTTP only, so Codex would retry a WebSocket several times first.
                    "model_providers.ags.supports_websockets=false".to_owned(),
                ]
                .into_iter()
                .flat_map(|value| [OsString::from("-c"), value.into()])
                .collect();
                setup.env.push((SECRET_ENV, secret.to_owned()));
            }
            (Self::Pi, CLAUDE_SUBSCRIPTION | CODEX_SUBSCRIPTION) => {
                let source = pi_extension();
                let extension = dir.join(PI_EXTENSION_NAME);
                if std::fs::read(&extension).ok().as_deref() != Some(source.as_bytes()) {
                    crate::store::write_private(dir, PI_EXTENSION_NAME, source.as_bytes())?;
                }
                let provider = if provider_type == CLAUDE_SUBSCRIPTION {
                    "anthropic"
                } else {
                    "openai-codex"
                };
                setup.args = vec!["--extension".into(), extension.into()];
                setup.env.extend([
                    ("AGS_PI_PROVIDER", provider.to_owned()),
                    ("AGS_PI_BASE_URL", base_url.to_owned()),
                    (SECRET_ENV, secret.to_owned()),
                ]);
            }
            _ => bail!("{self} can't run on a {provider_type} provider"),
        }
        Ok(setup)
    }
}

impl fmt::Display for Cli {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = self.to_possible_value().expect("every CLI has a name");
        f.write_str(value.get_name())
    }
}

/// What a launch adds to the CLI's command line and environment.
#[derive(Debug, Default)]
pub(crate) struct Setup {
    /// Goes before the user's own arguments.
    pub args: Vec<OsString>,
    pub env: Vec<(&'static str, String)>,
    pub unset: &'static [&'static str],
}

fn pi_extension() -> String {
    format!(
        "\
// Written by ags: sends one pi provider through this launch's Aegis proxy.
export default function (pi) {{
  const provider = process.env.AGS_PI_PROVIDER;
  const baseUrl = process.env.AGS_PI_BASE_URL;
  const secret = process.env.{SECRET_ENV};
  if (provider && baseUrl && secret) {{
    pi.registerProvider(provider, {{ baseUrl, headers: {{ \"{SECRET_HEADER}\": secret }} }});
  }}
}}
"
    )
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).expect("a string serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_wire::provider_type::BEDROCK;

    const BASE_URL: &str = "http://127.0.0.1:1";
    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    fn setup(cli: Cli, provider_type: &str, dir: &Path) -> Setup {
        cli.setup(provider_type, BASE_URL, SECRET, dir, |_| None)
            .unwrap()
    }

    /// The secret may be in the environment, never on the command line.
    fn secret_only_in_env(setup: &Setup) {
        assert!(
            setup
                .args
                .iter()
                .all(|arg| !arg.to_string_lossy().contains(SECRET)),
            "{:?}",
            setup.args
        );
        assert!(setup.env.iter().any(|(_, value)| value.contains(SECRET)));
    }

    #[test]
    fn every_cli_can_be_set_up_for_every_type_it_runs_on() {
        let dir = crate::store::tests::scratch();
        for cli in Cli::value_variants() {
            assert_eq!(Cli::from_str(&cli.to_string(), false).unwrap(), *cli);
            for provider_type in cli.provider_types() {
                assert!(cli.runs_on(provider_type));
                secret_only_in_env(&setup(*cli, provider_type, &dir));
            }
            assert!(!cli.runs_on(BEDROCK));
            assert!(
                cli.setup(BEDROCK, BASE_URL, SECRET, &dir, |_| None)
                    .is_err()
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn claude_code_sends_the_secret_as_a_custom_header_and_never_uses_a_cloud() {
        let dir = crate::store::tests::scratch();
        let setup = setup(Cli::Claude, CLAUDE_SUBSCRIPTION, &dir);
        assert!(setup.args.is_empty());
        assert_eq!(
            setup.env,
            [
                ("ANTHROPIC_BASE_URL", BASE_URL.to_owned()),
                (
                    "ANTHROPIC_CUSTOM_HEADERS",
                    format!("{SECRET_HEADER}: {SECRET}")
                ),
            ]
        );
        assert!(setup.unset.contains(&"CLAUDE_CODE_USE_BEDROCK"));
        let theirs = Cli::Claude
            .setup(CLAUDE_SUBSCRIPTION, BASE_URL, SECRET, &dir, |name| {
                (name == "ANTHROPIC_CUSTOM_HEADERS").then(|| "x-team: blue".to_owned())
            })
            .unwrap();
        assert_eq!(
            theirs.env[1].1,
            format!("x-team: blue\n{SECRET_HEADER}: {SECRET}"),
            "the user's own headers are kept"
        );
        let nested = Cli::Claude
            .setup(CLAUDE_SUBSCRIPTION, BASE_URL, SECRET, &dir, |_| {
                Some("X-Ags-Launch-Secret: outer\nx-team: blue".to_owned())
            })
            .unwrap();
        assert_eq!(
            nested.env[1].1,
            format!("x-team: blue\n{SECRET_HEADER}: {SECRET}"),
            "an outer launch's secret is replaced"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn codex_gets_an_http_provider_that_reads_the_secret_from_its_environment() {
        let dir = crate::store::tests::scratch();
        let setup = setup(Cli::Codex, CODEX_SUBSCRIPTION, &dir);
        let args: Vec<_> = setup.args.iter().map(|arg| arg.to_str().unwrap()).collect();
        assert_eq!(args[0], "-c");
        assert!(args.contains(&"model_provider=\"ags\""));
        assert!(args.contains(&"model_providers.ags.base_url=\"http://127.0.0.1:1\""));
        assert!(args.contains(
            &"model_providers.ags.env_http_headers={ \"x-ags-launch-secret\" = \"AGS_LAUNCH_SECRET\" }"
        ));
        assert!(args.contains(&"model_providers.ags.supports_websockets=false"));
        assert_eq!(setup.env, [(SECRET_ENV, SECRET.to_owned())]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pi_loads_an_extension_that_moves_one_provider() {
        let dir = crate::store::tests::scratch();
        let codex = setup(Cli::Pi, CODEX_SUBSCRIPTION, &dir);
        let extension = dir.join(PI_EXTENSION_NAME);
        assert_eq!(
            codex.args,
            [OsString::from("--extension"), extension.clone().into()]
        );
        let source = std::fs::read_to_string(&extension).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // A rewrite renames a new file into place, which changes the inode.
            let inode = || std::fs::metadata(&extension).unwrap().ino();
            let written = inode();
            setup(Cli::Pi, CODEX_SUBSCRIPTION, &dir);
            assert_eq!(inode(), written, "an unchanged extension is not rewritten");
        }
        assert!(source.contains("process.env.AGS_LAUNCH_SECRET"), "{source}");
        assert!(
            source.contains("\"x-ags-launch-secret\": secret"),
            "{source}"
        );
        assert!(
            codex
                .env
                .contains(&("AGS_PI_PROVIDER", "openai-codex".to_owned()))
        );
        assert_eq!(Cli::Pi.base_path(CODEX_SUBSCRIPTION), "/codex");
        assert_eq!(Cli::Pi.base_path(CLAUDE_SUBSCRIPTION), "");
        let claude = setup(Cli::Pi, CLAUDE_SUBSCRIPTION, &dir);
        assert!(
            claude
                .env
                .contains(&("AGS_PI_PROVIDER", "anthropic".to_owned()))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
