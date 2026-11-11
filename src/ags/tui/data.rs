//! What the setup screen shows, read from the AGS home: accounts, profiles, the local
//! connection, and where "here" is. Reloaded after every change.
use super::super::{
    account_cmd,
    account_store::{self, AccountStore},
    isolation::{self, SignIn},
    profile_cmd,
    profile_store::{Profile, ProfileStore},
    resolve, state, ui,
};
use anyhow::Result;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

/// The directory a "use here" rule applies to: the git root, else the current directory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Here {
    /// Canonical path, as stored in rules.
    pub key: String,
    /// For display: `~/Workspace/x`.
    pub display: String,
}

impl Here {
    pub fn current() -> Result<Self> {
        let root = resolve::here_root(&resolve::canonical_cwd()?);
        Ok(Self {
            key: root.to_string_lossy().into_owned(),
            display: ui::short_path(&resolve::show_path(&root)),
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct Data {
    pub accounts: AccountStore,
    pub profiles: ProfileStore,
    /// Sign-in state by account ID.
    pub signin: BTreeMap<String, SignIn>,
    /// Vendors whose native config directory exists on this machine.
    pub native_dirs: BTreeSet<String>,
    /// Routes granted to this machine: (provider ID, kind).
    pub granted: Vec<(String, String)>,
    /// The paired server, from the local connection.
    pub server: Option<String>,
    pub here: Here,
    /// The profile that applies here.
    pub active: Option<String>,
}

impl Data {
    pub fn load(home: &Path, here: &Here) -> Result<Self> {
        let accounts = AccountStore::load(home)?;
        let profiles = ProfileStore::load(home)?;
        let signin = accounts
            .accounts
            .iter()
            .map(|a| (a.id.clone(), isolation::signin(a, home)))
            .collect();
        let native_dirs = account_store::VENDORS
            .iter()
            .filter(|v| isolation::native_dir(v.2).is_some_and(|d| d.is_dir()))
            .map(|v| v.0.to_owned())
            .collect();
        let config = home
            .join("connection.json")
            .exists()
            .then(|| state::load(home).ok())
            .flatten();
        let active = profile_cmd::active_id(&profiles)?;
        Ok(Self {
            accounts,
            profiles,
            signin,
            native_dirs,
            granted: config.as_ref().map_or_else(Vec::new, |c| {
                c.providers
                    .iter()
                    .map(|p| (p.id.clone(), p.kind.clone()))
                    .collect()
            }),
            server: config.map(|c| c.server),
            here: here.clone(),
            active,
        })
    }

    pub fn paired(&self) -> bool {
        self.server.is_some()
    }

    pub fn signin(&self, id: &str) -> SignIn {
        self.signin.get(id).copied().unwrap_or(SignIn::Unknown)
    }

    /// Granted routes an account of `vendor` can be bound to.
    pub fn routes_for(&self, vendor: &str) -> Vec<String> {
        let kind = account_store::kind(vendor);
        self.granted
            .iter()
            .filter(|(_, k)| Some(k.as_str()) == kind)
            .map(|(id, _)| id.clone())
            .collect()
    }

    pub fn offer_native(&self, vendor: &str) -> bool {
        account_cmd::offer_native(&self.accounts, vendor, self.native_dirs.contains(vendor))
    }

    /// Names of the profiles using the account.
    pub fn used_by(&self, account: &str) -> Vec<String> {
        self.profiles
            .using(account)
            .iter()
            .map(|p| p.name.clone())
            .collect()
    }

    pub fn profile(&self, id: &str) -> Option<&Profile> {
        self.profiles.get(id)
    }

    /// Whether the profile has the rule for this directory.
    pub fn rule_here(&self, profile: &str) -> bool {
        self.profiles
            .rules
            .iter()
            .any(|r| r.path == self.here.key && r.profile == profile)
    }
}

/// The server to offer for a first pairing: the argument, the remembered one,
/// `AGS_SERVER_URL`, or the `server` file.
pub fn server_hint(home: &Path, argument: Option<&str>) -> String {
    argument
        .map(str::to_owned)
        .or_else(|| state::load(home).ok().map(|c| c.server))
        .or_else(|| std::env::var("AGS_SERVER_URL").ok())
        .or_else(|| std::fs::read_to_string(home.join("server")).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://".to_owned())
}

/// "Claude" or "ChatGPT".
pub fn product_label(vendor: &str) -> &'static str {
    match vendor {
        "anthropic" => "Claude",
        "openai" => "ChatGPT",
        _ => "?",
    }
}

/// What a native account uses: "your Claude Code login".
pub fn native_label(vendor: &str) -> &'static str {
    match vendor {
        "anthropic" => "your Claude Code login",
        "openai" => "your Codex login",
        _ => "this machine's login",
    }
}

/// A paired machine with three accounts and two profiles, for the screen's tests:
/// `personal-claude` (native), `work-claude` (not signed in), `personal-chatgpt` (signed in);
/// `personal` (the default) and `work` (the rule here, so active).
#[cfg(test)]
pub fn sample() -> Data {
    use super::super::{account_store::Account, profile_store::Rule};
    let account = |id: &str, name: &str, vendor: &str, native: bool| Account {
        id: id.into(),
        name: name.into(),
        vendor: vendor.into(),
        provider: if vendor == "anthropic" {
            "claude"
        } else {
            "codex"
        }
        .into(),
        native,
        created_at: 1,
        registered_with: None,
    };
    let profile = |id: &str, name: &str, accounts: &[(&str, &str)]| Profile {
        id: id.into(),
        name: name.into(),
        accounts: accounts
            .iter()
            .map(|(v, a)| ((*v).to_owned(), (*a).to_owned()))
            .collect(),
        created_at: 1,
        registered_with: None,
    };
    Data {
        accounts: AccountStore {
            accounts: vec![
                account("pc", "personal-claude", "anthropic", true),
                account("wc", "work-claude", "anthropic", false),
                account("pg", "personal-chatgpt", "openai", false),
            ],
        },
        profiles: ProfileStore {
            profiles: vec![
                profile("p", "personal", &[("anthropic", "pc"), ("openai", "pg")]),
                profile("w", "work", &[("anthropic", "wc")]),
            ],
            rules: vec![Rule {
                path: "/w/x".into(),
                profile: "w".into(),
            }],
            default: Some("p".into()),
        },
        signin: [
            ("pc".to_owned(), SignIn::Native),
            ("wc".to_owned(), SignIn::No),
            ("pg".to_owned(), SignIn::Yes),
        ]
        .into(),
        native_dirs: ["anthropic".to_owned(), "openai".to_owned()].into(),
        granted: vec![
            ("claude".into(), "claude_subscription".into()),
            ("codex".into(), "codex_subscription".into()),
        ],
        server: Some("https://aegis.example.com".into()),
        here: Here {
            key: "/w/x".into(),
            display: "~/x".into(),
        },
        active: Some("w".into()),
    }
}
