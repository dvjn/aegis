use crate::{
    compression::decode_body,
    db::begin_immediate,
    payload_facts::{extract, store, tool_definition},
};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement};

pub const NAME: &str = "payload_facts_backfill";

const SCAN_BATCH: i64 = 512;

struct Blob {
    id: String,
    body: Vec<u8>,
    is_tool: bool,
}

pub async fn run(database: &DatabaseConnection) -> Result<u64, DbErr> {
    let mut after = String::new();
    let mut extracted = 0;
    loop {
        let batch = blobs_without_facts(database, &after).await?;
        let Some(last) = batch.last() else {
            return Ok(extracted);
        };
        after = last.id.clone();
        let extracted_facts: Vec<_> = batch
            .into_iter()
            .filter_map(|blob| {
                let value = serde_json::from_slice(&blob.body).ok()?;
                let facts = if blob.is_tool {
                    tool_definition(&value)
                } else {
                    extract(&value)
                };
                (!facts.is_empty()).then_some((blob.id, facts))
            })
            .collect();
        if extracted_facts.is_empty() {
            continue;
        }
        let transaction = begin_immediate(database).await?;
        for (blob_id, facts) in extracted_facts {
            store(&transaction, &blob_id, &facts).await?;
            extracted += 1;
        }
        transaction.commit().await?;
    }
}

async fn blobs_without_facts(
    database: &impl ConnectionTrait,
    after: &str,
) -> Result<Vec<Blob>, DbErr> {
    let rows = database
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT b.id, b.body,
                    EXISTS (SELECT 1 FROM gateway_payload_part_refs t
                            WHERE t.part_id = b.id AND t.direction = 'request'
                              AND t.path = 'tools') is_tool
             FROM gateway_payload_blobs b
             WHERE b.id > ?
               AND EXISTS (SELECT 1 FROM gateway_payload_part_refs r
                           WHERE r.part_id = b.id AND r.direction = 'request')
               AND NOT EXISTS (SELECT 1 FROM gateway_payload_blob_facts f
                               WHERE f.blob_id = b.id)
             ORDER BY b.id
             LIMIT ?",
            [after.to_owned().into(), SCAN_BATCH.into()],
        ))
        .await?;
    rows.into_iter()
        .map(|row| {
            let body: Vec<u8> = row.try_get("", "body")?;
            Ok(Blob {
                id: row.try_get("", "id")?,
                body: decode_body(&body),
                is_tool: row.try_get("", "is_tool")?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{migration::Migrator, telemetry::timestamp};
    use sea_orm::Database;
    use sea_orm_migration::MigratorTrait;
    use sha2::{Digest, Sha256};

    const REQUEST_ID: &str = "01930000-0000-7000-8000-000000000001";

    async fn referenced_blob(database: &DatabaseConnection, position: i64, body: &str) {
        referenced_part(database, "messages/0/content", position, body).await;
    }

    async fn referenced_part(database: &DatabaseConnection, path: &str, position: i64, body: &str) {
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
                "INSERT INTO gateway_payload_part_refs (request_id, direction, path, position, kind, part_id) VALUES (?, 'request', ?, ?, 'content', ?)",
                [REQUEST_ID.into(), path.to_owned().into(), position.into(), id.into()],
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

        referenced_part(
            &database,
            "tools",
            0,
            r#"{"type":"custom","name":"apply_patch","format":{}}"#,
        )
        .await;

        assert_eq!(run(&database).await.unwrap(), 3);
        let extracted = facts(&database).await;
        assert_eq!(
            extracted,
            [
                (
                    "tool_definition".to_owned(),
                    0,
                    Some("apply_patch".to_owned()),
                    None
                ),
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

        assert_eq!(
            run(&database).await.unwrap(),
            0,
            "a blob that already has facts is skipped"
        );
        assert_eq!(facts(&database).await, extracted);
    }
}
