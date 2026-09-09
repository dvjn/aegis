use crate::domain::{AuthConfig, OAuthConfig, SmtpSettings};
use anyhow::{Context, Result, bail};
use hkdf::Hkdf;
use serde::Deserialize;
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

#[derive(Clone, Debug, Default, Deserialize)]
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
#[derive(Clone, Debug, Default, Deserialize)]
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
#[derive(Clone, Debug, Default, Deserialize)]
pub struct RegexGuardrailConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub detectors: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
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

#[derive(Clone, Debug, Deserialize)]
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
}

impl ProviderKind {
    pub fn base_url(&self) -> &str {
        match self {
            Self::ClaudeSubscription { base_url } | Self::CodexSubscription { base_url } => {
                base_url
            }
        }
    }
}

#[derive(Debug, Deserialize)]
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
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let path = env::var("AEGIS_CONFIG").unwrap_or_else(|_| "config.toml".into());
        let file = if Path::new(&path).exists() {
            toml::from_str::<FileConfig>(
                &fs::read_to_string(&path)
                    .with_context(|| format!("failed to read configuration from {path}"))?,
            )
            .with_context(|| format!("failed to parse configuration from {path}"))?
        } else {
            FileConfig::default()
        };

        let http_addr = env::var("HTTP_ADDR")
            .ok()
            .map(|value| value.parse())
            .transpose()
            .context("HTTP_ADDR must be an IP address and port")?
            .unwrap_or(file.http_addr);
        let database_url = env::var("DATABASE_URL").unwrap_or(file.database_url);
        let max_capture_bytes = env::var("MAX_CAPTURE_BYTES")
            .ok()
            .map(|value| value.parse())
            .transpose()
            .context("MAX_CAPTURE_BYTES must be a positive integer")?
            .unwrap_or(file.max_capture_bytes);
        if max_capture_bytes == 0 {
            bail!("MAX_CAPTURE_BYTES must be greater than zero");
        }

        let providers = file.providers;
        validate_providers(&providers)?;

        let mut pricing = file.pricing;
        if let Some(url) = optional_var("PRICING_URL")? {
            pricing.url = url;
        }
        if let Some(hours) = optional_var("PRICING_REFRESH_HOURS")? {
            pricing.refresh_hours = hours
                .parse()
                .context("PRICING_REFRESH_HOURS must be a positive integer")?;
        }
        if let Some(enabled) = optional_var("PRICING_ENABLED")? {
            pricing.enabled = enabled
                .parse()
                .context("PRICING_ENABLED must be true or false")?;
        }
        validate_pricing(&pricing)?;

        let mut guardrails = file.guardrails;
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
        validate_detectors(&guardrails)?;

        let smtp = smtp_config()?;
        let root_key = root_key()?;
        Ok(Self {
            http_addr,
            database_url,
            max_capture_bytes,
            providers,
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
            },
            pricing,
            guardrails,
            secret_placeholder_key: derive_key(&root_key, b"aegis/v1/secret-placeholder"),
        })
    }
}

fn smtp_config() -> Result<Option<SmtpSettings>> {
    let host = match env::var("SMTP_HOST") {
        Ok(value) => value,
        Err(env::VarError::NotPresent) => return Ok(None),
        Err(error) => return Err(error).context("SMTP_HOST must be valid Unicode"),
    };
    if host.is_empty() {
        bail!("SMTP_HOST must not be empty");
    }
    let port = match env::var("SMTP_PORT") {
        Ok(value) => value.parse::<u16>().context("SMTP_PORT must be a port")?,
        Err(env::VarError::NotPresent) => 587,
        Err(error) => return Err(error).context("SMTP_PORT must be valid Unicode"),
    };
    if port == 0 {
        bail!("SMTP_PORT must be 1..=65535");
    }
    let username = optional_var("SMTP_USERNAME")?;
    let password = optional_var("SMTP_PASSWORD")?;
    if username.is_some() != password.is_some() {
        bail!("SMTP_USERNAME and SMTP_PASSWORD must be set together");
    }
    let from = optional_var("SMTP_FROM")?.context("SMTP_FROM is required when SMTP_HOST is set")?;
    if from.parse::<email_address::EmailAddress>().is_err() {
        bail!("SMTP_FROM must be an email address");
    }
    Ok(Some(SmtpSettings {
        host,
        port,
        username,
        password,
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

fn default_anthropic_base_url() -> String {
    "https://api.anthropic.com".into()
}

fn default_codex_base_url() -> String {
    "https://chatgpt.com/backend-api/codex".into()
}

fn default_pricing_url() -> String {
    "https://raw.githubusercontent.com/dvjn/aegis/main/src/pricing/model_prices.json".into()
}

fn default_pricing_refresh_hours() -> u64 {
    12
}

fn default_pricing_enabled() -> bool {
    true
}

fn validate_providers(providers: &[ProviderConfig]) -> Result<()> {
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
    let url = url::Url::parse(&pricing.url).context("pricing url is not a valid URL")?;
    if url.scheme() != "https" || url.host_str().is_none() {
        bail!("pricing url must use https and include a host");
    }
    if pricing.refresh_hours < 1 {
        bail!("pricing refresh_hours must be at least 1");
    }
    Ok(())
}

/// A misspelled detector would silently mask nothing, so a name that no
/// guardrail defines is refused at startup.
fn validate_detectors(guardrails: &GuardrailsConfig) -> Result<()> {
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
            "https://raw.githubusercontent.com/dvjn/aegis/main/src/pricing/model_prices.json"
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
