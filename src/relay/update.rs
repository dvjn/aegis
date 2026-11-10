use super::{auth, state};
use crate::relay_wire::Release;
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

pub async fn update(home: &Path, rollback: bool) -> Result<()> {
    if !home.join(".installed").is_file()
        || std::env::current_exe()?.canonicalize()? != home.join("bin/ags").canonicalize()?
    {
        bail!("self-update requires a server-installed AGS executable");
    }
    let _lock = state::lock(home)?;
    let binary = home.join("bin/ags");
    let previous = home.join("bin/ags.previous");
    if rollback {
        let bytes = fs::read(&previous).context("no previous version is available")?;
        let current = fs::read(&binary)?;
        state::write_private(&binary, &bytes, true)?;
        state::write_private(&previous, &current, true)?;
        println!("Previous AGS executable restored.");
        return Ok(());
    }
    let config = state::load(home)?;
    let client = auth::client()?;
    let release: Release = client
        .get(format!("{}/.well-known/aegis-relay", config.server))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if release.protocol != crate::relay_wire::PROTOCOL_VERSION {
        bail!("server release uses an unsupported relay protocol");
    }
    if !release
        .version
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b".+-".contains(&b))
        || release.version.is_empty()
    {
        bail!("invalid release version");
    }
    let target = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-musl"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-musl"
    } else {
        bail!("self-update is not available for this platform");
    };
    if !release.targets.iter().any(|t| t == target) {
        bail!("server has no relay artifact for {target}");
    }
    let base = format!(
        "{}/downloads/ags/{}/{target}",
        config.server, release.version
    );
    let checksum = client
        .get(format!("{base}/ags.sha256"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let bytes = client
        .get(format!("{base}/ags"))
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    let digest: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if checksum.split_whitespace().next() != Some(digest.as_str()) {
        bail!("downloaded relay checksum mismatch");
    }
    let next = home.join("bin/ags.next");
    state::write_private(&next, &bytes, true)?;
    let output = tokio::process::Command::new(&next)
        .arg("--version")
        .output()
        .await?;
    if !output.status.success()
        || String::from_utf8_lossy(&output.stdout).trim() != format!("ags {}", release.version)
    {
        let _ = fs::remove_file(next);
        bail!("downloaded relay failed its version check");
    }
    state::write_private(&previous, &fs::read(&binary)?, true)?;
    fs::rename(next, binary)?;
    println!(
        "AGS {} installed. Existing CLI instances keep running.",
        release.version
    );
    Ok(())
}
