//! Splits the single-CLI "profile" into an account (one vendor sign-in) and a new, empty
//! profile (a named context). Existing rows become accounts under the same ids.
use sea_orm::ConnectionTrait;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "CREATE TABLE relay_accounts (
            id TEXT PRIMARY KEY,
            client_id TEXT NOT NULL REFERENCES gateway_clients(id) ON DELETE CASCADE,
            vendor TEXT NOT NULL CHECK(vendor IN ('anthropic','openai','google')),
            label TEXT NOT NULL, provider TEXT NOT NULL,
            observed_account TEXT, account_changed_at TEXT,
            created_at TEXT NOT NULL, updated_at TEXT NOT NULL, deleted_at TEXT)",
        )
        .await?;
        db.execute_unprepared(
            "INSERT INTO relay_accounts
            (id,client_id,vendor,label,provider,observed_account,account_changed_at,created_at,updated_at,deleted_at)
            SELECT id,client_id,
            CASE cli WHEN 'claude' THEN 'anthropic' WHEN 'codex' THEN 'openai' ELSE 'google' END,
            label,provider,observed_account,account_changed_at,created_at,updated_at,deleted_at
            FROM relay_profiles",
        )
        .await?;
        db.execute_unprepared("DROP TABLE relay_profiles").await?;
        db.execute_unprepared("CREATE INDEX ix_relay_accounts_client ON relay_accounts(client_id)")
            .await?;
        db.execute_unprepared(
            "CREATE TABLE relay_profiles (
            id TEXT PRIMARY KEY,
            client_id TEXT NOT NULL REFERENCES gateway_clients(id) ON DELETE CASCADE,
            label TEXT NOT NULL,
            created_at TEXT NOT NULL, updated_at TEXT NOT NULL, deleted_at TEXT)",
        )
        .await?;
        db.execute_unprepared("CREATE INDEX ix_relay_profiles_client ON relay_profiles(client_id)")
            .await?;

        db.execute_unprepared("ALTER TABLE gateway_requests ADD COLUMN account_id TEXT")
            .await?;
        db.execute_unprepared(
            "UPDATE gateway_requests SET account_id = profile_id, profile_id = NULL
             WHERE profile_id IS NOT NULL",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX ix_gateway_requests_account ON gateway_requests(account_id)",
        )
        .await?;

        db.execute_unprepared(
            "CREATE TABLE gateway_usage_hourly_new (
            user_id TEXT NOT NULL, hour TEXT NOT NULL, provider TEXT NOT NULL,
            requested_model TEXT NOT NULL, client_id TEXT NOT NULL,
            profile_id TEXT NOT NULL DEFAULT '',
            account_id TEXT NOT NULL DEFAULT '',
            requests INTEGER NOT NULL, succeeded INTEGER NOT NULL, failed INTEGER NOT NULL,
            input_tokens INTEGER NOT NULL, cache_read_tokens INTEGER NOT NULL,
            cache_write_tokens INTEGER NOT NULL, output_tokens INTEGER NOT NULL,
            reasoning_tokens INTEGER NOT NULL, cost_nanodollars INTEGER NOT NULL,
            unpriced INTEGER NOT NULL,
            PRIMARY KEY (user_id, hour, provider, requested_model, client_id, profile_id, account_id)
        ) WITHOUT ROWID",
        )
        .await?;
        db.execute_unprepared(
            "INSERT INTO gateway_usage_hourly_new
            (user_id,hour,provider,requested_model,client_id,profile_id,account_id,requests,succeeded,
            failed,input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,reasoning_tokens,
            cost_nanodollars,unpriced)
            SELECT user_id,hour,provider,requested_model,client_id,'',profile_id,requests,succeeded,
            failed,input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,reasoning_tokens,
            cost_nanodollars,unpriced FROM gateway_usage_hourly",
        )
        .await?;
        db.execute_unprepared("DROP TABLE gateway_usage_hourly")
            .await?;
        db.execute_unprepared(
            "ALTER TABLE gateway_usage_hourly_new RENAME TO gateway_usage_hourly",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "relay account migration is not reversible".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use crate::migration::Migrator;
    use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement};
    use sea_orm_migration::MigratorTrait;

    async fn text(db: &DatabaseConnection, sql: &str) -> String {
        db.query_one_raw(Statement::from_string(DbBackend::Sqlite, sql))
            .await
            .unwrap()
            .unwrap()
            .try_get_by_index::<String>(0)
            .unwrap()
    }

    #[tokio::test]
    async fn profiles_become_accounts_and_attribution_follows() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, Some((Migrator::migrations().len() - 1) as u32))
            .await
            .unwrap();
        db.execute_unprepared("PRAGMA foreign_keys=OFF")
            .await
            .unwrap();
        db.execute_unprepared("INSERT INTO relay_profiles(id,client_id,cli,label,provider,observed_account,account_changed_at,created_at,updated_at) VALUES('p1','c1','claude','work','claude','acct',NULL,'t','t'),('p2','c1','codex','me','codex',NULL,NULL,'t','t')").await.unwrap();
        db.execute_unprepared("INSERT INTO gateway_requests(id,request_id,client_id,provider,protocol,method,endpoint,started_at,request_bytes,profile_id,run_id) VALUES('r1','q1','c1','claude','x','POST','/m','2026-01-01T00:00:00Z',0,'p1','run'),('r2','q2','c1','claude','x','POST','/m','2026-01-01T00:00:00Z',0,NULL,NULL)").await.unwrap();
        db.execute_unprepared("INSERT INTO gateway_usage_hourly(user_id,hour,provider,requested_model,client_id,profile_id,requests,succeeded,failed,input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,reasoning_tokens,cost_nanodollars,unpriced) VALUES('u1','2026-01-01T00:00:00.000Z','p','m','c1','p1',1,1,0,10,0,0,2,0,5,0),('u1','2026-01-01T00:00:00.000Z','p','m','c1','',1,1,0,3,0,0,0,0,0,0)").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        assert_eq!(
            text(&db, "SELECT group_concat(id||':'||vendor||':'||label||':'||COALESCE(observed_account,'-'),',') FROM (SELECT * FROM relay_accounts ORDER BY id)").await,
            "p1:anthropic:work:acct,p2:openai:me:-"
        );
        assert_eq!(
            text(&db, "SELECT CAST(COUNT(*) AS TEXT) FROM relay_profiles").await,
            "0"
        );
        assert_eq!(
            text(&db, "SELECT group_concat(id||':'||COALESCE(account_id,'-')||':'||COALESCE(profile_id,'-')||':'||COALESCE(run_id,'-'),',') FROM (SELECT * FROM gateway_requests ORDER BY id)").await,
            "r1:p1:-:run,r2:-:-:-"
        );
        assert_eq!(
            text(&db, "SELECT group_concat(account_id||'/'||profile_id||'/'||input_tokens,',') FROM (SELECT * FROM gateway_usage_hourly ORDER BY input_tokens)").await,
            "//3,p1//10"
        );
        db.execute_unprepared(
            "SELECT id,client_id,label,created_at,updated_at,deleted_at FROM relay_profiles",
        )
        .await
        .unwrap();
        assert!(db.execute_unprepared("INSERT INTO relay_accounts(id,client_id,vendor,label,provider,created_at,updated_at) VALUES('x','c1','vim','l','p','t','t')").await.is_err());
    }
}
