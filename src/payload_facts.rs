use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement};
use serde_json::Value;

const TOOL_DEFINITION: &str = "tool_definition";
const TOOL_USE: &str = "tool_use";
const TOOL_RESULT: &str = "tool_result";
const TEXT: &str = "text";
const THINKING: &str = "thinking";
const IMAGE: &str = "image";
const REASONING: &str = "reasoning";
const MESSAGE: &str = "message";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct BlobFact {
    pub block_type: &'static str,
    pub tool_name: Option<String>,
    pub skill_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub is_error: Option<bool>,
    pub cache_ttl: Option<String>,
}

pub(crate) fn mcp_server(tool_name: &str) -> Option<&str> {
    tool_name
        .strip_prefix("mcp__")?
        .split_once("__")
        .map(|(server, _)| server)
        .filter(|server| !server.is_empty())
}

pub(crate) fn extract(value: &Value) -> Vec<BlobFact> {
    let Some(object) = value.as_object() else {
        return Vec::new();
    };
    let plain = |block_type| plain(block_type, value);
    let facts = match object.get("type").and_then(Value::as_str) {
        Some("tool_use") => vec![BlobFact {
            tool_name: string(value, "name"),
            tool_use_id: string(value, "id"),
            skill_name: skill_name(value, || value.get("input").cloned()),
            ..plain(TOOL_USE)
        }],
        Some("function_call" | "custom_tool_call") => vec![BlobFact {
            tool_name: string(value, "name"),
            tool_use_id: string(value, "call_id").or_else(|| string(value, "id")),
            skill_name: skill_name(value, || arguments(value)),
            ..plain(TOOL_USE)
        }],
        Some("tool_result") => vec![BlobFact {
            tool_use_id: string(value, "tool_use_id"),
            is_error: value.get("is_error").and_then(Value::as_bool),
            ..plain(TOOL_RESULT)
        }],
        Some("function_call_output" | "custom_tool_call_output") => vec![BlobFact {
            tool_use_id: string(value, "call_id"),
            is_error: value.get("is_error").and_then(Value::as_bool),
            ..plain(TOOL_RESULT)
        }],
        Some("text") => vec![plain(TEXT)],
        Some("thinking") => vec![plain(THINKING)],
        Some("image") => vec![plain(IMAGE)],
        Some("reasoning") => vec![plain(REASONING)],
        Some("additional_tools") => object
            .get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .flat_map(nested_tools)
            .map(|tool| BlobFact {
                tool_name: definition_name(tool),
                ..plain(TOOL_DEFINITION)
            })
            .collect(),
        Some("message") => match object.get("content").and_then(Value::as_array) {
            Some(blocks) => blocks
                .iter()
                .map(|block| plain(content_block_type(block)))
                .collect(),
            None => vec![plain(MESSAGE)],
        },
        _ if is_tool_definition(value) => tool_definition(value),
        _ => Vec::new(),
    };
    one_cache_breakpoint(facts)
}

/// An element of a request's `tools` array is a definition whatever its shape:
/// hosted tools carry no schema.
pub(crate) fn tool_definition(value: &Value) -> Vec<BlobFact> {
    if !value.is_object() {
        return Vec::new();
    }
    vec![BlobFact {
        tool_name: definition_name(value),
        ..plain(TOOL_DEFINITION, value)
    }]
}

fn plain(block_type: &'static str, value: &Value) -> BlobFact {
    BlobFact {
        block_type,
        tool_name: None,
        skill_name: None,
        tool_use_id: None,
        is_error: None,
        cache_ttl: cache_ttl(value),
    }
}

fn one_cache_breakpoint(mut facts: Vec<BlobFact>) -> Vec<BlobFact> {
    for fact in facts.iter_mut().skip(1) {
        fact.cache_ttl = None;
    }
    facts
}

fn nested_tools(container: &Value) -> Vec<&Value> {
    ["tools", "functions"]
        .into_iter()
        .find_map(|key| container.get(key).and_then(Value::as_array))
        .map_or_else(|| vec![container], |tools| tools.iter().collect())
}

fn is_tool_definition(value: &Value) -> bool {
    value.get("input_schema").is_some()
        || value.get("parameters").is_some()
        || value
            .get("function")
            .is_some_and(|function| function.get("parameters").is_some())
}

fn definition_name(value: &Value) -> Option<String> {
    value
        .get("function")
        .and_then(|function| string(function, "name"))
        .or_else(|| string(value, "name"))
}

fn content_block_type(block: &Value) -> &'static str {
    match block.get("type").and_then(Value::as_str) {
        Some("input_image" | "image") => IMAGE,
        _ => TEXT,
    }
}

fn arguments(value: &Value) -> Option<Value> {
    serde_json::from_str(value.get("arguments")?.as_str()?).ok()
}

fn skill_name(value: &Value, input: impl FnOnce() -> Option<Value>) -> Option<String> {
    (string(value, "name")? == "Skill")
        .then(|| string(&input()?, "skill"))
        .flatten()
}

fn string(value: &Value, key: &str) -> Option<String> {
    Some(value.get(key)?.as_str()?.to_owned())
}

fn cache_ttl(value: &Value) -> Option<String> {
    let cache_control = value.get("cache_control")?.as_object()?;
    Some(
        cache_control
            .get("ttl")
            .and_then(Value::as_str)
            .unwrap_or("5m")
            .to_owned(),
    )
}

pub(crate) async fn store(
    database: &impl ConnectionTrait,
    blob_id: &str,
    facts: &[BlobFact],
) -> Result<(), DbErr> {
    for (ordinal, fact) in facts.iter().enumerate() {
        database
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT OR IGNORE INTO gateway_payload_blob_facts (blob_id, ordinal, block_type, tool_name, mcp_server, skill_name, tool_use_id, is_error, cache_ttl) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                [
                    blob_id.to_owned().into(),
                    (ordinal as i64).into(),
                    fact.block_type.into(),
                    fact.tool_name.clone().into(),
                    fact.tool_name
                        .as_deref()
                        .and_then(mcp_server)
                        .map(str::to_owned)
                        .into(),
                    fact.skill_name.clone().into(),
                    fact.tool_use_id.clone().into(),
                    fact.is_error.into(),
                    fact.cache_ttl.clone().into(),
                ],
            ))
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn named(value: &Value) -> Vec<(&'static str, Option<String>)> {
        extract(value)
            .into_iter()
            .map(|fact| (fact.block_type, fact.tool_name))
            .collect()
    }

    #[test]
    fn a_grouping_container_yields_its_nested_tools_and_not_itself() {
        let value = json!({
            "type": "additional_tools",
            "tools": [
                {"name": "functions", "description": "", "tools": [
                    {"type": "function", "name": "exec", "parameters": {}},
                    {"type": "custom", "name": "apply_patch", "format": {}}
                ]},
                {"name": "collaboration", "functions": [
                    {"type": "function", "name": "spawn_agent", "parameters": {}}
                ]}
            ]
        });
        assert_eq!(
            named(&value),
            [
                ("tool_definition", Some("exec".to_owned())),
                ("tool_definition", Some("apply_patch".to_owned())),
                ("tool_definition", Some("spawn_agent".to_owned())),
            ]
        );
    }

    #[test]
    fn an_additional_tools_entry_without_nesting_is_the_tool_itself() {
        let value = json!({"type": "additional_tools", "tools": [{"name": "exec"}]});
        assert_eq!(
            named(&value),
            [("tool_definition", Some("exec".to_owned()))]
        );
    }

    #[test]
    fn a_definition_is_recognized_by_its_schema_and_not_by_its_name() {
        assert_eq!(
            named(&json!({"name": "Read", "input_schema": {"type": "object"}})),
            [("tool_definition", Some("Read".to_owned()))]
        );
        assert_eq!(
            named(&json!({"function": {"name": "wait", "parameters": {}}})),
            [("tool_definition", Some("wait".to_owned()))]
        );
        assert_eq!(named(&json!({"name": "Read", "description": "reads"})), []);
        assert_eq!(
            named(&json!({"role": "assistant", "content": []})),
            [],
            "a message shell is not a tool definition"
        );
    }

    #[test]
    fn a_skill_call_records_the_skill_it_runs() {
        let anthropic = json!({
            "type": "tool_use", "id": "toolu_1", "name": "Skill",
            "input": {"skill": "unslop"}
        });
        assert_eq!(extract(&anthropic)[0].skill_name.as_deref(), Some("unslop"));
        let codex = json!({
            "type": "function_call", "call_id": "call_1", "name": "Skill",
            "arguments": "{\"skill\":\"unslop\"}"
        });
        assert_eq!(extract(&codex)[0].skill_name.as_deref(), Some("unslop"));
        let other = json!({
            "type": "tool_use", "id": "toolu_2", "name": "Read",
            "input": {"skill": "unslop"}
        });
        assert_eq!(extract(&other)[0].skill_name, None);
    }

    #[test]
    fn a_call_and_its_result_share_the_identifier_that_joins_them() {
        let call = &extract(&json!({
            "type": "custom_tool_call", "id": "ctc_1", "call_id": "call_1",
            "name": "exec", "input": "ls"
        }))[0];
        assert_eq!(call.block_type, "tool_use");
        assert_eq!(call.tool_use_id.as_deref(), Some("call_1"));

        let output = &extract(&json!({
            "type": "custom_tool_call_output", "call_id": "call_1", "output": []
        }))[0];
        assert_eq!(output.block_type, "tool_result");
        assert_eq!(output.tool_use_id.as_deref(), Some("call_1"));

        let result = &extract(&json!({
            "type": "tool_result", "tool_use_id": "toolu_1", "is_error": true
        }))[0];
        assert_eq!(result.is_error, Some(true));
    }

    #[test]
    fn an_ephemeral_cache_marker_without_a_ttl_means_five_minutes() {
        let ttl = |cache_control| {
            extract(&json!({"type": "text", "text": "hi", "cache_control": cache_control}))[0]
                .cache_ttl
                .clone()
        };
        assert_eq!(ttl(json!({"type": "ephemeral"})), Some("5m".to_owned()));
        assert_eq!(
            ttl(json!({"type": "ephemeral", "ttl": "1h"})),
            Some("1h".to_owned())
        );
        assert_eq!(extract(&json!({"type": "text"}))[0].cache_ttl, None);
    }

    #[test]
    fn a_container_cache_marker_counts_as_one_breakpoint() {
        let value = json!({
            "type": "additional_tools", "cache_control": {"type": "ephemeral"},
            "tools": [{"name": "functions", "tools": [
                {"type": "function", "name": "exec", "parameters": {}},
                {"type": "function", "name": "wait", "parameters": {}}
            ]}]
        });
        let ttls: Vec<_> = extract(&value)
            .into_iter()
            .map(|fact| fact.cache_ttl)
            .collect();
        assert_eq!(ttls, [Some("5m".to_owned()), None]);
    }

    #[test]
    fn a_tools_element_is_a_definition_even_without_a_schema() {
        let custom = json!({"type": "custom", "name": "apply_patch", "format": {}});
        assert_eq!(
            named(&custom),
            [],
            "outside the tools array its shape says nothing"
        );
        let definition = |value: &Value| {
            tool_definition(value)
                .into_iter()
                .map(|fact| (fact.block_type, fact.tool_name))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            definition(&custom),
            [("tool_definition", Some("apply_patch".to_owned()))]
        );
        assert_eq!(
            definition(&json!({"type": "web_search"})),
            [("tool_definition", None)]
        );
        assert_eq!(
            definition(&json!({"type": "web_search_20250305", "name": "web_search"})),
            [("tool_definition", Some("web_search".to_owned()))]
        );
        assert_eq!(definition(&json!("not an object")), []);
    }

    #[test]
    fn a_codex_message_yields_one_fact_for_each_content_block() {
        let value = json!({
            "type": "message", "role": "user",
            "content": [
                {"type": "input_text", "text": "hi"},
                {"type": "input_image", "image_url": "data:"}
            ]
        });
        assert_eq!(named(&value), [("text", None), ("image", None)]);
    }

    #[test]
    fn an_mcp_tool_name_carries_its_server() {
        assert_eq!(
            mcp_server("mcp__claude_ai_GitLab__get_merge_request"),
            Some("claude_ai_GitLab")
        );
        assert_eq!(mcp_server("Bash"), None);
        assert_eq!(mcp_server("mcp__lonely"), None);
    }
}
