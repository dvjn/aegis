//! Facts-only projection. New generations remain incomplete until a separate backfill validates them.
pub mod source;
pub mod sqlite;
pub mod worker;

use anyhow::Result;

pub const SOURCE_VERSION: i64 = 1;
pub const SCHEMA_VERSION: i64 = 1;
pub const PROJECTION_VERSION: i64 = 1;

#[derive(Clone, Debug)]
pub struct Boundary {
    pub revision: i64,
    pub observed_at: String,
}
#[derive(Clone, Debug)]
pub struct SourceBatch {
    pub source_id: String,
    pub source_fact_version: i64,
    pub boundary: Boundary,
    pub snapshot_revision: i64,
    pub requests: Vec<Replacement>,
    pub tools: Vec<Tool>,
    pub identities: Vec<IdentityKey>,
    pub variants: Vec<Variant>,
    pub keys: Vec<Key>,
}
#[derive(Clone, Debug)]
pub struct Replacement {
    pub request_id: String,
    pub revision: i64,
    pub changed_at: String,
    pub mutation: Mutation,
}
#[derive(Clone, Debug)]
pub enum Mutation {
    Delete,
    Upsert(Box<RequestFacts>),
}
#[derive(Clone, Debug, Default)]
pub struct RequestFacts {
    pub key_id: Option<String>,
    pub key_version_id: Option<String>,
    pub owner_id: Option<String>,
    pub provider: String,
    pub requested_model: Option<String>,
    pub started_at: String,
    pub first_byte_at: Option<String>,
    pub completed_at: Option<String>,
    pub status: Option<i64>,
    pub has_error: bool,
    pub request_bytes: i64,
    pub response_bytes: i64,
    pub client_disconnected: bool,
    pub usage: Option<Usage>,
    pub context: Option<Context>,
    pub contributions: Vec<Contribution>,
    pub appearances: Vec<Appearance>,
    pub attribution: Vec<Attribution>,
}
#[derive(Clone, Debug)]
pub struct Usage {
    pub input_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub cost_nanos: Option<i64>,
    pub cost_source: Option<String>,
}
#[derive(Clone, Debug)]
pub struct Context {
    pub values: [i64; 13],
    pub created_at: String,
}
pub const CONTEXT_COLUMNS: &str = "tool_definition_bytes,system_bytes,user_text_bytes,assistant_text_bytes,thinking_bytes,tool_use_bytes,tool_result_bytes,other_bytes,total_bytes,tools_offered,tools_invoked,tool_result_errors,cache_breakpoints";
#[derive(Clone, Debug)]
pub struct Rational {
    pub numerator: i128,
    pub denominator: i128,
}
/// Exact semantic byte weights. Keep these fractions and authoritative usage
/// cost/context totals together; a reporting layer must allocate cost with
/// checked rational arithmetic, never round these weights into per-request cost.
#[derive(Clone, Debug)]
pub struct Contribution {
    pub tool_id: i64,
    pub definition_count: i64,
    pub definition: Rational,
    pub transmission: Rational,
}
#[derive(Clone, Debug)]
pub struct Appearance {
    pub variant_id: i64,
    pub multiplicity: i64,
}
#[derive(Clone, Debug)]
pub struct Attribution {
    pub identity_id: i64,
    pub state: String,
    pub tool_id: Option<i64>,
}
#[derive(Clone, Debug)]
pub struct Tool {
    pub id: i64,
    pub attribution_key: String,
    pub tool_name: Option<String>,
    pub skill_name: Option<String>,
}
#[derive(Clone, Debug)]
pub struct IdentityKey {
    pub id: i64,
    pub scope_key: String,
    pub provider: String,
    pub identity_kind: String,
    pub identity_key: String,
    pub conversation_available: bool,
}
#[derive(Clone, Debug)]
pub struct Variant {
    pub id: i64,
    pub identity_id: i64,
    pub kind: String,
    pub bytes: i64,
    pub observed_tool_id: i64,
}
#[derive(Clone, Debug)]
pub struct Key {
    pub id: String,
    pub name: String,
    pub owner_id: Option<String>,
}
/// Constructed only by a store after its transaction commits.
#[derive(Debug)]
pub struct CommittedReceipt {
    pub(crate) generation: String,
    pub(crate) source_id: String,
    pub(crate) revisions: Vec<(String, i64)>,
}
impl CommittedReceipt {
    pub fn generation(&self) -> &str {
        &self.generation
    }
    pub fn source_id(&self) -> &str {
        &self.source_id
    }
    pub fn revisions(&self) -> &[(String, i64)] {
        &self.revisions
    }
}
#[async_trait::async_trait]
pub trait ProjectionStore: Send + Sync {
    async fn apply_batch(&self, batch: &SourceBatch) -> Result<CommittedReceipt>;
}

#[cfg(test)]
mod tests {
    use super::sqlite::{SqliteStore, sql};
    use super::*;
    use sea_orm::{ConnectionTrait, DatabaseConnection};
    use std::time::Duration;

    async fn fixture() -> (crate::db::tests::FileDatabase, source::Source, SqliteStore) {
        let fixture = crate::db::tests::FileDatabase::new().await;
        let db = &fixture.database;
        let path = db
            .get_sqlite_connection_pool()
            .connect_options()
            .get_filename()
            .to_path_buf();
        let source = source::Source::open(db.clone(), &path).await.unwrap();
        let store = SqliteStore::open(
            &path,
            &path.with_file_name("analytics.db"),
            &source.source_id,
        )
        .await
        .unwrap();
        source.activate(store.generation()).await.unwrap();
        (fixture, source, store)
    }
    async fn request(db: &DatabaseConnection) {
        db.execute_unprepared("INSERT INTO gateway_requests(id,request_id,provider,protocol,method,endpoint,started_at,request_bytes) VALUES('internal','external','p','anthropic_messages','POST','/messages','2026-01-01',12); INSERT INTO gateway_analytics_requests(request_id) VALUES('internal');").await.unwrap();
    }
    async fn count(db: &DatabaseConnection, table: &str) -> i64 {
        db.query_one_raw(sql(&format!("SELECT COUNT(*) n FROM {table}"), vec![]))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "n")
            .unwrap()
    }
    fn batch(source: &str, revision: i64, mutation: Mutation) -> SourceBatch {
        SourceBatch {
            source_id: source.to_owned(),
            source_fact_version: SOURCE_VERSION,
            boundary: Boundary {
                revision,
                observed_at: "observed".into(),
            },
            snapshot_revision: revision,
            requests: vec![Replacement {
                request_id: "internal".into(),
                revision,
                changed_at: "changed".into(),
                mutation,
            }],
            tools: vec![],
            identities: vec![],
            variants: vec![],
            keys: vec![],
        }
    }
    #[tokio::test]
    async fn commit_without_ack_replays_and_newer_source_revision_survives() {
        let (fixture, source, store) = fixture().await;
        request(&fixture.database).await;
        let boundary = source.observe().await.unwrap();
        fixture
            .database
            .execute_unprepared("UPDATE gateway_requests SET http_status=200 WHERE id='internal'")
            .await
            .unwrap();
        let batch = source
            .batch(&boundary, &source::Limits::default())
            .await
            .unwrap();
        assert!(batch.requests[0].revision > boundary.revision);
        assert_eq!(batch.requests[0].request_id, "internal");
        let receipt = store.apply_batch(&batch).await.unwrap();
        assert_eq!(source.pending().await.unwrap(), 1);
        // Simulate process loss after the destination commit, before acknowledgement.
        let path = fixture
            .database
            .get_sqlite_connection_pool()
            .connect_options()
            .get_filename()
            .to_path_buf();
        drop(store);
        let store = SqliteStore::open(
            &path,
            &path.with_file_name("analytics.db"),
            &source.source_id,
        )
        .await
        .unwrap();
        let replay = store.apply_batch(&batch).await.unwrap();
        assert_eq!(count(&store.database, "requests").await, 1);
        assert_eq!(receipt.revisions(), replay.revisions());
        assert_eq!(receipt.generation(), store.generation());
        assert_eq!(receipt.source_id(), source.source_id);
        fixture
            .database
            .execute_unprepared("UPDATE gateway_requests SET http_status=201 WHERE id='internal'")
            .await
            .unwrap();
        assert_eq!(source.acknowledge(&receipt).await.unwrap(), 0);
        assert_eq!(source.pending().await.unwrap(), 1);
        let fresh = source
            .batch(&boundary, &source::Limits::default())
            .await
            .unwrap();
        let receipt = store.apply_batch(&fresh).await.unwrap();
        assert_eq!(source.acknowledge(&receipt).await.unwrap(), 1);
        assert_eq!(source.pending().await.unwrap(), 0);
    }
    #[tokio::test]
    async fn failed_destination_never_produces_receipt_or_ack() {
        let (fixture, source, store) = fixture().await;
        request(&fixture.database).await;
        let boundary = source.observe().await.unwrap();
        let mut batch = source
            .batch(&boundary, &source::Limits::default())
            .await
            .unwrap();
        if let Mutation::Upsert(f) = &mut batch.requests[0].mutation {
            f.appearances.push(Appearance {
                variant_id: 999,
                multiplicity: 1,
            });
        }
        assert!(store.apply_batch(&batch).await.is_err());
        assert_eq!(source.pending().await.unwrap(), 1);
        assert_eq!(count(&store.database, "requests").await, 0);
        assert_eq!(count(&store.database, "applied_requests").await, 0);
    }
    #[tokio::test]
    async fn wrong_generation_cannot_ack_and_baseline_blocks_publication() {
        let (fixture, source, store) = fixture().await;
        request(&fixture.database).await;
        let boundary = source.observe().await.unwrap();
        let batch = source
            .batch(&boundary, &source::Limits::default())
            .await
            .unwrap();
        let mut receipt = store.apply_batch(&batch).await.unwrap();
        let generation = receipt.generation.clone();
        receipt.generation = "wrong".into();
        assert!(
            source
                .acknowledge(&receipt)
                .await
                .unwrap_err()
                .to_string()
                .contains("no longer active")
        );
        receipt.generation = generation;
        assert!(!source.publish(&store, &boundary).await.unwrap());
        // Only a future validated backfill may set this in production.
        store
            .database
            .execute_unprepared("UPDATE generation SET baseline_complete=1")
            .await
            .unwrap();
        assert!(!source.publish(&store, &boundary).await.unwrap());
        source.acknowledge(&receipt).await.unwrap();
        store
            .database
            .execute_unprepared("UPDATE generation SET baseline_complete=0")
            .await
            .unwrap();
        assert!(!source.publish(&store, &boundary).await.unwrap());
        assert!(store.published().await.unwrap().is_none());
        store
            .database
            .execute_unprepared("UPDATE generation SET baseline_complete=1")
            .await
            .unwrap();
        assert!(source.publish(&store, &boundary).await.unwrap());
        assert_eq!(
            store.published().await.unwrap().unwrap().observed_at,
            boundary.observed_at
        );
        assert!(source.activate("other").await.is_err());
    }
    #[tokio::test]
    async fn tombstone_and_equal_or_older_replay_preserve_latest_state() {
        let (_fixture, source, store) = fixture().await;
        let first = batch(
            &source.source_id,
            2,
            Mutation::Upsert(Box::new(RequestFacts {
                provider: "p".into(),
                started_at: "start".into(),
                usage: Some(Usage {
                    input_tokens: None,
                    cache_read_tokens: Some(0),
                    cache_write_tokens: None,
                    output_tokens: None,
                    reasoning_tokens: None,
                    cost_nanos: Some(0),
                    cost_source: Some("fixture".into()),
                }),
                ..Default::default()
            })),
        );
        store.apply_batch(&first).await.unwrap();
        let row = store
            .database
            .query_one_raw(sql(
                "SELECT input_tokens,cache_read_tokens,cost_nanos FROM usage",
                vec![],
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.try_get::<Option<i64>>("", "input_tokens").unwrap(),
            None
        );
        assert_eq!(
            row.try_get::<Option<i64>>("", "cache_read_tokens").unwrap(),
            Some(0)
        );
        assert_eq!(
            row.try_get::<Option<i64>>("", "cost_nanos").unwrap(),
            Some(0)
        );
        store
            .apply_batch(&batch(&source.source_id, 3, Mutation::Delete))
            .await
            .unwrap();
        store.apply_batch(&first).await.unwrap();
        store
            .apply_batch(&batch(
                &source.source_id,
                3,
                Mutation::Upsert(Box::default()),
            ))
            .await
            .unwrap();
        assert_eq!(count(&store.database, "requests").await, 0);
        assert_eq!(count(&store.database, "usage").await, 0);
        let row = store
            .database
            .query_one_raw(sql("SELECT revision,deleted FROM applied_requests", vec![]))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get::<i64>("", "revision").unwrap(), 3);
        assert!(row.try_get::<bool>("", "deleted").unwrap());
    }
    #[tokio::test]
    async fn binding_lock_readonly_and_alias_checks() {
        let (fixture, source, store) = fixture().await;
        let path = fixture
            .database
            .get_sqlite_connection_pool()
            .connect_options()
            .get_filename()
            .to_path_buf();
        let destination = path.with_file_name("analytics.db");
        assert!(
            SqliteStore::open(&path, &destination, &source.source_id)
                .await
                .is_err()
        );
        assert!(
            SqliteStore::open(&path, &path, &source.source_id)
                .await
                .is_err()
        );
        let reader = store.reader().await.unwrap();
        assert!(
            reader
                .execute_unprepared("DELETE FROM generation")
                .await
                .is_err()
        );
        reader.close().await.unwrap();
        let generation = store.generation().to_owned();
        drop(store);
        assert!(
            SqliteStore::open(&path, &destination, "wrong-source")
                .await
                .is_err()
        );
        let store = SqliteStore::open(&path, &destination, &source.source_id)
            .await
            .unwrap();
        assert_eq!(store.generation(), generation);
        store
            .database
            .execute_unprepared("UPDATE generation SET projection_version=999")
            .await
            .unwrap();
        drop(store);
        assert!(
            SqliteStore::open(&path, &destination, &source.source_id)
                .await
                .is_err()
        );
        #[cfg(unix)]
        {
            let alias = path.with_file_name("alias.db");
            std::os::unix::fs::symlink(&path, &alias).unwrap();
            assert!(sqlite::validate_paths(&path, &alias).is_err());
            std::fs::remove_file(&alias).unwrap();

            let capture_hardlink = path.with_file_name("capture-hardlink.db");
            std::fs::hard_link(&path, &capture_hardlink).unwrap();
            assert!(sqlite::validate_paths(&path, &destination).is_err());
            std::fs::remove_file(&capture_hardlink).unwrap();

            let analytics_hardlink = path.with_file_name("analytics-hardlink.db");
            std::fs::hard_link(&destination, &analytics_hardlink).unwrap();
            assert!(sqlite::validate_paths(&path, &destination).is_err());
            std::fs::remove_file(&analytics_hardlink).unwrap();
        }
    }
    #[tokio::test]
    async fn missing_source_facts_and_oversized_request_block_without_ack() {
        let (fixture, source, store) = fixture().await;
        request(&fixture.database).await;
        let boundary = source.observe().await.unwrap();
        let limits = source::Limits {
            bytes: 1,
            ..Default::default()
        };
        assert!(
            source
                .batch(&boundary, &limits)
                .await
                .unwrap_err()
                .to_string()
                .contains("oversized")
        );
        fixture
            .database
            .execute_unprepared(
                "DELETE FROM gateway_analytics_requests WHERE request_id='internal'",
            )
            .await
            .unwrap();
        assert!(
            source
                .batch(&boundary, &source::Limits::default())
                .await
                .is_err()
        );
        assert_eq!(source.pending().await.unwrap(), 1);
        assert!(!source.publish(&store, &boundary).await.unwrap());
    }
    #[tokio::test]
    async fn dirty_contributions_use_snapshot_attribution_without_repairing_source() {
        let (fixture, source, store) = fixture().await;
        request(&fixture.database).await;
        fixture.database.execute_unprepared(r#"
            INSERT INTO gateway_analytics_tools VALUES(10,'["old",null]','old',NULL),(20,'["new",null]','new',NULL);
            INSERT INTO gateway_analytics_tool_identities VALUES(10,'["request","internal"]','p','call_id','call',0,'resolved',10);
            INSERT INTO gateway_analytics_tool_variants VALUES(10,10,'tool_use',7,10);
            INSERT INTO gateway_analytics_tool_appearances SELECT id,10,2 FROM gateway_analytics_requests WHERE request_id='internal';
            INSERT INTO gateway_analytics_tool_contributions SELECT id,10,0,'0','1','99','1' FROM gateway_analytics_requests WHERE request_id='internal';
        "#).await.unwrap();
        let boundary = source.observe().await.unwrap();
        let initial = source
            .batch(&boundary, &source::Limits::default())
            .await
            .unwrap();
        store.apply_batch(&initial).await.unwrap();
        fixture.database.execute_unprepared("UPDATE gateway_analytics_tool_identities SET tool_id=20 WHERE id=10; UPDATE gateway_analytics_requests SET contributions_dirty=1; UPDATE gateway_requests SET http_status=200 WHERE id='internal';").await.unwrap();
        let changed = source
            .batch(&boundary, &source::Limits::default())
            .await
            .unwrap();
        let Mutation::Upsert(f) = &changed.requests[0].mutation else {
            panic!("expected request");
        };
        assert_eq!(f.contributions.len(), 1);
        assert_eq!(f.contributions[0].tool_id, 20);
        assert_eq!(f.contributions[0].transmission.numerator, 14);
        assert_eq!(f.contributions[0].transmission.denominator, 1);
        let old = store
            .database
            .query_one_raw(sql(
                "SELECT tool_id FROM request_identity_attribution",
                vec![],
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(old.try_get::<i64>("", "tool_id").unwrap(), 10);
        let receipt = store.apply_batch(&changed).await.unwrap();
        source.acknowledge(&receipt).await.unwrap();
        let dirty = fixture
            .database
            .query_one_raw(sql(
                "SELECT contributions_dirty FROM gateway_analytics_requests",
                vec![],
            ))
            .await
            .unwrap()
            .unwrap();
        assert!(dirty.try_get::<bool>("", "contributions_dirty").unwrap());
        let new = store
            .database
            .query_one_raw(sql(
                "SELECT tool_id FROM request_identity_attribution",
                vec![],
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(new.try_get::<i64>("", "tool_id").unwrap(), 20);
        fixture
            .database
            .execute_unprepared(
                "UPDATE gateway_requests SET provider='different' WHERE id='internal'",
            )
            .await
            .unwrap();
        let boundary = source.observe().await.unwrap();
        assert!(
            source
                .batch(&boundary, &source::Limits::default())
                .await
                .unwrap_err()
                .to_string()
                .contains("namespace")
        );
    }
    #[tokio::test]
    async fn source_deletion_exports_a_tombstone() {
        let (fixture, source, store) = fixture().await;
        request(&fixture.database).await;
        let boundary = source.observe().await.unwrap();
        let batch = source
            .batch(&boundary, &source::Limits::default())
            .await
            .unwrap();
        source
            .acknowledge(&store.apply_batch(&batch).await.unwrap())
            .await
            .unwrap();
        fixture
            .database
            .execute_unprepared("DELETE FROM gateway_requests WHERE id='internal'")
            .await
            .unwrap();
        let boundary = source.observe().await.unwrap();
        let batch = source
            .batch(&boundary, &source::Limits::default())
            .await
            .unwrap();
        assert!(matches!(batch.requests[0].mutation, Mutation::Delete));
        source
            .acknowledge(&store.apply_batch(&batch).await.unwrap())
            .await
            .unwrap();
        assert_eq!(count(&store.database, "requests").await, 0);
        assert_eq!(source.pending().await.unwrap(), 0);
    }
    #[tokio::test]
    async fn stale_key_replay_cannot_roll_back_newer_metadata() {
        let (_fixture, source, store) = fixture().await;
        let fresh = SourceBatch {
            source_id: source.source_id.clone(),
            source_fact_version: SOURCE_VERSION,
            boundary: Boundary {
                revision: 5,
                observed_at: "fresh".into(),
            },
            snapshot_revision: 5,
            requests: vec![],
            tools: vec![],
            identities: vec![],
            variants: vec![],
            keys: vec![Key {
                id: "key".into(),
                name: "new name".into(),
                owner_id: Some("new owner".into()),
            }],
        };
        store.apply_batch(&fresh).await.unwrap();
        let stale = SourceBatch {
            boundary: Boundary {
                revision: 4,
                observed_at: "stale".into(),
            },
            snapshot_revision: 4,
            keys: vec![Key {
                id: "key".into(),
                name: "old name".into(),
                owner_id: Some("old owner".into()),
            }],
            ..fresh
        };
        store.apply_batch(&stale).await.unwrap();
        let row = store
            .database
            .query_one_raw(sql(
                "SELECT name,owner_id,source_revision FROM keys",
                vec![],
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get::<String>("", "name").unwrap(), "new name");
        assert_eq!(
            row.try_get::<Option<String>>("", "owner_id")
                .unwrap()
                .as_deref(),
            Some("new owner")
        );
        assert_eq!(row.try_get::<i64>("", "source_revision").unwrap(), 5);
    }
    #[tokio::test]
    async fn filename_options_handle_sqlite_url_characters() {
        let (fixture, source, store) = fixture().await;
        let path = fixture
            .database
            .get_sqlite_connection_pool()
            .connect_options()
            .get_filename()
            .to_path_buf();
        let special = path.with_file_name("analytics ?#.db");
        let special_store = SqliteStore::open(&path, &special, &source.source_id)
            .await
            .unwrap();
        special_store.close().await.unwrap();
        drop(store);
        assert!(special.exists());
    }
    #[tokio::test]
    async fn destination_wait_does_not_block_capture_after_source_snapshot_closes() {
        let (fixture, source, store) = fixture().await;
        request(&fixture.database).await;
        let boundary = source.observe().await.unwrap();
        let batch = source
            .batch(&boundary, &source::Limits::default())
            .await
            .unwrap();
        let held_writer = crate::db::begin_immediate(&store.database).await.unwrap();
        let apply = store.apply_batch(&batch);
        tokio::pin!(apply);
        tokio::select! {
            _ = &mut apply => panic!("projection unexpectedly bypassed the held destination writer"),
            _ = tokio::time::sleep(Duration::from_millis(30)) => {}
        }
        tokio::time::timeout(
            Duration::from_secs(1),
            fixture.database.execute_unprepared(
                "UPDATE gateway_requests SET http_status=200 WHERE id='internal'",
            ),
        )
        .await
        .unwrap()
        .unwrap();
        held_writer.commit().await.unwrap();
        apply.await.unwrap();
    }
    #[tokio::test]
    async fn scheduler_sleeps_after_attempt_and_cancels_without_waiting_interval() {
        let (fixture, source, store) = fixture().await;
        request(&fixture.database).await;
        let worker = worker::Worker::new(
            source,
            store,
            source::Limits::default(),
            Duration::from_secs(300),
            2,
        )
        .await
        .unwrap();
        let mut status = worker.status();
        let cancel = tokio_util::sync::CancellationToken::new();
        let task = tokio::spawn(worker.run(cancel.clone()));
        tokio::time::timeout(Duration::from_secs(5), status.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            status.borrow().availability,
            worker::Availability::Incomplete
        );
        assert_eq!(status.borrow().pending_count, Some(0));
        assert!(status.borrow().oldest_pending_at.is_none());
        assert!(
            !status
                .borrow()
                .oldest_pending_at_unavailable_reason
                .is_empty()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(30), status.changed())
                .await
                .is_err()
        );
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }
    #[tokio::test]
    async fn batch_budget_is_capped_and_reports_backlog() {
        let (fixture, source, store) = fixture().await;
        request(&fixture.database).await;
        fixture.database.execute_unprepared("INSERT INTO gateway_requests(id,request_id,provider,protocol,method,endpoint,started_at,request_bytes) VALUES('internal-two','external-two','p','anthropic_messages','POST','/messages','2026-01-01',12); INSERT INTO gateway_analytics_requests(request_id) VALUES('internal-two');").await.unwrap();
        store
            .database
            .execute_unprepared("UPDATE generation SET baseline_complete=1")
            .await
            .unwrap();
        let worker = worker::Worker::new(
            source,
            store,
            source::Limits {
                request_count: 1,
                ..Default::default()
            },
            Duration::from_secs(1),
            1,
        )
        .await
        .unwrap();
        let mut status = worker.status();
        let cancel = tokio_util::sync::CancellationToken::new();
        let task = tokio::spawn(worker.run(cancel.clone()));
        tokio::time::timeout(Duration::from_secs(5), status.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.borrow().availability, worker::Availability::Backlog);
        assert_eq!(status.borrow().pending_count, Some(1));
        cancel.cancel();
        task.await.unwrap();
    }
    #[tokio::test]
    async fn target_closure_drains_a_dependent_revised_after_the_initial_boundary() {
        let (fixture, source, store) = fixture().await;
        request(&fixture.database).await;
        fixture.database.execute_unprepared("INSERT INTO gateway_requests(id,request_id,provider,protocol,method,endpoint,started_at,request_bytes) VALUES('dependent','dependent-external','p','anthropic_messages','POST','/messages','2026-01-01',12); INSERT INTO gateway_analytics_requests(request_id) VALUES('dependent');").await.unwrap();
        let boundary = source.observe().await.unwrap();
        let initial = source
            .batch(&boundary, &source::Limits::default())
            .await
            .unwrap();
        source
            .acknowledge(&store.apply_batch(&initial).await.unwrap())
            .await
            .unwrap();
        fixture.database.execute_unprepared(&format!("INSERT INTO gateway_analytics_tools VALUES(10,'[\"old\",null]','old',NULL),(20,'[\"new\",null]','new',NULL); INSERT INTO gateway_analytics_tool_identities VALUES(10,'[\"request\",\"internal\"]','p','call_id','shared',0,'resolved',20); INSERT INTO gateway_analytics_tool_variants VALUES(10,10,'tool_use',7,20); INSERT INTO gateway_analytics_tool_appearances SELECT id,10,1 FROM gateway_analytics_requests WHERE request_id='internal'; UPDATE gateway_analytics_clock SET revision=10; UPDATE gateway_analytics_revisions SET revision=10,first_pending_revision={} WHERE request_id='internal'; UPDATE gateway_analytics_revisions SET revision=10,first_pending_revision=10 WHERE request_id='dependent';", boundary.revision)).await.unwrap();
        store
            .database
            .execute_unprepared("UPDATE generation SET baseline_complete=1")
            .await
            .unwrap();
        let worker = worker::Worker::new(
            source,
            store,
            source::Limits::default(),
            Duration::from_secs(1),
            3,
        )
        .await
        .unwrap();
        let mut status = worker::Status::default();
        worker
            .project_from_boundary_for_test(
                boundary,
                &tokio_util::sync::CancellationToken::new(),
                &mut status,
            )
            .await
            .unwrap();
        assert_eq!(status.availability, worker::Availability::Published);
        assert!(status.published.unwrap().revision >= 10);
        assert_eq!(status.pending_count, Some(0));
    }
    #[tokio::test]
    async fn worker_rejects_more_than_the_fixed_batch_budget() {
        let (fixture, source, store) = fixture().await;
        let source_id = source.source_id.clone();
        let path = fixture
            .database
            .get_sqlite_connection_pool()
            .connect_options()
            .get_filename()
            .to_path_buf();
        assert!(
            worker::Worker::new(
                source,
                store,
                source::Limits::default(),
                Duration::from_secs(1),
                worker::MAX_BATCHES_PER_ATTEMPT + 1,
            )
            .await
            .is_err()
        );
        let reopened = SqliteStore::open(&path, &path.with_file_name("analytics.db"), &source_id)
            .await
            .unwrap();
        reopened.close().await.unwrap();
    }
    #[tokio::test]
    async fn destination_refuses_an_alias_created_after_reservation() {
        let (fixture, source, store) = fixture().await;
        let path = fixture
            .database
            .get_sqlite_connection_pool()
            .connect_options()
            .get_filename()
            .to_path_buf();
        let target = path.with_file_name("raced-analytics.db");
        sqlite::reserve_destination(&target).unwrap();
        #[cfg(unix)]
        {
            std::fs::remove_file(&target).unwrap();
            std::os::unix::fs::symlink(&path, &target).unwrap();
            assert!(
                SqliteStore::open(&path, &target, &source.source_id)
                    .await
                    .is_err()
            );
        }
        drop(store);
    }
    #[tokio::test]
    async fn publication_refuses_a_boundary_below_applied_revisions() {
        let (_fixture, source, store) = fixture().await;
        store
            .database
            .execute_unprepared("UPDATE generation SET baseline_complete=1")
            .await
            .unwrap();
        store
            .apply_batch(&batch(
                &source.source_id,
                2,
                Mutation::Upsert(Box::new(RequestFacts {
                    provider: "p".into(),
                    started_at: "start".into(),
                    ..Default::default()
                })),
            ))
            .await
            .unwrap();
        assert!(
            store
                .publish(&Boundary {
                    revision: 1,
                    observed_at: "old".into(),
                })
                .await
                .is_err()
        );
    }
}
