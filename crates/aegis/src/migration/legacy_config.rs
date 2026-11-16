//! The config file's `[[providers]]` and `[guardrails]` and the `GUARDRAILS_*`
//! variables, which releases before the providers migration
//! (`m20261111_000030_providers`) read. Only the one-time import of that
//! migration uses them (see `settings::import_config_once`).
use crate::config::{
    self, GuardrailsConfig, ProviderConfig, validate_detectors, validate_providers,
};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::{env, fs, path::Path};

#[derive(Debug, Default, Deserialize)]
pub struct LegacyConfig {
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    #[serde(default)]
    pub guardrails: GuardrailsConfig,
}

impl LegacyConfig {
    /// Reads them from the config file, if any, with the variables
    /// overriding its guardrails.
    pub fn read() -> Result<Self> {
        let path = config::file_path();
        let mut legacy = if Path::new(&path).exists() {
            toml::from_str::<Self>(
                &fs::read_to_string(&path)
                    .with_context(|| format!("failed to read configuration from {path}"))?,
            )
            .with_context(|| format!("failed to parse configuration from {path}"))?
        } else {
            Self::default()
        };
        apply_guardrails_env(&mut legacy.guardrails)?;
        validate_providers(&legacy.providers)?;
        validate_detectors(&legacy.guardrails)?;
        Ok(legacy)
    }
}

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

fn optional_var(name: &str) -> Result<Option<String>> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error).with_context(|| format!("{name} must be valid Unicode")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GuardrailsMode;

    #[test]
    fn parses_multiple_accounts() {
        let config: LegacyConfig = toml::from_str(
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
    fn server_settings_in_the_same_file_are_ignored() {
        let config: LegacyConfig = toml::from_str(
            "http_addr = \"127.0.0.1:8765\"\n[smtp]\nhost = \"smtp.example.com\"\n[[pricing.overrides]]\nmodel = \"m\"\ninput_per_mtok = 1.0\noutput_per_mtok = 2.0",
        )
        .expect("configuration should parse");
        assert!(config.providers.is_empty());
        assert!(!config.guardrails.enabled);
    }

    #[test]
    fn guardrails_default_to_disabled_observation_and_parse_mask_mode() {
        let config: LegacyConfig = toml::from_str("").expect("empty configuration should parse");
        assert!(!config.guardrails.enabled);
        assert_eq!(config.guardrails.mode, GuardrailsMode::Observe);

        let config: LegacyConfig = toml::from_str(
            r#"
            [guardrails]
            enabled = true
            mode = "mask"
            "#,
        )
        .expect("configuration should parse");
        assert!(config.guardrails.enabled);
        assert_eq!(config.guardrails.mode, GuardrailsMode::Mask);
        assert!(toml::from_str::<LegacyConfig>("[guardrails]\nmode = \"block\"").is_err());
        assert!("block".parse::<GuardrailsMode>().is_err());
    }

    #[test]
    fn regex_detectors_parse_and_are_refused_when_misnamed_or_broken() {
        let parsed = |detectors: &str| {
            let config: LegacyConfig = toml::from_str(&format!(
                "[guardrails.regex]\nenabled = true\n[guardrails.regex.detectors]\n{detectors}"
            ))
            .expect("configuration should parse");
            validate_detectors(&config.guardrails)
        };
        let empty: LegacyConfig = toml::from_str("").unwrap();
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
}
