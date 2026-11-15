use maud::{Markup, html};

use super::layout;

pub fn page(csrf: &str, email: &str, error: Option<&str>) -> Markup {
    layout(
        "Create Account · Aegis",
        html! {
            main class="auth-shell" {
                article class="auth-card" {
                    h1 { "Create your account" }
                    p { "Sign up with your email address to use Aegis." }
                    @if let Some(error) = error {
                        p class="auth-error" role="alert" { (error) }
                    }
                    form method="post" action="/register" {
                        label for="email" { "Email" }
                        input type="email" id="email" name="email" value=(email)
                            autocomplete="username" autocapitalize="none" spellcheck="false"
                            required autofocus;
                        label for="password" { "Password" }
                        input type="password" id="password" name="password"
                            autocomplete="new-password" required;
                        input type="hidden" name="csrf" value=(csrf);
                        button type="submit" { "Create account" }
                    }
                    p class="auth-alternate" {
                        "Already have an account? " a href="/login" { "Sign in" }
                    }
                }
            }
        },
    )
}

pub fn sent(csrf: &str, email: &str, error: bool) -> Markup {
    layout(
        "Check Your Email · Aegis",
        html! {
            main class="auth-shell" {
                article class="auth-card" {
                    h1 { "Check your email" }
                    p { "If the address is new, Aegis sent a 6-digit code to it. Enter the code below." }
                    @if error {
                        p class="auth-error" role="alert" { "That code is invalid or expired." }
                    }
                    form method="post" action="/verify" {
                        label for="code" { "6-digit code" }
                        input type="text" id="code" name="code" required autofocus
                            inputmode="numeric" pattern="[0-9]{6}" maxlength="6"
                            autocomplete="one-time-code" autocapitalize="none" spellcheck="false";
                        input type="hidden" name="email" value=(email);
                        input type="hidden" name="csrf" value=(csrf);
                        button type="submit" { "Verify email" }
                    }
                    p class="auth-alternate" {
                        "Already have an account? " a href="/login" { "Sign in" }
                    }
                }
            }
        },
    )
}
