use crate::usage_json;
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use sea_orm_migration::prelude::*;

const BATCH_ROWS: usize = 500;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if !manager
            .has_column("gateway_usage", "raw_usage_encoding")
            .await?
        {
            manager
                .get_connection()
                .execute_unprepared(
                    "ALTER TABLE gateway_usage ADD COLUMN raw_usage_encoding TEXT NOT NULL \
                     DEFAULT 'identity' CHECK (raw_usage_encoding IN ('identity', 'gzip'))",
                )
                .await?;
        }
        let mut after_request_id = String::new();
        loop {
            let batch = manager.begin().await?;
            let rows = batch
                .get_connection()
                .query_all_raw(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    "SELECT request_id, raw_usage_json FROM gateway_usage \
                     WHERE raw_usage_encoding = 'identity' AND raw_usage_json IS NOT NULL \
                     AND request_id > ? ORDER BY request_id LIMIT ?",
                    [after_request_id.clone().into(), (BATCH_ROWS as i64).into()],
                ))
                .await?;
            if rows.is_empty() {
                batch.commit().await?;
                return Ok(());
            }
            for row in &rows {
                let request_id: String = row.try_get("", "request_id")?;
                let raw: Vec<u8> = row.try_get("", "raw_usage_json")?;
                let (stored, encoding) = crate::compression::gzip_if_smaller(raw);
                if encoding == usage_json::IDENTITY {
                    after_request_id = request_id;
                    continue;
                }
                batch
                    .get_connection()
                    .execute_raw(Statement::from_sql_and_values(
                        DbBackend::Sqlite,
                        "UPDATE gateway_usage SET raw_usage_json = ?, raw_usage_encoding = ? \
                         WHERE request_id = ?",
                        [stored.into(), encoding.into(), request_id.clone().into()],
                    ))
                    .await?;
                after_request_id = request_id;
            }
            batch.commit().await?;
        }
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let database = manager.get_connection();
        let rows = database
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT request_id, raw_usage_json FROM gateway_usage WHERE raw_usage_encoding = 'gzip'",
            ))
            .await?;
        for row in rows {
            let request_id: String = row.try_get("", "request_id")?;
            let raw: Vec<u8> = row.try_get("", "raw_usage_json")?;
            let json = usage_json::decode(&raw, usage_json::GZIP)
                .ok_or_else(|| DbErr::Migration(format!("usage for {request_id} is not gzip")))?;
            database
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    "UPDATE gateway_usage SET raw_usage_json = ?, raw_usage_encoding = 'identity' \
                     WHERE request_id = ?",
                    [json.into(), request_id.into()],
                ))
                .await?;
        }
        database
            .execute_unprepared("ALTER TABLE gateway_usage DROP COLUMN raw_usage_encoding")
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn existing_identity_rows_are_compressed_once() {
        let fixture = crate::db::tests::FileDatabase::new().await;
        let db = &fixture.database;
        let large = format!(r#"{{"attribution":"{}"}}"#, "x".repeat(4096));
        for (id, json) in [("u-large", large.as_str()), ("u-small", "{}")] {
            db.execute_unprepared(&format!(
                "INSERT INTO gateway_requests(id,request_id,provider,protocol,method,endpoint,started_at,request_bytes,response_bytes,client_disconnected) \
                 VALUES('{id}','{id}','claude','anthropic_messages','POST','/v1/messages','2026-09-06T00:00:00Z',0,0,FALSE)"
            ))
            .await
            .unwrap();
            db.execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO gateway_usage(request_id,input_tokens,output_tokens,raw_usage_json,raw_usage_encoding) VALUES(?,1,1,?,'identity')",
                [id.into(), json.into()],
            ))
            .await
            .unwrap();
        }
        for _ in 0..2 {
            Migration.up(&SchemaManager::new(db)).await.unwrap();
            let encodings = encodings(db).await;
            assert_eq!(
                encodings,
                [
                    ("u-large".into(), "gzip".into()),
                    ("u-small".into(), "identity".into())
                ]
            );
            assert_eq!(
                usage_json::load(db, "u-large").await.unwrap().as_deref(),
                Some(large.as_str())
            );
            assert_eq!(
                usage_json::load(db, "u-small").await.unwrap().as_deref(),
                Some("{}")
            );
        }
        Migration.down(&SchemaManager::new(db)).await.unwrap();
        let row = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT raw_usage_json FROM gateway_usage WHERE request_id = 'u-large'",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get::<String>("", "raw_usage_json").unwrap(), large);
        db.close_by_ref().await.unwrap();
    }

    async fn encodings(db: &sea_orm::DatabaseConnection) -> Vec<(String, String)> {
        db.query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT request_id, raw_usage_encoding FROM gateway_usage ORDER BY request_id",
        ))
        .await
        .unwrap()
        .iter()
        .map(|row| {
            (
                row.try_get("", "request_id").unwrap(),
                row.try_get("", "raw_usage_encoding").unwrap(),
            )
        })
        .collect()
    }
}
