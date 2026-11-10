use super::state::{self, Config};
use crate::relay_wire::{Connection, Enrollment, RESOURCE_PATH, SCOPE};
use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

pub fn client() -> Result<Client> {
    Ok(Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .build()?)
}

#[derive(Deserialize)]
struct Registration {
    client_id: String,
}
#[derive(Deserialize)]
struct Device {
    device_code: String,
    user_code: String,
    verification_uri_complete: String,
    expires_in: u64,
    interval: u64,
}
#[derive(Deserialize)]
struct Token {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

#[derive(Debug, thiserror::Error)]
#[error("Aegis login expired or was revoked; run ags setup to pair again")]
pub struct LoginRequired;

fn machine_name() -> String {
    #[cfg(unix)]
    {
        let mut hostname = [0u8; 256];
        // gethostname writes into this buffer; use the first NUL if present.
        if unsafe { libc::gethostname(hostname.as_mut_ptr().cast(), hostname.len()) } == 0 {
            let length = hostname
                .iter()
                .position(|&byte| byte == 0)
                .unwrap_or(hostname.len());
            let hostname = String::from_utf8_lossy(&hostname[..length])
                .trim()
                .to_owned();
            if !hostname.is_empty() {
                return hostname;
            }
        }
    }
    std::env::var("HOSTNAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "CLI machine".into())
}

pub async fn pair(server: &str, open_browser: bool) -> Result<Config> {
    let client = client()?;
    let machine = machine_name();
    let scope = format!("{SCOPE} offline_access");
    let registration: Registration = client.post(format!("{server}/oauth/register")).json(&serde_json::json!({
        "client_name": format!("ags · {}", machine.chars().take(60).collect::<String>()), "application_type": "native",
        "grant_types": ["urn:ietf:params:oauth:grant-type:device_code", "refresh_token"],
        "response_types": [], "scope": scope
    })).send().await?.error_for_status()?.json().await?;
    let resource = format!("{server}{RESOURCE_PATH}");
    let device: Device = client
        .post(format!("{server}/oauth/device_authorization"))
        .form(&[
            ("client_id", registration.client_id.as_str()),
            ("resource", &resource),
            ("scope", scope.as_str()),
        ])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    eprintln!(
        "Open {}\nConfirm code: {}",
        device.verification_uri_complete, device.user_code
    );
    // Browser opening is optional. The URL/code always works for remote terminals.
    if open_browser {
        let _ = std::process::Command::new("xdg-open")
            .arg(&device.verification_uri_complete)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    let deadline = Instant::now() + Duration::from_secs(device.expires_in.min(1800));
    let mut interval = device.interval.max(1);
    loop {
        if Instant::now() >= deadline {
            bail!("device approval expired; run setup again");
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let response = client
            .post(format!("{server}/oauth/token"))
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", &registration.client_id),
                ("device_code", &device.device_code),
                ("resource", &resource),
            ])
            .send()
            .await?;
        if response.status().is_success() {
            let token: Token = response.json().await?;
            return Ok(Config {
                server: server.into(),
                client_id: registration.client_id,
                access_token: token.access_token,
                refresh_token: token
                    .refresh_token
                    .context("server did not grant offline access")?,
                expires_at: state::now() + token.expires_in,
                legacy_key: None,
                providers: vec![],
                defaults: BTreeMap::new(),
                provider_defaults: BTreeMap::new(),
                cli_overrides: BTreeMap::new(),
            });
        }
        let error: serde_json::Value = response.json().await?;
        match error["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval = error["interval"].as_u64().unwrap_or(interval + 5),
            Some("access_denied") => bail!("device connection was denied"),
            _ => bail!("device approval failed; run setup again"),
        }
    }
}

/// Serialize refresh-token rotation across concurrent wrappers and save before launching a CLI.
pub async fn connected(home: &Path) -> Result<Config> {
    let path = home.to_owned();
    let _lock = tokio::task::spawn_blocking(move || state::lock(&path)).await??;
    let mut config = state::load(home)?;
    if renew(&mut config).await? {
        state::save(home, &config)?;
    }
    Ok(config)
}

/// Caller holds the state lock, including while repairing setup.
pub async fn renew(config: &mut Config) -> Result<bool> {
    let mut changed = config.legacy_key.take().is_some();
    if config.expires_at <= state::now() + 60 {
        let resource = format!("{}{RESOURCE_PATH}", config.server);
        let response = client()?
            .post(format!("{}/oauth/token", config.server))
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", &config.client_id),
                ("refresh_token", &config.refresh_token),
                ("resource", &resource),
            ])
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(LoginRequired.into());
        }
        if response.status() == reqwest::StatusCode::BAD_REQUEST {
            let error: serde_json::Value = response.json().await?;
            if matches!(
                error["error"].as_str(),
                Some("invalid_grant" | "invalid_client")
            ) {
                return Err(LoginRequired.into());
            }
            bail!(
                "Aegis rejected OAuth renewal: {}",
                error["error"].as_str().unwrap_or("invalid response")
            );
        }
        let response = response.error_for_status()?;
        let token: Token = response.json().await?;
        config.access_token = token.access_token;
        config.refresh_token = token
            .refresh_token
            .context("server omitted rotated refresh token")?;
        config.expires_at = state::now() + token.expires_in;
        changed = true;
    }
    Ok(changed)
}

pub async fn catalog(config: &Config) -> Result<Connection> {
    let response = client()?
        .get(format!("{}/api/relay/connection", config.server))
        .bearer_auth(&config.access_token)
        .send()
        .await?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(LoginRequired.into());
    }
    let value: Connection = response.error_for_status()?.json().await?;
    if value.version != crate::relay_wire::PROTOCOL_VERSION {
        bail!("server requires a different AGS protocol version");
    }
    Ok(value)
}

pub async fn enroll(config: &Config) -> Result<()> {
    client()?
        .post(format!("{}/api/relay/enroll", config.server))
        .bearer_auth(&config.access_token)
        .json(&Enrollment {
            providers: config.providers.iter().map(|p| p.id.clone()).collect(),
        })
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

#[derive(Clone)]
pub enum TokenSource {
    Store {
        home: PathBuf,
        serial: Arc<tokio::sync::Mutex<()>>,
    },
    #[cfg(test)]
    Fixed(String),
}

impl TokenSource {
    pub fn new(home: &Path) -> Self {
        Self::Store {
            home: home.into(),
            serial: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
    pub async fn access_token(&self) -> Result<String> {
        match self {
            Self::Store { home, serial } => {
                let _guard = serial.lock().await;
                Ok(connected(home).await?.access_token)
            }
            #[cfg(test)]
            Self::Fixed(token) => Ok(token.clone()),
        }
    }
}

pub async fn disconnect(config: &Config) -> Result<()> {
    client()?
        .post(format!("{}/api/relay/disconnect", config.server))
        .bearer_auth(&config.access_token)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}
