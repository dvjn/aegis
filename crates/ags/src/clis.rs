//! The coding CLIs ags launches: the provider types each runs on, and how each reaches the proxy.
//!
//! Each CLI gets the proxy's secret from its environment and sends it as a header, never on its
//! command line, which every local user can read.
use std::{
    ffi::{OsStr, OsString},
    fmt,
    path::Path,
};

use aegis_wire::provider_type::{BEDROCK, CLAUDE_SUBSCRIPTION, CODEX_SUBSCRIPTION, VERTEX};
use anyhow::{Result, bail};
use clap::ValueEnum;

use crate::{credentials::on_path, proxy::SECRET_HEADER, store::create_private_dir};

/// Carries the proxy's secret to the CLI.
pub(crate) const SECRET_ENV: &str = "AGS_LAUNCH_SECRET";
/// Codex has no Google sign-in for a custom provider, so it asks gcloud for each token.
const GCLOUD: &str = "gcloud";
/// gcloud's own config for those tokens, empty so that no impersonation or account of the
/// user's replaces the provider's service account.
const GCLOUD_CONFIG: &str = "gcloud";
/// What would make gcloud's token another account's than the provider's service account.
const GCLOUD_ACCOUNT_VARS: &[&str] = &[
    "CLOUDSDK_AUTH_IMPERSONATE_SERVICE_ACCOUNT",
    "CLOUDSDK_AUTH_ACCESS_TOKEN_FILE",
    "CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE",
    "CLOUDSDK_ACTIVE_CONFIG_NAME",
];
/// Claude Code's region for one model on Vertex, by the model's name after it.
const CLAUDE_MODEL_REGION: &str = "VERTEX_REGION_CLAUDE_";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum Cli {
    Claude,
    Codex,
    Pi,
}

/// The provider a launch runs on, as far as its CLI needs to know it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Upstream<'a> {
    pub id: &'a str,
    pub provider_type: &'a str,
    /// A Bedrock provider's AWS region or a Vertex provider's location, as the server reports it.
    pub region: Option<&'a str>,
    /// A Vertex provider's Google Cloud project.
    pub project: Option<&'a str>,
}

impl Cli {
    /// The provider types this CLI can run on.
    pub fn provider_types(self) -> &'static [&'static str] {
        match self {
            Self::Claude => &[CLAUDE_SUBSCRIPTION, BEDROCK, VERTEX],
            Self::Codex => &[CODEX_SUBSCRIPTION, BEDROCK, VERTEX],
            Self::Pi => &[CLAUDE_SUBSCRIPTION, CODEX_SUBSCRIPTION, BEDROCK, VERTEX],
        }
    }

    pub fn runs_on(self, provider_type: &str) -> bool {
        self.provider_types().contains(&provider_type)
    }

    /// Why this CLI can't run on provider `id`, or `None` when it can. `region` is the region the
    /// server reports, `reported` whether the server reports regions at all, and `path` the value
    /// of `PATH`.
    pub fn refusal(
        self,
        id: &str,
        provider_type: &str,
        region: Option<&str>,
        reported: bool,
        path: Option<&OsStr>,
    ) -> Option<String> {
        let refused = |why: &str| Some(format!("{self} can't run on {id}{why}"));
        if !self.runs_on(provider_type) {
            return refused(&format!(", a {provider_type} provider"));
        }
        match (self, provider_type) {
            // Aegis maps Codex's OpenAI-compatible API only for a standard regional Bedrock endpoint.
            (Self::Codex, BEDROCK) if region.is_none() && !reported => {
                refused(" until the Aegis server is updated to name its region")
            }
            (Self::Codex, BEDROCK) if region.is_none() => refused(&format!(
                ", a {provider_type} provider without a regional endpoint"
            )),
            (Self::Codex, VERTEX) if !cfg!(unix) => {
                refused(": ags runs Codex on Vertex only on Linux and macOS")
            }
            (Self::Codex, VERTEX) if !on_path(GCLOUD, path) => refused(
                ": Codex signs in to Vertex with gcloud, which is not on PATH; install the Google Cloud CLI",
            ),
            _ => None,
        }
    }

    /// The path the CLI adds after its base URL that the gateway does not expect.
    pub(crate) fn base_path(self, provider_type: &str) -> &'static str {
        match (self, provider_type) {
            // pi appends only `/responses` to a base URL that ends in `/codex`.
            (Self::Pi, CODEX_SUBSCRIPTION) => "/codex",
            _ => "",
        }
    }

    /// Points the CLI at the proxy's `base_url` for `upstream`.
    /// `inherited` reads the environment ags was started with.
    pub(crate) fn setup(
        self,
        upstream: Upstream<'_>,
        base_url: &str,
        secret: &str,
        dir: &Path,
        inherited: impl Fn(&str) -> Option<String>,
    ) -> Result<Setup> {
        let inherited = |name: &str| inherited(name).filter(|value| !value.is_empty());
        let provider_type = upstream.provider_type;
        let path = inherited("PATH").map(OsString::from);
        if let Some(refusal) = self.refusal(
            upstream.id,
            provider_type,
            upstream.region,
            true,
            path.as_deref(),
        ) {
            bail!(refusal);
        }
        let mut setup = Setup::default();
        match self {
            Self::Claude => {
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
                match provider_type {
                    BEDROCK => setup.env.extend([
                        ("CLAUDE_CODE_USE_BEDROCK", "1".to_owned()),
                        ("ANTHROPIC_BEDROCK_BASE_URL", base_url.to_owned()),
                        ("AWS_REGION", aws_region(self, upstream, inherited)?),
                    ]),
                    VERTEX => {
                        let (project, location) = vertex_location(
                            upstream,
                            "CLOUD_ML_REGION",
                            inherited,
                            &mut setup.notes,
                        )?;
                        setup.env.extend([
                            ("CLAUDE_CODE_USE_VERTEX", "1".to_owned()),
                            ("ANTHROPIC_VERTEX_BASE_URL", base_url.to_owned()),
                            ("ANTHROPIC_VERTEX_PROJECT_ID", project.to_owned()),
                            ("CLOUD_ML_REGION", location),
                        ]);
                    }
                    _ => setup.env.push(("ANTHROPIC_BASE_URL", base_url.to_owned())),
                }
                setup
                    .env
                    .push(("ANTHROPIC_CUSTOM_HEADERS", headers.join("\n")));
            }
            Self::Codex => {
                let (base_url, auth) = match provider_type {
                    BEDROCK => (
                        format!("{base_url}/openai/v1"),
                        "env_key=\"AWS_BEARER_TOKEN_BEDROCK\"".to_owned(),
                    ),
                    VERTEX => {
                        let (project, location) = vertex_location(
                            upstream,
                            "GOOGLE_CLOUD_LOCATION",
                            inherited,
                            &mut setup.notes,
                        )?;
                        let config = dir.join(GCLOUD_CONFIG);
                        create_private_dir(&config)?;
                        let Some(config) = config.to_str() else {
                            bail!("{} is not a path codex can be given", config.display());
                        };
                        // `env` clears the user's gcloud account for the token command only.
                        let mut command = vec!["env".to_owned()];
                        for name in GCLOUD_ACCOUNT_VARS {
                            command.extend(["-u".to_owned(), (*name).to_owned()]);
                        }
                        command.extend(
                            [
                                &format!("CLOUDSDK_CONFIG={config}"),
                                GCLOUD,
                                "auth",
                                "application-default",
                                "print-access-token",
                            ]
                            .map(str::to_owned),
                        );
                        let args: Vec<String> =
                            command[1..].iter().map(|arg| toml_string(arg)).collect();
                        (
                            format!(
                                "{base_url}/v1/projects/{project}/locations/{location}/publishers/openai/v1"
                            ),
                            format!(
                                "auth={{ command = {}, args = [{}] }}",
                                toml_string(&command[0]),
                                args.join(", ")
                            ),
                        )
                    }
                    _ => (base_url.to_owned(), "requires_openai_auth=true".to_owned()),
                };
                setup.args = [
                    "model_provider=\"ags\"".to_owned(),
                    "model_providers.ags.name=\"Aegis\"".to_owned(),
                    format!("model_providers.ags.base_url={}", toml_string(&base_url)),
                    "model_providers.ags.wire_api=\"responses\"".to_owned(),
                    format!("model_providers.ags.{auth}"),
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
            // pi reaches several providers at once, so a pi launch sets it up by its routes.
            Self::Pi => bail!("pi is set up by the routes of its launch"),
        }
        Ok(setup)
    }
}

/// The region of the provider's endpoint, else the one the user's AWS setup names.
pub(crate) fn aws_region(
    cli: Cli,
    upstream: Upstream<'_>,
    inherited: impl Fn(&str) -> Option<String>,
) -> Result<String> {
    if let Some(region) = upstream.region {
        return Ok(region.to_owned());
    }
    match inherited("AWS_REGION").or_else(|| inherited("AWS_DEFAULT_REGION")) {
        Some(region) => Ok(region),
        None => bail!(
            "{cli} needs the AWS region of {}, which the server does not name; set AWS_REGION",
            upstream.id
        ),
    }
}

/// The project and location of a Vertex request path. A provider whose host serves one location
/// uses it, with a note when `variable` names another; otherwise `variable` decides, else `global`.
pub(crate) fn vertex_location<'a>(
    upstream: Upstream<'a>,
    variable: &str,
    inherited: impl Fn(&str) -> Option<String>,
    notes: &mut Vec<String>,
) -> Result<(&'a str, String)> {
    let Some(project) = upstream.project else {
        bail!("{} names no Google Cloud project", upstream.id);
    };
    let theirs = inherited(variable);
    let location = match (upstream.region, theirs) {
        (Some(fixed), Some(theirs)) if theirs != fixed => {
            notes.push(ignored(upstream.id, fixed, variable, &theirs));
            fixed.to_owned()
        }
        (Some(fixed), _) => fixed.to_owned(),
        (None, theirs) => theirs.unwrap_or_else(|| "global".to_owned()),
    };
    if !location
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        bail!("{variable} is not a Google Cloud location: {location:?}");
    }
    Ok((project, location))
}

fn ignored(id: &str, location: &str, variable: &str, value: &str) -> String {
    format!("{id} serves only the {location} location, so {variable}={value} is ignored")
}

/// Claude Code's per-model Vertex regions in `vars` that differ from `location`, which provider `id`
/// is fixed at, each with a note that it is ignored.
pub(crate) fn stray_model_regions(
    id: &str,
    location: &str,
    vars: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, String)> {
    vars.into_iter()
        .filter_map(|(name, value)| {
            let text = name.to_str()?;
            let value = value.to_string_lossy();
            (text.starts_with(CLAUDE_MODEL_REGION) && value != location)
                .then(|| (name.clone(), ignored(id, location, text, &value)))
        })
        .collect()
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
    /// What the user should hear about this launch's setup.
    pub notes: Vec<String>,
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).expect("a string serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_wire::provider_type::TYPESAFE;

    const BASE_URL: &str = "http://127.0.0.1:1";
    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    /// A provider of `provider_type`; a Bedrock one is in `eu-west-1`, a Vertex one at every location.
    fn upstream(provider_type: &str) -> Upstream<'_> {
        Upstream {
            id: "work",
            provider_type,
            region: (provider_type == BEDROCK).then_some("eu-west-1"),
            project: Some("proj"),
        }
    }

    fn setup(cli: Cli, provider_type: &str, dir: &Path) -> Setup {
        cli.setup(upstream(provider_type), BASE_URL, SECRET, dir, |_| None)
            .unwrap()
    }

    fn env<'a>(setup: &'a Setup, name: &str) -> Option<&'a str> {
        setup
            .env
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }

    fn args(setup: &Setup) -> Vec<&str> {
        setup.args.iter().map(|arg| arg.to_str().unwrap()).collect()
    }

    /// A dir holding an executable `gcloud`, for a `PATH` that has it.
    fn gcloud(dir: &Path) -> String {
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join(GCLOUD), "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(bin.join(GCLOUD), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        bin.to_str().unwrap().to_owned()
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
        let path = gcloud(&dir);
        for cli in Cli::value_variants() {
            assert_eq!(Cli::from_str(&cli.to_string(), false).unwrap(), *cli);
            for provider_type in cli.provider_types() {
                assert!(cli.runs_on(provider_type));
                // A pi launch is set up by its routes; see the pi module.
                if *cli == Cli::Pi {
                    continue;
                }
                let setup = cli
                    .setup(upstream(provider_type), BASE_URL, SECRET, &dir, |name| {
                        (name == "PATH").then(|| path.clone())
                    })
                    .unwrap();
                secret_only_in_env(&setup);
            }
            for provider_type in [BEDROCK, VERTEX] {
                assert!(cli.runs_on(provider_type), "{cli} runs on {provider_type}");
            }
            assert!(!cli.runs_on(TYPESAFE));
            let refused = cli
                .setup(upstream(TYPESAFE), BASE_URL, SECRET, &dir, |_| None)
                .unwrap_err();
            assert_eq!(
                refused.to_string(),
                format!("{cli} can't run on work, a typesafe provider")
            );
        }
        assert!(!Cli::Claude.runs_on(CODEX_SUBSCRIPTION));
        assert!(!Cli::Codex.runs_on(CLAUDE_SUBSCRIPTION));
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
        let theirs = Cli::Claude
            .setup(
                upstream(CLAUDE_SUBSCRIPTION),
                BASE_URL,
                SECRET,
                &dir,
                |name| (name == "ANTHROPIC_CUSTOM_HEADERS").then(|| "x-team: blue".to_owned()),
            )
            .unwrap();
        assert_eq!(
            theirs.env[1].1,
            format!("x-team: blue\n{SECRET_HEADER}: {SECRET}"),
            "the user's own headers are kept"
        );
        let nested = Cli::Claude
            .setup(
                upstream(CLAUDE_SUBSCRIPTION),
                BASE_URL,
                SECRET,
                &dir,
                |_| Some("X-Ags-Launch-Secret: outer\nx-team: blue".to_owned()),
            )
            .unwrap();
        assert_eq!(
            nested.env[1].1,
            format!("x-team: blue\n{SECRET_HEADER}: {SECRET}"),
            "an outer launch's secret is replaced"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn claude_code_reaches_bedrock_in_the_providers_region_through_the_proxy() {
        let dir = crate::store::tests::scratch();
        let bedrock = setup(Cli::Claude, BEDROCK, &dir);
        assert_eq!(
            bedrock.env,
            [
                ("CLAUDE_CODE_USE_BEDROCK", "1".to_owned()),
                ("ANTHROPIC_BEDROCK_BASE_URL", BASE_URL.to_owned()),
                ("AWS_REGION", "eu-west-1".to_owned()),
                (
                    "ANTHROPIC_CUSTOM_HEADERS",
                    format!("{SECRET_HEADER}: {SECRET}")
                ),
            ]
        );

        let custom = Upstream {
            region: None,
            ..upstream(BEDROCK)
        };
        let theirs = Cli::Claude
            .setup(custom, BASE_URL, SECRET, &dir, |name| {
                (name == "AWS_DEFAULT_REGION").then(|| "us-east-2".to_owned())
            })
            .unwrap();
        assert_eq!(env(&theirs, "AWS_REGION"), Some("us-east-2"));
        let none = Cli::Claude
            .setup(custom, BASE_URL, SECRET, &dir, |_| None)
            .unwrap_err();
        assert_eq!(
            none.to_string(),
            "claude needs the AWS region of work, which the server does not name; set AWS_REGION"
        );
        let mine = Cli::Claude
            .setup(upstream(BEDROCK), BASE_URL, SECRET, &dir, |name| {
                (name == "AWS_REGION").then(|| "us-east-2".to_owned())
            })
            .unwrap();
        assert_eq!(
            env(&mine, "AWS_REGION"),
            Some("eu-west-1"),
            "the provider's endpoint decides the region"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn claude_code_reaches_vertex_in_the_accounts_project() {
        let dir = crate::store::tests::scratch();
        let vertex = setup(Cli::Claude, VERTEX, &dir);
        assert_eq!(env(&vertex, "CLAUDE_CODE_USE_VERTEX"), Some("1"));
        assert_eq!(env(&vertex, "ANTHROPIC_VERTEX_BASE_URL"), Some(BASE_URL));
        assert_eq!(env(&vertex, "ANTHROPIC_VERTEX_PROJECT_ID"), Some("proj"));
        assert_eq!(env(&vertex, "CLOUD_ML_REGION"), Some("global"));
        assert!(env(&vertex, "ANTHROPIC_CUSTOM_HEADERS").is_some());
        let regional = Cli::Claude
            .setup(upstream(VERTEX), BASE_URL, SECRET, &dir, |name| {
                (name == "CLOUD_ML_REGION").then(|| "us-east5".to_owned())
            })
            .unwrap();
        assert_eq!(env(&regional, "CLOUD_ML_REGION"), Some("us-east5"));
        let located = Upstream {
            region: Some("europe-west4"),
            ..upstream(VERTEX)
        };
        let server = Cli::Claude
            .setup(located, BASE_URL, SECRET, &dir, |_| None)
            .unwrap();
        assert_eq!(
            env(&server, "CLOUD_ML_REGION"),
            Some("europe-west4"),
            "the provider's location replaces global"
        );
        assert!(server.notes.is_empty());
        let mine = Cli::Claude
            .setup(located, BASE_URL, SECRET, &dir, |name| {
                (name == "CLOUD_ML_REGION").then(|| "us-east5".to_owned())
            })
            .unwrap();
        assert_eq!(
            env(&mine, "CLOUD_ML_REGION"),
            Some("europe-west4"),
            "a provider fixed at one location keeps it"
        );
        assert_eq!(
            mine.notes,
            ["work serves only the europe-west4 location, so CLOUD_ML_REGION=us-east5 is ignored"]
        );
        let same = Cli::Claude
            .setup(located, BASE_URL, SECRET, &dir, |name| {
                (name == "CLOUD_ML_REGION").then(|| "europe-west4".to_owned())
            })
            .unwrap();
        assert!(same.notes.is_empty());
        let models = stray_model_regions(
            "work",
            "europe-west4",
            [
                ("VERTEX_REGION_CLAUDE_HAIKU_4_5", "us-east5"),
                ("VERTEX_REGION_CLAUDE_5_5_OPUS", "europe-west4"),
                ("CLOUD_ML_REGION", "us-east5"),
            ]
            .map(|(name, value)| (OsString::from(name), OsString::from(value))),
        );
        assert_eq!(
            models,
            [(
                OsString::from("VERTEX_REGION_CLAUDE_HAIKU_4_5"),
                "work serves only the europe-west4 location, so VERTEX_REGION_CLAUDE_HAIKU_4_5=us-east5 is ignored".to_owned()
            )]
        );
        let odd = Cli::Claude
            .setup(upstream(VERTEX), BASE_URL, SECRET, &dir, |name| {
                (name == "CLOUD_ML_REGION").then(|| "../x".to_owned())
            })
            .unwrap_err();
        assert!(
            odd.to_string().starts_with("CLOUD_ML_REGION is not"),
            "{odd}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn codex_gets_an_http_provider_that_reads_the_secret_from_its_environment() {
        let dir = crate::store::tests::scratch();
        let setup = setup(Cli::Codex, CODEX_SUBSCRIPTION, &dir);
        let args = args(&setup);
        assert_eq!(args[0], "-c");
        assert!(args.contains(&"model_provider=\"ags\""));
        assert!(args.contains(&"model_providers.ags.base_url=\"http://127.0.0.1:1\""));
        assert!(args.contains(&"model_providers.ags.requires_openai_auth=true"));
        assert!(args.contains(
            &"model_providers.ags.env_http_headers={ \"x-ags-launch-secret\" = \"AGS_LAUNCH_SECRET\" }"
        ));
        assert!(args.contains(&"model_providers.ags.supports_websockets=false"));
        assert_eq!(setup.env, [(SECRET_ENV, SECRET.to_owned())]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn codex_reaches_each_clouds_openai_compatible_api() {
        let dir = crate::store::tests::scratch();
        let bedrock = setup(Cli::Codex, BEDROCK, &dir);
        let bedrock_args = args(&bedrock);
        assert!(
            bedrock_args.contains(&"model_providers.ags.base_url=\"http://127.0.0.1:1/openai/v1\"")
        );
        assert!(bedrock_args.contains(&"model_providers.ags.env_key=\"AWS_BEARER_TOKEN_BEDROCK\""));
        assert!(
            !bedrock_args
                .iter()
                .any(|arg| arg.contains("requires_openai_auth"))
        );
        let custom = Upstream {
            region: None,
            ..upstream(BEDROCK)
        };
        let refused = Cli::Codex
            .setup(custom, BASE_URL, SECRET, &dir, |_| None)
            .unwrap_err();
        assert_eq!(
            refused.to_string(),
            "codex can't run on work, a bedrock provider without a regional endpoint"
        );
        for cli in [Cli::Claude, Cli::Pi] {
            assert!(cli.refusal("work", BEDROCK, None, false, None).is_none());
            assert!(cli.refusal("work", VERTEX, None, false, None).is_none());
        }

        let refused = Cli::Codex
            .setup(upstream(VERTEX), BASE_URL, SECRET, &dir, |_| None)
            .unwrap_err();
        assert_eq!(
            refused.to_string(),
            "codex can't run on work: Codex signs in to Vertex with gcloud, which is not on PATH; install the Google Cloud CLI"
        );
        let path = gcloud(&dir);
        let vertex = Cli::Codex
            .setup(upstream(VERTEX), BASE_URL, SECRET, &dir, |name| {
                (name == "PATH").then(|| path.clone())
            })
            .unwrap();
        let vertex_args = args(&vertex);
        assert!(vertex_args.contains(
            &"model_providers.ags.base_url=\"http://127.0.0.1:1/v1/projects/proj/locations/global/publishers/openai/v1\""
        ));
        let config = dir.join(GCLOUD_CONFIG);
        assert!(config.is_dir());
        assert!(vertex_args.contains(&format!(
            "model_providers.ags.auth={{ command = \"env\", args = [\"-u\", \"CLOUDSDK_AUTH_IMPERSONATE_SERVICE_ACCOUNT\", \"-u\", \"CLOUDSDK_AUTH_ACCESS_TOKEN_FILE\", \"-u\", \"CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE\", \"-u\", \"CLOUDSDK_ACTIVE_CONFIG_NAME\", \"CLOUDSDK_CONFIG={}\", \"gcloud\", \"auth\", \"application-default\", \"print-access-token\"] }}",
            config.display()
        ).as_str()));
        for args in [bedrock_args, vertex_args] {
            let table: String = args
                .iter()
                .filter(|arg| **arg != "-c")
                .map(|arg| format!("{arg}\n"))
                .collect();
            let parsed: toml::Table = table.parse().expect("every setting is TOML");
            assert_eq!(parsed["model_provider"].as_str(), Some("ags"));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
