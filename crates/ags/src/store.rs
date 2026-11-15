//! The paired server and its tokens, kept in a file only this user can read.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

/// An access token this close to expiry is refreshed before use.
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
}

impl Login {
    pub fn expires_soon(&self) -> bool {
        now() + REFRESH_MARGIN_SECS >= self.expires_at
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
        create_private_dir(&self.dir)?;
        let path = self.path();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let staging = self
            .dir
            .join(format!("{FILE_NAME}.{}-{nanos}.tmp", std::process::id()));
        let written = (|| {
            let mut file = private_file(&staging, Create::New)?;
            file.write_all(&serde_json::to_vec_pretty(login)?)?;
            file.sync_all()?;
            fs::rename(&staging, &path).with_context(|| format!("writing {}", path.display()))
        })();
        if written.is_err() {
            let _ = fs::remove_file(&staging);
        }
        written
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
        let file = private_file(&self.dir.join(LOCK_NAME), Create::OrOpen)?;
        file.lock().context("locking the login file")?;
        Ok(StoreLock { _file: file })
    }
}

fn create_private_dir(dir: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}

enum Create {
    New,
    OrOpen,
}

fn private_file(path: &Path, create: Create) -> Result<File> {
    let mut options = OpenOptions::new();
    match create {
        Create::New => options.write(true).create_new(true),
        Create::OrOpen => options.write(true).create(true).truncate(false),
    };
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
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
        }
    }

    #[test]
    fn a_saved_login_reads_back_and_only_its_user_can_read_it() {
        let root = scratch();
        let store = Store::at(root.join("ags"));
        assert!(store.load().unwrap().is_none());
        store.save(&login()).unwrap();
        store.save(&login()).unwrap();
        assert!(store.load().unwrap() == Some(login()));
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
}
