use crate::{db::begin_immediate, request_metrics::rollup};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement};

pub const NAME: &str = "request_metrics_rollup";

const SCAN_BATCH: i64 = 256;

pub async fn run(database: &DatabaseConnection) -> Result<u64, DbErr> {
    let mut after = String::new();
    let mut rolled_up = 0;
    loop {
        let batch = requests_without_metrics(database, &after).await?;
        let Some(last) = batch.last() else {
            return Ok(rolled_up);
        };
        after = last.clone();
        let transaction = begin_immediate(database).await?;
        for request_id in batch {
            if rollup(&transaction, &request_id).await? {
                rolled_up += 1;
            }
        }
        transaction.commit().await?;
    }
}

async fn requests_without_metrics(
    database: &impl ConnectionTrait,
    after: &str,
) -> Result<Vec<String>, DbErr> {
    let rows = database
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT g.id
             FROM gateway_requests g
             WHERE g.id > ?
               AND EXISTS (SELECT 1 FROM gateway_payload_part_refs r
                           WHERE r.request_id = g.id AND r.direction = 'request')
               AND NOT EXISTS (SELECT 1 FROM gateway_request_metrics m
                               WHERE m.request_id = g.id)
             ORDER BY g.id
             LIMIT ?",
            [after.to_owned().into(), SCAN_BATCH.into()],
        ))
        .await?;
    rows.into_iter().map(|row| row.try_get("", "id")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        providers::Provider,
        request_metrics::tests::{ANTHROPIC_BODY, CODEX_BODY, database, metrics, started},
    };

    #[tokio::test]
    async fn every_request_with_stored_parts_is_rolled_up_once() {
        let database = database().await;
        let anthropic = started(&database, Provider::Anthropic, ANTHROPIC_BODY.as_bytes()).await;
        let codex = started(&database, Provider::Codex, CODEX_BODY.as_bytes()).await;
        let empty = started(&database, Provider::Codex, b"").await;

        assert_eq!(run(&database).await.unwrap(), 2);
        let first = metrics(&database, &anthropic).await.unwrap();
        assert_eq!(first.tools_offered, 2);
        assert_eq!(metrics(&database, &codex).await.unwrap().tools_invoked, 2);
        assert_eq!(metrics(&database, &empty).await, None);

        assert_eq!(
            run(&database).await.unwrap(),
            0,
            "a request that already has metrics is skipped"
        );
        assert_eq!(metrics(&database, &anthropic).await, Some(first));
    }
}
