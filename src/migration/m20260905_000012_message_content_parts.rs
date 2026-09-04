use crate::{
    compression::decode_body,
    telemetry::{
        SemanticPayload, StoredPart, StoredPayload, reassemble_request, split_request, store_blob,
    },
};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

struct Request {
    id: String,
    protocol: String,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let database = manager.get_connection();
        for request in requests(database).await? {
            let Some(body) = original_body(database, &request).await? else {
                continue;
            };
            let Some(payload) = split_request(&body, &request.protocol) else {
                tracing::warn!(
                    request_id = %request.id,
                    "kept the existing payload parts: the reassembled body did not parse"
                );
                continue;
            };
            database
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    "DELETE FROM gateway_payload_part_refs WHERE request_id = ? AND direction = 'request'",
                    [request.id.clone().into()],
                ))
                .await?;
            store_semantic(database, &request.id, payload).await?;
        }
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}

async fn requests(database: &impl ConnectionTrait) -> Result<Vec<Request>, DbErr> {
    let rows = database
        .query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT id, protocol FROM gateway_requests ORDER BY id",
        ))
        .await?;
    rows.into_iter()
        .map(|row| {
            Ok(Request {
                id: row.try_get("", "id")?,
                protocol: row.try_get("", "protocol")?,
            })
        })
        .collect()
}

async fn original_body(
    database: &impl ConnectionTrait,
    request: &Request,
) -> Result<Option<Vec<u8>>, DbErr> {
    let parts = stored_parts(database, &request.id).await?;
    if parts.is_empty() {
        return Ok(None);
    }
    let Some(envelope) = envelope_body(database, &request.id).await? else {
        return Ok(Some(concatenated(parts)));
    };
    Ok(reassemble_request(&envelope, parts, &request.protocol))
}

async fn stored_parts(
    database: &impl ConnectionTrait,
    request_id: &str,
) -> Result<Vec<StoredPart>, DbErr> {
    let rows = database
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT r.path, r.position, b.body
             FROM gateway_payload_part_refs r
             JOIN gateway_payload_blobs b ON b.id = r.part_id
             WHERE r.request_id = ? AND r.direction = 'request'
             ORDER BY r.path, r.position",
            [request_id.to_owned().into()],
        ))
        .await?;
    rows.into_iter()
        .map(|row| {
            let body: Vec<u8> = row.try_get("", "body")?;
            Ok(StoredPart {
                path: row.try_get("", "path")?,
                position: row.try_get("", "position")?,
                body: decode_body(&body),
            })
        })
        .collect()
}

async fn envelope_body(
    database: &impl ConnectionTrait,
    request_id: &str,
) -> Result<Option<Vec<u8>>, DbErr> {
    let row = database
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT b.body
             FROM gateway_payload_envelopes e
             JOIN gateway_payload_blobs b ON b.id = e.body_id
             WHERE e.request_id = ? AND e.direction = 'request'",
            [request_id.to_owned().into()],
        ))
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let body: Vec<u8> = row.try_get("", "body")?;
    Ok(Some(decode_body(&body)))
}

fn concatenated(mut parts: Vec<StoredPart>) -> Vec<u8> {
    parts.sort_by_key(|part| part.position);
    parts.into_iter().flat_map(|part| part.body).collect()
}

async fn store_semantic(
    database: &impl ConnectionTrait,
    request_id: &str,
    payload: SemanticPayload,
) -> Result<(), DbErr> {
    let envelope = store(database, payload.envelope).await?;
    database
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT OR REPLACE INTO gateway_payload_envelopes (request_id, direction, body_id) VALUES (?, 'request', ?)",
            [request_id.to_owned().into(), envelope.into()],
        ))
        .await?;
    for part in payload.parts {
        let id = store(database, part.payload).await?;
        database
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO gateway_payload_part_refs (request_id, direction, path, position, role, kind, part_id) VALUES (?, 'request', ?, ?, ?, ?, ?)",
                [
                    request_id.to_owned().into(),
                    part.path.into(),
                    part.position.into(),
                    part.role.into(),
                    part.kind.into(),
                    id.into(),
                ],
            ))
            .await?;
    }
    Ok(())
}

async fn store(database: &impl ConnectionTrait, payload: StoredPayload) -> Result<String, DbErr> {
    store_blob(database, &payload).await?;
    Ok(payload.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{migration::Migrator, telemetry::timestamp};
    use sea_orm::{Database, DatabaseConnection};
    use sea_orm_migration::MigratorTrait;
    use sha2::{Digest, Sha256};

    const BODY: &[u8] = br#"{"model":"claude-x","messages":[{"role":"user","content":[{"type":"text","text":"hi"},{"type":"tool_result","tool_use_id":"c1","content":"out"}]},{"role":"assistant","content":"ok"}]}"#;
    const REQUEST_ID: &str = "01930000-0000-7000-8000-000000000001";

    async fn blob(database: &DatabaseConnection, body: &str) -> String {
        let id: String = Sha256::digest(body.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        database
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT OR IGNORE INTO gateway_payload_blobs (id, body, encoding, original_bytes, created_at) VALUES (?, ?, 'identity', ?, ?)",
                [
                    id.clone().into(),
                    body.as_bytes().to_vec().into(),
                    (body.len() as i64).into(),
                    timestamp().into(),
                ],
            ))
            .await
            .unwrap();
        id
    }

    async fn message_level_capture(database: &DatabaseConnection) {
        database
            .execute_unprepared(&format!(
                "INSERT INTO gateway_requests(id,request_id,provider,protocol,method,endpoint,started_at,request_bytes,response_bytes,client_disconnected) \
                 VALUES('{REQUEST_ID}','{REQUEST_ID}','claude','anthropic_messages','POST','/providers/claude/v1/messages','2026-09-01T00:00:00.000Z',0,0,FALSE)"
            ))
            .await
            .unwrap();
        let envelope = blob(database, r#"{"model":"claude-x","messages":[]}"#).await;
        database
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO gateway_payload_envelopes (request_id, direction, body_id) VALUES (?, 'request', ?)",
                [REQUEST_ID.into(), envelope.into()],
            ))
            .await
            .unwrap();
        let messages = [
            r#"{"role":"user","content":[{"type":"text","text":"hi"},{"type":"tool_result","tool_use_id":"c1","content":"out"}]}"#,
            r#"{"role":"assistant","content":"ok"}"#,
        ];
        for (position, message) in messages.into_iter().enumerate() {
            let part = blob(database, message).await;
            database
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    "INSERT INTO gateway_payload_part_refs (request_id, direction, path, position, role, kind, part_id) VALUES (?, 'request', 'messages', ?, 'user', 'messages', ?)",
                    [REQUEST_ID.into(), (position as i64).into(), part.into()],
                ))
                .await
                .unwrap();
        }
    }

    async fn refs(database: &DatabaseConnection) -> Vec<(String, i64, String, String)> {
        database
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT r.path, r.position, r.kind, CAST(b.body AS TEXT) body
                 FROM gateway_payload_part_refs r
                 JOIN gateway_payload_blobs b ON b.id = r.part_id
                 ORDER BY r.path, r.position",
            ))
            .await
            .unwrap()
            .into_iter()
            .map(|row| {
                (
                    row.try_get("", "path").unwrap(),
                    row.try_get("", "position").unwrap(),
                    row.try_get("", "kind").unwrap(),
                    row.try_get("", "body").unwrap(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn message_level_refs_are_replaced_by_shells_and_content_blocks() {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&database, None).await.unwrap();
        message_level_capture(&database).await;

        Migration.up(&SchemaManager::new(&database)).await.unwrap();
        let migrated = refs(&database).await;

        let kinds: Vec<_> = migrated
            .iter()
            .map(|(path, position, kind, _)| (path.as_str(), *position, kind.as_str()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("messages", 0, "messages"),
                ("messages", 1, "messages"),
                ("messages/0/content", 0, "text"),
                ("messages/0/content", 1, "tool_result"),
            ]
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&migrated[0].3).unwrap(),
            serde_json::json!({"role": "user", "content": []}),
            "the shell must not repeat the bytes of its content blocks"
        );

        let request = Request {
            id: REQUEST_ID.to_owned(),
            protocol: "anthropic_messages".to_owned(),
        };
        let rebuilt = original_body(&database, &request).await.unwrap().unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&rebuilt).unwrap(),
            serde_json::from_slice::<serde_json::Value>(BODY).unwrap()
        );

        Migration.up(&SchemaManager::new(&database)).await.unwrap();
        assert_eq!(
            refs(&database).await,
            migrated,
            "the backfill is idempotent"
        );
    }

    #[tokio::test]
    async fn unparseable_chunk_refs_are_folded_back_into_semantic_parts() {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&database, None).await.unwrap();
        database
            .execute_unprepared(&format!(
                "INSERT INTO gateway_requests(id,request_id,provider,protocol,method,endpoint,started_at,request_bytes,response_bytes,client_disconnected) \
                 VALUES('{REQUEST_ID}','{REQUEST_ID}','codex','openai_responses','POST','/providers/codex/codex/responses','2026-09-01T00:00:00.000Z',0,0,FALSE)"
            ))
            .await
            .unwrap();
        let compressed = zstd::encode_all(
            br#"{"model":"gpt-x","instructions":"be brief","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#.as_slice(),
            0,
        )
        .unwrap();
        for (position, chunk) in compressed.chunks(8).enumerate() {
            let id = store(&database, StoredPayload::new(chunk).unwrap())
                .await
                .unwrap();
            database
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    "INSERT INTO gateway_payload_part_refs (request_id, direction, path, position, kind, part_id) VALUES (?, 'request', '$bytes', ?, 'chunk', ?)",
                    [REQUEST_ID.into(), (position as i64).into(), id.into()],
                ))
                .await
                .unwrap();
        }

        Migration.up(&SchemaManager::new(&database)).await.unwrap();

        let kinds: Vec<_> = refs(&database)
            .await
            .into_iter()
            .map(|(path, _, kind, _)| (path, kind))
            .collect();
        assert_eq!(
            kinds,
            [
                ("input".to_owned(), "message".to_owned()),
                ("instructions".to_owned(), "instructions".to_owned()),
            ]
        );
    }
}
