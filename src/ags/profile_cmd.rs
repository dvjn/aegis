//! `ags profile ...` and the profile side of `ags run`.
use super::{
    account_cmd::{self, tty},
    account_store::{self, Account, AccountStore, VENDORS},
    auth, isolation,
    isolation::SignIn,
    profile_store::{Profile, ProfileStore},
    prompt::Ask,
    relay,
    resolve::{self, Launch, show_path},
    state::{self, Config},
    ui,
};
use crate::ags_wire::{AccountRegistration, ProfileLabel};
use anyhow::{Context, Result, bail};
use clap::Subcommand;
use demand::{Confirm, DemandOption, Input, Select};
use std::{io::IsTerminal, path::Path};

#[derive(Subcommand)]
pub enum ProfileCommand {
    /// Create a profile that uses existing accounts (see `ags account new`).
    #[command(visible_aliases = ["n", "c", "add", "create"])]
    New {
        name: Option<String>,
        /// claude account: an existing account name, or `none`.
        #[arg(long, value_name = "ACCOUNT")]
        claude: Option<String>,
        /// chatgpt account: an existing account name, or `none`.
        #[arg(long, value_name = "ACCOUNT")]
        chatgpt: Option<String>,
        /// Make it the global default.
        #[arg(long = "default")]
        make_default: bool,
        /// Use it for this directory (the git root, or the current directory).
        #[arg(long)]
        here: bool,
    },
    /// List profiles with their accounts, the default, and directory rules
    #[command(visible_aliases = ["ls"])]
    List,
    /// Set the global default profile, or with --here a rule for this directory.
    #[command(visible_aliases = ["u"])]
    Use {
        name: Option<String>,
        #[arg(long)]
        here: bool,
        /// Remove the default, or with --here this directory's rule.
        #[arg(long, conflicts_with = "name")]
        clear: bool,
    },
    /// Choose the account a profile uses for a product.
    #[command(visible_aliases = ["s"])]
    Set {
        profile: String,
        #[arg(value_parser = ["claude", "chatgpt"])]
        product: String,
        /// An existing account name, or `none`.
        value: String,
    },
    /// Rename a profile
    #[command(visible_aliases = ["mv"])]
    Rename { name: String, new_name: String },
    /// Remove a profile and its rules
    #[command(visible_aliases = ["remove"])]
    Rm { name: String },
}

pub async fn dispatch(home: &Path, command: Option<ProfileCommand>) -> Result<i32> {
    match command {
        None => show(home)?,
        Some(ProfileCommand::New {
            name,
            claude,
            chatgpt,
            make_default,
            here,
        }) => new(home, name, [claude, chatgpt], make_default, here).await?,
        Some(ProfileCommand::List) => list(home)?,
        Some(ProfileCommand::Use { name, here, clear }) => use_profile(home, name, here, clear)?,
        Some(ProfileCommand::Set {
            profile,
            product,
            value,
        }) => set(home, &profile, &product, &value)?,
        Some(ProfileCommand::Rename { name, new_name }) => {
            rename(home, &name, &new_name)?;
            println!("Renamed profile {name} to {new_name}.");
        }
        Some(ProfileCommand::Rm { name }) => {
            remove(home, &name).await?;
            println!("Removed profile {name}.");
        }
    }
    Ok(0)
}

pub enum Picked {
    Account(Account),
    Skip,
    /// The product has no accounts yet; a hint was printed.
    Unavailable,
}

pub fn no_accounts_hint(product: &str) -> String {
    format!("No {product} accounts yet; add one with `ags account new <name> --{product}`.")
}

/// The preselected choice: the current account if it is among the options, else the opt-out.
pub fn initial_choice(accounts: &[&Account], current: Option<&str>) -> String {
    current
        .filter(|id| accounts.iter().any(|a| a.id == *id))
        .map_or_else(|| "skip".to_owned(), str::to_owned)
}

/// Ask which existing account `label` should use for `vendor`; `skip` labels the opt-out.
async fn pick_account(
    home: &Path,
    label: &str,
    vendor: &str,
    current: Option<&str>,
    skip: &str,
) -> Result<Picked> {
    let product = account_store::product(vendor);
    let store = AccountStore::load(home)?;
    let accounts = store.for_vendor(vendor);
    if accounts.is_empty() {
        println!("{}", no_accounts_hint(product));
        return Ok(Picked::Unavailable);
    }
    let initial = initial_choice(&accounts, current);
    let mut prompt = Select::new(format!("{label}: which {product} account?"));
    for a in &accounts {
        let hint = format!("{}, {}", a.provider, isolation::signin(a, home).label());
        prompt = prompt.option(
            DemandOption::new(a.id.clone())
                .label(&a.name)
                .description(&hint)
                .selected(a.id == initial),
        );
    }
    let chosen = prompt
        .option(
            DemandOption::new("skip".to_owned())
                .label(skip)
                .selected(initial == "skip"),
        )
        .ask()?;
    Ok(match store.get(&chosen) {
        Some(account) => Picked::Account(account.clone()),
        None => Picked::Skip,
    })
}

/// Resolves a `new`/`set` value to an existing account; `None` means no account.
fn account_for_value(
    accounts: &AccountStore,
    profile: &str,
    vendor: &str,
    value: &str,
) -> Result<Option<Account>> {
    let product = account_store::product(vendor);
    if value == "none" {
        return Ok(None);
    }
    if value == "new" || value == "native" {
        bail!(
            "`{value}` is no longer accepted here; create the account first with `ags account new <name> --{product}{}`, then `ags profile set {profile} {product} <name>`",
            if value == "native" { " --native" } else { "" }
        );
    }
    let account = accounts.find(value)?;
    if account.vendor != vendor {
        bail!(
            "account {value} is a {} account, not {product}; see `ags account list`",
            account_store::product(&account.vendor)
        );
    }
    Ok(Some(account.clone()))
}

pub async fn new(
    home: &Path,
    name: Option<String>,
    values: [Option<String>; 2],
    make_default: bool,
    here: bool,
) -> Result<()> {
    let interactive = tty();
    let name = match name {
        Some(name) => name,
        None if interactive => Input::new("Profile name")
            .validator(|v: &str| super::profile_store::validate_name(v).map_err(|e| e.to_string()))
            .ask()?,
        None => bail!("a profile name is required: `ags profile new <name>`"),
    };
    // Check everything the flags name before creating anything.
    ProfileStore::load(home)?.check_new(&name)?;
    let accounts = AccountStore::load(home)?;
    let mut chosen = Vec::new();
    for (&(vendor, _, _), value) in VENDORS.iter().zip(&values) {
        chosen.push(match value {
            Some(value) => Some(account_for_value(&accounts, &name, vendor, value)?),
            None => None,
        });
    }
    // Ask for everything before saving, so cancelling leaves no half-made profile.
    let mut picked = Vec::new();
    for (&(vendor, _, _), value) in VENDORS.iter().zip(chosen) {
        picked.push(match value {
            Some(account) => account,
            None if interactive => match pick_account(home, &name, vendor, None, "None").await? {
                Picked::Account(a) => Some(a),
                Picked::Skip | Picked::Unavailable => None,
            },
            None => None,
        });
    }
    let (mut make_default, mut here) = (make_default, here);
    if interactive && !make_default && !here {
        match ask_where(&name)? {
            Where::Default => make_default = true,
            Where::Here => here = true,
            _ => {}
        }
    }
    let profile = Profile::new(&name);
    ProfileStore::update(home, |s| s.add(profile.clone()))?;
    println!("Created profile {name}.");
    for (&(vendor, product, _), account) in VENDORS.iter().zip(picked) {
        match account {
            Some(account) => {
                ProfileStore::update(home, |s| {
                    s.set_account(&profile.id, vendor, Some(&account.id))
                })?;
                println!("  {product}: {}", account.name);
            }
            None if !interactive => {
                println!("  {product}: none (`ags profile set {name} {product} <account>`)")
            }
            None => {}
        }
    }
    if make_default || here {
        use_profile(home, Some(name), here, false)?;
        if make_default && here {
            use_profile_default(home, &profile.id)?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Where {
    Default,
    Here,
    Neither,
}

/// Options of the "Where should it apply?" question.
pub fn where_items(here: &str) -> Vec<(Where, String)> {
    vec![
        (Where::Default, "Everywhere (global default)".to_owned()),
        (Where::Here, format!("This directory ({here})")),
        (Where::Neither, "Only with -P".to_owned()),
    ]
}

/// Ask where a new profile `name` should apply.
fn ask_where(name: &str) -> Result<Where> {
    let root = resolve::here_root(&resolve::canonical_cwd()?);
    let mut prompt = Select::new(format!("Where should {name} apply?"));
    for (value, label) in where_items(&show_path(&root)) {
        prompt = prompt.option(DemandOption::with_label(label, value));
    }
    Ok(prompt.ask()?)
}

pub fn rename(home: &Path, name: &str, new_name: &str) -> Result<()> {
    ProfileStore::update(home, |s| {
        let id = s.find(name)?.id.clone();
        s.rename(&id, new_name)
    })
}

fn use_profile_default(home: &Path, id: &str) -> Result<()> {
    ProfileStore::update(home, |s| {
        s.default = Some(id.to_owned());
        Ok(())
    })
}

fn use_profile(home: &Path, name: Option<String>, here: bool, clear: bool) -> Result<()> {
    let root = resolve::here_root(&resolve::canonical_cwd()?);
    let key = root.to_string_lossy().into_owned();
    if clear {
        let removed = ProfileStore::update(home, |s| {
            Ok(if here {
                s.clear_rule(&key)
            } else {
                s.default.take().is_some()
            })
        })?;
        match (here, removed) {
            (true, true) => println!("Removed the rule for {}.", show_path(&root)),
            (true, false) => println!("No rule for {}.", show_path(&root)),
            (false, true) => println!("Removed the default profile."),
            (false, false) => println!("No default profile is set."),
        }
        return Ok(());
    }
    let store = ProfileStore::load(home)?;
    let name = match name {
        Some(name) => name,
        None if tty() && !store.profiles.is_empty() => {
            let mut prompt = Select::new("Which profile?");
            for p in &store.profiles {
                prompt = prompt.option(DemandOption::new(p.name.clone()).label(&p.name));
            }
            prompt.ask()?
        }
        None => bail!("name a profile: `ags profile use <name>` (see `ags profile list`)"),
    };
    ProfileStore::update(home, |s| {
        let id = s.find(&name)?.id.clone();
        if here {
            s.set_rule(&key, &id);
        } else {
            s.default = Some(id);
        }
        Ok(())
    })?;
    if here {
        println!("{name} now applies under {}.", show_path(&root));
    } else {
        println!("{name} is now the default profile.");
    }
    Ok(())
}

fn set(home: &Path, profile: &str, product: &str, value: &str) -> Result<()> {
    let id = ProfileStore::load(home)?.find(profile)?.id.clone();
    let vendor = account_store::vendor_for_product(product).context("unknown product")?;
    let account = account_for_value(&AccountStore::load(home)?, profile, vendor, value)?;
    ProfileStore::update(home, |s| {
        s.set_account(&id, vendor, account.as_ref().map(|a| a.id.as_str()))
    })?;
    match account {
        Some(a) => println!("{profile} now uses {} for {product}.", a.name),
        None => println!("{profile} no longer has a {product} account."),
    }
    Ok(())
}

fn list(home: &Path) -> Result<()> {
    match render(home, ui::Out::Stdout)? {
        Some(tree) => print!("{tree}"),
        None => println!(
            "No profiles. Create one with `ags profile new <name>`, or run `ags setup` for a guided setup."
        ),
    }
    Ok(())
}

/// Notes shown beside a profile: where it is active, the default, and its directory rules.
pub fn notes(store: &ProfileStore, p: &Profile, active: Option<&str>) -> Vec<String> {
    let mut notes = vec![];
    if active == Some(p.id.as_str()) {
        notes.push("active here".to_owned());
    }
    if store.default.as_deref() == Some(&p.id) {
        notes.push("default".to_owned());
    }
    notes.extend(
        store
            .rules
            .iter()
            .filter(|r| r.profile == p.id)
            .map(|r| format!("rule {}", show_path(Path::new(&r.path)))),
    );
    notes
}

/// The profile that applies in the current directory.
pub fn active_id(store: &ProfileStore) -> Result<Option<String>> {
    let cwd = resolve::canonical_cwd()?;
    let env = std::env::var("AGS_PROFILE").ok();
    Ok(resolve::resolve_profile(store, None, env.as_deref(), &cwd)
        .ok()
        .flatten()
        .map(|(p, _)| p.id.clone()))
}

/// The profile tree, or `None` when there are no profiles.
pub fn render(home: &Path, to: ui::Out) -> Result<Option<String>> {
    let profiles = ProfileStore::load(home)?;
    if profiles.profiles.is_empty() {
        return Ok(None);
    }
    let accounts = AccountStore::load(home)?;
    let active = active_id(&profiles)?;
    Ok(Some(tree(
        &profiles.profiles,
        &accounts,
        |p| notes(&profiles, p, active.as_deref()),
        to,
    )))
}

/// Profiles as a tree: one branch per product that has an account.
fn tree(
    profiles: &[Profile],
    accounts: &AccountStore,
    notes: impl Fn(&Profile) -> Vec<String>,
    to: ui::Out,
) -> String {
    let mut out = String::new();
    for profile in profiles {
        let notes: Vec<_> = notes(profile)
            .into_iter()
            .map(|note| match note.as_str() {
                "active here" => ui::ok(to, note).to_string(),
                "default" => ui::accent(to, note).to_string(),
                _ => match note.strip_prefix("rule ") {
                    Some(path) => {
                        ui::muted(to, format!("rule {}", ui::short_path(path))).to_string()
                    }
                    None => note,
                },
            })
            .collect();
        let name = ui::bold(to, &profile.name);
        if notes.is_empty() {
            out.push_str(&format!("{name}\n"));
        } else {
            out.push_str(&format!("{name}  ({})\n", notes.join(", ")));
        }
        let present: Vec<_> = VENDORS
            .iter()
            .filter_map(|&(vendor, product, _)| {
                let id = profile.accounts.get(vendor)?;
                Some((product, accounts.get(id).map_or("?", |a| a.name.as_str())))
            })
            .collect();
        for (i, (product, account)) in present.iter().enumerate() {
            let branch = if i + 1 == present.len() {
                "└─"
            } else {
                "├─"
            };
            out.push_str(&format!(
                "{} {}  {account}\n",
                ui::muted(to, branch),
                ui::muted(to, format!("{product:<7}"))
            ));
        }
    }
    out
}

/// The active profile for the current directory, why, and its accounts' sign-in state.
pub fn show(home: &Path) -> Result<()> {
    let profiles = ProfileStore::load(home)?;
    let accounts = AccountStore::load(home)?;
    let cwd = resolve::canonical_cwd()?;
    let env = std::env::var("AGS_PROFILE").ok();
    let Some((profile, source)) = resolve::resolve_profile(&profiles, None, env.as_deref(), &cwd)?
    else {
        println!("Profile: none here (unprofiled); create one with `ags profile new <name>`.");
        return Ok(());
    };
    println!(
        "Profile: {} ({})",
        profile.name,
        source.describe(resolve::home_dir().as_deref())
    );
    for &(vendor, product, _) in VENDORS {
        match profile.accounts.get(vendor).and_then(|id| accounts.get(id)) {
            Some(a) => {
                println!(
                    "  {product:<8} {}  [{}, {}]",
                    a.name,
                    a.provider,
                    isolation::signin(a, home).label()
                );
                let overrides = isolation::local_overrides(a, home);
                if !overrides.is_empty() {
                    println!("           local overrides: {}", overrides.join(", "));
                }
            }
            None => println!(
                "  {product:<8} none; run `ags profile set {} {product} <account>`",
                profile.name
            ),
        }
    }
    Ok(())
}

pub async fn remove(home: &Path, name: &str) -> Result<()> {
    let profile = ProfileStore::load(home)?.find(name)?.clone();
    ProfileStore::update(home, |s| {
        s.remove(&profile.id);
        Ok(())
    })?;
    if let Some(client) = &profile.registered_with
        && let Ok(config) = auth::connected(home).await
        && &config.client_id == client
    {
        let _ = auth::deregister_profile(&config, &profile.id).await;
    }
    Ok(())
}

pub struct Launched {
    pub account: Account,
    pub attribution: Option<relay::Attribution>,
}

/// Resolve the profile and account for a claude or codex launch; make sure the account is
/// usable and registered. `None` means an unprofiled launch.
pub async fn prepare_launch(
    home: &Path,
    config: &Config,
    client: &str,
    flag: Option<&str>,
    provider_forced: bool,
) -> Result<Option<Launched>> {
    let vendor = account_store::vendor_for_cli(client).context("unsupported CLI")?;
    let interactive = tty();
    let cwd = resolve::canonical_cwd()?;
    let env = std::env::var("AGS_PROFILE").ok();
    let profiles = ProfileStore::load(home)?;
    let accounts = AccountStore::load(home)?;
    let (profile, account, source) = match resolve::launch(
        &profiles,
        &accounts,
        vendor,
        provider_forced,
        flag,
        env.as_deref(),
        &cwd,
        interactive,
    )? {
        Launch::Unprofiled => return Ok(None),
        Launch::Ready {
            profile,
            account,
            source,
        } => (profile.clone(), account.clone(), source),
        Launch::NeedsAccount { profile, source } => {
            let product = account_store::product(vendor);
            eprintln!("Profile {} has no {product} account.", profile.name);
            match pick_account(
                home,
                &profile.name,
                vendor,
                None,
                "Launch without a profile this time",
            )
            .await?
            {
                Picked::Skip | Picked::Unavailable => return Ok(None),
                Picked::Account(account) => {
                    ProfileStore::update(home, |s| {
                        s.set_account(&profile.id, vendor, Some(&account.id))
                    })?;
                    (profile.clone(), account, source)
                }
            }
        }
    };
    if std::io::stderr().is_terminal() {
        eprintln!(
            "\x1b[2mags: {} \u{2192} {} ({})\x1b[0m",
            profile.name,
            account.name,
            source.describe(resolve::home_dir().as_deref())
        );
    }
    ensure_signed_in(home, &account, interactive).await?;
    if !account.native {
        state::private_dir(&account_store::dir(home, &account.id))?;
    }
    isolation::link_shared(&account, home)?;
    let attribution = register(home, config, &profile, &account).await;
    Ok(Some(Launched {
        account,
        attribution,
    }))
}

async fn ensure_signed_in(home: &Path, account: &Account, interactive: bool) -> Result<()> {
    if isolation::signin(account, home) != SignIn::No {
        return Ok(());
    }
    let name = &account.name;
    if !interactive {
        bail!("account {name} is not signed in; run `ags account login {name}`");
    }
    if !Confirm::new(format!("{name} isn't signed in. Sign in now?"))
        .selected(true)
        .ask()?
    {
        bail!("account {name} is not signed in; run `ags account login {name}`");
    }
    let code = account_cmd::vendor_auth(home, account, false).await?;
    if code != 0 {
        bail!("sign-in exited with status {code}; run `ags account login {name}`");
    }
    Ok(())
}

/// Registers the account and profile with the server if needed. Attribution is sent only for
/// an account registered with the current client; otherwise warn and launch without it.
async fn register(
    home: &Path,
    config: &Config,
    profile: &Profile,
    account: &Account,
) -> Option<relay::Attribution> {
    let current = Some(config.client_id.as_str());
    let account_ok = account.registered_with.as_deref() == current;
    let profile_ok = profile.registered_with.as_deref() == current;
    if !(account_ok && profile_ok) {
        match auth::supports_accounts(config).await {
            Ok(true) => {}
            Ok(false) => {
                eprintln!(
                    "This Aegis server does not support accounts; launching without attribution."
                );
                return None;
            }
            Err(error) => {
                eprintln!(
                    "Could not reach Aegis to register {} ({error}); launching without attribution.",
                    account.name
                );
                return None;
            }
        }
    }
    if !account_ok {
        let registration = AccountRegistration {
            vendor: account.vendor.clone(),
            label: account.name.clone(),
            provider: account.provider.clone(),
        };
        let done = auth::register_account(config, &account.id, &registration)
            .await
            .and_then(|()| {
                AccountStore::update(home, |s| {
                    if let Some(a) = s.accounts.iter_mut().find(|a| a.id == account.id) {
                        a.registered_with = Some(config.client_id.clone());
                    }
                    Ok(())
                })
            });
        if let Err(error) = done {
            eprintln!(
                "Account {} could not be registered with Aegis ({error}); launching without attribution.",
                account.name
            );
            return None;
        }
    }
    let mut profile_id = Some(profile.id.clone());
    if !profile_ok {
        let label = ProfileLabel {
            label: profile.name.clone(),
        };
        let done = auth::register_profile(config, &profile.id, &label)
            .await
            .and_then(|()| {
                ProfileStore::update(home, |s| {
                    if let Some(p) = s.profiles.iter_mut().find(|p| p.id == profile.id) {
                        p.registered_with = Some(config.client_id.clone());
                    }
                    Ok(())
                })
            });
        if let Err(error) = done {
            eprintln!(
                "Profile {} could not be registered with Aegis ({error}); usage will be attributed to the account only.",
                profile.name
            );
            profile_id = None;
        }
    }
    Some(relay::Attribution {
        account: account.id.clone(),
        profile: profile_id,
        run: uuid::Uuid::new_v4().to_string(),
    })
}

#[cfg(test)]
mod tests;
