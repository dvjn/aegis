use crate::telemetry::{StoredPayload, timestamp};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let database = manager.get_connection();
        let columns = database
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                "PRAGMA table_info(gateway_payloads)",
            ))
            .await?;
        if columns.iter().any(|column| {
            column
                .try_get::<String>("", "name")
                .is_ok_and(|name| name == "request_body_id")
        }) {
            return Ok(());
        }

        database
            .execute_unprepared(
                "CREATE TABLE IF NOT EXISTS gateway_payload_blobs (
                    id TEXT PRIMARY KEY NOT NULL,
                    body BLOB NOT NULL,
                    encoding TEXT NOT NULL CHECK (encoding IN ('identity', 'gzip')),
                    original_bytes INTEGER NOT NULL,
                    created_at TEXT NOT NULL
                )",
            )
            .await?;
        database
            .execute_unprepared(
                "ALTER TABLE gateway_payloads ADD COLUMN request_body_id TEXT REFERENCES gateway_payload_blobs(id)",
            )
            .await?;
        database
            .execute_unprepared(
                "ALTER TABLE gateway_payloads ADD COLUMN response_body_id TEXT REFERENCES gateway_payload_blobs(id)",
            )
            .await?;

        let rows = database
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT request_id, request_body, response_body FROM gateway_payloads",
            ))
            .await?;
        for row in rows {
            let request_id: String = row.try_get("", "request_id")?;
            let request_body: Option<Vec<u8>> = row.try_get("", "request_body")?;
            let response_body: Option<Vec<u8>> = row.try_get("", "response_body")?;
            let request = store(database, request_body.as_deref()).await?;
            let response = store(database, response_body.as_deref()).await?;
            database
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    "UPDATE gateway_payloads SET request_body_id = ?, response_body_id = ?, request_body = NULL, response_body = NULL WHERE request_id = ?",
                    [request.into(), response.into(), request_id.into()],
                ))
                .await?;
        }
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}

async fn store(
    database: &impl ConnectionTrait,
    body: Option<&[u8]>,
) -> Result<Option<String>, DbErr> {
    let Some(payload) = body.and_then(StoredPayload::new) else {
        return Ok(None);
    };
    let (body, encoding) = payload.encoded();
    database
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT OR IGNORE INTO gateway_payload_blobs (id, body, encoding, original_bytes, created_at) VALUES (?, ?, ?, ?, ?)",
            [
                payload.id.clone().into(),
                body.into(),
                encoding.into(),
                payload.original_bytes.into(),
                timestamp().into(),
            ],
        ))
        .await?;
    Ok(Some(payload.id))
}
