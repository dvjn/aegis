//! Hourly guardrail buckets, summed from the policy evaluations of the
//! finished requests that started in that hour. Three tables share one
//! stamp with the usage buckets, `gateway_requests.aggregated_at`, because
//! [`aggregate`] runs inside [`crate::usage_hourly::aggregate`] before it
//! stamps.
//!
//! `gateway_guardrails_hourly` counts scanned and masked requests and their
//! matches per provider and key. `gateway_guardrail_detectors_hourly` counts
//! matches and requests per detector. `gateway_guardrail_values_hourly` keeps
//! one row per placeholder seen in the hour, so a report counts distinct
//! masked values over any range of hours without opening the evaluations.

use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement};

use crate::usage_hourly::HOUR_FORMAT;

/// The finished requests to count, with the user and hour they belong to.
const REQUESTS_SQL: &str = "request AS ( \
     SELECT r.id, k.user_id, strftime('{hour}', r.started_at) hour, r.provider, r.key_id \
     FROM gateway_requests r \
     JOIN gateway_keys k ON k.id = r.key_id \
     WHERE r.completed_at IS NOT NULL {unstamped} {filter})";

const EVALUATIONS_SQL: &str = "evaluation AS ( \
     SELECT q.user_id, q.hour, q.provider, q.key_id, e.request_id, e.outcome, e.match_count, e.metadata \
     FROM request q \
     JOIN policy_evaluations e ON e.request_id = q.id AND e.policy IN ('secrets', 'regex'))";

const REQUEST_COUNTS_SQL: &str = "INSERT INTO gateway_guardrails_hourly \
     (user_id, hour, provider, key_id, scanned, masked, matches) \
     WITH {requests}, {evaluations}, \
     per_request AS ( \
     SELECT user_id, hour, provider, key_id, \
     MAX(CASE WHEN outcome = 'transform' THEN 1 ELSE 0 END) masked, \
     SUM(match_count) matches \
     FROM evaluation GROUP BY user_id, hour, provider, key_id, request_id) \
     SELECT user_id, hour, provider, key_id, COUNT(*), SUM(masked), SUM(matches) \
     FROM per_request GROUP BY 1, 2, 3, 4 \
     ON CONFLICT (user_id, hour, provider, key_id) DO UPDATE SET \
     scanned = scanned + excluded.scanned, \
     masked = masked + excluded.masked, \
     matches = matches + excluded.matches";

/// Metadata that is missing, malformed, or without a detectors object gives
/// `json_each` a NULL, which yields no rows.
const DETECTOR_COUNTS_SQL: &str = "INSERT INTO gateway_guardrail_detectors_hourly \
     (user_id, hour, detector, matches, requests) \
     WITH {requests}, {evaluations} \
     SELECT v.user_id, v.hour, d.key, SUM(CAST(d.value AS INTEGER)), COUNT(*) \
     FROM evaluation v \
     JOIN json_each(CASE WHEN json_valid(v.metadata) \
     AND json_type(v.metadata, '$.detectors') = 'object' \
     THEN json_extract(v.metadata, '$.detectors') END) d \
     GROUP BY 1, 2, 3 \
     ON CONFLICT (user_id, hour, detector) DO UPDATE SET \
     matches = matches + excluded.matches, \
     requests = requests + excluded.requests";

/// A placeholder reads `AEGIS_MASKED_<DETECTOR>_<22 hex>_END`, so the detector
/// is what sits between the 13 byte prefix and the 27 byte tail.
const VALUES_SQL: &str = "INSERT OR IGNORE INTO gateway_guardrail_values_hourly \
     (user_id, hour, detector, placeholder) \
     WITH {requests}, {evaluations} \
     SELECT DISTINCT v.user_id, v.hour, lower(substr(p.value, 14, length(p.value) - 40)), p.value \
     FROM evaluation v \
     JOIN json_each(CASE WHEN json_valid(v.metadata) \
     AND json_type(v.metadata, '$.placeholders') = 'array' \
     THEN json_extract(v.metadata, '$.placeholders') END) p";

/// Adds the guardrail evaluations of every finished, not yet stamped request
/// to their buckets, or only the given request's. Stamping is left to the
/// caller, which shares the stamp with the usage buckets.
pub async fn aggregate(
    database: &impl ConnectionTrait,
    request_id: Option<&str>,
) -> Result<(), DbErr> {
    insert(database, request_id, "AND r.aggregated_at IS NULL").await
}

/// Recounts every bucket from the evaluations of every finished request.
#[cfg(test)]
pub async fn rebuild(database: &impl ConnectionTrait) -> Result<(), DbErr> {
    for table in [
        "gateway_guardrails_hourly",
        "gateway_guardrail_detectors_hourly",
        "gateway_guardrail_values_hourly",
    ] {
        database
            .execute_unprepared(&format!("DELETE FROM {table}"))
            .await?;
    }
    insert(database, None, "").await
}

async fn insert(
    database: &impl ConnectionTrait,
    request_id: Option<&str>,
    unstamped: &str,
) -> Result<(), DbErr> {
    let filter = match request_id {
        Some(_) => "AND r.id = ?",
        None => "",
    };
    let id: Vec<sea_orm::Value> = request_id
        .map(|id| id.to_owned().into())
        .into_iter()
        .collect();
    let requests = REQUESTS_SQL
        .replace("{hour}", HOUR_FORMAT)
        .replace("{unstamped}", unstamped)
        .replace("{filter}", filter);
    for template in [REQUEST_COUNTS_SQL, DETECTOR_COUNTS_SQL, VALUES_SQL] {
        database
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                template
                    .replace("{requests}", &requests)
                    .replace("{evaluations}", EVALUATIONS_SQL),
                id.clone(),
            ))
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use sea_orm::{Database, DatabaseConnection, FromQueryResult};
    use sea_orm_migration::MigratorTrait;

    #[derive(Debug, PartialEq, Eq, FromQueryResult)]
    struct Counts {
        hour: String,
        scanned: i64,
        masked: i64,
        matches: i64,
    }

    #[derive(Debug, PartialEq, Eq, FromQueryResult)]
    struct Detector {
        detector: String,
        matches: i64,
        requests: i64,
    }

    async fn database() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        db.execute_unprepared("INSERT INTO users(id,email_normalized,email_display,role,status,auth_version,created_at,updated_at) VALUES('u1','a@example.com','a@example.com','user','active',0,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')").await.unwrap();
        db.execute_unprepared("INSERT INTO gateway_keys(id,user_id,name,allowed_providers,created_at) VALUES('k1','u1','agent','[\"claude\"]','2026-01-01T00:00:00Z')").await.unwrap();
        db
    }

    async fn request(db: &DatabaseConnection, id: &str, started_at: &str, completed: bool) {
        let completed_at = if completed {
            format!("'{started_at}'")
        } else {
            "NULL".to_owned()
        };
        db.execute_unprepared(&format!(
            "INSERT INTO gateway_requests(id,request_id,provider,protocol,method,endpoint,started_at,completed_at,request_bytes,response_bytes,client_disconnected,key_id) \
             VALUES('{id}','{id}','claude','anthropic_messages','POST','/v1/messages','{started_at}',{completed_at},0,0,FALSE,'k1')"
        ))
        .await
        .unwrap();
    }

    async fn evaluation(
        db: &DatabaseConnection,
        id: &str,
        request_id: &str,
        policy: &str,
        outcome: &str,
        match_count: i64,
        metadata: &str,
    ) {
        db.execute_unprepared(&format!(
            "INSERT INTO policy_evaluations(id,request_id,policy,policy_version,stage,outcome,match_count,duration_micros,metadata,created_at) \
             VALUES('{id}','{request_id}','{policy}',1,'request','{outcome}',{match_count},0,'{metadata}','2026-03-01T00:00:00Z')"
        ))
        .await
        .unwrap();
    }

    async fn counts(db: &DatabaseConnection) -> Vec<Counts> {
        Counts::find_by_statement(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT hour, scanned, masked, matches FROM gateway_guardrails_hourly ORDER BY hour",
        ))
        .all(db)
        .await
        .unwrap()
    }

    async fn detectors(db: &DatabaseConnection) -> Vec<Detector> {
        Detector::find_by_statement(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT detector, matches, requests FROM gateway_guardrail_detectors_hourly ORDER BY detector",
        ))
        .all(db)
        .await
        .unwrap()
    }

    async fn placeholders(db: &DatabaseConnection) -> Vec<(String, String)> {
        db.query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT hour || ' ' || detector, placeholder FROM gateway_guardrail_values_hourly ORDER BY 1, 2",
        ))
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            (
                row.try_get_by_index(0).unwrap(),
                row.try_get_by_index(1).unwrap(),
            )
        })
        .collect()
    }

    #[tokio::test]
    async fn finished_requests_fill_the_three_buckets_once() {
        let db = database().await;
        request(&db, "a", "2026-03-01T10:05:00.000Z", true).await;
        request(&db, "b", "2026-03-01T10:40:00.000Z", true).await;
        request(&db, "c", "2026-03-01T11:10:00.000Z", true).await;
        request(&db, "open", "2026-03-01T11:20:00.000Z", false).await;
        evaluation(&db, "e-a1", "a", "secrets", "transform", 3,
            r#"{"detectors":{"github_token":2,"aws_access_key_id":1},"placeholders":["AEGIS_MASKED_GITHUB_TOKEN_0123456789abcdef012345_END","AEGIS_MASKED_AWS_ACCESS_KEY_ID_00000000000000000000aa_END"]}"#).await;
        evaluation(&db, "e-a2", "a", "regex", "transform", 1,
            r#"{"detectors":{"internal_token":1},"placeholders":["AEGIS_MASKED_INTERNAL_TOKEN_fedcba9876543210fedcba_END"]}"#).await;
        evaluation(&db, "e-b", "b", "secrets", "allow", 0, "null").await;
        evaluation(&db, "e-c", "c", "secrets", "transform", 2,
            r#"{"detectors":{"github_token":2},"placeholders":["AEGIS_MASKED_GITHUB_TOKEN_0123456789abcdef012345_END"]}"#).await;
        evaluation(&db, "e-open", "open", "secrets", "transform", 9,
            r#"{"detectors":{"github_token":9},"placeholders":["AEGIS_MASKED_GITHUB_TOKEN_ffffffffffffffffffffff_END"]}"#).await;
        evaluation(
            &db,
            "e-other",
            "a",
            "other",
            "transform",
            5,
            r#"{"detectors":{"x":5}}"#,
        )
        .await;

        aggregate(&db, None).await.unwrap();

        assert_eq!(
            counts(&db).await,
            vec![
                Counts {
                    hour: "2026-03-01T10:00:00.000Z".to_owned(),
                    scanned: 2,
                    masked: 1,
                    matches: 4,
                },
                Counts {
                    hour: "2026-03-01T11:00:00.000Z".to_owned(),
                    scanned: 1,
                    masked: 1,
                    matches: 2,
                },
            ]
        );
        assert_eq!(
            detectors(&db).await,
            vec![
                Detector {
                    detector: "aws_access_key_id".to_owned(),
                    matches: 1,
                    requests: 1,
                },
                Detector {
                    detector: "github_token".to_owned(),
                    matches: 2,
                    requests: 1,
                },
                Detector {
                    detector: "github_token".to_owned(),
                    matches: 2,
                    requests: 1,
                },
                Detector {
                    detector: "internal_token".to_owned(),
                    matches: 1,
                    requests: 1,
                },
            ],
            "github_token has one row per hour"
        );
        assert_eq!(
            placeholders(&db).await,
            vec![
                (
                    "2026-03-01T10:00:00.000Z aws_access_key_id".to_owned(),
                    "AEGIS_MASKED_AWS_ACCESS_KEY_ID_00000000000000000000aa_END".to_owned()
                ),
                (
                    "2026-03-01T10:00:00.000Z github_token".to_owned(),
                    "AEGIS_MASKED_GITHUB_TOKEN_0123456789abcdef012345_END".to_owned()
                ),
                (
                    "2026-03-01T10:00:00.000Z internal_token".to_owned(),
                    "AEGIS_MASKED_INTERNAL_TOKEN_fedcba9876543210fedcba_END".to_owned()
                ),
                (
                    "2026-03-01T11:00:00.000Z github_token".to_owned(),
                    "AEGIS_MASKED_GITHUB_TOKEN_0123456789abcdef012345_END".to_owned()
                ),
            ]
        );

        db.execute_unprepared("UPDATE gateway_requests SET aggregated_at = 'stamped'")
            .await
            .unwrap();
        aggregate(&db, None).await.unwrap();
        assert_eq!(
            counts(&db).await[0].scanned,
            2,
            "stamped requests count once"
        );

        rebuild(&db).await.unwrap();
        assert_eq!(counts(&db).await[0].scanned, 2);
        assert_eq!(placeholders(&db).await.len(), 4);
    }
}
