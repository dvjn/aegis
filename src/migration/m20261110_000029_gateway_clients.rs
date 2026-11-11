use sea_orm::ConnectionTrait;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS gateway_clients (
            id TEXT PRIMARY KEY, user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            kind TEXT NOT NULL CHECK(kind IN ('api_key','oauth')), name TEXT NOT NULL,
            oauth_client_id TEXT, oauth_issuer TEXT, allowed_providers TEXT NOT NULL,
            created_at TEXT NOT NULL, last_used_at TEXT,
            UNIQUE(user_id,oauth_client_id,oauth_issuer))",
        )
        .await?;
        db.execute_unprepared("INSERT OR IGNORE INTO gateway_clients
            (id,user_id,kind,name,oauth_client_id,oauth_issuer,allowed_providers,created_at)
            SELECT k.id,k.user_id,CASE WHEN k.oauth_client_id IS NULL THEN 'api_key' ELSE 'oauth' END,
            COALESCE(c.client_name,k.name),k.oauth_client_id,k.oauth_issuer,k.allowed_providers,k.created_at
            FROM gateway_keys k LEFT JOIN oauth_clients c ON c.id=k.oauth_client_id AND c.issuer=k.oauth_issuer").await?;
        if !manager.has_column("gateway_requests", "client_id").await? {
            db.execute_unprepared("ALTER TABLE gateway_requests ADD COLUMN client_id TEXT")
                .await?;
        }
        db.execute_unprepared(
            "UPDATE gateway_requests SET client_id=key_id WHERE client_id IS NULL",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE gateway_requests SET key_id=NULL,key_version_id=NULL
            WHERE client_id IN (SELECT id FROM gateway_clients WHERE kind='oauth')",
        )
        .await?;
        // Old relay secrets are no longer accepted, including already-issued versions.
        db.execute_unprepared("DELETE FROM gateway_keys WHERE oauth_client_id IS NOT NULL")
            .await?;
        for table in ["gateway_usage_hourly", "gateway_guardrails_hourly"] {
            if manager.has_column(table, "key_id").await? {
                db.execute_unprepared(&format!(
                    "ALTER TABLE {table} RENAME COLUMN key_id TO client_id"
                ))
                .await?;
            }
        }
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS ix_gateway_requests_client ON gateway_requests(client_id)",
        )
        .await?;
        // API-key creation remains compatible with old fixtures and external callers.
        db.execute_unprepared("CREATE TRIGGER IF NOT EXISTS gateway_key_client_insert AFTER INSERT ON gateway_keys BEGIN
            INSERT INTO gateway_clients(id,user_id,kind,name,allowed_providers,created_at)
            VALUES(NEW.id,NEW.user_id,'api_key',NEW.name,NEW.allowed_providers,NEW.created_at); END").await?;
        db.execute_unprepared("CREATE TRIGGER IF NOT EXISTS gateway_key_client_update AFTER UPDATE OF name,allowed_providers ON gateway_keys BEGIN
            UPDATE gateway_clients SET name=NEW.name,allowed_providers=NEW.allowed_providers WHERE id=NEW.id AND kind='api_key'; END").await?;
        db.execute_unprepared("CREATE TRIGGER IF NOT EXISTS gateway_request_client_insert AFTER INSERT ON gateway_requests
            WHEN NEW.client_id IS NULL AND NEW.key_id IS NOT NULL BEGIN
            UPDATE gateway_requests SET client_id=NEW.key_id WHERE id=NEW.id; END").await?;
        db.execute_unprepared("CREATE TRIGGER IF NOT EXISTS gateway_request_client_update AFTER UPDATE OF key_id ON gateway_requests
            WHEN NEW.key_id IS NOT NULL BEGIN UPDATE gateway_requests SET client_id=NEW.key_id WHERE id=NEW.id; END").await?;
        Ok(())
    }

    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "OAuth client migration cannot restore removed relay secrets".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use sea_orm::{Database, DbBackend, Statement};
    use sea_orm_migration::MigratorTrait;

    #[tokio::test]
    async fn legacy_relay_secrets_are_removed_without_losing_request_or_bucket_history() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, Some((Migrator::migrations().len() - 3) as u32))
            .await
            .unwrap();
        for sql in [
            "INSERT INTO users(id,email_normalized,email_display,role,status,auth_version,created_at,updated_at) VALUES('u1','migration@example.com','migration@example.com','user','active',0,'2026-01-01','2026-01-01')",
            "INSERT INTO oauth_clients(id,issuer,client_name,application_type,grant_types,response_types,scope,token_endpoint_auth_method,created_at) VALUES('app1','https://aegis.example','AGS machine','native','refresh_token','','relay:manage offline_access','none','2026-01-01')",
            "INSERT INTO gateway_keys(id,user_id,name,allowed_providers,created_at,oauth_client_id,oauth_issuer) VALUES('old','u1','internal-key','[\"personal\"]','2026-01-01','app1','https://aegis.example')",
            "INSERT INTO gateway_key_versions(id,key_id,key_hash,prefix,created_at) VALUES('old-version','old','unused-hash','unused-prefix','2026-01-01')",
            "INSERT INTO gateway_requests(id,request_id,provider,protocol,method,endpoint,started_at,key_id,key_version_id) VALUES('request','request','personal','openai_responses','POST','/responses','2026-01-01','old','old-version')",
            "INSERT INTO gateway_usage_hourly(user_id,hour,provider,requested_model,key_id,requests,succeeded,failed,input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,reasoning_tokens,cost_nanodollars,unpriced) VALUES('u1','2026-01-01T00:00:00.000Z','personal','model','old',1,1,0,10,0,0,2,0,0,0)",
        ] {
            db.execute_unprepared(sql).await.unwrap();
        }
        Migrator::up(&db, None).await.unwrap();
        let row=db.query_one_raw(Statement::from_string(DbBackend::Sqlite,
            "SELECT r.client_id,r.key_id,r.key_version_id,c.kind,c.name,h.input_tokens FROM gateway_requests r JOIN gateway_clients c ON c.id=r.client_id JOIN gateway_usage_hourly h ON h.client_id=c.id")).await.unwrap().unwrap();
        assert_eq!(row.try_get::<String>("", "client_id").unwrap(), "old");
        assert!(
            row.try_get::<Option<String>>("", "key_id")
                .unwrap()
                .is_none()
        );
        assert!(
            row.try_get::<Option<String>>("", "key_version_id")
                .unwrap()
                .is_none()
        );
        assert_eq!(row.try_get::<String>("", "kind").unwrap(), "oauth");
        assert_eq!(row.try_get::<String>("", "name").unwrap(), "AGS machine");
        assert_eq!(row.try_get::<i64>("", "input_tokens").unwrap(), 10);
        for table in ["gateway_keys", "gateway_key_versions"] {
            let row = db
                .query_one_raw(Statement::from_string(
                    DbBackend::Sqlite,
                    format!("SELECT COUNT(*) n FROM {table}"),
                ))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.try_get::<i64>("", "n").unwrap(), 0);
        }
    }
}
