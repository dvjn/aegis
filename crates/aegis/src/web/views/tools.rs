//! The tool cards of the overview: what tools were called, which servers
//! provide them, and which skills were loaded.

use maud::{Markup, html};

use super::charts::{series_class, stacked_bar_without_legend};
use super::format::{count_text, money_text, token_text};
use crate::usage::{ContextTotals, ToolUsage};

fn tokens_bar(rows: &[(&str, i64)]) -> Markup {
    let classes: Vec<String> = (0..rows.len()).map(series_class).collect();
    let segments: Vec<(&str, f64, &str)> = rows
        .iter()
        .zip(&classes)
        .map(|((label, bytes), class)| {
            (
                *label,
                ContextTotals::estimated_tokens(*bytes) as f64,
                class.as_str(),
            )
        })
        .collect();
    stacked_bar_without_legend(&segments, &|value| token_text(value as i64))
}

fn swatch(index: usize) -> Markup {
    html! { span class=(format!("row-swatch {}", series_class(index))) aria-hidden="true" {} }
}

pub fn calls_card(usage: &ToolUsage) -> Markup {
    let called: Vec<_> = usage.tools.iter().filter(|tool| tool.calls > 0).collect();
    let rows: Vec<(&str, i64)> = called
        .iter()
        .map(|tool| (tool.label.as_deref().unwrap_or("unspecified"), tool.bytes))
        .collect();
    html! {
        article class="auth-card account-card grid-half rows-3" {
            h2 { "Tools" }
            p { "Calls, tokens, and cost by tool." }
            (tokens_bar(&rows))
            div class="table-wrap" {
                table class="data-table tool-table" {
                    thead {
                        tr {
                            th scope="col" { "Tool" }
                            th scope="col" class="col-numeric" { "Calls" }
                            th scope="col" class="col-numeric" { "Tokens" }
                            th scope="col" class="col-numeric" { "Cost" }
                        }
                    }
                    tbody {
                        @if called.is_empty() {
                            tr { td colspan="4" class="table-empty" { "No tool calls in this range." } }
                        }
                        @for (index, tool) in called.iter().enumerate() {
                            tr {
                                (label_cell(tool.label.as_deref(), index))
                                td class="col-numeric" { (count_text(tool.calls)) }
                                td class="col-numeric" { (token_text(ContextTotals::estimated_tokens(tool.bytes))) }
                                td class="col-numeric" { (money_text(tool.cost_nanodollars)) }
                            }
                        }
                    }
                }
            }
        }
    }
}

pub fn mcp_card(usage: &ToolUsage) -> Markup {
    let servers = usage.mcp_servers();
    let rows: Vec<(&str, i64)> = servers
        .iter()
        .map(|server| (server.label.as_str(), server.bytes))
        .collect();
    html! {
        article class="auth-card account-card grid-half rows-2" {
            h2 { "MCPs" }
            p { "Calls, tools, tokens, and cost by server." }
            (tokens_bar(&rows))
            div class="table-wrap" {
                table class="data-table" {
                    thead {
                        tr {
                            th scope="col" { "Server" }
                            th scope="col" class="col-numeric" { "Calls" }
                            th scope="col" class="col-numeric" { "Tools" }
                            th scope="col" class="col-numeric" { "Tokens" }
                            th scope="col" class="col-numeric" { "Cost" }
                        }
                    }
                    tbody {
                        @if servers.is_empty() {
                            tr { td colspan="5" class="table-empty" { "No MCP tools in this range." } }
                        }
                        @for (index, server) in servers.iter().enumerate() {
                            tr {
                                th scope="row" class="cell-title" title=(server.label) {
                                    (swatch(index))
                                    (server.label)
                                }
                                td class="col-numeric" { (count_text(server.calls)) }
                                td class="col-numeric" { (count_text(server.tools)) }
                                td class="col-numeric" { (token_text(ContextTotals::estimated_tokens(server.bytes))) }
                                td class="col-numeric" { (money_text(server.cost_nanodollars)) }
                            }
                        }
                    }
                }
            }
        }
    }
}

pub fn skills_card(usage: &ToolUsage) -> Markup {
    let rows: Vec<(&str, i64)> = usage
        .skills
        .iter()
        .map(|skill| (skill.label.as_str(), skill.bytes))
        .collect();
    html! {
        article class="auth-card account-card grid-half rows-2" {
            h2 { "Skills" }
            p { "Skill tool calls by skill." }
            (tokens_bar(&rows))
            div class="table-wrap" {
                table class="data-table" {
                    thead {
                        tr {
                            th scope="col" { "Skill" }
                            th scope="col" class="col-numeric" { "Calls" }
                            th scope="col" class="col-numeric" { "Tokens" }
                            th scope="col" class="col-numeric" { "Cost" }
                        }
                    }
                    tbody {
                        @if usage.skills.is_empty() {
                            tr { td colspan="4" class="table-empty" { "No skill calls in this range." } }
                        }
                        @for (index, skill) in usage.skills.iter().enumerate() {
                            tr {
                                th scope="row" class="cell-title" title=(skill.label) {
                                    (swatch(index))
                                    (skill.label)
                                }
                                td class="col-numeric" { (count_text(skill.calls)) }
                                td class="col-numeric" { (token_text(ContextTotals::estimated_tokens(skill.bytes))) }
                                td class="col-numeric" { (money_text(skill.cost_nanodollars)) }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn label_cell(label: Option<&str>, index: usize) -> Markup {
    html! {
        @match label {
            Some(label) => {
                th scope="row" class="cell-title" title=(label) {
                    (swatch(index))
                    (label.strip_prefix("mcp__").unwrap_or(label))
                }
            }
            None => {
                th scope="row" class="cell-title" title="unspecified" {
                    (swatch(index))
                    span class="cell-absent" { "unspecified" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{SkillCalls, ToolCalls};

    fn tool(label: &str, calls: i64, bytes: i64, cost_nanodollars: i64) -> ToolCalls {
        ToolCalls {
            label: Some(label.to_owned()),
            calls,
            bytes,
            cost_nanodollars,
        }
    }

    fn usage() -> ToolUsage {
        ToolUsage {
            tools: vec![
                tool("Bash", 521, 20_921_671, 2_150_000_000),
                tool("exec", 376, 111_302_820, 9_000_000_000),
                tool("mcp__claude_ai_GitLab__search", 12, 3_529_000, 120_000_000),
                tool("Glob", 2, 7_058, 900_000),
                ToolCalls {
                    label: None,
                    calls: 0,
                    bytes: 700,
                    cost_nanodollars: 35_000,
                },
                tool(
                    "mcp__claude_ai_Microsoft_365__outlook_send_mail",
                    0,
                    5_000_000,
                    400_000_000,
                ),
                tool(
                    "mcp__claude_ai_GitLab__get_pipeline",
                    0,
                    1_000_000,
                    80_000_000,
                ),
            ],
            skills: vec![
                SkillCalls {
                    label: "unslop".to_owned(),
                    calls: 5,
                    bytes: 27_000,
                    cost_nanodollars: 120_000_000,
                },
                SkillCalls {
                    label: "artifact-design".to_owned(),
                    calls: 1,
                    bytes: 3_529,
                    cost_nanodollars: 0,
                },
            ],
        }
    }

    #[test]
    fn the_tools_card_lists_every_called_tool_by_calls() {
        let card = calls_card(&usage()).into_string();
        assert!(
            card.contains(r#"<article class="auth-card account-card grid-half rows-3"><h2>Tools</h2><p>Calls, tokens, and cost by tool.</p><svg class="stacked-bar""#),
            "the bar sits between the subtitle and the table: {card}"
        );
        assert!(card.contains(r#"<th scope="col">Tool</th><th scope="col" class="col-numeric">Calls</th><th scope="col" class="col-numeric">Tokens</th><th scope="col" class="col-numeric">Cost</th>"#));
        let order: Vec<usize> = ["Bash", "exec", "mcp__claude_ai_GitLab__search", "Glob"]
            .iter()
            .map(|label| card.find(&format!(r#"title="{label}""#)).unwrap())
            .collect();
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{order:?}");
        assert!(
            card.contains(r#"<td class="col-numeric">521</td><td class="col-numeric">5.9M</td><td class="col-numeric">$2.15</td>"#),
            "bytes read as estimated tokens beside the apportioned cost: {card}"
        );
        for absent in ["Errors", "Result tokens", "est.", "%"] {
            assert!(!card.contains(absent), "{absent} is gone");
        }
        assert!(
            card.contains(r#"<th scope="row" class="cell-title" title="mcp__claude_ai_GitLab__search"><span class="row-swatch series-3" aria-hidden="true"></span>claude_ai_GitLab__search</th>"#),
            "an MCP tool reads as one name, less the mcp__ prefix: {card}"
        );
        assert!(
            !card.contains("unspecified") && !card.contains("outlook_send_mail"),
            "a result without a call, or a tool only defined, is not a tool that was called"
        );

        let empty = calls_card(&ToolUsage::default()).into_string();
        assert!(empty.contains("No tool calls in this range."));
    }

    #[test]
    fn the_mcp_card_lists_every_server_by_calls_then_name() {
        let card = mcp_card(&usage()).into_string();
        assert!(card.contains(r#"<article class="auth-card account-card grid-half rows-2"><h2>MCPs</h2><p>Calls, tools, tokens, and cost by server.</p><svg class="stacked-bar""#), "{card}");
        assert!(card.contains(r#"<th scope="col">Server</th><th scope="col" class="col-numeric">Calls</th><th scope="col" class="col-numeric">Tools</th><th scope="col" class="col-numeric">Tokens</th><th scope="col" class="col-numeric">Cost</th>"#));
        assert!(
            card.contains(r#"<th scope="row" class="cell-title" title="claude_ai_GitLab"><span class="row-swatch series-1" aria-hidden="true"></span>claude_ai_GitLab</th><td class="col-numeric">12</td><td class="col-numeric">2</td><td class="col-numeric">1.3M</td><td class="col-numeric">$0.20</td>"#),
            "a server sums its called and its merely defined tools: {card}"
        );
        let gitlab = card.find(r#"title="claude_ai_GitLab""#).unwrap();
        let microsoft = card.find(r#"title="claude_ai_Microsoft_365""#).unwrap();
        assert!(gitlab < microsoft, "the called server comes first");
        assert!(!card.contains("other servers"), "every server is listed");
        assert!(!card.contains("MB"));

        let empty = mcp_card(&ToolUsage::default()).into_string();
        assert!(empty.contains("No MCP tools in this range."));
    }

    #[test]
    fn the_skills_card_lists_skills_by_calls() {
        let card = skills_card(&usage()).into_string();
        assert!(
            card.contains(
                r#"<article class="auth-card account-card grid-half rows-2"><h2>Skills</h2><p>Skill tool calls by skill.</p><svg class="stacked-bar""#
            ),
            "{card}"
        );
        assert!(
            card.contains(r#"<th scope="row" class="cell-title" title="unslop"><span class="row-swatch series-1" aria-hidden="true"></span>unslop</th><td class="col-numeric">5</td><td class="col-numeric">7,651</td><td class="col-numeric">$0.12</td>"#),
            "{card}"
        );
        assert!(card.find("unslop").unwrap() < card.find("artifact-design").unwrap());
        assert!(!card.contains("loaded"), "no headline sentence");

        let empty = skills_card(&ToolUsage::default()).into_string();
        assert!(empty.contains("No skill calls in this range."));
    }
}
