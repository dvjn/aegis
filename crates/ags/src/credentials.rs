//! The credentials each provider needs. A subscription provider uses each CLI's own sign-in, in the
//! user's own config dir. A cloud provider keeps its Bedrock API key or Vertex service account in a
//! private file of ags's, which reaches a CLI only during a launch.
use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    fmt, fs, io,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
};

use aegis_wire::provider_type::{BEDROCK, CLAUDE_SUBSCRIPTION, CODEX_SUBSCRIPTION, VERTEX};
use anyhow::{Context, Result, anyhow, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::process::Command;

use crate::{Cli, Store, launch::granted, shims, store::write_private};

const DIR_NAME: &str = "providers";
const STATE: &str = "state.json";
const BEDROCK_KEY: &str = "bedrock-api-key";
const VERTEX_ACCOUNT: &str = "vertex-service-account.json";

/// One credential a provider needs before a CLI can use it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    /// The CLI's own sign-in, in the user's own config dir.
    SignIn(Cli),
    BedrockKey,
    VertexAccount,
}

impl Need {
    /// Everything a provider of `provider_type` needs: the sign-in of each CLI that runs on it, or its key.
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

    /// What ags must hold before a launch on a provider of `provider_type`. A CLI that is not
    /// signed in asks for its own sign-in.
    pub fn of_launch(provider_type: &str) -> Option<Self> {
        match provider_type {
            BEDROCK => Some(Self::BedrockKey),
            VERTEX => Some(Self::VertexAccount),
            _ => None,
        }
    }

    /// Names this need of `provider` in ags's state; a sign-in is the CLI's, whichever provider.
    fn key(self, provider: &str, provider_type: &str) -> String {
        match self {
            Self::SignIn(cli) => format!("sign-in/{cli}/{provider_type}"),
            Self::BedrockKey => format!("{provider}/{BEDROCK}"),
            Self::VertexAccount => format!("{provider}/{VERTEX}"),
        }
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

/// Credentials Claude Code reads from its environment before its own sign-in or a launch's key.
const CLAUDE_CREDENTIAL_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_REFRESH_TOKEN",
    "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
    "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
    "AWS_BEARER_TOKEN_BEDROCK",
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

/// What a launch of `cli` drops from its environment, and Claude Code's launch settings blank:
/// credentials and upstreams that would take the place of the provider's.
pub(crate) fn launch_unset(cli: Cli, provider_type: &str) -> Vec<&'static str> {
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

/// Changes to a child's environment, applied in order.
#[derive(Debug, Default)]
pub struct EnvChanges(Vec<(&'static str, Option<OsString>)>);

impl EnvChanges {
    pub fn set(&mut self, name: &'static str, value: impl Into<OsString>) {
        self.0.push((name, Some(value.into())));
    }

    pub fn remove(&mut self, name: &'static str) {
        self.0.push((name, None));
    }

    pub fn apply(&self, command: &mut std::process::Command) {
        for (name, value) in &self.0 {
            match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            };
        }
    }
}

/// What a launch gives its CLI beside Claude Code's routes, which a sign-in run from inside that
/// CLI must not inherit either.
const LAUNCH_ENV: &[&str] = &[
    "AGS_LAUNCH_SECRET",
    crate::shims::LAUNCHED,
    "ANTHROPIC_CUSTOM_HEADERS",
    "OPENAI_BASE_URL",
    "AGS_PI_ROUTES",
    "AWS_REGION",
    "AWS_BEARER_TOKEN_BEDROCK",
    "ANTHROPIC_VERTEX_PROJECT_ID",
    "CLOUD_ML_REGION",
    "GOOGLE_CLOUD_PROJECT",
    "GOOGLE_CLOUD_LOCATION",
    "GOOGLE_APPLICATION_CREDENTIALS",
];

/// Inside an Aegis launch, drops what it set.
pub fn outside_launch() -> EnvChanges {
    let mut changes = EnvChanges::default();
    if std::env::var_os(crate::clis::SECRET_ENV).is_none() {
        return changes;
    }
    for name in from_launch() {
        changes.remove(name);
    }
    changes
}

/// What a sign-in run from inside a launch drops before setting its own.
fn from_launch() -> impl Iterator<Item = &'static str> {
    LAUNCH_ENV
        .iter()
        .copied()
        .chain(CLAUDE_ROUTES.iter().map(|(name, _)| *name))
}

/// pi's own provider for providers of `provider_type`, whose name its sign-ins are kept under.
pub(crate) fn pi_provider(provider_type: &str) -> Option<&'static str> {
    match provider_type {
        CLAUDE_SUBSCRIPTION => Some("anthropic"),
        CODEX_SUBSCRIPTION => Some("openai-codex"),
        BEDROCK => Some("amazon-bedrock"),
        VERTEX => Some("google-vertex"),
        _ => None,
    }
}

/// Where the user keeps their own files, outside ags.
#[derive(Clone, Debug, Default)]
pub struct Home {
    /// The user's home dir, for `~` in paths the user gives.
    user: Option<PathBuf>,
    /// Claude Code's config dir, where the user's settings are.
    claude: Option<PathBuf>,
    /// The value of `PATH` that ags finds each CLI on.
    path: Option<OsString>,
}

impl Home {
    pub fn from_env() -> Self {
        Self::from_vars(|name| std::env::var_os(name))
    }

    fn from_vars(var: impl Fn(&str) -> Option<OsString>) -> Self {
        let path = |value: OsString| Some(PathBuf::from(value)).filter(|path| path.is_absolute());
        let user = var("HOME").or_else(|| var("USERPROFILE")).and_then(path);
        Self {
            claude: var("CLAUDE_CONFIG_DIR")
                .and_then(path)
                .or_else(|| user.as_ref().map(|home| home.join(".claude"))),
            user,
            path: var("PATH"),
        }
    }

    /// The default locations under `home`, with no CLI on `PATH`.
    pub fn at(home: &Path) -> Self {
        Self {
            user: Some(home.to_owned()),
            claude: Some(home.join(".claude")),
            path: None,
        }
    }

    /// Finds each CLI on `path`, a value of `PATH`.
    #[must_use]
    pub fn with_path(mut self, path: impl Into<OsString>) -> Self {
        self.path = Some(path.into());
        self
    }
}

/// The credentials of every provider on this machine.
pub struct Credentials {
    dir: PathBuf,
    home: Home,
    /// Its lock guards `state.json`.
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

/// Something about the user's settings they should hear about once.
struct Notice {
    key: String,
    text: String,
}

impl Credentials {
    pub fn new(store: &Store, home: Home) -> Self {
        Self {
            dir: store.dir().join(DIR_NAME),
            home,
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

    /// The real `cli`, past ags's shims.
    pub(crate) fn program(&self, cli: Cli) -> Result<PathBuf> {
        shims::program_on(&self.store, cli, self.home.path.as_deref())
    }

    /// Whether the real `cli` is installed.
    pub fn installed(&self, cli: Cli) -> bool {
        self.program(cli).is_ok()
    }

    /// Whether `provider` has `need`: a cloud credential ags keeps, or the CLI's own sign-in of the
    /// kind the provider takes, as the CLI's status command reports it.
    pub async fn has(&self, provider: &str, provider_type: &str, need: Need) -> Result<bool> {
        Ok(match need {
            Need::SignIn(cli) => self.signed_in(cli, provider_type).await,
            Need::BedrockKey | Need::VertexAccount => {
                is_file(&self.cloud_file(provider, provider_type)?)
            }
        })
    }

    /// Whether `cli` has the sign-in a `provider_type` launch uses: a Claude subscription, a
    /// ChatGPT login, or pi's own OAuth sign-in to that provider, not an API key.
    async fn signed_in(&self, cli: Cli, provider_type: &str) -> bool {
        let Ok(program) = self.program(cli) else {
            return false;
        };
        let mut command = Command::new(program);
        match cli {
            Cli::Claude => {
                // The blanks a launch gives Claude Code, so the user's settings answer as they would then.
                let blanks: Map<String, Value> = launch_unset(cli, provider_type)
                    .into_iter()
                    .map(|name| (name.to_owned(), "".into()))
                    .collect();
                let settings = serde_json::json!({ "env": blanks }).to_string();
                command
                    .arg("--settings")
                    .arg(settings)
                    .args(["auth", "status", "--json"])
            }
            Cli::Codex => command.args(["login", "status"]),
            Cli::Pi => {
                let Some(name) = pi_provider(provider_type) else {
                    return false;
                };
                command.args([
                    "auth",
                    "check",
                    "--provider",
                    name,
                    "--no-refresh",
                    "--json",
                ])
            }
        };
        as_users(&mut command, cli, provider_type);
        command.stdin(Stdio::null()).kill_on_drop(true);
        let Ok(output) = command.output().await else {
            return false;
        };
        let json = || serde_json::from_slice::<Map<String, Value>>(&output.stdout).ok();
        match cli {
            Cli::Claude => json().is_some_and(|status| {
                status.get("loggedIn") == Some(&Value::Bool(true))
                    && field(&status, "authMethod") == Some("claude.ai")
            }),
            Cli::Codex => [&output.stdout, &output.stderr]
                .iter()
                .any(|text| String::from_utf8_lossy(text).contains("Logged in using ChatGPT")),
            Cli::Pi => json().is_some_and(|check| {
                field(&check, "status") == Some("ready")
                    && field(&check, "authType") == Some("oauth")
            }),
        }
    }

    /// What the user has not been told yet about their own settings of `cli`.
    pub fn notices(&self, cli: Cli) -> Result<Vec<String>> {
        let mut notices = vec![];
        if cli == Cli::Claude
            && let Some(own) = &self.home.claude
            && let Some(settings) = read_json(&own.join("settings.json")).ok().flatten()
            && settings.contains_key("apiKeyHelper")
        {
            notices.push(Notice {
                key: "claude/apiKeyHelper".to_owned(),
                text: "your Claude Code settings set apiKeyHelper, so Claude Code sends that key to Claude providers in place of your sign-in".to_owned(),
            });
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

    /// Runs `cli`'s own sign-in for providers of `provider_type`, in the user's own config dir.
    pub async fn sign_in(&self, provider_type: &str, cli: Cli) -> Result<ExitStatus> {
        let program = self.program(cli)?;
        let mut command = Command::new(&program);
        match cli {
            Cli::Claude => command.args(["auth", "login"]),
            Cli::Codex => command.arg("login"),
            Cli::Pi => {
                let Some(name) = pi_provider(provider_type)
                    .filter(|_| matches!(provider_type, CLAUDE_SUBSCRIPTION | CODEX_SUBSCRIPTION))
                else {
                    bail!("pi can't sign in to a {provider_type} provider");
                };
                command.args(["auth", "login", name])
            }
        };
        as_users(&mut command, cli, provider_type);
        command.kill_on_drop(true);
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
        let mut child = crate::launch::spawn(command, program.as_os_str())?;
        child.wait().await.context("waiting for the sign-in")
    }

    /// Remembers that the user chose not to set up `need` for `provider`.
    pub fn skip(&self, provider: &str, provider_type: &str, need: Need) -> Result<()> {
        self.update(|state| {
            state.skipped.insert(need.key(provider, provider_type));
            Ok(())
        })
    }

    /// Forgets that `need` was skipped for `provider`, once it is set up.
    pub fn unskip(&self, provider: &str, provider_type: &str, need: Need) -> Result<()> {
        let key = need.key(provider, provider_type);
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

    /// The file of a cloud provider's stored credential: its Bedrock API key or Vertex service account.
    pub(crate) fn cloud_file(&self, provider: &str, provider_type: &str) -> Result<PathBuf> {
        let dir = self.provider_dir(provider)?;
        match provider_type {
            BEDROCK => Ok(dir.join(BEDROCK_KEY)),
            VERTEX => Ok(dir.join(VERTEX_ACCOUNT)),
            _ => bail!("{provider} is not a cloud provider"),
        }
    }

    /// How a CLI finds a cloud provider's stored credential: the variables its SDK reads.
    pub fn cloud_env(
        &self,
        provider: &str,
        provider_type: &str,
    ) -> Result<Vec<(&'static str, OsString)>> {
        Ok(match provider_type {
            BEDROCK => {
                let key = String::from_utf8(read(&self.cloud_file(provider, provider_type)?)?)
                    .context("the stored Bedrock API key is not text")?;
                vec![("AWS_BEARER_TOKEN_BEDROCK", key.trim().into())]
            }
            VERTEX => vec![(
                "GOOGLE_APPLICATION_CREDENTIALS",
                self.cloud_file(provider, provider_type)?.into_os_string(),
            )],
            _ => vec![],
        })
    }
}

/// Makes `command` run as the user's own CLI would, for providers of `provider_type`: with nothing
/// of a launch it may run in, and without what a launch drops.
fn as_users(command: &mut Command, cli: Cli, provider_type: &str) {
    for name in launch_unset(cli, provider_type)
        .into_iter()
        .chain(from_launch())
    {
        command.env_remove(name);
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

/// The credentials of every granted provider, or only of `provider`. A sign-in that several
/// providers share is listed once, under the first of them.
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
    let mut steps: Vec<Step> = vec![];
    for id in &machine.providers {
        if provider.is_some_and(|only| only != id) {
            continue;
        }
        let Some(provider_type) = machine.provider_types.get(id) else {
            continue;
        };
        for need in Need::all(provider_type) {
            let shared = matches!(need, Need::SignIn(_))
                && steps
                    .iter()
                    .any(|step| step.need == need && step.provider_type == *provider_type);
            if shared {
                continue;
            }
            steps.push(Step {
                provider: id.clone(),
                provider_type: provider_type.clone(),
                need,
                status: if credentials.has(id, provider_type, need).await? {
                    StepStatus::Done
                } else if skipped.contains(&need.key(id, provider_type)) {
                    StepStatus::Skipped
                } else {
                    StepStatus::Missing
                },
            });
        }
    }
    Ok(steps)
}

/// Whether `program` is an executable in one of the dirs of `path`, a value of `PATH`.
pub(crate) fn on_path(program: &str, path: Option<&OsStr>) -> bool {
    !crate::shims::executables(program, path).is_empty()
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

fn field<'a>(json: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    json.get(name).and_then(Value::as_str)
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
pub(crate) mod tests {
    use super::*;
    use crate::{clis::Upstream, store::tests::scratch};

    /// The variables that point a CLI at a config dir; a launch or sign-in never sets them.
    pub(crate) const CONFIG_DIRS: [&str; 3] =
        ["CLAUDE_CONFIG_DIR", "CODEX_HOME", "PI_CODING_AGENT_DIR"];

    struct Fixture {
        root: PathBuf,
        home: PathBuf,
        bin: PathBuf,
        credentials: Credentials,
    }

    impl Fixture {
        fn new() -> Self {
            let root = scratch();
            let home = root.join("home");
            let bin = root.join("bin");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&bin).unwrap();
            let credentials = Credentials::new(
                &Store::at(root.join("ags")),
                Home::at(&home).with_path(&bin),
            );
            Self {
                root,
                home,
                bin,
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

    /// Everything under `dir`, as sorted paths relative to it.
    fn tree(dir: &Path) -> Vec<PathBuf> {
        let mut found = vec![];
        let mut pending = vec![dir.to_owned()];
        while let Some(next) = pending.pop() {
            for entry in fs::read_dir(&next).into_iter().flatten().flatten() {
                let path = entry.path();
                found.push(path.strip_prefix(dir).unwrap().to_owned());
                if path.is_dir() {
                    pending.push(path);
                }
            }
        }
        found.sort();
        found
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_provider_needs_each_clis_sign_in_or_its_key() {
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
            Need::of_launch(CODEX_SUBSCRIPTION),
            None,
            "the CLI asks for its own sign-in"
        );
        assert_eq!(Need::of_launch(BEDROCK), Some(Need::BedrockKey));
        assert_eq!(Need::of_launch("typesafe"), None);
        assert_eq!(Need::SignIn(Cli::Claude).to_string(), "Claude Code sign-in");
        assert_eq!(Need::BedrockKey.to_string(), "Bedrock API key");
    }

    #[test]
    fn a_launch_drops_what_would_replace_its_providers_sign_in_and_upstream() {
        let claude = launch_unset(Cli::Claude, CLAUDE_SUBSCRIPTION);
        assert!(claude.contains(&"ANTHROPIC_API_KEY"));
        assert!(claude.contains(&"CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR"));
        for (route, _) in CLAUDE_ROUTES {
            assert_eq!(
                claude.contains(route),
                *route != "ANTHROPIC_BASE_URL",
                "{route}"
            );
        }
        let on_bedrock = launch_unset(Cli::Claude, BEDROCK);
        assert!(!on_bedrock.contains(&"CLAUDE_CODE_USE_BEDROCK"));
        assert!(on_bedrock.contains(&"CLAUDE_CODE_USE_VERTEX"));
        let codex = launch_unset(Cli::Codex, CODEX_SUBSCRIPTION);
        assert!(codex.contains(&"OPENAI_API_KEY"));
        assert!(codex.contains(&"CODEX_ACCESS_TOKEN"));
        assert!(launch_unset(Cli::Pi, CLAUDE_SUBSCRIPTION).contains(&"ANTHROPIC_OAUTH_TOKEN"));
        for cli in Cli::value_variants() {
            for provider_type in cli.provider_types() {
                let unset = launch_unset(*cli, provider_type);
                for name in CLOUD_CREDENTIALS {
                    assert_eq!(
                        unset.contains(name),
                        matches!(*provider_type, BEDROCK | VERTEX),
                        "{cli} on {provider_type}: {name}"
                    );
                }
                for name in CONFIG_DIRS
                    .into_iter()
                    .chain(["CLAUDE_SECURESTORAGE_CONFIG_DIR"])
                {
                    assert!(!unset.contains(&name), "the user's own {name} stays");
                }
            }
        }
    }

    #[test]
    fn the_users_own_claude_code_dir_holds_their_settings() {
        let vars = |pairs: &'static [(&str, &str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(known, _)| *known == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        let home = Home::from_vars(vars(&[("HOME", "/home/me"), ("PATH", "/bin")]));
        assert_eq!(home.claude, Some(PathBuf::from("/home/me/.claude")));
        assert_eq!(home.path, Some(OsString::from("/bin")));
        let own = Home::from_vars(vars(&[
            ("HOME", "/home/me"),
            ("CLAUDE_CONFIG_DIR", "/home/me/work-claude"),
        ]));
        assert_eq!(own.claude, Some(PathBuf::from("/home/me/work-claude")));
        assert_eq!(Home::at(Path::new("/home/me")).path, None);
    }

    #[test]
    fn a_skipped_sign_in_is_the_clis_whichever_provider_it_was_skipped_for() {
        let fixture = Fixture::new();
        let credentials = &fixture.credentials;
        let pi = Need::SignIn(Cli::Pi);
        credentials.skip("work", CLAUDE_SUBSCRIPTION, pi).unwrap();
        let skipped = credentials.state().unwrap().skipped;
        assert!(skipped.contains(&pi.key("home", CLAUDE_SUBSCRIPTION)));
        assert!(!skipped.contains(&pi.key("work", CODEX_SUBSCRIPTION)));
        credentials.skip("aws", BEDROCK, Need::BedrockKey).unwrap();
        let skipped = credentials.state().unwrap().skipped;
        assert!(!skipped.contains(&Need::BedrockKey.key("aws2", BEDROCK)));
        credentials.unskip("home", CLAUDE_SUBSCRIPTION, pi).unwrap();
        assert!(
            !credentials
                .state()
                .unwrap()
                .skipped
                .contains(&pi.key("work", CLAUDE_SUBSCRIPTION))
        );
    }

    #[test]
    fn only_a_key_helper_in_the_users_settings_is_named_and_once() {
        let fixture = Fixture::new();
        fixture.write(
            ".claude/settings.json",
            r#"{"apiKeyHelper":"key.sh","env":{"ANTHROPIC_API_KEY":"k","ANTHROPIC_UNIX_SOCKET":"/run/s",
            "CLAUDE_CODE_USE_VERTEX":"1","AWS_PROFILE":"dev","EDITOR":"vi"}}"#,
        );
        let credentials = &fixture.credentials;
        assert_eq!(
            credentials.notices(Cli::Claude).unwrap(),
            [
                "your Claude Code settings set apiKeyHelper, so Claude Code sends that key to Claude providers in place of your sign-in",
            ]
        );
        assert!(credentials.notices(Cli::Claude).unwrap().is_empty());
        assert!(credentials.notices(Cli::Codex).unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_cloud_provider_keeps_its_credential_in_a_private_file() {
        let fixture = Fixture::new();
        let credentials = &fixture.credentials;
        assert!(
            !credentials
                .has("aws", BEDROCK, Need::BedrockKey)
                .await
                .unwrap()
        );
        for refused in ["", "  ", "two words"] {
            assert!(credentials.save_bedrock_key("aws", refused).is_err());
        }
        credentials
            .save_bedrock_key("aws", " ABSKexample\n")
            .unwrap();
        assert!(
            credentials
                .has("aws", BEDROCK, Need::BedrockKey)
                .await
                .unwrap()
        );
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
        assert!(
            !credentials
                .has("gcp", VERTEX, Need::VertexAccount)
                .await
                .unwrap()
        );
        let account = r#"{"type":"service_account","project_id":"p","client_email":"a@p.iam.gserviceaccount.com","private_key":"-----BEGIN PRIVATE KEY-----"}"#;
        fixture.write("key.json", account);
        credentials
            .save_vertex_account("gcp", Path::new("~/key.json"))
            .unwrap();
        let stored = fixture.provider("gcp").join(VERTEX_ACCOUNT);
        assert_eq!(fs::read_to_string(&stored).unwrap(), account);
        assert_eq!(credentials.cloud_file("gcp", VERTEX).unwrap(), stored);
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
            assert_eq!(mode(&fixture.root.join("ags").join(DIR_NAME)), 0o700);
        }
        let work = credentials.cloud_file("Work", BEDROCK).unwrap();
        let name = work
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        assert!(name.starts_with("work."), "{name}");
        assert_ne!(
            work,
            credentials.cloud_file("work", BEDROCK).unwrap(),
            "Work and work stay apart where case is ignored"
        );
        for refused in ["", "../work", "a/b", ".", &"x".repeat(65)] {
            assert!(
                credentials.cloud_file(refused, BEDROCK).is_err(),
                "{refused:?}"
            );
        }
    }

    #[test]
    fn a_sign_in_inherits_nothing_a_launch_sets() {
        let fixture = Fixture::new();
        let dropped: Vec<&str> = from_launch().collect();
        // A launch only adds loopback hosts to the user's own list, which a sign-in may keep.
        let kept: Vec<&str> = crate::launch::no_proxy(|_| None)
            .iter()
            .map(|(name, _)| *name)
            .collect();
        for name in [crate::pi::ROUTES_ENV, crate::clis::SECRET_ENV] {
            assert!(dropped.contains(&name), "pi sets {name}");
        }
        fs::write(fixture.bin.join("gcloud"), "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                fixture.bin.join("gcloud"),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        let path = fixture.bin.to_str().unwrap().to_owned();
        for cli in [Cli::Claude, Cli::Codex] {
            for provider_type in cli.provider_types() {
                let upstream = Upstream {
                    id: "work",
                    provider_type,
                    region: Some("us-east-1"),
                    project: Some("p"),
                };
                let setup = cli
                    .setup(
                        upstream,
                        "http://127.0.0.1:1",
                        "secret",
                        &fixture.root,
                        |name| (name == "PATH").then(|| path.clone()),
                    )
                    .unwrap();
                let cloud = ["AWS_BEARER_TOKEN_BEDROCK", "GOOGLE_APPLICATION_CREDENTIALS"];
                for name in setup.env.iter().map(|(name, _)| *name).chain(cloud) {
                    assert!(dropped.contains(&name), "{cli} sets {name}");
                }
            }
        }
        assert_eq!(kept, ["NO_PROXY", "no_proxy"]);
        assert!(kept.iter().all(|name| !dropped.contains(name)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_sign_in_runs_the_vendors_own_in_the_users_dir() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        // Each stand-in CLI prints the status in `<cli>-status`, which its sign-in replaces with
        // `<cli>-login`, and records how it ran.
        let stub = "#!/bin/sh\nout=\"$(dirname \"$0\")\"\nname=\"${0##*/}\"\n\
            case \"$*\" in\n\
            *'auth status --json'|'login status'|'auth check '*) printf '%s\\n' \"$@\" > \"$out/$name-checked\"; cat \"$out/$name-status\"; exit 0 ;;\n\
            esac\n\
            printf '%s\\n' \"$@\" > \"$out/$name-args\"\n\
            printf '%s' \"${CLAUDE_CONFIG_DIR-unset} ${CODEX_HOME-unset} ${PI_CODING_AGENT_DIR-unset}\" > \"$out/$name-dirs\"\n\
            cp \"$out/$name-login\" \"$out/$name-status\"\n\
            exec grep SigIgn /proc/self/status > \"$out/$name-ignored\"\n";
        // An API key the CLI holds is not the sign-in a subscription provider takes.
        for (cli, key, login) in [
            (
                "claude",
                r#"{"loggedIn":true,"authMethod":"api_key"}"#,
                r#"{"loggedIn":true,"authMethod":"claude.ai"}"#,
            ),
            (
                "codex",
                "API key configured - ***",
                "Logged in using ChatGPT",
            ),
            (
                "pi",
                r#"{"status":"ready","authType":"api_key"}"#,
                r#"{"status":"ready","authType":"oauth"}"#,
            ),
        ] {
            let path = fixture.bin.join(cli);
            fs::write(&path, stub).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            fs::write(fixture.bin.join(format!("{cli}-status")), key).unwrap();
            fs::write(fixture.bin.join(format!("{cli}-login")), login).unwrap();
        }
        let earlier = fixture.provider("work").join("claude");
        fs::create_dir_all(&earlier).unwrap();
        fs::write(earlier.join(".claude.json"), "{}").unwrap();
        let interrupt = || {
            // SAFETY: a null new action only reads the current one into `current`.
            let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
            unsafe { libc::sigaction(libc::SIGINT, std::ptr::null(), &raw mut current) };
            current.sa_sigaction
        };
        let before = interrupt();
        let theirs = CONFIG_DIRS
            .map(|name| std::env::var(name).unwrap_or_else(|_| "unset".to_owned()))
            .join(" ");
        let credentials = &fixture.credentials;
        for (cli, provider_type, args) in [
            (Cli::Claude, CLAUDE_SUBSCRIPTION, "auth\nlogin\n"),
            (Cli::Codex, CODEX_SUBSCRIPTION, "login\n"),
            (Cli::Pi, CODEX_SUBSCRIPTION, "auth\nlogin\nopenai-codex\n"),
        ] {
            let need = Need::SignIn(cli);
            assert!(!credentials.has("work", provider_type, need).await.unwrap());
            let status = credentials.sign_in(provider_type, cli).await.unwrap();
            assert!(status.success());
            assert_eq!(interrupt(), before, "ags handles Ctrl-C as before");
            let read = |what: &str| fs::read_to_string(fixture.bin.join(format!("{cli}-{what}")));
            assert_eq!(read("args").unwrap(), args);
            assert_eq!(
                read("dirs").unwrap(),
                theirs,
                "{cli} signs in to the user's own dir"
            );
            #[cfg(target_os = "linux")]
            {
                let ignored = read("ignored").unwrap();
                let mask = u64::from_str_radix(ignored.trim().trim_start_matches("SigIgn:\t"), 16)
                    .unwrap();
                // Read by a program the script execs, since a shell ignores SIGQUIT itself.
                assert_eq!(
                    mask & 0b110,
                    0,
                    "the sign-in gets Ctrl-C and Ctrl-\\: {ignored}"
                );
            }
            assert!(credentials.has("work", provider_type, need).await.unwrap());
        }
        // Claude Code reports its sign-in as a launch would leave it, with the same blanks.
        let checked = fs::read_to_string(fixture.bin.join("claude-checked")).unwrap();
        let checked: Vec<&str> = checked.lines().collect();
        assert_eq!(checked[0], "--settings");
        let blanks: Value = serde_json::from_str(checked[1]).unwrap();
        assert_eq!(blanks["env"]["ANTHROPIC_API_KEY"], "");
        assert_eq!(blanks["env"]["CLAUDE_CODE_USE_BEDROCK"], "");
        assert_eq!(checked[2..], ["auth", "status", "--json"]);
        assert!(
            tree(&fixture.home).is_empty(),
            "ags writes nothing of the user's"
        );
        assert_eq!(
            tree(&fixture.provider("work")),
            [
                PathBuf::from("claude"),
                PathBuf::from("claude/.claude.json")
            ]
        );
        assert!(
            credentials
                .sign_in(BEDROCK, Cli::Pi)
                .await
                .unwrap_err()
                .to_string()
                .starts_with("pi can't sign in")
        );
        fs::remove_file(fixture.bin.join("codex")).unwrap();
        assert!(!credentials.installed(Cli::Codex));
        assert!(
            !credentials
                .has("work", CODEX_SUBSCRIPTION, Need::SignIn(Cli::Codex))
                .await
                .unwrap(),
            "a CLI that is not installed is not signed in"
        );
    }
}
