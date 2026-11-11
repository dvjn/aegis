//! Launch resolution: which profile and account a `claude` or `codex` launch uses.
use super::{
    account_store::{self, Account, AccountStore},
    profile_store::{Profile, ProfileStore},
};
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Flag,
    Env,
    Rule(String),
    Default,
}

impl Source {
    pub fn describe(&self, home: Option<&Path>) -> String {
        match self {
            Self::Flag => "--profile".into(),
            Self::Env => "AGS_PROFILE".into(),
            Self::Rule(path) => format!("rule {}", display_path(Path::new(path), home)),
            Self::Default => "default".into(),
        }
    }
}

/// `--profile`, then `AGS_PROFILE`, then the longest directory rule matching `cwd`, then the
/// global default. `cwd` must be canonical.
pub fn resolve_profile<'a>(
    store: &'a ProfileStore,
    flag: Option<&str>,
    env: Option<&str>,
    cwd: &Path,
) -> Result<Option<(&'a Profile, Source)>> {
    let named = |name: &str, source| {
        store.find(name).map(|p| (p, source)).map_err(|_| {
            anyhow::anyhow!(
                "no profile named {name}; create it with `ags profile new {name}` or see `ags profile list`"
            )
        })
    };
    if let Some(name) = flag.filter(|n| !n.is_empty()) {
        return named(name, Source::Flag).map(Some);
    }
    if let Some(name) = env.filter(|n| !n.is_empty()) {
        return named(name, Source::Env).map(Some);
    }
    if let Some(rule) = store.rule_for(cwd)
        && let Some(profile) = store.get(&rule.profile)
    {
        return Ok(Some((profile, Source::Rule(rule.path.clone()))));
    }
    Ok(store.default_profile().map(|p| (p, Source::Default)))
}

pub enum Launch<'a> {
    /// No profile applies: today's behavior.
    Unprofiled,
    Ready {
        profile: &'a Profile,
        account: &'a Account,
        source: Source,
    },
    /// The profile has no account for the vendor; only returned when a terminal can ask and
    /// there are accounts to offer.
    NeedsAccount {
        profile: &'a Profile,
        source: Source,
    },
}

pub fn missing_account_message(profile: &str, vendor: &str) -> String {
    let product = account_store::product(vendor);
    format!(
        "Profile {profile} has no {product} account. Add one with `ags account new <name> --{product}`, then `ags profile set {profile} {product} <name>`."
    )
}

#[allow(clippy::too_many_arguments)]
pub fn launch<'a>(
    profiles: &'a ProfileStore,
    accounts: &'a AccountStore,
    vendor: &str,
    provider_forced: bool,
    flag: Option<&str>,
    env: Option<&str>,
    cwd: &Path,
    tty: bool,
) -> Result<Launch<'a>> {
    if provider_forced {
        return Ok(Launch::Unprofiled);
    }
    let Some((profile, source)) = resolve_profile(profiles, flag, env, cwd)? else {
        return Ok(Launch::Unprofiled);
    };
    match profile.accounts.get(vendor).and_then(|id| accounts.get(id)) {
        Some(account) => Ok(Launch::Ready {
            profile,
            account,
            source,
        }),
        None if tty && !accounts.for_vendor(vendor).is_empty() => {
            Ok(Launch::NeedsAccount { profile, source })
        }
        None => bail!("{}", missing_account_message(&profile.name, vendor)),
    }
}

/// `~` for the home directory.
pub fn display_path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

pub fn home_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.canonicalize().unwrap_or(home))
}

pub fn show_path(path: &Path) -> String {
    display_path(path, home_dir().as_deref())
}

pub fn canonical_cwd() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    Ok(cwd.canonicalize().unwrap_or(cwd))
}

/// The git root containing `cwd`, else `cwd` itself; canonical.
pub fn here_root(cwd: &Path) -> PathBuf {
    std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| PathBuf::from(s.trim()))
        .and_then(|p| p.canonicalize().ok())
        .unwrap_or_else(|| cwd.to_owned())
}

#[cfg(test)]
mod tests;
