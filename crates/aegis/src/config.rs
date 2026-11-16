use crate::domain::{AuthConfig, OAuthConfig, SmtpSettings};
use aegis_wire::provider_type::{
    BEDROCK, CLAUDE_SUBSCRIPTION, CODEX_SUBSCRIPTION, TYPESAFE, VERTEX,
};
use anyhow::{Context, Result, anyhow, bail};
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
    str::FromStr,
    time::Duration,
};

pub struct Config {
    pub http_addr: SocketAddr,
    pub database_url: String,
    pub max_capture_bytes: usize,
    pub providers: Vec<ProviderConfig>,
    pub public_url: Option<String>,
    pub registration_enabled: bool,
    pub smtp: Option<SmtpSettings>,
    pub auth: AuthConfig,
    pub oauth: OAuthConfig,
    pub pricing: PricingConfig,
    pub guardrails: GuardrailsConfig,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PricingConfig {
    #[serde(default = "default_pricing_url")]
    pub url: String,
    #[serde(default = "default_pricing_refresh_hours")]
    pub refresh_hours: u64,
    #[serde(default = "default_pricing_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub overrides: Vec<PriceOverride>,
}

impl Default for PricingConfig {
    fn default() -> Self {
        Self {
            url: default_pricing_url(),
            refresh_hours: default_pricing_refresh_hours(),
            enabled: default_pricing_enabled(),
            overrides: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PriceOverride {
    pub model: String,
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    #[serde(default)]
    pub cache_read_per_mtok: Option<f64>,
    #[serde(default)]
    pub cache_write_per_mtok: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderConfig {
    pub id: String,
    #[serde(flatten)]
    pub kind: ProviderKind,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
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

#[derive(Debug, Deserialize, Serialize)]
struct FileConfig {
    #[serde(default = "default_http_addr")]
    http_addr: SocketAddr,
    #[serde(default = "default_database_url")]
    database_url: String,
    #[serde(default = "default_max_capture_bytes")]
    max_capture_bytes: usize,
    #[serde(default)]
    providers: Vec<ProviderConfig>,
    #[serde(default)]
    public_url: Option<String>,
    #[serde(default)]
    registration_enabled: bool,
    #[serde(default)]
    pricing: PricingConfig,
    #[serde(default)]
    guardrails: GuardrailsConfig,
    #[serde(default)]
    smtp: SmtpFileConfig,
    #[serde(default)]
    auth: AuthFileConfig,
}

#[derive(Debug, Deserialize, Serialize)]
struct SmtpFileConfig {
    #[serde(default)]
    host: Option<String>,
    #[serde(default = "default_smtp_port")]
    port: u16,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

impl Default for SmtpFileConfig {
    fn default() -> Self {
        Self {
            host: None,
            port: default_smtp_port(),
            from: None,
            username: None,
            password: None,
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct AuthFileConfig {
    #[serde(default)]
    access_token_minutes: Option<u64>,
}

type ApplyOverride = fn(&mut FileConfig, String) -> Result<(), &'static str>;

/// Every scalar config key and the environment variable that overrides it:
/// the key path uppercased, with sections joined by `_`.
const ENV_OVERRIDES: &[(&str, &str, ApplyOverride)] = &[
    ("http_addr", "HTTP_ADDR", |file, value| {
        parse(value, "an IP address and port").map(|value| file.http_addr = value)
    }),
    ("public_url", "PUBLIC_URL", |file, value| {
        file.public_url = Some(value);
        Ok(())
    }),
    ("database_url", "DATABASE_URL", |file, value| {
        file.database_url = value;
        Ok(())
    }),
    ("max_capture_bytes", "MAX_CAPTURE_BYTES", |file, value| {
        parse(value, "a positive integer").map(|value| file.max_capture_bytes = value)
    }),
    (
        "registration_enabled",
        "REGISTRATION_ENABLED",
        |file, value| parse(value, "true or false").map(|value| file.registration_enabled = value),
    ),
    ("smtp.host", "SMTP_HOST", |file, value| {
        file.smtp.host = Some(value);
        Ok(())
    }),
    ("smtp.port", "SMTP_PORT", |file, value| {
        parse(value, "a port").map(|value| file.smtp.port = value)
    }),
    ("smtp.from", "SMTP_FROM", |file, value| {
        file.smtp.from = Some(value);
        Ok(())
    }),
    ("smtp.username", "SMTP_USERNAME", |file, value| {
        file.smtp.username = Some(value);
        Ok(())
    }),
    ("smtp.password", "SMTP_PASSWORD", |file, value| {
        file.smtp.password = Some(value);
        Ok(())
    }),
    (
        "auth.access_token_minutes",
        "AUTH_ACCESS_TOKEN_MINUTES",
        |file, value| {
            parse(value, "a positive integer")
                .map(|value| file.auth.access_token_minutes = Some(value))
        },
    ),
    ("pricing.enabled", "PRICING_ENABLED", |file, value| {
        parse(value, "true or false").map(|value| file.pricing.enabled = value)
    }),
    (
        "pricing.refresh_hours",
        "PRICING_REFRESH_HOURS",
        |file, value| {
            parse(value, "a positive integer").map(|value| file.pricing.refresh_hours = value)
        },
    ),
    ("pricing.url", "PRICING_URL", |file, value| {
        file.pricing.url = value;
        Ok(())
    }),
];

fn parse<T: FromStr>(value: String, expected: &'static str) -> Result<T, &'static str> {
    value.parse().map_err(|_| expected)
}

/// Overrides `file` with every set variable of [`ENV_OVERRIDES`].
fn apply_env_overrides(
    file: &mut FileConfig,
    var: impl Fn(&str) -> Result<Option<String>>,
) -> Result<()> {
    for (_, name, apply) in ENV_OVERRIDES {
        if let Some(value) = var(name)? {
            apply(file, value).map_err(|expected| anyhow!("{name} must be {expected}"))?;
        }
    }
    Ok(())
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            http_addr: default_http_addr(),
            database_url: default_database_url(),
            max_capture_bytes: default_max_capture_bytes(),
            providers: Vec::new(),
            public_url: None,
            registration_enabled: false,
            pricing: PricingConfig::default(),
            guardrails: GuardrailsConfig::default(),
            smtp: SmtpFileConfig::default(),
            auth: AuthFileConfig::default(),
        }
    }
}

impl Config {
    #[cfg(test)]
    pub fn test_default() -> Self {
        Self {
            http_addr: default_http_addr(),
            database_url: default_database_url(),
            max_capture_bytes: default_max_capture_bytes(),
            providers: Vec::new(),
            public_url: None,
            registration_enabled: false,
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
                access_token_lifetime: OAuthConfig::DEFAULT_ACCESS_TOKEN_LIFETIME,
            },
            pricing: PricingConfig::default(),
            guardrails: GuardrailsConfig::default(),
            secret_placeholder_key: [4; 32],
        }
    }

    pub fn from_env() -> Result<Self> {
        let path = env::var("AEGIS_CONFIG").unwrap_or_else(|_| "config.toml".into());
        let mut file = if Path::new(&path).exists() {
            toml::from_str::<FileConfig>(
                &fs::read_to_string(&path)
                    .with_context(|| format!("failed to read configuration from {path}"))?,
            )
            .with_context(|| format!("failed to parse configuration from {path}"))?
        } else {
            FileConfig::default()
        };
        apply_env_overrides(&mut file, optional_var)?;

        if file.max_capture_bytes == 0 {
            bail!("MAX_CAPTURE_BYTES (max_capture_bytes) must be greater than zero");
        }
        validate_providers(&file.providers)?;
        validate_pricing(&file.pricing)?;

        let mut guardrails = file.guardrails;
        apply_guardrails_env(&mut guardrails)?;
        validate_detectors(&guardrails)?;

        let access_token_lifetime = access_token_lifetime(file.auth.access_token_minutes)?;
        let smtp = smtp_settings(file.smtp)?;
        let root_key = root_key()?;
        Ok(Self {
            http_addr: file.http_addr,
            database_url: file.database_url,
            max_capture_bytes: file.max_capture_bytes,
            providers: file.providers,
            public_url: file.public_url,
            registration_enabled: file.registration_enabled,
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
            pricing: file.pricing,
            guardrails,
            secret_placeholder_key: derive_key(&root_key, b"aegis/v1/secret-placeholder"),
        })
    }
}

/// Environment variables override the file's guardrails. Both are read only by
/// the one-time import into the database (see `settings::import_config_once`).
fn apply_guardrails_env(guardrails: &mut GuardrailsConfig) -> Result<()> {
    if let Some(enabled) = optional_var("GUARDRAILS_ENABLED")? {
        guardrails.enabled = enabled
            .parse()
            .context("GUARDRAILS_ENABLED must be true or false")?;
    }
    if let Some(mode) = optional_var("GUARDRAILS_MODE")? {
        guardrails.mode = mode.parse()?;
    }
    if let Some(enabled) = optional_var("GUARDRAILS_SECRETS_ENABLED")? {
        guardrails.secrets.enabled = enabled
            .parse()
            .context("GUARDRAILS_SECRETS_ENABLED must be true or false")?;
    }
    Ok(())
}

fn smtp_settings(smtp: SmtpFileConfig) -> Result<Option<SmtpSettings>> {
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

fn optional_var(name: &str) -> Result<Option<String>> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error).with_context(|| format!("{name} must be valid Unicode")),
    }
}

fn access_token_lifetime(minutes: Option<u64>) -> Result<Duration> {
    const TOO_LARGE: &str = "AUTH_ACCESS_TOKEN_MINUTES (auth.access_token_minutes) is too large";
    let Some(minutes) = minutes else {
        return Ok(OAuthConfig::DEFAULT_ACCESS_TOKEN_LIFETIME);
    };
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

fn default_http_addr() -> SocketAddr {
    "127.0.0.1:8765".parse().expect("valid default address")
}

fn default_database_url() -> String {
    "sqlite://data/aegis.db?mode=rwc".into()
}

fn default_max_capture_bytes() -> usize {
    16 * 1024 * 1024
}

fn default_smtp_port() -> u16 {
    587
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

fn default_pricing_url() -> String {
    "https://raw.githubusercontent.com/dvjn/aegis/main/pricing/model_prices.json".into()
}

fn default_pricing_refresh_hours() -> u64 {
    12
}

fn default_pricing_enabled() -> bool {
    true
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

    /// Every scalar set to a value other than its default.
    fn populated() -> FileConfig {
        FileConfig {
            http_addr: "0.0.0.0:9000".parse().unwrap(),
            database_url: "sqlite://other.db".into(),
            max_capture_bytes: 1024,
            providers: Vec::new(),
            public_url: Some("https://aegis.example.com".into()),
            registration_enabled: true,
            pricing: PricingConfig {
                url: "https://example.com/prices.json".into(),
                refresh_hours: 6,
                enabled: false,
                overrides: Vec::new(),
            },
            guardrails: GuardrailsConfig::default(),
            smtp: SmtpFileConfig {
                host: Some("smtp.example.com".into()),
                port: 2525,
                from: Some("aegis@example.com".into()),
                username: Some("aegis".into()),
                password: Some("secret".into()),
            },
            auth: AuthFileConfig {
                access_token_minutes: Some(15),
            },
        }
    }

    /// Each scalar's dotted key path and value; lists and the legacy
    /// guardrails, imported once into the database, have no variables.
    fn scalars(config: &FileConfig) -> BTreeMap<String, toml::Value> {
        fn walk(prefix: &str, table: &toml::Table, out: &mut BTreeMap<String, toml::Value>) {
            for (key, value) in table {
                let path = format!("{prefix}{key}");
                match value {
                    toml::Value::Table(table) => walk(&format!("{path}."), table, out),
                    toml::Value::Array(_) => {}
                    scalar => {
                        out.insert(path, scalar.clone());
                    }
                }
            }
        }
        let mut table = toml::Table::try_from(config).unwrap();
        table.remove("guardrails");
        let mut out = BTreeMap::new();
        walk("", &table, &mut out);
        out
    }

    #[test]
    fn every_scalar_key_has_its_env_var() {
        let keys: Vec<_> = ENV_OVERRIDES.iter().map(|(key, _, _)| *key).collect();
        let scalar_keys: Vec<_> = scalars(&populated()).into_keys().collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, scalar_keys);
        for (key, var, _) in ENV_OVERRIDES {
            assert_eq!(*var, key.replace('.', "_").to_uppercase());
        }
    }

    #[test]
    fn each_env_var_overrides_its_own_key() {
        let wanted = scalars(&populated());
        for (key, var, _) in ENV_OVERRIDES {
            let value = match &wanted[*key] {
                toml::Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            let mut config = FileConfig::default();
            let before = scalars(&config);
            apply_env_overrides(
                &mut config,
                |name| Ok((name == *var).then(|| value.clone())),
            )
            .unwrap();
            let after = scalars(&config);
            assert_eq!(after.get(*key), Some(&wanted[*key]), "{var}");
            for (other, value) in &before {
                if other != key {
                    assert_eq!(after.get(other), Some(value), "{var} changed {other}");
                }
            }
        }
    }

    #[test]
    fn an_unparsable_env_var_is_named() {
        for (var, value) in [
            ("HTTP_ADDR", "8765"),
            ("REGISTRATION_ENABLED", "yes"),
            ("SMTP_PORT", "70000"),
            ("AUTH_ACCESS_TOKEN_MINUTES", "-5"),
            ("AUTH_ACCESS_TOKEN_MINUTES", "1.5"),
            ("AUTH_ACCESS_TOKEN_MINUTES", ""),
            ("AUTH_ACCESS_TOKEN_MINUTES", "99999999999999999999"),
        ] {
            let error = apply_env_overrides(&mut FileConfig::default(), |name| {
                Ok((name == var).then(|| value.into()))
            })
            .unwrap_err();
            assert!(error.to_string().starts_with(var), "{var}={value}: {error}");
        }
    }

    #[test]
    fn access_tokens_last_five_minutes_unless_overridden() {
        assert_eq!(access_token_lifetime(None).unwrap().as_secs(), 300);
        assert_eq!(access_token_lifetime(Some(15)).unwrap().as_secs(), 900);
        let config: FileConfig = toml::from_str("[auth]\naccess_token_minutes = 15").unwrap();
        assert_eq!(config.auth.access_token_minutes, Some(15));
    }

    #[test]
    fn access_token_minutes_must_be_in_range() {
        for minutes in [0, 9_999_999_999_999, u64::MAX] {
            let error = access_token_lifetime(Some(minutes)).unwrap_err();
            assert!(
                error.to_string().contains("auth.access_token_minutes"),
                "{minutes}"
            );
        }
        assert!(toml::from_str::<FileConfig>("[auth]\naccess_token_minutes = -5").is_err());
    }

    #[test]
    fn smtp_is_read_from_the_file_and_validated() {
        let smtp = |table: &str| {
            let config: FileConfig = toml::from_str(&format!("[smtp]\n{table}")).unwrap();
            smtp_settings(config.smtp)
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
    fn parses_multiple_accounts() {
        let config: FileConfig = toml::from_str(
            r#"
            [[providers]]
            id = "claude-personal"
            type = "claude_subscription"

            [[providers]]
            id = "claude-work"
            type = "claude_subscription"
            "#,
        )
        .expect("configuration should parse");
        validate_providers(&config.providers).expect("providers should be valid");
        assert_eq!(config.providers.len(), 2);
        assert_eq!(
            config.providers[0].kind.base_url(),
            "https://api.anthropic.com"
        );
    }

    #[test]
    fn applies_file_defaults_during_deserialization() {
        let config: FileConfig = toml::from_str("").expect("empty configuration should parse");
        assert_eq!(config.http_addr, default_http_addr());
        assert_eq!(config.database_url, default_database_url());
        assert!(config.providers.is_empty());
    }

    #[test]
    fn parses_a_pricing_block_with_overrides() {
        let config: FileConfig = toml::from_str(
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
        )
        .expect("configuration should parse");
        validate_pricing(&config.pricing).expect("pricing should be valid");
        assert_eq!(config.pricing.url, "https://example.invalid/prices.json");
        assert_eq!(config.pricing.refresh_hours, 6);
        assert!(!config.pricing.enabled);
        let [override_entry] = config.pricing.overrides.as_slice() else {
            panic!("one override should parse");
        };
        assert_eq!(override_entry.model, "claude-opus-4-5");
        assert_eq!(override_entry.cache_read_per_mtok, Some(0.5));
        assert_eq!(override_entry.cache_write_per_mtok, None);
    }

    #[test]
    fn pricing_falls_back_to_defaults_when_the_section_is_absent() {
        let config: FileConfig = toml::from_str("").expect("empty configuration should parse");
        assert_eq!(
            config.pricing.url,
            "https://raw.githubusercontent.com/dvjn/aegis/main/pricing/model_prices.json"
        );
        assert_eq!(config.pricing.refresh_hours, 12);
        assert!(config.pricing.enabled);
        assert!(config.pricing.overrides.is_empty());
        validate_pricing(&config.pricing).expect("defaults should be valid");
    }

    #[test]
    fn pricing_rejects_a_plaintext_url_and_a_zero_refresh_interval() {
        let plaintext = PricingConfig {
            url: "http://example.invalid/prices.json".into(),
            ..PricingConfig::default()
        };
        assert!(validate_pricing(&plaintext).is_err());

        let never_refreshed = PricingConfig {
            refresh_hours: 0,
            ..PricingConfig::default()
        };
        assert!(validate_pricing(&never_refreshed).is_err());
    }

    #[test]
    fn guardrails_default_to_disabled_observation_and_parse_mask_mode() {
        let config: FileConfig = toml::from_str("").expect("empty configuration should parse");
        assert!(!config.guardrails.enabled);
        assert_eq!(config.guardrails.mode, GuardrailsMode::Observe);

        let config: FileConfig = toml::from_str(
            r#"
            [guardrails]
            enabled = true
            mode = "mask"
            "#,
        )
        .expect("configuration should parse");
        assert!(config.guardrails.enabled);
        assert_eq!(config.guardrails.mode, GuardrailsMode::Mask);
        assert!(toml::from_str::<FileConfig>("[guardrails]\nmode = \"block\"").is_err());
        assert!("block".parse::<GuardrailsMode>().is_err());
    }

    #[test]
    fn regex_detectors_parse_and_are_refused_when_misnamed_or_broken() {
        let parsed = |detectors: &str| {
            let config: FileConfig = toml::from_str(&format!(
                "[guardrails.regex]\nenabled = true\n[guardrails.regex.detectors]\n{detectors}"
            ))
            .expect("configuration should parse");
            validate_detectors(&config.guardrails)
        };
        let empty: FileConfig = toml::from_str("").unwrap();
        assert!(!empty.guardrails.regex.enabled);
        assert!(empty.guardrails.regex.detectors.is_empty());

        let ok =
            parsed("internal_token = '\\bint_[a-z0-9]{32}\\b'\nemployee_id = '\\bEMP-\\d{6}\\b'");
        assert!(ok.is_ok(), "{ok:?}");

        let misnamed = parsed("Internal-Token = 'x'").unwrap_err().to_string();
        assert!(misnamed.contains("\"Internal-Token\""), "{misnamed}");
        assert!(misnamed.contains("snake_case"), "{misnamed}");

        let collision = parsed("github_token = 'x'").unwrap_err().to_string();
        assert!(collision.contains("\"github_token\""), "{collision}");
        assert!(collision.contains("built-in secrets"), "{collision}");

        let broken = format!("{:#}", parsed("open_group = '('").unwrap_err());
        assert!(broken.contains("\"open_group\""), "{broken}");
        assert!(broken.contains("does not compile"), "{broken}");
    }

    #[test]
    fn default_listener_uses_the_documented_port() {
        assert_eq!(default_http_addr().to_string(), "127.0.0.1:8765");
    }
}
