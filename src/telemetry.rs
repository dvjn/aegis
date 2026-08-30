use crate::providers::{Provider, Usage};
use chrono::{SecondsFormat, TimeDelta, Utc};
use flate2::{Compression, write::GzEncoder};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};
use sha2::{Digest, Sha256};
use std::io::Write;
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
    pub path: &'static str,
    pub position: i64,
    pub role: Option<String>,
    pub kind: String,
    pub payload: StoredPayload,
}

pub(crate) struct SemanticPayload {
    pub envelope: StoredPayload,
    pub parts: Vec<SemanticPart>,
}

pub(crate) fn split_request(body: &[u8], protocol: &str) -> Option<SemanticPayload> {
    let mut root = serde_json::from_slice::<serde_json::Value>(body).ok()?;
    let object = root.as_object_mut()?;
    let paths: &[&'static str] = match protocol {
        "anthropic_messages" => &["system", "messages", "tools"],
        "openai_responses" => &["instructions", "input", "tools"],
        _ => return None,
    };
    let mut parts = Vec::new();
    for path in paths {
        let Some(value) = object.get_mut(*path) else {
            continue;
        };
        let values = match value {
            serde_json::Value::Array(items) => std::mem::take(items),
            serde_json::Value::Null => Vec::new(),
            _ => vec![std::mem::replace(value, serde_json::Value::Null)],
        };
        for (position, value) in values.into_iter().enumerate() {
            let role = value
                .get("role")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            let kind = value
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(path)
                .to_owned();
            let encoded = serde_json::to_vec(&value).ok()?;
            parts.push(SemanticPart {
                path,
                position: position as i64,
                role,
                kind,
                payload: encode_payload(&encoded)?,
            });
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

pub struct CompletionRecord<'a> {
    pub id: Uuid,
    pub status: u16,
    pub first_byte_at: Option<&'a str>,
    pub response_body: &'a [u8],
    pub response_bytes: usize,
    pub response_truncated: bool,
    pub client_disconnected: bool,
    pub usage: &'a Usage,
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
                    "INSERT OR REPLACE INTO gateway_usage (request_id, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens, raw_usage_json) VALUES (?, ?, ?, ?, ?, ?, ?)",
                    [
                        record.id.to_string().into(),
                        record.usage.input_tokens.into(),
                        record.usage.output_tokens.into(),
                        record.usage.cache_read_tokens.into(),
                        record.usage.cache_write_tokens.into(),
                        record.usage.reasoning_tokens.into(),
                        record.usage.raw_json.clone().into(),
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
