//! Credential isolation: managed accounts get their own vendor config dir that shares only
//! non-secret settings with the user's native config by symlink.
use super::account_store::{self, Account};
use anyhow::Result;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

const CLAUDE_SHARED: &[&str] = &[
    "settings.json",
    "CLAUDE.md",
    "skills",
    "agents",
    "commands",
    "plugins",
    "hooks",
    "statusline.py",
];
const CODEX_SHARED: &[&str] = &[
    "config.toml",
    "AGENTS.md",
    "prompts",
    "skills",
    "plugins",
    "hooks.json",
    "rules",
];

fn shared(cli: &str) -> &'static [&'static str] {
    match cli {
        "claude" => CLAUDE_SHARED,
        "codex" => CODEX_SHARED,
        _ => &[],
    }
}

/// The vendor's native config directory, honoring its own override variable.
pub fn native_dir(cli: &str) -> Option<PathBuf> {
    let (variable, fallback) = match cli {
        "claude" => ("CLAUDE_CONFIG_DIR", ".claude"),
        "codex" => ("CODEX_HOME", ".codex"),
        _ => return None,
    };
    std::env::var_os(variable)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(fallback)))
}

pub fn env(account: &Account, home: &Path) -> Result<Vec<(String, String)>> {
    if account.native {
        return Ok(vec![]);
    }
    let dir = account_store::dir(home, &account.id)
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("AGS path is not UTF-8"))?
        .to_owned();
    Ok(match account.vendor.as_str() {
        "anthropic" => vec![("CLAUDE_CONFIG_DIR".into(), dir)],
        "openai" => vec![("CODEX_HOME".into(), dir)],
        _ => vec![],
    })
}

pub fn extra_args(account: &Account) -> Vec<OsString> {
    if account.native || account.vendor != "openai" {
        return vec![];
    }
    vec![
        "--config".into(),
        "cli_auth_credentials_store=\"file\"".into(),
    ]
}

/// Inserts `extra` before any `--` so vendor options stay options.
pub fn splice(args: &mut Vec<OsString>, extra: Vec<OsString>) {
    let index = args.iter().position(|a| a == "--").unwrap_or(args.len());
    args.splice(index..index, extra);
}

pub fn link_shared(account: &Account, home: &Path) -> Result<()> {
    if account.native {
        return Ok(());
    }
    let cli = account_store::cli(&account.vendor);
    let Some(source) = native_dir(cli) else {
        return Ok(());
    };
    link_from(&source, &account_store::dir(home, &account.id), cli)
}

pub fn link_from(source: &Path, target: &Path, cli: &str) -> Result<()> {
    #[cfg(not(unix))]
    {
        let _ = (source, target, cli);
        anyhow::bail!("managed accounts require symlinks and are unsupported on this platform");
    }
    #[cfg(unix)]
    {
        if !source.is_dir() || source.canonicalize().ok() == target.canonicalize().ok() {
            return Ok(());
        }
        for item in shared(cli) {
            let from = source.join(item);
            let to = target.join(item);
            if from.symlink_metadata().is_err() || to.symlink_metadata().is_ok() {
                continue;
            }
            std::os::unix::fs::symlink(&from, &to)?;
        }
        Ok(())
    }
}

/// Shared items replaced by real local files or directories.
pub fn local_overrides(account: &Account, home: &Path) -> Vec<String> {
    let dir = account_store::dir(home, &account.id);
    shared(account_store::cli(&account.vendor))
        .iter()
        .filter(|item| {
            dir.join(item)
                .symlink_metadata()
                .is_ok_and(|m| !m.file_type().is_symlink())
        })
        .map(|s| (*s).to_owned())
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignIn {
    Yes,
    No,
    /// Cannot be told from files alone (Claude may keep credentials in the macOS keychain).
    Unknown,
    Native,
}

impl SignIn {
    pub fn label(self) -> &'static str {
        match self {
            Self::Yes => "signed in",
            Self::No => "not signed in",
            Self::Unknown => "unknown",
            Self::Native => "native",
        }
    }
}

/// Sign-in state from the account directory alone; never spawns the vendor process.
pub fn signin(account: &Account, home: &Path) -> SignIn {
    if account.native {
        return SignIn::Native;
    }
    let dir = account_store::dir(home, &account.id);
    let file = if account.vendor == "anthropic" {
        ".credentials.json"
    } else {
        "auth.json"
    };
    if dir.join(file).exists() {
        SignIn::Yes
    } else if cfg!(target_os = "macos") && account.vendor == "anthropic" {
        SignIn::Unknown
    } else {
        SignIn::No
    }
}

#[cfg(test)]
mod tests;
