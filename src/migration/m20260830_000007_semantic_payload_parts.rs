use crate::telemetry::{SemanticPayload, StoredPayload, chunk_payload, split_request, timestamp};
use flate2::read::GzDecoder;
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use sea_orm_migration::prelude::*;
use std::io::Read;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let database = manager.get_connection();
        database
            .execute_unprepared(
                "CREATE TABLE gateway_payload_envelopes (
                    request_id TEXT NOT NULL,
                    direction TEXT NOT NULL CHECK (direction IN ('request', 'response')),
                    body_id TEXT NOT NULL,
                    PRIMARY KEY (request_id, direction),
                    FOREIGN KEY (request_id) REFERENCES gateway_requests(id) ON DELETE CASCADE,
                    FOREIGN KEY (body_id) REFERENCES gateway_payload_blobs(id)
                )",
            )
            .await?;
        database
            .execute_unprepared(
                "CREATE TABLE gateway_payload_part_refs (
                    request_id TEXT NOT NULL,
                    direction TEXT NOT NULL CHECK (direction IN ('request', 'response')),
                    path TEXT NOT NULL,
                    position INTEGER NOT NULL,
                    role TEXT,
                    kind TEXT NOT NULL,
                    part_id TEXT NOT NULL,
                    PRIMARY KEY (request_id, direction, path, position),
                    FOREIGN KEY (request_id) REFERENCES gateway_requests(id) ON DELETE CASCADE,
                    FOREIGN KEY (part_id) REFERENCES gateway_payload_blobs(id)
                )",
            )
            .await?;
        database
            .execute_unprepared(
                "CREATE INDEX idx_gateway_payload_part_refs_part ON gateway_payload_part_refs(part_id)",
            )
            .await?;

        let rows = database
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT r.id, r.protocol, b.body, b.encoding
                 FROM gateway_requests r
                 JOIN gateway_payloads p ON p.request_id = r.id
                 JOIN gateway_payload_blobs b ON b.id = p.request_body_id",
            ))
            .await?;
        for row in rows {
            let request_id: String = row.try_get("", "id")?;
            let protocol: String = row.try_get("", "protocol")?;
            let body: Vec<u8> = row.try_get("", "body")?;
            let encoding: String = row.try_get("", "encoding")?;
            let body = decode(body, &encoding)?;
            if let Some(payload) = split_request(&body, &protocol) {
                store_semantic(database, &request_id, payload).await?;
            } else {
                store_chunks(database, &request_id, &body).await?;
            }
            database
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    "UPDATE gateway_payloads SET request_body_id = NULL WHERE request_id = ?",
                    [request_id.into()],
                ))
                .await?;
        }
        database
            .execute_unprepared(
                "DELETE FROM gateway_payload_blobs
                 WHERE id NOT IN (SELECT request_body_id FROM gateway_payloads WHERE request_body_id IS NOT NULL)
                   AND id NOT IN (SELECT response_body_id FROM gateway_payloads WHERE response_body_id IS NOT NULL)
                   AND id NOT IN (SELECT body_id FROM gateway_payload_envelopes)
                   AND id NOT IN (SELECT part_id FROM gateway_payload_part_refs)",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}

async fn store_chunks(
    database: &impl ConnectionTrait,
    request_id: &str,
    body: &[u8],
) -> Result<(), DbErr> {
    for (position, payload) in chunk_payload(body).into_iter().enumerate() {
        let id = store(database, payload).await?;
        database
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO gateway_payload_part_refs (request_id, direction, path, position, kind, part_id) VALUES (?, 'request', '$bytes', ?, 'chunk', ?)",
                [
                    request_id.to_owned().into(),
                    (position as i64).into(),
                    id.into(),
                ],
            ))
            .await?;
    }
    Ok(())
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
            "INSERT INTO gateway_payload_envelopes (request_id, direction, body_id) VALUES (?, 'request', ?)",
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
    database
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT OR IGNORE INTO gateway_payload_blobs (id, body, encoding, original_bytes, created_at) VALUES (?, ?, ?, ?, ?)",
            [
                payload.id.clone().into(),
                payload.body.into(),
                payload.encoding.into(),
                payload.original_bytes.into(),
                timestamp().into(),
            ],
        ))
        .await?;
    Ok(payload.id)
}

fn decode(body: Vec<u8>, encoding: &str) -> Result<Vec<u8>, DbErr> {
    if encoding != "gzip" {
        return Ok(body);
    }
    let mut decoded = Vec::new();
    GzDecoder::new(body.as_slice())
        .read_to_end(&mut decoded)
        .map_err(|error| DbErr::Migration(error.to_string()))?;
    Ok(decoded)
}
