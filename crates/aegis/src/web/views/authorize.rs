use maud::{Markup, html};

use super::{
    layout,
    permissions::{account_entry, application_group, detail_row, permission_group, switch_form},
};

pub struct Consent<'a> {
    pub csrf: &'a str,
    pub email: &'a str,
    pub client_name: &'a str,
    pub client_id: &'a str,
    pub scope: &'a str,
    pub role: &'a str,
    pub resource: &'a str,
    pub redirect_uri: &'a str,
    pub switch_to: &'a str,
}

pub fn page(
    Consent {
        csrf,
        email,
        client_name,
        client_id,
        scope,
        role,
        resource,
        redirect_uri,
        switch_to,
    }: Consent,
) -> Markup {
    layout(
        "Authorize · Aegis",
        html! {
            main class="auth-shell" {
                form method="post" {
                    input type="hidden" name="csrf" value=(csrf);
                    article class="auth-card consent-card" {
                        div class="card-head" {
                            h1 { "Authorize access" }
                            p { (client_name) " wants access to your Aegis account." }
                        }
                        (account_entry(email))
                        (application_group(html! {
                            (detail_row("Name", html! { strong { (client_name) } }))
                            (detail_row("Client ID", html! { code { (client_id) } }))
                            (detail_row("Callback", html! { code { (redirect_uri) } }))
                            (detail_row("Resource", html! { code { (resource) } }))
                        }))
                        (permission_group(scope, role))
                        div class="consent-actions" {
                            button type="submit" name="decision" value="deny" class="secondary" {
                                "Cancel"
                            }
                            button type="submit" name="decision" value="allow" {
                                "Authorize"
                            }
                        }
                    }
                }
                (switch_form(csrf, switch_to))
            }
        },
    )
}
