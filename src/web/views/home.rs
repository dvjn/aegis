use maud::{Markup, html};

use super::charts::{
    Series, line_chart, row_series_class, series_class, sparkline, stacked_bar,
    stacked_bar_without_legend,
};
use super::format::{count_text, money_text, percent_text, token_text};
use super::{layout_with_nav, signed_in_nav, tools};
use crate::usage::{
    ContextTotals, GuardrailsSummary, LabeledSeries, Range, ToolUsage, TotalsSeries, UsageGroup,
    UsageTotals,
};

/// Everything the overview needs, gathered by the handler in one place.
pub struct Overview<'a> {
    pub range: Range,
    pub totals: &'a UsageTotals,
    pub context: &'a ContextTotals,
    pub tools: &'a ToolUsage,
    pub series: &'a TotalsSeries,
    pub models: &'a [UsageGroup],
    pub providers: &'a [UsageGroup],
    pub keys: &'a [UsageGroup],
    pub model_series: &'a [LabeledSeries],
    pub provider_series: &'a [LabeledSeries],
    pub key_series: &'a [LabeledSeries],
    pub guardrails: &'a GuardrailsSummary,
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
                div class="overview-grid" {
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
                    (breakdown_card("Models", "Requests, tokens, and cost by the model each request asked for.", "Model", "grid-half rows-3", overview.models, overview.model_series))
                    (breakdown_card("Providers", "Requests, tokens, and cost by the provider account that served them.", "Provider", "grid-half rows-2", overview.providers, overview.provider_series))
                    (breakdown_card("Keys", "Requests, tokens, and cost by the key that sent them.", "Key", "grid-half rows-2", overview.keys, overview.key_series))
                    (context_card(overview.context))
                    (tools::mcp_card(overview.tools))
                    (tools::calls_card(overview.tools))
                    (tools::skills_card(overview.tools))
                    (guardrails_card(overview.guardrails))
                }
            }
        },
    )
}

/// One part of the request payload: its label, bytes, apportioned cost, and
/// colour class.
struct ContextPart {
    label: &'static str,
    bytes: i64,
    cost_nanodollars: i64,
    class: &'static str,
}

/// What a turn is built from, in the order it reaches the model, in one fixed
/// order whatever the sizes, so the same row is in the same place on every
/// visit.
fn context_parts(context: &ContextTotals) -> [ContextPart; 8] {
    let part = |label, bytes, cost_nanodollars, class| ContextPart {
        label,
        bytes,
        cost_nanodollars,
        class,
    };
    [
        part(
            "system",
            context.system_bytes,
            context.system_cost_nanodollars,
            "context-system",
        ),
        part(
            "tool definition",
            context.tool_definition_bytes,
            context.tool_definition_cost_nanodollars,
            "context-tool-definitions",
        ),
        part(
            "user",
            context.user_text_bytes,
            context.user_text_cost_nanodollars,
            "context-user",
        ),
        part(
            "thinking",
            context.thinking_bytes,
            context.thinking_cost_nanodollars,
            "context-thinking",
        ),
        part(
            "assistant",
            context.assistant_text_bytes,
            context.assistant_text_cost_nanodollars,
            "context-assistant",
        ),
        part(
            "tool call",
            context.tool_use_bytes,
            context.tool_use_cost_nanodollars,
            "context-tool-calls",
        ),
        part(
            "tool result",
            context.tool_result_bytes,
            context.tool_result_cost_nanodollars,
            "context-tool-results",
        ),
        part(
            "other",
            context.other_bytes,
            context.other_cost_nanodollars,
            "context-other",
        ),
    ]
}

fn context_card(context: &ContextTotals) -> Markup {
    let parts = context_parts(context);
    let estimated = |bytes: i64| token_text(ContextTotals::estimated_tokens(bytes));
    let segments: Vec<(&str, f64, &str)> = parts
        .iter()
        .map(|part| (part.label, part.bytes as f64, part.class))
        .collect();
    html! {
        article class="auth-card account-card grid-half rows-2" {
            h2 { "Context" }
            p { "Estimated tokens in request payloads by part." }
            (stacked_bar_without_legend(&segments, &|value| estimated(value as i64)))
            div class="table-wrap" {
                table class="data-table" {
                    thead {
                        tr {
                            th scope="col" { "Part" }
                            th scope="col" class="col-numeric" { "Tokens" }
                            th scope="col" class="col-numeric" { "Share" }
                            th scope="col" class="col-numeric" { "Cost" }
                        }
                    }
                    tbody {
                        @for part in &parts {
                            tr {
                                th scope="row" class="cell-title" title=(part.label) {
                                    span class=(format!("row-swatch {}", part.class)) aria-hidden="true" {}
                                    (part.label)
                                }
                                td class="col-numeric" { (estimated(part.bytes)) }
                                td class="col-numeric" { (percent_text(part.bytes, context.total_bytes)) }
                                td class="col-numeric" { (money_text(part.cost_nanodollars)) }
                            }
                        }
                    }
                }
            }
        }
    }
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

/// `placement` is the card's span classes in the overview grid.
fn breakdown_card(
    title: &str,
    blurb: &str,
    column: &str,
    placement: &str,
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
        article class=(format!("auth-card account-card {placement}")) {
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

fn guardrails_card(summary: &GuardrailsSummary) -> Markup {
    html! {
        article class="auth-card account-card" {
            h2 { "Guardrails" }
            p { "Requests the secrets policy scanned, and the secrets it masked before they left." }
            div class="table-wrap" {
                table class="data-table" {
                    thead {
                        tr {
                            th scope="col" { "Measure" }
                            th scope="col" class="col-numeric" { "Count" }
                        }
                    }
                    tbody {
                        @if summary.is_empty() {
                            tr { td colspan="2" class="table-empty" { "No guardrail activity in this range." } }
                        } @else {
                            tr {
                                th scope="row" class="cell-title" { "Requests scanned" }
                                td class="col-numeric" { (count_text(summary.requests_scanned)) }
                            }
                            tr {
                                th scope="row" class="cell-title" { "Requests masked" }
                                td class="col-numeric" { (count_text(summary.requests_masked)) }
                            }
                            tr {
                                th scope="row" class="cell-title" { "Placeholders substituted" }
                                td class="col-numeric" { (count_text(summary.placeholders_substituted)) }
                            }
                            @for detector in &summary.detectors {
                                tr {
                                    th scope="row" class="cell-title" title=(detector.detector) {
                                        span class="cell-absent" { "detector" } " " (detector.detector)
                                    }
                                    td class="col-numeric" { (count_text(detector.matches)) }
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
    use crate::usage::DetectorCount;

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
            context: &ContextTotals::default(),
            tools: &ToolUsage::default(),
            series: &series,
            models: &models,
            providers: &[],
            keys: &[],
            model_series: &model_series,
            provider_series: &[],
            key_series: &[],
            guardrails: &GuardrailsSummary::default(),
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
    fn the_keys_card_names_the_key_and_nothing_about_its_versions() {
        let keys = vec![UsageGroup {
            label: Some("agent".to_owned()),
            requests: 4,
            tokens: 120,
            cost_nanodollars: 2_000_000,
        }];
        let key_series = vec![LabeledSeries {
            label: Some("agent".to_owned()),
            tokens: vec![20, 100, 0, 0, 0, 0, 0, 0],
        }];
        let page = page(&Overview {
            range: Range::Week,
            totals: &UsageTotals::default(),
            context: &ContextTotals::default(),
            tools: &ToolUsage::default(),
            series: &TotalsSeries {
                requests: vec![0; 8],
                tokens: vec![0; 8],
                cost_nanodollars: vec![0; 8],
            },
            models: &[],
            providers: &[],
            keys: &keys,
            model_series: &[],
            provider_series: &[],
            key_series: &key_series,
            guardrails: &GuardrailsSummary::default(),
        })
        .into_string();
        assert!(page.contains(r#"<h2>Keys</h2>"#), "{page}");
        assert!(page.contains(r#"<th scope="col">Key</th>"#));
        assert!(
            page.contains(r#"<th scope="row" class="cell-title" title="agent">"#),
            "the row names the key"
        );
        for absent in ["version", "Version", "prefix", "Prefix", "sk-"] {
            assert!(!page.contains(absent), "{absent} leaked into the page");
        }
        assert!(
            page.contains(
                r#"<article class="auth-card account-card grid-half rows-2"><h2>Keys</h2>"#
            ),
            "Keys takes half the columns and two row units"
        );
    }

    #[test]
    fn the_overview_is_one_twelve_column_grid_with_a_fixed_row_unit() {
        let styles = include_str!("assets/styles.css");
        assert!(styles.contains(
            ".overview-grid {\n  display: grid;\n  grid-template-columns: repeat(12, minmax(0, 1fr));\n  grid-template-rows: auto;\n  grid-auto-rows: 172px;"
        ));
        assert!(
            styles.contains(".overview-grid > .tile {\n  grid-column: span 4;\n  grid-row: 1;")
        );
        assert!(styles.contains(".grid-half {\n  grid-column: span 6;"));
        assert!(styles.contains(".rows-3 {\n  grid-row: span 3;"));
        assert!(styles.contains(
            ".overview-grid > .account-card > .table-wrap {\n  flex: 1;\n  min-height: 0;\n  overflow-y: auto;"
        ));
        assert!(
            styles.contains(
                ".overview-grid > .account-card thead th {\n  position: sticky;\n  top: 0;"
            )
        );

        let page = page_without_traffic().into_string();
        assert_eq!(page.matches(r#"<article class="tile">"#).count(), 3);
        assert!(
            page.contains(
                r#"<article class="auth-card account-card grid-half rows-3"><h2>Models</h2>"#
            ),
            "Models is three units tall beside Providers and Keys at two each: {page}"
        );
        assert!(page.contains(
            r#"<article class="auth-card account-card grid-half rows-2"><h2>Providers</h2>"#
        ));
        assert!(
            !page.contains("account-column"),
            "no column wrappers: the grid places every card"
        );
        assert!(!page.contains("tile-row"));
        let order: Vec<usize> = [
            r#"grid-half rows-3"><h2>Models</h2>"#,
            r#"grid-half rows-2"><h2>Providers</h2>"#,
            r#"grid-half rows-2"><h2>Keys</h2>"#,
            r#"grid-half rows-2"><h2>Context</h2>"#,
            r#"grid-half rows-2"><h2>MCPs</h2>"#,
            r#"grid-half rows-3"><h2>Tools</h2>"#,
            r#"grid-half rows-2"><h2>Skills</h2>"#,
        ]
        .iter()
        .map(|card| {
            page.find(card)
                .unwrap_or_else(|| panic!("{card} in {page}"))
        })
        .collect();
        assert!(
            order.windows(2).all(|pair| pair[0] < pair[1]),
            "auto-placement in this order puts Models (3) over Context (2) on the left, Providers (2), Keys (2), Skills (2) on the right, Tools on the left beside the slot Skills leaves, then MCP servers across the row: {order:?}"
        );
    }

    #[test]
    fn series_classes_colour_without_filling() {
        let styles = include_str!("assets/styles.css");
        for rule in styles.lines().filter(|line| {
            line.starts_with(".series-")
                || line.starts_with(".status-")
                || line.starts_with(".tokens-")
                || line.starts_with(".context-")
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

    #[test]
    fn the_context_card_keeps_every_part_in_one_fixed_order() {
        let context = ContextTotals {
            requests: 4,
            tool_definition_bytes: 3_228_000,
            system_bytes: 1_000_000,
            user_text_bytes: 500_000,
            assistant_text_bytes: 400_000,
            thinking_bytes: 253_000,
            tool_use_bytes: 619_000,
            tool_result_bytes: 2_000_000,
            other_bytes: 0,
            total_bytes: 8_000_000,
            tools_offered: 40,
            tools_invoked: 6,
            tool_result_errors: 1,
            cache_breakpoints: 8,
            tool_definition_cost_nanodollars: 40_350_000,
            system_cost_nanodollars: 12_500_000,
            user_text_cost_nanodollars: 6_250_000,
            assistant_text_cost_nanodollars: 5_000_000,
            thinking_cost_nanodollars: 3_162_500,
            tool_use_cost_nanodollars: 7_737_500,
            tool_result_cost_nanodollars: 25_000_000,
            other_cost_nanodollars: 0,
        };
        let card = context_card(&context).into_string();
        assert!(
            card.contains(
                r#"<h2>Context</h2><p>Estimated tokens in request payloads by part.</p>"#
            ),
            "{card}"
        );
        let order: Vec<usize> = [
            "system",
            "tool definition",
            "user",
            "thinking",
            "assistant",
            "tool call",
            "tool result",
            "other",
        ]
        .iter()
        .map(|label| card.find(&format!(r#"title="{label}""#)).unwrap())
        .collect();
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{order:?}");
        assert!(
            card.contains(r#"<rect class="context-tool-definitions" x="12.5" y="0" width="40.35" height="8"><title>914.7k tool definition</title></rect>"#),
            "{card}"
        );
        assert!(!card.contains("stacked-legend"), "the table is the key");
        assert!(!card.contains("MB"), "sizes read as tokens, never bytes");
        assert!(
            !card.contains("est."),
            "the estimate is not repeated in every header"
        );
        assert!(
            card.contains(r#"<th scope="col" class="col-numeric">Tokens</th><th scope="col" class="col-numeric">Share</th><th scope="col" class="col-numeric">Cost</th>"#)
        );
        assert!(
            card.contains(r#"<td class="col-numeric">566.7k</td><td class="col-numeric">25.0%</td><td class="col-numeric">$0.03</td>"#),
            "{card}"
        );
        assert!(
            card.contains(r#"<span class="row-swatch context-other" aria-hidden="true"></span>other</th><td class="col-numeric">0</td><td class="col-numeric">0.0%</td><td class="col-numeric">$0.00</td>"#),
            "an empty part keeps its row: {card}"
        );
        for absent in ["tool-related", "requests", "bytes per token"] {
            assert!(!card.contains(absent), "{absent} is a derived sentence");
        }

        let page = page_without_traffic().into_string();
        assert!(
            page.contains(r#"<h2>Keys</h2>"#) && page.contains(r#"<h2>Context</h2>"#),
            "{page}"
        );
        assert!(
            page.contains(
                r#"<article class="auth-card account-card grid-half rows-2"><h2>Context</h2>"#
            ),
            "Context takes half the columns and two row units"
        );
        assert!(
            !page.contains("section-title"),
            "no separator between the cards"
        );
    }

    #[test]
    fn the_guardrails_card_lists_the_counts_and_the_detectors() {
        let guardrails = GuardrailsSummary {
            requests_scanned: 12,
            requests_masked: 4,
            placeholders_substituted: 7,
            detectors: vec![
                DetectorCount {
                    detector: "github_token".to_owned(),
                    matches: 5,
                },
                DetectorCount {
                    detector: "aws_access_key_id".to_owned(),
                    matches: 2,
                },
            ],
        };
        let page = page(&Overview {
            range: Range::Week,
            totals: &UsageTotals::default(),
            context: &ContextTotals::default(),
            tools: &ToolUsage::default(),
            series: &TotalsSeries {
                requests: vec![0; 8],
                tokens: vec![0; 8],
                cost_nanodollars: vec![0; 8],
            },
            models: &[],
            providers: &[],
            keys: &[],
            model_series: &[],
            provider_series: &[],
            key_series: &[],
            guardrails: &guardrails,
        })
        .into_string();
        assert!(page.contains("<h2>Guardrails</h2>"), "{page}");
        assert!(page.contains(
            r#"<th scope="row" class="cell-title">Requests scanned</th><td class="col-numeric">12</td>"#
        ));
        assert!(page.contains(
            r#"<th scope="row" class="cell-title">Requests masked</th><td class="col-numeric">4</td>"#
        ));
        assert!(page.contains(
            r#"<th scope="row" class="cell-title">Placeholders substituted</th><td class="col-numeric">7</td>"#
        ));
        assert!(page.contains(
            r#"<th scope="row" class="cell-title" title="github_token"><span class="cell-absent">detector</span> github_token</th><td class="col-numeric">5</td>"#
        ), "{page}");
        assert!(page.contains("aws_access_key_id"));
        assert!(!page.contains("No guardrail activity in this range."));
        let keys_card = page.find("<h2>Keys</h2>").unwrap();
        let guardrails_card = page.find("<h2>Guardrails</h2>").unwrap();
        assert!(keys_card < guardrails_card, "Guardrails comes after Keys");
    }

    #[test]
    fn the_guardrails_card_says_when_nothing_was_scanned() {
        let page = page_without_traffic().into_string();
        assert!(page.contains("<h2>Guardrails</h2>"));
        assert!(
            page.contains(
                r#"<td colspan="2" class="table-empty">No guardrail activity in this range.</td>"#
            ),
            "{page}"
        );
        assert!(!page.contains("Requests scanned"));
    }

    fn page_without_traffic() -> Markup {
        page(&Overview {
            range: Range::Week,
            totals: &UsageTotals::default(),
            context: &ContextTotals::default(),
            tools: &ToolUsage::default(),
            series: &TotalsSeries {
                requests: vec![0; 8],
                tokens: vec![0; 8],
                cost_nanodollars: vec![0; 8],
            },
            models: &[],
            providers: &[],
            keys: &[],
            model_series: &[],
            provider_series: &[],
            key_series: &[],
            guardrails: &GuardrailsSummary::default(),
        })
    }
}
