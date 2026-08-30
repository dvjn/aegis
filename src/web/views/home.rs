use maud::{Markup, html};

use super::{layout_with_nav, signed_in_nav};
use crate::usage::{Range, UsageGroup, UsageTotals};

pub fn page(
    range: Range,
    totals: &UsageTotals,
    models: &[UsageGroup],
    providers: &[UsageGroup],
) -> Markup {
    layout_with_nav(
        "Aegis",
        Some(signed_in_nav()),
        html! {
            main class="account-shell" {
                div class="page-head" {
                    h1 class="page-title" { "Overview" }
                    (range_switch(range))
                }
                div class="tile-row" {
                    (tile("Requests", &count_text(totals.requests), Some(&outcome_text(totals))))
                    (tile(
                        "Tokens",
                        &token_text(totals.tokens()),
                        Some(&format!(
                            "{} uncached in · {} cache read · {} cache write · {} out",
                            token_text(totals.input_tokens),
                            token_text(totals.cache_read_tokens),
                            token_text(totals.cache_write_tokens),
                            token_text(totals.output_tokens)
                        )),
                    ))
                }
                div class="account-grid account-grid-even" {
                    (breakdown_card("Models", "Requests and tokens by the model each request asked for.", "Model", models))
                    (breakdown_card("Providers", "Requests and tokens by the provider account that served them.", "Provider", providers))
                }
            }
        },
    )
}

fn range_switch(current: Range) -> Markup {
    html! {
        nav class="range-switch" aria-label="Reporting range" {
            @for option in Range::ALL {
                @if option == current {
                    span class="range-option is-current" aria-current="page" { (option.label()) }
                } @else {
                    a class="range-option" href=(format!("/?range={}", option.slug())) {
                        (option.label())
                    }
                }
            }
        }
    }
}

fn outcome_text(totals: &UsageTotals) -> String {
    let mut parts = vec![
        format!("{} ok", count_text(totals.succeeded)),
        format!("{} failed", count_text(totals.failed)),
    ];
    let unfinished = totals.unfinished();
    if unfinished > 0 {
        parts.push(format!("{} unfinished", count_text(unfinished)));
    }
    parts.join(" · ")
}

fn tile(label: &str, value: &str, note: Option<&str>) -> Markup {
    html! {
        article class="tile" {
            div class="tile-label" { (label) }
            div class="tile-value" { (value) }
            @if let Some(note) = note {
                div class="tile-note" { (note) }
            }
        }
    }
}

fn breakdown_card(title: &str, blurb: &str, column: &str, rows: &[UsageGroup]) -> Markup {
    html! {
        article class="auth-card account-card" {
            h2 { (title) }
            p { (blurb) }
            div class="table-wrap" {
                table class="data-table" {
                    thead {
                        tr {
                            th scope="col" { (column) }
                            th scope="col" class="col-numeric" { "Requests" }
                            th scope="col" class="col-numeric" { "Tokens" }
                        }
                    }
                    tbody {
                        @if rows.is_empty() {
                            tr { td colspan="3" class="table-empty" { "No traffic in this range." } }
                        }
                        @for row in rows {
                            tr {
                                th scope="row" class="cell-title" {
                                    @match row.label.as_deref() {
                                        Some(label) => (label),
                                        None => span class="cell-absent" { "unspecified" },
                                    }
                                }
                                td class="col-numeric" { (count_text(row.requests)) }
                                td class="col-numeric" { (token_text(row.tokens)) }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn count_text(value: i64) -> String {
    let digits = value.abs().to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    if value < 0 {
        format!("-{grouped}")
    } else {
        grouped
    }
}

fn token_text(value: i64) -> String {
    match value {
        0 => "0".to_owned(),
        value if value < 10_000 => count_text(value),
        value if value < 1_000_000 => format!("{:.1}k", value as f64 / 1_000.0),
        value => format!("{:.1}M", value as f64 / 1_000_000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::{count_text, token_text};

    #[test]
    fn numbers_read_as_summaries() {
        for (value, expected) in [(0, "0"), (42, "42"), (1_284, "1,284"), (350_709, "350,709")] {
            assert_eq!(count_text(value), expected, "count {value}");
        }
        for (value, expected) in [
            (0, "0"),
            (9_999, "9,999"),
            (10_000, "10.0k"),
            (350_709, "350.7k"),
            (4_200_000, "4.2M"),
        ] {
            assert_eq!(token_text(value), expected, "tokens {value}");
        }
    }
}
