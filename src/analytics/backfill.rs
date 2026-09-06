//! Enumerates requests captured before revision tracking existed and installs
//! their source facts, so a generation's baseline can become publishable.
//!
//! Facts come from the capture preparation path: the stored payload is rebuilt,
//! split, and handed to [`PreparedTools::semantic`], exactly as a live request
//! is. Nothing here derives a fact of its own.
//!
//! Every write goes to the capture database. Enumeration never opens the
//! analytics destination, so it can run beside a serving process and its
//! projection worker.
use super::sqlite::sql;
use crate::{analytics_facts::PreparedTools, db::begin_immediate, telemetry::split_request};
use anyhow::{Context as _, Result, ensure};
use sea_orm::{ConnectionTrait, DatabaseConnection, DatabaseTransaction, TransactionTrait};
use std::time::Duration;

/// How much of history one invocation may cover. Enumeration is resumable, so a
/// budget cuts a run short rather than losing the work it already committed.
#[derive(Clone, Debug)]
pub struct Budget {
    /// Requests per capture transaction, which is what decides how long a page
    /// holds the writer. Installing one request re-dirties the requests sharing
    /// its tool identities, so the cost per request grows with conversation
    /// length: on the production snapshot a page of 8 stays under a second
    /// while a page of 16 runs past it.
    pub page: usize,
    /// Requests this invocation may visit. `None` runs until history is covered.
    pub max_requests: Option<usize>,
    /// Pause between pages, leaving the capture writer to live traffic.
    pub throttle: Duration,
}
impl Default for Budget {
    fn default() -> Self {
        Self {
            page: 8,
            max_requests: None,
            throttle: Duration::from_millis(10),
        }
    }
}
impl Budget {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.page > 0 && self.page <= i64::MAX as usize && self.max_requests != Some(0),
            "analytics backfill page size must be positive and a request budget must be nonzero"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    pub generation: String,
    /// The greatest request ID the frame covers, fixed when the checkpoint was created.
    pub ceiling: String,
    /// The last request ID enumeration examined.
    pub cursor: String,
    pub visited: i64,
    pub installed: i64,
    /// Requests a live revision already covered, so backfill left them alone.
    pub live: i64,
    pub unavailable: i64,
    pub completed_at: Option<String>,
    pub gaps_accepted: bool,
    /// Requests still inside the frame and above the cursor.
    pub remaining: i64,
}
impl Progress {
    /// Enumeration finished and nothing is unaccounted for. A generation whose
    /// baseline is not covered must keep reporting itself incomplete.
    pub fn covered(&self) -> bool {
        self.completed_at.is_some() && (self.unavailable == 0 || self.gaps_accepted)
    }
}

/// A request whose payloads are gone. Recorded rather than installed with empty
/// facts, which would claim the request used no tools and no context.
#[derive(Clone, Debug)]
pub struct Gap {
    pub request_id: String,
    pub reason: String,
    pub detected_at: String,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub async fn progress(database: &DatabaseConnection, generation: &str) -> Result<Option<Progress>> {
    let tx = database.begin().await?;
    let found = read_checkpoint(&tx, generation).await?;
    let progress = match found {
        Some(checkpoint) => Some(measure(&tx, generation, checkpoint).await?),
        None => None,
    };
    tx.commit().await?;
    Ok(progress)
}

pub async fn gaps(database: &DatabaseConnection, generation: &str) -> Result<Vec<Gap>> {
    let rows = database
        .query_all_raw(sql(
            "SELECT request_id,reason,detected_at FROM gateway_analytics_backfill_gaps WHERE generation=? ORDER BY request_id",
            vec![generation.into()],
        ))
        .await?;
    rows.into_iter()
        .map(|row| {
            Ok(Gap {
                request_id: row.try_get("", "request_id")?,
                reason: row.try_get("", "reason")?,
                detected_at: row.try_get("", "detected_at")?,
            })
        })
        .collect()
}

/// True once enumeration has covered the whole frame for this generation.
pub(super) async fn baseline_covered(
    database: &DatabaseConnection,
    generation: &str,
) -> Result<bool> {
    Ok(database
        .query_one_raw(sql(
            "SELECT 1 FROM gateway_analytics_backfill WHERE generation=? AND completed_at IS NOT NULL AND (unavailable=0 OR gaps_accepted=1)",
            vec![generation.into()],
        ))
        .await?
        .is_some())
}

/// Starts or resumes enumeration for `generation`, which must already be
/// registered: an unregistered generation accumulates no queue entries, so the
/// facts installed here would never be offered to a projector.
pub async fn run(
    database: &DatabaseConnection,
    generation: &str,
    budget: &Budget,
    accept_gaps: bool,
) -> Result<Progress> {
    budget.validate()?;
    ensure!(
        database
            .query_one_raw(sql(
                "SELECT 1 FROM gateway_analytics_generations WHERE generation=?",
                vec![generation.into()],
            ))
            .await?
            .is_some(),
        "analytics generation {generation} is not registered; start the server once so it claims the source"
    );
    let mut checkpoint = open_checkpoint(database, generation).await?;
    let mut visited = 0usize;
    while checkpoint.completed_at.is_none() {
        if budget.max_requests.is_some_and(|limit| visited >= limit) {
            break;
        }
        let page = read_page(database, &checkpoint, budget.page).await?;
        let Some(last) = page.last().map(|request| request.id.clone()) else {
            checkpoint = finish(database, generation, accept_gaps).await?;
            break;
        };
        let exhausted = page.len() < budget.page || last == checkpoint.ceiling;
        let mut prepared = Vec::with_capacity(page.len());
        for request in page {
            prepared.push(prepare(database, &request).await?);
        }
        visited += prepared.len();
        checkpoint = install(
            database,
            generation,
            prepared,
            &last,
            exhausted,
            accept_gaps,
        )
        .await?;
        tokio::time::sleep(budget.throttle).await;
    }
    // Acceptance is the operator's current instruction, not a property of the
    // enumeration, so a rerun states it again over an already finished frame.
    if checkpoint.completed_at.is_some() && checkpoint.gaps_accepted != accept_gaps {
        finish(database, generation, accept_gaps).await?;
    }
    progress(database, generation)
        .await?
        .context("analytics backfill checkpoint disappeared")
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Checkpoint {
    ceiling: String,
    cursor: String,
    visited: i64,
    installed: i64,
    live: i64,
    unavailable: i64,
    completed_at: Option<String>,
    gaps_accepted: bool,
}

async fn read_checkpoint(
    database: &impl ConnectionTrait,
    generation: &str,
) -> Result<Option<Checkpoint>> {
    let row = database
        .query_one_raw(sql(
            "SELECT ceiling_request_id,cursor_request_id,visited,installed,live,unavailable,completed_at,gaps_accepted FROM gateway_analytics_backfill WHERE generation=?",
            vec![generation.into()],
        ))
        .await?;
    row.map(|row| {
        Ok(Checkpoint {
            ceiling: row.try_get("", "ceiling_request_id")?,
            cursor: row.try_get("", "cursor_request_id")?,
            visited: row.try_get("", "visited")?,
            installed: row.try_get("", "installed")?,
            live: row.try_get("", "live")?,
            unavailable: row.try_get("", "unavailable")?,
            completed_at: row.try_get("", "completed_at")?,
            gaps_accepted: row.try_get("", "gaps_accepted")?,
        })
    })
    .transpose()
}

async fn measure(
    database: &impl ConnectionTrait,
    generation: &str,
    checkpoint: Checkpoint,
) -> Result<Progress> {
    let remaining: i64 = database
        .query_one_raw(sql(
            "SELECT COUNT(*) n FROM gateway_requests WHERE id>? AND id<=?",
            vec![
                checkpoint.cursor.clone().into(),
                checkpoint.ceiling.clone().into(),
            ],
        ))
        .await?
        .context("missing remaining count")?
        .try_get("", "n")?;
    Ok(Progress {
        generation: generation.to_owned(),
        ceiling: checkpoint.ceiling,
        cursor: checkpoint.cursor,
        visited: checkpoint.visited,
        installed: checkpoint.installed,
        live: checkpoint.live,
        unavailable: checkpoint.unavailable,
        completed_at: checkpoint.completed_at,
        gaps_accepted: checkpoint.gaps_accepted,
        remaining,
    })
}

/// The frame is fixed here and never widened. Requests created afterwards are
/// tracked by the capture triggers, which enqueue them for every registered
/// generation, so enumeration must not chase them.
async fn open_checkpoint(database: &DatabaseConnection, generation: &str) -> Result<Checkpoint> {
    let tx = begin_immediate(database).await?;
    if let Some(checkpoint) = read_checkpoint(&tx, generation).await? {
        tx.commit().await?;
        return Ok(checkpoint);
    }
    let ceiling: Option<String> = tx
        .query_one_raw(sql("SELECT MAX(id) id FROM gateway_requests", vec![]))
        .await?
        .context("missing request ceiling")?
        .try_get("", "id")?;
    let ceiling = ceiling.unwrap_or_default();
    let timestamp = now();
    tx.execute_raw(sql(
        "INSERT INTO gateway_analytics_backfill VALUES(?,?,'',?,?,0,0,0,0,?,0)",
        vec![
            generation.into(),
            ceiling.clone().into(),
            timestamp.clone().into(),
            timestamp.clone().into(),
            // An empty source has nothing to enumerate, so its frame is covered
            // the moment it is fixed.
            ceiling.is_empty().then(|| timestamp.clone()).into(),
        ],
    ))
    .await?;
    let checkpoint = read_checkpoint(&tx, generation)
        .await?
        .context("missing new analytics backfill checkpoint")?;
    tx.commit().await?;
    Ok(checkpoint)
}

struct PageRequest {
    id: String,
    protocol: String,
    request_bytes: i64,
    has_refs: bool,
    chunked: bool,
}

async fn read_page(
    database: &DatabaseConnection,
    checkpoint: &Checkpoint,
    page: usize,
) -> Result<Vec<PageRequest>> {
    let tx = database.begin().await?;
    let rows = tx
        .query_all_raw(sql(
            "SELECT g.id,g.protocol,g.request_bytes,
                EXISTS(SELECT 1 FROM gateway_payload_part_refs r WHERE r.request_id=g.id AND r.direction='request') has_refs,
                EXISTS(SELECT 1 FROM gateway_payload_part_refs r WHERE r.request_id=g.id AND r.direction='request' AND r.path='$bytes') chunked
             FROM gateway_requests g WHERE g.id>? AND g.id<=? ORDER BY g.id LIMIT ?",
            vec![
                checkpoint.cursor.clone().into(),
                checkpoint.ceiling.clone().into(),
                (page as i64).into(),
            ],
        ))
        .await?;
    let page = rows
        .into_iter()
        .map(|row| {
            Ok(PageRequest {
                id: row.try_get("", "id")?,
                protocol: row.try_get("", "protocol")?,
                request_bytes: row.try_get("", "request_bytes")?,
                has_refs: row.try_get("", "has_refs")?,
                chunked: row.try_get("", "chunked")?,
            })
        })
        .collect::<Result<Vec<_>>>();
    tx.commit().await?;
    page
}

enum Prepared {
    /// Empty facts are a fact: an empty or unparsed body used no tools, which is
    /// what live capture stores for one.
    Facts(Box<PreparedTools>),
    Unavailable(String),
}

/// Preparation reads a bounded snapshot and holds no writer, so decoding and
/// parsing history never stalls capture.
async fn prepare(
    database: &DatabaseConnection,
    request: &PageRequest,
) -> Result<(String, Prepared)> {
    let prepared = if !request.has_refs {
        if request.request_bytes == 0 {
            Prepared::Facts(Box::default())
        } else {
            Prepared::Unavailable(format!(
                "no stored request payload for {} captured bytes",
                request.request_bytes
            ))
        }
    } else if request.chunked {
        Prepared::Facts(Box::default())
    } else {
        let tx = database.begin().await?;
        let body = crate::jobs::payload_resplit::original_request_body(
            &tx,
            &request.id,
            &request.protocol,
        )
        .await?;
        tx.commit().await?;
        match body
            .as_deref()
            .and_then(|body| split_request(body, &request.protocol))
        {
            Some(payload) => Prepared::Facts(Box::new(PreparedTools::semantic(&payload)?)),
            None => Prepared::Unavailable(
                "stored request payload did not rebuild into a parsable body".to_owned(),
            ),
        }
    };
    Ok((request.id.clone(), prepared))
}

/// One page, one capture transaction. The checkpoint advances with the facts it
/// describes, so a crash resumes at the last committed page.
async fn install(
    database: &DatabaseConnection,
    generation: &str,
    prepared: Vec<(String, Prepared)>,
    cursor: &str,
    exhausted: bool,
    accept_gaps: bool,
) -> Result<Checkpoint> {
    let visited = prepared.len() as i64;
    let timestamp = now();
    let tx = begin_immediate(database).await?;
    let mut installed = 0i64;
    let mut live = 0i64;
    for (request, prepared) in prepared {
        if covered_by_live_traffic(&tx, &request).await? {
            live += 1;
            continue;
        }
        match prepared {
            Prepared::Facts(tools) => {
                tools.store(&tx, &request).await?;
                crate::request_metrics::rollup(&tx, &request).await?;
                installed += 1;
            }
            Prepared::Unavailable(reason) => {
                tx.execute_raw(sql(
                    "INSERT INTO gateway_analytics_backfill_gaps VALUES(?,?,?,?) ON CONFLICT(generation,request_id) DO UPDATE SET reason=excluded.reason,detected_at=excluded.detected_at",
                    vec![
                        generation.into(),
                        request.into(),
                        reason.into(),
                        timestamp.clone().into(),
                    ],
                ))
                .await?;
            }
        }
    }
    tx.execute_raw(sql(
        "UPDATE gateway_analytics_backfill SET cursor_request_id=?,updated_at=?,visited=visited+?,installed=installed+?,live=live+?,unavailable=(SELECT COUNT(*) FROM gateway_analytics_backfill_gaps WHERE generation=?),completed_at=CASE WHEN ? THEN COALESCE(completed_at,?) ELSE completed_at END,gaps_accepted=? WHERE generation=?",
        vec![
            cursor.into(),
            timestamp.clone().into(),
            visited.into(),
            installed.into(),
            live.into(),
            generation.into(),
            exhausted.into(),
            timestamp.into(),
            accept_gaps.into(),
            generation.into(),
        ],
    ))
    .await?;
    let checkpoint = read_checkpoint(&tx, generation)
        .await?
        .context("missing analytics backfill checkpoint")?;
    tx.commit().await?;
    Ok(checkpoint)
}

/// A newer live revision wins: the request either already carries facts or is
/// queued for one, and backfill data prepared beforehand must not replace it.
/// A request deleted mid-run reaches this the same way, through the tombstone
/// its delete trigger left.
async fn covered_by_live_traffic(tx: &DatabaseTransaction, request: &str) -> Result<bool> {
    Ok(tx
        .query_one_raw(sql(
            "SELECT 1 FROM gateway_analytics_revisions WHERE request_id=? UNION ALL SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM gateway_requests WHERE id=?) LIMIT 1",
            vec![request.into(), request.into()],
        ))
        .await?
        .is_some())
}

/// Reached only when the frame holds no further requests, which is the one
/// proof that enumeration covered all of history.
async fn finish(
    database: &DatabaseConnection,
    generation: &str,
    accept_gaps: bool,
) -> Result<Checkpoint> {
    let tx = begin_immediate(database).await?;
    let timestamp = now();
    tx.execute_raw(sql(
        "UPDATE gateway_analytics_backfill SET cursor_request_id=ceiling_request_id,updated_at=?,completed_at=COALESCE(completed_at,?),gaps_accepted=? WHERE generation=?",
        vec![
            timestamp.clone().into(),
            timestamp.into(),
            accept_gaps.into(),
            generation.into(),
        ],
    ))
    .await?;
    let checkpoint = read_checkpoint(&tx, generation)
        .await?
        .context("missing analytics backfill checkpoint")?;
    tx.commit().await?;
    Ok(checkpoint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        providers::Provider,
        telemetry::{SqliteSink, StartRecord},
    };
    use serde_json::{Value as Json, json};

    const GENERATION: &str = "g";

    async fn capture_world() -> crate::db::tests::FileDatabase {
        let fixture = crate::db::tests::FileDatabase::new().await;
        fixture
            .database
            .execute_unprepared(&format!(
                "INSERT INTO gateway_analytics_generations
                 VALUES('{GENERATION}',0,'2026-01-01T00:00:00.000Z');
                 UPDATE gateway_analytics_clock SET active_generation='{GENERATION}' WHERE id=1;
                 INSERT INTO users(id,email_normalized,email_display,role,status,auth_version,created_at,updated_at)
                 VALUES('u','u@example.com','u@example.com','user','active',0,'2026-01-01','2026-01-01');
                 INSERT INTO gateway_keys(id,user_id,name,allowed_providers,created_at)
                 VALUES('k','u','agent','[]','2026-01-01T00:00:00.000Z')"
            ))
            .await
            .unwrap();
        fixture
    }

    async fn capture(database: &DatabaseConnection, body: &[u8]) -> String {
        SqliteSink::new(database.clone())
            .start(StartRecord {
                request_id: "external",
                key_id: "k",
                key_version_id: "v",
                provider_id: "p",
                provider: Provider::Anthropic,
                method: "POST",
                endpoint: "/messages",
                requested_model: None,
                request_body: body,
            })
            .await
            .unwrap()
            .to_string()
    }

    /// A request as it looks when it was captured before analytics tracking
    /// existed: payloads intact, every trace of a source fact removed. Metrics
    /// go first, because deleting them is itself a tracked mutation.
    async fn historical(database: &DatabaseConnection, body: &[u8]) -> String {
        let request = capture(database, body).await;
        database
            .execute_raw(sql(
                "DELETE FROM gateway_request_metrics WHERE request_id=?",
                vec![request.clone().into()],
            ))
            .await
            .unwrap();
        for statement in [
            "DELETE FROM gateway_analytics_tool_appearances WHERE request_id IN(SELECT id FROM gateway_analytics_requests WHERE request_id=?)",
            "DELETE FROM gateway_analytics_tool_contributions WHERE request_id IN(SELECT id FROM gateway_analytics_requests WHERE request_id=?)",
            "DELETE FROM gateway_analytics_requests WHERE request_id=?",
            "DELETE FROM gateway_analytics_pending_requests WHERE request_id=?",
            "DELETE FROM gateway_analytics_revisions WHERE request_id=?",
        ] {
            database
                .execute_raw(sql(statement, vec![request.clone().into()]))
                .await
                .unwrap();
        }
        request
    }

    fn body(parts: Vec<Json>) -> Vec<u8> {
        serde_json::to_vec(&json!({"messages":[{"role":"user","content":parts}]})).unwrap()
    }
    fn call(id: &str, name: &str) -> Json {
        json!({"type":"tool_use","id":id,"name":name,"input":{}})
    }

    async fn number(database: &DatabaseConnection, query: &str) -> i64 {
        database
            .query_one_raw(sql(query, vec![]))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "n")
            .unwrap()
    }

    async fn queued(database: &DatabaseConnection) -> i64 {
        number(
            database,
            &format!(
                "SELECT COUNT(*) n FROM gateway_analytics_pending_requests WHERE generation='{GENERATION}'"
            ),
        )
        .await
    }

    fn unbounded() -> Budget {
        Budget {
            page: 8,
            max_requests: None,
            throttle: Duration::ZERO,
        }
    }

    #[tokio::test]
    async fn enumerating_history_installs_facts_through_the_capture_preparation_path() {
        let fixture = capture_world().await;
        let db = &fixture.database;
        let request = historical(db, &body(vec![call("c1", "Read")])).await;
        assert_eq!(queued(db).await, 0);

        let progress = run(db, GENERATION, &unbounded(), false).await.unwrap();

        assert_eq!((progress.installed, progress.unavailable), (1, 0));
        assert!(progress.covered());
        assert_eq!(queued(db).await, 1);
        assert_eq!(
            number(
                db,
                "SELECT COUNT(*) n FROM gateway_analytics_tool_contributions c
                 JOIN gateway_analytics_tools t ON t.id=c.tool_id WHERE t.tool_name='Read'"
            )
            .await,
            1
        );
        // Facts arrive through PreparedTools, so the identity is resolved and
        // the request metrics its capture would have written are present.
        assert_eq!(
            number(
                db,
                "SELECT COUNT(*) n FROM gateway_analytics_tool_identities WHERE state='resolved'"
            )
            .await,
            1
        );
        assert!(
            crate::request_metrics::tests::metrics(db, &request)
                .await
                .is_some_and(|metrics| metrics.tool_use_bytes > 0)
        );
    }

    #[tokio::test]
    async fn a_bounded_run_stops_at_its_budget_and_the_next_run_continues() {
        let fixture = capture_world().await;
        let db = &fixture.database;
        for index in 0..5 {
            historical(db, &body(vec![call(&format!("c{index}"), "Read")])).await;
        }
        let budget = Budget {
            page: 2,
            max_requests: Some(2),
            throttle: Duration::ZERO,
        };

        let first = run(db, GENERATION, &budget, false).await.unwrap();
        assert_eq!((first.visited, first.installed), (2, 2));
        assert_eq!(first.remaining, 3);
        assert!(first.completed_at.is_none());
        assert!(!first.covered());

        let second = run(db, GENERATION, &budget, false).await.unwrap();
        assert_eq!((second.visited, second.installed), (4, 4));
        assert!(second.cursor > first.cursor);
        assert!(!second.covered());

        let third = run(db, GENERATION, &unbounded(), false).await.unwrap();
        assert_eq!((third.visited, third.installed), (5, 5));
        assert_eq!(third.remaining, 0);
        assert!(third.covered());
        assert_eq!(queued(db).await, 5);
    }

    #[tokio::test]
    async fn resuming_after_an_interrupted_run_completes_exactly_the_remaining_work() {
        let fixture = capture_world().await;
        let db = &fixture.database;
        for index in 0..6 {
            historical(db, &body(vec![call(&format!("c{index}"), "Read")])).await;
        }
        let interrupted = run(
            db,
            GENERATION,
            &Budget {
                page: 2,
                max_requests: Some(4),
                throttle: Duration::ZERO,
            },
            false,
        )
        .await
        .unwrap();
        assert_eq!(interrupted.installed, 4);

        // The checkpoint is all the next run is given, exactly as after a crash.
        let resumed = run(db, GENERATION, &unbounded(), false).await.unwrap();

        assert_eq!(resumed.visited, 6, "no request is visited twice");
        assert_eq!(resumed.installed, 6, "and none is skipped");
        assert!(resumed.covered());
        assert_eq!(
            number(db, "SELECT COUNT(*) n FROM gateway_analytics_requests").await,
            6
        );
        assert_eq!(
            run(db, GENERATION, &unbounded(), false)
                .await
                .unwrap()
                .visited,
            6,
            "a covered baseline re-enumerates nothing"
        );
    }

    #[tokio::test]
    async fn traffic_arriving_during_backfill_is_tracked_by_capture_and_not_missed() {
        let fixture = capture_world().await;
        let db = &fixture.database;
        for index in 0..4 {
            historical(db, &body(vec![call(&format!("old{index}"), "Read")])).await;
        }
        let bounded = Budget {
            page: 2,
            max_requests: Some(2),
            throttle: Duration::ZERO,
        };
        let partial = run(db, GENERATION, &bounded, false).await.unwrap();
        assert!(partial.completed_at.is_none());

        // Mid-run traffic: one brand new request, and one revision of a request
        // enumeration has not reached yet.
        let arrived = capture(db, &body(vec![call("new", "Write")])).await;
        let ahead: String = db
            .query_one_raw(sql(
                "SELECT id FROM gateway_requests WHERE id>? AND id<=? ORDER BY id LIMIT 1",
                vec![
                    partial.cursor.clone().into(),
                    partial.ceiling.clone().into(),
                ],
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "id")
            .unwrap();
        db.execute_raw(sql(
            "UPDATE gateway_requests SET http_status=200 WHERE id=?",
            vec![ahead.clone().into()],
        ))
        .await
        .unwrap();

        let done = run(db, GENERATION, &unbounded(), false).await.unwrap();

        assert!(done.covered());
        assert_eq!(done.visited, 4, "the arrival is outside the fixed frame");
        assert_eq!(
            done.live, 1,
            "the revised request was left to live tracking"
        );
        assert_eq!(done.installed, 3);
        // Every request, historical or not, now owes the generation work.
        assert_eq!(queued(db).await, 5);
        for request in [&arrived, &ahead] {
            assert_eq!(
                number(
                    db,
                    &format!(
                        "SELECT COUNT(*) n FROM gateway_analytics_pending_requests
                         WHERE generation='{GENERATION}' AND request_id='{request}'"
                    )
                )
                .await,
                1,
                "{request} must still be owed"
            );
        }
    }

    #[tokio::test]
    async fn a_request_whose_payloads_are_gone_is_reported_rather_than_given_empty_facts() {
        let fixture = capture_world().await;
        let db = &fixture.database;
        let lost = historical(db, &body(vec![call("c1", "Read")])).await;
        let empty = historical(db, b"").await;
        db.execute_raw(sql(
            "DELETE FROM gateway_payload_part_refs WHERE request_id=?",
            vec![lost.clone().into()],
        ))
        .await
        .unwrap();

        let progress = run(db, GENERATION, &unbounded(), false).await.unwrap();

        assert!(progress.completed_at.is_some());
        assert_eq!(progress.unavailable, 1);
        assert_eq!(
            progress.installed, 1,
            "an empty captured body still deserves its empty facts"
        );
        assert!(
            !progress.covered(),
            "an unexplained gap must not let the baseline pass as covered"
        );
        let reported = gaps(db, GENERATION).await.unwrap();
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0].request_id, lost);
        assert!(reported[0].reason.contains("no stored request payload"));
        assert_eq!(
            number(
                db,
                &format!(
                    "SELECT COUNT(*) n FROM gateway_analytics_requests WHERE request_id='{lost}'"
                )
            )
            .await,
            0,
            "no fact may claim the lost request used no tools"
        );
        assert_eq!(
            number(
                db,
                &format!(
                    "SELECT COUNT(*) n FROM gateway_analytics_requests WHERE request_id='{empty}'"
                )
            )
            .await,
            1
        );

        let accepted = run(db, GENERATION, &unbounded(), true).await.unwrap();
        assert!(
            accepted.covered(),
            "an operator may accept the gap once it has been reported"
        );
        assert_eq!(accepted.unavailable, 1, "accepting it does not hide it");
    }

    #[tokio::test]
    async fn the_baseline_is_covered_only_when_enumeration_reached_the_whole_frame() {
        let fixture = capture_world().await;
        let db = &fixture.database;
        for index in 0..3 {
            historical(db, &body(vec![call(&format!("c{index}"), "Read")])).await;
        }
        assert!(!baseline_covered(db, GENERATION).await.unwrap());

        let partial = run(
            db,
            GENERATION,
            &Budget {
                page: 1,
                max_requests: Some(1),
                throttle: Duration::ZERO,
            },
            false,
        )
        .await
        .unwrap();
        assert_eq!(partial.installed, 1);
        assert!(
            !baseline_covered(db, GENERATION).await.unwrap(),
            "installing some facts is not a covered baseline"
        );

        run(db, GENERATION, &unbounded(), false).await.unwrap();
        assert!(baseline_covered(db, GENERATION).await.unwrap());
        assert!(
            !baseline_covered(db, "other").await.unwrap(),
            "coverage belongs to the generation that enumerated it"
        );
    }

    #[tokio::test]
    async fn an_unregistered_generation_cannot_enumerate_work_nothing_would_queue() {
        let fixture = capture_world().await;
        let db = &fixture.database;
        historical(db, &body(vec![call("c1", "Read")])).await;
        let error = run(db, "rebuild", &unbounded(), false)
            .await
            .expect_err("an unregistered generation accumulates no queue entries");
        assert!(format!("{error:#}").contains("not registered"));
        assert!(progress(db, "rebuild").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_empty_source_has_a_covered_baseline_without_enumerating_anything() {
        let fixture = capture_world().await;
        let db = &fixture.database;
        let progress = run(db, GENERATION, &unbounded(), false).await.unwrap();
        assert_eq!((progress.visited, progress.installed), (0, 0));
        assert!(progress.covered());
        assert!(baseline_covered(db, GENERATION).await.unwrap());
    }
}
