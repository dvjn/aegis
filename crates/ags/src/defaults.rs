//! The provider each CLI uses on this machine, kept next to the login.
use std::{collections::BTreeMap, fs, io, path::Path};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{Cli, store::write_private};

const FILE_NAME: &str = "defaults.json";

/// What ags remembers for one CLI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum Remembered {
    One(String),
    /// pi's provider for each provider type, by type.
    ByType(BTreeMap<String, String>),
}

/// What ags remembers, by CLI name.
pub(crate) fn load(dir: &Path) -> Result<BTreeMap<String, Remembered>> {
    let path = dir.join(FILE_NAME);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_str(&text).with_context(|| format!("{} is not valid", path.display()))
}

pub(crate) fn set(dir: &Path, cli: Cli, remembered: Remembered) -> Result<()> {
    let mut defaults = load(dir)?;
    defaults.insert(cli.to_string(), remembered);
    write_private(dir, FILE_NAME, &serde_json::to_vec_pretty(&defaults)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_default_is_kept_per_cli() {
        let dir = crate::store::tests::scratch().join("ags");
        assert!(load(&dir).unwrap().is_empty());
        let one = |id: &str| Remembered::One(id.to_owned());
        set(&dir, Cli::Claude, one("aws")).unwrap();
        set(&dir, Cli::Codex, one("work")).unwrap();
        set(&dir, Cli::Claude, one("claude")).unwrap();
        let by_type = Remembered::ByType(BTreeMap::from([("bedrock".into(), "aws".into())]));
        set(&dir, Cli::Pi, by_type.clone()).unwrap();
        let defaults = load(&dir).unwrap();
        assert_eq!(defaults["claude"], one("claude"));
        assert_eq!(defaults["codex"], one("work"));
        assert_eq!(defaults["pi"], by_type);
        assert_eq!(defaults.len(), 3);
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            1,
            "no staging file is left"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join(FILE_NAME))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn an_earlier_single_pi_default_still_reads() {
        let dir = crate::store::tests::scratch();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(FILE_NAME), r#"{"claude":"c","pi":"aws"}"#).unwrap();
        let defaults = load(&dir).unwrap();
        assert_eq!(defaults["pi"], Remembered::One("aws".into()));
        fs::remove_dir_all(dir).unwrap();
    }
}
