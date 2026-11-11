use sea_orm::ConnectionTrait;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS relay_profiles (
            id TEXT PRIMARY KEY,
            client_id TEXT NOT NULL REFERENCES gateway_clients(id) ON DELETE CASCADE,
            cli TEXT NOT NULL CHECK(cli IN ('claude','codex','agy')),
            label TEXT NOT NULL, provider TEXT NOT NULL,
            observed_account TEXT, account_changed_at TEXT,
            created_at TEXT NOT NULL, updated_at TEXT NOT NULL, deleted_at TEXT)",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS ix_relay_profiles_client ON relay_profiles(client_id)",
        )
        .await?;
        for column in ["profile_id", "run_id", "session_id", "upstream_account"] {
            if !manager.has_column("gateway_requests", column).await? {
                db.execute_unprepared(&format!(
                    "ALTER TABLE gateway_requests ADD COLUMN {column} TEXT"
                ))
                .await?;
            }
        }
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS ix_gateway_requests_profile ON gateway_requests(profile_id)",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS ix_gateway_requests_session ON gateway_requests(session_id)",
        )
        .await?;
        // The bucket key gains profile_id; SQLite cannot alter a key in place.
        if !manager
            .has_column("gateway_usage_hourly", "profile_id")
            .await?
        {
            db.execute_unprepared(
                "CREATE TABLE gateway_usage_hourly_new (
                user_id TEXT NOT NULL, hour TEXT NOT NULL, provider TEXT NOT NULL,
                requested_model TEXT NOT NULL, client_id TEXT NOT NULL,
                profile_id TEXT NOT NULL DEFAULT '',
                requests INTEGER NOT NULL, succeeded INTEGER NOT NULL, failed INTEGER NOT NULL,
                input_tokens INTEGER NOT NULL, cache_read_tokens INTEGER NOT NULL,
                cache_write_tokens INTEGER NOT NULL, output_tokens INTEGER NOT NULL,
                reasoning_tokens INTEGER NOT NULL, cost_nanodollars INTEGER NOT NULL,
                unpriced INTEGER NOT NULL,
                PRIMARY KEY (user_id, hour, provider, requested_model, client_id, profile_id)
            ) WITHOUT ROWID",
            )
            .await?;
            db.execute_unprepared(
                "INSERT INTO gateway_usage_hourly_new
                (user_id,hour,provider,requested_model,client_id,profile_id,requests,succeeded,failed,
                input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,reasoning_tokens,
                cost_nanodollars,unpriced)
                SELECT user_id,hour,provider,requested_model,client_id,'',requests,succeeded,failed,
                input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,reasoning_tokens,
                cost_nanodollars,unpriced FROM gateway_usage_hourly",
            )
            .await?;
            db.execute_unprepared("DROP TABLE gateway_usage_hourly")
                .await?;
            db.execute_unprepared(
                "ALTER TABLE gateway_usage_hourly_new RENAME TO gateway_usage_hourly",
            )
            .await?;
        }
        Ok(())
    }

    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "relay profile migration is not reversible".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use crate::migration::Migrator;
    use sea_orm::{ConnectionTrait, Database, DbBackend, Statement};
    use sea_orm_migration::MigratorTrait;

    #[tokio::test]
    async fn buckets_keep_their_rows_and_gain_a_profile_key() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, Some((Migrator::migrations().len() - 2) as u32))
            .await
            .unwrap();
        db.execute_unprepared("INSERT INTO gateway_usage_hourly(user_id,hour,provider,requested_model,client_id,requests,succeeded,failed,input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,reasoning_tokens,cost_nanodollars,unpriced) VALUES('u1','2026-01-01T00:00:00.000Z','p','m','c',1,1,0,10,0,0,2,0,5,0)").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let row = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT profile_id,input_tokens,cost_nanodollars FROM gateway_usage_hourly",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get::<String>("", "profile_id").unwrap(), "");
        assert_eq!(row.try_get::<i64>("", "input_tokens").unwrap(), 10);
        // The same bucket under another profile is a separate row.
        db.execute_unprepared("INSERT INTO gateway_usage_hourly(user_id,hour,provider,requested_model,client_id,profile_id,requests,succeeded,failed,input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,reasoning_tokens,cost_nanodollars,unpriced) VALUES('u1','2026-01-01T00:00:00.000Z','p','m','c','prof',1,1,0,1,0,0,0,0,0,0)").await.unwrap();
    }

    #[tokio::test]
    async fn fresh_database_has_the_profile_schema() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        for column in ["profile_id", "run_id", "session_id", "upstream_account"] {
            db.execute_unprepared(&format!("SELECT {column} FROM gateway_requests LIMIT 1"))
                .await
                .unwrap();
        }
        db.execute_unprepared("SELECT id,client_id,vendor,label,provider,observed_account,account_changed_at,created_at,updated_at,deleted_at FROM relay_accounts").await.unwrap();
        let row = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT COUNT(*) n FROM pragma_table_info('gateway_usage_hourly') WHERE name='profile_id'",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get::<i64>("", "n").unwrap(), 1);
    }
}
