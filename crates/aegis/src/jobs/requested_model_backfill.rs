use crate::{db::begin_immediate, providers::requested_model};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement};

pub const NAME: &str = "requested_model_backfill";

const SCAN_BATCH: i64 = 32;

struct Request {
    id: String,
    envelope: Vec<u8>,
}

pub async fn run(database: &DatabaseConnection) -> Result<u64, DbErr> {
    let mut after = String::new();
    let mut updated = 0;
    loop {
        let batch = requests_without_models(database, &after).await?;
        let Some(last) = batch.last() else {
            return Ok(updated);
        };
        after = last.id.clone();
        let updates: Vec<_> = batch
            .into_iter()
            .filter_map(|request| {
                requested_model(&request.envelope).map(|model| (request.id, model))
            })
            .collect();
        if updates.is_empty() {
            continue;
        }
        let transaction = begin_immediate(database).await?;
        for (request_id, model) in updates {
            let result = transaction
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    "UPDATE gateway_requests SET requested_model = ?
                     WHERE id = ? AND requested_model IS NULL",
                    [model.into(), request_id.into()],
                ))
                .await?;
            updated += result.rows_affected();
        }
        transaction.commit().await?;
        tracing::debug!(job = NAME, after = %after, "background job batch committed");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

async fn requests_without_models(
    database: &impl ConnectionTrait,
    after: &str,
) -> Result<Vec<Request>, DbErr> {
    let rows = database
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT r.id, b.body
             FROM gateway_requests r
             JOIN gateway_payload_envelopes e
               ON e.request_id = r.id AND e.direction = 'request'
             JOIN gateway_payload_blobs b ON b.id = e.body_id
             WHERE r.id > ? AND r.requested_model IS NULL
             ORDER BY r.id
             LIMIT ?",
            [after.to_owned().into(), SCAN_BATCH.into()],
        ))
        .await?;
    rows.into_iter()
        .map(|row| {
            Ok(Request {
                id: row.try_get("", "id")?,
                envelope: row.try_get("", "body")?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        migration::Migrator,
        telemetry::{StoredPayload, store_blob},
    };
    use sea_orm::Database;
    use sea_orm_migration::MigratorTrait;

    async fn insert_request(database: &DatabaseConnection, id: &str, model: Option<&str>) {
        database
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO gateway_requests
                 (id, request_id, provider, protocol, method, endpoint, requested_model,
                  started_at, request_bytes, response_bytes, client_disconnected)
                 VALUES (?, ?, 'codex', 'openai_responses', 'POST',
                         '/providers/codex/responses', ?, '2026-09-05T00:00:00Z', 42, 0, FALSE)",
                [id.into(), id.into(), model.map(str::to_owned).into()],
            ))
            .await
            .unwrap();
        let body = serde_json::to_vec(&serde_json::json!({
            "model": "gpt-5.6-sol",
            "instructions": "repeatable context ".repeat(128),
            "input": []
        }))
        .unwrap();
        let payload = StoredPayload::new(&body).unwrap();
        store_blob(database, &payload).await.unwrap();
        database
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO gateway_payload_envelopes (request_id, direction, body_id)
                 VALUES (?, 'request', ?)",
                [id.into(), payload.id.into()],
            ))
            .await
            .unwrap();
    }

    async fn stored_model(database: &DatabaseConnection, id: &str) -> Option<String> {
        database
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT requested_model FROM gateway_requests WHERE id = ?",
                [id.into()],
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "requested_model")
            .unwrap()
    }

    #[tokio::test]
    async fn recovers_only_missing_models_and_is_safe_to_repeat() {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&database, None).await.unwrap();
        insert_request(&database, "missing", None).await;
        insert_request(&database, "existing", Some("original-model")).await;

        assert_eq!(run(&database).await.unwrap(), 1);
        assert_eq!(
            stored_model(&database, "missing").await.as_deref(),
            Some("gpt-5.6-sol")
        );
        assert_eq!(
            stored_model(&database, "existing").await.as_deref(),
            Some("original-model")
        );
        assert_eq!(run(&database).await.unwrap(), 0);
    }
}
