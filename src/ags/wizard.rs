//! Inline provider selection. Runtime routing only consumes the saved choices.
use super::{profiles, prompt::Ask, state::Config};
use crate::ags_wire::Provider;
use anyhow::{Context, Result, bail};
use demand::{DemandOption, MultiSelect, Select};
use std::{collections::BTreeMap, io::IsTerminal};

pub const KINDS: &[(&str, &str, &str)] = &[
    ("claude_subscription", "Claude", "anthropic"),
    ("codex_subscription", "Codex", "openai-codex"),
    (
        "antigravity_subscription",
        "Antigravity",
        "google-antigravity",
    ),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Prompts<'a> {
    /// Flags or saved choices only.
    None,
    /// These providers; defaults follow automatically.
    Given(&'a [String]),
    /// Providers, the default per kind, and CLI overrides.
    All,
}

/// `ags configure` and the scripted setup: prompts for everything on a terminal without flags.
pub fn configure(config: &mut Config, available: &[Provider], flags: &[String]) -> Result<()> {
    let prompts = if flags.is_empty() && std::io::stdin().is_terminal() {
        Prompts::All
    } else {
        Prompts::None
    };
    choose(config, available, flags, prompts)
}

/// Uses exactly the `selected` providers (the setup screen's route checkboxes); each kind's
/// default stays the saved one when still selected, else the first selected.
pub fn set_routes(config: &mut Config, available: &[Provider], selected: &[String]) -> Result<()> {
    choose(config, available, &[], Prompts::Given(selected))
}

/// Providers the AGS clients can use, in the server's order.
pub fn supported(available: &[Provider]) -> Vec<Provider> {
    available
        .iter()
        .filter(|p| profiles::CLIENTS.iter().any(|c| profiles::compatible(c, p)))
        .cloned()
        .collect()
}

/// The display label of a provider kind, e.g. `Claude`.
pub fn kind_label(kind: &str) -> &str {
    KINDS.iter().find(|k| k.0 == kind).map_or(kind, |k| k.1)
}

fn choose(
    config: &mut Config,
    available: &[Provider],
    flags: &[String],
    prompts: Prompts,
) -> Result<()> {
    let available = supported(available);
    if available.is_empty() {
        bail!("server has no supported subscription providers");
    }
    let interactive = prompts == Prompts::All;
    if prompts == Prompts::None && flags.is_empty() && available.len() > 1 {
        bail!(
            "interactive terminal required for provider selection; use ags setup/configure --provider CLI=ID for noninteractive configuration"
        );
    }
    let mut explicit = BTreeMap::new();
    for flag in flags {
        let (cli, id) = flag
            .split_once('=')
            .context("default must be CLI=PROVIDER_ID")?;
        if !profiles::CLIENTS.contains(&cli) {
            bail!("unsupported CLI {cli}");
        }
        if !available
            .iter()
            .any(|p| p.id == id && profiles::compatible(cli, p))
        {
            bail!("provider {id} is not compatible with {cli}");
        }
        if explicit.insert(cli.to_owned(), id.to_owned()).is_some() {
            bail!("duplicate default for {cli}");
        }
    }
    let selected = if let Prompts::Given(selected) = prompts {
        selected.to_vec()
    } else if prompts == Prompts::All {
        let prior: Vec<String> = if config.providers.is_empty() {
            available.iter().map(|p| p.id.clone()).collect()
        } else {
            config.providers.iter().map(|p| p.id.clone()).collect()
        };
        let mut prompt = MultiSelect::new("Which providers will you use?")
            .description("Space toggles, Enter confirms.")
            .min(1);
        for p in &available {
            let label = kind_label(&p.kind);
            prompt = prompt.option(
                DemandOption::new(p.id.clone())
                    .label(&p.id)
                    .description(label)
                    .selected(prior.contains(&p.id)),
            );
        }
        prompt.ask()?
    } else {
        available
            .iter()
            .filter(|p| {
                config.providers.is_empty()
                    || config.providers.iter().any(|old| old.id == p.id)
                    || explicit.values().any(|id| id == &p.id)
            })
            .map(|p| p.id.clone())
            .collect()
    };
    let providers: Vec<_> = available
        .into_iter()
        .filter(|p| selected.contains(&p.id))
        .collect();
    if providers.is_empty() {
        bail!("no providers selected; run ags configure in an interactive terminal");
    }
    let mut defaults = BTreeMap::new();
    for &(kind, label, _) in KINDS {
        let choices: Vec<_> = providers.iter().filter(|p| p.kind == kind).collect();
        if choices.is_empty() {
            continue;
        }
        let prior = config
            .provider_defaults
            .get(kind)
            .or_else(|| {
                explicit
                    .iter()
                    .find(|(cli, id)| cli.as_str() != "pi" && choices.iter().any(|p| &p.id == *id))
                    .map(|(_, id)| id)
            })
            .or_else(|| {
                config
                    .defaults
                    .values()
                    .find(|id| choices.iter().any(|p| &p.id == *id))
            })
            .filter(|id| choices.iter().any(|p| &p.id == *id))
            .unwrap_or(&choices[0].id)
            .clone();
        let id = if interactive && choices.len() > 1 {
            let mut prompt = Select::new(format!("Default {label} subscription"));
            for p in &choices {
                prompt = prompt.option(
                    DemandOption::new(p.id.clone())
                        .label(&p.id)
                        .selected(p.id == prior),
                );
            }
            prompt.ask()?
        } else {
            prior
        };
        defaults.insert(kind.to_owned(), id);
    }
    let mut overrides = BTreeMap::new();
    if interactive {
        let mut prompt = MultiSelect::new("Override provider defaults for specific CLIs?")
            .description("Optional. Space toggles, Enter confirms.");
        let mut has_choices = false;
        for &cli in profiles::CLIENTS {
            if KINDS.iter().any(|k| {
                providers
                    .iter()
                    .filter(|p| p.kind == k.0 && profiles::compatible(cli, p))
                    .count()
                    > 1
            }) {
                prompt = prompt.option(
                    DemandOption::new(cli.to_owned())
                        .label(cli)
                        .selected(config.cli_overrides.contains_key(cli)),
                );
                has_choices = true;
            }
        }
        if has_choices {
            for cli in prompt.ask()? {
                let mut cli_defaults = BTreeMap::new();
                for &(kind, label, _) in KINDS {
                    let choices: Vec<_> = providers
                        .iter()
                        .filter(|p| p.kind == kind && profiles::compatible(&cli, p))
                        .collect();
                    if choices.len() < 2 {
                        continue;
                    }
                    let default = &defaults[kind];
                    let prior = config
                        .cli_overrides
                        .get(&cli)
                        .and_then(|m| m.get(kind))
                        .filter(|id| choices.iter().any(|p| &p.id == *id))
                        .unwrap_or(default);
                    let mut prompt = Select::new(format!("{cli}: {label} subscription")).option(
                        DemandOption::new(default.clone())
                            .label(&format!("Use default ({default})"))
                            .selected(prior == default),
                    );
                    for p in choices.iter().filter(|p| &p.id != default) {
                        prompt = prompt.option(
                            DemandOption::new(p.id.clone())
                                .label(&p.id)
                                .description("CLI override")
                                .selected(&p.id == prior),
                        );
                    }
                    let id = prompt.ask()?;
                    if &id != default {
                        cli_defaults.insert(kind.to_owned(), id);
                    }
                }
                if !cli_defaults.is_empty() {
                    overrides.insert(cli, cli_defaults);
                }
            }
        }
    } else {
        overrides = config.cli_overrides.clone();
        for choices in overrides.values_mut() {
            choices.retain(|kind, id| providers.iter().any(|p| &p.id == id && &p.kind == kind));
        }
        for (cli, id) in &explicit {
            let provider = providers
                .iter()
                .find(|p| &p.id == id)
                .context("provider unavailable")?;
            overrides
                .entry(cli.clone())
                .or_insert_with(BTreeMap::new)
                .insert(provider.kind.clone(), id.clone());
        }
    }
    config.providers = providers;
    config.provider_defaults = defaults;
    config.cli_overrides = overrides;
    config.defaults.clear();
    for &cli in profiles::CLIENTS {
        let Some(kind) = profiles::kind(cli) else {
            continue;
        };
        if let Some(id) = config.provider_for(cli, kind).map(str::to_owned) {
            config.defaults.insert(cli.to_owned(), id);
        }
    }
    if let Some(id) = explicit.get("pi") {
        config.defaults.insert("pi".into(), id.clone());
    }
    if interactive {
        eprintln!(
            "Selection complete. pi keeps its model defaults and can use all enabled providers."
        );
    }
    Ok(())
}
