use crate::{
    compression::decode_body,
    db::begin_immediate,
    telemetry::{SemanticPayload, StoredPart, reassemble_request, split_request, store_blob},
};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement, TransactionTrait};

pub const NAME: &str = "payload_resplit";

const SCAN_BATCH: i64 = 128;

struct Request {
    id: String,
    protocol: String,
}

pub async fn run(database: &DatabaseConnection) -> Result<u64, DbErr> {
    let mut after = String::new();
    let mut converted = 0;
    loop {
        let batch = unsplit_requests(database, &after).await?;
        let Some(last) = batch.last() else {
            return Ok(converted);
        };
        after = last.id.clone();
        for request in batch {
            if resplit(database, &request).await? {
                converted += 1;
            }
            tracing::debug!(request_id = %request.id, converted, "payload resplit progress");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

async fn unsplit_requests(
    database: &impl ConnectionTrait,
    after: &str,
) -> Result<Vec<Request>, DbErr> {
    let rows = database
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT g.id, g.protocol
             FROM gateway_requests g
             WHERE g.id > ?
               AND (EXISTS (SELECT 1 FROM gateway_payload_part_refs c
                            WHERE c.request_id = g.id AND c.direction = 'request'
                              AND c.path = '$bytes')
                    OR (g.protocol = 'anthropic_messages'
                        AND EXISTS (SELECT 1 FROM gateway_payload_part_refs m
                                    WHERE m.request_id = g.id AND m.direction = 'request'
                                      AND m.path = 'messages')
                        AND NOT EXISTS (SELECT 1 FROM gateway_payload_part_refs b
                                        WHERE b.request_id = g.id AND b.direction = 'request'
                                          AND b.path LIKE 'messages/%/content')))
             ORDER BY g.id
             LIMIT ?",
            [after.to_owned().into(), SCAN_BATCH.into()],
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

// Compare reference identities, not decompressed bodies, while holding the write lock.
// Blob IDs are content hashes and blobs are immutable.
async fn source_state(
    database: &impl ConnectionTrait,
    request: &Request,
) -> Result<SourceState, DbErr> {
    let rows = database
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT path, position, part_id AS blob_id, role, kind FROM gateway_payload_part_refs
         WHERE request_id = ? AND direction = 'request'
         UNION ALL SELECT '$envelope', -1, body_id, NULL, '' FROM gateway_payload_envelopes
         WHERE request_id = ? AND direction = 'request'
         UNION ALL SELECT '$request', -1, protocol, NULL, '' FROM gateway_requests WHERE id = ?
         ORDER BY 1, 2, 3, 4, 5",
            [
                request.id.clone().into(),
                request.id.clone().into(),
                request.id.clone().into(),
            ],
        ))
        .await?;
    rows.into_iter()
        .map(|row| {
            Ok((
                row.try_get("", "path")?,
                row.try_get("", "position")?,
                row.try_get("", "blob_id")?,
                row.try_get("", "role")?,
                row.try_get("", "kind")?,
            ))
        })
        .collect()
}

async fn resplit(database: &DatabaseConnection, request: &Request) -> Result<bool, DbErr> {
    for _ in 0..3 {
        // A read snapshot keeps the envelope and parts consistent without reserving the writer.
        let snapshot = database.begin().await?;
        let state = source_state(&snapshot, request).await?;
        let Some(protocol) = state
            .iter()
            .find(|row| row.0 == "$request")
            .map(|row| row.2.clone())
        else {
            snapshot.commit().await?;
            return Ok(false);
        };
        let current = Request {
            id: request.id.clone(),
            protocol,
        };
        let body = original_body(&snapshot, &current).await?;
        snapshot.commit().await?;
        let Some(body) = body else {
            return Ok(false);
        };
        let Some(payload) = split_request(&body, &current.protocol) else {
            tracing::warn!(request_id = %request.id, "kept the existing payload parts: the reassembled body did not parse");
            return Ok(false);
        };
        if replace_if_unchanged(database, request, &state, payload).await? {
            return Ok(true);
        }
        tokio::task::yield_now().await;
    }
    Err(DbErr::Custom(format!(
        "payload source kept changing for {}",
        request.id
    )))
}

type SourceState = Vec<(String, i64, String, Option<String>, String)>;

async fn replace_if_unchanged(
    database: &DatabaseConnection,
    request: &Request,
    state: &SourceState,
    payload: SemanticPayload,
) -> Result<bool, DbErr> {
    let transaction = begin_immediate(database).await?;
    if source_state(&transaction, request).await? != *state {
        transaction.rollback().await?;
        return Ok(false);
    }
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "DELETE FROM gateway_payload_part_refs WHERE request_id = ? AND direction = 'request'",
            [request.id.clone().into()],
        ))
        .await?;
    store_semantic(&transaction, &request.id, payload).await?;
    transaction.commit().await?;
    Ok(true)
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
    store_blob(database, &payload.envelope).await?;
    database
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT OR REPLACE INTO gateway_payload_envelopes (request_id, direction, body_id) VALUES (?, 'request', ?)",
            [request_id.to_owned().into(), payload.envelope.id.into()],
        ))
        .await?;
    for part in payload.parts {
        store_blob(database, &part.payload).await?;
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
                    part.payload.id.into(),
                ],
            ))
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        migration::Migrator,
        telemetry::{StoredPayload, timestamp},
    };
    use sea_orm::Database;
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

    async fn request_row(database: &DatabaseConnection, protocol: &str, endpoint: &str) {
        database
            .execute_unprepared(&format!(
                "INSERT INTO gateway_requests(id,request_id,provider,protocol,method,endpoint,started_at,request_bytes,response_bytes,client_disconnected) \
                 VALUES('{REQUEST_ID}','{REQUEST_ID}','claude','{protocol}','POST','{endpoint}','2026-09-01T00:00:00.000Z',0,0,FALSE)"
            ))
            .await
            .unwrap();
    }

    async fn message_level_capture(database: &DatabaseConnection) {
        request_row(
            database,
            "anthropic_messages",
            "/providers/claude/v1/messages",
        )
        .await;
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
    async fn resplit_and_large_foreground_captures_progress_with_reporting_readers() {
        use crate::{
            pricing::{Cost, CostSource},
            providers::{Provider, Usage},
            telemetry::{CompletionRecord, SqliteSink},
        };
        let fixture = crate::db::tests::FileDatabase::new().await;
        message_level_capture(&fixture.database).await;
        let reporting = crate::db::reporting_connection(&fixture.url, &fixture.database)
            .await
            .unwrap();
        let first = reporting.begin().await.unwrap();
        let second = reporting.begin().await.unwrap();
        first
            .execute_unprepared("SELECT COUNT(*) FROM gateway_requests")
            .await
            .unwrap();
        second
            .execute_unprepared("SELECT COUNT(*) FROM gateway_requests")
            .await
            .unwrap();
        let body = serde_json::to_vec(&serde_json::json!({"model":"claude-x", "messages":[{"role":"user","content":"capture ".repeat(32000)}]})).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let job = run(&fixture.database);
            let captures = futures_util::future::join_all((0..8).map(|_| async {
                let id = crate::request_metrics::tests::started(
                    &fixture.database,
                    Provider::Anthropic,
                    &body,
                )
                .await;
                SqliteSink::new(fixture.database.clone())
                    .complete(CompletionRecord {
                        id: uuid::Uuid::parse_str(&id).unwrap(),
                        status: 200,
                        first_byte_at: None,
                        response_body: &body,
                        response_bytes: body.len(),
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
            }));
            let (converted, _) = tokio::join!(job, captures);
            assert!(converted.unwrap() >= 1);
        })
        .await
        .expect("both background and foreground writers must progress");
        first.rollback().await.unwrap();
        second.rollback().await.unwrap();
        reporting.close().await.unwrap();
        fixture.database.close_by_ref().await.unwrap();
    }

    #[tokio::test]
    async fn replacement_rejects_stale_preparation_without_deleting_new_refs() {
        let fixture = crate::db::tests::FileDatabase::new().await;
        let db = &fixture.database;
        message_level_capture(db).await;
        let request = Request {
            id: REQUEST_ID.into(),
            protocol: "anthropic_messages".into(),
        };
        let state = source_state(db, &request).await.unwrap();
        let body = original_body(db, &request).await.unwrap().unwrap();
        let prepared = split_request(&body, &request.protocol).unwrap();
        db.execute_unprepared(
            "UPDATE gateway_payload_part_refs SET role = 'assistant' WHERE position = 0",
        )
        .await
        .unwrap();
        let changed = source_state(db, &request).await.unwrap();
        assert!(
            !replace_if_unchanged(db, &request, &state, prepared)
                .await
                .unwrap()
        );
        assert_eq!(source_state(db, &request).await.unwrap(), changed);
        assert_eq!(run(db).await.unwrap(), 1);
        db.close_by_ref().await.unwrap();
    }

    #[tokio::test]
    async fn message_level_refs_are_replaced_by_shells_and_content_blocks() {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&database, None).await.unwrap();
        message_level_capture(&database).await;

        assert_eq!(run(&database).await.unwrap(), 1);
        let converted = refs(&database).await;

        let kinds: Vec<_> = converted
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
            serde_json::from_str::<serde_json::Value>(&converted[0].3).unwrap(),
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

        assert_eq!(
            run(&database).await.unwrap(),
            0,
            "a converted request is not picked up again"
        );
        assert_eq!(refs(&database).await, converted);
    }

    #[tokio::test]
    async fn unparseable_chunk_refs_are_folded_back_into_semantic_parts() {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&database, None).await.unwrap();
        request_row(
            &database,
            "openai_responses",
            "/providers/codex/codex/responses",
        )
        .await;
        let compressed = zstd::encode_all(
            br#"{"model":"gpt-x","instructions":"be brief","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#.as_slice(),
            0,
        )
        .unwrap();
        for (position, chunk) in compressed.chunks(8).enumerate() {
            let payload = StoredPayload::new(chunk).unwrap();
            store_blob(&database, &payload).await.unwrap();
            database
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    "INSERT INTO gateway_payload_part_refs (request_id, direction, path, position, kind, part_id) VALUES (?, 'request', '$bytes', ?, 'chunk', ?)",
                    [REQUEST_ID.into(), (position as i64).into(), payload.id.into()],
                ))
                .await
                .unwrap();
        }

        assert_eq!(run(&database).await.unwrap(), 1);

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
        assert_eq!(run(&database).await.unwrap(), 0);
    }
}
