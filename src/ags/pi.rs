//! Per-launch pi extension; the user's model, auth, package and subagent state stays native.
use super::{
    profiles::LaunchPlan,
    state::{self, Config},
    wizard::KINDS,
};
use anyhow::{Context, Result, bail};
use serde_json::json;
use std::{collections::BTreeMap, ffi::OsString, path::Path};

/// pi appends `/codex/responses` to an `openai-codex` base URL, so it gets the provider's
/// ChatGPT path (`/backend-api/codex`), which Aegis maps onto the upstream as-is.
fn pi_endpoint(source: &str, relay: &str) -> String {
    match source {
        "openai-codex" => format!("{relay}/backend-api/codex"),
        _ => relay.to_owned(),
    }
}

pub fn plan(
    config: &Config,
    endpoints: &BTreeMap<String, String>,
    selected: Option<&str>,
    args: Vec<OsString>,
    home: &Path,
) -> Result<LaunchPlan> {
    if endpoints.is_empty() {
        bail!("no providers enabled for pi; run ags configure");
    }
    let mut defaults = BTreeMap::new();
    for &(kind, _, source) in KINDS {
        if !config
            .providers
            .iter()
            .any(|p| p.kind == kind && super::profiles::compatible("pi", p))
        {
            continue;
        }
        if let Some(id) = config.provider_for("pi", kind) {
            defaults.insert(source.to_owned(), id.to_owned());
        }
    }
    if let Some(id) = selected {
        let provider = config
            .providers
            .iter()
            .find(|p| p.id == id)
            .context("provider is not enabled; run ags configure")?;
        let source = KINDS
            .iter()
            .find(|k| k.0 == provider.kind)
            .context("provider is incompatible with pi")?
            .2;
        defaults.insert(source.to_owned(), id.to_owned());
    }
    let routes: Vec<_> = config.providers.iter().filter_map(|p| {
        let source = KINDS.iter().find(|k| k.0 == p.kind)?.2;
        let endpoint = pi_endpoint(source, endpoints.get(&p.id)?);
        Some(json!({"id":p.id,"source":source,"alias":format!("ags-{}",p.id),"endpoint":endpoint}))
    }).collect();
    let data = json!({"server":config.server,"routes":routes,"defaults":defaults});
    let path = home
        .join("sessions")
        .join(uuid::Uuid::new_v4().simple().to_string());
    state::private_dir(&path)?;
    let mut result = LaunchPlan {
        args,
        env: vec![],
        temporary: Some(path.clone()),
    };
    let extension = include_str!("pi.ts").replace("@CONFIG@", &serde_json::to_string(&data)?);
    let file = path.join("ags-relay.ts");
    state::write_private(&file, extension.as_bytes(), false)?;
    let index = result
        .args
        .iter()
        .position(|a| a == "--")
        .unwrap_or(result.args.len());
    result.args.splice(
        index..index,
        [OsString::from("--extension"), file.into_os_string()],
    );
    Ok(result)
}
