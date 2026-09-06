use super::{
    Boundary, ProjectionStore, Quarantine,
    source::{Limits, Source},
    sqlite::SqliteStore,
};
use anyhow::{Result, ensure};
use futures_util::FutureExt;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub const MAX_BATCHES_PER_ATTEMPT: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Availability {
    Incomplete,
    Backlog,
    Published,
    Unavailable(String),
}
#[derive(Clone, Debug)]
pub struct Status {
    pub availability: Availability,
    pub published: Option<Boundary>,
    pub pending_count: Option<i64>,
    /// Requests refused this attempt. They remain pending and unprojected.
    pub quarantined: Vec<Quarantine>,
    /// When the oldest work this generation still owes first became pending;
    /// None only when both queues are empty.
    pub oldest_pending_at: Option<String>,
    pub processing_duration: Duration,
    pub last_batch_duration: Duration,
}
impl Default for Status {
    fn default() -> Self {
        Self {
            availability: Availability::Incomplete,
            published: None,
            pending_count: None,
            quarantined: Vec::new(),
            oldest_pending_at: None,
            processing_duration: Duration::ZERO,
            last_batch_duration: Duration::ZERO,
        }
    }
}
pub struct Worker {
    source: Source,
    store: SqliteStore,
    limits: Limits,
    interval: Duration,
    max_batches: usize,
    status: watch::Sender<Status>,
}
impl Worker {
    /// Keep this worker alive for the entire generation lifetime. Its store
    /// owns the cross-process writer lock. Startup errors are nonfatal to capture.
    pub async fn new(
        source: Source,
        store: SqliteStore,
        limits: Limits,
        interval: Duration,
        max_batches: usize,
    ) -> Result<Self> {
        let initialized = async {
            limits.validate()?;
            ensure!(
                !interval.is_zero() && (1..=MAX_BATCHES_PER_ATTEMPT).contains(&max_batches),
                "worker interval must be positive and batch budget must be between 1 and {MAX_BATCHES_PER_ATTEMPT}"
            );
            ensure!(
                source.source_id == store.source_id,
                "worker source binding mismatch"
            );
            source.activate(store.generation()).await
        }
        .await;
        if let Err(error) = initialized {
            store.close().await?;
            return Err(error);
        }
        let (status, _) = watch::channel(Status::default());
        Ok(Self {
            source,
            store,
            limits,
            interval,
            max_batches,
            status,
        })
    }
    pub fn status(&self) -> watch::Receiver<Status> {
        self.status.subscribe()
    }
    /// One mutable owner runs attempts serially. Cancellation never drops an
    /// in-flight source acknowledgement or destination transaction.
    pub async fn run(mut self, cancel: CancellationToken) {
        let result = std::panic::AssertUnwindSafe(async {
            loop {
                if cancel.is_cancelled() {
                    break;
                }
                self.attempt(&cancel).await;
                tokio::select! { _=cancel.cancelled()=>break, _=tokio::time::sleep(self.interval)=>{} }
            }
        })
        .catch_unwind()
        .await;
        if let Err(error) = self.store.close().await {
            tracing::warn!(%error, "failed to close analytics database");
        }
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
    pub async fn attempt(&mut self, cancel: &CancellationToken) {
        let started = Instant::now();
        let mut status = self.status.borrow().clone();
        if let Err(error) = self.project(cancel, &mut status).await {
            status.availability = Availability::Unavailable(format!("{error:#}"));
        }
        status.processing_duration = started.elapsed();
        self.status.send_replace(status);
    }
    async fn project(&self, cancel: &CancellationToken, status: &mut Status) -> Result<()> {
        let boundary = self.source.observe().await?;
        self.project_from_boundary(boundary, cancel, status).await
    }
    #[cfg(test)]
    pub(crate) fn store_for_test(&self) -> &SqliteStore {
        &self.store
    }
    #[cfg(test)]
    pub(crate) async fn project_from_boundary_for_test(
        &self,
        boundary: Boundary,
        cancel: &CancellationToken,
        status: &mut Status,
    ) -> Result<()> {
        self.project_from_boundary(boundary, cancel, status).await
    }
    async fn project_from_boundary(
        &self,
        mut boundary: Boundary,
        cancel: &CancellationToken,
        status: &mut Status,
    ) -> Result<()> {
        let mut drained = false;
        status.quarantined.clear();
        // Backfill records completion in the capture database, which it can
        // reach while this process serves. Adopting it here, before the batches
        // that may drain the last of the queue, lets one attempt both finish the
        // backlog and publish it.
        if !self.store.baseline_complete().await? && self.source.baseline_covered().await? {
            self.store.mark_baseline_complete().await?;
        }
        for _ in 0..self.max_batches {
            if cancel.is_cancelled() {
                break;
            }
            let started = Instant::now();
            let batch = self.source.batch(&boundary, &self.limits).await?;
            for quarantined in &batch.quarantined {
                if !status
                    .quarantined
                    .iter()
                    .any(|seen: &Quarantine| seen.request_id == quarantined.request_id)
                {
                    tracing::warn!(
                        request = quarantined.request_id,
                        revision = quarantined.revision,
                        reason = quarantined.reason,
                        "analytics request quarantined; leaving it pending"
                    );
                    status.quarantined.push(quarantined.clone());
                }
            }
            // Quarantined requests are never drained, so a batch carrying only
            // those has no work left that this attempt can apply.
            if batch.requests.is_empty() && batch.key_revisions.is_empty() {
                drained = true;
                break;
            }
            let receipt = self.store.apply_batch(&batch).await?;
            self.source.acknowledge(&receipt).await?;
            if receipt
                .revisions()
                .iter()
                .any(|(_, revision)| *revision > boundary.revision)
            {
                boundary = self.source.observe().await?;
            }
            status.last_batch_duration = started.elapsed();
            tokio::task::yield_now().await;
        }
        let published = if cancel.is_cancelled() || !drained {
            false
        } else {
            self.source.publish(&self.store, &boundary).await?
        };
        status.published = self.store.published().await?;
        let pending = self.source.pending().await?;
        status.pending_count = Some(pending.count);
        status.oldest_pending_at = pending.oldest_at;
        status.availability = if !self.store.baseline_complete().await? {
            Availability::Incomplete
        } else if published && pending.count == 0 {
            Availability::Published
        } else {
            Availability::Backlog
        };
        Ok(())
    }
}
