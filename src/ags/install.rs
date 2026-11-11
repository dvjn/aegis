use super::{profiles::CLIENTS, state};
use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::{Path, PathBuf},
};

const START: &str = "# >>> AGS managed PATH >>>";
const END: &str = "# <<< AGS managed PATH <<<";

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn shell_file() -> Result<(PathBuf, bool)> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is required")?);
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    if shell.ends_with("fish") {
        return Ok((home.join(".config/fish/conf.d/ags.fish"), true));
    }
    if shell.ends_with("zsh") {
        let base = std::env::var_os("ZDOTDIR")
            .map(PathBuf::from)
            .unwrap_or(home);
        return Ok((base.join(".zshrc"), false));
    }
    if shell.ends_with("bash") {
        return Ok((home.join(".bashrc"), false));
    }
    Ok((home.join(".profile"), false))
}

fn block(home: &Path, fish: bool) -> Result<String> {
    let shims = quote(
        home.join("shims")
            .to_str()
            .context("AGS path is not UTF-8")?,
    );
    let line = if fish {
        format!("set -gx PATH {shims} $PATH")
    } else {
        format!("export PATH={shims}:\"$PATH\"")
    };
    Ok(format!("{START}\n{line}\n{END}\n"))
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Journal {
    wrappers: std::collections::BTreeMap<String, String>,
    shells: Vec<ManagedShell>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ManagedShell {
    path: PathBuf,
    block: String,
}

fn read_optional(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

fn journal(home: &Path) -> Result<Journal> {
    let text = read_optional(&home.join("integration.json"))?;
    if text.is_empty() {
        return Ok(Journal::default());
    }
    Ok(serde_json::from_str(&text)?)
}

fn launcher() -> Result<String> {
    let executable = std::env::current_exe()?;
    // A version-manager install must keep following its active version rather
    // than pinning every vendor wrapper to a versioned installation path.
    if let Ok(output) = std::process::Command::new("mise")
        .args(["which", "ags"])
        .stderr(std::process::Stdio::null())
        .output()
        && output.status.success()
        && let Ok(path) = std::str::from_utf8(&output.stdout)
        && Path::new(path.trim()).canonicalize().ok() == Some(executable.canonicalize()?)
    {
        return Ok("mise exec -- ags".into());
    }
    Ok(quote(
        executable
            .to_str()
            .context("AGS executable path is not UTF-8")?,
    ))
}

/// Installs the wrappers; `announce` prints where they are.
pub fn wrappers(home: &Path, announce: bool) -> Result<()> {
    state::private_dir(&home.join("shims"))?;
    let mut owned = journal(home)?;
    let executable = launcher()?;
    let mut wrappers = std::collections::BTreeMap::new();
    for cli in CLIENTS.iter().copied().chain(std::iter::once("ags")) {
        let path = home.join("shims").join(cli);
        if path.symlink_metadata().is_ok() {
            let current = fs::read_to_string(&path)?;
            if owned.wrappers.get(cli) != Some(&current) {
                bail!(
                    "refusing to replace edited/unmanaged wrapper {}",
                    path.display()
                );
            }
        }
        let command = if cli == "ags" {
            format!("exec {executable} \"$@\"")
        } else {
            format!("exec {executable} run {cli} -- \"$@\"")
        };
        wrappers.insert(
            cli.to_owned(),
            format!("#!/bin/sh\n# Managed by AGS\n{command}\n"),
        );
    }
    let (file, fish) = shell_file()?;
    let current = read_optional(&file)?;
    let expected = block(home, fish)?;
    if current.contains(START) && !current.contains(&expected) {
        bail!(
            "{} contains an edited AGS PATH block; update it manually",
            file.display()
        );
    }
    if !owned.shells.iter().any(|s| s.path == file) {
        owned.shells.push(ManagedShell {
            path: file.clone(),
            block: expected.clone(),
        });
    }
    owned.wrappers = wrappers;
    // Record ownership before mutation, so interrupted setup can be retried.
    state::write_private(
        &home.join("integration.json"),
        &serde_json::to_vec_pretty(&owned)?,
        false,
    )?;
    for (cli, text) in &owned.wrappers {
        state::write_private(&home.join("shims").join(cli), text.as_bytes(), true)?;
    }
    if !current.contains(&expected) {
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&file, format!("{current}\n{expected}"))?;
    }
    if announce {
        eprintln!(
            "Wrappers installed. Start a new shell, or add {} to PATH.",
            home.join("shims").display()
        );
    }
    Ok(())
}

pub fn remove(home: &Path) -> Result<()> {
    let owned = journal(home)?;
    // Validate every owned artifact before removing anything.
    for (cli, expected) in &owned.wrappers {
        let path = home.join("shims").join(cli);
        if path.exists() && fs::read_to_string(&path)? != *expected {
            bail!("refusing to remove edited wrapper {}", path.display());
        }
    }
    for shell in &owned.shells {
        let current = read_optional(&shell.path)?;
        if current.contains(START) && !current.contains(&shell.block) {
            bail!(
                "{} contains an edited AGS PATH block; remove it manually",
                shell.path.display()
            );
        }
    }
    for cli in owned.wrappers.keys() {
        let path = home.join("shims").join(cli);
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    for shell in owned.shells {
        let current = read_optional(&shell.path)?;
        if current.contains(&shell.block) {
            fs::write(shell.path, current.replace(&shell.block, ""))?;
        }
    }
    let journal = home.join("integration.json");
    if journal.exists() {
        fs::remove_file(journal)?;
    }
    let server = home.join("server");
    if server.exists() {
        fs::remove_file(server)?;
    }
    if home.join(".installed").exists() {
        for name in ["ags", "ags.previous", "ags.next"] {
            let path = home.join("bin").join(name);
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
        fs::remove_file(home.join(".installed"))?;
    }
    Ok(())
}
