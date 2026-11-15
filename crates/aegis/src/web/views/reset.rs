use maud::{Markup, html};

use super::layout;

pub fn page(csrf: &str, email: &str) -> Markup {
    layout(
        "Reset Password · Aegis",
        html! {
            main class="auth-shell" {
                article class="auth-card" {
                    h1 { "Reset your password" }
                    p { "Enter your email address. Aegis sends a 6-digit code to it." }
                    form method="post" action="/reset" {
                        label for="email" { "Email" }
                        input type="email" id="email" name="email" value=(email)
                            autocomplete="username" autocapitalize="none" spellcheck="false"
                            required autofocus;
                        input type="hidden" name="csrf" value=(csrf);
                        button type="submit" { "Send code" }
                    }
                    p class="auth-alternate" {
                        "Remembered it? " a href="/login" { "Sign in" }
                    }
                }
            }
        },
    )
}

pub fn sent(csrf: &str, email: &str, error: Option<&str>) -> Markup {
    layout(
        "Check Your Email · Aegis",
        html! {
            main class="auth-shell" {
                article class="auth-card" {
                    h1 { "Check your email" }
                    p { "If the address has an account, Aegis sent a 6-digit code to it. Enter the code and a new password." }
                    @if let Some(error) = error {
                        p class="auth-error" role="alert" { (error) }
                    }
                    form method="post" action="/reset/confirm" {
                        label for="code" { "6-digit code" }
                        input type="text" id="code" name="code" required autofocus
                            inputmode="numeric" pattern="[0-9]{6}" maxlength="6"
                            autocomplete="one-time-code" autocapitalize="none" spellcheck="false";
                        label for="password" { "New password" }
                        input type="password" id="password" name="password"
                            autocomplete="new-password" required;
                        label for="password_confirm" { "Repeat new password" }
                        input type="password" id="password_confirm" name="password_confirm"
                            autocomplete="new-password" required;
                        input type="hidden" name="email" value=(email);
                        input type="hidden" name="csrf" value=(csrf);
                        button type="submit" { "Reset password" }
                    }
                    p class="auth-alternate" {
                        "Remembered it? " a href="/login" { "Sign in" }
                    }
                }
            }
        },
    )
}
