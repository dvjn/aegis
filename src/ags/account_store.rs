//! Local account registry: one vendor sign-in each, bound to one granted route.
use super::state;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::path::{Path, PathBuf};

/// (vendor, product name shown to users, CLI). `google` is reserved for agy and not offered.
pub const VENDORS: &[(&str, &str, &str)] = &[
    ("anthropic", "claude", "claude"),
    ("openai", "chatgpt", "codex"),
];

pub fn vendor_for_cli(cli: &str) -> Option<&'static str> {
    VENDORS.iter().find(|v| v.2 == cli).map(|v| v.0)
}

pub fn vendor_for_product(product: &str) -> Option<&'static str> {
    VENDORS.iter().find(|v| v.1 == product).map(|v| v.0)
}

pub fn product(vendor: &str) -> &str {
    VENDORS
        .iter()
        .find(|v| v.0 == vendor)
        .map_or(vendor, |v| v.1)
}

pub fn cli(vendor: &str) -> &str {
    VENDORS
        .iter()
        .find(|v| v.0 == vendor)
        .map_or(vendor, |v| v.2)
}

/// The route kind an account of this vendor must be bound to.
pub fn kind(vendor: &str) -> Option<&'static str> {
    crate::ags_wire::VENDORS
        .iter()
        .find(|v| v.0 == vendor)
        .map(|v| v.1)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub provider: String,
    /// Uses the vendor's existing config instead of a directory owned by AGS.
    #[serde(default)]
    pub native: bool,
    pub created_at: u64,
    /// Client ID this account was last registered under; reconnecting forces re-registration.
    #[serde(default)]
    pub registered_with: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AccountStore {
    #[serde(default)]
    pub accounts: Vec<Account>,
}

pub fn validate_name(kind: &str, name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        bail!("{kind} name must be 1-64 characters of A-Z a-z 0-9 . _ -");
    }
    Ok(())
}

pub fn dir(home: &Path, id: &str) -> PathBuf {
    home.join("accounts").join(id)
}

pub fn read_json<T: Default + DeserializeOwned>(path: &Path) -> Result<T> {
    match std::fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).with_context(|| format!("{} is invalid", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error.into()),
    }
}

impl AccountStore {
    pub fn load(home: &Path) -> Result<Self> {
        read_json(&home.join("accounts.json"))
    }

    pub fn save(&self, home: &Path) -> Result<()> {
        state::write_private(
            &home.join("accounts.json"),
            &serde_json::to_vec_pretty(self)?,
            false,
        )
    }

    /// Load, mutate, and save under the state lock.
    pub fn update<T>(home: &Path, change: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let _lock = state::lock(home)?;
        let mut store = Self::load(home)?;
        let value = change(&mut store)?;
        store.save(home)?;
        Ok(value)
    }

    pub fn find(&self, name: &str) -> Result<&Account> {
        self.accounts
            .iter()
            .find(|a| a.name == name)
            .with_context(|| format!("no account named {name}; see `ags account list`"))
    }

    pub fn get(&self, id: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.id == id)
    }

    pub fn for_vendor(&self, vendor: &str) -> Vec<&Account> {
        self.accounts
            .iter()
            .filter(|a| a.vendor == vendor)
            .collect()
    }

    pub fn native(&self, vendor: &str) -> Option<&Account> {
        self.accounts
            .iter()
            .find(|a| a.native && a.vendor == vendor)
    }

    /// `base`, else `base-2`, `base-3`, ...
    pub fn unique_name(&self, base: &str) -> String {
        let taken = |n: &str| self.accounts.iter().any(|a| a.name == n);
        if !taken(base) {
            return base.to_owned();
        }
        (2..)
            .map(|n| format!("{base}-{n}"))
            .find(|n| !taken(n))
            .expect("unbounded range")
    }

    pub fn check_new(&self, account: &Account) -> Result<()> {
        validate_name("account", &account.name)?;
        if self.accounts.iter().any(|a| a.name == account.name) {
            bail!("account {} already exists", account.name);
        }
        if account.native
            && let Some(existing) = self.native(&account.vendor)
        {
            bail!(
                "the native {} account already exists as {}",
                product(&account.vendor),
                existing.name
            );
        }
        if !VENDORS.iter().any(|v| v.0 == account.vendor) {
            bail!("accounts support claude and chatgpt");
        }
        Ok(())
    }

    pub fn add(&mut self, account: Account) -> Result<()> {
        self.check_new(&account)?;
        self.accounts.push(account);
        Ok(())
    }

    pub fn remove(&mut self, id: &str) -> Option<Account> {
        let index = self.accounts.iter().position(|a| a.id == id)?;
        Some(self.accounts.remove(index))
    }

    pub fn rename(&mut self, id: &str, name: &str) -> Result<()> {
        validate_name("account", name)?;
        if self.accounts.iter().any(|a| a.name == name && a.id != id) {
            bail!("account {name} already exists");
        }
        let account = self
            .accounts
            .iter_mut()
            .find(|a| a.id == id)
            .context("account disappeared")?;
        account.name = name.to_owned();
        // The server label follows the name; re-register on the next launch.
        account.registered_with = None;
        Ok(())
    }
}

#[cfg(test)]
pub mod tests;
