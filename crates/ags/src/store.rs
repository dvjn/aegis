//! The paired server and its tokens, kept in a file only this user can read.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

/// An access token this close to expiry is refreshed before use, unless its lifetime is shorter than four times this.
const REFRESH_MARGIN_SECS: u64 = 60;
const FILE_NAME: &str = "login.json";
const LOCK_NAME: &str = "login.lock";

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Login {
    /// The server's OAuth issuer, which is also the base URL of its gateway.
    pub server: String,
    pub client_id: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Seconds since the Unix epoch.
    pub expires_at: u64,
    /// Seconds the access token was issued for.
    #[serde(default = "default_lifetime")]
    pub lifetime: u64,
}

fn default_lifetime() -> u64 {
    3600
}

impl Login {
    pub fn refresh_margin(&self) -> u64 {
        REFRESH_MARGIN_SECS.min(self.lifetime / 4)
    }

    pub fn expires_soon(&self) -> bool {
        now() + self.refresh_margin() >= self.expires_at
    }
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

pub struct Store {
    dir: PathBuf,
}

/// Held while tokens are refreshed, so two `ags` processes never spend one refresh token.
pub struct StoreLock {
    _file: File,
}

impl Store {
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// `ags` under the platform's configuration directory.
    pub fn default_location() -> Result<Self> {
        config_dir()
            .map(|dir| Self::at(dir.join("ags")))
            .ok_or_else(|| anyhow!("could not find a configuration directory for this user"))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(FILE_NAME)
    }

    pub fn load(&self) -> Result<Option<Login>> {
        let path = self.path();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        serde_json::from_str(&text)
            .map(Some)
            .with_context(|| format!("{} is not a valid login file", path.display()))
    }

    pub fn save(&self, login: &Login) -> Result<()> {
        write_private(&self.dir, FILE_NAME, &serde_json::to_vec_pretty(login)?).map(|_| ())
    }

    pub fn remove(&self) -> Result<()> {
        match fs::remove_file(self.path()) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                Err(error).with_context(|| format!("removing {}", self.path().display()))
            }
            _ => Ok(()),
        }
    }

    pub fn lock(&self) -> Result<StoreLock> {
        create_private_dir(&self.dir)?;
        let file = private_file(&self.dir.join(LOCK_NAME), Create::OrOpen, 0o600)?;
        file.lock().context("locking the login file")?;
        Ok(StoreLock { _file: file })
    }

    /// `lock` on a blocking thread, so waiting for another ags process stalls no async task.
    pub async fn lock_async(&self) -> Result<StoreLock> {
        let store = Self::at(&self.dir);
        tokio::task::spawn_blocking(move || store.lock())
            .await
            .context("locking the login file")?
    }
}

/// Replaces `dir/name` whole, so a reader never sees part of it.
pub(crate) fn write_private(dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf> {
    write_whole(dir, name, bytes, 0o600)
}

/// `write_private` for a program only this user can run.
pub(crate) fn write_executable(dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf> {
    write_whole(dir, name, bytes, 0o700)
}

fn write_whole(dir: &Path, name: &str, bytes: &[u8], mode: u32) -> Result<PathBuf> {
    create_private_dir(dir)?;
    let path = dir.join(name);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let staging = dir.join(format!("{name}.{}-{nanos}.tmp", std::process::id()));
    let written = (|| {
        let mut file = private_file(&staging, Create::New, mode)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&staging, &path).with_context(|| format!("writing {}", path.display()))
    })();
    if written.is_err() {
        let _ = fs::remove_file(&staging);
    } else {
        sync_dir(dir);
    }
    written.map(|()| path)
}

pub(crate) fn create_private_dir(dir: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}

/// Best effort: makes a rename into `dir` survive a crash, so a rotated refresh token is not lost.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(dir) = File::open(dir) {
        let _ = dir.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

enum Create {
    New,
    OrOpen,
}

fn private_file(path: &Path, create: Create, mode: u32) -> Result<File> {
    let mut options = OpenOptions::new();
    match create {
        Create::New => options.write(true).create_new(true),
        Create::OrOpen => options.write(true).create(true).truncate(false),
    };
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, mode);
    #[cfg(not(unix))]
    let _ = mode;
    options
        .open(path)
        .with_context(|| format!("opening {}", path.display()))
}

fn config_dir() -> Option<PathBuf> {
    let from = |name: &str| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    if cfg!(windows) {
        from("APPDATA")
    } else if cfg!(target_os = "macos") {
        from("HOME").map(|home| home.join("Library/Application Support"))
    } else {
        from("XDG_CONFIG_HOME").or_else(|| from("HOME").map(|home| home.join(".config")))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ags-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn login() -> Login {
        Login {
            server: "https://aegis.example".into(),
            client_id: "client".into(),
            access_token: "ag_at1.a.b".into(),
            refresh_token: Some("ag_rt1.a.b".into()),
            expires_at: now() + 3600,
            lifetime: 3600,
        }
    }

    #[test]
    fn a_saved_login_reads_back_and_only_its_user_can_read_it() {
        let root = scratch();
        let store = Store::at(root.join("ags"));
        assert!(store.load().unwrap().is_none());
        let saved = login();
        store.save(&saved).unwrap();
        store.save(&saved).unwrap();
        assert!(store.load().unwrap() == Some(saved));
        assert_eq!(
            fs::read_dir(root.join("ags")).unwrap().count(),
            1,
            "no staging file is left behind"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&store.path()), 0o600);
            assert_eq!(mode(&root.join("ags")), 0o700);
        }
        store.remove().unwrap();
        store.remove().unwrap();
        assert!(store.load().unwrap().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_token_is_refreshed_shortly_before_it_expires() {
        let mut login = login();
        assert!(!login.expires_soon());
        login.expires_at = now() + REFRESH_MARGIN_SECS;
        assert!(login.expires_soon());
    }

    #[test]
    fn a_short_lived_token_is_refreshed_only_in_its_last_quarter() {
        let mut login = login();
        login.lifetime = 60;
        login.expires_at = now() + 40;
        assert!(!login.expires_soon());
        login.expires_at = now() + 15;
        assert!(login.expires_soon());
        assert_eq!(login.refresh_margin(), 15);
    }
}
