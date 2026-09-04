use crate::{
    compression::{decode_body, decode_brotli_unsniffable},
    pricing::Cost,
    providers::{Provider, Usage},
};
use chrono::{SecondsFormat, TimeDelta, Utc};
use flate2::{Compression, write::GzEncoder};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, io::Write};
use uuid::Uuid;

const INTERRUPTED_AFTER: TimeDelta = TimeDelta::minutes(15);
const INTERRUPTED_MESSAGE: &str = "interrupted before the response completed";

#[derive(Clone)]
pub struct SqliteSink {
    database: DatabaseConnection,
}

pub struct StartRecord<'a> {
    pub request_id: &'a str,
    pub key_id: &'a str,
    pub key_version_id: &'a str,
    pub provider_id: &'a str,
    pub provider: Provider,
    pub method: &'a str,
    pub endpoint: &'a str,
    pub requested_model: Option<&'a str>,
    pub request_body: &'a [u8],
}

pub(crate) struct StoredPayload {
    pub id: String,
    pub body: Vec<u8>,
    pub encoding: &'static str,
    pub original_bytes: i64,
}

pub(crate) fn encode_payload(body: &[u8]) -> Option<StoredPayload> {
    if body.is_empty() {
        return None;
    }
    let original_bytes = body.len() as i64;
    let id = Sha256::digest(body)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    let compressed = encoder
        .write_all(body)
        .and_then(|()| encoder.finish())
        .unwrap_or_default();
    let (body, encoding) = if !compressed.is_empty() && compressed.len() < body.len() {
        (compressed, "gzip")
    } else {
        (body.to_vec(), "identity")
    };
    Some(StoredPayload {
        id,
        body,
        encoding,
        original_bytes,
    })
}

pub(crate) fn chunk_payload(body: &[u8]) -> Vec<StoredPayload> {
    if body.is_empty() {
        return Vec::new();
    }
    fastcdc::v2020::FastCDC::new(body, 4 * 1024, 16 * 1024, 64 * 1024)
        .filter_map(|chunk| encode_payload(&body[chunk.offset..chunk.offset + chunk.length]))
        .collect()
}

pub(crate) struct SemanticPart {
    pub path: String,
    pub position: i64,
    pub role: Option<String>,
    pub kind: String,
    pub payload: StoredPayload,
}

pub(crate) struct SemanticPayload {
    pub envelope: StoredPayload,
    pub parts: Vec<SemanticPart>,
}

pub(crate) struct StoredPart {
    pub path: String,
    pub position: i64,
    pub body: Vec<u8>,
}

fn parse_body(body: &[u8]) -> Option<serde_json::Value> {
    if let Ok(root) = serde_json::from_slice(&decode_body(body)) {
        return Some(root);
    }
    serde_json::from_slice(&decode_brotli_unsniffable(body)?).ok()
}

fn split_paths(protocol: &str) -> Option<&'static [&'static str]> {
    match protocol {
        "anthropic_messages" => Some(&["system", "messages", "tools"]),
        "openai_responses" => Some(&["instructions", "input", "tools"]),
        _ => None,
    }
}

fn content_block_path(position: usize) -> String {
    format!("messages/{position}/content")
}

fn take_elements(value: &mut serde_json::Value) -> Vec<serde_json::Value> {
    match value {
        serde_json::Value::Array(items) => std::mem::take(items),
        serde_json::Value::Null => Vec::new(),
        _ => vec![std::mem::replace(value, serde_json::Value::Null)],
    }
}

fn take_content_blocks(message: &mut serde_json::Value) -> Option<Vec<serde_json::Value>> {
    match message.get_mut("content") {
        Some(serde_json::Value::Array(blocks)) => Some(std::mem::take(blocks)),
        _ => None,
    }
}

fn part(
    path: String,
    position: usize,
    role: Option<String>,
    fallback_kind: &str,
    value: &serde_json::Value,
) -> Option<SemanticPart> {
    let kind = value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(fallback_kind)
        .to_owned();
    Some(SemanticPart {
        path,
        position: position as i64,
        role,
        kind,
        payload: encode_payload(&serde_json::to_vec(value).ok()?)?,
    })
}

pub(crate) fn split_request(body: &[u8], protocol: &str) -> Option<SemanticPayload> {
    let mut root = parse_body(body)?;
    let object = root.as_object_mut()?;
    let mut parts = Vec::new();
    for path in split_paths(protocol)? {
        let Some(value) = object.get_mut(*path) else {
            continue;
        };
        for (position, mut value) in take_elements(value).into_iter().enumerate() {
            let role = value
                .get("role")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            let blocks = (protocol == "anthropic_messages" && *path == "messages")
                .then(|| take_content_blocks(&mut value))
                .flatten();
            parts.push(part(
                (*path).to_owned(),
                position,
                role.clone(),
                path,
                &value,
            )?);
            for (block_position, block) in blocks.into_iter().flatten().enumerate() {
                parts.push(part(
                    content_block_path(position),
                    block_position,
                    role.clone(),
                    "content",
                    &block,
                )?);
            }
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(SemanticPayload {
        envelope: encode_payload(&serde_json::to_vec(&root).ok()?)?,
        parts,
    })
}

pub(crate) fn reassemble_request(
    envelope: &[u8],
    parts: Vec<StoredPart>,
    protocol: &str,
) -> Option<Vec<u8>> {
    let mut grouped: BTreeMap<String, Vec<(i64, serde_json::Value)>> = BTreeMap::new();
    for stored in parts {
        grouped
            .entry(stored.path)
            .or_default()
            .push((stored.position, serde_json::from_slice(&stored.body).ok()?));
    }
    for values in grouped.values_mut() {
        values.sort_by_key(|(position, _)| *position);
    }
    let ordered = |values: Vec<(i64, serde_json::Value)>| {
        values
            .into_iter()
            .map(|(_, value)| value)
            .collect::<Vec<_>>()
    };

    let mut root: serde_json::Value = serde_json::from_slice(envelope).ok()?;
    let object = root.as_object_mut()?;
    for path in split_paths(protocol)? {
        let Some(slot) = object.get_mut(*path) else {
            continue;
        };
        let values = ordered(grouped.remove(*path).unwrap_or_default());
        match slot {
            serde_json::Value::Array(items) => *items = values,
            _ => {
                if let Some(value) = values.into_iter().next() {
                    *slot = value;
                }
            }
        }
    }
    if let Some(serde_json::Value::Array(messages)) = object.get_mut("messages") {
        for (position, message) in messages.iter_mut().enumerate() {
            let Some(blocks) = grouped.remove(&content_block_path(position)) else {
                continue;
            };
            if let Some(serde_json::Value::Array(content)) = message.get_mut("content") {
                *content = ordered(blocks);
            }
        }
    }
    serde_json::to_vec(&root).ok()
}

pub struct CompletionRecord<'a> {
    pub id: Uuid,
    pub status: u16,
    pub first_byte_at: Option<&'a str>,
    pub response_body: &'a [u8],
    pub response_bytes: usize,
    pub response_truncated: bool,
    pub client_disconnected: bool,
    pub usage: &'a Usage,
    pub cost: Cost,
    pub error_message: Option<&'a str>,
}

impl SqliteSink {
    pub fn new(database: DatabaseConnection) -> Self {
        Self { database }
    }

    pub async fn start(&self, record: StartRecord<'_>) -> Result<Uuid, sea_orm::DbErr> {
        let id = Uuid::now_v7();
        self.database
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO gateway_requests (id, request_id, key_id, key_version_id, provider, protocol, method, endpoint, requested_model, started_at, request_bytes) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                [
                    id.to_string().into(),
                    record.request_id.to_owned().into(),
                    record.key_id.to_owned().into(),
                    record.key_version_id.to_owned().into(),
                    record.provider_id.to_owned().into(),
                    record.provider.protocol().into(),
                    record.method.to_owned().into(),
                    record.endpoint.to_owned().into(),
                    record.requested_model.map(str::to_owned).into(),
                    timestamp().into(),
                    (record.request_body.len() as i64).into(),
                ],
            ))
            .await?;
        let protocol = record.provider.protocol();
        if let Some(payload) = split_request(record.request_body, protocol) {
            self.store_semantic_payload(id, payload).await?;
        } else if !record.request_body.is_empty() {
            self.store_chunked_payload(id, record.request_body).await?;
        }
        self.database
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO gateway_payloads (request_id, request_body_id) VALUES (?, NULL)",
                [id.to_string().into()],
            ))
            .await?;
        Ok(id)
    }

    pub async fn complete(&self, record: CompletionRecord<'_>) -> Result<(), sea_orm::DbErr> {
        self.database
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "UPDATE gateway_requests SET first_byte_at = ?, completed_at = ?, http_status = ?, response_bytes = ?, client_disconnected = ?, error_message = ? WHERE id = ?",
                [
                    record.first_byte_at.map(str::to_owned).into(),
                    timestamp().into(),
                    i32::from(record.status).into(),
                    (record.response_bytes as i64).into(),
                    record.client_disconnected.into(),
                    record.error_message.map(str::to_owned).into(),
                    record.id.to_string().into(),
                ],
            ))
            .await?;
        let response_payload = self.store_payload(record.response_body).await?;
        self.database
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "UPDATE gateway_payloads SET response_body_id = ?, response_truncated = ? WHERE request_id = ?",
                [
                    response_payload.map(|payload| payload.id).into(),
                    record.response_truncated.into(),
                    record.id.to_string().into(),
                ],
            ))
            .await?;
        if record.usage.raw_json.is_some() {
            self.database
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    "INSERT OR REPLACE INTO gateway_usage (request_id, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens, raw_usage_json, cost_nanodollars, cost_source) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    [
                        record.id.to_string().into(),
                        record.usage.input_tokens.into(),
                        record.usage.output_tokens.into(),
                        record.usage.cache_read_tokens.into(),
                        record.usage.cache_write_tokens.into(),
                        record.usage.reasoning_tokens.into(),
                        record.usage.raw_json.clone().into(),
                        record.cost.nanodollars.into(),
                        record.cost.source.as_str().into(),
                    ],
                ))
                .await?;
        }
        Ok(())
    }

    async fn store_chunked_payload(
        &self,
        request_id: Uuid,
        body: &[u8],
    ) -> Result<(), sea_orm::DbErr> {
        for (position, payload) in chunk_payload(body).into_iter().enumerate() {
            let stored = self.store_encoded(payload).await?;
            self.database
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    "INSERT INTO gateway_payload_part_refs (request_id, direction, path, position, kind, part_id) VALUES (?, 'request', '$bytes', ?, 'chunk', ?)",
                    [
                        request_id.to_string().into(),
                        (position as i64).into(),
                        stored.id.into(),
                    ],
                ))
                .await?;
        }
        Ok(())
    }

    async fn store_semantic_payload(
        &self,
        request_id: Uuid,
        payload: SemanticPayload,
    ) -> Result<(), sea_orm::DbErr> {
        let envelope = self.store_encoded(payload.envelope).await?;
        self.database
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO gateway_payload_envelopes (request_id, direction, body_id) VALUES (?, 'request', ?)",
                [request_id.to_string().into(), envelope.id.into()],
            ))
            .await?;
        for part in payload.parts {
            let stored = self.store_encoded(part.payload).await?;
            self.database
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    "INSERT INTO gateway_payload_part_refs (request_id, direction, path, position, role, kind, part_id) VALUES (?, 'request', ?, ?, ?, ?, ?)",
                    [
                        request_id.to_string().into(),
                        part.path.into(),
                        part.position.into(),
                        part.role.into(),
                        part.kind.into(),
                        stored.id.into(),
                    ],
                ))
                .await?;
        }
        Ok(())
    }

    async fn store_encoded(&self, payload: StoredPayload) -> Result<StoredPayload, sea_orm::DbErr> {
        self.database
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT OR IGNORE INTO gateway_payload_blobs (id, body, encoding, original_bytes, created_at) VALUES (?, ?, ?, ?, ?)",
                [
                    payload.id.clone().into(),
                    payload.body.clone().into(),
                    payload.encoding.into(),
                    payload.original_bytes.into(),
                    timestamp().into(),
                ],
            ))
            .await?;
        Ok(payload)
    }

    async fn store_payload(&self, body: &[u8]) -> Result<Option<StoredPayload>, sea_orm::DbErr> {
        let Some(payload) = encode_payload(body) else {
            return Ok(None);
        };
        Ok(Some(self.store_encoded(payload).await?))
    }

    pub async fn reconcile_interrupted(&self) -> Result<u64, sea_orm::DbErr> {
        let cutoff = (Utc::now() - INTERRUPTED_AFTER).to_rfc3339_opts(SecondsFormat::Millis, true);
        let result = self
            .database
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "UPDATE gateway_requests SET completed_at = ?, error_message = ? \
                 WHERE completed_at IS NULL AND http_status IS NULL AND error_message IS NULL \
                 AND started_at < ?",
                [
                    timestamp().into(),
                    INTERRUPTED_MESSAGE.into(),
                    cutoff.into(),
                ],
            ))
            .await?;
        Ok(result.rows_affected())
    }

    pub async fn fail(&self, id: Uuid, message: &str) {
        if let Err(error) = self
            .database
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "UPDATE gateway_requests SET completed_at = ?, error_message = ? WHERE id = ?",
                [
                    timestamp().into(),
                    message.to_owned().into(),
                    id.to_string().into(),
                ],
            ))
            .await
        {
            tracing::error!(%error, %id, "failed to persist gateway failure");
        }
    }
}

pub fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use chrono::DateTime;
    use sea_orm::Database;
    use sea_orm_migration::MigratorTrait;

    async fn record(db: &DatabaseConnection, id: &str, started_at: &str, status: Option<i32>) {
        let status = match status {
            Some(status) => status.to_string(),
            None => "NULL".to_owned(),
        };
        db.execute_unprepared(&format!(
            "INSERT INTO gateway_requests(id,request_id,provider,protocol,method,endpoint,started_at,request_bytes,response_bytes,client_disconnected,http_status) \
             VALUES('{id}','{id}','claude','anthropic_messages','POST','/providers/claude/v1/messages','{started_at}',0,0,FALSE,{status})"
        ))
        .await
        .unwrap();
    }

    fn outcome(row: &(Option<String>, Option<String>)) -> (bool, bool) {
        (row.0.is_some(), row.1.is_some())
    }

    const REQUEST_BODY: &[u8] =
        br#"{"model":"claude-x","system":"be brief","messages":[{"role":"user","content":"hi"}]}"#;

    fn assert_split(body: &[u8]) {
        let payload = split_request(body, "anthropic_messages").expect("the body must parse");
        let paths: Vec<_> = payload
            .parts
            .iter()
            .map(|part| part.path.as_str())
            .collect();
        assert_eq!(paths, ["system", "messages"]);
        assert_eq!(payload.parts[1].role.as_deref(), Some("user"));
    }

    #[test]
    fn splits_a_plain_json_body() {
        assert_split(REQUEST_BODY);
    }

    #[test]
    fn splits_a_gzip_compressed_body() {
        assert_split(&crate::compression::tests::gzip(REQUEST_BODY));
    }

    #[test]
    fn splits_a_zstd_compressed_body() {
        assert_split(&zstd::encode_all(REQUEST_BODY, 0).unwrap());
    }

    #[test]
    fn splits_a_brotli_compressed_body() {
        assert_split(&crate::compression::tests::brotli(REQUEST_BODY));
    }

    const MIXED_CONTENT_BODY: &[u8] = br#"{
        "model": "claude-x",
        "system": [{"type": "text", "text": "be brief"}],
        "messages": [
            {"role": "user", "content": "plain string content"},
            {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "weighing it up", "signature": "sig"},
                    {"type": "text", "text": "let me look"},
                    {"type": "tool_use", "id": "call_1", "name": "read", "input": {"path": "a.rs"}}
                ]
            },
            {
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "file body"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBOR"}}
                ]
            }
        ],
        "tools": [{"name": "read", "input_schema": {"type": "object"}}]
    }"#;

    fn part_body(part: &SemanticPart) -> Vec<u8> {
        decode_body(&part.payload.body)
    }

    fn stored_parts(payload: &SemanticPayload) -> Vec<StoredPart> {
        payload
            .parts
            .iter()
            .map(|part| StoredPart {
                path: part.path.clone(),
                position: part.position,
                body: part_body(part),
            })
            .collect()
    }

    fn indexed(payload: &SemanticPayload) -> Vec<(&str, i64, &str, Option<&str>)> {
        payload
            .parts
            .iter()
            .map(|part| {
                (
                    part.path.as_str(),
                    part.position,
                    part.kind.as_str(),
                    part.role.as_deref(),
                )
            })
            .collect()
    }

    #[test]
    fn message_content_blocks_are_indexed_under_their_message() {
        let payload =
            split_request(MIXED_CONTENT_BODY, "anthropic_messages").expect("the body must parse");
        assert_eq!(
            indexed(&payload),
            [
                ("system", 0, "text", None),
                ("messages", 0, "messages", Some("user")),
                ("messages", 1, "messages", Some("assistant")),
                ("messages/1/content", 0, "thinking", Some("assistant")),
                ("messages/1/content", 1, "text", Some("assistant")),
                ("messages/1/content", 2, "tool_use", Some("assistant")),
                ("messages", 2, "messages", Some("user")),
                ("messages/2/content", 0, "tool_result", Some("user")),
                ("messages/2/content", 1, "image", Some("user")),
                ("tools", 0, "tools", None),
            ]
        );
    }

    #[test]
    fn a_message_shell_does_not_repeat_the_bytes_of_its_content_blocks() {
        let payload =
            split_request(MIXED_CONTENT_BODY, "anthropic_messages").expect("the body must parse");
        let shells: Vec<serde_json::Value> = payload
            .parts
            .iter()
            .filter(|part| part.path == "messages")
            .map(|part| serde_json::from_slice(&part_body(part)).unwrap())
            .collect();
        assert_eq!(
            shells,
            [
                serde_json::json!({"role": "user", "content": "plain string content"}),
                serde_json::json!({"role": "assistant", "content": []}),
                serde_json::json!({"role": "user", "content": []}),
            ]
        );
    }

    #[test]
    fn a_string_content_message_keeps_a_single_row() {
        let payload =
            split_request(REQUEST_BODY, "anthropic_messages").expect("the body must parse");
        assert_eq!(
            indexed(&payload),
            [
                ("system", 0, "system", None),
                ("messages", 0, "messages", Some("user")),
            ]
        );
    }

    #[test]
    fn codex_input_items_are_not_split_any_further() {
        let body = br#"{"model":"gpt-x","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;
        let payload = split_request(body, "openai_responses").expect("the body must parse");
        assert_eq!(indexed(&payload), [("input", 0, "message", Some("user"))]);
    }

    fn assert_round_trips(body: &[u8], protocol: &str) {
        let payload = split_request(body, protocol).expect("the body must parse");
        let envelope = decode_body(&payload.envelope.body);
        let rebuilt = reassemble_request(&envelope, stored_parts(&payload), protocol)
            .expect("the parts must reassemble");
        let original: serde_json::Value = serde_json::from_slice(body).unwrap();
        let rebuilt: serde_json::Value = serde_json::from_slice(&rebuilt).unwrap();
        assert_eq!(rebuilt, original);
    }

    #[test]
    fn a_message_mixing_every_content_kind_reassembles_into_the_original_body() {
        assert_round_trips(MIXED_CONTENT_BODY, "anthropic_messages");
    }

    #[test]
    fn scalar_and_absent_fields_reassemble_into_the_original_body() {
        assert_round_trips(REQUEST_BODY, "anthropic_messages");
        assert_round_trips(
            br#"{"model":"gpt-x","instructions":"be brief","input":"hi","tools":[]}"#,
            "openai_responses",
        );
        assert_round_trips(
            br#"{"model":"claude-x","system":null,"messages":[{"role":"user","content":[]}]}"#,
            "anthropic_messages",
        );
    }

    #[test]
    fn splitting_an_already_split_body_is_stable() {
        let once = split_request(MIXED_CONTENT_BODY, "anthropic_messages").unwrap();
        let envelope = decode_body(&once.envelope.body);
        let rebuilt =
            reassemble_request(&envelope, stored_parts(&once), "anthropic_messages").unwrap();
        let twice = split_request(&rebuilt, "anthropic_messages").unwrap();
        assert_eq!(indexed(&once), indexed(&twice));
        assert_eq!(once.envelope.id, twice.envelope.id);
        let ids = |payload: &SemanticPayload| {
            payload
                .parts
                .iter()
                .map(|part| part.payload.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&once), ids(&twice));
    }

    #[test]
    fn a_body_that_is_not_json_in_any_encoding_falls_back_to_chunking() {
        let body = b"not json, not compressed, not anything";
        assert!(split_request(body, "anthropic_messages").is_none());
        assert!(!chunk_payload(body).is_empty());
    }

    #[tokio::test]
    async fn interrupted_requests_are_closed_out_once_they_are_older_than_the_timeout() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let at = |ago: TimeDelta| (Utc::now() - ago).to_rfc3339_opts(SecondsFormat::Millis, true);

        record(&db, "stranded", &at(TimeDelta::hours(4)), None).await;
        record(&db, "recent", &at(TimeDelta::minutes(2)), None).await;
        record(&db, "finished", &at(TimeDelta::hours(4)), Some(200)).await;

        let sink = SqliteSink::new(db.clone());
        assert_eq!(sink.reconcile_interrupted().await.unwrap(), 1);
        assert_eq!(sink.reconcile_interrupted().await.unwrap(), 0);

        let read = |id: &'static str| {
            let db = db.clone();
            async move {
                let row = db
                    .query_one_raw(Statement::from_string(
                        DbBackend::Sqlite,
                        format!(
                            "SELECT completed_at, error_message, http_status FROM gateway_requests WHERE id = '{id}'"
                        ),
                    ))
                    .await
                    .unwrap()
                    .unwrap();
                (
                    row.try_get::<Option<String>>("", "completed_at").unwrap(),
                    row.try_get::<Option<String>>("", "error_message").unwrap(),
                    row.try_get::<Option<i32>>("", "http_status").unwrap(),
                )
            }
        };

        let stranded = read("stranded").await;
        assert_eq!(
            outcome(&(stranded.0.clone(), stranded.1.clone())),
            (true, true)
        );
        assert_eq!(stranded.1.as_deref(), Some(INTERRUPTED_MESSAGE));
        assert_eq!(stranded.2, None);
        assert!(DateTime::parse_from_rfc3339(stranded.0.as_deref().unwrap()).is_ok());

        let recent = read("recent").await;
        assert_eq!(outcome(&(recent.0, recent.1)), (false, false));

        let finished = read("finished").await;
        assert_eq!(finished.1, None, "a completed request is left alone");
        assert_eq!(finished.2, Some(200));
    }
}
