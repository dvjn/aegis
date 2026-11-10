use crate::relay_wire::Provider;
use anyhow::{Result, bail};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

pub const CLIENTS: &[&str] = &["claude", "codex", "pi"];

pub fn kind(client: &str) -> Option<&'static str> {
    match client {
        "claude" => Some("claude_subscription"),
        "codex" => Some("codex_subscription"),
        _ => None,
    }
}

pub fn compatible(client: &str, provider: &Provider) -> bool {
    match client {
        "pi" => matches!(
            provider.kind.as_str(),
            "claude_subscription" | "codex_subscription"
        ),
        _ => kind(client).is_some_and(|kind| provider.kind == kind),
    }
}

pub fn resolve(client: &str, home: &Path) -> Result<PathBuf> {
    if !CLIENTS.contains(&client) {
        bail!("unsupported CLI {client}; supported: claude, codex, pi");
    }
    let shims = home.join("shims");
    let own = std::env::current_exe()?.canonicalize()?;
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        if dir == shims
            || dir.canonicalize().ok() == shims.canonicalize().ok().filter(|_| shims.exists())
        {
            continue;
        }
        let file = dir.join(client);
        if !file.is_file() || file.canonicalize().ok() == Some(own.clone()) {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if std::fs::metadata(&file)?.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        use std::io::Read;
        let mut prefix = [0u8; 64];
        if std::fs::File::open(&file)
            .and_then(|mut f| f.read(&mut prefix))
            .is_ok()
            && prefix.starts_with(b"#!/bin/sh\n# Managed by AGS\n")
        {
            continue;
        }
        // Do not canonicalize version-manager shims; their own argv[0]/location can matter.
        return Ok(std::path::absolute(file)?);
    }
    bail!("{client} is not installed outside the AGS wrapper directory")
}

pub struct LaunchPlan {
    pub args: Vec<OsString>,
    pub env: Vec<(String, String)>,
    pub temporary: Option<PathBuf>,
}
impl Drop for LaunchPlan {
    fn drop(&mut self) {
        if let Some(path) = &self.temporary {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

pub fn plan(
    client: &str,
    provider: &Provider,
    endpoint: &str,
    args: Vec<OsString>,
    _home: &Path,
) -> Result<LaunchPlan> {
    if !compatible(client, provider) {
        bail!("provider {} is incompatible with {client}", provider.id);
    }
    let mut result = LaunchPlan {
        args,
        env: vec![],
        temporary: None,
    };
    match client {
        "claude" => result
            .env
            .push(("ANTHROPIC_BASE_URL".into(), endpoint.into())),
        "codex" => {
            let config = [
                "model_provider=\"ags\"".into(),
                "model_providers.ags.name=\"Aegis\"".into(),
                format!(
                    "model_providers.ags.base_url={}",
                    serde_json::to_string(endpoint)?
                ),
                "model_providers.ags.wire_api=\"responses\"".into(),
                "model_providers.ags.requires_openai_auth=true".into(),
                "model_providers.ags.supports_websockets=false".into(),
            ];
            let index = result
                .args
                .iter()
                .position(|a| a == "--")
                .unwrap_or(result.args.len());
            result.args.splice(
                index..index,
                config
                    .into_iter()
                    .flat_map(|c: String| [OsString::from("--config"), c.into()]),
            );
        }
        _ => bail!("unsupported CLI"),
    }
    Ok(result)
}
