use crate::{
    compression::decode_body,
    payload_facts::{BLOCK_TYPES, extract, store},
};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let database = manager.get_connection();
        let block_types = BLOCK_TYPES
            .map(|block_type| format!("'{block_type}'"))
            .join(", ");
        database
            .execute_unprepared(&format!(
                "CREATE TABLE IF NOT EXISTS gateway_payload_blob_facts (
                    blob_id TEXT NOT NULL,
                    ordinal INTEGER NOT NULL,
                    block_type TEXT NOT NULL CHECK (block_type IN ({block_types})),
                    tool_name TEXT,
                    mcp_server TEXT,
                    skill_name TEXT,
                    tool_use_id TEXT,
                    is_error INTEGER,
                    cache_ttl TEXT,
                    PRIMARY KEY (blob_id, ordinal),
                    FOREIGN KEY (blob_id) REFERENCES gateway_payload_blobs(id) ON DELETE CASCADE
                )"
            ))
            .await?;
        database
            .execute_unprepared(
                "CREATE INDEX IF NOT EXISTS idx_gateway_payload_blob_facts_tool_name ON gateway_payload_blob_facts(tool_name)",
            )
            .await?;
        database
            .execute_unprepared(
                "CREATE INDEX IF NOT EXISTS idx_gateway_payload_blob_facts_tool_use_id ON gateway_payload_blob_facts(tool_use_id)",
            )
            .await?;

        for blob_id in request_blob_ids(database).await? {
            let Some(body) = blob_body(database, &blob_id).await? else {
                continue;
            };
            let Ok(value) = serde_json::from_slice(&body) else {
                continue;
            };
            store(database, &blob_id, &extract(&value)).await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS gateway_payload_blob_facts")
            .await?;
        Ok(())
    }
}

async fn request_blob_ids(database: &impl ConnectionTrait) -> Result<Vec<String>, DbErr> {
    let rows = database
        .query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT DISTINCT part_id FROM gateway_payload_part_refs WHERE direction = 'request'",
        ))
        .await?;
    rows.into_iter()
        .map(|row| row.try_get("", "part_id"))
        .collect()
}

async fn blob_body(
    database: &impl ConnectionTrait,
    blob_id: &str,
) -> Result<Option<Vec<u8>>, DbErr> {
    let row = database
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT body FROM gateway_payload_blobs WHERE id = ?",
            [blob_id.to_owned().into()],
        ))
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let body: Vec<u8> = row.try_get("", "body")?;
    Ok(Some(decode_body(&body)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{migration::Migrator, telemetry::timestamp};
    use sea_orm::{Database, DatabaseConnection};
    use sea_orm_migration::MigratorTrait;
    use sha2::{Digest, Sha256};

    const REQUEST_ID: &str = "01930000-0000-7000-8000-000000000001";

    async fn referenced_blob(database: &DatabaseConnection, position: i64, body: &str) {
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
        database
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO gateway_payload_part_refs (request_id, direction, path, position, kind, part_id) VALUES (?, 'request', 'messages/0/content', ?, 'content', ?)",
                [REQUEST_ID.into(), position.into(), id.into()],
            ))
            .await
            .unwrap();
    }

    async fn facts(
        database: &DatabaseConnection,
    ) -> Vec<(String, i64, Option<String>, Option<String>)> {
        database
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT block_type, ordinal, tool_name, mcp_server FROM gateway_payload_blob_facts ORDER BY block_type, tool_name, ordinal",
            ))
            .await
            .unwrap()
            .into_iter()
            .map(|row| {
                (
                    row.try_get("", "block_type").unwrap(),
                    row.try_get("", "ordinal").unwrap(),
                    row.try_get("", "tool_name").unwrap(),
                    row.try_get("", "mcp_server").unwrap(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn every_request_blob_is_extracted_once_however_often_the_backfill_runs() {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&database, None).await.unwrap();
        database
            .execute_unprepared(&format!(
                "INSERT INTO gateway_requests(id,request_id,provider,protocol,method,endpoint,started_at,request_bytes,response_bytes,client_disconnected) \
                 VALUES('{REQUEST_ID}','{REQUEST_ID}','claude','anthropic_messages','POST','/providers/claude/v1/messages','2026-09-01T00:00:00.000Z',0,0,FALSE)"
            ))
            .await
            .unwrap();
        referenced_blob(
            &database,
            0,
            r#"{"type":"tool_use","id":"toolu_1","name":"mcp__claude_ai_GitLab__get_merge_request","input":{}}"#,
        )
        .await;
        referenced_blob(
            &database,
            1,
            r#"{"type":"additional_tools","tools":[{"name":"functions","tools":[{"type":"function","name":"exec","parameters":{}}]}]}"#,
        )
        .await;

        Migration.up(&SchemaManager::new(&database)).await.unwrap();
        let extracted = facts(&database).await;
        assert_eq!(
            extracted,
            [
                (
                    "tool_definition".to_owned(),
                    0,
                    Some("exec".to_owned()),
                    None
                ),
                (
                    "tool_use".to_owned(),
                    0,
                    Some("mcp__claude_ai_GitLab__get_merge_request".to_owned()),
                    Some("claude_ai_GitLab".to_owned())
                ),
            ]
        );

        Migration.up(&SchemaManager::new(&database)).await.unwrap();
        assert_eq!(
            facts(&database).await,
            extracted,
            "the backfill is idempotent"
        );
    }
}
