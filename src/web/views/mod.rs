pub mod account;
pub mod authorize;
pub mod charts;
pub mod device;
pub mod format;
pub mod home;
pub mod login;
pub mod permissions;
pub mod register;
pub mod reset;
pub mod tools;

use maud::{DOCTYPE, Markup, PreEscaped, html};
use std::sync::LazyLock;

include!(concat!(env!("OUT_DIR"), "/browser_asset_paths.rs"));

pub static FONT_STYLE: LazyLock<String> = LazyLock::new(|| {
    format!(
        concat!(
            "@font-face{{",
            "font-family:\"Inter Variable\";",
            "font-style:normal;",
            "font-display:swap;",
            "font-weight:100 900;",
            "src:url(\"{}\") format(\"woff2-variations\");",
            "unicode-range:U+0000-00FF,U+0131,U+0152-0153,U+02BB-02BC,U+02C6,U+02DA,U+02DC,",
            "U+0304,U+0308,U+0329,U+2000-206F,U+20AC,U+2122,U+2191,U+2193,U+2212,U+2215,",
            "U+FEFF,U+FFFD;",
            "}}"
        ),
        FONT_PATH
    )
});

pub fn signed_in_nav() -> Markup {
    html! {
        a class="nav-icon" href="/account" role="button" aria-label="Account" title="Account" {
            svg viewBox="0 0 24 24" width="18" height="18" aria-hidden="true" focusable="false"
                fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" {
                circle cx="12" cy="8.5" r="3.5" {}
                path d="M4.5 19.5c1.6-3.4 4.3-5 7.5-5s5.9 1.6 7.5 5" {}
            }
        }
    }
}

pub fn layout(title: &str, body: Markup) -> Markup {
    layout_with_nav(title, None, body)
}

pub fn layout_with_nav(title: &str, nav_action: Option<Markup>, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="color-scheme" content="dark";
                title { (title) }
                link rel="icon" href="data:,";
                style { (PreEscaped(FONT_STYLE.as_str())) }
                link rel="stylesheet" href=(STYLES_PATH);
            }
            body {
                div class="site-shell" {
                    header class="site" {
                        div class="container" {
                            nav class="site-nav" aria-label="Primary navigation" {
                                a class="site-title" href="/" aria-label="Aegis home" { "Aegis" }
                                @if let Some(nav_action) = nav_action {
                                    div class="nav-auth" { (nav_action) }
                                }
                            }
                        }
                    }
                    (body)
                }
            }
        }
    }
}
