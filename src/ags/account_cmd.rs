//! `ags account ...` and account creation shared with the profile commands.
use super::{
    account_store::{self, Account, AccountStore},
    auth, isolation,
    profile_store::ProfileStore,
    profiles,
    prompt::Ask,
    state::{self, Config},
};
use anyhow::{Context, Result, bail};
use clap::Subcommand;
use demand::{Confirm, DemandOption, Input, Select};
use std::{ffi::OsString, io::IsTerminal, path::Path};

#[derive(Subcommand)]
pub enum AccountCommand {
    /// List accounts, their sign-in state, and the profiles that use them.
    #[command(visible_aliases = ["ls"])]
    List,
    /// Sign the account in with the vendor login flow.
    #[command(visible_aliases = ["in"])]
    Login { name: String },
    /// Sign the account out.
    #[command(visible_aliases = ["out"])]
    Logout { name: String },
    /// Rename an account
    #[command(visible_aliases = ["mv"])]
    Rename { name: String, new_name: String },
    /// Remove an account; refuses while profiles use it unless --force (which detaches it).
    #[command(visible_aliases = ["remove"])]
    Rm {
        name: String,
        #[arg(long)]
        force: bool,
    },
    /// Create an account. On a terminal, anything not given is asked for.
    #[command(visible_aliases = ["n", "c", "add", "create"])]
    New {
        name: Option<String>,
        #[arg(long, conflicts_with = "chatgpt")]
        claude: bool,
        #[arg(long)]
        chatgpt: bool,
        /// Use the vendor's existing sign-in instead of a separate one.
        #[arg(long)]
        native: bool,
        /// Aegis route to bind; needed only when several are granted.
        #[arg(long)]
        route: Option<String>,
    },
}

/// Interactive prompts need a terminal on both ends.
pub fn tty() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// The only granted route of the vendor's kind, else the named one, else ask.
/// `retry` is the command to repeat, shown when there is no terminal to ask.
pub fn pick_route(
    config: &Config,
    vendor: &str,
    route: Option<&str>,
    tty: bool,
    retry: &str,
) -> Result<String> {
    let product = account_store::product(vendor);
    let kind = account_store::kind(vendor).context("unsupported vendor")?;
    let choices: Vec<_> = config.providers.iter().filter(|p| p.kind == kind).collect();
    if let Some(id) = route {
        return Ok(choices
            .iter()
            .find(|p| p.id == id)
            .with_context(|| format!("{id} is not a granted {product} route; run `ags configure`"))?
            .id
            .clone());
    }
    match choices.as_slice() {
        [] => bail!("no {product} routes are granted; run `ags configure`"),
        [only] => Ok(only.id.clone()),
        _ if tty => {
            let mut prompt = Select::new(format!("Which route for this {product} account?"));
            for p in &choices {
                prompt = prompt.option(DemandOption::new(p.id.clone()).label(&p.id));
            }
            Ok(prompt.ask()?)
        }
        _ => bail!(
            "several {product} routes are granted ({}); run `{retry} --route <id>`",
            choices
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Creates (and stores) an account. A native account exists at most once per vendor.
pub async fn create(
    home: &Path,
    vendor: &str,
    name: &str,
    native: bool,
    route: Option<&str>,
    retry: &str,
) -> Result<Account> {
    let config = auth::connected(home).await?;
    let provider = pick_route(&config, vendor, route, tty(), retry)?;
    let id = uuid::Uuid::now_v7().to_string();
    let directory = account_store::dir(home, &id);
    if !native {
        #[cfg(not(unix))]
        bail!("managed accounts require symlinks and are unsupported on this platform");
        state::private_dir(&directory)?;
    }
    let result = AccountStore::update(home, |store| {
        let account = Account {
            id: id.clone(),
            name: name.to_owned(),
            vendor: vendor.to_owned(),
            provider,
            native,
            created_at: state::now(),
            registered_with: None,
        };
        store.add(account.clone())?;
        Ok(account)
    });
    match result {
        Ok(account) => {
            isolation::link_shared(&account, home)?;
            Ok(account)
        }
        Err(error) => {
            let _ = std::fs::remove_dir_all(&directory);
            Err(error)
        }
    }
}

/// Suggested, editable name for a new account: `<product>`, then `<product>-2`, `-3`, …
pub fn default_name(store: &AccountStore, product: &str) -> String {
    store.unique_name(product)
}

/// Offer the native-or-separate choice only when it can still be made.
pub fn offer_native(store: &AccountStore, vendor: &str, native_dir_exists: bool) -> bool {
    native_dir_exists && store.native(vendor).is_none()
}

/// Validator for a new account name.
pub fn name_problem(store: &AccountStore, name: &str) -> Option<String> {
    if let Err(error) = account_store::validate_name("account", name) {
        return Some(error.to_string());
    }
    store
        .accounts
        .iter()
        .any(|a| a.name == name)
        .then(|| format!("account {name} already exists"))
}

fn ask_name(store: &AccountStore, product: &str) -> Result<String> {
    let snapshot = store.clone();
    Ok(Input::new("Account name")
        .default_value(default_name(store, product))
        .validator(move |v: &str| name_problem(&snapshot, v).map_or(Ok(()), Err))
        .ask()?)
}

/// `ags account new`, and the guided setup's "add an account". Without a terminal, `name` and
/// `vendor` are required; with one, whatever is missing is asked for.
pub async fn new_account(
    home: &Path,
    vendor: Option<&str>,
    name: Option<String>,
    native: bool,
    route: Option<String>,
) -> Result<Account> {
    let interactive = tty();
    let vendor = match vendor {
        Some(vendor) => vendor,
        None if interactive => {
            let mut prompt = Select::new("Which product?");
            for &(vendor, product, _) in account_store::VENDORS {
                prompt = prompt.option(DemandOption::new(vendor).label(product));
            }
            prompt.ask()?
        }
        None => match &name {
            Some(name) => {
                bail!("choose a product: `ags account new {name} --claude` or `--chatgpt`")
            }
            None => bail!("a name and product are required: `ags account new <name> --claude`"),
        },
    };
    let product = account_store::product(vendor);
    let store = AccountStore::load(home)?;
    let mut native = native;
    if interactive && !native {
        let dir = isolation::native_dir(account_store::cli(vendor));
        if offer_native(&store, vendor, dir.is_some_and(|d| d.is_dir())) {
            native = Select::new(format!("Which {product} sign-in?"))
                .option(DemandOption::new(true).label("Use the sign-in already on this machine"))
                .option(DemandOption::new(false).label("A separate sign-in"))
                .ask()?;
        }
    }
    let name = match name {
        Some(name) => name,
        None if interactive => ask_name(&store, product)?,
        None => bail!("a name is required: `ags account new <name> --{product}`"),
    };
    let flag = if vendor == "anthropic" {
        "--claude"
    } else {
        "--chatgpt"
    };
    let retry = format!(
        "ags account new {name} {flag}{}",
        if native { " --native" } else { "" }
    );
    // Validate before asking for a route or touching the disk.
    if let Some(problem) = name_problem(&store, &name) {
        bail!("{problem}");
    }
    let account = create(home, vendor, &name, native, route.as_deref(), &retry).await?;
    println!(
        "Created {product} account {} on route {}.",
        account.name, account.provider
    );
    if !native {
        if interactive {
            if Confirm::new("Sign in now?").selected(true).ask()? {
                let code = vendor_auth(home, &account, false).await?;
                if code != 0 {
                    eprintln!(
                        "Sign-in exited with status {code}; run `ags account login {}`.",
                        account.name
                    );
                }
            }
        } else {
            println!("Sign in with `ags account login {}`.", account.name);
        }
    }
    Ok(account)
}

/// Runs the vendor's login or logout inside the account.
pub async fn vendor_auth(home: &Path, account: &Account, logout: bool) -> Result<i32> {
    let cli = account_store::cli(&account.vendor);
    let executable = profiles::resolve(cli, home)?;
    if !account.native {
        state::private_dir(&account_store::dir(home, &account.id))?;
        isolation::link_shared(account, home)?;
    }
    let verb = if logout { "logout" } else { "login" };
    let mut args: Vec<OsString> = isolation::extra_args(account);
    if cli == "claude" {
        args.push("auth".into());
    }
    args.push(verb.into());
    let status = tokio::process::Command::new(executable)
        .args(args)
        .envs(isolation::env(account, home)?)
        .status()
        .await
        .context("could not launch vendor CLI")?;
    Ok(status.code().unwrap_or(1))
}

/// Refuses while profiles use the account unless `force`, which detaches it from them.
/// Returns the names of the profiles it was detached from.
pub fn detach_or_refuse(
    profiles: &mut ProfileStore,
    account: &Account,
    force: bool,
) -> Result<Vec<String>> {
    let users: Vec<String> = profiles
        .using(&account.id)
        .iter()
        .map(|p| p.name.clone())
        .collect();
    if !users.is_empty() && !force {
        bail!(
            "account {name} is used by profile{s} {list}; run `ags account rm {name} --force` to detach it, or switch the profile first (e.g. `ags profile set {first} {product} none`)",
            name = account.name,
            s = if users.len() == 1 { "" } else { "s" },
            list = users.join(", "),
            first = users[0],
            product = account_store::product(&account.vendor),
        );
    }
    Ok(profiles.detach(&account.id))
}

pub fn used_by(profiles: &ProfileStore, account: &Account) -> String {
    let names: Vec<_> = profiles
        .using(&account.id)
        .iter()
        .map(|p| p.name.clone())
        .collect();
    if names.is_empty() {
        "-".into()
    } else {
        names.join(", ")
    }
}

pub async fn dispatch(home: &Path, command: AccountCommand) -> Result<i32> {
    match command {
        AccountCommand::List => list(home)?,
        AccountCommand::Login { name } => {
            return vendor_auth(home, AccountStore::load(home)?.find(&name)?, false).await;
        }
        AccountCommand::Logout { name } => {
            return vendor_auth(home, AccountStore::load(home)?.find(&name)?, true).await;
        }
        AccountCommand::Rename { name, new_name } => {
            rename(home, &name, &new_name)?;
            println!("Renamed account {name} to {new_name}.");
        }
        AccountCommand::Rm { name, force } => remove(home, &name, force).await?,
        AccountCommand::New {
            name,
            claude,
            chatgpt,
            native,
            route,
        } => {
            let vendor = match (claude, chatgpt) {
                (true, _) => Some("anthropic"),
                (_, true) => Some("openai"),
                _ => None,
            };
            new_account(home, vendor, name, native, route).await?;
        }
    }
    Ok(0)
}

fn list(home: &Path) -> Result<()> {
    match table(home)? {
        Some(table) => print!("{table}"),
        None => println!(
            "No accounts. Create one with `ags account new <name> --claude` or `--chatgpt`, or run `ags setup` for a guided setup."
        ),
    }
    Ok(())
}

/// The account table, or `None` when there are no accounts.
pub fn table(home: &Path) -> Result<Option<String>> {
    let accounts = AccountStore::load(home)?;
    let profiles = ProfileStore::load(home)?;
    if accounts.accounts.is_empty() {
        return Ok(None);
    }
    let rows: Vec<[String; 5]> = accounts
        .accounts
        .iter()
        .map(|a| {
            [
                a.name.clone(),
                account_store::product(&a.vendor).to_owned(),
                a.provider.clone(),
                isolation::signin(a, home).label().to_owned(),
                used_by(&profiles, a),
            ]
        })
        .collect();
    let head = ["NAME", "PRODUCT", "ROUTE", "SIGN-IN", "USED BY"].map(String::from);
    let width = |i: usize| {
        std::iter::once(&head)
            .chain(&rows)
            .map(|r| r[i].len())
            .max()
            .unwrap_or(0)
    };
    let widths: Vec<_> = (0..4).map(width).collect();
    let mut out = String::new();
    for row in std::iter::once(&head).chain(&rows) {
        out.push_str(&format!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {}\n",
            row[0],
            row[1],
            row[2],
            row[3],
            row[4],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
            w3 = widths[3],
        ));
    }
    Ok(Some(out))
}

pub fn rename(home: &Path, name: &str, new_name: &str) -> Result<()> {
    AccountStore::update(home, |s| {
        let id = s.find(name)?.id.clone();
        s.rename(&id, new_name)
    })
}

pub async fn remove(home: &Path, name: &str, force: bool) -> Result<()> {
    let account = AccountStore::load(home)?.find(name)?.clone();
    let directory = account_store::dir(home, &account.id);
    let owned = !account.native && directory.symlink_metadata().is_ok_and(|m| m.is_dir());
    // Check before asking anything else.
    detach_or_refuse(&mut ProfileStore::load(home)?, &account, force)?;
    if owned && !force {
        if !tty() {
            bail!(
                "removing {name} deletes {}; run `ags account rm {name} --force` to confirm",
                directory.display()
            );
        }
        if !Confirm::new(format!(
            "Remove account {name} and delete {}?",
            directory.display()
        ))
        .selected(false)
        .ask()?
        {
            return Ok(());
        }
    }
    let detached = delete(home, &account, force).await?;
    println!("Removed account {name}.");
    if !detached.is_empty() {
        println!("Detached from profile {}.", detached.join(", "));
    }
    Ok(())
}

/// Removes the account (detaching it from profiles when `detach`), deletes a managed account's
/// directory, and deregisters it from Aegis when possible. Returns the profiles it left.
pub async fn delete(home: &Path, account: &Account, detach: bool) -> Result<Vec<String>> {
    let directory = account_store::dir(home, &account.id);
    let owned = !account.native && directory.symlink_metadata().is_ok_and(|m| m.is_dir());
    let detached = ProfileStore::update_both(home, |accounts, profiles| {
        let detached = detach_or_refuse(profiles, account, detach)?;
        accounts.remove(&account.id);
        Ok(detached)
    })?;
    if owned {
        std::fs::remove_dir_all(&directory)?;
    }
    if let Some(client) = &account.registered_with
        && let Ok(config) = auth::connected(home).await
        && &config.client_id == client
    {
        let _ = auth::deregister_account(&config, &account.id).await;
    }
    Ok(detached)
}

#[cfg(test)]
mod tests;
