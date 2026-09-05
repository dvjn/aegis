use super::*;
use anyhow::{Context as _, Result, bail, ensure};
use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement, Value,
};
use std::{
    fs::{File, OpenOptions},
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};

pub(crate) fn sql(text: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Sqlite, text, values)
}
pub(crate) async fn insert(
    db: &impl ConnectionTrait,
    table: &str,
    values: Vec<Value>,
) -> Result<()> {
    let marks = vec!["?"; values.len()].join(",");
    db.execute_raw(sql(
        &format!("INSERT INTO {table} VALUES ({marks})"),
        values,
    ))
    .await?;
    Ok(())
}

/// Advisory lock files are permanent; unlinking would permit two owners of different inodes.
pub struct OwnershipLock {
    _file: File,
}
impl OwnershipLock {
    pub fn acquire(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        file.try_lock()
            .with_context(|| format!("analytics lock unavailable: {}", path.display()))?;
        Ok(Self { _file: file })
    }
}
pub fn canonical_database_path(path: &Path) -> Result<PathBuf> {
    if path
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        bail!("database symlinks are not supported: {}", path.display());
    }
    if path.exists() {
        return Ok(path.canonicalize()?);
    }
    let name = path
        .file_name()
        .context("database path requires a filename")?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok(parent.canonicalize()?.join(name))
}
pub fn validate_paths(source: &Path, analytics: &Path) -> Result<(PathBuf, PathBuf)> {
    let source = canonical_database_path(source)?;
    let analytics = canonical_database_path(analytics)?;
    ensure!(
        source != analytics,
        "capture and analytics database paths must differ"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let source_metadata = source.metadata()?;
        ensure!(
            source_metadata.nlink() == 1,
            "hard-linked capture database cannot use path-based analytics ownership locks"
        );
        if analytics.exists() {
            let analytics_metadata = analytics.metadata()?;
            ensure!(
                (source_metadata.dev(), source_metadata.ino())
                    != (analytics_metadata.dev(), analytics_metadata.ino()),
                "capture and analytics paths alias the same inode"
            );
            ensure!(
                analytics_metadata.nlink() == 1,
                "hard-linked analytics database cannot use path-based analytics ownership locks"
            );
        }
    }
    Ok((source, analytics))
}
/// Reserve a previously absent destination before opening SQLite. SQLx 0.9 has
/// no descriptor-based or no-follow SQLite open option, so this rejects every
/// observed symlink and verifies identity again before any file-changing pragma.
/// A hostile process that can replace names in this directory after each check
/// remains outside what this driver can prove safe; deployment must keep the
/// analytics directory private to the service account.
pub(crate) fn reserve_destination(path: &Path) -> Result<()> {
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => {
            file.sync_all()?;
            Ok(())
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn lock_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

pub struct SqliteStore {
    pub(crate) database: DatabaseConnection,
    pub(crate) source_id: String,
    pub(crate) generation: String,
    path: PathBuf,
    _ownership: OwnershipLock,
}
impl SqliteStore {
    pub async fn open(source_path: &Path, analytics_path: &Path, source_id: &str) -> Result<Self> {
        ensure!(!source_id.is_empty(), "empty source identity");
        let (_, path) = validate_paths(source_path, analytics_path)?;
        reserve_destination(&path)?;
        let ownership = OwnershipLock::acquire(&lock_path(&path, ".writer.lock"))?;
        let mut options = ConnectOptions::new("sqlite::memory:");
        let filename = path.clone();
        options.max_connections(1).map_sqlx_sqlite_opts(move |_| {
            sea_orm::sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&filename)
                .create_if_missing(false)
                .foreign_keys(true)
                .busy_timeout(Duration::from_secs(5))
        });
        let database = Database::connect(options).await?;
        let initialized: Result<String> = async {
            // Recheck after SQLx opens the filename but before WAL/schema writes.
            validate_paths(source_path, &path)?;
            database
                .execute_unprepared("PRAGMA journal_mode=WAL")
                .await?;
            let tx = crate::db::begin_immediate(&database).await?;
            tx.execute_unprepared(SCHEMA).await?;
            let row = tx
                .query_one_raw(sql("SELECT * FROM generation WHERE singleton=1", vec![]))
                .await?;
            let generation = if let Some(row) = row {
                ensure!(
                    row.try_get::<String>("", "source_id")? == source_id,
                    "analytics source binding mismatch"
                );
                ensure!(
                    row.try_get::<i64>("", "source_version")? == SOURCE_VERSION
                        && row.try_get::<i64>("", "schema_version")? == SCHEMA_VERSION
                        && row.try_get::<i64>("", "projection_version")? == PROJECTION_VERSION,
                    "unsupported analytics versions"
                );
                row.try_get("", "id")?
            } else {
                let id = uuid::Uuid::new_v4().to_string();
                insert(
                    &tx,
                    "generation",
                    vec![
                        1.into(),
                        source_id.into(),
                        id.clone().into(),
                        SOURCE_VERSION.into(),
                        SCHEMA_VERSION.into(),
                        PROJECTION_VERSION.into(),
                        false.into(),
                    ],
                )
                .await?;
                id
            };
            tx.commit().await?;
            Ok(generation)
        }
        .await;
        let generation = match initialized {
            Ok(generation) => generation,
            Err(error) => {
                database.close_by_ref().await?;
                return Err(error);
            }
        };
        Ok(Self {
            database,
            source_id: source_id.to_owned(),
            generation,
            path,
            _ownership: ownership,
        })
    }
    pub fn generation(&self) -> &str {
        &self.generation
    }
    pub async fn close(self) -> Result<()> {
        // Retain the ownership lock until pooled connections have closed.
        self.database.close_by_ref().await?;
        Ok(())
    }
    pub async fn reader(&self) -> Result<DatabaseConnection> {
        let mut options = ConnectOptions::new("sqlite::memory:");
        let filename = self.path.clone();
        options.max_connections(2).map_sqlx_sqlite_opts(move |_| {
            sea_orm::sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&filename)
                .read_only(true)
                .pragma("query_only", "ON")
                .busy_timeout(Duration::from_secs(5))
        });
        Ok(Database::connect(options).await?)
    }
    pub async fn baseline_complete(&self) -> Result<bool> {
        Ok(self
            .database
            .query_one_raw(sql(
                "SELECT baseline_complete FROM generation WHERE singleton=1",
                vec![],
            ))
            .await?
            .context("missing generation")?
            .try_get("", "baseline_complete")?)
    }
    pub async fn published(&self) -> Result<Option<Boundary>> {
        self.database
            .query_one_raw(sql(
                "SELECT revision,observed_at FROM publication WHERE singleton=1",
                vec![],
            ))
            .await?
            .map(|r| {
                Ok(Boundary {
                    revision: r.try_get("", "revision")?,
                    observed_at: r.try_get("", "observed_at")?,
                })
            })
            .transpose()
    }
    pub(crate) async fn publish(&self, boundary: &Boundary) -> Result<()> {
        let tx = crate::db::begin_immediate(&self.database).await?;
        let row = tx
            .query_one_raw(sql(
                "SELECT baseline_complete FROM generation WHERE singleton=1",
                vec![],
            ))
            .await?
            .context("missing generation")?;
        ensure!(
            row.try_get::<bool>("", "baseline_complete")?,
            "baseline incomplete"
        );
        let highest_applied = tx
            .query_one_raw(sql(
                "SELECT MAX(revision) revision FROM applied_requests",
                vec![],
            ))
            .await?
            .context("missing applied revision result")?
            .try_get::<Option<i64>>("", "revision")?
            .unwrap_or(0);
        ensure!(
            boundary.revision >= highest_applied,
            "publication boundary precedes an applied request revision"
        );
        tx.execute_raw(sql("INSERT INTO publication VALUES(1,?,?) ON CONFLICT(singleton) DO UPDATE SET revision=excluded.revision, observed_at=excluded.observed_at WHERE excluded.revision >= publication.revision",vec![boundary.revision.into(),boundary.observed_at.clone().into()])).await?;
        tx.commit().await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl ProjectionStore for SqliteStore {
    async fn apply_batch(&self, batch: &SourceBatch) -> Result<CommittedReceipt> {
        ensure!(
            batch.source_id == self.source_id && batch.source_fact_version == SOURCE_VERSION,
            "unsupported source binding/version"
        );
        ensure!(
            batch.boundary.revision <= batch.snapshot_revision,
            "boundary exceeds snapshot"
        );
        let tx = crate::db::begin_immediate(&self.database).await?;
        for t in &batch.tools {
            immutable(
                &tx,
                "tools",
                t.id,
                vec![
                    t.id.into(),
                    t.attribution_key.clone().into(),
                    t.tool_name.clone().into(),
                    t.skill_name.clone().into(),
                ],
                &["id", "attribution_key", "tool_name", "skill_name"],
            )
            .await?;
        }
        for i in &batch.identities {
            immutable(
                &tx,
                "identity_keys",
                i.id,
                vec![
                    i.id.into(),
                    i.scope_key.clone().into(),
                    i.provider.clone().into(),
                    i.identity_kind.clone().into(),
                    i.identity_key.clone().into(),
                    i.conversation_available.into(),
                ],
                &[
                    "id",
                    "scope_key",
                    "provider",
                    "identity_kind",
                    "identity_key",
                    "conversation_available",
                ],
            )
            .await?;
        }
        for v in &batch.variants {
            immutable(
                &tx,
                "variants",
                v.id,
                vec![
                    v.id.into(),
                    v.identity_id.into(),
                    v.kind.clone().into(),
                    v.bytes.into(),
                    v.observed_tool_id.into(),
                ],
                &["id", "identity_id", "kind", "bytes", "observed_tool_id"],
            )
            .await?;
        }
        for k in &batch.keys {
            tx.execute_raw(sql("INSERT INTO keys VALUES(?,?,?,?) ON CONFLICT(id) DO UPDATE SET name=excluded.name,owner_id=excluded.owner_id,source_revision=excluded.source_revision WHERE excluded.source_revision >= keys.source_revision",vec![k.id.clone().into(),k.name.clone().into(),k.owner_id.clone().into(),batch.snapshot_revision.into()])).await?;
        }
        let mut revisions = Vec::new();
        for r in &batch.requests {
            ensure!(
                r.revision > 0 && r.revision <= batch.snapshot_revision,
                "invalid request revision"
            );
            let old = tx
                .query_one_raw(sql(
                    "SELECT revision FROM applied_requests WHERE request_id=?",
                    vec![r.request_id.clone().into()],
                ))
                .await?;
            if old
                .map(|r| r.try_get::<i64>("", "revision"))
                .transpose()?
                .is_some_and(|v| v >= r.revision)
            {
                revisions.push((r.request_id.clone(), r.revision));
                continue;
            }
            tx.execute_raw(sql(
                "DELETE FROM requests WHERE source_request_id=?",
                vec![r.request_id.clone().into()],
            ))
            .await?;
            if let Mutation::Upsert(f) = &r.mutation {
                tx.execute_raw(sql("INSERT INTO requests(source_request_id,key_id,key_version_id,owner_id,provider,requested_model,started_at,first_byte_at,completed_at,status,has_error,request_bytes,response_bytes,client_disconnected,changed_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",vec![r.request_id.clone().into(),f.key_id.clone().into(),f.key_version_id.clone().into(),f.owner_id.clone().into(),f.provider.clone().into(),f.requested_model.clone().into(),f.started_at.clone().into(),f.first_byte_at.clone().into(),f.completed_at.clone().into(),f.status.into(),f.has_error.into(),f.request_bytes.into(),f.response_bytes.into(),f.client_disconnected.into(),r.changed_at.clone().into()])).await?;
                let id: i64 = tx
                    .query_one_raw(sql(
                        "SELECT id FROM requests WHERE source_request_id=?",
                        vec![r.request_id.clone().into()],
                    ))
                    .await?
                    .context("missing inserted request")?
                    .try_get("", "id")?;
                if let Some(u) = &f.usage {
                    insert(
                        &tx,
                        "usage",
                        vec![
                            id.into(),
                            u.input_tokens.into(),
                            u.cache_read_tokens.into(),
                            u.cache_write_tokens.into(),
                            u.output_tokens.into(),
                            u.reasoning_tokens.into(),
                            u.cost_nanos.into(),
                            u.cost_source.clone().into(),
                        ],
                    )
                    .await?;
                }
                if let Some(c) = &f.context {
                    let mut values = vec![id.into()];
                    values.extend(c.values.iter().map(|n| (*n).into()));
                    values.push(c.created_at.clone().into());
                    insert(&tx, "context", values).await?;
                }
                for c in &f.contributions {
                    ensure!(
                        c.definition.denominator > 0
                            && c.transmission.denominator > 0
                            && c.definition.numerator >= 0
                            && c.transmission.numerator >= 0,
                        "invalid rational weight"
                    );
                    insert(
                        &tx,
                        "contributions",
                        vec![
                            id.into(),
                            c.tool_id.into(),
                            c.definition_count.into(),
                            c.definition.numerator.to_string().into(),
                            c.definition.denominator.to_string().into(),
                            c.transmission.numerator.to_string().into(),
                            c.transmission.denominator.to_string().into(),
                        ],
                    )
                    .await?;
                }
                for a in &f.appearances {
                    insert(
                        &tx,
                        "appearances",
                        vec![id.into(), a.variant_id.into(), a.multiplicity.into()],
                    )
                    .await?;
                }
                for a in &f.attribution {
                    insert(
                        &tx,
                        "request_identity_attribution",
                        vec![
                            id.into(),
                            a.identity_id.into(),
                            a.state.clone().into(),
                            a.tool_id.into(),
                        ],
                    )
                    .await?;
                }
            }
            tx.execute_raw(sql("INSERT INTO applied_requests VALUES(?,?,?) ON CONFLICT(request_id) DO UPDATE SET revision=excluded.revision,deleted=excluded.deleted",vec![r.request_id.clone().into(),r.revision.into(),matches!(r.mutation,Mutation::Delete).into()])).await?;
            revisions.push((r.request_id.clone(), r.revision));
        }
        tx.execute_raw(sql("INSERT INTO worker_progress VALUES(1,?,?) ON CONFLICT(singleton) DO UPDATE SET snapshot_revision=MAX(snapshot_revision,excluded.snapshot_revision),batch_at=excluded.batch_at",vec![batch.snapshot_revision.into(),chrono::Utc::now().to_rfc3339().into()])).await?;
        tx.commit().await?;
        Ok(CommittedReceipt {
            generation: self.generation.clone(),
            source_id: self.source_id.clone(),
            revisions,
        })
    }
}
async fn immutable(
    db: &impl ConnectionTrait,
    table: &str,
    id: i64,
    values: Vec<Value>,
    columns: &[&str],
) -> Result<()> {
    let predicate = columns
        .iter()
        .map(|c| format!("{c} IS ?"))
        .collect::<Vec<_>>()
        .join(" AND ");
    if db
        .query_one_raw(sql(
            &format!("SELECT id FROM {table} WHERE id=?"),
            vec![id.into()],
        ))
        .await?
        .is_some()
    {
        if db
            .query_one_raw(sql(
                &format!("SELECT id FROM {table} WHERE {predicate}"),
                values,
            ))
            .await?
            .is_none()
        {
            bail!("immutable {table} dictionary changed");
        }
    } else {
        insert(db, table, values).await?;
    }
    Ok(())
}
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS generation(singleton INTEGER PRIMARY KEY CHECK(singleton=1),source_id TEXT NOT NULL,id TEXT NOT NULL,source_version INTEGER NOT NULL,schema_version INTEGER NOT NULL,projection_version INTEGER NOT NULL,baseline_complete INTEGER NOT NULL DEFAULT 0 CHECK(baseline_complete IN(0,1)));
CREATE TABLE IF NOT EXISTS publication(singleton INTEGER PRIMARY KEY CHECK(singleton=1),revision INTEGER NOT NULL,observed_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS applied_requests(request_id TEXT PRIMARY KEY,revision INTEGER NOT NULL,deleted INTEGER NOT NULL CHECK(deleted IN(0,1)));
CREATE TABLE IF NOT EXISTS keys(id TEXT PRIMARY KEY,name TEXT NOT NULL,owner_id TEXT,source_revision INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS requests(id INTEGER PRIMARY KEY,source_request_id TEXT NOT NULL UNIQUE,key_id TEXT,key_version_id TEXT,owner_id TEXT,provider TEXT NOT NULL,requested_model TEXT,started_at TEXT NOT NULL,first_byte_at TEXT,completed_at TEXT,status INTEGER,has_error INTEGER NOT NULL,request_bytes INTEGER NOT NULL,response_bytes INTEGER NOT NULL,client_disconnected INTEGER NOT NULL,changed_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS requests_owner_start ON requests(owner_id,started_at);
CREATE TABLE IF NOT EXISTS usage(request_id INTEGER PRIMARY KEY REFERENCES requests(id) ON DELETE CASCADE,input_tokens INTEGER,cache_read_tokens INTEGER,cache_write_tokens INTEGER,output_tokens INTEGER,reasoning_tokens INTEGER,cost_nanos INTEGER,cost_source TEXT);
CREATE TABLE IF NOT EXISTS context(request_id INTEGER PRIMARY KEY REFERENCES requests(id) ON DELETE CASCADE,tool_definition_bytes INTEGER NOT NULL,system_bytes INTEGER NOT NULL,user_text_bytes INTEGER NOT NULL,assistant_text_bytes INTEGER NOT NULL,thinking_bytes INTEGER NOT NULL,tool_use_bytes INTEGER NOT NULL,tool_result_bytes INTEGER NOT NULL,other_bytes INTEGER NOT NULL,total_bytes INTEGER NOT NULL,tools_offered INTEGER NOT NULL,tools_invoked INTEGER NOT NULL,tool_result_errors INTEGER NOT NULL,cache_breakpoints INTEGER NOT NULL,created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tools(id INTEGER PRIMARY KEY,attribution_key TEXT NOT NULL UNIQUE,tool_name TEXT,skill_name TEXT);
CREATE TABLE IF NOT EXISTS identity_keys(id INTEGER PRIMARY KEY,scope_key TEXT NOT NULL,provider TEXT NOT NULL,identity_kind TEXT NOT NULL,identity_key TEXT NOT NULL,conversation_available INTEGER NOT NULL,UNIQUE(scope_key,provider,identity_kind,identity_key));
CREATE TABLE IF NOT EXISTS variants(id INTEGER PRIMARY KEY,identity_id INTEGER NOT NULL REFERENCES identity_keys(id),kind TEXT NOT NULL,bytes INTEGER NOT NULL CHECK(bytes>=0),observed_tool_id INTEGER NOT NULL REFERENCES tools(id));
CREATE TABLE IF NOT EXISTS contributions(request_id INTEGER NOT NULL REFERENCES requests(id) ON DELETE CASCADE,tool_id INTEGER NOT NULL REFERENCES tools(id),definition_count INTEGER NOT NULL CHECK(definition_count>=0),definition_num TEXT NOT NULL,definition_den TEXT NOT NULL,transmission_num TEXT NOT NULL,transmission_den TEXT NOT NULL,PRIMARY KEY(request_id,tool_id)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS contribution_tool_request ON contributions(tool_id,request_id);
CREATE TABLE IF NOT EXISTS appearances(request_id INTEGER NOT NULL REFERENCES requests(id) ON DELETE CASCADE,variant_id INTEGER NOT NULL REFERENCES variants(id),multiplicity INTEGER NOT NULL CHECK(multiplicity>0),PRIMARY KEY(request_id,variant_id)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS appearance_variant_request ON appearances(variant_id,request_id);
CREATE TABLE IF NOT EXISTS request_identity_attribution(request_id INTEGER NOT NULL REFERENCES requests(id) ON DELETE CASCADE,identity_id INTEGER NOT NULL REFERENCES identity_keys(id),state TEXT NOT NULL CHECK(state IN('unresolved','resolved','ambiguous')),tool_id INTEGER REFERENCES tools(id),CHECK((state='resolved')=(tool_id IS NOT NULL)),PRIMARY KEY(request_id,identity_id)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS worker_progress(singleton INTEGER PRIMARY KEY CHECK(singleton=1),snapshot_revision INTEGER NOT NULL,batch_at TEXT NOT NULL);
";
