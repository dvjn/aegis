use maud::{Markup, html};

use super::layout;

pub fn page(
    csrf: &str,
    email: &str,
    return_to: Option<&str>,
    error: bool,
    registration_enabled: bool,
    notice: Option<&str>,
) -> Markup {
    layout(
        "Sign In · Aegis",
        html! {
            main class="auth-shell" {
                article class="auth-card" {
                    h1 { "Welcome back" }
                    p { "Sign in to continue to Aegis." }
                    @if let Some(notice) = notice {
                        p class="auth-note" role="status" { (notice) }
                    }
                    @if error {
                        p class="auth-error" role="alert" { "Invalid email or password." }
                    }
                    form method="post" action="/login" {
                        label for="email" { "Email" }
                        input type="email" id="email" name="email" value=(email)
                            autocomplete="username" autocapitalize="none" spellcheck="false"
                            required autofocus;
                        label for="password" { "Password" }
                        input type="password" id="password" name="password"
                            autocomplete="current-password" required;
                        input type="hidden" name="csrf" value=(csrf);
                        @if let Some(return_to) = return_to {
                            input type="hidden" name="return_to" value=(return_to);
                        }
                        button type="submit" { "Sign in" }
                    }
                    p class="auth-alternate" {
                        a href="/reset" { "Forgot your password?" }
                    }
                    @if registration_enabled {
                        p class="auth-alternate" {
                            "Don't have an account? " a href="/register" { "Register" }
                        }
                    }
                }
            }
        },
    )
}
