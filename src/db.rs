use crate::migration::Migrator;
use anyhow::{Context, Result};
use sea_orm::{
    ConnectOptions, Database, DatabaseConnection, DatabaseTransaction, DbErr,
    SqliteTransactionMode, TransactionOptions, TransactionTrait, sqlx::sqlite::SqliteJournalMode,
};
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
            .busy_timeout(Duration::from_secs(30))
    });

    let database = Database::connect(options)
        .await
        .context("failed to connect to the database")?;
    Migrator::up(&database, None)
        .await
        .context("failed to apply database migrations")?;
    Ok(database)
}

/// Isolate reporting queries from gateway connection acquisition. Open only after migrations.
pub async fn reporting_connection(
    database_url: &str,
    primary: &DatabaseConnection,
) -> Result<DatabaseConnection> {
    // A private in-memory database cannot be opened by an independent pool.
    if primary
        .get_sqlite_connection_pool()
        .connect_options()
        .get_filename()
        == Path::new(":memory:")
    {
        return Ok(primary.clone());
    }
    let mut options = ConnectOptions::new(database_url);
    // The dashboard starts ten independent reports concurrently. Give each
    // report a connection so a pair of expensive queries cannot make the
    // remaining reports wait behind them until the HTTP deadline expires.
    options.max_connections(10);
    options.map_sqlx_sqlite_opts(|options| {
        options
            .read_only(true)
            .pragma("query_only", "ON")
            .busy_timeout(Duration::from_secs(5))
    });
    Database::connect(options)
        .await
        .context("failed to open reporting database")
}

pub async fn begin_immediate(database: &DatabaseConnection) -> Result<WriteTransaction, DbErr> {
    let mut connection = writer(database).await?;
    let started = std::time::Instant::now();
    let transaction = connection
        .inner
        .begin_with_options(TransactionOptions {
            sqlite_transaction_mode: Some(SqliteTransactionMode::Immediate),
            ..Default::default()
        })
        .await?;
    tracing::debug!(
        wait_ms = started.elapsed().as_millis() as u64,
        "SQLite write lock acquired"
    );
    connection._permit.transaction_started = Some(std::time::Instant::now());
    Ok(Guarded {
        inner: transaction,
        _permit: connection._permit,
    })
}

// Every runtime writer uses this gate before acquiring a pooled connection.
// The registry keys identify SQLx pools; cloned DatabaseConnections share their options Arc.
// Separate processes/pools still rely on SQLite's busy timeout.
struct WriterGate {
    queue: std::sync::Arc<tokio::sync::Semaphore>,
    writer: std::sync::Arc<tokio::sync::Semaphore>,
}

struct WriterPermit {
    _gate: std::sync::Arc<WriterGate>,
    _queue: tokio::sync::OwnedSemaphorePermit,
    _writer: tokio::sync::OwnedSemaphorePermit,
    acquired: std::time::Instant,
    transaction_started: Option<std::time::Instant>,
}

impl Drop for WriterPermit {
    fn drop(&mut self) {
        let held_ms = self.acquired.elapsed().as_millis() as u64;
        let transaction_ms = self
            .transaction_started
            .map(|started| started.elapsed().as_millis() as u64);
        tracing::debug!(held_ms, transaction_ms, "database writer released");
        if held_ms >= 1000 {
            tracing::warn!(held_ms, transaction_ms, "slow database writer");
        }
    }
}

pub struct Guarded<C> {
    inner: C,
    _permit: WriterPermit,
}

pub type WriteTransaction = Guarded<DatabaseTransaction>;

impl<C> std::ops::Deref for Guarded<C> {
    type Target = C;
    fn deref(&self) -> &C {
        &self.inner
    }
}

impl WriteTransaction {
    pub async fn commit(self) -> Result<(), DbErr> {
        self.inner.commit().await
    }
    pub async fn rollback(self) -> Result<(), DbErr> {
        self.inner.rollback().await
    }
}

#[async_trait::async_trait]
impl<C: sea_orm::ConnectionTrait + Send> sea_orm::ConnectionTrait for Guarded<C> {
    fn get_database_backend(&self) -> sea_orm::DbBackend {
        self.inner.get_database_backend()
    }
    async fn execute_raw(&self, stmt: sea_orm::Statement) -> Result<sea_orm::ExecResult, DbErr> {
        self.inner.execute_raw(stmt).await
    }
    async fn execute_unprepared(&self, sql: &str) -> Result<sea_orm::ExecResult, DbErr> {
        self.inner.execute_unprepared(sql).await
    }
    async fn query_one_raw(
        &self,
        stmt: sea_orm::Statement,
    ) -> Result<Option<sea_orm::QueryResult>, DbErr> {
        self.inner.query_one_raw(stmt).await
    }
    async fn query_all_raw(
        &self,
        stmt: sea_orm::Statement,
    ) -> Result<Vec<sea_orm::QueryResult>, DbErr> {
        self.inner.query_all_raw(stmt).await
    }
}

pub async fn writer(database: &DatabaseConnection) -> Result<Guarded<DatabaseConnection>, DbErr> {
    use std::sync::{Arc, Mutex, OnceLock, Weak};
    static GATES: OnceLock<Mutex<std::collections::HashMap<usize, Weak<WriterGate>>>> =
        OnceLock::new();
    let options = database.get_sqlite_connection_pool().connect_options();
    let key = Arc::as_ptr(&options) as usize;
    let gate = {
        let mut gates = GATES
            .get_or_init(Default::default)
            .lock()
            .expect("writer registry poisoned");
        gates.retain(|_, gate| gate.strong_count() > 0);
        gates.get(&key).and_then(Weak::upgrade).unwrap_or_else(|| {
            let gate = Arc::new(WriterGate {
                queue: Arc::new(tokio::sync::Semaphore::new(128)),
                writer: Arc::new(tokio::sync::Semaphore::new(1)),
            });
            gates.insert(key, Arc::downgrade(&gate));
            gate
        })
    };
    let queue = gate
        .queue
        .clone()
        .try_acquire_owned()
        .map_err(|_| DbErr::Custom("database writer queue is full".into()))?;
    let started = std::time::Instant::now();
    let permit = tokio::time::timeout(Duration::from_secs(30), gate.writer.clone().acquire_owned())
        .await
        .map_err(|_| DbErr::Custom("database writer queue deadline exceeded".into()))?
        .map_err(|_| DbErr::Custom("database writer queue is closed".into()))?;
    let wait_ms = started.elapsed().as_millis() as u64;
    tracing::debug!(wait_ms, "database writer acquired");
    if wait_ms >= 1000 {
        tracing::warn!(wait_ms, "database writer queue delayed");
    }
    Ok(Guarded {
        inner: database.clone(),
        _permit: WriterPermit {
            _gate: gate,
            _queue: queue,
            _writer: permit,
            acquired: std::time::Instant::now(),
            transaction_started: None,
        },
    })
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use sea_orm::ConnectionTrait;

    pub(crate) struct FileDatabase {
        pub database: DatabaseConnection,
        pub url: String,
        directory: std::path::PathBuf,
    }

    impl FileDatabase {
        pub async fn new() -> Self {
            let directory =
                std::env::temp_dir().join(format!("aegis-db-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&directory).unwrap();
            let url = format!("sqlite://{}?mode=rwc", directory.join("test.db").display());
            let database = connect(&url).await.unwrap();
            Self {
                database,
                url,
                directory,
            }
        }
    }

    impl Drop for FileDatabase {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    #[tokio::test]
    async fn reporting_pool_is_read_only_and_does_not_consume_gateway_connections() {
        let fixture = FileDatabase::new().await;
        let reporting = reporting_connection(&fixture.url, &fixture.database)
            .await
            .unwrap();
        assert!(
            reporting
                .execute_unprepared("DELETE FROM gateway_requests")
                .await
                .is_err()
        );
        let mut readers = Vec::new();
        for _ in 0..10 {
            let reader = reporting.begin().await.unwrap();
            reader
                .execute_unprepared("SELECT COUNT(*) FROM gateway_requests")
                .await
                .unwrap();
            readers.push(reader);
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            let tx = begin_immediate(&fixture.database).await.unwrap();
            tx.execute_unprepared(
                "INSERT INTO background_jobs(name, completed_at) VALUES('pool-test','now')",
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
        })
        .await
        .expect("reporting readers must not block WAL writes or consume the primary pool");
        for reader in readers {
            reader.rollback().await.unwrap();
        }
        reporting.close().await.unwrap();
        fixture.database.close_by_ref().await.unwrap();
    }
    #[tokio::test]
    async fn writer_queue_is_bounded_cancellation_safe_and_does_not_borrow_connections() {
        use futures_util::poll;
        let fixture = FileDatabase::new().await;
        let active = writer(&fixture.database).await.unwrap();
        let idle = fixture.database.get_sqlite_connection_pool().num_idle();
        let mut waiting = Vec::new();
        for _ in 0..127 {
            let mut future = Box::pin(writer(&fixture.database));
            assert!(poll!(&mut future).is_pending());
            waiting.push(future);
        }
        assert_eq!(
            fixture.database.get_sqlite_connection_pool().num_idle(),
            idle
        );
        assert!(writer(&fixture.database).await.is_err());
        // Unrelated reads retain access to the pool while writers are queued.
        tokio::time::timeout(
            Duration::from_secs(1),
            fixture.database.execute_unprepared("SELECT 1"),
        )
        .await
        .unwrap()
        .unwrap();
        drop(waiting);
        drop(active);
        tokio::time::timeout(Duration::from_secs(1), writer(&fixture.database))
            .await
            .unwrap()
            .unwrap();
        let tx = begin_immediate(&fixture.database).await.unwrap();
        tx.rollback().await.unwrap();
        let tx = begin_immediate(&fixture.database).await.unwrap();
        tx.commit().await.unwrap();
        fixture.database.close_by_ref().await.unwrap();
    }
}
