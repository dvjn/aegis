use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r#"
                UPDATE oauth_access_tokens
                SET revoked_at = COALESCE(revoked_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));

                UPDATE oauth_refresh_token_families
                SET revoked_at = COALESCE(revoked_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));

                UPDATE oauth_authorization_codes
                SET consumed_at = COALESCE(consumed_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));

                UPDATE oauth_device_authorizations
                SET status = 'denied',
                    decision_at = COALESCE(decision_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
                WHERE status IN ('pending', 'approved');

                UPDATE oauth_clients
                SET scope = CASE
                    WHEN instr(grant_types, '"refresh_token"') > 0
                    THEN 'usage:read requests:read payloads:read keys:read offline_access'
                    ELSE 'usage:read requests:read payloads:read keys:read'
                END;
                "#,
            )
            .await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ConnectionTrait, Database, DbBackend, Statement};

    async fn scalar(db: &impl ConnectionTrait, sql: &str) -> i64 {
        db.query_one_raw(Statement::from_string(DbBackend::Sqlite, sql))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "value")
            .unwrap()
    }

    #[tokio::test]
    async fn old_grants_are_invalidated_and_client_ceilings_are_replaced() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        db.execute_unprepared(
            r#"
            CREATE TABLE oauth_access_tokens (scope TEXT NOT NULL, revoked_at TEXT);
            CREATE TABLE oauth_refresh_token_families (scope TEXT NOT NULL, revoked_at TEXT);
            CREATE TABLE oauth_authorization_codes (scope TEXT NOT NULL, consumed_at TEXT);
            CREATE TABLE oauth_device_authorizations (
                scope TEXT NOT NULL,
                status TEXT NOT NULL,
                decision_at TEXT
            );
            CREATE TABLE oauth_clients (
                scope TEXT NOT NULL,
                grant_types TEXT NOT NULL
            );

            INSERT INTO oauth_access_tokens VALUES ('mcp:read', NULL);
            INSERT INTO oauth_refresh_token_families VALUES ('analytics:read', NULL);
            INSERT INTO oauth_authorization_codes VALUES ('mcp:write', NULL);
            INSERT INTO oauth_device_authorizations VALUES ('analytics:payloads', 'pending', NULL);
            INSERT INTO oauth_clients VALUES ('mcp:read', '["authorization_code"]');
            INSERT INTO oauth_clients VALUES ('mcp:read offline_access', '["refresh_token"]');
            "#,
        )
        .await
        .unwrap();

        Migration.up(&SchemaManager::new(&db)).await.unwrap();

        assert_eq!(
            scalar(
                &db,
                "SELECT COUNT(*) value FROM oauth_access_tokens WHERE revoked_at IS NOT NULL"
            )
            .await,
            1
        );
        assert_eq!(
            scalar(
                &db,
                "SELECT COUNT(*) value FROM oauth_refresh_token_families WHERE revoked_at IS NOT NULL"
            )
            .await,
            1
        );
        assert_eq!(
            scalar(
                &db,
                "SELECT COUNT(*) value FROM oauth_authorization_codes WHERE consumed_at IS NOT NULL"
            )
            .await,
            1
        );
        assert_eq!(
            scalar(
                &db,
                "SELECT COUNT(*) value FROM oauth_device_authorizations WHERE status = 'denied' AND decision_at IS NOT NULL"
            )
            .await,
            1
        );
        assert_eq!(
            scalar(
                &db,
                "SELECT COUNT(*) value FROM oauth_clients WHERE scope = 'usage:read requests:read payloads:read keys:read'"
            )
            .await,
            1
        );
        assert_eq!(
            scalar(
                &db,
                "SELECT COUNT(*) value FROM oauth_clients WHERE scope = 'usage:read requests:read payloads:read keys:read offline_access'"
            )
            .await,
            1
        );
    }
}
