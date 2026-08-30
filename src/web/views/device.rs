use super::layout;
use super::permissions::{
    account_entry, application_group, detail_row, permission_group, switch_form,
};
use maud::{Markup, html};

pub fn entry(csrf: &str, invalid: bool) -> Markup {
    layout(
        "Connect a Device · Aegis",
        html! {
            main class="auth-shell" { article class="auth-card" {
                h1 { "Connect a device" }
                p { "Enter the code shown by your app or device." }
                @if invalid { p class="auth-error" role="alert" { "That code is invalid or expired." } }
                form method="post" action="/oauth/device" {
                    input type="hidden" name="csrf" value=(csrf);
                    label for="user_code" { "Device code" }
                    input id="user_code" name="user_code" type="text" required maxlength="14"
                        autocomplete="one-time-code" autocapitalize="characters" spellcheck="false" placeholder="XXXX-XXXX-XXXX";
                    button type="submit" { "Continue" }
                }
            } }
        },
    )
}

pub struct Consent<'a> {
    pub csrf: &'a str,
    pub email: &'a str,
    pub code: &'a str,
    pub client_name: &'a str,
    pub client_id: &'a str,
    pub scope: &'a str,
    pub role: &'a str,
    pub resource: &'a str,
    pub switch_to: &'a str,
}

pub fn consent(
    Consent {
        csrf,
        email,
        code,
        client_name,
        client_id,
        scope,
        role,
        resource,
        switch_to,
    }: Consent,
) -> Markup {
    layout(
        "Connect Device · Aegis",
        html! {
            main class="auth-shell" { form method="post" {
                input type="hidden" name="csrf" value=(csrf);
                article class="auth-card consent-card" {
                    div class="card-head" {
                        h1 { "Connect this device?" }
                        p { "Confirm that the code below matches the app or device you are connecting." }
                    }
                    p class="user-code" aria-label="Device code" { (code) }
                    (account_entry(email))
                    (application_group(html! {
                        (detail_row("Name", html! { strong { (client_name) } }))
                        (detail_row("Client ID", html! { code { (client_id) } }))
                        (detail_row("Resource", html! { code { (resource) } }))
                    }))
                    (permission_group(scope, role))
                    div class="consent-actions" {
                        button type="submit" name="decision" value="deny" class="secondary" { "Deny" }
                        button type="submit" name="decision" value="allow" { "Allow" }
                    }
                }
            }
            (switch_form(csrf, switch_to)) }
        },
    )
}

pub fn terminal(approved: bool) -> Markup {
    let (title, message) = if approved {
        ("Device connected", "You can return to your app or device.")
    } else {
        ("Connection denied", "The app or device was not connected.")
    };
    layout(
        "Device Confirmation · Aegis",
        html! {
            main class="auth-shell" { article class="auth-card" {
                h1 { (title) }
                p { (message) }
                a href="/" role="button" class="secondary" { "Done" }
            } }
        },
    )
}
