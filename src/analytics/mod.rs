//! Facts-only projection. New generations remain incomplete until a separate backfill validates them.
pub mod aggregates;
pub mod arithmetic;
pub mod reports;
pub mod source;
pub mod sqlite;
pub mod worker;

use anyhow::Result;

pub const SOURCE_VERSION: i64 = 1;
pub const SCHEMA_VERSION: i64 = 4;
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
    pub epoch: i64,
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
    pub(crate) epoch: i64,
    pub(crate) revisions: Vec<(String, i64)>,
}
impl CommittedReceipt {
    pub fn generation(&self) -> &str {
        &self.generation
    }
    pub fn epoch(&self) -> i64 {
        self.epoch
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
    use crate::usage::{Bucket, Window};
    use chrono::{DateTime, Utc};
    use sea_orm::{ConnectionTrait, DatabaseConnection};
    use std::time::Duration;
    use uuid::Uuid;

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
        db.execute_unprepared("INSERT INTO gateway_requests(id,request_id,provider,protocol,method,endpoint,started_at,request_bytes) VALUES('internal','external','p','anthropic_messages','POST','/messages','2026-01-01T00:00:00.000Z',12); INSERT INTO gateway_analytics_requests(request_id) VALUES('internal');").await.unwrap();
    }
    async fn count(db: &DatabaseConnection, table: &str) -> i64 {
        db.query_one_raw(sql(&format!("SELECT COUNT(*) n FROM {table}"), vec![]))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "n")
            .unwrap()
    }
    fn batch(source: &source::Source, revision: i64, mutation: Mutation) -> SourceBatch {
        SourceBatch {
            source_id: source.source_id.clone(),
            source_fact_version: SOURCE_VERSION,
            epoch: source.epoch(),
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
    async fn reactivating_one_generation_opens_an_era_that_strands_earlier_receipts() {
        let (fixture, source, store) = fixture().await;
        request(&fixture.database).await;
        let boundary = source.observe().await.unwrap();
        let batch = source
            .batch(&boundary, &source::Limits::default())
            .await
            .unwrap();
        let superseded = store.apply_batch(&batch).await.unwrap();
        let previous = source.epoch();
        // A lost lock lets the same generation id claim the source a second time.
        assert_eq!(
            source.activate(store.generation()).await.unwrap(),
            previous + 1
        );
        assert_eq!(source.epoch(), previous + 1);
        assert_eq!(superseded.generation(), store.generation());
        assert_eq!(superseded.epoch(), previous);
        assert!(
            source
                .acknowledge(&superseded)
                .await
                .unwrap_err()
                .to_string()
                .contains("ownership era changed")
        );
        assert_eq!(source.pending().await.unwrap(), 1);
        let current = source
            .batch(&boundary, &source::Limits::default())
            .await
            .unwrap();
        let receipt = store.apply_batch(&current).await.unwrap();
        assert_eq!(receipt.epoch(), previous + 1);
        assert_eq!(source.acknowledge(&receipt).await.unwrap(), 1);
        assert_eq!(source.pending().await.unwrap(), 0);
    }
    #[tokio::test]
    async fn publication_refuses_a_superseded_ownership_era() {
        let (fixture, source, store) = fixture().await;
        let boundary = source.observe().await.unwrap();
        store
            .database
            .execute_unprepared("UPDATE generation SET baseline_complete=1")
            .await
            .unwrap();
        // A competing owner reclaims the same generation without this Source noticing.
        fixture
            .database
            .execute_unprepared("UPDATE gateway_analytics_clock SET epoch=epoch+1 WHERE id=1")
            .await
            .unwrap();
        assert!(
            source
                .publish(&store, &boundary)
                .await
                .unwrap_err()
                .to_string()
                .contains("ownership era changed")
        );
        assert!(store.published().await.unwrap().is_none());
        source.activate(store.generation()).await.unwrap();
        assert!(source.publish(&store, &boundary).await.unwrap());
    }
    #[tokio::test]
    async fn tombstone_and_equal_or_older_replay_preserve_latest_state() {
        let (_fixture, source, store) = fixture().await;
        let first = batch(
            &source,
            2,
            Mutation::Upsert(Box::new(RequestFacts {
                provider: "p".into(),
                started_at: "2026-01-01T00:00:00.000Z".into(),
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
            .apply_batch(&batch(&source, 3, Mutation::Delete))
            .await
            .unwrap();
        store.apply_batch(&first).await.unwrap();
        store
            .apply_batch(&batch(&source, 3, Mutation::Upsert(Box::default())))
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
            epoch: source.epoch(),
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
        fixture.database.execute_unprepared("INSERT INTO gateway_requests(id,request_id,provider,protocol,method,endpoint,started_at,request_bytes) VALUES('internal-two','external-two','p','anthropic_messages','POST','/messages','2026-01-01T00:00:00.000Z',12); INSERT INTO gateway_analytics_requests(request_id) VALUES('internal-two');").await.unwrap();
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
        fixture.database.execute_unprepared("INSERT INTO gateway_requests(id,request_id,provider,protocol,method,endpoint,started_at,request_bytes) VALUES('dependent','dependent-external','p','anthropic_messages','POST','/messages','2026-01-01T00:00:00.000Z',12); INSERT INTO gateway_analytics_requests(request_id) VALUES('dependent');").await.unwrap();
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
    fn report_window(start: &str, end: &str) -> Window {
        Window {
            start: DateTime::parse_from_rfc3339(start)
                .unwrap()
                .with_timezone(&Utc),
            end: DateTime::parse_from_rfc3339(end)
                .unwrap()
                .with_timezone(&Utc),
            bucket: Bucket::ThreeHours,
        }
    }

    fn report_request(
        owner: Uuid,
        started_at: &str,
        tool_id: i64,
        variant_ids: &[i64],
        transmission: i128,
    ) -> RequestFacts {
        RequestFacts {
            key_id: Some("key".into()),
            owner_id: Some(owner.to_string()),
            provider: "provider".into(),
            requested_model: Some("model".into()),
            started_at: started_at.into(),
            status: Some(200),
            usage: Some(Usage {
                input_tokens: Some(1),
                cache_read_tokens: Some(2),
                cache_write_tokens: Some(3),
                output_tokens: Some(4),
                reasoning_tokens: None,
                cost_nanos: Some(100),
                cost_source: Some("fixture".into()),
            }),
            context: Some(Context {
                values: [6, 0, 0, 0, 0, transmission as i64, 0, 0, 100, 1, 1, 0, 0],
                created_at: started_at.into(),
            }),
            contributions: vec![Contribution {
                tool_id,
                definition_count: 1,
                definition: Rational {
                    numerator: 6,
                    denominator: 1,
                },
                transmission: Rational {
                    numerator: transmission,
                    denominator: 1,
                },
            }],
            appearances: variant_ids
                .iter()
                .map(|id| Appearance {
                    variant_id: *id,
                    multiplicity: 1,
                })
                .collect(),
            attribution: vec![Attribution {
                identity_id: 10,
                state: "resolved".into(),
                tool_id: Some(tool_id),
            }],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn snapshot_reports_merge_hours_deduplicate_identity_and_hold_one_read_transaction() {
        let (_fixture, source, store) = fixture().await;
        let owner = Uuid::now_v7();
        let make =
            |revision, request_id: &str, started_at, variants: &[i64], transmission| Replacement {
                request_id: request_id.into(),
                revision,
                changed_at: format!("change-{revision}"),
                mutation: Mutation::Upsert(Box::new(report_request(
                    owner,
                    started_at,
                    1,
                    variants,
                    transmission,
                ))),
            };
        store
            .apply_batch(&SourceBatch {
                source_id: source.source_id.clone(),
                source_fact_version: SOURCE_VERSION,
                epoch: source.epoch(),
                boundary: Boundary {
                    revision: 3,
                    observed_at: "observed".into(),
                },
                snapshot_revision: 3,
                requests: vec![
                    make(1, "boundary", "2026-01-01T10:30:00.000Z", &[1], 10),
                    make(2, "full", "2026-01-01T11:10:00.000Z", &[2, 3], 50),
                    make(3, "endpoint", "2026-01-01T12:00:00.000Z", &[1], 10),
                ],
                tools: vec![Tool {
                    id: 1,
                    attribution_key: "tool".into(),
                    tool_name: Some("mcp__server__read".into()),
                    skill_name: None,
                }],
                identities: vec![IdentityKey {
                    id: 10,
                    scope_key: format!("[\"owner\",\"{owner}\"]"),
                    provider: "provider".into(),
                    identity_kind: "call_id".into(),
                    identity_key: "replayed".into(),
                    conversation_available: true,
                }],
                variants: vec![
                    Variant {
                        id: 1,
                        identity_id: 10,
                        kind: "tool_use".into(),
                        bytes: 10,
                        observed_tool_id: 1,
                    },
                    Variant {
                        id: 2,
                        identity_id: 10,
                        kind: "tool_use".into(),
                        bytes: 20,
                        observed_tool_id: 1,
                    },
                    Variant {
                        id: 3,
                        identity_id: 10,
                        kind: "tool_result".into(),
                        bytes: 30,
                        observed_tool_id: 1,
                    },
                ],
                keys: vec![Key {
                    id: "key".into(),
                    name: "same label".into(),
                    owner_id: Some(owner.to_string()),
                }],
            })
            .await
            .unwrap();
        let window = report_window("2026-01-01T10:30:00Z", "2026-01-01T12:00:00Z");
        // Reports read through the read-only pool. Reading through the writer
        // pool would hold its one connection and starve the projection worker,
        // which the concurrent batch below would then never complete.
        let reader = store.reader().await.unwrap();
        let refusal = reports::Snapshot::begin(&reader, false).await;
        assert!(
            refusal.is_err()
                && refusal
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("baseline is incomplete")
        );
        let snapshot = reports::Snapshot::begin(&reader, true).await.unwrap();
        assert_eq!(snapshot.totals(owner, window).await.unwrap().requests, 3);
        assert_eq!(
            snapshot.by_model(owner, window).await.unwrap()[0].tokens,
            30
        );
        assert_eq!(
            snapshot.by_provider(owner, window).await.unwrap()[0].requests,
            3
        );
        assert_eq!(
            snapshot.by_key(owner, window).await.unwrap()[0]
                .label
                .as_deref(),
            Some("same label")
        );
        assert_eq!(
            snapshot
                .totals_series(owner, window)
                .await
                .unwrap()
                .requests
                .iter()
                .sum::<i64>(),
            3
        );
        let usage = snapshot.tool_usage(owner, window).await.unwrap();
        assert_eq!(usage.tools.len(), 1);
        assert_eq!(usage.tools[0].calls, 1);
        assert_eq!(usage.tools[0].bytes, 68);
        assert_eq!(usage.tools[0].cost_nanodollars, 88);
        assert_eq!(usage.mcp_servers()[0].label, "server");

        store
            .apply_batch(&SourceBatch {
                source_id: source.source_id.clone(),
                source_fact_version: SOURCE_VERSION,
                epoch: source.epoch(),
                boundary: Boundary {
                    revision: 4,
                    observed_at: "later".into(),
                },
                snapshot_revision: 4,
                requests: vec![make(4, "later", "2026-01-01T11:30:00.000Z", &[1], 10)],
                tools: vec![],
                identities: vec![],
                variants: vec![],
                keys: vec![],
            })
            .await
            .unwrap();
        assert_eq!(snapshot.totals(owner, window).await.unwrap().requests, 3);
        snapshot.commit().await.unwrap();
        let later = reports::Snapshot::begin(&reader, true).await.unwrap();
        assert_eq!(later.totals(owner, window).await.unwrap().requests, 4);
        later.commit().await.unwrap();
    }

    #[tokio::test]
    async fn replacement_and_deletion_repair_old_and_new_report_hours() {
        let (_fixture, source, store) = fixture().await;
        let owner = Uuid::now_v7();
        let mut initial = report_request(owner, "2026-01-01T10:10:00.000Z", 1, &[], 0);
        initial.contributions.clear();
        initial.attribution.clear();
        store
            .apply_batch(&SourceBatch {
                source_id: source.source_id.clone(),
                source_fact_version: SOURCE_VERSION,
                epoch: source.epoch(),
                boundary: Boundary {
                    revision: 1,
                    observed_at: "one".into(),
                },
                snapshot_revision: 1,
                requests: vec![Replacement {
                    request_id: "move".into(),
                    revision: 1,
                    changed_at: "one".into(),
                    mutation: Mutation::Upsert(Box::new(initial)),
                }],
                tools: vec![],
                identities: vec![],
                variants: vec![],
                keys: vec![Key {
                    id: "key".into(),
                    name: "key".into(),
                    owner_id: Some(owner.to_string()),
                }],
            })
            .await
            .unwrap();
        let mut moved = report_request(owner, "2026-01-01T12:10:00.000Z", 1, &[], 0);
        moved.contributions.clear();
        moved.attribution.clear();
        store
            .apply_batch(&SourceBatch {
                source_id: source.source_id.clone(),
                source_fact_version: SOURCE_VERSION,
                epoch: source.epoch(),
                boundary: Boundary {
                    revision: 2,
                    observed_at: "two".into(),
                },
                snapshot_revision: 2,
                requests: vec![Replacement {
                    request_id: "move".into(),
                    revision: 2,
                    changed_at: "two".into(),
                    mutation: Mutation::Upsert(Box::new(moved)),
                }],
                tools: vec![],
                identities: vec![],
                variants: vec![],
                keys: vec![],
            })
            .await
            .unwrap();
        let reader = store.reader().await.unwrap();
        let snapshot = reports::Snapshot::begin(&reader, true).await.unwrap();
        assert_eq!(
            snapshot
                .totals(
                    owner,
                    report_window("2026-01-01T10:00:00Z", "2026-01-01T11:00:00Z")
                )
                .await
                .unwrap()
                .requests,
            0
        );
        assert_eq!(
            snapshot
                .totals(
                    owner,
                    report_window("2026-01-01T12:00:00Z", "2026-01-01T13:00:00Z")
                )
                .await
                .unwrap()
                .requests,
            1
        );
        snapshot.commit().await.unwrap();
        store
            .apply_batch(&SourceBatch {
                source_id: source.source_id.clone(),
                source_fact_version: SOURCE_VERSION,
                epoch: source.epoch(),
                boundary: Boundary {
                    revision: 3,
                    observed_at: "three".into(),
                },
                snapshot_revision: 3,
                requests: vec![Replacement {
                    request_id: "move".into(),
                    revision: 3,
                    changed_at: "three".into(),
                    mutation: Mutation::Delete,
                }],
                tools: vec![],
                identities: vec![],
                variants: vec![],
                keys: vec![],
            })
            .await
            .unwrap();
        let reader = store.reader().await.unwrap();
        let snapshot = reports::Snapshot::begin(&reader, true).await.unwrap();
        assert_eq!(
            snapshot
                .totals(
                    owner,
                    report_window("2026-01-01T12:00:00Z", "2026-01-01T13:00:00Z")
                )
                .await
                .unwrap()
                .requests,
            0
        );
        snapshot.commit().await.unwrap();
    }

    fn seam_request(owner: Uuid, started_at: &str, cost_nanos: Option<i64>) -> RequestFacts {
        RequestFacts {
            key_id: Some("key".into()),
            owner_id: Some(owner.to_string()),
            provider: "provider".into(),
            requested_model: Some("model".into()),
            started_at: started_at.into(),
            status: Some(200),
            usage: Some(Usage {
                input_tokens: Some(1),
                cache_read_tokens: Some(2),
                cache_write_tokens: Some(3),
                output_tokens: Some(4),
                reasoning_tokens: None,
                cost_nanos,
                cost_source: Some("fixture".into()),
            }),
            context: Some(Context {
                values: [10, 0, 0, 0, 0, 10, 10, 0, 100, 1, 1, 0, 0],
                created_at: started_at.into(),
            }),
            ..Default::default()
        }
    }

    fn weight(numerator: i128) -> Rational {
        Rational {
            numerator,
            denominator: 1,
        }
    }

    fn seam_identity(owner: Uuid, id: i64) -> IdentityKey {
        IdentityKey {
            id,
            scope_key: format!("[\"owner\",\"{owner}\"]"),
            provider: "provider".into(),
            identity_kind: "call_id".into(),
            identity_key: format!("identity-{id}"),
            conversation_available: true,
        }
    }

    fn seam_variant(id: i64, identity_id: i64, kind: &str, bytes: i64) -> Variant {
        Variant {
            id,
            identity_id,
            kind: kind.into(),
            bytes,
            observed_tool_id: 1,
        }
    }

    fn seam_batch(
        source: &source::Source,
        owner: Uuid,
        requests: Vec<(&str, RequestFacts)>,
        tools: Vec<Tool>,
        identities: Vec<IdentityKey>,
        variants: Vec<Variant>,
    ) -> SourceBatch {
        let revision = requests.len() as i64;
        SourceBatch {
            source_id: source.source_id.clone(),
            source_fact_version: SOURCE_VERSION,
            epoch: source.epoch(),
            boundary: Boundary {
                revision,
                observed_at: "observed".into(),
            },
            snapshot_revision: revision,
            requests: requests
                .into_iter()
                .enumerate()
                .map(|(index, (request_id, facts))| Replacement {
                    request_id: request_id.to_owned(),
                    revision: index as i64 + 1,
                    changed_at: format!("change-{index}"),
                    mutation: Mutation::Upsert(Box::new(facts)),
                })
                .collect(),
            tools,
            identities,
            variants,
            keys: vec![Key {
                id: "key".into(),
                name: "key".into(),
                owner_id: Some(owner.to_string()),
            }],
        }
    }

    #[tokio::test]
    async fn unpriced_contributions_count_once_across_the_aggregate_and_boundary_seam() {
        let (_fixture, source, store) = fixture().await;
        let owner = Uuid::now_v7();
        let contribution = |tool_id, definition: i128, transmission: i128| Contribution {
            tool_id,
            definition_count: 1,
            definition: weight(definition),
            transmission: weight(transmission),
        };
        let mut boundary_unpriced = seam_request(owner, "2026-01-01T10:30:00.000Z", None);
        boundary_unpriced.contributions = vec![contribution(1, 6, 4), contribution(3, 5, 5)];
        let mut full_unpriced = seam_request(owner, "2026-01-01T11:10:00.000Z", None);
        full_unpriced.contributions = vec![contribution(1, 6, 4)];
        let mut full_silent = seam_request(owner, "2026-01-01T11:20:00.000Z", None);
        full_silent.contributions = vec![contribution(2, 0, 0)];
        let mut full_priced = seam_request(owner, "2026-01-01T11:40:00.000Z", Some(100));
        full_priced.contributions = vec![contribution(1, 6, 4)];
        store
            .apply_batch(&seam_batch(
                &source,
                owner,
                vec![
                    ("unpriced-boundary", boundary_unpriced),
                    ("unpriced-full", full_unpriced),
                    ("silent-full", full_silent),
                    ("priced-full", full_priced),
                ],
                vec![
                    Tool {
                        id: 1,
                        attribution_key: "one".into(),
                        tool_name: Some("mcp__server__read".into()),
                        skill_name: None,
                    },
                    Tool {
                        id: 2,
                        attribution_key: "two".into(),
                        tool_name: Some("quiet".into()),
                        skill_name: None,
                    },
                    Tool {
                        id: 3,
                        attribution_key: "three".into(),
                        tool_name: Some("Skill".into()),
                        skill_name: Some("research".into()),
                    },
                ],
                vec![],
                vec![],
            ))
            .await
            .unwrap();
        let reader = store.reader().await.unwrap();
        let snapshot = reports::Snapshot::begin(&reader, true).await.unwrap();

        let whole = report_window("2026-01-01T10:30:00Z", "2026-01-01T12:00:00Z");
        let usage = snapshot.tool_usage(owner, whole).await.unwrap();
        let unpriced_of = |usage: &crate::usage::ToolUsage, label: &str| {
            usage
                .tools
                .iter()
                .find(|tool| tool.label.as_deref() == Some(label))
                .unwrap()
                .unpriced_requests
        };
        assert_eq!(unpriced_of(&usage, "mcp__server__read"), 2);
        assert_eq!(unpriced_of(&usage, "quiet"), 0);
        assert_eq!(usage.skills.len(), 1);
        assert_eq!(usage.skills[0].unpriced_requests, 1);
        assert_eq!(usage.mcp_servers()[0].unpriced_requests, 2);
        assert_eq!(snapshot.totals(owner, whole).await.unwrap().unpriced, 3);
        assert_eq!(
            snapshot
                .context(owner, whole)
                .await
                .unwrap()
                .unpriced_requests,
            3
        );

        let boundary_only = report_window("2026-01-01T10:30:00Z", "2026-01-01T11:00:00Z");
        let aggregate_only = report_window("2026-01-01T11:00:00Z", "2026-01-01T12:00:00Z");
        let boundary = snapshot.tool_usage(owner, boundary_only).await.unwrap();
        let aggregate = snapshot.tool_usage(owner, aggregate_only).await.unwrap();
        assert_eq!(unpriced_of(&boundary, "mcp__server__read"), 1);
        assert_eq!(unpriced_of(&aggregate, "mcp__server__read"), 1);
        assert_eq!(
            snapshot
                .totals(owner, boundary_only)
                .await
                .unwrap()
                .unpriced
                + snapshot
                    .totals(owner, aggregate_only)
                    .await
                    .unwrap()
                    .unpriced,
            3
        );
        assert_eq!(
            snapshot
                .context(owner, boundary_only)
                .await
                .unwrap()
                .unpriced_requests
                + snapshot
                    .context(owner, aggregate_only)
                    .await
                    .unwrap()
                    .unpriced_requests,
            3
        );
        snapshot.commit().await.unwrap();
    }

    #[tokio::test]
    async fn ambiguous_identities_are_held_out_of_every_tool_row_including_the_uncaptured_bucket() {
        let (_fixture, source, store) = fixture().await;
        let owner = Uuid::now_v7();
        let appearance = |variant_id| Appearance {
            variant_id,
            multiplicity: 1,
        };
        let resolved = |identity_id, tool_id| Attribution {
            identity_id,
            state: "resolved".into(),
            tool_id: Some(tool_id),
        };
        let mut boundary = seam_request(owner, "2026-01-01T10:45:00.000Z", Some(100));
        boundary.appearances = vec![appearance(200), appearance(210), appearance(220)];
        boundary.attribution = vec![
            Attribution {
                identity_id: 20,
                state: "ambiguous".into(),
                tool_id: None,
            },
            resolved(21, 1),
            resolved(22, 1),
        ];
        let mut full = seam_request(owner, "2026-01-01T11:20:00.000Z", Some(100));
        full.appearances = vec![appearance(210), appearance(220)];
        full.attribution = vec![resolved(21, 2), resolved(22, 1)];
        store
            .apply_batch(&seam_batch(
                &source,
                owner,
                vec![("ambiguous-boundary", boundary), ("ambiguous-full", full)],
                vec![
                    Tool {
                        id: 1,
                        attribution_key: "one".into(),
                        tool_name: Some("read".into()),
                        skill_name: None,
                    },
                    Tool {
                        id: 2,
                        attribution_key: "two".into(),
                        tool_name: Some("write".into()),
                        skill_name: None,
                    },
                ],
                vec![
                    seam_identity(owner, 20),
                    seam_identity(owner, 21),
                    seam_identity(owner, 22),
                ],
                vec![
                    seam_variant(200, 20, "tool_use", 11),
                    seam_variant(210, 21, "tool_use", 13),
                    seam_variant(220, 22, "tool_use", 17),
                ],
            ))
            .await
            .unwrap();
        let reader = store.reader().await.unwrap();
        let snapshot = reports::Snapshot::begin(&reader, true).await.unwrap();
        let window = report_window("2026-01-01T10:30:00Z", "2026-01-01T12:00:00Z");
        let usage = snapshot.tool_usage(owner, window).await.unwrap();
        assert_eq!(usage.ambiguous_calls, 2);
        assert_eq!(usage.ambiguous_bytes, 24);
        assert!(usage.tools.iter().all(|tool| tool.label.is_some()));
        assert_eq!(
            usage
                .tools
                .iter()
                .find(|tool| tool.label.as_deref() == Some("read"))
                .unwrap()
                .calls,
            1
        );
        assert_eq!(usage.tools.iter().map(|tool| tool.calls).sum::<i64>(), 1);
        snapshot.commit().await.unwrap();
    }

    #[tokio::test]
    async fn calls_take_the_per_identity_maximum_so_a_replayed_conversation_counts_once() {
        let (_fixture, source, store) = fixture().await;
        let owner = Uuid::now_v7();
        let resolved = |identity_id| Attribution {
            identity_id,
            state: "resolved".into(),
            tool_id: Some(1),
        };
        let mut repeated = seam_request(owner, "2026-01-01T11:10:00.000Z", Some(100));
        repeated.appearances = vec![
            Appearance {
                variant_id: 300,
                multiplicity: 3,
            },
            Appearance {
                variant_id: 310,
                multiplicity: 1,
            },
        ];
        repeated.attribution = vec![resolved(30), resolved(31)];
        let replay = |started_at: &str| {
            let mut facts = seam_request(owner, started_at, Some(100));
            facts.appearances = vec![Appearance {
                variant_id: 310,
                multiplicity: 1,
            }];
            facts.attribution = vec![resolved(31)];
            facts
        };
        store
            .apply_batch(&seam_batch(
                &source,
                owner,
                vec![
                    ("replay-boundary", replay("2026-01-01T10:30:00.000Z")),
                    ("multiplicity-full", repeated),
                    ("replay-full", replay("2026-01-01T11:40:00.000Z")),
                ],
                vec![Tool {
                    id: 1,
                    attribution_key: "one".into(),
                    tool_name: Some("read".into()),
                    skill_name: None,
                }],
                vec![seam_identity(owner, 30), seam_identity(owner, 31)],
                vec![
                    seam_variant(300, 30, "tool_use", 5),
                    seam_variant(310, 31, "tool_use", 5),
                ],
            ))
            .await
            .unwrap();
        let reader = store.reader().await.unwrap();
        let snapshot = reports::Snapshot::begin(&reader, true).await.unwrap();
        let window = report_window("2026-01-01T10:30:00Z", "2026-01-01T12:00:00Z");
        let usage = snapshot.tool_usage(owner, window).await.unwrap();
        assert_eq!(usage.tools.len(), 1);
        assert_eq!(usage.tools[0].calls, 4);
        assert_eq!(usage.tools[0].bytes, 10);
        let boundary_only = report_window("2026-01-01T10:30:00Z", "2026-01-01T11:00:00Z");
        assert_eq!(
            snapshot
                .tool_usage(owner, boundary_only)
                .await
                .unwrap()
                .tools[0]
                .calls,
            1
        );
        snapshot.commit().await.unwrap();
    }

    #[tokio::test]
    async fn an_unnamed_tool_keeps_its_own_row_beside_the_uncaptured_bucket() {
        let (_fixture, source, store) = fixture().await;
        let owner = Uuid::now_v7();
        let mut boundary = seam_request(owner, "2026-01-01T10:30:00.000Z", Some(100));
        boundary.appearances = vec![Appearance {
            variant_id: 400,
            multiplicity: 1,
        }];
        boundary.attribution = vec![Attribution {
            identity_id: 40,
            state: "resolved".into(),
            tool_id: Some(5),
        }];
        let mut full = seam_request(owner, "2026-01-01T11:10:00.000Z", Some(100));
        full.appearances = vec![Appearance {
            variant_id: 410,
            multiplicity: 1,
        }];
        full.attribution = vec![Attribution {
            identity_id: 41,
            state: "unresolved".into(),
            tool_id: None,
        }];
        store
            .apply_batch(&seam_batch(
                &source,
                owner,
                vec![("unnamed-boundary", boundary), ("uncaptured-full", full)],
                vec![Tool {
                    id: 5,
                    attribution_key: "five".into(),
                    tool_name: None,
                    skill_name: None,
                }],
                vec![seam_identity(owner, 40), seam_identity(owner, 41)],
                vec![
                    Variant {
                        id: 400,
                        identity_id: 40,
                        kind: "tool_use".into(),
                        bytes: 9,
                        observed_tool_id: 5,
                    },
                    Variant {
                        id: 410,
                        identity_id: 41,
                        kind: "tool_result".into(),
                        bytes: 21,
                        observed_tool_id: 5,
                    },
                ],
            ))
            .await
            .unwrap();
        let reader = store.reader().await.unwrap();
        let snapshot = reports::Snapshot::begin(&reader, true).await.unwrap();
        let window = report_window("2026-01-01T10:30:00Z", "2026-01-01T12:00:00Z");
        let usage = snapshot.tool_usage(owner, window).await.unwrap();
        assert_eq!(usage.tools.len(), 2);
        assert!(usage.tools.iter().all(|tool| tool.label.is_none()));
        assert_eq!(usage.tools[0].calls, 1);
        assert_eq!(usage.tools[0].bytes, 9);
        assert_eq!(usage.tools[1].calls, 0);
        assert_eq!(usage.tools[1].bytes, 21);
        assert_eq!(usage.ambiguous_calls, 0);
        snapshot.commit().await.unwrap();
    }

    #[tokio::test]
    async fn an_appearance_without_an_attribution_row_reports_as_uncaptured() {
        let (_fixture, source, store) = fixture().await;
        let owner = Uuid::now_v7();
        let mut orphan = seam_request(owner, "2026-01-01T11:10:00.000Z", Some(100));
        orphan.appearances = vec![Appearance {
            variant_id: 500,
            multiplicity: 2,
        }];
        orphan.attribution.clear();
        store
            .apply_batch(&seam_batch(
                &source,
                owner,
                vec![("orphan", orphan)],
                vec![Tool {
                    id: 1,
                    attribution_key: "one".into(),
                    tool_name: Some("read".into()),
                    skill_name: None,
                }],
                vec![seam_identity(owner, 50)],
                vec![seam_variant(500, 50, "tool_use", 13)],
            ))
            .await
            .unwrap();
        let reader = store.reader().await.unwrap();
        let snapshot = reports::Snapshot::begin(&reader, true).await.unwrap();
        let window = report_window("2026-01-01T10:30:00Z", "2026-01-01T12:00:00Z");
        let usage = snapshot.tool_usage(owner, window).await.unwrap();
        assert_eq!(usage.tools.len(), 1, "{usage:?}");
        assert_eq!(usage.tools[0].label, None);
        assert_eq!(usage.tools[0].calls, 2);
        assert_eq!(usage.tools[0].bytes, 13);
        assert_eq!(usage.ambiguous_calls, 0);
        snapshot.commit().await.unwrap();
    }

    #[tokio::test]
    async fn offset_request_starts_are_stored_canonically_and_unreadable_ones_are_refused() {
        let (_fixture, source, store) = fixture().await;
        let owner = Uuid::now_v7();
        let refusal = store
            .apply_batch(&seam_batch(
                &source,
                owner,
                vec![(
                    "unreadable",
                    seam_request(owner, "2026-01-01 10:30:00", Some(100)),
                )],
                vec![],
                vec![],
                vec![],
            ))
            .await;
        let refusal = format!("{:#}", refusal.unwrap_err());
        assert!(
            refusal.contains("unreadable request start time")
                && refusal.contains("source request unreadable"),
            "the refusal must name both the unreadable value and the request carrying it: {refusal}"
        );
        assert_eq!(count(&store.database, "requests").await, 0);

        store
            .apply_batch(&seam_batch(
                &source,
                owner,
                vec![(
                    "offset",
                    seam_request(owner, "2026-01-01T16:00:00+05:30", Some(100)),
                )],
                vec![],
                vec![],
                vec![],
            ))
            .await
            .unwrap();
        let stored: String = store
            .database
            .query_one_raw(sql("SELECT started_at FROM requests", vec![]))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "started_at")
            .unwrap();
        assert_eq!(stored, "2026-01-01T10:30:00.000Z");
        let reader = store.reader().await.unwrap();
        let snapshot = reports::Snapshot::begin(&reader, true).await.unwrap();
        assert_eq!(
            snapshot
                .totals(
                    owner,
                    report_window("2026-01-01T10:30:00Z", "2026-01-01T11:00:00Z")
                )
                .await
                .unwrap()
                .requests,
            1
        );
        snapshot.commit().await.unwrap();
    }

    /// Every statement the report layer issues, in the form `reports.rs` builds
    /// it. Each is asserted to occur in that file, so a query that changes
    /// shape cannot quietly escape the plan assertions below.
    /// The second element is the text `reports.rs` spells out when the
    /// statement is assembled from a format string; `None` means the statement
    /// is written there literally.
    const REPORT_QUERIES: [(&str, Option<&str>); 19] = [
        (
            "SELECT requests,succeeded,failed,input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,cost_nanos,unpriced FROM hourly_owner_overview WHERE owner_key=? AND hour>=? AND hour<?",
            None,
        ),
        (
            "SELECT COUNT(*) requests,COALESCE(SUM(CASE WHEN r.status<400 AND NOT r.has_error THEN 1 ELSE 0 END),0) succeeded,COALESCE(SUM(CASE WHEN r.status>=400 OR r.has_error THEN 1 ELSE 0 END),0) failed,COALESCE(SUM(u.input_tokens),0) input_tokens,COALESCE(SUM(u.cache_read_tokens),0) cache_read_tokens,COALESCE(SUM(u.cache_write_tokens),0) cache_write_tokens,COALESCE(SUM(u.output_tokens),0) output_tokens,COALESCE(SUM(u.cost_nanos),0) cost_nanos,COALESCE(SUM(u.cost_nanos IS NULL),0) unpriced FROM requests r LEFT JOIN usage u ON u.request_id=r.id WHERE r.owner_id=? AND (r.started_at>=? AND r.started_at<=?)",
            Some(
                "COALESCE(SUM(u.cost_nanos IS NULL),0) unpriced FROM requests r LEFT JOIN usage u ON u.request_id=r.id WHERE r.owner_id=? AND ({predicate})",
            ),
        ),
        (
            "SELECT requests,tool_definition_bytes,system_bytes,user_text_bytes,assistant_text_bytes,thinking_bytes,tool_use_bytes,tool_result_bytes,other_bytes,total_bytes,tools_offered,tools_invoked,tool_result_errors,cache_breakpoints FROM hourly_context WHERE owner_key=? AND hour>=? AND hour<?",
            None,
        ),
        (
            "SELECT component,lower_scaled,remainders,unpriced FROM hourly_context_cost WHERE owner_key=? AND hour>=? AND hour<?",
            None,
        ),
        (
            "SELECT c.*,u.cost_nanos FROM requests r JOIN context c ON c.request_id=r.id LEFT JOIN usage u ON u.request_id=r.id WHERE r.owner_id=? AND (r.started_at>=? AND r.started_at<=?)",
            Some(
                "SELECT c.*,u.cost_nanos FROM requests r JOIN context c ON c.request_id=r.id LEFT JOIN usage u ON u.request_id=r.id WHERE r.owner_id=? AND ({predicate})",
            ),
        ),
        (
            "SELECT c.*,u.cost_nanos FROM requests r JOIN context c ON c.request_id=r.id LEFT JOIN usage u ON u.request_id=r.id WHERE r.owner_id=? AND r.started_at>=? AND r.started_at<=?",
            None,
        ),
        (
            "SELECT h.*,t.tool_name,t.skill_name FROM hourly_tool_contribution h JOIN tools t ON t.id=h.tool_id WHERE h.owner_key=? AND h.hour>=? AND h.hour<?",
            None,
        ),
        (
            "SELECT c.*,u.cost_nanos,x.total_bytes,t.tool_name,t.skill_name FROM requests r JOIN contributions c ON c.request_id=r.id JOIN context x ON x.request_id=r.id LEFT JOIN usage u ON u.request_id=r.id JOIN tools t ON t.id=c.tool_id WHERE r.owner_id=? AND (r.started_at>=? AND r.started_at<=?)",
            Some(
                "SELECT c.*,u.cost_nanos,x.total_bytes,t.tool_name,t.skill_name FROM requests r JOIN contributions c ON c.request_id=r.id JOIN context x ON x.request_id=r.id LEFT JOIN usage u ON u.request_id=r.id JOIN tools t ON t.id=c.tool_id WHERE r.owner_id=? AND ({predicate})",
            ),
        ),
        (
            "SELECT c.*,u.cost_nanos,x.total_bytes FROM requests r JOIN contributions c ON c.request_id=r.id JOIN context x ON x.request_id=r.id LEFT JOIN usage u ON u.request_id=r.id WHERE r.owner_id=? AND r.started_at>=? AND r.started_at<=?",
            None,
        ),
        (
            "SELECT identity_id FROM hourly_identity_presence WHERE owner_key=? AND hour>=? AND hour<?",
            None,
        ),
        (
            "SELECT DISTINCT v.identity_id FROM requests r JOIN appearances a ON a.request_id=r.id JOIN variants v ON v.id=a.variant_id WHERE r.owner_id=? AND (r.started_at>=? AND r.started_at<=?)",
            Some(
                "SELECT DISTINCT v.identity_id FROM requests r JOIN appearances a ON a.request_id=r.id JOIN variants v ON v.id=a.variant_id WHERE r.owner_id=? AND ({predicate})",
            ),
        ),
        (
            "SELECT v.identity_id,v.kind,v.bytes,a.multiplicity,ria.state,ria.tool_id,t.tool_name,t.skill_name FROM requests r JOIN appearances a ON a.request_id=r.id JOIN variants v ON v.id=a.variant_id LEFT JOIN request_identity_attribution ria ON ria.request_id=r.id AND ria.identity_id=v.identity_id LEFT JOIN tools t ON t.id=ria.tool_id WHERE r.owner_id=? AND r.started_at>=? AND r.started_at<=?",
            None,
        ),
        (
            "SELECT dimension,requests,succeeded,failed,input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,cost_nanos,unpriced FROM hourly_owner_model WHERE owner_key=? AND hour>=? AND hour<?",
            Some(
                "SELECT dimension,requests,succeeded,failed,input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,cost_nanos,unpriced FROM {} WHERE owner_key=? AND hour>=? AND hour<?",
            ),
        ),
        (
            "SELECT dimension,requests,succeeded,failed,input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,cost_nanos,unpriced FROM hourly_owner_provider WHERE owner_key=? AND hour>=? AND hour<?",
            Some("hourly_owner_provider"),
        ),
        (
            "SELECT dimension,requests,succeeded,failed,input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,cost_nanos,unpriced FROM hourly_owner_key WHERE owner_key=? AND hour>=? AND hour<?",
            Some("hourly_owner_key"),
        ),
        (
            "SELECT r.requested_model dimension,COUNT(*) requests,COALESCE(SUM(CASE WHEN r.status<400 AND NOT r.has_error THEN 1 ELSE 0 END),0) succeeded,COALESCE(SUM(CASE WHEN r.status>=400 OR r.has_error THEN 1 ELSE 0 END),0) failed,COALESCE(SUM(u.input_tokens),0) input_tokens,COALESCE(SUM(u.cache_read_tokens),0) cache_read_tokens,COALESCE(SUM(u.cache_write_tokens),0) cache_write_tokens,COALESCE(SUM(u.output_tokens),0) output_tokens,COALESCE(SUM(u.cost_nanos),0) cost_nanos,COALESCE(SUM(u.cost_nanos IS NULL),0) unpriced FROM requests r LEFT JOIN usage u ON u.request_id=r.id WHERE r.owner_id=? AND (r.started_at>=? AND r.started_at<=?) GROUP BY r.requested_model",
            Some(
                "FROM requests r LEFT JOIN usage u ON u.request_id=r.id WHERE r.owner_id=? AND ({predicate}) GROUP BY {}",
            ),
        ),
        (
            "SELECT dimension dimension,hour,requests,succeeded,failed,input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,cost_nanos,unpriced FROM hourly_owner_model WHERE owner_key=? AND hour>=? AND hour<?",
            Some(
                "SELECT {dim} dimension,hour,requests,succeeded,failed,input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,cost_nanos,unpriced FROM {table} WHERE owner_key=? AND hour>=? AND hour<?",
            ),
        ),
        (
            "SELECT r.provider dimension,strftime('%Y-%m-%dT', r.started_at) || printf('%02d', (CAST(strftime('%H', r.started_at) AS INTEGER) / 3) * 3) || ':00:00Z' bucket,COUNT(*) requests,COALESCE(SUM(CASE WHEN r.status<400 AND NOT r.has_error THEN 1 ELSE 0 END),0) succeeded,COALESCE(SUM(CASE WHEN r.status>=400 OR r.has_error THEN 1 ELSE 0 END),0) failed,COALESCE(SUM(u.input_tokens),0) input_tokens,COALESCE(SUM(u.cache_read_tokens),0) cache_read_tokens,COALESCE(SUM(u.cache_write_tokens),0) cache_write_tokens,COALESCE(SUM(u.output_tokens),0) output_tokens,COALESCE(SUM(u.cost_nanos),0) cost_nanos,COALESCE(SUM(u.cost_nanos IS NULL),0) unpriced FROM requests r LEFT JOIN usage u ON u.request_id=r.id WHERE r.owner_id=? AND (r.started_at>=? AND r.started_at<=?) GROUP BY dimension,bucket",
            Some(
                "FROM requests r LEFT JOIN usage u ON u.request_id=r.id WHERE r.owner_id=? AND ({predicate}) GROUP BY dimension,bucket",
            ),
        ),
        ("SELECT name FROM keys WHERE id=?", None),
    ];

    /// Grains a report must read from. Listing them keeps a table from losing
    /// its plan coverage when its query is rewritten or removed.
    const COVERED_GRAINS: [&str; 12] = [
        "hourly_owner_overview",
        "hourly_owner_model",
        "hourly_owner_provider",
        "hourly_owner_key",
        "hourly_context",
        "hourly_context_cost",
        "hourly_tool_contribution",
        "hourly_identity_presence",
        "requests",
        "usage",
        "context",
        "contributions",
    ];

    #[tokio::test]
    async fn report_queries_seek_analytics_indexes_and_never_reach_for_payload_tables() {
        let (_fixture, _source, store) = fixture().await;
        let reader = store.reader().await.unwrap();
        let source_text = include_str!("reports.rs");
        for (query, written) in REPORT_QUERIES {
            let written = written.unwrap_or(query);
            assert!(
                source_text.contains(written),
                "report query drifted from reports.rs: {written}"
            );
            let plan = reader
                .query_all_raw(sql(&format!("EXPLAIN QUERY PLAN {query}"), vec![]))
                .await
                .unwrap()
                .into_iter()
                .map(|row| row.try_get::<String>("", "detail").unwrap())
                .collect::<Vec<_>>()
                .join("\n");
            for line in plan.lines() {
                let line = line.trim();
                if let Some(step) = line.strip_prefix("SCAN ") {
                    panic!("report query full-scans {step}: {query}\n{plan}");
                }
                assert!(
                    !line.starts_with("SEARCH ") || line.contains(" USING "),
                    "report query seeks without an index: {line}\n{query}"
                );
            }
            assert!(
                plan.contains("USING PRIMARY KEY")
                    || plan.contains("USING INTEGER PRIMARY KEY")
                    || plan.contains("USING INDEX")
                    || plan.contains("USING COVERING INDEX"),
                "report query reaches no index at all: {query}\n{plan}"
            );
            for capture_side in [
                "gateway_payload_envelopes",
                "gateway_payload_blobs",
                "gateway_payload_part_refs",
            ] {
                assert!(!plan.contains(capture_side), "{query} reads {capture_side}");
            }
        }
        assert!(
            !source_text.contains("payload") && !source_text.contains("gateway_"),
            "reports.rs names a capture-side table"
        );
        for table in COVERED_GRAINS {
            assert!(
                REPORT_QUERIES
                    .iter()
                    .any(|(query, _)| query.contains(table)),
                "no report query covers {table}"
            );
        }
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
                &source,
                2,
                Mutation::Upsert(Box::new(RequestFacts {
                    provider: "p".into(),
                    started_at: "2026-01-01T00:00:00.000Z".into(),
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
