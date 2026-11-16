//! The provider each CLI uses on this machine, kept next to the login.
use std::{collections::BTreeMap, fs, io, path::Path};

use anyhow::{Context, Result};

use crate::{Cli, store::write_private};

const FILE_NAME: &str = "defaults.json";

/// Provider IDs by CLI name.
pub(crate) fn load(dir: &Path) -> Result<BTreeMap<String, String>> {
    let path = dir.join(FILE_NAME);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_str(&text).with_context(|| format!("{} is not valid", path.display()))
}

pub(crate) fn set(dir: &Path, cli: Cli, provider: &str) -> Result<()> {
    let mut defaults = load(dir)?;
    defaults.insert(cli.to_string(), provider.to_owned());
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
        set(&dir, Cli::Claude, "aws").unwrap();
        set(&dir, Cli::Codex, "work").unwrap();
        set(&dir, Cli::Claude, "claude").unwrap();
        let defaults = load(&dir).unwrap();
        assert_eq!(defaults["claude"], "claude");
        assert_eq!(defaults["codex"], "work");
        assert_eq!(defaults.len(), 2);
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
}
