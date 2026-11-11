//! Inline provider selection. Runtime routing only consumes the saved choices.
use super::{profiles, state::Config};
use crate::ags_wire::Provider;
use anyhow::{Context, Result, bail};
use std::{collections::BTreeMap, io::IsTerminal};

struct PlainTheme;

impl cliclack::Theme for PlainTheme {
    fn state_symbol(&self, state: &cliclack::ThemeState) -> String {
        let marker = match state {
            cliclack::ThemeState::Active => ">",
            cliclack::ThemeState::Submit => "+",
            cliclack::ThemeState::Cancel => "x",
            cliclack::ThemeState::Error(_) => "!",
        };
        self.state_symbol_color(state).apply_to(marker).to_string()
    }

    fn radio_symbol(&self, state: &cliclack::ThemeState, selected: bool) -> String {
        match state {
            cliclack::ThemeState::Active | cliclack::ThemeState::Error(_) => {
                if selected { ">" } else { " " }.to_owned()
            }
            _ => String::new(),
        }
    }

    fn checkbox_symbol(
        &self,
        state: &cliclack::ThemeState,
        selected: bool,
        active: bool,
    ) -> String {
        match state {
            cliclack::ThemeState::Active | cliclack::ThemeState::Error(_) => {
                format!(
                    "{} [{}]",
                    if active { ">" } else { " " },
                    if selected { "x" } else { " " }
                )
            }
            _ => String::new(),
        }
    }
}

pub fn init_theme() {
    cliclack::set_theme(PlainTheme);
}

pub const KINDS: &[(&str, &str, &str)] = &[
    ("claude_subscription", "Claude", "anthropic"),
    ("codex_subscription", "Codex", "openai-codex"),
    (
        "antigravity_subscription",
        "Antigravity",
        "google-antigravity",
    ),
];

pub fn configure(config: &mut Config, available: &[Provider], flags: &[String]) -> Result<()> {
    let available: Vec<_> = available
        .iter()
        .filter(|p| profiles::CLIENTS.iter().any(|c| profiles::compatible(c, p)))
        .cloned()
        .collect();
    if available.is_empty() {
        bail!("server has no supported subscription providers");
    }
    let interactive = flags.is_empty() && std::io::stdin().is_terminal();
    if !interactive && flags.is_empty() && available.len() > 1 {
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
    let selected = if interactive {
        cliclack::intro("AGS relay configuration")?;
        let prior = if config.providers.is_empty() {
            available.iter().map(|p| p.id.clone()).collect()
        } else {
            config.providers.iter().map(|p| p.id.clone()).collect()
        };
        let mut prompt = cliclack::multiselect("Which providers will you use?")
            .initial_values(prior)
            .max_rows(10);
        for p in &available {
            let label = KINDS
                .iter()
                .find(|k| k.0 == p.kind)
                .map(|k| k.1)
                .unwrap_or(&p.kind);
            prompt = prompt.item(p.id.clone(), &p.id, label);
        }
        prompt.interact()?
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
            let mut prompt =
                cliclack::select(format!("Default {label} subscription")).initial_value(prior);
            for p in &choices {
                prompt = prompt.item(p.id.clone(), &p.id, "");
            }
            prompt.interact()?
        } else {
            prior
        };
        defaults.insert(kind.to_owned(), id);
    }
    let mut overrides = BTreeMap::new();
    if interactive {
        let mut prompt = cliclack::multiselect("Override provider defaults for specific CLIs?")
            .required(false)
            .initial_values(config.cli_overrides.keys().cloned().collect());
        let mut has_choices = false;
        for &cli in profiles::CLIENTS {
            if KINDS.iter().any(|k| {
                providers
                    .iter()
                    .filter(|p| p.kind == k.0 && profiles::compatible(cli, p))
                    .count()
                    > 1
            }) {
                prompt = prompt.item(cli.to_owned(), cli, "optional");
                has_choices = true;
            }
        }
        if has_choices {
            for cli in prompt.interact()? {
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
                    let mut prompt = cliclack::select(format!("{cli}: {label} subscription"))
                        .initial_value(prior.clone())
                        .item(default.clone(), format!("Use default ({default})"), "");
                    for p in choices.iter().filter(|p| &p.id != default) {
                        prompt = prompt.item(p.id.clone(), &p.id, "CLI override");
                    }
                    let id = prompt.interact()?;
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
        cliclack::outro(
            "Selection complete. pi keeps its model defaults and can use all enabled providers.",
        )?;
    }
    Ok(())
}
