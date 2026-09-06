//! Rebuilding the payload tables frees pages inside the database file without
//! returning them to the filesystem. VACUUM rewrites the file to reclaim them.
//! It takes an exclusive lock, needs free space for a second copy, and cannot
//! run inside a transaction, so it runs once, after the jobs that rewrite rows.

use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement};

pub const NAME: &str = "vacuum_after_integer_payload_keys";

/// Returns the megabytes returned to the filesystem.
pub async fn run(database: &DatabaseConnection) -> Result<u64, DbErr> {
    let before = file_bytes(database).await?;
    database.execute_unprepared("VACUUM").await?;
    let after = file_bytes(database).await?;
    tracing::info!(
        before_bytes = before,
        after_bytes = after,
        "database file compacted"
    );
    Ok(before.saturating_sub(after) / (1024 * 1024))
}

async fn file_bytes(database: &DatabaseConnection) -> Result<u64, DbErr> {
    let row = database
        .query_one_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
        ))
        .await?
        .ok_or_else(|| DbErr::Migration("page count returned no row".to_owned()))?;
    let bytes: i64 = row.try_get_by_index(0)?;
    Ok(bytes as u64)
}
