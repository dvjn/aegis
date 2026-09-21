use crate::{db::begin_immediate, providers::response_model};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement};

pub const NAME: &str = "resolved_model_backfill";

const SCAN_BATCH: i64 = 32;

struct Request {
    id: String,
    response: Vec<u8>,
}

/// Requests captured before the gateway read the model out of the response are
/// labelled with whatever alias the client sent. Their responses are stored, so
/// the model that actually ran can still be recovered.
pub async fn run(database: &DatabaseConnection) -> Result<u64, DbErr> {
    let mut after = String::new();
    let mut updated = 0;
    loop {
        let batch = requests_without_resolved_models(database, &after).await?;
        let Some(last) = batch.last() else {
            return Ok(updated);
        };
        after = last.id.clone();
        let updates: Vec<_> = batch
            .into_iter()
            .filter_map(|request| {
                response_model(&request.response).map(|model| (request.id, model))
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
                    "UPDATE gateway_requests SET resolved_model = ?
                     WHERE id = ? AND resolved_model IS NULL",
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

async fn requests_without_resolved_models(
    database: &impl ConnectionTrait,
    after: &str,
) -> Result<Vec<Request>, DbErr> {
    let rows = database
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT r.id, b.body
             FROM gateway_requests r
             JOIN gateway_payloads p ON p.request_id = r.id
             JOIN gateway_payload_blobs b ON b.id = p.response_body_id
             WHERE r.id > ? AND r.resolved_model IS NULL
             ORDER BY r.id
             LIMIT ?",
            [after.to_owned().into(), SCAN_BATCH.into()],
        ))
        .await?;
    rows.into_iter()
        .map(|row| {
            Ok(Request {
                id: row.try_get("", "id")?,
                response: row.try_get("", "body")?,
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

    const RESPONSE: &[u8] =
        br#"{"model":"jev-1.13.0","answers":{},"usage":{"input_tokens":9,"output_tokens":1}}"#;

    async fn insert_request(
        database: &DatabaseConnection,
        id: &str,
        resolved: Option<&str>,
        response: Option<&[u8]>,
    ) {
        database
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO gateway_requests
                 (id, request_id, provider, protocol, method, endpoint, requested_model,
                  resolved_model, started_at, request_bytes, response_bytes, client_disconnected)
                 VALUES (?, ?, 'typesafe', 'typesafe_systemone', 'POST',
                         '/providers/typesafe/v1/systemone', 'jev-latest', ?,
                         '2026-09-22T00:00:00Z', 42, 0, FALSE)",
                [id.into(), id.into(), resolved.map(str::to_owned).into()],
            ))
            .await
            .unwrap();
        let Some(response) = response else {
            return;
        };
        let payload = StoredPayload::new(response).unwrap();
        store_blob(database, &payload).await.unwrap();
        database
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO gateway_payloads (request_id, response_body_id) VALUES (?, ?)",
                [id.into(), payload.id.into()],
            ))
            .await
            .unwrap();
    }

    async fn stored_model(database: &DatabaseConnection, id: &str) -> Option<String> {
        database
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT resolved_model FROM gateway_requests WHERE id = ?",
                [id.into()],
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "resolved_model")
            .unwrap()
    }

    #[tokio::test]
    async fn recovers_only_missing_models_and_is_safe_to_repeat() {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&database, None).await.unwrap();
        insert_request(&database, "a", None, Some(RESPONSE)).await;
        insert_request(&database, "b", Some("jev-1.12.0"), Some(RESPONSE)).await;
        insert_request(&database, "c", None, None).await;

        assert_eq!(run(&database).await.unwrap(), 1);
        assert_eq!(
            stored_model(&database, "a").await.as_deref(),
            Some("jev-1.13.0")
        );
        assert_eq!(
            stored_model(&database, "b").await.as_deref(),
            Some("jev-1.12.0"),
            "a model already recorded is never overwritten"
        );
        assert_eq!(
            stored_model(&database, "c").await,
            None,
            "a request with no captured response keeps falling back to the alias"
        );

        assert_eq!(
            run(&database).await.unwrap(),
            0,
            "a second pass has nothing left to do"
        );
    }
}
