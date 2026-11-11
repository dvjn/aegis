//! One-time migration from the single-CLI `profiles.json` (entries carry a `cli` field) to
//! `accounts.json` plus the accounts-and-profiles `profiles.json`.
use super::{
    account_store::{self, Account, AccountStore},
    profile_store::{Profile, ProfileStore},
    state,
};
use anyhow::Result;
use serde::Deserialize;
use std::{collections::BTreeMap, fs, path::Path};

#[derive(Deserialize)]
struct OldStore {
    #[serde(default)]
    profiles: Vec<OldProfile>,
    #[serde(default)]
    defaults: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct OldProfile {
    id: String,
    name: String,
    cli: String,
    provider: String,
    #[serde(default)]
    adopted: bool,
    #[serde(default)]
    created_at: u64,
    #[serde(default)]
    registered_with: Option<String>,
}

/// The old format has a `defaults` map or profiles with a `cli` field; the new one has neither.
fn parse_old(bytes: &[u8]) -> Option<OldStore> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let old = value.get("defaults").is_some()
        || value
            .get("profiles")
            .and_then(|p| p.as_array())
            .is_some_and(|p| p.iter().any(|p| p.get("cli").is_some()));
    old.then(|| serde_json::from_value(value).ok()).flatten()
}

fn convert(old: OldStore) -> (AccountStore, ProfileStore) {
    let mut accounts = AccountStore::default();
    let mut profiles = ProfileStore::default();
    // Old profile name -> new profile index; old profile ID -> new account ID.
    let mut by_name: BTreeMap<String, usize> = BTreeMap::new();
    let mut account_of: BTreeMap<String, String> = BTreeMap::new();
    for p in &old.profiles {
        let Some(vendor) = account_store::vendor_for_cli(&p.cli) else {
            continue;
        };
        let name = accounts.unique_name(&format!("{}-{}", p.name, account_store::product(vendor)));
        accounts.accounts.push(Account {
            id: p.id.clone(),
            name,
            vendor: vendor.to_owned(),
            provider: p.provider.clone(),
            native: p.adopted,
            created_at: p.created_at,
            registered_with: p.registered_with.clone(),
        });
        account_of.insert(p.id.clone(), p.id.clone());
        let index = *by_name.entry(p.name.clone()).or_insert_with(|| {
            let mut profile = Profile::new(&p.name);
            profile.created_at = p.created_at;
            profiles.profiles.push(profile);
            profiles.profiles.len() - 1
        });
        profiles.profiles[index]
            .accounts
            .insert(vendor.to_owned(), p.id.clone());
    }
    let profile_of = |id: &String| {
        old.profiles
            .iter()
            .find(|p| &p.id == id)
            .and_then(|p| by_name.get(&p.name))
            .copied()
    };
    let claude = old.defaults.get("claude").and_then(profile_of);
    let codex = old.defaults.get("codex").and_then(profile_of);
    if let Some(index) = claude.or(codex) {
        if let Some(codex_id) = old.defaults.get("codex")
            && account_of.contains_key(codex_id)
        {
            profiles.profiles[index]
                .accounts
                .entry("openai".to_owned())
                .or_insert_with(|| codex_id.clone());
        }
        profiles.default = Some(profiles.profiles[index].id.clone());
    }
    (accounts, profiles)
}

/// Migrates in place when `profiles.json` is in the old format. Returns whether it migrated.
pub fn run(home: &Path) -> Result<bool> {
    let path = home.join("profiles.json");
    if !path.exists() {
        return Ok(false);
    }
    let _lock = state::lock(home)?;
    let bytes = fs::read(&path)?;
    let Some(old) = parse_old(&bytes) else {
        return Ok(false);
    };
    let ids: Vec<(String, bool)> = old
        .profiles
        .iter()
        .map(|p| (p.id.clone(), p.adopted))
        .collect();
    let (accounts, profiles) = convert(old);
    // Order keeps a crash recoverable: the old file stays authoritative until the last step.
    accounts.save(home)?;
    for (id, adopted) in ids {
        let from = home.join("profiles").join(&id);
        let to = account_store::dir(home, &id);
        if !adopted && from.exists() && !to.exists() {
            state::private_dir(&home.join("accounts"))?;
            fs::rename(&from, &to)?;
        }
    }
    state::write_private(&home.join("profiles.v2.json.bak"), &bytes, false)?;
    profiles.save(home)?;
    Ok(true)
}

#[cfg(test)]
mod tests;
