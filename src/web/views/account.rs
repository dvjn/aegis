use chrono::{DateTime, Utc};
use maud::{Markup, html};

use super::{layout_with_nav, signed_in_nav};
use crate::api_keys::KeySummary;
use crate::domain::{ConnectedClient, SessionSummary};

fn moment(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%d %H:%M").to_string()
}

fn client_label(user_agent: Option<&str>) -> String {
    let Some(value) = user_agent else {
        return "Unknown client".to_owned();
    };
    let browser = [
        ("Edg", "Edge"),
        ("OPR", "Opera"),
        ("Firefox", "Firefox"),
        ("Chrome", "Chrome"),
        ("Safari", "Safari"),
        ("curl", "curl"),
        ("node", "Node.js"),
        ("python", "Python"),
        ("claude", "Claude"),
    ]
    .into_iter()
    .find(|(token, _)| contains_ignore_case(value, token))
    .map(|(_, name)| name);
    let system = [
        ("Android", "Android"),
        ("iPhone", "iOS"),
        ("iPad", "iOS"),
        ("Windows", "Windows"),
        ("Macintosh", "macOS"),
        ("Mac OS X", "macOS"),
        ("CrOS", "ChromeOS"),
        ("Linux", "Linux"),
    ]
    .into_iter()
    .find(|(token, _)| contains_ignore_case(value, token))
    .map(|(_, name)| name);
    match (browser, system) {
        (Some(browser), Some(system)) => format!("{browser} on {system}"),
        (Some(browser), None) => browser.to_owned(),
        (None, Some(system)) => format!("Unknown client on {system}"),
        (None, None) => "Unknown client".to_owned(),
    }
}

fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

#[allow(clippy::too_many_arguments)]
pub fn page(
    csrf: &str,
    sessions: &[SessionSummary],
    apps: &[ConnectedClient],
    keys: &[KeySummary],
    providers: &[String],
    revealed: Option<&str>,
    key_error: Option<&str>,
    notice: Option<&str>,
    error: Option<&str>,
) -> Markup {
    layout_with_nav(
        "Account · Aegis",
        Some(signed_in_nav()),
        html! {
            main class="account-shell" {
                div class="page-head" {
                    h1 class="page-title" { "Account" }
                    form method="post" action="/logout" {
                        input type="hidden" name="csrf" value=(csrf);
                        button type="submit" class="secondary compact" { "Sign out" }
                    }
                }
                div class="account-grid" {
                    div class="account-column" {
                        (keys_card(csrf, keys))
                        (sessions_card(csrf, sessions))
                        (apps_card(csrf, apps))
                    }
                    div class="account-column" {
                        (new_key_card(csrf, providers, revealed, key_error))
                        (password_card(csrf, notice, error))
                    }
                }
            }
        },
    )
}

fn new_key_card(
    csrf: &str,
    providers: &[String],
    revealed: Option<&str>,
    key_error: Option<&str>,
) -> Markup {
    html! {
        article class="auth-card account-card" {
            h2 { "New API key" }
            p { "Create credentials for your coding agents and restrict each key to selected provider accounts." }
            @if let Some(secret) = revealed {
                div class="auth-note" role="status" {
                    strong { "Copy this key now. It will not be shown again." }
                    pre { (secret) }
                }
            }
            @if let Some(key_error) = key_error { p class="auth-error" role="alert" { (key_error) } }
            form method="post" action="/account/api-keys" {
                input type="hidden" name="csrf" value=(csrf);
                label for="key-name" { "Name" }
                input id="key-name" name="name" required maxlength="100";
                fieldset class="field-group" {
                    legend { "Providers" }
                    div class="checkbox-grid" {
                        @for provider in providers {
                            label class="checkbox-row" {
                                input type="checkbox" name="providers" value=(provider);
                                span { (provider) }
                            }
                        }
                    }
                }
                button type="submit" { "Create key" }
            }
        }
    }
}

fn keys_card(csrf: &str, keys: &[KeySummary]) -> Markup {
    html! {
        article class="auth-card account-card" {
            h2 { "API keys" }
            p { "Credentials your coding agents send to reach the gateway." }
            div class="table-wrap" {
                table class="data-table" {
                    thead {
                        tr {
                            th scope="col" { "Name" }
                            th scope="col" { "Providers" }
                            th scope="col" { "Last used (UTC)" }
                            th scope="col" class="col-actions" {
                                span class="visually-hidden" { "Actions" }
                            }
                        }
                    }
                    tbody {
                        @if keys.is_empty() {
                            tr { td colspan="4" class="table-empty" { "No key exists yet." } }
                        }
                        @for key in keys {
                            tr {
                                th scope="row" class="cell-title" { (key_name_text(&key.name)) }
                                td { (provider_chips(&key.allowed_providers)) }
                                td class="cell-moment" { (last_used_text(key.last_used_at.as_deref())) }
                                td class="col-actions" {
                                    @if key.revoked_at.is_none() {
                                        div class="cell-actions" {
                                            form method="post" action=(format!("/account/api-keys/{}/rotate", key.id)) {
                                                input type="hidden" name="csrf" value=(csrf);
                                                button class="secondary compact" type="submit" { "Rotate" }
                                            }
                                            form method="post" action=(format!("/account/api-keys/{}/revoke", key.id)) {
                                                input type="hidden" name="csrf" value=(csrf);
                                                button class="secondary compact" type="submit" { "Revoke" }
                                            }
                                        }
                                    } @else {
                                        button type="button" class="secondary compact" disabled
                                            aria-disabled="true" { "Revoked" }
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

fn sessions_card(csrf: &str, sessions: &[SessionSummary]) -> Markup {
    html! {
        article class="auth-card account-card" {
            h2 { "Active sessions" }
            p { "These browsers are signed in to your account." }
            div class="table-wrap" {
                table class="data-table" {
                    thead {
                        tr {
                            th scope="col" { "Client" }
                            th scope="col" { "Started (UTC)" }
                            th scope="col" { "Last used (UTC)" }
                            th scope="col" class="col-actions" {
                                span class="visually-hidden" { "Actions" }
                            }
                        }
                    }
                    tbody {
                        @if sessions.is_empty() {
                            tr { td colspan="4" class="table-empty" { "No session is active." } }
                        }
                        @for session in sessions {
                            tr {
                                th scope="row" class="cell-title" {
                                    (client_label(session.user_agent.as_deref()))
                                    @if session.current { span class="status-tag" { "This session" } }
                                }
                                td class="cell-moment" { (moment(session.created_at)) }
                                td class="cell-moment" { (moment(session.last_seen_at)) }
                                td class="col-actions" {
                                    form method="post"
                                        action=(format!("/account/sessions/{}/revoke", session.id)) {
                                        input type="hidden" name="csrf" value=(csrf);
                                        button type="submit" class="secondary compact" {
                                            @if session.current { "Sign out" } @else { "Revoke" }
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
}

fn apps_card(csrf: &str, apps: &[ConnectedClient]) -> Markup {
    html! {
        article class="auth-card account-card" {
            h2 { "Connected apps" }
            p { "These applications can use your account." }
            div class="table-wrap" {
                table class="data-table" {
                    thead {
                        tr {
                            th scope="col" { "Application" }
                            th scope="col" { "Permissions" }
                            th scope="col" { "Connected (UTC)" }
                            th scope="col" { "Last used (UTC)" }
                            th scope="col" class="col-actions" {
                                span class="visually-hidden" { "Actions" }
                            }
                        }
                    }
                    tbody {
                        @if apps.is_empty() {
                            tr { td colspan="5" class="table-empty" { "No application is connected." } }
                        }
                        @for app in apps {
                            tr {
                                th scope="row" class="cell-title" {
                                    (app.client_name.as_deref().unwrap_or("Unnamed application"))
                                    code class="cell-subtitle" { (app.client_id) }
                                }
                                td { (super::permissions::scope_chips(&app.scope)) }
                                td class="cell-moment" { (moment(app.created_at)) }
                                td class="cell-moment" {
                                    @match app.last_used_at {
                                        Some(used) => (moment(used)),
                                        None => "never",
                                    }
                                }
                                td class="col-actions" {
                                    form method="post"
                                        action=(format!("/account/apps/{}/revoke", app.client_id)) {
                                        input type="hidden" name="csrf" value=(csrf);
                                        button type="submit" class="secondary compact" { "Disconnect" }
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

fn password_card(csrf: &str, notice: Option<&str>, error: Option<&str>) -> Markup {
    html! {
        article class="auth-card account-card" {
            h2 { "Change password" }
            p { "Use at least 8 characters, with 1 letter, 1 digit, and 1 special character." }
            @if let Some(notice) = notice {
                p class="auth-note" role="status" { (notice) }
            }
            @if let Some(error) = error {
                p class="auth-error" role="alert" { (error) }
            }
            form method="post" action="/account/password" {
                label for="current_password" { "Current password" }
                input type="password" id="current_password" name="current_password"
                    autocomplete="current-password" required;
                label for="new_password" { "New password" }
                input type="password" id="new_password" name="new_password"
                    autocomplete="new-password" required;
                input type="hidden" name="csrf" value=(csrf);
                button type="submit" { "Change password" }
            }
            p class="account-note" {
                "A password change ends your other sessions. Your connected apps keep working."
            }
        }
    }
}

fn last_used_text(value: Option<&str>) -> String {
    let Some(value) = value else {
        return "never".to_owned();
    };
    DateTime::parse_from_rfc3339(value)
        .map(|parsed| moment(parsed.with_timezone(&Utc)))
        .unwrap_or_else(|_| value.to_owned())
}

fn key_name_text(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        "(unnamed key)".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn provider_chips(value: &str) -> Markup {
    let providers =
        serde_json::from_str::<Vec<String>>(value).unwrap_or_else(|_| vec![value.to_owned()]);
    html! {
        @if providers.is_empty() {
            span class="cell-absent" { "none" }
        } @else {
            div class="chip-row" {
                @for provider in &providers { span class="chip" { (provider) } }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::client_label;

    #[test]
    fn labels_cover_the_common_clients() {
        for (agent, expected) in [
            (
                "Mozilla/5.0 (X11; Linux x86_64; rv:130.0) Gecko/20100101 Firefox/130.0",
                "Firefox on Linux",
            ),
            (
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36",
                "Chrome on Windows",
            ),
            (
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36 Edg/128.0.0.0",
                "Edge on Windows",
            ),
            (
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.6 Safari/605.1.15",
                "Safari on macOS",
            ),
            (
                "Mozilla/5.0 (iPhone; CPU iPhone OS 17_6 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.6 Mobile/15E148 Safari/604.1",
                "Safari on iOS",
            ),
            (
                "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Mobile Safari/537.36",
                "Chrome on Android",
            ),
            ("curl/8.9.1", "curl"),
            ("claude-code/1.0.0", "Claude"),
            ("node-fetch/3.3.2", "Node.js"),
            ("SomeRobot/1.0", "Unknown client"),
            ("SomeRobot/1.0 (Linux)", "Unknown client on Linux"),
        ] {
            assert_eq!(client_label(Some(agent)), expected, "{agent}");
        }
        assert_eq!(client_label(None), "Unknown client");
    }
}
