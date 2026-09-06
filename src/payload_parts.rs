use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement};

/// A part without a role is stored with an empty role so the kind table's
/// UNIQUE constraint can see it; SQLite treats NULLs as distinct.
pub(crate) fn role_column(role: Option<&str>) -> &str {
    role.unwrap_or("")
}

pub(crate) async fn kind_seq(
    database: &impl ConnectionTrait,
    path: &str,
    role: Option<&str>,
    kind: &str,
) -> Result<i64, DbErr> {
    let values = [
        path.to_owned().into(),
        role_column(role).to_owned().into(),
        kind.to_owned().into(),
    ];
    database
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT OR IGNORE INTO gateway_payload_part_kinds (path, role, kind) VALUES (?, ?, ?)",
            values.clone(),
        ))
        .await?;
    let row = database
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT seq FROM gateway_payload_part_kinds WHERE path = ? AND role = ? AND kind = ?",
            values,
        ))
        .await?
        .ok_or_else(|| DbErr::RecordNotFound(format!("part kind {path} {kind}")))?;
    row.try_get("", "seq")
}

pub(crate) async fn insert(
    database: &impl ConnectionTrait,
    request_seq: i64,
    kind_seq: i64,
    position: i64,
    blob_seq: i64,
) -> Result<(), DbErr> {
    database
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO gateway_payload_parts (request_seq, kind_seq, position, blob_seq) VALUES (?, ?, ?, ?)",
            [
                request_seq.into(),
                kind_seq.into(),
                position.into(),
                blob_seq.into(),
            ],
        ))
        .await?;
    Ok(())
}

/// Insert a part reference from the text ids a fixture has to hand, resolving
/// the request, kind and blob keys itself.
#[cfg(test)]
pub(crate) async fn insert_by_id(
    database: &impl ConnectionTrait,
    request_id: &str,
    path: &str,
    role: Option<&str>,
    kind: &str,
    position: i64,
    blob_id: &str,
) -> Result<(), DbErr> {
    let request = request_seq(database, request_id)
        .await?
        .ok_or_else(|| DbErr::RecordNotFound(format!("request {request_id}")))?;
    let kind = kind_seq(database, path, role, kind).await?;
    let blob = blob_seq(database, blob_id)
        .await?
        .ok_or_else(|| DbErr::RecordNotFound(format!("blob {blob_id}")))?;
    insert(database, request, kind, position, blob).await
}

#[cfg(test)]
pub(crate) async fn blob_seq(
    database: &impl ConnectionTrait,
    blob_id: &str,
) -> Result<Option<i64>, DbErr> {
    let row = database
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT seq FROM gateway_payload_blobs WHERE id = ?",
            [blob_id.to_owned().into()],
        ))
        .await?;
    row.map(|row| row.try_get("", "seq")).transpose()
}

pub(crate) async fn request_seq(
    database: &impl ConnectionTrait,
    request_id: &str,
) -> Result<Option<i64>, DbErr> {
    let row = database
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT seq FROM gateway_requests WHERE id = ?",
            [request_id.to_owned().into()],
        ))
        .await?;
    row.map(|row| row.try_get("", "seq")).transpose()
}
