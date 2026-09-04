use crate::telemetry::timestamp;
use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement};

const COMPONENT_SQL: &str = "CASE
    WHEN r.path = 'tools' OR r.kind = 'additional_tools' THEN 'tool_definition'
    WHEN r.path IN ('system', 'instructions') THEN 'system'
    WHEN r.kind IN ('thinking', 'redacted_thinking', 'reasoning') THEN 'thinking'
    WHEN r.kind IN ('tool_use', 'server_tool_use', 'function_call', 'custom_tool_call') THEN 'tool_use'
    WHEN r.kind IN ('tool_result', 'web_search_tool_result', 'function_call_output', 'custom_tool_call_output') THEN 'tool_result'
    WHEN r.kind IN ('text', 'message')
      OR (r.kind = 'messages' AND NOT EXISTS (
            SELECT 1 FROM gateway_payload_part_refs c
            WHERE c.request_id = r.request_id AND c.direction = 'request'
              AND c.path = 'messages/' || r.position || '/content'))
    THEN CASE r.role
        WHEN 'user' THEN 'user_text'
        WHEN 'assistant' THEN 'assistant_text'
        WHEN 'system' THEN 'system'
        WHEN 'developer' THEN 'system'
        ELSE 'other'
    END
    ELSE 'other'
END";

const ROLLUP_SQL: &str = "INSERT OR IGNORE INTO gateway_request_metrics (
    request_id, tool_definition_bytes, system_bytes, user_text_bytes, assistant_text_bytes,
    thinking_bytes, tool_use_bytes, tool_result_bytes, other_bytes, total_bytes,
    tools_offered, tools_invoked, tool_result_errors, cache_breakpoints, created_at)
SELECT ?1,
    SUM(CASE WHEN component = 'tool_definition' THEN bytes ELSE 0 END),
    SUM(CASE WHEN component = 'system' THEN bytes ELSE 0 END),
    SUM(CASE WHEN component = 'user_text' THEN bytes ELSE 0 END),
    SUM(CASE WHEN component = 'assistant_text' THEN bytes ELSE 0 END),
    SUM(CASE WHEN component = 'thinking' THEN bytes ELSE 0 END),
    SUM(CASE WHEN component = 'tool_use' THEN bytes ELSE 0 END),
    SUM(CASE WHEN component = 'tool_result' THEN bytes ELSE 0 END),
    SUM(CASE WHEN component = 'other' THEN bytes ELSE 0 END),
    SUM(bytes),
    (SELECT COUNT(*) FROM gateway_payload_part_refs r
     JOIN gateway_payload_blob_facts f ON f.blob_id = r.part_id
     WHERE r.request_id = ?1 AND r.direction = 'request' AND f.block_type = 'tool_definition'),
    (SELECT COUNT(*) FROM gateway_payload_part_refs r
     JOIN gateway_payload_blob_facts f ON f.blob_id = r.part_id
     WHERE r.request_id = ?1 AND r.direction = 'request' AND f.block_type = 'tool_use'),
    (SELECT COUNT(*) FROM gateway_payload_part_refs r
     JOIN gateway_payload_blob_facts f ON f.blob_id = r.part_id
     WHERE r.request_id = ?1 AND r.direction = 'request' AND f.block_type = 'tool_result' AND f.is_error = 1),
    (SELECT COUNT(*) FROM gateway_payload_part_refs r
     JOIN gateway_payload_blob_facts f ON f.blob_id = r.part_id
     WHERE r.request_id = ?1 AND r.direction = 'request' AND f.cache_ttl IS NOT NULL),
    ?2
FROM (SELECT b.original_bytes bytes, {component} component
      FROM gateway_payload_part_refs r
      JOIN gateway_payload_blobs b ON b.id = r.part_id
      WHERE r.request_id = ?1 AND r.direction = 'request')
HAVING COUNT(*) > 0";

pub(crate) async fn rollup(
    database: &impl ConnectionTrait,
    request_id: &str,
) -> Result<bool, DbErr> {
    let result = database
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            ROLLUP_SQL.replace("{component}", COMPONENT_SQL),
            [request_id.to_owned().into(), timestamp().into()],
        ))
        .await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        migration::Migrator,
        pricing::{Cost, CostSource},
        providers::{Provider, Usage},
        telemetry::{CompletionRecord, SqliteSink, StartRecord},
    };
    use sea_orm::{Database, DatabaseConnection, FromQueryResult};
    use sea_orm_migration::MigratorTrait;
    use uuid::Uuid;

    #[derive(Debug, Default, PartialEq, Eq, FromQueryResult)]
    pub(crate) struct Metrics {
        pub tool_definition_bytes: i64,
        pub system_bytes: i64,
        pub user_text_bytes: i64,
        pub assistant_text_bytes: i64,
        pub thinking_bytes: i64,
        pub tool_use_bytes: i64,
        pub tool_result_bytes: i64,
        pub other_bytes: i64,
        pub total_bytes: i64,
        pub tools_offered: i64,
        pub tools_invoked: i64,
        pub tool_result_errors: i64,
        pub cache_breakpoints: i64,
    }

    pub(crate) async fn metrics(
        database: &DatabaseConnection,
        request_id: &str,
    ) -> Option<Metrics> {
        Metrics::find_by_statement(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT * FROM gateway_request_metrics WHERE request_id = ?",
            [request_id.to_owned().into()],
        ))
        .one(database)
        .await
        .unwrap()
    }

    pub(crate) async fn started(
        database: &DatabaseConnection,
        provider: Provider,
        body: &[u8],
    ) -> String {
        SqliteSink::new(database.clone())
            .start(StartRecord {
                request_id: "req-1",
                key_id: "key-1",
                key_version_id: "key-version-1",
                provider_id: "claude",
                provider,
                method: "POST",
                endpoint: "/v1/messages",
                requested_model: None,
                request_body: body,
            })
            .await
            .unwrap()
            .to_string()
    }

    pub(crate) async fn database() -> DatabaseConnection {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&database, None).await.unwrap();
        database
    }

    fn json_len(value: serde_json::Value) -> i64 {
        serde_json::to_vec(&value).unwrap().len() as i64
    }

    pub(crate) const ANTHROPIC_BODY: &str = r#"{"model":"claude-x",
        "system":[{"type":"text","text":"be brief","cache_control":{"type":"ephemeral"}}],
        "tools":[{"name":"Read","input_schema":{"type":"object"},"cache_control":{"type":"ephemeral"}},
                 {"name":"Bash","input_schema":{"type":"object"}}],
        "messages":[
            {"role":"user","content":"hello"},
            {"role":"assistant","content":[
                {"type":"thinking","thinking":"hmm","signature":"x"},
                {"type":"text","text":"reading"},
                {"type":"tool_use","id":"toolu_1","name":"Read","input":{"path":"a"}}]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"toolu_1","content":"nope","is_error":true},
                {"type":"image","source":{"type":"base64","data":"AAAA"}},
                {"type":"text","text":"try again"}]}]}"#;

    #[tokio::test]
    async fn an_anthropic_request_is_rolled_up_by_component() {
        let database = database().await;
        let request_id = started(&database, Provider::Anthropic, ANTHROPIC_BODY.as_bytes()).await;

        assert!(rollup(&database, &request_id).await.unwrap());
        let json = |text: &str| json_len(serde_json::from_str(text).unwrap());
        let shell = |role: &str| json_len(serde_json::json!({"role": role, "content": []}));
        let expected = Metrics {
            tool_definition_bytes: json(
                r#"{"name":"Read","input_schema":{"type":"object"},"cache_control":{"type":"ephemeral"}}"#,
            ) + json(r#"{"name":"Bash","input_schema":{"type":"object"}}"#),
            system_bytes: json(
                r#"{"type":"text","text":"be brief","cache_control":{"type":"ephemeral"}}"#,
            ),
            user_text_bytes: json(r#"{"role":"user","content":"hello"}"#)
                + json(r#"{"type":"text","text":"try again"}"#),
            assistant_text_bytes: json(r#"{"type":"text","text":"reading"}"#),
            thinking_bytes: json(r#"{"type":"thinking","thinking":"hmm","signature":"x"}"#),
            tool_use_bytes: json(
                r#"{"type":"tool_use","id":"toolu_1","name":"Read","input":{"path":"a"}}"#,
            ),
            tool_result_bytes: json(
                r#"{"type":"tool_result","tool_use_id":"toolu_1","content":"nope","is_error":true}"#,
            ),
            other_bytes: shell("assistant")
                + shell("user")
                + json(r#"{"type":"image","source":{"type":"base64","data":"AAAA"}}"#),
            total_bytes: 0,
            tools_offered: 2,
            tools_invoked: 1,
            tool_result_errors: 1,
            cache_breakpoints: 2,
        };
        let expected = Metrics {
            total_bytes: expected.tool_definition_bytes
                + expected.system_bytes
                + expected.user_text_bytes
                + expected.assistant_text_bytes
                + expected.thinking_bytes
                + expected.tool_use_bytes
                + expected.tool_result_bytes
                + expected.other_bytes,
            ..expected
        };
        assert_eq!(metrics(&database, &request_id).await, Some(expected));

        assert!(
            !rollup(&database, &request_id).await.unwrap(),
            "a second pass leaves the existing row alone"
        );
    }

    #[tokio::test]
    async fn the_total_counts_every_stored_part_once_and_shells_carry_no_content() {
        let database = database().await;
        let request_id = started(&database, Provider::Anthropic, ANTHROPIC_BODY.as_bytes()).await;
        rollup(&database, &request_id).await.unwrap();

        let row = database
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT SUM(b.original_bytes) total,
                        SUM(CASE WHEN r.path = 'messages' THEN b.original_bytes ELSE 0 END) shells
                 FROM gateway_payload_part_refs r
                 JOIN gateway_payload_blobs b ON b.id = r.part_id
                 WHERE r.request_id = ? AND r.direction = 'request'",
                [request_id.clone().into()],
            ))
            .await
            .unwrap()
            .unwrap();
        let stored_total: i64 = row.try_get("", "total").unwrap();
        let shells: i64 = row.try_get("", "shells").unwrap();
        let rolled = metrics(&database, &request_id).await.unwrap();

        assert_eq!(rolled.total_bytes, stored_total);
        assert_eq!(
            shells,
            json_len(serde_json::json!({"role": "user", "content": "hello"}))
                + json_len(serde_json::json!({"role": "assistant", "content": []}))
                + json_len(serde_json::json!({"role": "user", "content": []}))
        );
        assert!(
            rolled.other_bytes < rolled.user_text_bytes + rolled.tool_result_bytes,
            "the shells hold only the role, not the blocks that were split out of them"
        );
    }

    pub(crate) const CODEX_BODY: &str = r#"{"model":"gpt-x","instructions":"be brief",
        "input":[
            {"type":"message","role":"developer","content":[{"type":"input_text","text":"rules"}]},
            {"type":"additional_tools","role":"developer","tools":[{"name":"functions","tools":[
                {"type":"function","name":"exec","parameters":{}},
                {"type":"custom","name":"apply_patch","format":{}}]}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
            {"type":"reasoning","summary":[]},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]},
            {"type":"custom_tool_call","id":"ctc_1","call_id":"call_1","name":"exec","input":"ls"},
            {"type":"custom_tool_call_output","call_id":"call_1","output":"a.txt"},
            {"type":"function_call","id":"fc_1","call_id":"call_2","name":"apply_patch","arguments":"{}"},
            {"type":"function_call_output","call_id":"call_2","output":"done"}],
        "tools":[{"type":"function","name":"wait","parameters":{}}]}"#;

    #[tokio::test]
    async fn a_codex_request_is_rolled_up_by_component() {
        let database = database().await;
        let request_id = started(&database, Provider::Codex, CODEX_BODY.as_bytes()).await;

        assert!(rollup(&database, &request_id).await.unwrap());
        let json = |text: &str| json_len(serde_json::from_str(text).unwrap());
        let rolled = metrics(&database, &request_id).await.unwrap();
        assert_eq!(
            rolled.tool_definition_bytes,
            json(
                r#"{"type":"additional_tools","role":"developer","tools":[{"name":"functions","tools":[
                {"type":"function","name":"exec","parameters":{}},
                {"type":"custom","name":"apply_patch","format":{}}]}]}"#
            ) + json(r#"{"type":"function","name":"wait","parameters":{}}"#)
        );
        assert_eq!(
            rolled.system_bytes,
            json(r#""be brief""#)
                + json(
                    r#"{"type":"message","role":"developer","content":[{"type":"input_text","text":"rules"}]}"#
                )
        );
        assert_eq!(
            rolled.user_text_bytes,
            json(
                r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}"#
            )
        );
        assert_eq!(
            rolled.assistant_text_bytes,
            json(
                r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}"#
            )
        );
        assert_eq!(
            rolled.thinking_bytes,
            json(r#"{"type":"reasoning","summary":[]}"#)
        );
        assert_eq!(
            rolled.tool_use_bytes,
            json(
                r#"{"type":"custom_tool_call","id":"ctc_1","call_id":"call_1","name":"exec","input":"ls"}"#
            ) + json(
                r#"{"type":"function_call","id":"fc_1","call_id":"call_2","name":"apply_patch","arguments":"{}"}"#
            )
        );
        assert_eq!(
            rolled.tool_result_bytes,
            json(r#"{"type":"custom_tool_call_output","call_id":"call_1","output":"a.txt"}"#)
                + json(r#"{"type":"function_call_output","call_id":"call_2","output":"done"}"#)
        );
        assert_eq!(rolled.other_bytes, 0);
        assert_eq!(
            rolled.total_bytes,
            rolled.tool_definition_bytes
                + rolled.system_bytes
                + rolled.user_text_bytes
                + rolled.assistant_text_bytes
                + rolled.thinking_bytes
                + rolled.tool_use_bytes
                + rolled.tool_result_bytes
        );
        assert_eq!(
            (
                rolled.tools_offered,
                rolled.tools_invoked,
                rolled.tool_result_errors,
                rolled.cache_breakpoints
            ),
            (3, 2, 0, 0),
            "the additional_tools container counts its nested tools and not itself"
        );
    }

    #[tokio::test]
    async fn completing_a_request_writes_its_metrics() {
        let database = database().await;
        let request_id = started(&database, Provider::Codex, CODEX_BODY.as_bytes()).await;
        SqliteSink::new(database.clone())
            .complete(CompletionRecord {
                id: Uuid::parse_str(&request_id).unwrap(),
                status: 200,
                first_byte_at: None,
                response_body: b"{}",
                response_bytes: 2,
                response_truncated: false,
                client_disconnected: false,
                usage: &Usage::default(),
                cost: Cost {
                    nanodollars: None,
                    source: CostSource::Unknown,
                },
                error_message: None,
            })
            .await
            .unwrap();
        assert_eq!(
            metrics(&database, &request_id).await.unwrap().tools_invoked,
            2
        );
    }

    #[tokio::test]
    async fn a_request_without_stored_parts_gets_no_row() {
        let database = database().await;
        let request_id = started(&database, Provider::Anthropic, b"").await;
        assert!(!rollup(&database, &request_id).await.unwrap());
        assert_eq!(metrics(&database, &request_id).await, None);
    }
}
