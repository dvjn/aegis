//! Local profile registry: named contexts mapping each vendor to at most one account,
//! plus directory rules and the global default.
use super::{
    account_store::{self, AccountStore, read_json},
    state,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Profile {
    pub id: String,
    pub name: String,
    /// Vendor to account ID.
    #[serde(default)]
    pub accounts: BTreeMap<String, String>,
    pub created_at: u64,
    #[serde(default)]
    pub registered_with: Option<String>,
}

/// Directory rule: `path` is canonical and absolute; `profile` is a profile ID.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Rule {
    pub path: String,
    pub profile: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ProfileStore {
    #[serde(default)]
    pub profiles: Vec<Profile>,
    #[serde(default)]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub default: Option<String>,
}

pub fn validate_name(name: &str) -> Result<()> {
    account_store::validate_name("profile", name)
}

impl Profile {
    pub fn new(name: &str) -> Self {
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            name: name.to_owned(),
            accounts: BTreeMap::new(),
            created_at: state::now(),
            registered_with: None,
        }
    }
}

impl ProfileStore {
    pub fn load(home: &Path) -> Result<Self> {
        read_json(&home.join("profiles.json"))
    }

    pub fn save(&self, home: &Path) -> Result<()> {
        state::write_private(
            &home.join("profiles.json"),
            &serde_json::to_vec_pretty(self)?,
            false,
        )
    }

    pub fn update<T>(home: &Path, change: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let _lock = state::lock(home)?;
        let mut store = Self::load(home)?;
        let value = change(&mut store)?;
        store.save(home)?;
        Ok(value)
    }

    /// Mutate both stores under one lock; each is written only if the closure succeeds.
    pub fn update_both<T>(
        home: &Path,
        change: impl FnOnce(&mut AccountStore, &mut Self) -> Result<T>,
    ) -> Result<T> {
        let _lock = state::lock(home)?;
        let mut accounts = AccountStore::load(home)?;
        let mut profiles = Self::load(home)?;
        let value = change(&mut accounts, &mut profiles)?;
        accounts.save(home)?;
        profiles.save(home)?;
        Ok(value)
    }

    pub fn find(&self, name: &str) -> Result<&Profile> {
        self.profiles
            .iter()
            .find(|p| p.name == name)
            .with_context(|| format!("no profile named {name}; see `ags profile list`"))
    }

    pub fn get(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    pub fn default_profile(&self) -> Option<&Profile> {
        self.get(self.default.as_deref()?)
    }

    pub fn check_new(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        if self.profiles.iter().any(|p| p.name == name) {
            bail!("profile {name} already exists");
        }
        Ok(())
    }

    pub fn add(&mut self, profile: Profile) -> Result<()> {
        self.check_new(&profile.name)?;
        self.profiles.push(profile);
        Ok(())
    }

    /// Removes the profile, its rules, and the default if it was one.
    pub fn remove(&mut self, id: &str) -> Option<Profile> {
        let index = self.profiles.iter().position(|p| p.id == id)?;
        let profile = self.profiles.remove(index);
        self.rules.retain(|r| r.profile != id);
        if self.default.as_deref() == Some(id) {
            self.default = None;
        }
        Some(profile)
    }

    pub fn rename(&mut self, id: &str, name: &str) -> Result<()> {
        validate_name(name)?;
        if self.profiles.iter().any(|p| p.name == name && p.id != id) {
            bail!("profile {name} already exists");
        }
        let profile = self
            .profiles
            .iter_mut()
            .find(|p| p.id == id)
            .context("profile disappeared")?;
        profile.name = name.to_owned();
        profile.registered_with = None;
        Ok(())
    }

    pub fn set_account(&mut self, id: &str, vendor: &str, account: Option<&str>) -> Result<()> {
        let profile = self
            .profiles
            .iter_mut()
            .find(|p| p.id == id)
            .context("profile disappeared")?;
        match account {
            Some(account) => profile
                .accounts
                .insert(vendor.to_owned(), account.to_owned()),
            None => profile.accounts.remove(vendor),
        };
        Ok(())
    }

    pub fn set_rule(&mut self, path: &str, profile: &str) {
        match self.rules.iter_mut().find(|r| r.path == path) {
            Some(rule) => rule.profile = profile.to_owned(),
            None => self.rules.push(Rule {
                path: path.to_owned(),
                profile: profile.to_owned(),
            }),
        }
    }

    pub fn clear_rule(&mut self, path: &str) -> bool {
        let before = self.rules.len();
        self.rules.retain(|r| r.path != path);
        self.rules.len() != before
    }

    /// Profiles that use the account.
    pub fn using(&self, account: &str) -> Vec<&Profile> {
        self.profiles
            .iter()
            .filter(|p| p.accounts.values().any(|a| a == account))
            .collect()
    }

    /// Removes the account from every profile; returns the names of those that used it.
    pub fn detach(&mut self, account: &str) -> Vec<String> {
        let mut names = vec![];
        for profile in &mut self.profiles {
            let before = profile.accounts.len();
            profile.accounts.retain(|_, a| a != account);
            if profile.accounts.len() != before {
                names.push(profile.name.clone());
            }
        }
        names
    }

    /// The rule for the longest directory that contains `cwd`, compared by path component.
    pub fn rule_for(&self, cwd: &Path) -> Option<&Rule> {
        self.rules
            .iter()
            .filter(|r| cwd.starts_with(&r.path))
            .max_by_key(|r| Path::new(&r.path).components().count())
    }
}

#[cfg(test)]
mod tests;
