use crate::domain::{AuthConfig, OAuthConfig, SmtpSettings};
use aegis_wire::provider_type::{
    BEDROCK, CLAUDE_SUBSCRIPTION, CODEX_SUBSCRIPTION, TYPESAFE, VERTEX,
};
use anyhow::{Context, Result, bail};
use chrono::Datelike;
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    io::{ErrorKind, Write},
    net::SocketAddr,
    path::Path,
    time::Duration,
};

pub struct Config {
    pub http_addr: SocketAddr,
    pub database_url: String,
    pub max_capture_bytes: usize,
    pub public_url: Option<String>,
    pub registration_enabled: bool,
    pub smtp: Option<SmtpSettings>,
    pub auth: AuthConfig,
    pub oauth: OAuthConfig,
    pub pricing: PricingConfig,
    pub secret_placeholder_key: [u8; 32],
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct GuardrailsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub mode: GuardrailsMode,
    #[serde(default = "GuardrailConfig::on")]
    pub secrets: GuardrailConfig,
    #[serde(default)]
    pub regex: RegexGuardrailConfig,
}

/// One guardrail. `detectors` names the subset to run; absent, every detector
/// of that guardrail runs.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct GuardrailConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub detectors: Option<Vec<String>>,
}

impl GuardrailConfig {
    pub fn on() -> Self {
        Self {
            enabled: true,
            detectors: None,
        }
    }
}

/// User-defined detectors: a name and the regex it runs. Every match is
/// masked like a secret.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct RegexGuardrailConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub detectors: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GuardrailsMode {
    #[default]
    Observe,
    Mask,
}

impl std::str::FromStr for GuardrailsMode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "observe" => Ok(Self::Observe),
            "mask" => Ok(Self::Mask),
            other => bail!("guardrails mode {other:?} must be observe or mask"),
        }
    }
}

#[derive(Clone, Debug, confique::Config)]
pub struct PricingConfig {
    #[config(env = "PRICING_ENABLED", default = true)]
    pub enabled: bool,
    #[config(env = "PRICING_REFRESH_HOURS", default = 12)]
    pub refresh_hours: u64,
    #[config(
        env = "PRICING_URL",
        default = "https://raw.githubusercontent.com/dvjn/aegis/main/pricing/model_prices.json"
    )]
    pub url: String,
    #[config(default = [])]
    pub overrides: Vec<PriceOverride>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PriceOverride {
    pub model: String,
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    #[serde(default)]
    pub cache_read_per_mtok: Option<f64>,
    #[serde(default)]
    pub cache_write_per_mtok: Option<f64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    #[serde(flatten)]
    pub kind: ProviderKind,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderKind {
    ClaudeSubscription {
        #[serde(default = "default_anthropic_base_url")]
        base_url: String,
    },
    CodexSubscription {
        #[serde(default = "default_codex_base_url")]
        base_url: String,
    },
    #[serde(rename = "typesafe")]
    TypeSafe {
        #[serde(default = "default_typesafe_base_url")]
        base_url: String,
    },
    /// Amazon Bedrock in one region, named by the runtime host in `base_url`.
    /// Claude Code and pi reach the runtime endpoint; Codex reaches the
    /// region's OpenAI-compatible endpoint, derived from the same region.
    Bedrock {
        #[serde(default = "default_bedrock_base_url")]
        base_url: String,
    },
    /// Google Vertex AI. With the default `base_url`, each request goes to the
    /// Google host for the location named in its own path.
    Vertex {
        #[serde(default = "default_vertex_base_url")]
        base_url: String,
    },
}

impl ProviderKind {
    pub const TYPE_NAMES: [&'static str; 5] = [
        CLAUDE_SUBSCRIPTION,
        CODEX_SUBSCRIPTION,
        TYPESAFE,
        BEDROCK,
        VERTEX,
    ];

    pub fn type_name(&self) -> &'static str {
        match self {
            Self::ClaudeSubscription { .. } => CLAUDE_SUBSCRIPTION,
            Self::CodexSubscription { .. } => CODEX_SUBSCRIPTION,
            Self::TypeSafe { .. } => TYPESAFE,
            Self::Bedrock { .. } => BEDROCK,
            Self::Vertex { .. } => VERTEX,
        }
    }

    /// The AWS region of a Bedrock provider whose upstream is a standard
    /// runtime host.
    pub fn bedrock_region(&self) -> Option<&str> {
        let Self::Bedrock { base_url } = self else {
            return None;
        };
        bedrock_region(base_url)
    }

    /// The location of a Vertex provider whose upstream is one location's
    /// own host; the global host serves every location.
    pub fn vertex_location(&self) -> Option<&str> {
        let Self::Vertex { base_url } = self else {
            return None;
        };
        vertex_location(base_url)
    }

    /// Rebuilds a kind from its stored type name; an absent `base_url` takes
    /// that type's default upstream.
    pub fn from_parts(type_name: &str, base_url: Option<String>) -> Result<Self> {
        Ok(match type_name {
            CLAUDE_SUBSCRIPTION => Self::ClaudeSubscription {
                base_url: base_url.unwrap_or_else(default_anthropic_base_url),
            },
            CODEX_SUBSCRIPTION => Self::CodexSubscription {
                base_url: base_url.unwrap_or_else(default_codex_base_url),
            },
            TYPESAFE => Self::TypeSafe {
                base_url: base_url.unwrap_or_else(default_typesafe_base_url),
            },
            BEDROCK => Self::Bedrock {
                base_url: base_url.unwrap_or_else(default_bedrock_base_url),
            },
            VERTEX => Self::Vertex {
                base_url: base_url.unwrap_or_else(default_vertex_base_url),
            },
            other => bail!(
                "provider type {other:?} must be one of {}",
                Self::TYPE_NAMES.join(", ")
            ),
        })
    }

    pub fn base_url(&self) -> &str {
        match self {
            Self::ClaudeSubscription { base_url }
            | Self::CodexSubscription { base_url }
            | Self::TypeSafe { base_url }
            | Self::Bedrock { base_url }
            | Self::Vertex { base_url } => base_url,
        }
    }
}

/// The server settings in the config file. Each scalar also has an
/// environment variable, its key path uppercased with sections joined by `_`,
/// which wins over the file. Unknown keys, such as the `[[providers]]` and
/// `[guardrails]` of older releases, are ignored.
#[derive(confique::Config)]
struct Settings {
    #[config(env = "HTTP_ADDR", default = "127.0.0.1:8765")]
    http_addr: SocketAddr,
    #[config(env = "PUBLIC_URL")]
    public_url: Option<String>,
    #[config(env = "DATABASE_URL", default = "sqlite://data/aegis.db?mode=rwc")]
    database_url: String,
    #[config(env = "MAX_CAPTURE_BYTES", default = 16777216)]
    max_capture_bytes: usize,
    #[config(env = "REGISTRATION_ENABLED", default = false)]
    registration_enabled: bool,
    #[config(nested)]
    smtp: Smtp,
    #[config(nested)]
    auth: Auth,
    #[config(nested)]
    pricing: PricingConfig,
}

#[derive(confique::Config)]
struct Smtp {
    #[config(env = "SMTP_HOST")]
    host: Option<String>,
    #[config(env = "SMTP_PORT", default = 587)]
    port: u16,
    #[config(env = "SMTP_FROM")]
    from: Option<String>,
    #[config(env = "SMTP_USERNAME")]
    username: Option<String>,
    #[config(env = "SMTP_PASSWORD")]
    password: Option<String>,
}

#[derive(confique::Config)]
struct Auth {
    #[config(env = "AUTH_ACCESS_TOKEN_MINUTES", default = 5)]
    access_token_minutes: u64,
}

pub fn file_path() -> String {
    env::var("AEGIS_CONFIG").unwrap_or_else(|_| "config.toml".into())
}

/// The file's settings, read as TOML whatever its extension; a missing file
/// sets nothing.
fn file_settings(path: &str) -> Result<<Settings as confique::Config>::Layer, confique::Error> {
    confique::File::with_format(path, confique::FileFormat::Toml).load()
}

impl Config {
    #[cfg(test)]
    pub fn test_default() -> Self {
        use confique::{Config as _, Layer as _};
        let settings =
            Settings::from_layer(<Settings as confique::Config>::Layer::default_values())
                .expect("every setting has a default or is optional");
        Self {
            http_addr: settings.http_addr,
            database_url: settings.database_url,
            max_capture_bytes: settings.max_capture_bytes,
            public_url: settings.public_url,
            registration_enabled: settings.registration_enabled,
            smtp: None,
            auth: AuthConfig {
                session_hmac_key: [1; 32],
                session_key_id: "v1".into(),
                password_pepper: vec![2; 32],
                pepper_key_id: "v1".into(),
                password_concurrency: 2,
                idle_lifetime: Duration::from_secs(60),
                absolute_lifetime: Duration::from_secs(60),
            },
            oauth: OAuthConfig {
                hmac_key: [3; 32],
                key_id: "v1".into(),
                access_token_lifetime: access_token_lifetime(settings.auth.access_token_minutes)
                    .expect("the default lifetime is valid"),
            },
            pricing: settings.pricing,
            secret_placeholder_key: [4; 32],
        }
    }

    pub fn from_env() -> Result<Self> {
        use confique::Config as _;
        let settings = Settings::builder()
            .env()
            .preloaded(file_settings(&file_path())?)
            .load()?;
        if settings.max_capture_bytes == 0 {
            bail!("MAX_CAPTURE_BYTES (max_capture_bytes) must be greater than zero");
        }
        validate_pricing(&settings.pricing)?;
        let access_token_lifetime = access_token_lifetime(settings.auth.access_token_minutes)?;
        let smtp = smtp_settings(settings.smtp)?;
        let root_key = root_key()?;
        Ok(Self {
            http_addr: settings.http_addr,
            database_url: settings.database_url,
            max_capture_bytes: settings.max_capture_bytes,
            public_url: settings.public_url,
            registration_enabled: settings.registration_enabled,
            smtp,
            auth: AuthConfig {
                session_hmac_key: derive_key(&root_key, b"aegis/v1/session-hmac"),
                session_key_id: "v1".into(),
                password_pepper: derive_key(&root_key, b"aegis/v1/password-pepper").to_vec(),
                pepper_key_id: "v1".into(),
                password_concurrency: 2,
                idle_lifetime: Duration::from_secs(30 * 24 * 60 * 60),
                absolute_lifetime: Duration::from_secs(90 * 24 * 60 * 60),
            },
            oauth: OAuthConfig {
                hmac_key: derive_key(&root_key, b"aegis/v1/oauth-hmac"),
                key_id: "v1".into(),
                access_token_lifetime,
            },
            pricing: settings.pricing,
            secret_placeholder_key: derive_key(&root_key, b"aegis/v1/secret-placeholder"),
        })
    }
}

fn smtp_settings(smtp: Smtp) -> Result<Option<SmtpSettings>> {
    let Some(host) = smtp.host else {
        return Ok(None);
    };
    if host.is_empty() {
        bail!("SMTP_HOST (smtp.host) must not be empty");
    }
    if smtp.port == 0 {
        bail!("SMTP_PORT (smtp.port) must be 1..=65535");
    }
    if smtp.username.is_some() != smtp.password.is_some() {
        bail!(
            "SMTP_USERNAME (smtp.username) and SMTP_PASSWORD (smtp.password) must be set together"
        );
    }
    let from = smtp
        .from
        .context("SMTP_FROM (smtp.from) is required when SMTP_HOST (smtp.host) is set")?;
    if from.parse::<email_address::EmailAddress>().is_err() {
        bail!("SMTP_FROM (smtp.from) must be an email address");
    }
    Ok(Some(SmtpSettings {
        host,
        port: smtp.port,
        username: smtp.username,
        password: smtp.password,
        from,
    }))
}

fn access_token_lifetime(minutes: u64) -> Result<Duration> {
    const TOO_LARGE: &str = "AUTH_ACCESS_TOKEN_MINUTES (auth.access_token_minutes) is too large";
    if minutes == 0 {
        bail!("AUTH_ACCESS_TOKEN_MINUTES (auth.access_token_minutes) must be greater than zero");
    }
    let lifetime = minutes
        .checked_mul(60)
        .map(Duration::from_secs)
        .context(TOO_LARGE)?;
    let expires = chrono::Duration::from_std(lifetime)
        .ok()
        .and_then(|lifetime| chrono::Utc::now().checked_add_signed(lifetime))
        .filter(|expires| expires.year() <= 9999);
    if expires.is_none() {
        bail!(TOO_LARGE);
    }
    Ok(lifetime)
}

fn root_key() -> Result<[u8; 32]> {
    let path = Path::new("data/root.key");
    match read_root_key(path) {
        Ok(key) => return Ok(key),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("failed to read data/root.key"),
    }
    fs::create_dir_all("data").context("failed to create data directory")?;
    let mut key = [0_u8; 32];
    getrandom::fill(&mut key)
        .map_err(|error| anyhow::anyhow!("failed to generate root key: {error}"))?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    match options.open(path) {
        Ok(mut file) => {
            file.write_all(&key)?;
            file.sync_all()?;
            Ok(key)
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            read_root_key(path).context("failed to read concurrently-created data/root.key")
        }
        Err(error) => Err(error).context("failed to create data/root.key"),
    }
}

fn read_root_key(path: &Path) -> std::io::Result<[u8; 32]> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            ErrorKind::PermissionDenied,
            "root key must be a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(std::io::Error::new(
                ErrorKind::PermissionDenied,
                "root key must not be accessible by group or other users",
            ));
        }
    }
    let bytes = fs::read(path)?;
    bytes.as_slice().try_into().map_err(|_| {
        std::io::Error::new(
            ErrorKind::InvalidData,
            "root key must contain exactly 32 bytes",
        )
    })
}

fn derive_key(root: &[u8; 32], info: &[u8]) -> [u8; 32] {
    let mut output = [0_u8; 32];
    Hkdf::<Sha256>::new(None, root)
        .expand(info, &mut output)
        .expect("32 bytes is a valid HKDF output length");
    output
}

fn default_anthropic_base_url() -> String {
    "https://api.anthropic.com".into()
}

fn default_codex_base_url() -> String {
    "https://chatgpt.com/backend-api/codex".into()
}

fn default_typesafe_base_url() -> String {
    "https://api.typesafe.ai".into()
}

fn default_bedrock_base_url() -> String {
    bedrock_runtime_url("us-east-1")
}

pub fn bedrock_runtime_url(region: &str) -> String {
    format!("https://bedrock-runtime.{region}.amazonaws.com")
}

/// The region of a standard Bedrock runtime URL, such as
/// `https://bedrock-runtime.eu-west-1.amazonaws.com`.
pub fn bedrock_region(base_url: &str) -> Option<&str> {
    let host = base_url
        .strip_prefix("https://bedrock-runtime.")?
        .trim_end_matches('/');
    let region = host.strip_suffix(".amazonaws.com")?;
    is_cloud_region(region).then_some(region)
}

/// Letters, digits and hyphens only: a region or location becomes part of a
/// host name, so nothing else may reach it.
pub fn is_cloud_region(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

pub fn default_vertex_base_url() -> String {
    "https://aiplatform.googleapis.com".into()
}

/// The upstream of a provider of `type_name` in `region`: a Bedrock region or
/// a Vertex location. `None` for a type that has no regions.
pub fn region_url(type_name: &str, region: &str) -> Option<String> {
    match type_name {
        BEDROCK => Some(bedrock_runtime_url(region)),
        VERTEX => Some(vertex_url(region)),
        _ => None,
    }
}

/// Vertex's host for `location`: `global` uses the global host, the `eu` and
/// `us` multi-regions their own, and a region its regional host.
pub fn vertex_url(location: &str) -> String {
    match location {
        "global" => default_vertex_base_url(),
        "eu" | "us" => format!("https://aiplatform.{location}.rep.googleapis.com"),
        region => format!("https://{region}-aiplatform.googleapis.com"),
    }
}

/// The location of a Vertex URL that `vertex_url` builds for one location
/// other than `global`.
pub fn vertex_location(base_url: &str) -> Option<&str> {
    let host = base_url.strip_prefix("https://")?.trim_end_matches('/');
    let location = match host.strip_suffix("-aiplatform.googleapis.com") {
        Some(region) => region,
        None => host
            .strip_prefix("aiplatform.")?
            .strip_suffix(".rep.googleapis.com")
            .filter(|multi| matches!(*multi, "eu" | "us"))?,
    };
    is_cloud_region(location).then_some(location)
}

pub(crate) fn validate_providers(providers: &[ProviderConfig]) -> Result<()> {
    let mut ids = HashSet::new();
    for provider in providers {
        let valid_id = !provider.id.is_empty()
            && provider.id.len() <= 64
            && provider
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
        if !valid_id {
            bail!(
                "provider id {:?} must contain only letters, digits, '-' or '_' and be at most 64 characters",
                provider.id
            );
        }
        if !ids.insert(&provider.id) {
            bail!("provider id {:?} is duplicated", provider.id);
        }
        let url = url::Url::parse(provider.kind.base_url())
            .with_context(|| format!("provider {:?} has an invalid base_url", provider.id))?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            bail!(
                "provider {:?} base_url must use http or https and include a host",
                provider.id
            );
        }
    }
    Ok(())
}

fn validate_pricing(pricing: &PricingConfig) -> Result<()> {
    let url =
        url::Url::parse(&pricing.url).context("PRICING_URL (pricing.url) is not a valid URL")?;
    if url.scheme() != "https" || url.host_str().is_none() {
        bail!("PRICING_URL (pricing.url) must use https and include a host");
    }
    if pricing.refresh_hours < 1 {
        bail!("PRICING_REFRESH_HOURS (pricing.refresh_hours) must be at least 1");
    }
    Ok(())
}

/// A misspelled detector would silently mask nothing, so a name that no
/// guardrail defines is refused at startup.
pub(crate) fn validate_detectors(guardrails: &GuardrailsConfig) -> Result<()> {
    if let Some(names) = &guardrails.secrets.detectors {
        let known = crate::policies::detect::names(&crate::policies::secrets::DETECTORS);
        for name in names {
            if !known.contains(&name.as_str()) {
                bail!("guardrails secrets detector {name:?} is unknown, expected one of {known:?}");
            }
        }
    }
    validate_regex_detectors(&guardrails.regex)
}

/// A regex detector name becomes a capture group and a placeholder, so it has
/// to be a snake_case identifier that no secrets detector already uses.
fn validate_regex_detectors(regex: &RegexGuardrailConfig) -> Result<()> {
    let built_in = crate::policies::detect::names(&crate::policies::secrets::DETECTORS);
    for (name, pattern) in &regex.detectors {
        if !is_snake_case_identifier(name) {
            bail!(
                "guardrails regex detector {name:?} must be a lowercase snake_case identifier such as \"internal_token\""
            );
        }
        if built_in.contains(&name.as_str()) {
            bail!(
                "guardrails regex detector {name:?} collides with the built-in secrets detector of the same name"
            );
        }
        regex::Regex::new(pattern).with_context(|| {
            format!("guardrails regex detector {name:?} pattern {pattern:?} does not compile")
        })?;
    }
    crate::policies::detect::DetectorSet::regex(&regex.detectors)
        .context("guardrails regex detectors do not compile together")?;
    Ok(())
}

fn is_snake_case_identifier(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_lowercase())
        && characters.all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use confique::{
        Config as _,
        meta::{FieldKind, Meta},
    };

    type SettingsLayer = <Settings as confique::Config>::Layer;

    /// Keys that only the file sets, because they hold lists.
    const FILE_ONLY: [&str; 1] = ["pricing.overrides"];

    /// Settings from a config file alone, without the process environment.
    fn from_file(toml: &str) -> Result<Settings, confique::Error> {
        Settings::builder()
            .preloaded(toml::from_str(toml).expect("configuration should parse"))
            .load()
    }

    /// Each setting's key path and environment variable.
    fn settings() -> Vec<(String, Option<&'static str>)> {
        fn walk(meta: &Meta, prefix: &str, out: &mut Vec<(String, Option<&'static str>)>) {
            for field in meta.fields {
                let key = format!("{prefix}{}", field.name);
                match field.kind {
                    FieldKind::Leaf { env, .. } => out.push((key, env)),
                    FieldKind::Nested { meta } => walk(meta, &format!("{key}."), out),
                }
            }
        }
        let mut out = Vec::new();
        walk(&Settings::META, "", &mut out);
        out
    }

    #[test]
    fn every_setting_has_its_env_var_and_is_documented() {
        let docs = include_str!("../../../docs/server-configuration.md");
        let mut seen = HashSet::new();
        for (key, env) in settings() {
            if FILE_ONLY.contains(&key.as_str()) {
                assert_eq!(env, None, "{key}");
                continue;
            }
            let env = env.unwrap_or_else(|| panic!("{key} has no environment variable"));
            assert_eq!(env, key.replace('.', "_").to_uppercase(), "{key}");
            assert!(seen.insert(env), "{env} names two settings");
            assert!(
                docs.contains(&format!("`{env}` (`{key}`)")),
                "{env} ({key}) is not in docs/server-configuration.md"
            );
        }
    }

    /// Prints the settings that the process environment layers over a fixed
    /// file. Run as a child by [`load_in_child`], with an environment of its own.
    #[test]
    #[ignore = "run in a child process by the environment tests"]
    fn print_settings_from_the_env() {
        let file = toml::from_str(
            "[smtp]\nport = 2525\npassword = \"file\"\n[pricing]\nrefresh_hours = 6",
        )
        .unwrap();
        match Settings::builder().env().preloaded(file).load() {
            Ok(settings) => println!(
                "settings: password={:?} username={:?} from={:?} port={} refresh_hours={}",
                settings.smtp.password,
                settings.smtp.username,
                settings.smtp.from,
                settings.smtp.port,
                settings.pricing.refresh_hours,
            ),
            Err(error) => println!("settings: error {error}"),
        }
    }

    /// What [`print_settings_from_the_env`] prints with only `vars` set.
    fn load_in_child(vars: &[(&str, &str)]) -> String {
        let output = std::process::Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "config::tests::print_settings_from_the_env",
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .envs(vars.iter().copied())
            .output()
            .unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(output.status.success(), "{stdout}");
        stdout
            .lines()
            .find_map(|line| line.strip_prefix("settings: "))
            .unwrap_or_else(|| panic!("the child printed no settings: {stdout}"))
            .to_owned()
    }

    #[test]
    fn env_vars_win_over_the_file_and_keep_strings_exact() {
        assert_eq!(
            load_in_child(&[
                ("SMTP_PASSWORD", "12345"),
                ("SMTP_USERNAME", "true"),
                ("SMTP_FROM", "\"[quoted]\""),
                ("PRICING_REFRESH_HOURS", "3"),
            ]),
            r#"password=Some("12345") username=Some("true") from=Some("\"[quoted]\"") port=2525 refresh_hours=3"#
        );
        let error = load_in_child(&[("SMTP_PORT", "abc")]);
        assert!(
            error.starts_with("error ") && error.contains("SMTP_PORT"),
            "{error}"
        );
    }

    #[test]
    fn the_file_is_toml_whatever_its_extension_and_optional() {
        let dir = env::temp_dir().join(format!("aegis-config-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let conf = dir.join("aegis.conf");
        fs::write(&conf, "[smtp]\nport = 2525").unwrap();
        let load = |path: &Path| {
            Settings::builder()
                .preloaded(file_settings(path.to_str().unwrap()).unwrap())
                .load()
                .ok()
                .map(|settings| settings.smtp.port)
        };
        assert_eq!(load(&conf), Some(2525));
        assert_eq!(load(&dir.join("missing")), Some(587));
        assert_eq!(load(Path::new("/dev/null")), Some(587));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn defaults_apply_and_legacy_tables_are_ignored() {
        let Ok(settings) = from_file(
            r#"
            [guardrails]
            enabled = true
            mode = "mask"

            [[providers]]
            id = "claude"
            type = "claude_subscription"
            "#,
        ) else {
            panic!("legacy tables should be ignored");
        };
        assert_eq!(settings.http_addr.to_string(), "127.0.0.1:8765");
        assert_eq!(settings.database_url, "sqlite://data/aegis.db?mode=rwc");
        assert_eq!(settings.max_capture_bytes, 16 * 1024 * 1024);
        assert_eq!(settings.public_url, None);
        assert!(!settings.registration_enabled);
        assert_eq!(settings.smtp.port, 587);
        assert_eq!(settings.auth.access_token_minutes, 5);
    }

    #[test]
    fn access_tokens_last_five_minutes_unless_overridden() {
        assert_eq!(
            Config::test_default().oauth.access_token_lifetime.as_secs(),
            300
        );
        assert_eq!(access_token_lifetime(15).unwrap().as_secs(), 900);
        let Ok(settings) = from_file("[auth]\naccess_token_minutes = 15") else {
            panic!("configuration should load");
        };
        assert_eq!(settings.auth.access_token_minutes, 15);
    }

    #[test]
    fn access_token_minutes_must_be_in_range() {
        for minutes in [0, 9_999_999_999_999, u64::MAX] {
            let error = access_token_lifetime(minutes).unwrap_err();
            assert!(
                error.to_string().contains("AUTH_ACCESS_TOKEN_MINUTES"),
                "{minutes}"
            );
        }
        assert!(toml::from_str::<SettingsLayer>("[auth]\naccess_token_minutes = -5").is_err());
    }

    #[test]
    fn smtp_is_read_from_the_file_and_validated() {
        let smtp = |table: &str| {
            let Ok(settings) = from_file(&format!("[smtp]\n{table}")) else {
                panic!("{table} should load");
            };
            smtp_settings(settings.smtp)
        };
        assert!(smtp("port = 25").unwrap().is_none());
        let settings = smtp("host = \"smtp.example.com\"\nfrom = \"aegis@example.com\"")
            .unwrap()
            .unwrap();
        assert_eq!(settings.host, "smtp.example.com");
        assert_eq!(settings.port, 587);
        assert_eq!(settings.from, "aegis@example.com");
        assert!(settings.username.is_none());

        for (table, named) in [
            ("host = \"\"\nfrom = \"a@example.com\"", "smtp.host"),
            ("host = \"h\"", "smtp.from"),
            ("host = \"h\"\nfrom = \"not an address\"", "smtp.from"),
            (
                "host = \"h\"\nfrom = \"a@example.com\"\nport = 0",
                "smtp.port",
            ),
            (
                "host = \"h\"\nfrom = \"a@example.com\"\nusername = \"u\"",
                "smtp.password",
            ),
        ] {
            let Err(error) = smtp(table) else {
                panic!("{table} should be refused");
            };
            let error = error.to_string();
            assert!(error.contains(named), "{table}: {error}");
        }
    }

    #[test]
    fn a_vertex_host_for_one_location_names_it() {
        for location in ["us-east5", "europe-west4", "eu", "us"] {
            let url = region_url(VERTEX, location).unwrap();
            assert_eq!(vertex_location(&url), Some(location), "{url}");
        }
        assert_eq!(
            region_url(VERTEX, "global").unwrap(),
            default_vertex_base_url()
        );
        for other in [
            "https://aiplatform.googleapis.com",
            "https://aiplatform.asia.rep.googleapis.com",
            "http://us-east5-aiplatform.googleapis.com",
            "https://proxy.example.com/vertex",
            "https://US-aiplatform.googleapis.com",
        ] {
            assert_eq!(vertex_location(other), None, "{other}");
        }
        let regional = ProviderKind::from_parts(VERTEX, region_url(VERTEX, "us-east5")).unwrap();
        assert_eq!(regional.vertex_location(), Some("us-east5"));
        assert_eq!(regional.bedrock_region(), None);
        assert_eq!(
            region_url(BEDROCK, "eu-west-1").as_deref(),
            Some("https://bedrock-runtime.eu-west-1.amazonaws.com")
        );
        assert_eq!(region_url(CLAUDE_SUBSCRIPTION, "us-east-1"), None);
    }

    #[test]
    fn parses_a_pricing_block_with_overrides() {
        let Ok(settings) = from_file(
            r#"
            [pricing]
            url = "https://example.invalid/prices.json"
            refresh_hours = 6
            enabled = false

            [[pricing.overrides]]
            model = "claude-opus-4-5"
            input_per_mtok = 5.0
            output_per_mtok = 25.0
            cache_read_per_mtok = 0.5
            "#,
        ) else {
            panic!("configuration should load");
        };
        let pricing = settings.pricing;
        validate_pricing(&pricing).expect("pricing should be valid");
        assert_eq!(pricing.url, "https://example.invalid/prices.json");
        assert_eq!(pricing.refresh_hours, 6);
        assert!(!pricing.enabled);
        let [override_entry] = pricing.overrides.as_slice() else {
            panic!("one override should parse");
        };
        assert_eq!(override_entry.model, "claude-opus-4-5");
        assert_eq!(override_entry.cache_read_per_mtok, Some(0.5));
        assert_eq!(override_entry.cache_write_per_mtok, None);
    }

    #[test]
    fn pricing_falls_back_to_defaults_when_the_section_is_absent() {
        let pricing = Config::test_default().pricing;
        assert_eq!(
            pricing.url,
            "https://raw.githubusercontent.com/dvjn/aegis/main/pricing/model_prices.json"
        );
        assert_eq!(pricing.refresh_hours, 12);
        assert!(pricing.enabled);
        assert!(pricing.overrides.is_empty());
        validate_pricing(&pricing).expect("defaults should be valid");
    }

    #[test]
    fn pricing_rejects_a_plaintext_url_and_a_zero_refresh_interval() {
        let mut plaintext = Config::test_default().pricing;
        plaintext.url = "http://example.invalid/prices.json".into();
        assert!(validate_pricing(&plaintext).is_err());

        let mut never_refreshed = Config::test_default().pricing;
        never_refreshed.refresh_hours = 0;
        assert!(validate_pricing(&never_refreshed).is_err());
    }
}
