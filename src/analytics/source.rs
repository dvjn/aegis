//! Reads only compact facts. Every batch owns one bounded, consistent source snapshot.
use super::sqlite::{OwnershipLock, lock_path, sql};
use super::*;
use anyhow::{Context as _, Result, ensure};
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, QueryResult, TransactionTrait, Value,
};
use std::{
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicI64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct Limits {
    pub request_count: usize,
    pub child_rows: usize,
    pub bytes: usize,
    pub snapshot_duration: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            request_count: 32,
            child_rows: 100_000,
            bytes: 16 * 1024 * 1024,
            snapshot_duration: Duration::from_secs(5),
        }
    }
}
impl Limits {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.request_count > 0
                && self.request_count <= i64::MAX as usize
                && self.child_rows > 0
                && self.bytes > 0
                && !self.snapshot_duration.is_zero(),
            "analytics limits must be positive"
        );
        Ok(())
    }
}

/// One consistent read of a generation's queues. A count without its oldest
/// entry, or the reverse, could disagree across two reads of a live queue.
#[derive(Clone, Debug)]
pub struct Pending {
    pub count: i64,
    pub oldest_at: Option<String>,
}

pub struct Source {
    database: DatabaseConnection,
    activation_path: PathBuf,
    /// Zero until this process activates; no clock epoch is ever zero once owned.
    epoch: AtomicI64,
    /// Activation binds this handle to one generation for its lifetime, so queue
    /// reads scope themselves without carrying the name through every call.
    generation: OnceLock<String>,
    pub source_id: String,
}
impl Source {
    pub async fn open(database: DatabaseConnection, source_path: &Path) -> Result<Self> {
        let source_path = source_path.canonicalize()?;
        let pool_path = database
            .get_sqlite_connection_pool()
            .connect_options()
            .get_filename()
            .canonicalize()?;
        ensure!(
            pool_path == source_path,
            "source pool and activation lock path differ"
        );
        let row = database
            .query_one_raw(sql(
                "SELECT source_id FROM gateway_analytics_clock WHERE id=1",
                vec![],
            ))
            .await?
            .context("missing analytics clock")?;
        let source_id: String = row.try_get("", "source_id")?;
        ensure!(
            !source_id.is_empty(),
            "missing durable analytics source identity"
        );
        Ok(Self {
            database,
            activation_path: lock_path(&source_path, ".analytics-activation.lock"),
            epoch: AtomicI64::new(0),
            generation: OnceLock::new(),
            source_id,
        })
    }
    pub fn epoch(&self) -> i64 {
        self.epoch.load(Ordering::SeqCst)
    }
    fn generation(&self) -> Result<&str> {
        Ok(self
            .generation
            .get()
            .context("analytics generation is not activated")?)
    }
    /// Registration is what makes a generation accumulate work: a source
    /// revision fans out only to registered generations, and an unregistered one
    /// stays empty however long it waits. A newly registered generation owes
    /// every recorded fact; re-registering an existing one changes nothing, so
    /// reclaiming ownership cannot resurrect acknowledged work.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "activation registers its own generation; a second one is registered by the rebuild path, which does not exist yet"
        )
    )]
    pub async fn register_generation(&self, generation: &str) -> Result<()> {
        let tx = crate::db::begin_immediate(&self.database).await?;
        register(&tx, generation).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn observe(&self) -> Result<Boundary> {
        let tx = self.database.begin().await?;
        let row=tx.query_one_raw(sql("SELECT revision,strftime('%Y-%m-%dT%H:%M:%fZ','now') observed_at FROM gateway_analytics_clock WHERE id=1",vec![])).await?.context("missing clock")?;
        let boundary = Boundary {
            revision: row.try_get("", "revision")?,
            observed_at: row.try_get("", "observed_at")?,
        };
        tx.commit().await?;
        Ok(boundary)
    }
    /// A generation may claim an unowned source, or reclaim one it already holds,
    /// never steal an existing owner. Every claim opens a new ownership era, so a
    /// reclaim after lost ownership strands the previous era's in-flight work.
    pub async fn activate(&self, generation: &str) -> Result<i64> {
        let _lock = OwnershipLock::acquire(&self.activation_path)?;
        let tx = crate::db::begin_immediate(&self.database).await?;
        tx.execute_raw(sql("UPDATE gateway_analytics_clock SET active_generation=?,epoch=epoch+1 WHERE id=1 AND source_id=? AND (active_generation IS NULL OR active_generation=?)",vec![generation.into(),self.source_id.clone().into(),generation.into()])).await?;
        let row = tx
            .query_one_raw(sql(
                "SELECT active_generation,source_id,epoch FROM gateway_analytics_clock WHERE id=1",
                vec![],
            ))
            .await?
            .context("missing clock")?;
        ensure!(
            row.try_get::<Option<String>>("", "active_generation")?
                .as_deref()
                == Some(generation)
                && row.try_get::<String>("", "source_id")? == self.source_id,
            "another analytics generation is active"
        );
        let epoch = row.try_get::<i64>("", "epoch")?;
        register(&tx, generation).await?;
        tx.commit().await?;
        self.epoch.store(epoch, Ordering::SeqCst);
        let _ = self.generation.set(generation.to_owned());
        Ok(epoch)
    }
    pub async fn acknowledge(&self, receipt: &CommittedReceipt) -> Result<u64> {
        ensure!(
            receipt.source_id() == self.source_id,
            "receipt source mismatch"
        );
        let tx = crate::db::begin_immediate(&self.database).await?;
        let clock = tx
            .query_one_raw(sql(
                "SELECT source_id,active_generation,epoch FROM gateway_analytics_clock WHERE id=1",
                vec![],
            ))
            .await?
            .context("missing clock")?;
        ensure!(
            clock.try_get::<String>("", "source_id")? == receipt.source_id()
                && clock
                    .try_get::<Option<String>>("", "active_generation")?
                    .as_deref()
                    == Some(receipt.generation()),
            "analytics generation is no longer active; stopping projection attempt"
        );
        ensure!(
            clock.try_get::<i64>("", "epoch")? == receipt.epoch(),
            "analytics ownership era changed; stopping projection attempt"
        );
        // Draining only this generation's entries leaves a rebuilding generation
        // owing the same work. The revision still has to match the current fact,
        // so a mutation racing the batch is not acknowledged away.
        let generation = receipt.generation();
        let mut n = 0;
        for (request, revision) in receipt.revisions() {
            n += tx
                .execute_raw(sql(
                    "DELETE FROM gateway_analytics_pending_requests WHERE generation=? AND request_id=? AND EXISTS(SELECT 1 FROM gateway_analytics_revisions r WHERE r.request_id=gateway_analytics_pending_requests.request_id AND r.revision=?)",
                    vec![generation.into(), request.clone().into(), (*revision).into()],
                ))
                .await?
                .rows_affected();
        }
        for (key, revision) in receipt.key_revisions() {
            n += tx
                .execute_raw(sql(
                    "DELETE FROM gateway_analytics_pending_keys WHERE generation=? AND key_id=? AND EXISTS(SELECT 1 FROM gateway_analytics_key_revisions r WHERE r.key_id=gateway_analytics_pending_keys.key_id AND r.revision=?)",
                    vec![generation.into(), key.clone().into(), (*revision).into()],
                ))
                .await?
                .rows_affected();
        }
        tx.commit().await?;
        Ok(n)
    }
    pub async fn pending(&self) -> Result<Pending> {
        let generation = self.generation()?;
        let row=self.database.query_one_raw(sql("SELECT COUNT(*) n,MIN(first_pending_at) oldest FROM (SELECT first_pending_at FROM gateway_analytics_pending_requests WHERE generation=? UNION ALL SELECT first_pending_at FROM gateway_analytics_pending_keys WHERE generation=?)",vec![generation.into(),generation.into()])).await?.context("missing count")?;
        Ok(Pending {
            count: row.try_get("", "n")?,
            oldest_at: row.try_get("", "oldest")?,
        })
    }
    /// Activation ownership excludes generation switching. Once no work through
    /// B remains, ordinary source mutations receive revisions above B, so the
    /// source snapshot can close before touching the analytics writer.
    pub async fn publish(&self, store: &sqlite::SqliteStore, boundary: &Boundary) -> Result<bool> {
        let _lock = OwnershipLock::acquire(&self.activation_path)?;
        let tx = self.database.begin().await?;
        let row=tx.query_one_raw(sql("SELECT source_id,active_generation,revision,epoch FROM gateway_analytics_clock WHERE id=1",vec![])).await?.context("missing clock")?;
        ensure!(
            row.try_get::<String>("", "source_id")? == store.source_id
                && row
                    .try_get::<Option<String>>("", "active_generation")?
                    .as_deref()
                    == Some(store.generation()),
            "inactive analytics generation"
        );
        ensure!(
            row.try_get::<i64>("", "epoch")? == self.epoch(),
            "analytics ownership era changed; refusing publication"
        );
        ensure!(
            boundary.revision <= row.try_get::<i64>("", "revision")?,
            "future publication boundary"
        );
        let generation = store.generation();
        let pending=tx.query_one_raw(sql("SELECT 1 n FROM gateway_analytics_pending_requests WHERE generation=? AND first_pending_revision<=? UNION ALL SELECT 1 FROM gateway_analytics_pending_keys WHERE generation=? AND first_pending_revision<=? LIMIT 1",vec![generation.into(),boundary.revision.into(),generation.into(),boundary.revision.into()])).await?.is_some();
        tx.commit().await?;
        let complete = !pending && store.baseline_complete().await?;
        if complete {
            store.publish(boundary).await?;
        }
        Ok(complete)
    }
    pub async fn batch(&self, boundary: &Boundary, limits: &Limits) -> Result<SourceBatch> {
        limits.validate()?;
        // Timeout drops the read transaction, causing rollback. Destination work
        // cannot begin until this future returns and the snapshot has closed.
        tokio::time::timeout(limits.snapshot_duration, self.read_batch(boundary, limits))
            .await
            .context("analytics source snapshot deadline exceeded")?
    }
    async fn read_batch(&self, boundary: &Boundary, limits: &Limits) -> Result<SourceBatch> {
        let started = Instant::now();
        let tx = self.database.begin().await?;
        let clock = tx
            .query_one_raw(sql(
                "SELECT source_id,revision FROM gateway_analytics_clock WHERE id=1",
                vec![],
            ))
            .await?
            .context("missing clock")?;
        ensure!(
            clock.try_get::<String>("", "source_id")? == self.source_id,
            "source binding changed"
        );
        let snapshot_revision = clock.try_get("", "revision")?;
        ensure!(
            boundary.revision <= snapshot_revision,
            "boundary exceeds source snapshot"
        );
        let generation = self.generation()?;
        // Deliberately no current-revision <= boundary predicate.
        let rows=tx.query_all_raw(sql("SELECT r.request_id,r.revision,r.changed_at,r.deleted FROM gateway_analytics_pending_requests p JOIN gateway_analytics_revisions r ON r.request_id=p.request_id WHERE p.generation=? AND p.first_pending_revision<=? ORDER BY p.first_pending_revision,p.request_id LIMIT ?",vec![generation.into(),boundary.revision.into(),(limits.request_count as i64).into()])).await?;
        let mut batch = SourceBatch {
            source_id: self.source_id.clone(),
            source_fact_version: SOURCE_VERSION,
            epoch: self.epoch(),
            boundary: boundary.clone(),
            snapshot_revision,
            requests: vec![],
            tools: vec![],
            identities: vec![],
            variants: vec![],
            keys: vec![],
            key_revisions: vec![],
            quarantined: vec![],
        };
        // A key deleted while its correction was pending has nothing left to
        // relabel, so the queue entry is drained without touching the dimension.
        for row in tx.query_all_raw(sql("SELECT r.key_id,r.revision,k.name,k.user_id FROM gateway_analytics_pending_keys p JOIN gateway_analytics_key_revisions r ON r.key_id=p.key_id LEFT JOIN gateway_keys k ON k.id=p.key_id WHERE p.generation=? AND p.first_pending_revision<=? ORDER BY p.first_pending_revision,p.key_id LIMIT ?",vec![generation.into(),boundary.revision.into(),(limits.request_count as i64).into()])).await? {
            let id: String = row.try_get("", "key_id")?;
            let revision: i64 = row.try_get("", "revision")?;
            if let Some(name) = row.try_get::<Option<String>>("", "name")? {
                batch.keys.push(Key {
                    id: id.clone(),
                    name,
                    owner_id: row.try_get("", "user_id")?,
                    source_revision: revision,
                });
            }
            batch.key_revisions.push((id, revision));
        }
        let mut used_rows = 0usize;
        let mut used_bytes = 0usize;
        for row in rows {
            let request: String = row.try_get("", "request_id")?;
            let deleted: bool = row.try_get("", "deleted")?;
            let revision = row.try_get("", "revision")?;
            let changed_at: String = row.try_get("", "changed_at")?;
            let (children, bytes) = preflight(&tx, &request, deleted).await?;
            let bytes = bytes
                .checked_add(request.len() + changed_at.len() + 128)
                .context("batch byte overflow")?;
            ensure!(
                children <= limits.child_rows && bytes <= limits.bytes,
                "oversized analytics request {request}: {children} child rows, {bytes} decoded bytes"
            );
            if used_rows
                .checked_add(children)
                .context("batch row overflow")?
                > limits.child_rows
                || used_bytes
                    .checked_add(bytes)
                    .context("batch byte overflow")?
                    > limits.bytes
            {
                break;
            }
            used_rows += children;
            used_bytes += bytes;
            let mutation = if deleted {
                Mutation::Delete
            } else {
                let dictionaries = (
                    batch.tools.len(),
                    batch.identities.len(),
                    batch.variants.len(),
                    batch.keys.len(),
                );
                match read_request(&tx, &request, &mut batch).await? {
                    RequestRead::Facts(facts) => Mutation::Upsert(facts),
                    RequestRead::Rescoped(reason) => {
                        // Nothing this request alone introduced may reach the
                        // destination, including its dictionary rows.
                        batch.tools.truncate(dictionaries.0);
                        batch.identities.truncate(dictionaries.1);
                        batch.variants.truncate(dictionaries.2);
                        batch.keys.truncate(dictionaries.3);
                        batch.quarantined.push(Quarantine {
                            request_id: request,
                            revision,
                            reason,
                        });
                        continue;
                    }
                }
            };
            batch.requests.push(Replacement {
                request_id: request,
                revision,
                changed_at,
                mutation,
            });
            ensure!(
                started.elapsed() < limits.snapshot_duration,
                "analytics snapshot duration exceeded"
            );
        }
        // Duplicate dictionaries are cheap to eliminate and cannot change within a snapshot.
        batch.tools.sort_by_key(|v| v.id);
        batch.tools.dedup_by_key(|v| v.id);
        batch.identities.sort_by_key(|v| v.id);
        batch.identities.dedup_by_key(|v| v.id);
        batch.variants.sort_by_key(|v| v.id);
        batch.variants.dedup_by_key(|v| v.id);
        // Highest revision first, so deduplication keeps the newest observation
        // when a queued correction and a request read see the same key.
        batch.keys.sort_by(|a, b| {
            a.id.cmp(&b.id)
                .then(b.source_revision.cmp(&a.source_revision))
        });
        batch.keys.dedup_by(|a, b| a.id == b.id);
        tx.commit().await?;
        Ok(batch)
    }
}

/// Seeding runs only for a generation registered for the first time, so it never
/// competes with the fan-out that keeps an established queue current.
async fn register(tx: &DatabaseTransaction, generation: &str) -> Result<()> {
    ensure!(!generation.is_empty(), "empty analytics generation");
    let registered=tx.execute_raw(sql("INSERT INTO gateway_analytics_generations(generation,registered_revision,registered_at) SELECT ?,revision,strftime('%Y-%m-%dT%H:%M:%fZ','now') FROM gateway_analytics_clock WHERE id=1 ON CONFLICT(generation) DO NOTHING",vec![generation.into()])).await?.rows_affected();
    if registered == 0 {
        return Ok(());
    }
    tx.execute_raw(sql("INSERT INTO gateway_analytics_pending_requests(generation,request_id,first_pending_revision,first_pending_at) SELECT ?,request_id,revision,strftime('%Y-%m-%dT%H:%M:%fZ','now') FROM gateway_analytics_revisions",vec![generation.into()])).await?;
    tx.execute_raw(sql("INSERT INTO gateway_analytics_pending_keys(generation,key_id,first_pending_revision,first_pending_at) SELECT ?,key_id,revision,strftime('%Y-%m-%dT%H:%M:%fZ','now') FROM gateway_analytics_key_revisions",vec![generation.into()])).await?;
    Ok(())
}

// SQL counts and UTF-8 byte lengths precede materializing any request-owned
// child data. Fixed allowances cover integer values, row/DTO overhead, and the
// effective reader's intermediate maps. Strings are counted repeatedly where
// joins/read_contributions duplicate them, intentionally conservatively.
async fn preflight(
    tx: &DatabaseTransaction,
    request: &str,
    deleted: bool,
) -> Result<(usize, usize)> {
    if deleted {
        return Ok((0, 0));
    }
    let queries = [
        "SELECT 1 n, 2048+COALESCE(length(CAST(r.key_id AS BLOB)),0)+COALESCE(length(CAST(r.key_version_id AS BLOB)),0)+length(CAST(r.provider AS BLOB))+COALESCE(length(CAST(r.requested_model AS BLOB)),0)+length(CAST(r.started_at AS BLOB))+COALESCE(length(CAST(r.first_byte_at AS BLOB)),0)+COALESCE(length(CAST(r.completed_at AS BLOB)),0)+COALESCE(length(CAST(k.name AS BLOB)),0)+2*COALESCE(length(CAST(k.user_id AS BLOB)),0) bytes FROM gateway_requests r LEFT JOIN gateway_keys k ON k.id=r.key_id WHERE r.id=?",
        "SELECT COUNT(*) n,COALESCE(SUM(1024+COALESCE(length(CAST(cost_source AS BLOB)),0)),0) bytes FROM gateway_usage WHERE request_id=?",
        "SELECT COUNT(*) n,COALESCE(SUM(1024+length(CAST(created_at AS BLOB))),0) bytes FROM gateway_request_metrics WHERE request_id=?",
        "SELECT COUNT(*) n,COALESCE(SUM(2048+8*(length(CAST(t.attribution_key AS BLOB))+COALESCE(length(CAST(t.tool_name AS BLOB)),0)+COALESCE(length(CAST(t.skill_name AS BLOB)),0))+length(CAST(c.definition_num AS BLOB))+length(CAST(c.definition_den AS BLOB))+length(CAST(c.transmission_num AS BLOB))+length(CAST(c.transmission_den AS BLOB))),0) bytes FROM gateway_analytics_tool_contributions c JOIN gateway_analytics_requests r ON r.id=c.request_id JOIN gateway_analytics_tools t ON t.id=c.tool_id WHERE r.request_id=?",
        "SELECT COUNT(*)*5 n,COALESCE(SUM(4096+8*(length(CAST(i.scope_key AS BLOB))+length(CAST(i.provider AS BLOB))+length(CAST(i.identity_kind AS BLOB))+length(CAST(i.identity_key AS BLOB))+length(CAST(v.kind AS BLOB))+length(CAST(t.attribution_key AS BLOB))+COALESCE(length(CAST(t.tool_name AS BLOB)),0)+COALESCE(length(CAST(t.skill_name AS BLOB)),0)+COALESCE(length(CAST(rt.attribution_key AS BLOB)),0)+COALESCE(length(CAST(rt.tool_name AS BLOB)),0)+COALESCE(length(CAST(rt.skill_name AS BLOB)),0))),0) bytes FROM gateway_analytics_tool_appearances a JOIN gateway_analytics_requests r ON r.id=a.request_id JOIN gateway_analytics_tool_variants v ON v.id=a.variant_id JOIN gateway_analytics_tool_identities i ON i.id=v.identity_id JOIN gateway_analytics_tools t ON t.id=v.observed_tool_id LEFT JOIN gateway_analytics_tools rt ON rt.id=i.tool_id WHERE r.request_id=?",
    ];
    let mut rows = 0usize;
    let mut bytes = 0usize;
    for q in queries {
        if let Some(r) = tx.query_one_raw(sql(q, vec![request.into()])).await? {
            rows = rows
                .checked_add(usize::try_from(r.try_get::<i64>("", "n")?)?)
                .context("row count overflow")?;
            bytes = bytes
                .checked_add(usize::try_from(r.try_get::<i64>("", "bytes")?)?)
                .context("decoded byte count overflow")?;
        }
    }
    Ok((rows, bytes))
}
async fn one(tx: &DatabaseTransaction, q: &str, values: Vec<Value>) -> Result<QueryResult> {
    tx.query_one_raw(sql(q, values))
        .await?
        .context("missing compact source fact; baseline incomplete")
}
async fn tool(tx: &DatabaseTransaction, id: i64, batch: &mut SourceBatch) -> Result<()> {
    let r = one(
        tx,
        "SELECT id,attribution_key,tool_name,skill_name FROM gateway_analytics_tools WHERE id=?",
        vec![id.into()],
    )
    .await?;
    batch.tools.push(Tool {
        id,
        attribution_key: r.try_get("", "attribution_key")?,
        tool_name: r.try_get("", "tool_name")?,
        skill_name: r.try_get("", "skill_name")?,
    });
    Ok(())
}
enum RequestRead {
    Facts(Box<RequestFacts>),
    Rescoped(String),
}
async fn read_request(
    tx: &DatabaseTransaction,
    request: &str,
    batch: &mut SourceBatch,
) -> Result<RequestRead> {
    let r=one(tx,"SELECT r.key_id,r.key_version_id,k.user_id owner_id,k.name key_name,COALESCE(kr.revision,0) key_revision,r.provider,r.requested_model,r.started_at,r.first_byte_at,r.completed_at,r.http_status,r.error_message IS NOT NULL has_error,r.request_bytes,r.response_bytes,r.client_disconnected FROM gateway_requests r LEFT JOIN gateway_keys k ON k.id=r.key_id LEFT JOIN gateway_analytics_key_revisions kr ON kr.key_id=r.key_id WHERE r.id=?",vec![request.into()]).await?;
    let mut f = RequestFacts {
        key_id: r.try_get("", "key_id")?,
        key_version_id: r.try_get("", "key_version_id")?,
        owner_id: r.try_get("", "owner_id")?,
        provider: r.try_get("", "provider")?,
        requested_model: r.try_get("", "requested_model")?,
        started_at: r.try_get("", "started_at")?,
        first_byte_at: r.try_get("", "first_byte_at")?,
        completed_at: r.try_get("", "completed_at")?,
        status: r.try_get("", "http_status")?,
        has_error: r.try_get("", "has_error")?,
        request_bytes: r.try_get("", "request_bytes")?,
        response_bytes: r.try_get("", "response_bytes")?,
        client_disconnected: r.try_get("", "client_disconnected")?,
        ..Default::default()
    };
    if let (Some(id), Some(name)) = (&f.key_id, r.try_get::<Option<String>>("", "key_name")?) {
        batch.keys.push(Key {
            id: id.clone(),
            name,
            owner_id: f.owner_id.clone(),
            source_revision: r.try_get("", "key_revision")?,
        });
    }
    if let Some(u)=tx.query_one_raw(sql("SELECT input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,reasoning_tokens,cost_nanodollars,cost_source FROM gateway_usage WHERE request_id=?",vec![request.into()])).await? {f.usage=Some(Usage {input_tokens:u.try_get("","input_tokens")?,cache_read_tokens:u.try_get("","cache_read_tokens")?,cache_write_tokens:u.try_get("","cache_write_tokens")?,output_tokens:u.try_get("","output_tokens")?,reasoning_tokens:u.try_get("","reasoning_tokens")?,cost_nanos:u.try_get("","cost_nanodollars")?,cost_source:u.try_get("","cost_source")?});}
    if let Some(c)=tx.query_one_raw(sql(&format!("SELECT {CONTEXT_COLUMNS},created_at FROM gateway_request_metrics WHERE request_id=?"),vec![request.into()])).await? {let mut values=[0;13];for (n,column) in CONTEXT_COLUMNS.split(',').enumerate(){values[n]=c.try_get("",column)?;}f.context=Some(Context {values,created_at:c.try_get("","created_at")?});}
    let metadata = one(
        tx,
        "SELECT id,fact_version FROM gateway_analytics_requests WHERE request_id=?",
        vec![request.into()],
    )
    .await?;
    ensure!(
        metadata.try_get::<i64>("", "fact_version")? == SOURCE_VERSION,
        "unsupported source fact version for {request}"
    );
    let id: i64 = metadata.try_get("", "id")?;
    for c in crate::analytics_facts::snapshot_contributions(tx, id).await? {
        let attribution_key = serde_json::to_string(&(c.tool_name, c.skill_name))?;
        let t = one(
            tx,
            "SELECT id FROM gateway_analytics_tools WHERE attribution_key=?",
            vec![attribution_key.into()],
        )
        .await?;
        let tool_id = t.try_get("", "id")?;
        tool(tx, tool_id, batch).await?;
        f.contributions.push(Contribution {
            tool_id,
            definition_count: c.definition_count,
            definition: Rational {
                numerator: c.definition_weight.num,
                denominator: c.definition_weight.den,
            },
            transmission: Rational {
                numerator: c.transmission_weight.num,
                denominator: c.transmission_weight.den,
            },
        });
    }
    for a in tx.query_all_raw(sql("SELECT a.variant_id,a.multiplicity,v.identity_id,v.kind,v.bytes,v.observed_tool_id,i.scope_key,i.provider,i.identity_kind,i.identity_key,i.conversation_available,i.state,i.tool_id FROM gateway_analytics_tool_appearances a JOIN gateway_analytics_tool_variants v ON v.id=a.variant_id JOIN gateway_analytics_tool_identities i ON i.id=v.identity_id WHERE a.request_id=?",vec![id.into()])).await? {
        let expected_scope=serde_json::to_string(&match &f.owner_id {Some(owner)=>("owner",owner.as_str()),None=>("request",request)})?;
        if a.try_get::<String>("", "scope_key")? != expected_scope
            || a.try_get::<String>("", "provider")? != f.provider
        {
            return Ok(RequestRead::Rescoped(format!(
                "analytics identity namespace differs from request {request}; source correction required"
            )));
        }
        let variant_id=a.try_get("","variant_id")?;let identity_id=a.try_get("","identity_id")?;let observed_tool_id=a.try_get("","observed_tool_id")?;let tool_id:Option<i64>=a.try_get("","tool_id")?;
        tool(tx,observed_tool_id,batch).await?;if let Some(t)=tool_id {tool(tx,t,batch).await?;}
        batch.identities.push(IdentityKey {id:identity_id,scope_key:a.try_get("","scope_key")?,provider:a.try_get("","provider")?,identity_kind:a.try_get("","identity_kind")?,identity_key:a.try_get("","identity_key")?,conversation_available:a.try_get("","conversation_available")?});
        batch.variants.push(Variant {id:variant_id,identity_id,kind:a.try_get("","kind")?,bytes:a.try_get("","bytes")?,observed_tool_id});
        f.appearances.push(Appearance {variant_id,multiplicity:a.try_get("","multiplicity")?});
        f.attribution.push(Attribution {identity_id,state:a.try_get("","state")?,tool_id});
    }
    f.attribution.sort_by_key(|a| a.identity_id);
    f.attribution.dedup_by_key(|a| a.identity_id);
    Ok(RequestRead::Facts(Box::new(f)))
}
