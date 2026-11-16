//! Provider and guardrail settings, for a user's own layer and, for
//! superusers, the instance layer.
use aegis_wire::provider_type;
use maud::{Markup, html};
use std::collections::HashMap;

use super::{layout_with_nav, signed_in_nav};
use crate::{
    config::{GuardrailsConfig, GuardrailsMode, ProviderConfig, ProviderKind},
    policies::{detect, secrets},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    Mine,
    Instance,
}

impl Layer {
    pub fn base(self) -> &'static str {
        match self {
            Self::Mine => "/settings",
            Self::Instance => "/admin",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Account,
    Providers,
    Guardrails,
    InstanceProviders,
    InstanceGuardrails,
}

/// The tabs of one settings area: a user's own account settings, or, for
/// superusers, the instance settings reached from their own button in the top
/// bar.
pub fn tabs(active: Tab) -> Markup {
    let entries: &[(Tab, &str, &str)] = match active {
        Tab::Account | Tab::Providers | Tab::Guardrails => &[
            (Tab::Account, "/account", "Account"),
            (Tab::Providers, "/settings/providers", "Providers"),
            (Tab::Guardrails, "/settings/guardrails", "Guardrails"),
        ],
        Tab::InstanceProviders | Tab::InstanceGuardrails => &[
            (Tab::InstanceProviders, "/admin/providers", "Providers"),
            (Tab::InstanceGuardrails, "/admin/guardrails", "Guardrails"),
        ],
    };
    html! {
        nav class="range-switch settings-tabs" aria-label="Settings" {
            @for (tab, href, label) in entries {
                @if *tab == active {
                    span class="range-option is-current" aria-current="page" { (label) }
                } @else {
                    a class="range-option" href=(href) { (label) }
                }
            }
        }
    }
}

pub fn type_label(type_name: &str) -> &'static str {
    match type_name {
        provider_type::CLAUDE_SUBSCRIPTION => "Claude subscription",
        provider_type::CODEX_SUBSCRIPTION => "Codex subscription",
        provider_type::TYPESAFE => "TypeSafe",
        provider_type::BEDROCK => "Amazon Bedrock",
        provider_type::VERTEX => "Google Vertex AI",
        _ => "Unknown",
    }
}

fn default_url(type_name: &str) -> String {
    ProviderKind::from_parts(type_name, None)
        .map(|kind| kind.base_url().to_owned())
        .unwrap_or_default()
}

/// Values of the add form, kept when it is shown again with an error.
#[derive(Default)]
pub struct NewProvider {
    pub id: String,
    pub kind: String,
    pub region: String,
    pub location: String,
    pub base_url: String,
}

pub struct ProvidersView<'a> {
    pub csrf: &'a str,
    pub layer: Layer,
    pub superuser: bool,
    /// This layer's providers.
    pub providers: &'a [ProviderConfig],
    /// The instance's providers, shown read-only on a user's own page.
    pub inherited: &'a [ProviderConfig],
    /// Active API keys and connected apps granted each ID.
    pub grants: &'a HashMap<String, (i64, i64)>,
    pub editing: Option<&'a str>,
    pub deleting: Option<&'a str>,
    /// For an instance provider being deleted, the emails of the users whose
    /// active keys or apps use it, who can get a copy instead of losing it.
    pub users_of_deleting: &'a [String],
    pub new: &'a NewProvider,
    pub error: Option<&'a str>,
    pub notice: Option<&'a str>,
}

pub fn providers_page(view: &ProvidersView<'_>) -> Markup {
    let (heading, title, tab, intro) = match view.layer {
        Layer::Mine => (
            "Providers",
            "Providers",
            Tab::Providers,
            "Upstreams your API keys and connected apps can reach. Clients address each one as /providers/<ID>.",
        ),
        Layer::Instance => (
            "Instance settings",
            "Instance providers",
            Tab::InstanceProviders,
            "Upstreams every user on this instance can use. Users cannot create a provider with the same ID.",
        ),
    };
    let base = view.layer.base();
    layout_with_nav(
        &format!("{title} · Aegis"),
        Some(signed_in_nav(view.superuser)),
        html! {
            main class="account-shell" {
                div class="page-head" { h1 class="page-title" { (heading) } }
                (tabs(tab))
                div class="account-grid" {
                    div class="account-column" {
                        @if let Some(id) = view.deleting {
                            (delete_card(view, id))
                        }
                        article class="auth-card account-card" {
                            h2 { (title) }
                            p { (intro) }
                            @if let Some(notice) = view.notice { p class="auth-note" role="status" { (notice) } }
                            div class="table-wrap" {
                                table class="data-table" {
                                    thead {
                                        tr {
                                            th scope="col" { "ID" }
                                            th scope="col" { "Type" }
                                            th scope="col" { "Upstream" }
                                            th scope="col" { "Used by" }
                                            th scope="col" class="col-actions" {
                                                span class="visually-hidden" { "Actions" }
                                            }
                                        }
                                    }
                                    tbody {
                                        @if view.providers.is_empty() && view.inherited.is_empty() {
                                            tr { td colspan="5" class="table-empty" { "No provider exists yet." } }
                                        }
                                        @for provider in view.providers {
                                            (provider_row(view, provider, false))
                                        }
                                        @for provider in view.inherited {
                                            (provider_row(view, provider, true))
                                        }
                                    }
                                }
                            }
                        }
                    }
                    div class="account-column" {
                        article class="auth-card account-card" {
                            h2 { "Add provider" }
                            p { "The ID and type cannot change later, because client URLs and usage history refer to them." }
                            @if let Some(error) = view.error { p class="auth-error" role="alert" { (error) } }
                            form method="post" action=(format!("{base}/providers")) {
                                input type="hidden" name="csrf" value=(view.csrf);
                                label for="provider-id" { "ID" }
                                input id="provider-id" name="id" required maxlength="64"
                                    pattern="[A-Za-z0-9_-]+" placeholder="work" value=(view.new.id);
                                label for="provider-type" { "Type" }
                                select id="provider-type" name="kind" required {
                                    @for type_name in ProviderKind::TYPE_NAMES {
                                        option value=(type_name) selected[view.new.kind == type_name] {
                                            (type_label(type_name))
                                        }
                                    }
                                }
                                // Each shown only while its type is selected; see `.bedrock-only`
                                // and `.vertex-only` in the stylesheet.
                                div class="bedrock-only" {
                                    label for="provider-region" { "Region" }
                                    input id="provider-region" name="region" maxlength="40" pattern="[a-z0-9-]+"
                                        placeholder="us-east-1" value=(view.new.region);
                                }
                                div class="vertex-only" {
                                    label for="provider-location" { "Location" }
                                    input id="provider-location" name="location" maxlength="40" pattern="[a-z0-9-]+"
                                        placeholder="global, or one such as us-east5" value=(view.new.location);
                                }
                                details class="advanced" open[!view.new.base_url.is_empty()] {
                                    summary { "Advanced" }
                                    label for="provider-url" { "Upstream base URL" }
                                    input id="provider-url" name="base_url" type="url"
                                        placeholder="Leave empty for the type's default" value=(view.new.base_url);
                                }
                                button type="submit" { "Add provider" }
                            }
                        }
                    }
                }
            }
        },
    )
}

fn provider_row(view: &ProvidersView<'_>, provider: &ProviderConfig, inherited: bool) -> Markup {
    let type_name = provider.kind.type_name();
    let url = provider.kind.base_url();
    let default = default_url(type_name);
    let (keys, apps) = view.grants.get(&provider.id).copied().unwrap_or((0, 0));
    let base = view.layer.base();
    let editing = !inherited && view.editing == Some(provider.id.as_str());
    html! {
        tr {
            th scope="row" class="cell-title" {
                code { (provider.id) }
                @if inherited { span class="status-tag" { "Instance" } }
            }
            td { (type_label(type_name)) }
            @if editing {
                td colspan="3" {
                    form class="inline-form" method="post" action=(format!("{base}/providers/{}", provider.id)) {
                        input type="hidden" name="csrf" value=(view.csrf);
                        label class="visually-hidden" for="edit-url" { "Upstream base URL" }
                        input id="edit-url" name="base_url" type="url"
                            value=(if url == default { "" } else { url })
                            placeholder=(format!("Default: {default}"));
                        button type="submit" class="compact" { "Save" }
                        a href=(format!("{base}/providers")) role="button" class="secondary compact" { "Cancel" }
                    }
                }
            } @else {
                td {
                    @if url == default { span class="cell-absent" { "default" } }
                    @else if let Some(region) = provider.kind.bedrock_region().or_else(|| provider.kind.vertex_location()) { (region) }
                    @else { code { (url) } }
                }
                td { (usage_text(keys, apps)) }
                td class="col-actions" {
                    @if !inherited {
                        div class="cell-actions" {
                            a href=(format!("{base}/providers?edit={}", provider.id)) role="button" class="secondary compact" { "Edit" }
                            a href=(format!("{base}/providers?delete={}", provider.id)) role="button" class="secondary compact" { "Delete" }
                        }
                    }
                }
            }
        }
    }
}

fn usage_text(keys: i64, apps: i64) -> String {
    let plural = |n: i64, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    match (keys, apps) {
        (0, 0) => "—".to_owned(),
        (keys, 0) => plural(keys, "key", "keys"),
        (0, apps) => plural(apps, "app", "apps"),
        (keys, apps) => format!(
            "{}, {}",
            plural(keys, "key", "keys"),
            plural(apps, "app", "apps")
        ),
    }
}

fn delete_card(view: &ProvidersView<'_>, id: &str) -> Markup {
    let (keys, apps) = view.grants.get(id).copied().unwrap_or((0, 0));
    let base = view.layer.base();
    let users = view.users_of_deleting;
    let offer_move = view.layer == Layer::Instance && !users.is_empty();
    html! {
        article class="auth-card account-card danger-card" role="alertdialog" aria-labelledby="delete-title" {
            h2 id="delete-title" { "Delete " code { (id) } "?" }
            @if keys + apps == 0 {
                p { "No API key or connected app uses it." }
            } @else if offer_move {
                p { "It is granted to " (usage_text(keys, apps)) " belonging to:" }
                ul class="plain-list" {
                    @for email in users { li { (email) } }
                }
                p {
                    strong { "Keep for users" }
                    " gives each of them their own provider " code { (id) }
                    " with the same type and upstream, which they can edit or delete. Their keys and apps keep working; everyone else loses access. "
                    strong { "Delete for everyone" }
                    " removes it from every key and app."
                }
            } @else {
                p { "It is granted to " (usage_text(keys, apps)) ". They lose access to it, and a provider created later with the same ID is not granted to them." }
            }
            p { "Usage history stays." }
            div class="cell-actions" {
                @if offer_move {
                    form method="post" action=(format!("{base}/providers/{id}/move")) {
                        input type="hidden" name="csrf" value=(view.csrf);
                        button type="submit" class="compact" {
                            @if users.len() == 1 { "Keep for 1 user" } @else { "Keep for " (users.len()) " users" }
                        }
                    }
                }
                form method="post" action=(format!("{base}/providers/{id}/delete")) {
                    input type="hidden" name="csrf" value=(view.csrf);
                    button type="submit" class="danger compact" {
                        @if view.layer == Layer::Instance { "Delete for everyone" } @else { "Delete provider" }
                    }
                }
                a href=(format!("{base}/providers")) role="button" class="secondary compact" { "Cancel" }
            }
        }
    }
}

pub struct GuardrailsView<'a> {
    pub csrf: &'a str,
    pub layer: Layer,
    pub superuser: bool,
    /// This layer's settings.
    pub config: &'a GuardrailsConfig,
    /// The instance's settings, shown as enforced on a user's own page.
    pub enforced: Option<&'a GuardrailsConfig>,
    pub error: Option<&'a str>,
    pub notice: Option<&'a str>,
    pub sample: Option<&'a str>,
    /// Detector name and a shortened match, for the sample.
    pub findings: Option<&'a [(String, String)]>,
    /// Values to show again in the empty pattern row after an error.
    pub new_row: (&'a str, &'a str),
}

pub fn guardrails_page(view: &GuardrailsView<'_>) -> Markup {
    let (heading, title, tab, intro) = match view.layer {
        Layer::Mine => (
            "Guardrails",
            "Guardrails",
            Tab::Guardrails,
            "Checks that run on your requests before they reach a provider. The instance's guardrails always run as well; yours can only add to them.",
        ),
        Layer::Instance => (
            "Instance settings",
            "Instance guardrails",
            Tab::InstanceGuardrails,
            "Checks that run on every user's requests. Users can add their own but cannot weaken these.",
        ),
    };
    let base = view.layer.base();
    let config = view.config;
    let enforced = view.enforced.filter(|instance| instance.enabled);
    let mask_enforced = enforced.is_some_and(|instance| instance.mode == GuardrailsMode::Mask);
    let enforced_secrets: Option<Option<&[String]>> = enforced
        .filter(|instance| instance.secrets.enabled)
        .map(|instance| instance.secrets.detectors.as_deref());
    let enforced_patterns: Vec<(&String, &String)> = enforced
        .filter(|instance| instance.regex.enabled)
        .map(|instance| instance.regex.detectors.iter().collect())
        .unwrap_or_default();
    let detector_names = detect::names(&secrets::DETECTORS);
    let selected = |name: &str| {
        config
            .secrets
            .detectors
            .as_ref()
            .is_none_or(|names| names.iter().any(|selected| selected == name))
    };
    layout_with_nav(
        &format!("{title} · Aegis"),
        Some(signed_in_nav(view.superuser)),
        html! {
            main class="account-shell" {
                div class="page-head" { h1 class="page-title" { (heading) } }
                (tabs(tab))
                div class="account-grid" {
                    div class="account-column" {
                        article class="auth-card account-card" {
                            h2 { (title) }
                            p { (intro) }
                            @if let Some(instance) = enforced {
                                div class="auth-note" {
                                    strong { "Enforced by this instance: " }
                                    (enforced_summary(instance))
                                }
                            }
                            @if let Some(notice) = view.notice { p class="auth-note" role="status" { (notice) } }
                            @if let Some(error) = view.error { p class="auth-error" role="alert" { (error) } }
                            form method="post" action=(format!("{base}/guardrails")) {
                                // Enter in a field presses the first submit button in the form, which
                                // must be Save rather than a row's Delete.
                                button type="submit" name="op" value="save" class="visually-hidden" tabindex="-1" aria-hidden="true" { "Save" }
                                input type="hidden" name="csrf" value=(view.csrf);
                                label class="checkbox-row toggle-row" {
                                    input type="checkbox" name="enabled" checked[config.enabled];
                                    span { "Turn on guardrails" }
                                }
                                fieldset class="field-group" {
                                    legend { "When something is found" }
                                    div class="checkbox-grid single" {
                                        label class="checkbox-row" {
                                            input type="radio" name="mode" value="observe"
                                                checked[config.mode == GuardrailsMode::Observe]
                                                disabled[mask_enforced];
                                            span { "Observe: record it and send the request unchanged" }
                                        }
                                        label class="checkbox-row" {
                                            input type="radio" name="mode" value="mask"
                                                checked[config.mode == GuardrailsMode::Mask || mask_enforced];
                                            span { "Mask: replace it before sending, and restore it in the response" }
                                        }
                                    }
                                    @if mask_enforced {
                                        p class="field-note" { "The instance masks, so observe is not available." }
                                    }
                                }
                                fieldset class="field-group" {
                                    legend { "Secrets" }
                                    div class="checkbox-grid single" {
                                        label class="checkbox-row" {
                                            input type="checkbox" name="secrets_enabled" checked[config.secrets.enabled];
                                            span { "Detect credentials such as API keys and tokens" }
                                        }
                                        label class="checkbox-row" {
                                            input type="checkbox" name="secrets_all" checked[config.secrets.detectors.is_none()];
                                            span { "Run every built-in detector, including ones added in later releases" }
                                        }
                                    }
                                    p class="field-note" { "Otherwise, run only these:" }
                                    div class="checkbox-grid" {
                                        @for name in &detector_names {
                                            @let forced = enforced_secrets.is_some_and(|names| {
                                                names.is_none_or(|names| names.iter().any(|n| n == name))
                                            });
                                            label class="checkbox-row" {
                                                input type="checkbox" name="secret" value=(name)
                                                    checked[selected(name) || forced] disabled[forced];
                                                span { code { (name) } @if forced { " (instance)" } }
                                            }
                                        }
                                    }
                                }
                                fieldset class="field-group" {
                                    legend { "Custom patterns" }
                                    div class="checkbox-grid single" {
                                        label class="checkbox-row" {
                                            input type="checkbox" name="regex_enabled" checked[config.regex.enabled];
                                            span { "Detect these regular expressions" }
                                        }
                                    }
                                    div class="pattern-rows" {
                                        @for (name, pattern) in &enforced_patterns {
                                            div class="pattern-row" {
                                                input value=(name) disabled aria-label="Name";
                                                input value=(pattern) disabled aria-label="Pattern";
                                                span class="status-tag" { "Instance" }
                                            }
                                        }
                                        @for (index, (name, pattern)) in config.regex.detectors.iter().enumerate() {
                                            div class="pattern-row" {
                                                input name="regex_name" value=(name) aria-label="Name" placeholder="internal_token";
                                                input name="regex_pattern" value=(pattern) aria-label="Pattern";
                                                button type="submit" name="op" value=(format!("delete:{index}"))
                                                    class="secondary compact" aria-label=(format!("Delete {name}")) { "Delete" }
                                            }
                                        }
                                        div class="pattern-row" {
                                            input name="new_regex_name" value=(view.new_row.0) aria-label="New pattern name" placeholder="internal_token";
                                            input name="new_regex_pattern" value=(view.new_row.1) aria-label="New pattern" placeholder=r"\bint_[a-z0-9]{32}\b";
                                            button type="submit" name="op" value="add" class="compact" { "Add" }
                                        }
                                    }
                                    p class="field-note" { "Names are lowercase snake_case." }
                                }
                                button type="submit" name="op" value="save" { "Save guardrails" }
                            }
                        }
                    }
                    div class="account-column" {
                        article class="auth-card account-card" {
                            h2 { "Test" }
                            p {
                                @match view.layer {
                                    Layer::Mine => "Paste text to see what your saved guardrails, combined with the instance's, would find.",
                                    Layer::Instance => "Paste text to see what the saved instance guardrails would find.",
                                }
                            }
                            form method="post" action=(format!("{base}/guardrails/test")) {
                                input type="hidden" name="csrf" value=(view.csrf);
                                label for="sample" { "Sample" }
                                textarea id="sample" name="sample" rows="6" maxlength="20000" { (view.sample.unwrap_or_default()) }
                                button type="submit" class="secondary" { "Test" }
                            }
                            @if let Some(findings) = view.findings {
                                @if findings.is_empty() {
                                    p class="auth-note" role="status" { "Nothing found." }
                                } @else {
                                    div class="table-wrap" {
                                        table class="data-table" {
                                            thead { tr { th scope="col" { "Detector" } th scope="col" { "Match" } } }
                                            tbody {
                                                @for (detector, preview) in findings {
                                                    tr { td { code { (detector) } } td { code { (preview) } } }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        },
    )
}

fn enforced_summary(instance: &GuardrailsConfig) -> String {
    let mut parts = vec![match instance.mode {
        GuardrailsMode::Mask => "mask".to_owned(),
        GuardrailsMode::Observe => "observe".to_owned(),
    }];
    if instance.secrets.enabled {
        parts.push(match &instance.secrets.detectors {
            None => "every secret detector".to_owned(),
            Some(names) => format!("{} secret detector(s)", names.len()),
        });
    }
    if instance.regex.enabled && !instance.regex.detectors.is_empty() {
        parts.push(format!(
            "{} custom pattern(s)",
            instance.regex.detectors.len()
        ));
    }
    parts.join(", ")
}
