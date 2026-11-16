//! The vendor credentials of each provider, kept apart from the user's own and from each other's.
//!
//! A subscription provider gets a config dir per CLI, where that CLI's own sign-in runs. The
//! user's settings are linked into it, so only credentials and session state are separate. A
//! cloud provider keeps its Bedrock API key or Vertex service account in a private file.
use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    fmt, fs, io,
    path::{Path, PathBuf},
    process::ExitStatus,
};

use aegis_wire::provider_type::{BEDROCK, CLAUDE_SUBSCRIPTION, CODEX_SUBSCRIPTION, VERTEX};
use anyhow::{Context, Result, anyhow, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::process::Command;

use crate::{
    Cli, Store,
    launch::granted,
    store::{create_private_dir, write_private},
};

const DIR_NAME: &str = "providers";
const STATE: &str = "state.json";
const BEDROCK_KEY: &str = "bedrock-api-key";
const VERTEX_ACCOUNT: &str = "vertex-service-account.json";

const CLAUDE_CREDENTIALS: &str = ".credentials.json";
const CLAUDE_STATE: &str = ".claude.json";
const CODEX_AUTH: &str = "auth.json";
const PI_AUTH: &str = "auth.json";
/// Keeps Codex's sign-in in `auth.json` under its home, not in a keyring shared by every home.
const CODEX_FILE_STORE: &str = "cli_auth_credentials_store=\"file\"";

/// Linked from the user's own config dir; everything else stays with the provider.
const CLAUDE_SHARED: &[&str] = &[
    "settings.json",
    "CLAUDE.md",
    "rules",
    "agents",
    "commands",
    "skills",
    "output-styles",
    "workflows",
    "keybindings.json",
    "themes",
    "hooks",
    "plugins",
    "statusline.py",
    "statusline.sh",
];
const CODEX_SHARED: &[&str] = &[
    "config.toml",
    "AGENTS.md",
    "AGENTS.override.md",
    "rules",
    "prompts",
    "hooks.json",
    ".hook-trust",
    "skills",
    "plugins",
];
const PI_SHARED: &[&str] = &[
    "settings.json",
    "keybindings.json",
    "mcp.json",
    "models.json",
    "AGENTS.md",
    "CLAUDE.md",
    "SYSTEM.md",
    "APPEND_SYSTEM.md",
    "extensions",
    "skills",
    "prompts",
    "themes",
    "npm",
];
/// Settings Claude Code keeps in `.claude.json` beside its sign-in, copied from the user's own.
const CLAUDE_STATE_SHARED: &[&str] = &[
    "mcpServers",
    "hasCompletedOnboarding",
    "lastOnboardingVersion",
];
const CLAUDE_PROJECT_SHARED: &[&str] = &[
    "mcpServers",
    "enabledMcpjsonServers",
    "disabledMcpjsonServers",
    "hasTrustDialogAccepted",
];

/// One credential a provider needs before a CLI can use it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    /// The CLI's own sign-in, in its own config dir for this provider.
    SignIn(Cli),
    BedrockKey,
    VertexAccount,
}

impl Need {
    /// Everything a provider of `provider_type` needs: one sign-in per CLI that runs on it, or its key.
    pub fn all(provider_type: &str) -> Vec<Self> {
        match provider_type {
            BEDROCK => vec![Self::BedrockKey],
            VERTEX => vec![Self::VertexAccount],
            _ => Cli::value_variants()
                .iter()
                .copied()
                .filter(|cli| cli.runs_on(provider_type))
                .map(Self::SignIn)
                .collect(),
        }
    }

    /// What `cli` needs to run on a provider of `provider_type`.
    pub fn of_launch(cli: Cli, provider_type: &str) -> Option<Self> {
        match provider_type {
            BEDROCK => Some(Self::BedrockKey),
            VERTEX => Some(Self::VertexAccount),
            CLAUDE_SUBSCRIPTION | CODEX_SUBSCRIPTION => Some(Self::SignIn(cli)),
            _ => None,
        }
    }
}

impl Need {
    /// Names this need of `provider` in ags's state.
    fn key(self, provider: &str) -> String {
        let need = match self {
            Self::SignIn(cli) => cli.to_string(),
            Self::BedrockKey => BEDROCK.to_owned(),
            Self::VertexAccount => VERTEX.to_owned(),
        };
        format!("{provider}/{need}")
    }
}

impl fmt::Display for Need {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SignIn(cli) => write!(f, "{} sign-in", product(*cli)),
            Self::BedrockKey => f.write_str("Bedrock API key"),
            Self::VertexAccount => f.write_str("Vertex service account"),
        }
    }
}

/// The name a CLI goes by in prose.
pub fn product(cli: Cli) -> &'static str {
    match cli {
        Cli::Claude => "Claude Code",
        Cli::Codex => "Codex",
        Cli::Pi => "pi",
    }
}

/// Credentials Claude Code reads from its environment before its own sign-in.
const CLAUDE_CREDENTIAL_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_REFRESH_TOKEN",
    "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
    "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
    "CLAUDE_SECURESTORAGE_CONFIG_DIR",
];

/// What picks where Claude Code sends its requests, as Claude Code reads it: each upstream's
/// switch, base URL and skipped sign-in, with the provider type whose launch sets it.
const CLAUDE_ROUTES: &[(&str, Option<&str>)] = &[
    ("ANTHROPIC_BASE_URL", Some(CLAUDE_SUBSCRIPTION)),
    ("CLAUDE_CODE_USE_BEDROCK", Some(BEDROCK)),
    ("ANTHROPIC_BEDROCK_BASE_URL", Some(BEDROCK)),
    ("CLAUDE_CODE_USE_VERTEX", Some(VERTEX)),
    ("ANTHROPIC_VERTEX_BASE_URL", Some(VERTEX)),
    ("CLAUDE_CODE_USE_FOUNDRY", None),
    ("CLAUDE_CODE_USE_ANTHROPIC_AWS", None),
    ("CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD", None),
    ("CLAUDE_CODE_USE_MANTLE", None),
    ("CLAUDE_CODE_USE_GATEWAY", None),
    ("ANTHROPIC_FOUNDRY_BASE_URL", None),
    ("ANTHROPIC_FOUNDRY_RESOURCE", None),
    ("ANTHROPIC_BEDROCK_MANTLE_BASE_URL", None),
    ("ANTHROPIC_AWS_BASE_URL", None),
    ("ANTHROPIC_GOOGLE_CLOUD_BASE_URL", None),
    ("ANTHROPIC_UNIX_SOCKET", None),
    ("CLAUDE_CODE_SKIP_BEDROCK_AUTH", None),
    ("CLAUDE_CODE_SKIP_VERTEX_AUTH", None),
    ("CLAUDE_CODE_SKIP_FOUNDRY_AUTH", None),
    ("CLAUDE_CODE_SKIP_MANTLE_AUTH", None),
    ("CLAUDE_CODE_SKIP_ANTHROPIC_AWS_AUTH", None),
    ("CLAUDE_CODE_SKIP_ANTHROPIC_GOOGLE_CLOUD_AUTH", None),
];

/// Cloud credentials a CLI would use in place of a Bedrock or Vertex provider's own: AWS's, which
/// a Bedrock client tries beside an API key, and a Google API key, which replaces a service account.
const CLOUD_CREDENTIALS: &[&str] = &[
    "AWS_PROFILE",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_WEB_IDENTITY_TOKEN_FILE",
    "AWS_ROLE_ARN",
    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
    "GOOGLE_CLOUD_API_KEY",
];

/// What a CLI would use for every provider in place of the provider's own sign-in and upstream.
fn shared_auth(cli: Cli, provider_type: &str) -> Vec<&'static str> {
    let mut names = match (cli, provider_type) {
        (Cli::Claude, _) => CLAUDE_CREDENTIAL_VARS
            .iter()
            .copied()
            .chain(
                CLAUDE_ROUTES
                    .iter()
                    .filter(|(_, set_by)| *set_by != Some(provider_type))
                    .map(|(name, _)| *name),
            )
            .collect(),
        (Cli::Codex, _) => vec!["OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_ACCESS_TOKEN"],
        (Cli::Pi, CLAUDE_SUBSCRIPTION) => vec![
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_OAUTH_TOKEN",
            "ANTHROPIC_AUTH_TOKEN",
        ],
        (Cli::Pi, _) => vec![],
    };
    if matches!(provider_type, BEDROCK | VERTEX) {
        names.extend(CLOUD_CREDENTIALS);
    }
    names
}

/// What a launch gives its CLI beside Claude Code's routes, which a sign-in run from inside that
/// CLI must not inherit either.
const LAUNCH_ENV: &[&str] = &[
    "AGS_LAUNCH_SECRET",
    "ANTHROPIC_CUSTOM_HEADERS",
    "OPENAI_BASE_URL",
    "AGS_PI_PROVIDER",
    "AGS_PI_BASE_URL",
    "AWS_REGION",
    "AWS_BEARER_TOKEN_BEDROCK",
    "ANTHROPIC_VERTEX_PROJECT_ID",
    "CLOUD_ML_REGION",
    "GOOGLE_CLOUD_PROJECT",
    "GOOGLE_CLOUD_LOCATION",
    "GOOGLE_APPLICATION_CREDENTIALS",
];

/// What a sign-in run from inside a launch drops before setting its own.
fn from_launch() -> impl Iterator<Item = &'static str> {
    LAUNCH_ENV
        .iter()
        .copied()
        .chain(CLAUDE_ROUTES.iter().map(|(name, _)| *name))
        .chain(DIR_VARS.iter().flat_map(|(own, kept)| [*own, *kept]))
}

/// The provider pi signs in to for a provider of `provider_type`, by its name in pi.
fn pi_provider(provider_type: &str) -> Option<&'static str> {
    match provider_type {
        CLAUDE_SUBSCRIPTION => Some("anthropic"),
        CODEX_SUBSCRIPTION => Some("openai-codex"),
        _ => None,
    }
}

/// Each CLI's variable for its config dir, and the variable a launch keeps the user's value in, so
/// that ags run inside a launch still finds the user's own dirs.
const DIR_VARS: [(&str, &str); 3] = [
    ("CLAUDE_CONFIG_DIR", "AGS_USER_CLAUDE_CONFIG_DIR"),
    ("CODEX_HOME", "AGS_USER_CODEX_HOME"),
    ("PI_CODING_AGENT_DIR", "AGS_USER_PI_CODING_AGENT_DIR"),
];

/// The user's own config of each CLI, outside ags.
#[derive(Clone, Debug, Default)]
pub struct Home {
    claude: Option<PathBuf>,
    claude_state: Option<PathBuf>,
    codex: Option<PathBuf>,
    pi: Option<PathBuf>,
    /// The user's value of each of `DIR_VARS`, which a launch passes on.
    vars: [Option<PathBuf>; 3],
    /// The user's home dir, whose defaults stand in for unset variables.
    user: Option<PathBuf>,
    /// Each of `DIR_VARS` as this process has it.
    own: [Option<PathBuf>; 3],
    /// What the launch ags runs in kept of each, `Some(None)` when the user had not set it.
    kept: [Option<Option<PathBuf>>; 3],
}

impl Home {
    /// Where each CLI keeps its config for this user, following the CLI's own variables.
    pub fn from_env() -> Self {
        Self::from_vars(|name| std::env::var_os(name))
    }

    fn from_vars(var: impl Fn(&str) -> Option<OsString>) -> Self {
        let path = |value: OsString| Some(PathBuf::from(value)).filter(|path| path.is_absolute());
        Self {
            user: var("HOME").or_else(|| var("USERPROFILE")).and_then(path),
            own: DIR_VARS.map(|(name, _)| var(name).and_then(path)),
            kept: DIR_VARS.map(|(_, kept)| var(kept).map(path)),
            ..Self::default()
        }
        .resolve(None)
    }

    /// The default locations under `home`.
    pub fn at(home: &Path) -> Self {
        Self {
            user: Some(home.to_owned()),
            ..Self::default()
        }
        .resolve(None)
    }

    /// Settles each CLI's dir. A variable that points inside `ags`, as it does in a CLI that ags
    /// launched, gives way to the user's value the launch kept, or else to the default.
    fn resolve(mut self, ags: Option<&Path>) -> Self {
        let ags = ags.map(canonical);
        let inside = |path: &Path| {
            ags.as_ref()
                .is_some_and(|ags| canonical(path).starts_with(ags))
        };
        let defaults = Self {
            claude: self.user.as_ref().map(|home| home.join(".claude")),
            claude_state: self.user.as_ref().map(|home| home.join(CLAUDE_STATE)),
            codex: self.user.as_ref().map(|home| home.join(".codex")),
            pi: self.user.as_ref().map(|home| home.join(".pi/agent")),
            ..Self::default()
        };
        let mut vars = self.own.clone();
        for (var, kept) in vars.iter_mut().zip(&self.kept) {
            if var.as_deref().is_some_and(inside) {
                *var = kept.clone().flatten().filter(|kept| !inside(kept));
            }
        }
        let [claude, codex, pi] = vars.clone();
        let outside = |path: Option<PathBuf>| path.filter(|path| !inside(path));
        self.claude_state = outside(
            claude
                .as_ref()
                .map(|dir| dir.join(CLAUDE_STATE))
                .or(defaults.claude_state),
        );
        self.claude = outside(claude.or(defaults.claude));
        self.codex = outside(codex.or(defaults.codex));
        self.pi = outside(pi.or(defaults.pi));
        self.vars = vars;
        self
    }

    fn dir(&self, cli: Cli) -> Option<&Path> {
        match cli {
            Cli::Claude => self.claude.as_deref(),
            Cli::Codex => self.codex.as_deref(),
            Cli::Pi => self.pi.as_deref(),
        }
    }

    /// What a launch passes on so that ags run inside it finds these same dirs.
    fn kept(&self) -> impl Iterator<Item = (&'static str, OsString)> + '_ {
        DIR_VARS
            .iter()
            .zip(&self.vars)
            .map(|((_, kept), value)| (*kept, value.clone().unwrap_or_default().into_os_string()))
    }
}

/// `path` with links resolved, as far as it exists.
fn canonical(path: &Path) -> PathBuf {
    if let Ok(path) = fs::canonicalize(path) {
        return path;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => canonical(parent).join(name),
        _ => path.to_owned(),
    }
}

/// What a launch adds so that a CLI uses one provider's own config dir.
#[derive(Debug, Default)]
pub(crate) struct Isolation {
    pub args: Vec<OsString>,
    pub env: Vec<(&'static str, OsString)>,
    pub unset: Vec<&'static str>,
}

/// The credentials of every provider on this machine.
pub struct Credentials {
    dir: PathBuf,
    home: Home,
    /// Its lock guards `state.json` and each provider's copy of Claude Code's state file.
    store: Store,
}

/// What ags remembers about setting up providers.
#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// Warnings already shown.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    warned: BTreeSet<String>,
    /// Credentials the user chose not to set up, so `ags setup` does not ask for them again.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    skipped: BTreeSet<String>,
}

/// Something about a provider's settings the user should hear about once.
struct Notice {
    key: String,
    text: String,
}

impl Credentials {
    pub fn new(store: &Store, home: Home) -> Self {
        let dir = store.dir().join(DIR_NAME);
        Self {
            home: home.resolve(Some(&dir)),
            dir,
            store: Store::at(store.dir()),
        }
    }

    fn state(&self) -> Result<State> {
        Ok(match read_json(&self.dir.join(STATE))? {
            Some(state) => serde_json::from_value(Value::Object(state))
                .with_context(|| format!("{} is not valid", self.dir.join(STATE).display()))?,
            None => State::default(),
        })
    }

    /// Changes the state under the store lock. `change` must not take the lock itself.
    fn update<T>(&self, change: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let _lock = self.store.lock()?;
        let mut state = self.state()?;
        let result = change(&mut state)?;
        write_private(&self.dir, STATE, &serde_json::to_vec_pretty(&state)?)?;
        Ok(result)
    }

    fn provider_dir(&self, provider: &str) -> Result<PathBuf> {
        let valid = (1..=64).contains(&provider.len())
            && provider
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
        if !valid {
            bail!("ags can't keep credentials for a provider named {provider:?}");
        }
        Ok(self.dir.join(dir_name(provider)))
    }

    /// The config dir `cli` uses for `provider`.
    pub fn cli_dir(&self, provider: &str, cli: Cli) -> Result<PathBuf> {
        Ok(self.provider_dir(provider)?.join(cli.to_string()))
    }

    /// Whether `provider` has `need`, without asking the vendor.
    pub fn has(&self, provider: &str, provider_type: &str, need: Need) -> Result<bool> {
        let dir = self.provider_dir(provider)?;
        Ok(match need {
            Need::SignIn(Cli::Claude) => {
                let dir = dir.join(Cli::Claude.to_string());
                // On macOS the sign-in itself is in the keychain.
                if cfg!(target_os = "macos") {
                    read_json(&dir.join(CLAUDE_STATE))?
                        .is_some_and(|state| state.get("oauthAccount").is_some())
                } else {
                    has_claude_sign_in(&dir.join(CLAUDE_CREDENTIALS))?
                }
            }
            Need::SignIn(Cli::Codex) => is_file(&dir.join(Cli::Codex.to_string()).join(CODEX_AUTH)),
            Need::SignIn(Cli::Pi) => {
                let Some(name) = pi_provider(provider_type) else {
                    return Ok(false);
                };
                read_json(&dir.join(Cli::Pi.to_string()).join(PI_AUTH))?
                    .is_some_and(|auth| auth.get(name).is_some())
            }
            Need::BedrockKey => is_file(&dir.join(BEDROCK_KEY)),
            Need::VertexAccount => is_file(&dir.join(VERTEX_ACCOUNT)),
        })
    }

    /// Creates `provider`'s dir for `cli` and brings the user's settings into it.
    pub(crate) fn prepare(&self, provider: &str, cli: Cli) -> Result<PathBuf> {
        self.prepared(provider, cli).map(|(dir, _)| dir)
    }

    /// `prepare`, also naming the user's settings files that the provider holds a different copy of.
    fn prepared(&self, provider: &str, cli: Cli) -> Result<(PathBuf, Links)> {
        let dir = self.cli_dir(provider, cli)?;
        create_private_dir(&dir)?;
        let links = match self.home.dir(cli) {
            Some(own) => link(own, &dir, shared(cli))?,
            None => Links::default(),
        };
        // A file shared again is warned about again if it is split off later.
        let stale: Vec<String> = links
            .linked
            .iter()
            .flat_map(|name| [differs_key(&dir, name), blocked_key(&dir, name)])
            .collect();
        if stale
            .iter()
            .any(|key| self.state().is_ok_and(|state| state.warned.contains(key)))
        {
            self.update(|state| {
                for key in &stale {
                    state.warned.remove(key);
                }
                Ok(())
            })?;
        }
        if cli == Cli::Claude
            && let Some(own) = &self.home.claude_state
        {
            let _lock = self.store.lock()?;
            share_claude_state(own, &dir.join(CLAUDE_STATE))?;
        }
        Ok((dir, links))
    }

    /// Prepares `provider`'s dir for `cli` and returns what the user has not been told yet about it.
    pub fn notices(&self, provider: &str, cli: Cli) -> Result<Vec<String>> {
        let (dir, links) = self.prepared(provider, cli)?;
        let product = product(cli);
        let differing = links.differing.iter().map(|name| Notice {
            key: differs_key(&dir, name),
            text: format!(
                "{provider} keeps its own {name} for {product}, which differs from yours; delete {} to share yours again",
                dir.join(name).display()
            ),
        });
        let blocked = links.blocked.iter().map(|name| Notice {
            key: blocked_key(&dir, name),
            text: format!(
                "{provider} has its own {name} for {product}, so yours is not shared; delete {} to share yours",
                dir.join(name).display()
            ),
        });
        let mut notices: Vec<Notice> = differing.chain(blocked).collect();
        if cli == Cli::Claude
            && let Some(own) = &self.home.claude
            && let Some(settings) = read_json(&own.join("settings.json")).ok().flatten()
        {
            if settings.contains_key("apiKeyHelper") {
                notices.push(Notice {
                    key: "claude/apiKeyHelper".to_owned(),
                    text: "your Claude Code settings set apiKeyHelper, so every Claude provider uses that key in place of its own sign-in".to_owned(),
                });
            }
            let env = settings.get("env").and_then(Value::as_object);
            let set = |name: &str| env.is_some_and(|env| env.contains_key(name));
            let mut warn = |name: &str, text: String| {
                notices.push(Notice {
                    key: format!("claude/env/{name}"),
                    text: format!("your Claude Code settings set {name}, {text}"),
                });
            };
            for name in CLAUDE_CREDENTIAL_VARS.iter().filter(|name| set(name)) {
                warn(
                    name,
                    "so every Claude provider uses it in place of its own sign-in".to_owned(),
                );
            }
            // Claude Code applies its settings' environment over the one a launch gives it.
            for (name, _) in CLAUDE_ROUTES.iter().filter(|(name, _)| set(name)) {
                warn(
                    name,
                    "which overrides where each ags launch sends Claude Code".to_owned(),
                );
            }
            for name in CLOUD_CREDENTIALS.iter().filter(|name| set(name)) {
                warn(
                    name,
                    "which a Bedrock or Vertex provider may use in place of its own credential"
                        .to_owned(),
                );
            }
        }
        if notices.iter().all(|notice| {
            self.state()
                .is_ok_and(|state| state.warned.contains(&notice.key))
        }) {
            return Ok(vec![]);
        }
        self.update(|state| {
            Ok(notices
                .into_iter()
                .filter(|notice| state.warned.insert(notice.key.clone()))
                .map(|notice| notice.text)
                .collect())
        })
    }

    /// Points `cli` at `provider`'s config dir, after preparing it.
    pub(crate) fn isolate(
        &self,
        provider: &str,
        provider_type: &str,
        cli: Cli,
    ) -> Result<Isolation> {
        let dir = self.prepare(provider, cli)?.into_os_string();
        let unset = shared_auth(cli, provider_type);
        let mut isolation = match cli {
            Cli::Claude => Isolation {
                args: vec![],
                env: vec![("CLAUDE_CONFIG_DIR", dir)],
                unset: unset.clone(),
            },
            Cli::Codex => Isolation {
                args: vec!["-c".into(), CODEX_FILE_STORE.into()],
                env: vec![("CODEX_HOME", dir)],
                unset: unset.clone(),
            },
            Cli::Pi => Isolation {
                args: vec![],
                env: vec![("PI_CODING_AGENT_DIR", dir)],
                unset,
            },
        };
        isolation.env.extend(self.home.kept());
        Ok(isolation)
    }

    /// Runs `cli`'s own sign-in in `provider`'s config dir. `program` replaces the CLI's name on `PATH`.
    pub async fn sign_in(
        &self,
        provider: &str,
        provider_type: &str,
        cli: Cli,
        program: Option<&OsStr>,
    ) -> Result<ExitStatus> {
        let isolation = self.isolate(provider, provider_type, cli)?;
        let name = OsString::from(cli.to_string());
        let program = program.unwrap_or(&name);
        let mut command = Command::new(program);
        match cli {
            Cli::Claude => command.args(["auth", "login"]),
            Cli::Codex => command.arg("login").args(isolation.args),
            Cli::Pi => {
                let Some(name) = pi_provider(provider_type) else {
                    bail!("pi can't sign in to a {provider_type} provider");
                };
                command.args(["auth", "login", name])
            }
        };
        for name in isolation.unset.iter().copied().chain(from_launch()) {
            command.env_remove(name);
        }
        command.envs(isolation.env).kill_on_drop(true);
        // Ctrl-C is the sign-in's to handle; ags carries on to its next prompt.
        #[cfg(unix)]
        // SAFETY: the closure only calls signal, which is async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                libc::signal(libc::SIGQUIT, libc::SIG_DFL);
                Ok(())
            });
        }
        let _ignored = Interrupts::ignore();
        let mut child = crate::launch::spawn(command, program)?;
        child.wait().await.context("waiting for the sign-in")
    }

    /// Remembers that the user chose not to set up `need` for `provider`.
    pub fn skip(&self, provider: &str, need: Need) -> Result<()> {
        self.update(|state| {
            state.skipped.insert(need.key(provider));
            Ok(())
        })
    }

    /// Forgets that `need` was skipped for `provider`, once it is set up.
    pub fn unskip(&self, provider: &str, need: Need) -> Result<()> {
        let key = need.key(provider);
        if !self.state()?.skipped.contains(&key) {
            return Ok(());
        }
        self.update(|state| {
            state.skipped.remove(&key);
            Ok(())
        })
    }

    /// Keeps `key` as `provider`'s Bedrock API key.
    pub fn save_bedrock_key(&self, provider: &str, key: &str) -> Result<()> {
        let key = key.trim();
        if key.is_empty()
            || key
                .chars()
                .any(|char| char.is_whitespace() || char.is_control())
        {
            bail!("a Bedrock API key is one line without spaces");
        }
        let dir = self.provider_dir(provider)?;
        write_private(&dir, BEDROCK_KEY, key.as_bytes())?;
        Ok(())
    }

    /// Keeps a copy of the service account key in `file` as `provider`'s Vertex credential.
    pub fn save_vertex_account(&self, provider: &str, file: &Path) -> Result<()> {
        let expanded = match (file.strip_prefix("~"), &self.home.user) {
            (Ok(rest), Some(user)) => user.join(rest),
            _ => file.to_owned(),
        };
        let file = expanded.as_path();
        let bytes = read(file)?;
        let is_account = serde_json::from_slice::<Map<String, Value>>(&bytes).is_ok_and(|key| {
            key.get("type").and_then(Value::as_str) == Some("service_account")
                && ["client_email", "private_key"]
                    .iter()
                    .all(|field| key.get(*field).is_some_and(Value::is_string))
        });
        if !is_account {
            bail!(
                "{} is not a Google Cloud service account key",
                file.display()
            );
        }
        let dir = self.provider_dir(provider)?;
        write_private(&dir, VERTEX_ACCOUNT, &bytes)?;
        Ok(())
    }

    /// The Google Cloud project of `provider`'s stored service account, where its CLIs call Vertex.
    pub fn vertex_project(&self, provider: &str) -> Result<String> {
        let path = self.provider_dir(provider)?.join(VERTEX_ACCOUNT);
        let account = read_json(&path)?.ok_or_else(|| {
            anyhow!(
                "{provider} has no {}; run `ags setup {provider}`",
                Need::VertexAccount
            )
        })?;
        account
            .get("project_id")
            .and_then(Value::as_str)
            // A project ID starts with a letter, so it is never `.` or `..` in a request path.
            .filter(|project| {
                project.starts_with(|first: char| first.is_ascii_lowercase())
                    && project.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-.:".contains(&byte)
                    })
            })
            .map(str::to_owned)
            .ok_or_else(|| {
                anyhow!("the service account of {provider} names no Google Cloud project")
            })
    }

    /// How a CLI finds a cloud provider's stored credential: the variables its SDK reads.
    pub fn cloud_env(
        &self,
        provider: &str,
        provider_type: &str,
    ) -> Result<Vec<(&'static str, OsString)>> {
        let dir = self.provider_dir(provider)?;
        Ok(match provider_type {
            BEDROCK => {
                let key = String::from_utf8(read(&dir.join(BEDROCK_KEY))?)
                    .context("the stored Bedrock API key is not text")?;
                vec![("AWS_BEARER_TOKEN_BEDROCK", key.trim().into())]
            }
            VERTEX => vec![(
                "GOOGLE_APPLICATION_CREDENTIALS",
                dir.join(VERTEX_ACCOUNT).into_os_string(),
            )],
            _ => vec![],
        })
    }
}

/// One credential of a granted provider, and whether it is in place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub provider: String,
    pub provider_type: String,
    pub need: Need,
    pub status: StepStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepStatus {
    Done,
    Missing,
    /// Missing, and the user chose not to set it up.
    Skipped,
}

/// The credentials of every granted provider, or only of `provider`.
pub async fn setup_steps(
    store: &Store,
    credentials: &Credentials,
    provider: Option<&str>,
) -> Result<Vec<Step>> {
    let (login, machine) = granted(store)
        .await?
        .ok_or_else(crate::launch::not_logged_in)?;
    if let Some(id) = provider {
        if !machine.providers.iter().any(|granted| granted == id) {
            bail!(
                "{id} is not granted to this machine; run `ags logout`, then `ags login {}` to grant it",
                login.server
            );
        }
        if !machine.provider_types.contains_key(id) {
            bail!("{} no longer has a provider named {id}", login.server);
        }
    }
    let skipped = credentials.state()?.skipped;
    let mut steps = vec![];
    for id in &machine.providers {
        if provider.is_some_and(|only| only != id) {
            continue;
        }
        let Some(provider_type) = machine.provider_types.get(id) else {
            continue;
        };
        for need in Need::all(provider_type) {
            steps.push(Step {
                provider: id.clone(),
                provider_type: provider_type.clone(),
                need,
                status: if credentials.has(id, provider_type, need)? {
                    StepStatus::Done
                } else if skipped.contains(&need.key(id)) {
                    StepStatus::Skipped
                } else {
                    StepStatus::Missing
                },
            });
        }
    }
    Ok(steps)
}

/// Whether `cli` is an executable on `PATH`.
pub fn installed(cli: Cli) -> bool {
    on_path(&cli.to_string(), std::env::var_os("PATH").as_deref())
}

/// Whether `program` is an executable in one of the dirs of `path`, a value of `PATH`.
pub(crate) fn on_path(program: &str, path: Option<&OsStr>) -> bool {
    let Some(path) = path else {
        return false;
    };
    let names: &[String] = &if cfg!(windows) {
        ["exe", "cmd", "bat"]
            .map(|extension| format!("{program}.{extension}"))
            .to_vec()
    } else {
        vec![program.to_owned()]
    };
    std::env::split_paths(path).any(|dir| {
        names.iter().any(|name| {
            let candidate = dir.join(name);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::metadata(&candidate)
                    .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            }
            #[cfg(not(unix))]
            candidate.is_file()
        })
    })
}

fn shared(cli: Cli) -> &'static [&'static str] {
    match cli {
        Cli::Claude => CLAUDE_SHARED,
        Cli::Codex => CODEX_SHARED,
        Cli::Pi => PI_SHARED,
    }
}

/// What linking the user's settings into a provider's dir did and could not do.
#[derive(Default)]
struct Links {
    /// Linked now, where nothing or an unchanged copy was.
    linked: Vec<String>,
    /// Files the provider holds a different copy of.
    differing: Vec<String>,
    /// Entries the provider holds that are not a file the user's could replace, such as a dir.
    blocked: Vec<String>,
}

fn differs_key(dir: &Path, name: &str) -> String {
    format!("differs/{}/{name}", dir.display())
}

fn blocked_key(dir: &Path, name: &str) -> String {
    format!("blocked/{}/{name}", dir.display())
}

/// Links each of `names` that exists in `own` into `dir`. A CLI that saves a file by renaming over
/// it replaces the link with a copy; an unchanged copy is linked again.
fn link(own: &Path, dir: &Path, names: &[&str]) -> Result<Links> {
    let mut links = Links::default();
    for name in names {
        let (from, to) = (own.join(name), dir.join(name));
        if from.symlink_metadata().is_err() {
            continue;
        }
        match to.symlink_metadata() {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(meta) if meta.is_symlink() => continue,
            Ok(meta) if meta.is_file() && from.is_file() => {
                if read(&to)? != read(&from)? {
                    links.differing.push((*name).to_owned());
                    continue;
                }
            }
            Ok(_) => {
                links.blocked.push((*name).to_owned());
                continue;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", to.display()));
            }
        }
        #[cfg(unix)]
        {
            let staging = dir.join(format!(".{name}.ags-link"));
            let _ = fs::remove_file(&staging);
            std::os::unix::fs::symlink(&from, &staging)
                .and_then(|()| fs::rename(&staging, &to))
                .with_context(|| format!("linking {} into {}", from.display(), dir.display()))?;
            links.linked.push((*name).to_owned());
        }
        // Symbolic links need extra rights on Windows, so settings stay apart there.
        #[cfg(not(unix))]
        let _ = to;
    }
    Ok(links)
}

/// Brings the settings Claude Code keeps in the user's own `.claude.json` into `state`.
fn share_claude_state(own: &Path, state: &Path) -> Result<()> {
    let Some(own) = read_json(own)? else {
        return Ok(());
    };
    let mut current = read_json(state)?.unwrap_or_default();
    let mut next = current.clone();
    mirror(&own, &mut next, CLAUDE_STATE_SHARED);
    if let Some(Value::Object(projects)) = own.get("projects") {
        let mine = next
            .entry("projects")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(mine) = mine {
            for (path, project) in projects {
                let Value::Object(project) = project else {
                    continue;
                };
                let entry = mine
                    .entry(path)
                    .or_insert_with(|| Value::Object(Map::new()));
                if let Value::Object(entry) = entry {
                    mirror(project, entry, CLAUDE_PROJECT_SHARED);
                }
            }
        }
    }
    if next == current {
        return Ok(());
    }
    current = next;
    write_json(state, &current)
}

/// Sets each of `keys` that `from` has to its value there; keys `from` lacks are left alone.
fn mirror(from: &Map<String, Value>, to: &mut Map<String, Value>, keys: &[&str]) {
    for key in keys {
        if let Some(value) = from.get(*key) {
            to.insert((*key).to_owned(), value.clone());
        }
    }
}

fn write_json(path: &Path, value: &Map<String, Value>) -> Result<()> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(OsStr::to_str)) else {
        bail!("{} is not a file path", path.display());
    };
    write_private(dir, name, &serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

/// A dir name for a provider ID that no other ID shares on a filesystem that ignores case: the
/// ID in lowercase, followed by a hash of the ID itself when it has capitals.
fn dir_name(provider: &str) -> String {
    let lower = provider.to_ascii_lowercase();
    if lower == provider {
        return lower;
    }
    // FNV-1a, which stays the same across Rust versions, unlike the standard hasher.
    let hash = provider.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    format!("{lower}.{hash:08x}")
}

/// Ignores Ctrl-C and Ctrl-\ in ags until dropped, then handles them as before.
struct Interrupts {
    #[cfg(unix)]
    saved: [(libc::c_int, libc::sigaction); 2],
}

impl Interrupts {
    fn ignore() -> Self {
        #[cfg(unix)]
        {
            let saved = [libc::SIGINT, libc::SIGQUIT].map(|signal| {
                // SAFETY: sigaction is plain data; the call fills in `saved`.
                let mut saved: libc::sigaction = unsafe { std::mem::zeroed() };
                let mut ignore: libc::sigaction = unsafe { std::mem::zeroed() };
                ignore.sa_sigaction = libc::SIG_IGN;
                // SAFETY: both pointers are to live sigaction values.
                unsafe { libc::sigaction(signal, &raw const ignore, &raw mut saved) };
                (signal, saved)
            });
            Self { saved }
        }
        #[cfg(not(unix))]
        Self {}
    }
}

impl Drop for Interrupts {
    fn drop(&mut self) {
        #[cfg(unix)]
        for (signal, saved) in &self.saved {
            // SAFETY: restores the action sigaction reported before.
            unsafe { libc::sigaction(*signal, saved, std::ptr::null_mut()) };
        }
    }
}

/// Claude Code keeps MCP sign-ins in the same file, so only `claudeAiOauth` is its own.
fn has_claude_sign_in(path: &Path) -> Result<bool> {
    Ok(read_json(path)?.is_some_and(|credentials| credentials.contains_key("claudeAiOauth")))
}

fn is_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.len() > 0)
}

fn read(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).with_context(|| format!("reading {}", path.display()))
}

/// A JSON object from `path`; `None` when there is no file.
fn read_json(path: &Path) -> Result<Option<Map<String, Value>>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .with_context(|| format!("{} is not a JSON object", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{clis::Upstream, store::tests::scratch};

    struct Fixture {
        root: PathBuf,
        home: PathBuf,
        credentials: Credentials,
    }

    impl Fixture {
        fn new() -> Self {
            let root = scratch();
            let home = root.join("home");
            fs::create_dir_all(&home).unwrap();
            let credentials = Credentials::new(&Store::at(root.join("ags")), Home::at(&home));
            Self {
                root,
                home,
                credentials,
            }
        }

        fn write(&self, path: &str, text: &str) {
            let path = self.home.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        fn provider(&self, id: &str) -> PathBuf {
            self.root.join("ags").join(DIR_NAME).join(id)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_provider_needs_a_sign_in_per_cli_or_its_key() {
        assert_eq!(
            Need::all(CLAUDE_SUBSCRIPTION),
            [Need::SignIn(Cli::Claude), Need::SignIn(Cli::Pi)]
        );
        assert_eq!(
            Need::all(CODEX_SUBSCRIPTION),
            [Need::SignIn(Cli::Codex), Need::SignIn(Cli::Pi)]
        );
        assert_eq!(Need::all(BEDROCK), [Need::BedrockKey]);
        assert_eq!(Need::all(VERTEX), [Need::VertexAccount]);
        assert!(Need::all("typesafe").is_empty());
        assert_eq!(
            Need::of_launch(Cli::Pi, CODEX_SUBSCRIPTION),
            Some(Need::SignIn(Cli::Pi))
        );
        assert_eq!(Need::of_launch(Cli::Claude, "typesafe"), None);
        assert_eq!(Need::SignIn(Cli::Claude).to_string(), "Claude Code sign-in");
        assert_eq!(Need::BedrockKey.to_string(), "Bedrock API key");
    }

    #[test]
    fn each_provider_has_its_own_dir_per_cli() {
        let fixture = Fixture::new();
        let credentials = &fixture.credentials;
        let claude = credentials
            .isolate("work", CLAUDE_SUBSCRIPTION, Cli::Claude)
            .unwrap();
        assert!(claude.args.is_empty());
        assert_eq!(
            claude.env[..1],
            [(
                "CLAUDE_CONFIG_DIR",
                fixture.provider("work").join("claude").into_os_string()
            )]
        );
        let other = credentials
            .isolate("personal", CLAUDE_SUBSCRIPTION, Cli::Claude)
            .unwrap();
        assert_ne!(claude.env, other.env, "two providers never share a sign-in");
        let codex = credentials
            .isolate("work", CODEX_SUBSCRIPTION, Cli::Codex)
            .unwrap();
        assert_eq!(codex.args, [OsString::from("-c"), CODEX_FILE_STORE.into()]);
        assert!(claude.unset.contains(&"ANTHROPIC_API_KEY"));
        assert!(claude.unset.contains(&"CLAUDE_CODE_OAUTH_TOKEN"));
        assert!(codex.unset.contains(&"OPENAI_API_KEY"));
        assert!(codex.unset.contains(&"CODEX_ACCESS_TOKEN"));
        for (route, _) in CLAUDE_ROUTES {
            let set_by_subscription = *route == "ANTHROPIC_BASE_URL";
            assert_eq!(
                claude.unset.contains(route),
                !set_by_subscription,
                "{route}"
            );
        }
        assert!(
            claude
                .unset
                .contains(&"CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR")
        );
        let on_bedrock = shared_auth(Cli::Claude, BEDROCK);
        assert!(!on_bedrock.contains(&"CLAUDE_CODE_USE_BEDROCK"));
        assert!(on_bedrock.contains(&"CLAUDE_CODE_USE_VERTEX"));
        assert_eq!(codex.env[0].0, "CODEX_HOME");
        let pi = credentials
            .isolate("work", CODEX_SUBSCRIPTION, Cli::Pi)
            .unwrap();
        assert_eq!(
            pi.env[..1],
            [(
                "PI_CODING_AGENT_DIR",
                fixture.provider("work").join("pi").into_os_string()
            )]
        );
        #[cfg(unix)]
        {
            assert_eq!(mode(&fixture.root.join("ags").join(DIR_NAME)), 0o700);
            assert_eq!(mode(&fixture.provider("work")), 0o700);
            assert_eq!(mode(&fixture.provider("work").join("codex")), 0o700);
        }
        let work = credentials.cli_dir("Work", Cli::Claude).unwrap();
        let name = work
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        assert!(name.starts_with("work."), "{name}");
        assert_ne!(
            name.to_lowercase(),
            "work",
            "Work and work stay apart where case is ignored"
        );
        assert_ne!(work, credentials.cli_dir("WORK", Cli::Claude).unwrap());
        for refused in ["", "../work", "a/b", ".", &"x".repeat(65)] {
            assert!(
                credentials
                    .isolate(refused, CLAUDE_SUBSCRIPTION, Cli::Claude)
                    .is_err(),
                "{refused:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn settings_are_linked_and_credentials_stay_apart() {
        let fixture = Fixture::new();
        fixture.write(".claude/settings.json", "{}");
        fixture.write(".claude/agents/reviewer.md", "agent");
        fixture.write(".claude/.credentials.json", "{\"own\":1}");
        fixture.write(".claude/history.jsonl", "");
        fixture.write(".codex/config.toml", "model = \"x\"");
        fixture.write(".codex/auth.json", "{}");
        fixture.write(".pi/agent/settings.json", "{}");
        fixture.write(".pi/agent/auth.json", "{}");
        let credentials = &fixture.credentials;
        let claude = credentials.prepare("work", Cli::Claude).unwrap();
        assert_eq!(
            fs::read_link(claude.join("settings.json")).unwrap(),
            fixture.home.join(".claude/settings.json")
        );
        assert_eq!(
            fs::read_to_string(claude.join("agents/reviewer.md")).unwrap(),
            "agent"
        );
        for isolated in [".credentials.json", "history.jsonl", "commands"] {
            assert!(
                claude.join(isolated).symlink_metadata().is_err(),
                "{isolated}"
            );
        }
        let codex = credentials.prepare("work", Cli::Codex).unwrap();
        assert!(codex.join("config.toml").is_symlink());
        assert!(!codex.join("auth.json").exists());
        let pi = credentials.prepare("work", Cli::Pi).unwrap();
        assert!(pi.join("settings.json").is_symlink());
        assert!(!pi.join("auth.json").exists());

        // A provider's own file is kept, and settings added later are linked on the next launch.
        fs::remove_file(claude.join("settings.json")).unwrap();
        fs::write(claude.join("settings.json"), "{\"mine\":1}").unwrap();
        fixture.write(".claude/commands/review.md", "command");
        credentials.prepare("work", Cli::Claude).unwrap();
        assert_eq!(
            fs::read_to_string(claude.join("settings.json")).unwrap(),
            "{\"mine\":1}"
        );
        assert!(claude.join("commands").is_symlink());
    }

    #[test]
    fn claude_code_settings_in_its_state_file_follow_the_users_own() {
        let fixture = Fixture::new();
        fixture.write(
            ".claude.json",
            r#"{"mcpServers":{"docs":{"command":"docs"}},"hasCompletedOnboarding":true,
            "oauthAccount":{"emailAddress":"me@example.com"},"userID":"u",
            "projects":{"/src":{"mcpServers":{"db":{}},"hasTrustDialogAccepted":true,"lastCost":1}}}"#,
        );
        let credentials = &fixture.credentials;
        let dir = credentials.prepare("work", Cli::Claude).unwrap();
        let state = read_json(&dir.join(CLAUDE_STATE)).unwrap().unwrap();
        assert_eq!(state["mcpServers"]["docs"]["command"], "docs");
        assert_eq!(state["hasCompletedOnboarding"], true);
        assert!(
            state.get("oauthAccount").is_none(),
            "the sign-in stays apart"
        );
        assert!(state.get("userID").is_none());
        let project = &state["projects"]["/src"];
        assert_eq!(project["hasTrustDialogAccepted"], true);
        assert!(project.get("lastCost").is_none());
        #[cfg(unix)]
        assert_eq!(mode(&dir.join(CLAUDE_STATE)), 0o600);

        // Only keys the user's file has are set; Claude Code's own are never removed.
        let mut state = state;
        state.insert("numStartups".into(), 3.into());
        write_json(&dir.join(CLAUDE_STATE), &state).unwrap();
        fixture.write(
            ".claude.json",
            r#"{"mcpServers":{},"projects":{"/src":{}}}"#,
        );
        credentials.prepare("work", Cli::Claude).unwrap();
        let state = read_json(&dir.join(CLAUDE_STATE)).unwrap().unwrap();
        assert_eq!(state["numStartups"], 3);
        assert_eq!(state["mcpServers"], Value::Object(Map::new()));
        assert_eq!(state["hasCompletedOnboarding"], true);
        assert_eq!(state["projects"]["/src"]["hasTrustDialogAccepted"], true);
    }

    #[cfg(unix)]
    #[test]
    fn a_shared_file_a_cli_replaced_is_linked_again_or_named_once() {
        let fixture = Fixture::new();
        fixture.write(".codex/config.toml", "model = \"a\"");
        fixture.write(".codex/AGENTS.md", "rules");
        let credentials = &fixture.credentials;
        let dir = credentials.prepare("work", Cli::Codex).unwrap();
        assert!(credentials.notices("work", Cli::Codex).unwrap().is_empty());

        // Codex saved config.toml by renaming a new file over the link, unchanged.
        fs::remove_file(dir.join("config.toml")).unwrap();
        fs::write(dir.join("config.toml"), "model = \"a\"").unwrap();
        assert!(credentials.notices("work", Cli::Codex).unwrap().is_empty());
        assert!(dir.join("config.toml").is_symlink());

        // A copy that differs is kept, and named only once.
        fs::remove_file(dir.join("config.toml")).unwrap();
        fs::write(dir.join("config.toml"), "model = \"b\"").unwrap();
        let notices = credentials.notices("work", Cli::Codex).unwrap();
        assert_eq!(notices.len(), 1);
        assert!(
            notices[0].starts_with("work keeps its own config.toml for Codex"),
            "{}",
            notices[0]
        );
        assert!(credentials.notices("work", Cli::Codex).unwrap().is_empty());
        assert_eq!(
            fs::read_to_string(dir.join("config.toml")).unwrap(),
            "model = \"b\""
        );
        assert!(dir.join("AGENTS.md").is_symlink());

        // Once shared again, a later split is named again.
        fs::write(dir.join("config.toml"), "model = \"a\"").unwrap();
        assert!(credentials.notices("work", Cli::Codex).unwrap().is_empty());
        assert!(dir.join("config.toml").is_symlink());
        fs::remove_file(dir.join("config.toml")).unwrap();
        fs::write(dir.join("config.toml"), "model = \"c\"").unwrap();
        assert_eq!(credentials.notices("work", Cli::Codex).unwrap().len(), 1);

        // A dir of the provider's own where the user's is shared is named once.
        fixture.write(".codex/rules/default.rules", "");
        fs::create_dir(dir.join("rules")).unwrap();
        let notices = credentials.notices("work", Cli::Codex).unwrap();
        assert_eq!(notices.len(), 1);
        assert!(
            notices[0].starts_with("work has its own rules for Codex, so yours is not shared"),
            "{}",
            notices[0]
        );
        assert!(credentials.notices("work", Cli::Codex).unwrap().is_empty());
    }

    #[test]
    fn a_key_helper_in_the_users_settings_is_named_once() {
        let fixture = Fixture::new();
        fixture.write(
            ".claude/settings.json",
            r#"{"apiKeyHelper":"key.sh","env":{"ANTHROPIC_API_KEY":"k","EDITOR":"vi"}}"#,
        );
        let credentials = &fixture.credentials;
        let notices = credentials.notices("work", Cli::Claude).unwrap();
        assert_eq!(
            notices,
            [
                "your Claude Code settings set apiKeyHelper, so every Claude provider uses that key in place of its own sign-in",
                "your Claude Code settings set ANTHROPIC_API_KEY, so every Claude provider uses it in place of its own sign-in",
            ]
        );
        assert!(
            credentials
                .notices("other", Cli::Claude)
                .unwrap()
                .is_empty()
        );
        assert!(credentials.notices("work", Cli::Codex).unwrap().is_empty());
    }

    #[test]
    fn a_route_or_cloud_credential_in_the_users_settings_is_named_once() {
        let fixture = Fixture::new();
        fixture.write(
            ".claude/settings.json",
            r#"{"env":{"ANTHROPIC_UNIX_SOCKET":"/run/s","AWS_PROFILE":"dev","AWS_REGION":"us-east-1"}}"#,
        );
        let credentials = &fixture.credentials;
        assert_eq!(
            credentials.notices("work", Cli::Claude).unwrap(),
            [
                "your Claude Code settings set ANTHROPIC_UNIX_SOCKET, which overrides where each ags launch sends Claude Code",
                "your Claude Code settings set AWS_PROFILE, which a Bedrock or Vertex provider may use in place of its own credential",
            ]
        );
        assert!(credentials.notices("work", Cli::Claude).unwrap().is_empty());
    }

    #[test]
    fn a_claude_sign_in_is_its_own_oauth_entry() {
        let fixture = Fixture::new();
        let credentials = &fixture.credentials;
        let claude = Need::SignIn(Cli::Claude);
        let dir = credentials.prepare("work", Cli::Claude).unwrap();
        fs::write(dir.join(CLAUDE_CREDENTIALS), r#"{"mcpOAuth":{}}"#).unwrap();
        assert!(
            !credentials
                .has("work", CLAUDE_SUBSCRIPTION, claude)
                .unwrap(),
            "MCP sign-ins alone are not a Claude sign-in"
        );
        if !cfg!(target_os = "macos") {
            fs::write(dir.join(CLAUDE_CREDENTIALS), r#"{"claudeAiOauth":{}}"#).unwrap();
            assert!(
                credentials
                    .has("work", CLAUDE_SUBSCRIPTION, claude)
                    .unwrap()
            );
        }
    }

    #[test]
    fn a_cloud_provider_keeps_its_credential_in_a_private_file() {
        let fixture = Fixture::new();
        let credentials = &fixture.credentials;
        assert!(!credentials.has("aws", BEDROCK, Need::BedrockKey).unwrap());
        for refused in ["", "  ", "two words"] {
            assert!(credentials.save_bedrock_key("aws", refused).is_err());
        }
        credentials
            .save_bedrock_key("aws", " ABSKexample\n")
            .unwrap();
        assert!(credentials.has("aws", BEDROCK, Need::BedrockKey).unwrap());
        assert_eq!(
            credentials.cloud_env("aws", BEDROCK).unwrap(),
            [("AWS_BEARER_TOKEN_BEDROCK", OsString::from("ABSKexample"))]
        );

        fixture.write("key.json", r#"{"type":"authorized_user"}"#);
        let error = credentials
            .save_vertex_account("gcp", &fixture.home.join("key.json"))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .ends_with("is not a Google Cloud service account key")
        );
        assert!(!credentials.has("gcp", VERTEX, Need::VertexAccount).unwrap());
        let account = r#"{"type":"service_account","project_id":"p","client_email":"a@p.iam.gserviceaccount.com","private_key":"-----BEGIN PRIVATE KEY-----"}"#;
        fixture.write("key.json", account);
        credentials
            .save_vertex_account("gcp", Path::new("~/key.json"))
            .unwrap();
        let stored = fixture.provider("gcp").join(VERTEX_ACCOUNT);
        assert_eq!(fs::read_to_string(&stored).unwrap(), account);
        assert_eq!(credentials.vertex_project("gcp").unwrap(), "p");
        assert!(credentials.vertex_project("aws").is_err());
        for project in ["p/../x", ".", "..", "", "-p"] {
            fixture.write(
                "key.json",
                &account.replace(
                    r#""project_id":"p""#,
                    &format!(r#""project_id":"{project}""#),
                ),
            );
            credentials
                .save_vertex_account("bad", &fixture.home.join("key.json"))
                .unwrap();
            assert_eq!(
                credentials.vertex_project("bad").unwrap_err().to_string(),
                "the service account of bad names no Google Cloud project",
                "{project}"
            );
        }
        assert_eq!(
            credentials.cloud_env("gcp", VERTEX).unwrap(),
            [(
                "GOOGLE_APPLICATION_CREDENTIALS",
                stored.clone().into_os_string()
            )]
        );
        #[cfg(unix)]
        {
            assert_eq!(mode(&stored), 0o600);
            assert_eq!(mode(&fixture.provider("aws").join(BEDROCK_KEY)), 0o600);
            assert_eq!(mode(&fixture.provider("gcp")), 0o700);
        }
    }

    #[test]
    fn a_cli_that_ags_launched_is_not_the_users_own() {
        let fixture = Fixture::new();
        let custom = fixture.root.join("custom-claude");
        let launch = fixture.provider("work").join("claude");
        let home = fixture.home.clone();
        let env = |pairs: Vec<(&'static str, PathBuf)>| {
            let pairs: Vec<(&str, OsString)> = pairs
                .into_iter()
                .map(|(name, value)| (name, value.into_os_string()))
                .collect();
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(known, _)| *known == name)
                    .map(|(_, value)| value.clone())
            }
        };

        let store = Store::at(fixture.root.join("ags"));
        let resolved = |pairs| Credentials::new(&store, Home::from_vars(env(pairs))).home;

        // The user's own CLAUDE_CONFIG_DIR is passed on to a launch, which sets its own.
        let outer = resolved(vec![
            ("HOME", home.clone()),
            ("CLAUDE_CONFIG_DIR", custom.clone()),
        ]);
        assert_eq!(outer.claude_state, Some(custom.join(CLAUDE_STATE)));
        let kept: Vec<(&'static str, PathBuf)> = outer
            .kept()
            .map(|(name, value)| (name, PathBuf::from(value)))
            .collect();
        let inside_launch = |claude_config_dir: &Path| {
            let mut pairs = vec![
                ("HOME", home.clone()),
                ("CLAUDE_CONFIG_DIR", claude_config_dir.to_owned()),
            ];
            pairs.extend(kept.iter().cloned());
            resolved(pairs)
        };
        let inner = inside_launch(&launch);
        assert_eq!(inner.claude, Some(custom.clone()));
        assert_eq!(inner.claude_state, Some(custom.join(CLAUDE_STATE)));
        assert_eq!(
            inner.codex,
            Some(home.join(".codex")),
            "an unset variable stays unset"
        );

        // A dir the user chose inside the launch, outside ags, is theirs.
        let chosen = fixture.root.join("chosen");
        assert_eq!(inside_launch(&chosen).claude, Some(chosen));

        // Without them, a dir inside ags falls back to the default, also through a link.
        fs::create_dir_all(&launch).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&launch, fixture.root.join("link")).unwrap();
        #[cfg(unix)]
        let launch = fixture.root.join("link");
        let nested = resolved(vec![("HOME", home.clone()), ("CLAUDE_CONFIG_DIR", launch)]);
        assert_eq!(nested.claude, Some(home.join(".claude")));
        assert_eq!(nested.claude_state, Some(home.join(CLAUDE_STATE)));
    }

    #[test]
    fn a_sign_in_inherits_nothing_a_launch_sets() {
        let fixture = Fixture::new();
        let path = gcloud_on_path(&fixture);
        store_cloud_credentials(&fixture, "work");
        let dropped: Vec<&str> = from_launch().collect();
        // A launch only adds loopback hosts to the user's own list, which a sign-in may keep.
        let kept: Vec<&str> = crate::launch::no_proxy(|_| None)
            .iter()
            .map(|(name, _)| *name)
            .collect();
        for cli in Cli::value_variants() {
            for provider_type in cli.provider_types() {
                let setup = cli
                    .setup(
                        cloud_upstream(provider_type),
                        "http://127.0.0.1:1",
                        "secret",
                        &fixture.root,
                        |name| (name == "PATH").then(|| path.clone()),
                    )
                    .unwrap();
                let isolation = fixture
                    .credentials
                    .isolate("work", provider_type, *cli)
                    .unwrap();
                let cloud = fixture
                    .credentials
                    .cloud_env("work", provider_type)
                    .unwrap();
                let set = setup
                    .env
                    .iter()
                    .map(|(name, _)| *name)
                    .chain(isolation.env.iter().map(|(name, _)| *name))
                    .chain(cloud.iter().map(|(name, _)| *name));
                for name in set {
                    assert!(dropped.contains(&name), "{cli} sets {name}");
                }
            }
        }
        assert_eq!(kept, ["NO_PROXY", "no_proxy"]);
        assert!(kept.iter().all(|name| !dropped.contains(name)));
    }

    fn cloud_upstream(provider_type: &str) -> Upstream<'_> {
        Upstream {
            id: "work",
            provider_type,
            region: Some("us-east-1"),
            project: Some("p"),
        }
    }

    /// A `PATH` value holding an executable `gcloud`.
    fn gcloud_on_path(fixture: &Fixture) -> String {
        let bin = fixture.root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("gcloud"), "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(bin.join("gcloud"), fs::Permissions::from_mode(0o755)).unwrap();
        }
        bin.to_str().unwrap().to_owned()
    }

    fn store_cloud_credentials(fixture: &Fixture, provider: &str) {
        fixture.write(
            "key.json",
            r#"{"type":"service_account","project_id":"p","client_email":"a@p.iam.gserviceaccount.com","private_key":"k"}"#,
        );
        let credentials = &fixture.credentials;
        credentials
            .save_bedrock_key(provider, "ABSKexample")
            .unwrap();
        credentials
            .save_vertex_account(provider, &fixture.home.join("key.json"))
            .unwrap();
    }

    #[test]
    fn a_claude_launch_keeps_only_its_providers_route_and_cloud_credential() {
        let fixture = Fixture::new();
        let path = gcloud_on_path(&fixture);
        for provider_type in [CLAUDE_SUBSCRIPTION, BEDROCK, VERTEX] {
            let setup = Cli::Claude
                .setup(
                    cloud_upstream(provider_type),
                    "http://127.0.0.1:1",
                    "secret",
                    &fixture.root,
                    |name| (name == "PATH").then(|| path.clone()),
                )
                .unwrap();
            let isolation = fixture
                .credentials
                .isolate("work", provider_type, Cli::Claude)
                .unwrap();
            for (route, _) in CLAUDE_ROUTES {
                let set = setup.env.iter().any(|(name, _)| name == route);
                assert_ne!(
                    set,
                    isolation.unset.contains(route),
                    "{provider_type}: {route} is either set or cleared"
                );
            }
            let cloud = matches!(provider_type, BEDROCK | VERTEX);
            for cli in Cli::value_variants() {
                let isolation = fixture
                    .credentials
                    .isolate("work", provider_type, *cli)
                    .unwrap();
                for name in CLOUD_CREDENTIALS {
                    assert_eq!(
                        isolation.unset.contains(name),
                        cloud,
                        "{cli} on {provider_type}: {name}"
                    );
                }
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_sign_in_runs_the_vendors_own_in_the_providers_dir() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let stub = fixture.home.join("stub");
        fs::write(
            &stub,
            "#!/bin/sh\nhome=\"$CODEX_HOME$PI_CODING_AGENT_DIR\"\nprintf '%s\\n' \"$@\" > \"$home/args\"\nprintf '{\"openai-codex\":{}}' > \"$home/auth.json\"\nexec grep SigIgn /proc/self/status > \"$home/ignored\"\n",
        )
        .unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        let interrupt = || {
            // SAFETY: a null new action only reads the current one into `current`.
            let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
            unsafe { libc::sigaction(libc::SIGINT, std::ptr::null(), &raw mut current) };
            current.sa_sigaction
        };
        let before = interrupt();
        let credentials = &fixture.credentials;
        for (cli, args) in [
            (Cli::Codex, format!("login\n-c\n{CODEX_FILE_STORE}\n")),
            (Cli::Pi, "auth\nlogin\nopenai-codex\n".to_owned()),
        ] {
            let status = credentials
                .sign_in("work", CODEX_SUBSCRIPTION, cli, Some(stub.as_os_str()))
                .await
                .unwrap();
            assert!(status.success());
            assert_eq!(interrupt(), before, "ags handles Ctrl-C as before");
            let dir = fixture.provider("work").join(cli.to_string());
            assert_eq!(fs::read_to_string(dir.join("args")).unwrap(), args);
            #[cfg(target_os = "linux")]
            {
                let ignored = fs::read_to_string(dir.join("ignored")).unwrap();
                let mask = u64::from_str_radix(ignored.trim().trim_start_matches("SigIgn:\t"), 16)
                    .unwrap();
                // Read by a program the script execs, since a shell ignores SIGQUIT itself.
                assert_eq!(
                    mask & 0b110,
                    0,
                    "the sign-in gets Ctrl-C and Ctrl-\\: {ignored}"
                );
            }
            assert!(
                credentials
                    .has("work", CODEX_SUBSCRIPTION, Need::SignIn(cli))
                    .unwrap()
            );
        }
    }
}
