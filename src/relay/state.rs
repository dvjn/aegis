use crate::relay_wire::Provider;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Serialize, Deserialize)]
pub struct Config {
    pub server: String,
    pub client_id: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    #[serde(default, rename = "key", skip_serializing)]
    pub legacy_key: Option<String>,
    pub providers: Vec<Provider>,
    pub defaults: BTreeMap<String, String>,
    #[serde(default)]
    pub provider_defaults: BTreeMap<String, String>,
    #[serde(default)]
    pub cli_overrides: BTreeMap<String, BTreeMap<String, String>>,
}

impl Config {
    pub fn provider_for(&self, client: &str, kind: &str) -> Option<&str> {
        self.cli_overrides
            .get(client)
            .and_then(|m| m.get(kind))
            .or_else(|| self.provider_defaults.get(kind))
            .or_else(|| {
                self.defaults.get(client).filter(|id| {
                    self.providers
                        .iter()
                        .any(|p| &p.id == *id && p.kind == kind)
                })
            })
            .or_else(|| {
                self.providers
                    .iter()
                    .find(|p| p.kind == kind)
                    .map(|p| &p.id)
            })
            .map(String::as_str)
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn home() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("AGS_HOME") {
        return Ok(path.into());
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")))
        .context("HOME or AGS_HOME is required")?;
    Ok(base.join("ags"))
}

pub fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn write_private(path: &Path, bytes: &[u8], executable: bool) -> Result<()> {
    let parent = path.parent().context("missing parent directory")?;
    private_dir(parent)?;
    let temp = parent.join(format!(".ags-{}", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if executable { 0o700 } else { 0o600 });
    }
    let result = (|| -> Result<()> {
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if temp.exists() {
        let _ = fs::remove_file(temp);
    }
    result
}

pub fn lock(home: &Path) -> Result<File> {
    private_dir(home)?;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(home.join("state.lock"))?;
    file.lock()?;
    Ok(file)
}

pub fn load(home: &Path) -> Result<Config> {
    let path = home.join("connection.json");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::symlink_metadata(&path).context("not paired; run ags setup <server>")?;
        if !metadata.is_file() || metadata.mode() & 0o077 != 0 {
            bail!("connection.json must be a private regular file (chmod 600)");
        }
    }
    let config: Config =
        serde_json::from_slice(&fs::read(&path).context("not paired; run ags setup <server>")?)?;
    validate_server(&config.server)?;
    Ok(config)
}

pub fn save(home: &Path, config: &Config) -> Result<()> {
    write_private(
        &home.join("connection.json"),
        &serde_json::to_vec_pretty(config)?,
        false,
    )
}

pub fn validate_server(value: &str) -> Result<String> {
    let url = url::Url::parse(value).context("invalid Aegis server URL")?;
    let local = url.host_str().is_some_and(|h| {
        h == "localhost"
            || h.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if !(url.scheme() == "https" || url.scheme() == "http" && local)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !["", "/"].contains(&url.path())
    {
        bail!("server must be an HTTPS origin (HTTP is allowed on loopback only)");
    }
    Ok(url.origin().ascii_serialization())
}
