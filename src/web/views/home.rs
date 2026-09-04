use maud::{Markup, html};

use super::charts::{Series, line_chart, row_series_class, series_class, sparkline, stacked_bar};
use super::format::{count_text, money_text, token_text};
use super::{layout_with_nav, signed_in_nav};
use crate::usage::{LabeledSeries, Range, TotalsSeries, UsageGroup, UsageTotals};

/// Everything the overview needs, gathered by the handler in one place.
pub struct Overview<'a> {
    pub range: Range,
    pub totals: &'a UsageTotals,
    pub series: &'a TotalsSeries,
    pub models: &'a [UsageGroup],
    pub providers: &'a [UsageGroup],
    pub model_series: &'a [LabeledSeries],
    pub provider_series: &'a [LabeledSeries],
}

const TILE_SPARKLINE: (u32, u32) = (96, 28);
const ROW_SPARKLINE: (u32, u32) = (72, 18);

pub fn page(overview: &Overview<'_>) -> Markup {
    let totals = overview.totals;
    layout_with_nav(
        "Aegis",
        Some(signed_in_nav()),
        html! {
            main class="account-shell" {
                div class="page-head" {
                    h1 class="page-title" { "Overview" }
                    (range_switch(overview.range))
                }
                div class="tile-row" {
                    (tile(
                        "Requests",
                        &count_text(totals.requests),
                        &format!("{} requests", count_text(totals.requests)),
                        tile_sparkline(&overview.series.requests),
                        stacked_bar(
                            &[
                                ("ok", totals.succeeded as f64, "status-ok"),
                                ("failed", totals.failed as f64, "status-failed"),
                                ("unfinished", totals.unfinished() as f64, "status-unfinished"),
                            ],
                            &|value| count_text(value as i64),
                        ),
                    ))
                    (tile(
                        "Tokens",
                        &token_text(totals.tokens()),
                        &format!("{} tokens", count_text(totals.tokens())),
                        tile_sparkline(&overview.series.tokens),
                        stacked_bar(
                            &[
                                ("cache read", totals.cache_read_tokens as f64, "tokens-cache-read"),
                                ("uncached in", totals.input_tokens as f64, "tokens-input"),
                                ("cache write", totals.cache_write_tokens as f64, "tokens-cache-write"),
                                ("out", totals.output_tokens as f64, "tokens-output"),
                            ],
                            &|value| token_text(value as i64),
                        ),
                    ))
                    (tile(
                        "Cost",
                        &money_text(totals.cost_nanodollars),
                        &exact_money_text(totals.cost_nanodollars),
                        tile_sparkline(&overview.series.cost_nanodollars),
                        cost_by_model_bar(overview.models),
                    ))
                }
                div class="account-grid account-grid-even" {
                    (breakdown_card("Models", "Requests, tokens, and cost by the model each request asked for.", "Model", overview.models, overview.model_series))
                    (breakdown_card("Providers", "Requests, tokens, and cost by the provider account that served them.", "Provider", overview.providers, overview.provider_series))
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

/// The tile shows this many models by cost before the rest become "others".
const COST_SEGMENTS: usize = 3;

fn cost_by_model_bar(models: &[UsageGroup]) -> Markup {
    let mut by_cost: Vec<(String, i64)> = models
        .iter()
        .map(|row| (series_label(row.label.as_deref()), row.cost_nanodollars))
        .collect();
    by_cost.sort_by_key(|(_, cost)| std::cmp::Reverse(*cost));
    let mut segments: Vec<(String, f64, String)> = by_cost
        .iter()
        .take(COST_SEGMENTS)
        .enumerate()
        .map(|(index, (label, cost))| (label.clone(), *cost as f64, series_class(index)))
        .collect();
    if by_cost.len() > COST_SEGMENTS {
        let others: i64 = by_cost[COST_SEGMENTS..].iter().map(|(_, cost)| cost).sum();
        segments.push(("others".to_owned(), others as f64, "series-6".to_owned()));
    }
    let borrowed: Vec<(&str, f64, &str)> = segments
        .iter()
        .map(|(label, cost, class)| (label.as_str(), *cost, class.as_str()))
        .collect();
    stacked_bar(&borrowed, &|value| money_text(value as i64))
}

/// The full dollar figure for a tooltip, where the rounded tile value hides
/// fractions of a cent.
fn exact_money_text(nanodollars: i64) -> String {
    format!(
        "${:.4}",
        nanodollars as f64 / crate::pricing::NANODOLLARS_PER_DOLLAR
    )
}

fn tile(label: &str, value: &str, exact: &str, trend: Markup, detail: Markup) -> Markup {
    html! {
        article class="tile" {
            div class="tile-label" { (label) }
            div class="tile-headline" {
                div class="tile-value" title=(exact) { (value) }
                div class="tile-trend" { (trend) }
            }
            (detail)
        }
    }
}

fn tile_sparkline(values: &[i64]) -> Markup {
    sparkline(&as_f64(values), TILE_SPARKLINE.0, TILE_SPARKLINE.1)
}

fn as_f64(values: &[i64]) -> Vec<f64> {
    values.iter().map(|value| *value as f64).collect()
}

fn series_label(label: Option<&str>) -> String {
    label.unwrap_or("unspecified").to_owned()
}

fn chart_series(series: &[LabeledSeries]) -> Vec<Series> {
    series
        .iter()
        .map(|line| Series {
            label: series_label(line.label.as_deref()),
            values: as_f64(&line.tokens),
        })
        .collect()
}

fn breakdown_card(
    title: &str,
    blurb: &str,
    column: &str,
    rows: &[UsageGroup],
    series: &[LabeledSeries],
) -> Markup {
    // A row is matched to its line by label, not by position, so a tie broken
    // differently by the two queries cannot swap colours.
    let row_line = |label: Option<&str>| {
        series
            .iter()
            .position(|line| line.label.as_deref() == label)
            .map(|index| (row_series_class(index, series.len()), &series[index].tokens))
    };
    html! {
        article class="auth-card account-card" {
            h2 { (title) }
            p { (blurb) }
            (line_chart(&chart_series(series), &|value| token_text(value as i64)))
            div class="table-wrap" {
                table class="data-table" {
                    thead {
                        tr {
                            th scope="col" { (column) }
                            th scope="col" class="col-numeric" { "Requests" }
                            th scope="col" class="col-numeric" { "Tokens" }
                            th scope="col" class="col-numeric" { "Cost" }
                            th scope="col" class="col-trend" { span class="visually-hidden" { "Tokens over time" } }
                        }
                    }
                    tbody {
                        @if rows.is_empty() {
                            tr { td colspan="5" class="table-empty" { "No traffic in this range." } }
                        }
                        @for row in rows {
                            @let line = row_line(row.label.as_deref());
                            tr {
                                th scope="row" class="cell-title" title=(series_label(row.label.as_deref())) {
                                    @if let Some((class, _)) = &line {
                                        span class=(format!("row-swatch {class}")) aria-hidden="true" {}
                                    }
                                    @match row.label.as_deref() {
                                        Some(label) => (label),
                                        None => span class="cell-absent" { "unspecified" },
                                    }
                                }
                                td class="col-numeric" { (count_text(row.requests)) }
                                td class="col-numeric" { (token_text(row.tokens)) }
                                td class="col-numeric" { (money_text(row.cost_nanodollars)) }
                                td class="col-trend" {
                                    @if let Some((class, tokens)) = &line {
                                        span class=(class) {
                                            (sparkline(&as_f64(tokens), ROW_SPARKLINE.0, ROW_SPARKLINE.1))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_overview_renders_with_and_without_traffic() {
        let totals = UsageTotals {
            requests: 3,
            succeeded: 2,
            failed: 1,
            input_tokens: 10,
            cache_read_tokens: 20,
            cache_write_tokens: 0,
            output_tokens: 5,
            cost_nanodollars: 1_500_000,
            unpriced: 1,
        };
        let series = TotalsSeries {
            requests: vec![1, 2, 0, 0, 0, 0, 0, 0],
            tokens: vec![10, 25, 0, 0, 0, 0, 0, 0],
            cost_nanodollars: vec![0, 1_500_000, 0, 0, 0, 0, 0, 0],
        };
        let models = vec![UsageGroup {
            label: None,
            requests: 3,
            tokens: 35,
            cost_nanodollars: 1_500_000,
        }];
        let model_series = vec![LabeledSeries {
            label: None,
            tokens: vec![10, 25, 0, 0, 0, 0, 0, 0],
        }];
        let page = page(&Overview {
            range: Range::Week,
            totals: &totals,
            series: &series,
            models: &models,
            providers: &[],
            model_series: &model_series,
            provider_series: &[],
        })
        .into_string();
        assert!(
            page.contains(r#"<span class="range-option is-current" aria-current="page">7d</span>"#)
        );
        assert!(page.contains(r#"href="/?range=24h""#));
        assert!(!page.contains("unpriced"), "the cost tile carries no note");
        assert!(!page.contains("chart-legend"), "the table is the key");
        assert!(
            !page.contains(r#"text-anchor="middle""#),
            "no x axis labels"
        );
        assert!(
            page.contains(r#"<span class="row-swatch series-1" aria-hidden="true"></span><span class="cell-absent">unspecified</span>"#),
            "the row carries its line's colour"
        );
        assert!(page.contains(r#"<span class="series-1"><svg class="sparkline""#));
        assert!(
            page.contains(r#"<div class="tile-value" title="3 requests">3</div>"#),
            "{page}"
        );
        assert!(page.contains(r#"title="35 tokens""#));
        assert!(page.contains(r#"title="$0.0015""#));
        assert!(
            page.contains(r#"<th scope="row" class="cell-title" title="unspecified">"#),
            "the label cell names its full label"
        );
        assert!(
            page.contains(r#"<li title="$0.0015 unspecified">"#),
            "the legend entry carries its full text"
        );
        assert!(
            page.contains(r#"<rect class="status-ok" x="0" y="0" width="66.67" height="8"><title>2 ok</title></rect>"#),
            "{page}"
        );
        assert!(page.contains(r#"<span class="legend-label">"#));
        assert!(
            page.contains("No traffic in this range."),
            "the empty provider table says so"
        );
        assert_eq!(
            page.matches(r#"class="sparkline""#).count(),
            3 + 1,
            "three tiles and one table row"
        );
        assert_eq!(page.matches("chart-line").count(), 1, "one model line");
        assert!(!page.contains("<polyline"), "curves, not pointy lines");
        assert!(
            page.contains(r#"<span class="legend-value">$0.0015</span> <span class="legend-label">unspecified</span>"#),
            "the cost tile lists the model's cost"
        );

        let empty = page_without_traffic().into_string();
        assert!(empty.contains("class=\"chart-empty\""));
    }

    #[test]
    fn series_classes_colour_without_filling() {
        let styles = include_str!("assets/styles.css");
        for rule in styles.lines().filter(|line| {
            line.starts_with(".series-")
                || line.starts_with(".status-")
                || line.starts_with(".tokens-")
        }) {
            assert!(!rule.contains("fill"), "{rule}");
        }
        assert!(styles.contains(".sparkline path {\n  fill: none;"));
    }

    #[test]
    fn the_cost_bar_keeps_the_top_three_models_and_sums_the_rest() {
        let models: Vec<UsageGroup> = [("a", 5), ("b", 40), ("c", 10), ("d", 20), ("e", 1)]
            .into_iter()
            .map(|(label, cost)| UsageGroup {
                label: Some(label.to_owned()),
                requests: 1,
                tokens: 1,
                cost_nanodollars: cost * 10_000_000,
            })
            .collect();
        let bar = cost_by_model_bar(&models).into_string();
        assert_eq!(
            bar.matches(r#"<span class="legend-label">"#).count(),
            4,
            "three models plus others"
        );
        assert!(
            bar.contains(
                r#"<span class="legend-value">$0.40</span> <span class="legend-label">b</span>"#
            ),
            "{bar}"
        );
        assert!(bar.contains(r#"<span class="legend-value">$0.06</span> <span class="legend-label">others</span>"#), "{bar}");
        assert!(bar.contains(r#"class="swatch series-6""#));
        assert_eq!(
            cost_by_model_bar(&models[..3])
                .into_string()
                .matches("others")
                .count(),
            0,
            "three models need no others segment"
        );
    }

    fn page_without_traffic() -> Markup {
        page(&Overview {
            range: Range::Week,
            totals: &UsageTotals::default(),
            series: &TotalsSeries {
                requests: vec![0; 8],
                tokens: vec![0; 8],
                cost_nanodollars: vec![0; 8],
            },
            models: &[],
            providers: &[],
            model_series: &[],
            provider_series: &[],
        })
    }
}
