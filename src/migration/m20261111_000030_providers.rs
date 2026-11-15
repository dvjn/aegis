use sea_orm::ConnectionTrait;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Providers and guardrails move from `config.toml` into the database, at two
/// layers: the instance (`user_id IS NULL`) and each user. A deleted provider
/// keeps its row with `deleted_at` set, so past usage can still be traced to
/// the type and upstream its ID meant; only live rows hold an ID.
///
/// A database that already has users came from a release that read these
/// settings from the configuration file, so it is marked for a one-time import
/// on the next start (see `settings::import_config_once`). A new database is
/// not marked, and the file's providers and guardrails never take effect.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS providers (
            id TEXT NOT NULL,
            user_id TEXT REFERENCES users(id) ON DELETE CASCADE,
            kind TEXT NOT NULL CHECK(kind IN
                ('claude_subscription','codex_subscription','typesafe','bedrock','vertex')),
            base_url TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            deleted_at TEXT)",
        )
        .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS ux_providers_instance ON providers(id) WHERE user_id IS NULL AND deleted_at IS NULL",
        )
        .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS ux_providers_user ON providers(user_id, id) WHERE user_id IS NOT NULL AND deleted_at IS NULL",
        )
        .await?;
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS settings (
            key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at TEXT NOT NULL)",
        )
        .await?;
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS user_guardrails (
            user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
            config TEXT NOT NULL, updated_at TEXT NOT NULL)",
        )
        .await?;
        db.execute_unprepared(
            "INSERT OR IGNORE INTO settings (key, value, updated_at)
            SELECT 'config_import_pending', strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now')
            WHERE EXISTS (SELECT 1 FROM users)",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        for table in ["user_guardrails", "settings", "providers"] {
            db.execute_unprepared(&format!("DROP TABLE IF EXISTS {table}"))
                .await?;
        }
        Ok(())
    }
}
