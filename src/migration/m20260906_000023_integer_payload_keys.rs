use sea_orm::{ConnectionTrait, DbBackend, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

const REQUEST_COLUMNS: &str = "id, request_id, provider, protocol, method, endpoint, requested_model, \
    started_at, first_byte_at, completed_at, http_status, request_bytes, response_bytes, \
    client_disconnected, error_message, key_id, key_version_id, aggregated_at, \
    tools_aggregated_at";

/// Every part reference repeated a request UUID and a blob hash as text in
/// four B-trees. Requests and blobs get an integer key, the (path, role, kind)
/// triple moves to its own table, and references carry only integers.
///
/// Dropping a parent table while foreign keys are enforced cascades into its
/// children, so enforcement is switched off for the rebuild and the result is
/// checked before it is committed. The pragma is a no-op inside a
/// transaction, which is why it is issued before one is opened.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let connection = manager.get_connection();
        connection
            .execute_unprepared("PRAGMA foreign_keys = OFF")
            .await?;
        let result = rebuild(manager).await;
        connection
            .execute_unprepared("PRAGMA foreign_keys = ON")
            .await?;
        result
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Migration(
            "integer payload keys cannot be rolled back".to_owned(),
        ))
    }
}

async fn rebuild(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let transaction = manager.begin().await?;
    let connection = transaction.get_connection();
    if pragma(connection, "foreign_keys").await? != 0 {
        return Err(DbErr::Migration(
            "foreign key enforcement stayed on for the payload key rebuild".to_owned(),
        ));
    }
    let response_refs = count(
        connection,
        "SELECT COUNT(*) FROM gateway_payload_part_refs WHERE direction <> 'request'",
    )
    .await?;
    if response_refs != 0 {
        return Err(DbErr::Migration(format!(
            "{response_refs} response part references have no place in the new schema"
        )));
    }

    rebuild_requests(connection).await?;
    rebuild_blobs(connection).await?;
    create_parts(connection).await?;

    let violations = connection
        .query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            "PRAGMA foreign_key_check",
        ))
        .await?;
    if !violations.is_empty() {
        return Err(DbErr::Migration(format!(
            "{} foreign key violations after the payload key rebuild",
            violations.len()
        )));
    }
    transaction.commit().await
}

/// The rebuild copies the columns named in `REQUEST_COLUMNS`, so a column
/// added by an earlier migration and missing from that list would be dropped
/// with every other check still passing.
async fn check_request_columns(connection: &impl ConnectionTrait) -> Result<(), DbErr> {
    let listed: Vec<&str> = REQUEST_COLUMNS.split(',').map(str::trim).collect();
    let rows = connection
        .query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            "PRAGMA table_info(gateway_requests)",
        ))
        .await?;
    let mut dropped = Vec::new();
    for row in rows {
        let column: String = row.try_get("", "name")?;
        if !listed.contains(&column.as_str()) {
            dropped.push(column);
        }
    }
    if !dropped.is_empty() {
        return Err(DbErr::Migration(format!(
            "gateway_requests has columns the rebuild would drop: {}",
            dropped.join(", ")
        )));
    }
    Ok(())
}

async fn rebuild_requests(connection: &impl ConnectionTrait) -> Result<(), DbErr> {
    check_request_columns(connection).await?;
    connection
        .execute_unprepared(
            "CREATE TABLE gateway_requests_new (
                seq INTEGER PRIMARY KEY,
                id TEXT NOT NULL UNIQUE,
                request_id TEXT NOT NULL,
                provider TEXT NOT NULL,
                protocol TEXT NOT NULL,
                method TEXT NOT NULL,
                endpoint TEXT NOT NULL,
                requested_model TEXT,
                started_at TEXT NOT NULL,
                first_byte_at TEXT,
                completed_at TEXT,
                http_status INTEGER,
                request_bytes INTEGER NOT NULL DEFAULT 0,
                response_bytes INTEGER NOT NULL DEFAULT 0,
                client_disconnected BOOLEAN NOT NULL DEFAULT FALSE,
                error_message TEXT,
                key_id TEXT,
                key_version_id TEXT,
                aggregated_at TEXT,
                tools_aggregated_at TEXT
            )",
        )
        .await?;
    connection
        .execute_unprepared(&format!(
            "INSERT INTO gateway_requests_new ({REQUEST_COLUMNS})
             SELECT {REQUEST_COLUMNS} FROM gateway_requests ORDER BY id"
        ))
        .await?;
    connection
        .execute_unprepared("DROP TABLE gateway_requests")
        .await?;
    connection
        .execute_unprepared("ALTER TABLE gateway_requests_new RENAME TO gateway_requests")
        .await?;
    for statement in [
        "CREATE INDEX ix_gateway_requests_started_at ON gateway_requests (started_at)",
        "CREATE INDEX ix_gateway_requests_provider ON gateway_requests (provider)",
        "CREATE INDEX ix_gateway_requests_key_id ON gateway_requests (key_id)",
        "CREATE INDEX ix_gateway_requests_unaggregated ON gateway_requests (id) WHERE aggregated_at IS NULL",
        "CREATE INDEX ix_gateway_requests_tools_unaggregated ON gateway_requests (id) WHERE tools_aggregated_at IS NULL",
    ] {
        connection.execute_unprepared(statement).await?;
    }
    Ok(())
}

async fn rebuild_blobs(connection: &impl ConnectionTrait) -> Result<(), DbErr> {
    connection
        .execute_unprepared(
            "CREATE TABLE gateway_payload_blobs_new (
                seq INTEGER PRIMARY KEY,
                id TEXT NOT NULL UNIQUE,
                body BLOB NOT NULL,
                encoding TEXT NOT NULL CHECK (encoding IN ('identity', 'gzip')),
                original_bytes INTEGER NOT NULL,
                created_at TEXT NOT NULL
            )",
        )
        .await?;
    connection
        .execute_unprepared(
            "INSERT INTO gateway_payload_blobs_new (id, body, encoding, original_bytes, created_at)
             SELECT id, body, encoding, original_bytes, created_at
             FROM gateway_payload_blobs ORDER BY rowid",
        )
        .await?;
    connection
        .execute_unprepared("DROP TABLE gateway_payload_blobs")
        .await?;
    connection
        .execute_unprepared("ALTER TABLE gateway_payload_blobs_new RENAME TO gateway_payload_blobs")
        .await?;
    Ok(())
}

async fn create_parts(connection: &impl ConnectionTrait) -> Result<(), DbErr> {
    connection
        .execute_unprepared(
            "CREATE TABLE gateway_payload_part_kinds (
                seq INTEGER PRIMARY KEY,
                path TEXT NOT NULL,
                role TEXT NOT NULL,
                kind TEXT NOT NULL,
                UNIQUE (path, role, kind)
            )",
        )
        .await?;
    connection
        .execute_unprepared(
            "INSERT INTO gateway_payload_part_kinds (path, role, kind)
             SELECT DISTINCT path, COALESCE(role, ''), kind
             FROM gateway_payload_part_refs ORDER BY path, role, kind",
        )
        .await?;
    connection
        .execute_unprepared(
            "CREATE TABLE gateway_payload_parts (
                request_seq INTEGER NOT NULL REFERENCES gateway_requests(seq) ON DELETE CASCADE,
                kind_seq INTEGER NOT NULL REFERENCES gateway_payload_part_kinds(seq),
                position INTEGER NOT NULL,
                blob_seq INTEGER NOT NULL REFERENCES gateway_payload_blobs(seq),
                PRIMARY KEY (request_seq, kind_seq, position)
            ) WITHOUT ROWID",
        )
        .await?;
    let old_refs = count(connection, "SELECT COUNT(*) FROM gateway_payload_part_refs").await?;
    let copied = connection
        .execute_unprepared(
            "INSERT INTO gateway_payload_parts (request_seq, kind_seq, position, blob_seq)
             SELECT r.seq, k.seq, p.position, b.seq
             FROM gateway_payload_part_refs p
             JOIN gateway_requests r ON r.id = p.request_id
             JOIN gateway_payload_part_kinds k
               ON k.path = p.path AND k.role = COALESCE(p.role, '') AND k.kind = p.kind
             JOIN gateway_payload_blobs b ON b.id = p.part_id",
        )
        .await?
        .rows_affected();
    if copied != old_refs {
        return Err(DbErr::Migration(format!(
            "copied {copied} of {old_refs} part references; some pointed at missing rows"
        )));
    }
    connection
        .execute_unprepared(
            "CREATE INDEX idx_gateway_payload_parts_blob ON gateway_payload_parts (request_seq, blob_seq)",
        )
        .await?;
    connection
        .execute_unprepared("DROP TABLE gateway_payload_part_refs")
        .await?;
    Ok(())
}

async fn pragma(connection: &impl ConnectionTrait, name: &str) -> Result<i64, DbErr> {
    let row = connection
        .query_one_raw(Statement::from_string(
            DbBackend::Sqlite,
            format!("PRAGMA {name}"),
        ))
        .await?
        .ok_or_else(|| DbErr::Migration(format!("PRAGMA {name} returned no row")))?;
    row.try_get_by_index(0)
}

async fn count(connection: &impl ConnectionTrait, sql: &str) -> Result<u64, DbErr> {
    let row = connection
        .query_one_raw(Statement::from_string(DbBackend::Sqlite, sql))
        .await?
        .ok_or_else(|| DbErr::Migration("count returned no row".to_owned()))?;
    let value: i64 = row.try_get_by_index(0)?;
    Ok(value as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::Database;

    async fn requests_table(extra: &str) -> sea_orm::DatabaseConnection {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        let columns = REQUEST_COLUMNS
            .split(',')
            .map(|column| format!("{} TEXT", column.trim()))
            .collect::<Vec<_>>()
            .join(", ");
        database
            .execute_unprepared(&format!("CREATE TABLE gateway_requests ({columns}{extra})"))
            .await
            .unwrap();
        database
    }

    #[tokio::test]
    async fn the_rebuild_refuses_a_column_it_would_drop() {
        let database = requests_table(", added_by_a_later_migration TEXT").await;
        let error = check_request_columns(&database)
            .await
            .expect_err("a column outside the copied list must stop the rebuild");
        assert!(
            error.to_string().contains("added_by_a_later_migration"),
            "the error names the column at risk: {error}"
        );
    }

    #[tokio::test]
    async fn the_rebuild_accepts_exactly_the_copied_columns() {
        let database = requests_table("").await;
        check_request_columns(&database).await.unwrap();
    }
}
