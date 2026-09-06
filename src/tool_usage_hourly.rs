//! Hourly tool buckets: one row per owner, UTC hour of request start, block
//! type, tool label and skill, summing the tool parts of the finished
//! requests that started in that hour. The tool report reads whole hours
//! from here and never walks the part references.
//!
//! A definition counts once per request that carried it: `calls` is the
//! number of requests, `bytes` its share of the blob's bytes, `cost` that
//! share of each request's cost. A call or result costs its byte share of
//! every request that replays it, but counts and adds its bytes only the
//! first time its call id is seen for the owner, in the earliest request
//! that carried it; `gateway_tool_calls_seen` remembers which ids have been
//! counted. A result borrows the label and skill of the call it answers.
//!
//! A request enters its buckets once, when it finishes, and is stamped with
//! `tools_aggregated_at` so a retried completion cannot count it twice.
//! `rebuild` throws every bucket and every seen id away and recounts from
//! the request rows in start order.

use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement};

use crate::telemetry::timestamp;
use crate::usage_hourly::HOUR_FORMAT;

/// The finished, not yet counted requests, with the price of one byte of
/// their context: the request's cost spread over its measured bytes, or
/// nothing when either is missing.
const REQUESTS_SQL: &str = "request AS ( \
     SELECT r.id, k.user_id, strftime('{hour}', r.started_at) hour, r.started_at, \
     COALESCE(u.cost_nanodollars * 1.0 / m.total_bytes, 0) cost_per_byte \
     FROM gateway_requests r \
     JOIN gateway_keys k ON k.id = r.key_id \
     LEFT JOIN gateway_request_metrics m ON m.request_id = r.id AND m.total_bytes > 0 \
     LEFT JOIN gateway_usage u ON u.request_id = r.id \
     WHERE r.tools_aggregated_at IS NULL AND r.completed_at IS NOT NULL {filter})";

const UPSERT_SQL: &str = "ON CONFLICT (user_id, hour, block_type, label, skill) DO UPDATE SET \
     calls = calls + excluded.calls, \
     bytes = bytes + excluded.bytes, \
     cost_nanodollars = cost_nanodollars + excluded.cost_nanodollars";

/// A blob holding several definitions splits its bytes between them.
const DEFINITIONS_SQL: &str = "INSERT INTO gateway_tool_usage_hourly \
     (user_id, hour, block_type, label, skill, calls, bytes, cost_nanodollars) \
     WITH {requests}, \
     part AS ( \
     SELECT q.user_id, q.hour, q.cost_per_byte, f.block_type, f.tool_name, b.original_bytes bytes, \
     (SELECT COUNT(*) FROM gateway_payload_blob_facts x WHERE x.blob_id = f.blob_id) facts \
     FROM request q \
     JOIN gateway_payload_part_refs p ON p.request_id = q.id AND p.direction = 'request' \
     JOIN gateway_payload_blob_facts f ON f.blob_id = p.part_id AND f.block_type = 'tool_definition' \
     JOIN gateway_payload_blobs b ON b.id = p.part_id) \
     SELECT user_id, hour, block_type, COALESCE(tool_name, ''), '', \
     COUNT(*), \
     SUM(bytes * 1.0 / facts), \
     SUM(bytes * cost_per_byte / facts) \
     FROM part \
     GROUP BY 1, 2, 3, 4, 5 \
     {upsert}";

/// The calls and results of the pending requests, one row per request and
/// call id, labelled like the legacy report. `appearance` numbers the
/// pending requests that carry a call from the earliest started.
const CALLS_SQL: &str = "{requests}, \
     part AS ( \
     SELECT q.user_id, q.hour, q.started_at, q.id request_id, q.cost_per_byte, f.block_type, \
     CASE WHEN f.block_type = 'tool_result' THEN (SELECT MIN(x.tool_name) FROM gateway_payload_blob_facts x \
     WHERE x.tool_use_id = f.tool_use_id AND x.block_type = 'tool_use') ELSE f.tool_name END label, \
     CASE WHEN f.block_type = 'tool_result' THEN (SELECT MIN(x.skill_name) FROM gateway_payload_blob_facts x \
     WHERE x.tool_use_id = f.tool_use_id AND x.block_type = 'tool_use') ELSE f.skill_name END skill, \
     COALESCE(f.tool_use_id, f.blob_id) call_id, \
     b.original_bytes bytes \
     FROM request q \
     JOIN gateway_payload_part_refs p ON p.request_id = q.id AND p.direction = 'request' \
     JOIN gateway_payload_blob_facts f ON f.blob_id = p.part_id AND f.block_type IN ('tool_use', 'tool_result') \
     JOIN gateway_payload_blobs b ON b.id = p.part_id), \
     call AS ( \
     SELECT user_id, hour, block_type, call_id, MIN(label) label, MIN(skill) skill, \
     MAX(bytes) bytes, SUM(bytes * cost_per_byte) cost, \
     ROW_NUMBER() OVER (PARTITION BY user_id, block_type, call_id ORDER BY started_at, request_id) appearance \
     FROM part GROUP BY user_id, hour, block_type, call_id, started_at, request_id)";

const CALL_BUCKETS_SQL: &str = "INSERT INTO gateway_tool_usage_hourly \
     (user_id, hour, block_type, label, skill, calls, bytes, cost_nanodollars) \
     WITH {calls}, \
     counted AS ( \
     SELECT c.*, c.appearance = 1 AND NOT EXISTS (SELECT 1 FROM gateway_tool_calls_seen s \
     WHERE s.user_id = c.user_id AND s.block_type = c.block_type AND s.call_id = c.call_id) new \
     FROM call c) \
     SELECT user_id, hour, block_type, COALESCE(label, ''), COALESCE(skill, ''), \
     SUM(CASE WHEN new THEN 1 ELSE 0 END), \
     SUM(CASE WHEN new THEN bytes ELSE 0 END), \
     SUM(cost) \
     FROM counted \
     GROUP BY 1, 2, 3, 4, 5 \
     {upsert}";

const CALLS_SEEN_SQL: &str = "INSERT OR IGNORE INTO gateway_tool_calls_seen (user_id, block_type, call_id) \
     WITH {calls} SELECT DISTINCT user_id, block_type, call_id FROM call";

const STAMP_SQL: &str = "UPDATE gateway_requests SET tools_aggregated_at = ? \
     WHERE tools_aggregated_at IS NULL AND completed_at IS NOT NULL {filter}";

/// Adds the tool parts of every finished, not yet counted request to their
/// buckets, or only the given request's. Returns how many requests were
/// counted. Run inside the transaction that finished the request so the
/// buckets and the row agree.
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
    let requests = REQUESTS_SQL
        .replace("{hour}", HOUR_FORMAT)
        .replace("{filter}", request_filter);
    let calls = CALLS_SQL.replace("{requests}", &requests);
    for sql in [
        DEFINITIONS_SQL
            .replace("{requests}", &requests)
            .replace("{upsert}", UPSERT_SQL),
        CALL_BUCKETS_SQL
            .replace("{calls}", &calls)
            .replace("{upsert}", UPSERT_SQL),
        CALLS_SEEN_SQL.replace("{calls}", &calls),
    ] {
        database
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                sql,
                id.clone(),
            ))
            .await?;
    }
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
/// changes a finished request's cost or parts after the fact.
pub async fn rebuild(database: &impl ConnectionTrait) -> Result<u64, DbErr> {
    database
        .execute_unprepared("DELETE FROM gateway_tool_usage_hourly")
        .await?;
    database
        .execute_unprepared("DELETE FROM gateway_tool_calls_seen")
        .await?;
    database
        .execute_unprepared("UPDATE gateway_requests SET tools_aggregated_at = NULL")
        .await?;
    aggregate(database, None).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use sea_orm::{Database, DatabaseConnection, FromQueryResult};
    use sea_orm_migration::MigratorTrait;

    #[derive(Debug, PartialEq, FromQueryResult)]
    struct Row {
        hour: String,
        block_type: String,
        label: String,
        skill: String,
        calls: i64,
        bytes: f64,
        cost_nanodollars: f64,
    }

    fn row(
        hour: &str,
        block_type: &str,
        label: &str,
        skill: &str,
        calls: i64,
        bytes: f64,
        cost_nanodollars: f64,
    ) -> Row {
        Row {
            hour: format!("2026-03-01T{hour}:00:00.000Z"),
            block_type: block_type.to_owned(),
            label: label.to_owned(),
            skill: skill.to_owned(),
            calls,
            bytes,
            cost_nanodollars,
        }
    }

    async fn database() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        db.execute_unprepared("INSERT INTO users(id,email_normalized,email_display,role,status,auth_version,created_at,updated_at) VALUES('u1','a@example.com','a@example.com','user','active',0,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')").await.unwrap();
        db.execute_unprepared("INSERT INTO gateway_keys(id,user_id,name,allowed_providers,created_at) VALUES('k1','u1','agent','[\"claude\"]','2026-01-01T00:00:00Z')").await.unwrap();
        db
    }

    /// A finished request priced at `cost_per_byte` nanodollars per byte
    /// over 1,000 measured bytes.
    async fn request(db: &DatabaseConnection, id: &str, started_at: &str, cost_per_byte: i64) {
        db.execute_unprepared(&format!(
            "INSERT INTO gateway_requests(id,request_id,provider,protocol,method,endpoint,started_at,completed_at,http_status,request_bytes,response_bytes,client_disconnected,key_id) \
             VALUES('{id}','{id}','claude','anthropic_messages','POST','/v1/messages','2026-03-01T{started_at}:00.000Z','2026-03-01T{started_at}:01.000Z',200,0,0,FALSE,'k1'); \
             INSERT INTO gateway_request_metrics(request_id,total_bytes,created_at) VALUES('{id}',1000,'2026-03-01T00:00:00Z'); \
             INSERT INTO gateway_usage(request_id,input_tokens,output_tokens,cost_nanodollars) VALUES('{id}',1,1,{})",
            cost_per_byte * 1000
        ))
        .await
        .unwrap();
    }

    /// `(block_type, tool_name, skill_name, tool_use_id)`.
    type Fact<'a> = (&'a str, Option<&'a str>, Option<&'a str>, Option<&'a str>);

    async fn part(
        db: &DatabaseConnection,
        request_id: &str,
        position: i64,
        blob_id: &str,
        bytes: i64,
        facts: &[Fact<'_>],
    ) {
        db.execute_unprepared(&format!(
            "INSERT OR IGNORE INTO gateway_payload_blobs(id,body,encoding,original_bytes,created_at) \
             VALUES('{blob_id}',x'00','identity',{bytes},'2026-03-01T00:00:00Z'); \
             INSERT INTO gateway_payload_part_refs(request_id,direction,path,position,kind,part_id) \
             VALUES('{request_id}','request','tools',{position},'tools','{blob_id}')"
        ))
        .await
        .unwrap();
        let quoted =
            |value: Option<&str>| value.map_or("NULL".to_owned(), |value| format!("'{value}'"));
        for (ordinal, (block_type, tool_name, skill_name, tool_use_id)) in facts.iter().enumerate()
        {
            db.execute_unprepared(&format!(
                "INSERT OR IGNORE INTO gateway_payload_blob_facts(blob_id,ordinal,block_type,tool_name,skill_name,tool_use_id) \
                 VALUES('{blob_id}',{ordinal},'{block_type}',{},{},{})",
                quoted(*tool_name),
                quoted(*skill_name),
                quoted(*tool_use_id),
            ))
            .await
            .unwrap();
        }
    }

    async fn rows(db: &DatabaseConnection) -> Vec<Row> {
        Row::find_by_statement(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT hour, block_type, label, skill, calls, bytes, cost_nanodollars \
             FROM gateway_tool_usage_hourly ORDER BY hour, block_type, label, skill",
        ))
        .all(db)
        .await
        .unwrap()
    }

    async fn seen(db: &DatabaseConnection) -> i64 {
        let row = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT COUNT(*) FROM gateway_tool_calls_seen",
            ))
            .await
            .unwrap()
            .unwrap();
        row.try_get_by_index(0).unwrap()
    }

    #[tokio::test]
    async fn a_replayed_call_counts_once_in_its_first_hour_and_costs_every_appearance() {
        let db = database().await;
        request(&db, "a", "10:15", 100).await;
        request(&db, "b", "10:45", 50).await;
        request(&db, "c", "11:05", 10).await;
        let call = ("tool_use", Some("Bash"), None, Some("toolu_1"));
        for (id, position) in [("a", 0), ("b", 0), ("c", 0)] {
            part(&db, id, position, "call", 100, &[call]).await;
        }
        part(&db, "c", 1, "call", 100, &[call]).await;

        assert_eq!(aggregate(&db, None).await.unwrap(), 3);
        assert_eq!(
            rows(&db).await,
            vec![
                row("10", "tool_use", "Bash", "", 1, 100.0, 15_000.0),
                row("11", "tool_use", "Bash", "", 0, 0.0, 2_000.0),
            ],
            "the call counts in the hour of a; b, and c twice, pay for their bytes"
        );
        assert_eq!(seen(&db).await, 1);
    }

    #[tokio::test]
    async fn a_definition_blob_splits_its_bytes_between_its_facts() {
        let db = database().await;
        request(&db, "a", "10:15", 100).await;
        part(
            &db,
            "a",
            0,
            "defs",
            4_000,
            &[
                ("tool_definition", Some("exec"), None, None),
                ("tool_definition", Some("wait"), None, None),
            ],
        )
        .await;

        assert_eq!(aggregate(&db, Some("a")).await.unwrap(), 1);
        assert_eq!(
            rows(&db).await,
            vec![
                row("10", "tool_definition", "exec", "", 1, 2_000.0, 200_000.0),
                row("10", "tool_definition", "wait", "", 1, 2_000.0, 200_000.0),
            ]
        );
    }

    #[tokio::test]
    async fn a_result_borrows_the_label_and_skill_of_its_call() {
        let db = database().await;
        request(&db, "a", "10:15", 100).await;
        request(&db, "b", "12:15", 1).await;
        part(
            &db,
            "a",
            0,
            "call",
            100,
            &[("tool_use", Some("Skill"), Some("unslop"), Some("toolu_1"))],
        )
        .await;
        part(
            &db,
            "a",
            1,
            "result",
            900,
            &[("tool_result", None, None, Some("toolu_1"))],
        )
        .await;
        part(
            &db,
            "b",
            0,
            "orphan",
            700,
            &[("tool_result", None, None, Some("toolu_lost"))],
        )
        .await;

        assert_eq!(aggregate(&db, None).await.unwrap(), 2);
        assert_eq!(
            rows(&db).await,
            vec![
                row("10", "tool_result", "Skill", "unslop", 1, 900.0, 90_000.0),
                row("10", "tool_use", "Skill", "unslop", 1, 100.0, 10_000.0),
                row("12", "tool_result", "", "", 1, 700.0, 700.0),
            ],
            "a result whose call was never captured keeps an empty label"
        );
    }

    #[tokio::test]
    async fn a_rebuild_reproduces_incremental_counting_and_a_retry_adds_nothing() {
        let db = database().await;
        request(&db, "a", "10:15", 100).await;
        request(&db, "b", "11:15", 50).await;
        part(
            &db,
            "a",
            0,
            "def",
            1_000,
            &[("tool_definition", Some("Bash"), None, None)],
        )
        .await;
        part(
            &db,
            "b",
            0,
            "def",
            1_000,
            &[("tool_definition", Some("Bash"), None, None)],
        )
        .await;
        for (id, position) in [("a", 1), ("b", 1)] {
            part(
                &db,
                id,
                position,
                "call",
                100,
                &[("tool_use", Some("Bash"), None, Some("toolu_1"))],
            )
            .await;
        }
        part(
            &db,
            "b",
            2,
            "glob",
            100,
            &[("tool_use", Some("Glob"), None, None)],
        )
        .await;

        assert_eq!(aggregate(&db, Some("b")).await.unwrap(), 1);
        assert_eq!(aggregate(&db, Some("b")).await.unwrap(), 0);
        assert_eq!(aggregate(&db, Some("a")).await.unwrap(), 1);
        let out_of_order = rows(&db).await;
        assert_eq!(
            out_of_order,
            vec![
                row("10", "tool_definition", "Bash", "", 1, 1_000.0, 100_000.0),
                row("10", "tool_use", "Bash", "", 0, 0.0, 10_000.0),
                row("11", "tool_definition", "Bash", "", 1, 1_000.0, 50_000.0),
                row("11", "tool_use", "Bash", "", 1, 100.0, 5_000.0),
                row("11", "tool_use", "Glob", "", 1, 100.0, 5_000.0),
            ],
            "finished out of order, the call lands in the hour that finished first; \
             a call without an id counts by blob"
        );

        assert_eq!(rebuild(&db).await.unwrap(), 2);
        assert_eq!(
            rows(&db).await,
            vec![
                row("10", "tool_definition", "Bash", "", 1, 1_000.0, 100_000.0),
                row("10", "tool_use", "Bash", "", 1, 100.0, 10_000.0),
                row("11", "tool_definition", "Bash", "", 1, 1_000.0, 50_000.0),
                row("11", "tool_use", "Bash", "", 0, 0.0, 5_000.0),
                row("11", "tool_use", "Glob", "", 1, 100.0, 5_000.0),
            ],
            "recounted in start order, the call lands in its earliest hour"
        );
        assert_eq!(seen(&db).await, 2);
        assert_eq!(aggregate(&db, None).await.unwrap(), 0);
    }
}
