//! Hourly usage buckets: one row per owner, UTC hour of request start,
//! provider, requested model and key, summing the finished requests that
//! started in that hour. Reports read whole hours from here and only the
//! partial hours at the window edges from the request rows.
//!
//! A request enters its bucket once, when it finishes, and is stamped with
//! `aggregated_at` so a retried completion cannot count it twice. `rebuild`
//! throws every bucket away and recounts from the request rows.

use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement};

use crate::telemetry::timestamp;

pub const HOUR_FORMAT: &str = "%Y-%m-%dT%H:00:00.000Z";

const AGGREGATE_SQL: &str = "INSERT INTO gateway_usage_hourly \
     (user_id, hour, provider, requested_model, key_id, requests, succeeded, failed, \
     input_tokens, cache_read_tokens, cache_write_tokens, output_tokens, reasoning_tokens, \
     cost_nanodollars, unpriced) \
     SELECT k.user_id, strftime('{hour}', r.started_at), r.provider, COALESCE(r.requested_model, ''), r.key_id, \
     COUNT(*), \
     SUM(CASE WHEN r.http_status < 400 AND r.error_message IS NULL THEN 1 ELSE 0 END), \
     SUM(CASE WHEN r.http_status >= 400 OR r.error_message IS NOT NULL THEN 1 ELSE 0 END), \
     COALESCE(SUM(u.input_tokens), 0), \
     COALESCE(SUM(u.cache_read_tokens), 0), \
     COALESCE(SUM(u.cache_write_tokens), 0), \
     COALESCE(SUM(u.output_tokens), 0), \
     COALESCE(SUM(u.reasoning_tokens), 0), \
     COALESCE(SUM(u.cost_nanodollars), 0), \
     SUM(CASE WHEN u.cost_nanodollars IS NULL THEN 1 ELSE 0 END) \
     FROM gateway_requests r \
     JOIN gateway_keys k ON k.id = r.key_id \
     LEFT JOIN gateway_usage u ON u.request_id = r.id \
     WHERE r.aggregated_at IS NULL AND r.completed_at IS NOT NULL {filter} \
     GROUP BY 1, 2, 3, 4, 5 \
     ON CONFLICT (user_id, hour, provider, requested_model, key_id) DO UPDATE SET \
     requests = requests + excluded.requests, \
     succeeded = succeeded + excluded.succeeded, \
     failed = failed + excluded.failed, \
     input_tokens = input_tokens + excluded.input_tokens, \
     cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens, \
     cache_write_tokens = cache_write_tokens + excluded.cache_write_tokens, \
     output_tokens = output_tokens + excluded.output_tokens, \
     reasoning_tokens = reasoning_tokens + excluded.reasoning_tokens, \
     cost_nanodollars = cost_nanodollars + excluded.cost_nanodollars, \
     unpriced = unpriced + excluded.unpriced";

const STAMP_SQL: &str = "UPDATE gateway_requests SET aggregated_at = ? \
     WHERE aggregated_at IS NULL AND completed_at IS NOT NULL {filter}";

/// Adds every finished, not yet counted request to its bucket, or only the
/// given one. Returns how many requests were counted. Run inside the
/// transaction that finished the request so the bucket and the row agree.
pub async fn aggregate(
    database: &impl ConnectionTrait,
    request_id: Option<&str>,
) -> Result<u64, DbErr> {
    let (request_filter, stamp_filter) = match request_id {
        Some(_) => ("AND r.id = ?", "AND id = ?"),
        None => ("", ""),
    };
    let id: Vec<sea_orm::Value> = request_id
        .map(|id| id.to_owned().into())
        .into_iter()
        .collect();
    database
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            AGGREGATE_SQL
                .replace("{hour}", HOUR_FORMAT)
                .replace("{filter}", request_filter),
            id.clone(),
        ))
        .await?;
    let stamped = database
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            STAMP_SQL.replace("{filter}", stamp_filter),
            std::iter::once(timestamp().into()).chain(id),
        ))
        .await?;
    Ok(stamped.rows_affected())
}

/// Recounts every bucket from the request rows. Use after anything that
/// changes a finished request's tokens or cost after the fact.
pub async fn rebuild(database: &impl ConnectionTrait) -> Result<u64, DbErr> {
    database
        .execute_unprepared("DELETE FROM gateway_usage_hourly")
        .await?;
    database
        .execute_unprepared("UPDATE gateway_requests SET aggregated_at = NULL")
        .await?;
    aggregate(database, None).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use sea_orm::{Database, DatabaseConnection, FromQueryResult};
    use sea_orm_migration::MigratorTrait;

    #[derive(Debug, PartialEq, Eq, FromQueryResult)]
    struct Row {
        hour: String,
        requested_model: String,
        requests: i64,
        succeeded: i64,
        failed: i64,
        input_tokens: i64,
        cost_nanodollars: i64,
        unpriced: i64,
    }

    async fn database() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        db.execute_unprepared("INSERT INTO users(id,email_normalized,email_display,role,status,auth_version,created_at,updated_at) VALUES('u1','a@example.com','a@example.com','user','active',0,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')").await.unwrap();
        db.execute_unprepared("INSERT INTO gateway_keys(id,user_id,name,allowed_providers,created_at) VALUES('k1','u1','agent','[\"claude\"]','2026-01-01T00:00:00Z')").await.unwrap();
        db
    }

    async fn request(db: &DatabaseConnection, id: &str, started_at: &str, model: Option<&str>) {
        let model = model.map_or("NULL".to_owned(), |model| format!("'{model}'"));
        db.execute_unprepared(&format!(
            "INSERT INTO gateway_requests(id,request_id,provider,protocol,method,endpoint,requested_model,started_at,request_bytes,response_bytes,client_disconnected,key_id) \
             VALUES('{id}','{id}','claude','anthropic_messages','POST','/v1/messages',{model},'{started_at}',0,0,FALSE,'k1')"
        ))
        .await
        .unwrap();
    }

    async fn finish(db: &DatabaseConnection, id: &str, status: i32, input: i64, cost: Option<i64>) {
        db.execute_unprepared(&format!(
            "UPDATE gateway_requests SET completed_at = '2026-03-01T10:20:00.000Z', http_status = {status} WHERE id = '{id}'"
        ))
        .await
        .unwrap();
        let cost = cost.map_or("NULL".to_owned(), |cost| cost.to_string());
        db.execute_unprepared(&format!(
            "INSERT INTO gateway_usage(request_id,input_tokens,output_tokens,cost_nanodollars) VALUES('{id}',{input},0,{cost})"
        ))
        .await
        .unwrap();
    }

    async fn rows(db: &DatabaseConnection) -> Vec<Row> {
        Row::find_by_statement(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT hour, requested_model, requests, succeeded, failed, input_tokens, cost_nanodollars, unpriced \
             FROM gateway_usage_hourly ORDER BY hour, requested_model",
        ))
        .all(db)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_finished_request_joins_its_start_hour_once() {
        let db = database().await;
        request(&db, "a", "2026-03-01T10:15:00.000Z", Some("opus")).await;
        finish(&db, "a", 200, 100, Some(5)).await;

        assert_eq!(aggregate(&db, Some("a")).await.unwrap(), 1);
        assert_eq!(aggregate(&db, Some("a")).await.unwrap(), 0);
        assert_eq!(aggregate(&db, None).await.unwrap(), 0);

        assert_eq!(
            rows(&db).await,
            vec![Row {
                hour: "2026-03-01T10:00:00.000Z".to_owned(),
                requested_model: "opus".to_owned(),
                requests: 1,
                succeeded: 1,
                failed: 0,
                input_tokens: 100,
                cost_nanodollars: 5,
                unpriced: 0,
            }]
        );
    }

    #[tokio::test]
    async fn an_unfinished_request_waits_and_a_rebuild_matches_incremental_counting() {
        let db = database().await;
        request(&db, "a", "2026-03-01T10:15:00.000Z", Some("opus")).await;
        request(&db, "b", "2026-03-01T10:45:00.000Z", Some("opus")).await;
        request(&db, "c", "2026-03-01T11:05:00.000Z", None).await;
        request(&db, "pending", "2026-03-01T11:30:00.000Z", None).await;
        finish(&db, "a", 200, 100, Some(5)).await;
        finish(&db, "b", 500, 20, None).await;
        finish(&db, "c", 200, 7, Some(1)).await;

        assert_eq!(aggregate(&db, None).await.unwrap(), 3);
        let incremental = rows(&db).await;
        assert_eq!(
            incremental,
            vec![
                Row {
                    hour: "2026-03-01T10:00:00.000Z".to_owned(),
                    requested_model: "opus".to_owned(),
                    requests: 2,
                    succeeded: 1,
                    failed: 1,
                    input_tokens: 120,
                    cost_nanodollars: 5,
                    unpriced: 1,
                },
                Row {
                    hour: "2026-03-01T11:00:00.000Z".to_owned(),
                    requested_model: String::new(),
                    requests: 1,
                    succeeded: 1,
                    failed: 0,
                    input_tokens: 7,
                    cost_nanodollars: 1,
                    unpriced: 0,
                },
            ]
        );

        assert_eq!(rebuild(&db).await.unwrap(), 3);
        assert_eq!(rows(&db).await, incremental);

        finish(&db, "pending", 200, 1, Some(1)).await;
        assert_eq!(aggregate(&db, None).await.unwrap(), 1);
        assert_eq!(rows(&db).await[1].requests, 2);
    }
}
