use crate::migration::Migrator;
use anyhow::{Context, Result};
use sea_orm::{ConnectOptions, Database, DatabaseConnection, sqlx::sqlite::SqliteJournalMode};
use sea_orm_migration::MigratorTrait;
use std::{path::Path, time::Duration};

pub async fn connect(database_url: &str) -> Result<DatabaseConnection> {
    ensure_sqlite_parent(database_url)?;
    let mut options = ConnectOptions::new(database_url);
    options.max_connections(4);
    options.map_sqlx_sqlite_opts(|options| {
        options
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5))
    });

    let database = Database::connect(options)
        .await
        .context("failed to connect to the database")?;
    Migrator::up(&database, None)
        .await
        .context("failed to apply database migrations")?;
    Ok(database)
}

fn ensure_sqlite_parent(database_url: &str) -> Result<()> {
    let Some(path) = database_url.strip_prefix("sqlite://") else {
        return Ok(());
    };
    let path = path.split('?').next().unwrap_or(path);
    if path.is_empty() || path == ":memory:" {
        return Ok(());
    }
    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create SQLite directory {}", parent.display()))?;
    }
    Ok(())
}
