//! Replaceable compact tool source facts. Preparation never queries stored payloads.
//! Weights are exact byte fractions, not rounded allocated costs. Projection must
//! multiply by authoritative request nanodollars / request total bytes.
//!
//! Deletion with occupied appearances is deliberately blocked by migration 19.
//! A later withdrawal API must remove observations and invalidate dependents
//! atomically. Definition-only deletion can retain facts behind a tombstone.
use crate::telemetry::SemanticPayload;
use sea_orm::{ConnectionTrait, DatabaseTransaction, DbBackend, DbErr, Statement, Value};
use std::collections::{BTreeMap, BTreeSet};

type Attribution = (Option<String>, Option<String>);

/// Checked positive rational arithmetic. Decimal TEXT storage avoids SQLite's
/// silent promotion of overflowing integer arithmetic to floating point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Weight {
    pub(crate) num: i128,
    pub(crate) den: i128,
}
impl Default for Weight {
    fn default() -> Self {
        Self { num: 0, den: 1 }
    }
}
impl Weight {
    fn add(&mut self, num: i128, den: i128) -> Result<(), DbErr> {
        if num < 0 || den <= 0 {
            return Err(overflow());
        }
        fn gcd(mut a: i128, mut b: i128) -> i128 {
            while b != 0 {
                (a, b) = (b, a % b);
            }
            a
        }
        let g = gcd(self.den, den);
        let n = self
            .num
            .checked_mul(den / g)
            .and_then(|a| num.checked_mul(self.den / g).and_then(|b| a.checked_add(b)))
            .ok_or_else(overflow)?;
        let d = self.den.checked_mul(den / g).ok_or_else(overflow)?;
        let g = gcd(n, d);
        *self = Self {
            num: n / g,
            den: d / g,
        };
        Ok(())
    }
}
fn overflow() -> DbErr {
    DbErr::Custom("analytics tool weight overflow or invalid bytes".into())
}
fn sql(text: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Sqlite, text, values)
}

#[derive(Default)]
struct Definition {
    count: i64,
    weight: Weight,
}
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Observation {
    identity_kind: &'static str,
    identity_key: String,
    kind: &'static str,
    bytes: i64,
    attribution: Attribution,
}
#[derive(Default)]
pub(crate) struct PreparedTools {
    definitions: BTreeMap<Attribution, Definition>,
    observations: BTreeMap<Observation, i64>,
}
impl PreparedTools {
    pub(crate) fn semantic(payload: &SemanticPayload) -> Result<Self, DbErr> {
        let mut prepared = Self::default();
        for part in &payload.parts {
            let definitions = part
                .facts
                .iter()
                .filter(|f| f.block_type == "tool_definition")
                .count();
            for fact in &part.facts {
                let attribution = (fact.tool_name.clone(), fact.skill_name.clone());
                if fact.block_type == "tool_definition" {
                    let entry = prepared.definitions.entry(attribution).or_default();
                    entry.count = entry.count.checked_add(1).ok_or_else(overflow)?;
                    entry
                        .weight
                        .add(i128::from(part.payload.original_bytes), definitions as i128)?;
                } else if matches!(fact.block_type, "tool_use" | "tool_result") {
                    if part.payload.original_bytes < 0 {
                        return Err(overflow());
                    }
                    let (identity_kind, identity_key) = match &fact.tool_use_id {
                        Some(id) => ("call_id", id.clone()),
                        None => ("content_hash", part.payload.id.clone()),
                    };
                    let count = prepared
                        .observations
                        .entry(Observation {
                            identity_kind,
                            identity_key,
                            kind: fact.block_type,
                            bytes: part.payload.original_bytes,
                            attribution,
                        })
                        .or_default();
                    *count = count.checked_add(1).ok_or_else(overflow)?;
                }
            }
        }
        Ok(prepared)
    }

    /// Caller owns the capture transaction and writer gate. Replacements remove
    /// old appearances before recomputing attribution from occupied call variants.
    pub(crate) async fn store(&self, db: &DatabaseTransaction, request: &str) -> Result<(), DbErr> {
        let source = db.query_one_raw(sql(
            "SELECT r.provider, k.user_id FROM gateway_requests r LEFT JOIN gateway_keys k ON k.id = r.key_id WHERE r.id = ?",
            vec![request.into()],
        )).await?.ok_or_else(|| DbErr::Custom("tool facts require an existing request".into()))?;
        let provider: String = source.try_get("", "provider")?;
        let owner: Option<String> = source.try_get("", "user_id")?;
        // Tagged JSON namespaces cannot collide with a literal owner or UUID.
        let scope = serde_json::to_string(&match owner {
            Some(owner) => ("owner", owner),
            None => ("request", request.to_owned()),
        })
        .map_err(|e| DbErr::Custom(e.to_string()))?;
        db.execute_raw(sql(
            "INSERT OR IGNORE INTO gateway_analytics_requests(request_id) VALUES (?)",
            vec![request.into()],
        ))
        .await?;
        let request_id = id(
            db,
            "SELECT id FROM gateway_analytics_requests WHERE request_id = ?",
            vec![request.into()],
        )
        .await?;
        let mut identities = BTreeSet::new();
        for row in db.query_all_raw(sql("SELECT DISTINCT v.identity_id FROM gateway_analytics_tool_appearances a JOIN gateway_analytics_tool_variants v ON v.id = a.variant_id WHERE a.request_id = ?", vec![request_id.into()])).await? {
            identities.insert(row.try_get::<i64>("", "identity_id")?);
        }
        db.execute_raw(sql(
            "DELETE FROM gateway_analytics_tool_appearances WHERE request_id = ?",
            vec![request_id.into()],
        ))
        .await?;
        db.execute_raw(sql(
            "DELETE FROM gateway_analytics_tool_contributions WHERE request_id = ?",
            vec![request_id.into()],
        ))
        .await?;
        for (attribution, definition) in &self.definitions {
            let tool = dimension(db, attribution).await?;
            db.execute_raw(sql(
                "INSERT INTO gateway_analytics_tool_contributions VALUES (?, ?, ?, ?, ?, '0', '1')",
                vec![
                    request_id.into(),
                    tool.into(),
                    definition.count.into(),
                    definition.weight.num.to_string().into(),
                    definition.weight.den.to_string().into(),
                ],
            ))
            .await?;
        }
        for (observation, multiplicity) in &self.observations {
            let tool = dimension(db, &observation.attribution).await?;
            let keys = vec![
                scope.clone().into(),
                provider.clone().into(),
                observation.identity_kind.into(),
                observation.identity_key.clone().into(),
            ];
            db.execute_raw(sql("INSERT OR IGNORE INTO gateway_analytics_tool_identities(scope_key, provider, identity_kind, identity_key, state) VALUES (?, ?, ?, ?, 'unresolved')", keys.clone())).await?;
            let identity = id(db, "SELECT id FROM gateway_analytics_tool_identities WHERE scope_key = ? AND provider = ? AND identity_kind = ? AND identity_key = ?", keys).await?;
            identities.insert(identity);
            let keys = vec![
                identity.into(),
                observation.kind.into(),
                observation.bytes.into(),
                tool.into(),
            ];
            db.execute_raw(sql("INSERT OR IGNORE INTO gateway_analytics_tool_variants(identity_id, kind, bytes, observed_tool_id) VALUES (?, ?, ?, ?)", keys.clone())).await?;
            let variant = id(db, "SELECT id FROM gateway_analytics_tool_variants WHERE identity_id = ? AND kind = ? AND bytes = ? AND observed_tool_id = ?", keys).await?;
            db.execute_raw(sql(
                "INSERT INTO gateway_analytics_tool_appearances VALUES (?, ?, ?)",
                vec![request_id.into(), variant.into(), (*multiplicity).into()],
            ))
            .await?;
        }
        let mut changed = Vec::new();
        for identity in identities {
            if resolve(db, identity).await? {
                changed.push(identity);
            }
        }
        invalidate(db, request_id, &changed).await?;
        // Only this request's cache is rebuilt under the capture writer.
        // Historical dependents remain dirty until a snapshot reader derives them.
        let contributions = snapshot_contributions(db, request_id).await?;
        db.execute_raw(sql(
            "DELETE FROM gateway_analytics_tool_contributions WHERE request_id = ?",
            vec![request_id.into()],
        ))
        .await?;
        for fact in contributions {
            let tool = dimension(db, &(fact.tool_name, fact.skill_name)).await?;
            db.execute_raw(sql(
                "INSERT INTO gateway_analytics_tool_contributions VALUES (?, ?, ?, ?, ?, ?, ?)",
                vec![
                    request_id.into(),
                    tool.into(),
                    fact.definition_count.into(),
                    fact.definition_weight.num.to_string().into(),
                    fact.definition_weight.den.to_string().into(),
                    fact.transmission_weight.num.to_string().into(),
                    fact.transmission_weight.den.to_string().into(),
                ],
            ))
            .await?;
        }
        db.execute_raw(sql(
            "UPDATE gateway_analytics_requests SET contributions_dirty = 0 WHERE id = ?",
            vec![request_id.into()],
        ))
        .await?;
        Ok(())
    }
}
async fn id(db: &impl ConnectionTrait, query: &str, values: Vec<Value>) -> Result<i64, DbErr> {
    db.query_one_raw(sql(query, values))
        .await?
        .ok_or_else(|| DbErr::Custom("missing analytics fact".into()))?
        .try_get("", "id")
}
async fn dimension(db: &impl ConnectionTrait, attribution: &Attribution) -> Result<i64, DbErr> {
    let key = serde_json::to_string(attribution).map_err(|e| DbErr::Custom(e.to_string()))?;
    db.execute_raw(sql("INSERT OR IGNORE INTO gateway_analytics_tools(attribution_key, tool_name, skill_name) VALUES (?, ?, ?)", vec![key.clone().into(), attribution.0.clone().into(), attribution.1.clone().into()])).await?;
    id(
        db,
        "SELECT id FROM gateway_analytics_tools WHERE attribution_key = ?",
        vec![key.into()],
    )
    .await
}
async fn resolve(db: &impl ConnectionTrait, identity: i64) -> Result<bool, DbErr> {
    let calls = db.query_all_raw(sql("SELECT DISTINCT v.observed_tool_id, t.tool_name, t.skill_name FROM gateway_analytics_tool_variants v JOIN gateway_analytics_tools t ON t.id = v.observed_tool_id WHERE v.identity_id = ? AND v.kind = 'tool_use' AND EXISTS (SELECT 1 FROM gateway_analytics_tool_appearances a WHERE a.variant_id = v.id)", vec![identity.into()])).await?;
    let (state, tool): (&str, Option<i64>) = match calls.as_slice() {
        [] => ("unresolved", None),
        [call]
            if call.try_get::<Option<String>>("", "tool_name")?.is_some()
                || call.try_get::<Option<String>>("", "skill_name")?.is_some() =>
        {
            ("resolved", Some(call.try_get("", "observed_tool_id")?))
        }
        [_] => ("unresolved", None),
        _ => ("ambiguous", None),
    };
    Ok(db.execute_raw(sql("UPDATE gateway_analytics_tool_identities SET state = ?, tool_id = ? WHERE id = ? AND (state != ? OR tool_id IS NOT ?)", vec![state.into(), tool.into(), identity.into(), state.into(), tool.into()])).await?.rows_affected() != 0)
}
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ContributionFact {
    pub(crate) tool_name: Option<String>,
    pub(crate) skill_name: Option<String>,
    pub(crate) definition_count: i64,
    pub(crate) definition_weight: Weight,
    pub(crate) transmission_weight: Weight,
}

/// Read using the integer request surrogate in the caller's consistent snapshot.
/// Dirty caches are never repaired here. No payload or pricing reads occur.
pub(crate) async fn snapshot_contributions(
    db: &DatabaseTransaction,
    request: i64,
) -> Result<Vec<ContributionFact>, DbErr> {
    let row = db
        .query_one_raw(sql(
            "SELECT contributions_dirty FROM gateway_analytics_requests WHERE id = ?",
            vec![request.into()],
        ))
        .await?
        .ok_or_else(|| DbErr::Custom("missing analytics request".into()))?;
    let dirty: bool = row.try_get("", "contributions_dirty")?;
    let rows = db.query_all_raw(sql("SELECT t.tool_name, t.skill_name, c.definition_count, c.definition_num, c.definition_den, c.transmission_num, c.transmission_den FROM gateway_analytics_tool_contributions c JOIN gateway_analytics_tools t ON t.id = c.tool_id WHERE c.request_id = ?", vec![request.into()])).await?;
    let mut facts: BTreeMap<Attribution, ContributionFact> = BTreeMap::new();
    for row in rows {
        let count: i64 = row.try_get("", "definition_count")?;
        if dirty && count == 0 {
            continue;
        }
        let attribution = (
            row.try_get("", "tool_name")?,
            row.try_get("", "skill_name")?,
        );
        let weight = |num: &str, den: &str| -> Result<Weight, DbErr> {
            let num = row
                .try_get::<String>("", num)?
                .parse()
                .map_err(|_| overflow())?;
            let den = row
                .try_get::<String>("", den)?
                .parse()
                .map_err(|_| overflow())?;
            let mut weight = Weight::default();
            weight.add(num, den)?;
            Ok(weight)
        };
        facts.insert(
            attribution,
            ContributionFact {
                definition_count: count,
                definition_weight: weight("definition_num", "definition_den")?,
                transmission_weight: if dirty {
                    Weight::default()
                } else {
                    weight("transmission_num", "transmission_den")?
                },
                ..Default::default()
            },
        );
    }
    if dirty {
        let rows = db.query_all_raw(sql("SELECT t.tool_name, t.skill_name, v.bytes, a.multiplicity FROM gateway_analytics_tool_appearances a JOIN gateway_analytics_tool_variants v ON v.id = a.variant_id JOIN gateway_analytics_tool_identities i ON i.id = v.identity_id LEFT JOIN gateway_analytics_tools t ON t.id = i.tool_id WHERE a.request_id = ?", vec![request.into()])).await?;
        for row in rows {
            let attribution = (
                row.try_get("", "tool_name")?,
                row.try_get("", "skill_name")?,
            );
            let bytes: i64 = row.try_get("", "bytes")?;
            let count: i64 = row.try_get("", "multiplicity")?;
            let num = i128::from(bytes)
                .checked_mul(i128::from(count))
                .ok_or_else(overflow)?;
            facts
                .entry(attribution)
                .or_default()
                .transmission_weight
                .add(num, 1)?;
        }
    }
    Ok(facts
        .into_iter()
        .map(|((tool_name, skill_name), fact)| ContributionFact {
            tool_name,
            skill_name,
            ..fact
        })
        .collect())
}

async fn invalidate(
    db: &impl ConnectionTrait,
    request: i64,
    identities: &[i64],
) -> Result<(), DbErr> {
    // Integer type CHECK on the clock also protects trigger callers. Check here
    // explicitly so exhaustion cannot silently leave dependencies acknowledged.
    let changed = db.execute_unprepared("UPDATE gateway_analytics_clock SET revision = revision + 1 WHERE id = 1 AND revision < 9223372036854775807").await?;
    if changed.rows_affected() != 1 {
        return Err(DbErr::Custom("analytics revision clock exhausted".into()));
    }
    // Bind only identities touched by this request, never materialize dependent IDs.
    let identities = serde_json::to_string(identities).map_err(|e| DbErr::Custom(e.to_string()))?;
    let affected = "SELECT ? UNION SELECT a.request_id FROM gateway_analytics_tool_variants v JOIN gateway_analytics_tool_appearances a ON a.variant_id = v.id WHERE v.identity_id IN (SELECT value FROM json_each(?))";
    let values = vec![request.into(), identities.into()];
    db.execute_raw(sql(
        &format!(
            "UPDATE gateway_analytics_requests SET contributions_dirty = 1 WHERE id IN ({affected})"
        ),
        values.clone(),
    ))
    .await?;
    db.execute_raw(sql(&format!("INSERT INTO gateway_analytics_revisions(request_id, revision, first_pending_revision, changed_at, deleted)
SELECT r.request_id, c.revision, c.revision, strftime('%Y-%m-%dT%H:%M:%fZ','now'), NOT EXISTS(SELECT 1 FROM gateway_requests g WHERE g.id = r.request_id)
FROM gateway_analytics_requests r CROSS JOIN gateway_analytics_clock c WHERE r.id IN ({affected}) AND c.id = 1
ON CONFLICT(request_id) DO UPDATE SET revision = excluded.revision, first_pending_revision = COALESCE(gateway_analytics_revisions.first_pending_revision, excluded.revision), changed_at = excluded.changed_at, deleted = excluded.deleted"), values)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        db::begin_immediate,
        providers::Provider,
        telemetry::{SqliteSink, StartRecord, split_request},
    };
    use sea_orm::{DatabaseConnection, TransactionTrait};
    use serde_json::{Value as Json, json};

    async fn owners(db: &DatabaseConnection) {
        for owner in ["a", "b"] {
            db.execute_unprepared(&format!("INSERT INTO users(id,email_normalized,email_display,role,status,auth_version,created_at,updated_at) VALUES('{owner}','{owner}@example.com','{owner}@example.com','user','active',0,'2026-01-01','2026-01-01'); INSERT INTO gateway_keys(id,user_id,name,allowed_providers,created_at) VALUES('{owner}','{owner}','agent','[]','2026-01-01')")).await.unwrap();
        }
    }
    async fn capture(db: &DatabaseConnection, owner: &str, provider: &str, body: Json) -> String {
        SqliteSink::new(db.clone())
            .start(StartRecord {
                request_id: "external",
                key_id: owner,
                key_version_id: "v",
                provider_id: provider,
                provider: Provider::Anthropic,
                method: "POST",
                endpoint: "/messages",
                requested_model: None,
                request_body: &serde_json::to_vec(&body).unwrap(),
            })
            .await
            .unwrap()
            .to_string()
    }
    fn call(name: &str) -> Json {
        json!({"type":"tool_use","id":"same","name":name,"input":{}})
    }
    fn result() -> Json {
        json!({"type":"tool_result","tool_use_id":"same","content":"ok"})
    }
    fn body(parts: Vec<Json>) -> Json {
        json!({"messages":[{"role":"user","content":parts}]})
    }
    async fn number(db: &impl ConnectionTrait, query: &str) -> i64 {
        db.query_one_raw(sql(query, vec![]))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "n")
            .unwrap()
    }
    async fn revision(db: &impl ConnectionTrait, request: &str) -> (i64, Option<i64>) {
        let row = db.query_one_raw(sql("SELECT revision, first_pending_revision FROM gateway_analytics_revisions WHERE request_id = ?", vec![request.into()])).await.unwrap().unwrap();
        (
            row.try_get("", "revision").unwrap(),
            row.try_get("", "first_pending_revision").unwrap(),
        )
    }
    async fn replace(db: &DatabaseConnection, request: &str, value: Json) {
        let payload = split_request(&serde_json::to_vec(&value).unwrap(), "anthropic_messages");
        let prepared = payload
            .as_ref()
            .map(PreparedTools::semantic)
            .transpose()
            .unwrap()
            .unwrap_or_default();
        let tx = begin_immediate(db).await.unwrap();
        prepared.store(&tx, request).await.unwrap();
        tx.commit().await.unwrap();
    }

    #[tokio::test]
    async fn replay_multiplicity_and_byte_variants_are_compact() {
        let db = crate::request_metrics::tests::database().await;
        owners(&db).await;
        let c = call("Read");
        let bytes = serde_json::to_vec(&c).unwrap().len() as i64;
        capture(&db, "a", "p", body(vec![c.clone(), c.clone(), result()])).await;
        let mut longer = c.clone();
        longer["input"] = json!({"path":"longer"});
        capture(&db, "a", "p", body(vec![c, longer])).await;
        assert_eq!(
            number(
                &db,
                "SELECT COUNT(*) n FROM gateway_analytics_tool_identities"
            )
            .await,
            1
        );
        assert_eq!(
            number(
                &db,
                "SELECT COUNT(*) n FROM gateway_analytics_tool_variants"
            )
            .await,
            3
        );
        assert_eq!(
            number(
                &db,
                "SELECT COUNT(*) n FROM gateway_analytics_tool_appearances"
            )
            .await,
            4
        );
        assert_eq!(
            number(
                &db,
                "SELECT SUM(multiplicity) n FROM gateway_analytics_tool_appearances"
            )
            .await,
            5
        );
        assert_eq!(
            number(
                &db,
                "SELECT COUNT(*) n FROM gateway_analytics_tool_contributions"
            )
            .await,
            2
        );
        assert_eq!(number(&db, &format!("SELECT COUNT(*) n FROM gateway_analytics_tool_variants WHERE kind = 'tool_use' AND bytes = {bytes}")).await, 1);
        assert_eq!(number(&db, "SELECT SUM(CAST(transmission_num AS INTEGER)) n FROM gateway_analytics_tool_contributions").await,
            3 * bytes + serde_json::to_vec(&call_with_path()).unwrap().len() as i64 + serde_json::to_vec(&result()).unwrap().len() as i64);
    }
    fn call_with_path() -> Json {
        let mut c = call("Read");
        c["input"] = json!({"path":"longer"});
        c
    }

    #[test]
    fn repeated_definitions_share_one_grain_and_container_bytes_split_exactly() {
        let payload = split_request(br#"{"input":[{"type":"additional_tools","tools":[{"name":"A"},{"name":"A"},{"name":"B"}]}],"tools":[{"name":"A"}]}"#, Provider::Codex.protocol()).unwrap();
        let container = payload
            .parts
            .iter()
            .find(|p| p.kind == "additional_tools")
            .unwrap()
            .payload
            .original_bytes;
        let single = payload
            .parts
            .iter()
            .find(|p| p.path == "tools")
            .unwrap()
            .payload
            .original_bytes;
        let prepared = PreparedTools::semantic(&payload).unwrap();
        assert_eq!(prepared.definitions.len(), 2);
        let a = &prepared.definitions[&(Some("A".into()), None)];
        let b = &prepared.definitions[&(Some("B".into()), None)];
        assert_eq!((a.count, b.count), (3, 1));
        let mut expected = Weight::default();
        expected.add(i128::from(container) * 2, 3).unwrap();
        expected.add(single.into(), 1).unwrap();
        assert_eq!(a.weight, expected);
        let mut total = a.weight;
        total.add(b.weight.num, b.weight.den).unwrap();
        assert_eq!(
            total,
            Weight {
                num: i128::from(container + single),
                den: 1
            }
        );
    }

    #[tokio::test]
    async fn result_before_call_revises_dependents_in_a_file_backed_snapshot() {
        let fixture = crate::db::tests::FileDatabase::new().await;
        let db = &fixture.database;
        owners(db).await;
        let request = capture(db, "a", "p", body(vec![result()])).await;
        let surrogate = id(
            db,
            "SELECT id FROM gateway_analytics_requests WHERE request_id = ?",
            vec![request.clone().into()],
        )
        .await
        .unwrap();
        let before = revision(db, &request).await;
        let reader = crate::db::reporting_connection(&fixture.url, db)
            .await
            .unwrap();
        let snapshot = reader.begin().await.unwrap();
        assert_eq!(number(&snapshot, "SELECT COUNT(*) n FROM gateway_analytics_tool_identities WHERE state = 'unresolved'").await, 1);
        capture(db, "a", "p", body(vec![call("Read")])).await;
        assert_eq!(number(&snapshot, "SELECT COUNT(*) n FROM gateway_analytics_tool_identities WHERE state = 'unresolved'").await, 1);
        assert_eq!(revision(&snapshot, &request).await, before);
        assert_eq!(
            snapshot_contributions(&snapshot, surrogate).await.unwrap()[0].tool_name,
            None
        );
        snapshot.commit().await.unwrap();
        let after = revision(db, &request).await;
        assert!(after.0 > before.0);
        assert_eq!(after.1, before.1);
        assert_eq!(number(db, "SELECT COUNT(*) n FROM gateway_analytics_tool_contributions c JOIN gateway_analytics_tools t ON t.id = c.tool_id WHERE t.tool_name = 'Read'").await, 1);
        assert_eq!(
            number(
                db,
                "SELECT COUNT(*) n FROM gateway_analytics_requests WHERE contributions_dirty = 1"
            )
            .await,
            1
        );
        assert_eq!(
            snapshot_contributions(&db.begin().await.unwrap(), surrogate)
                .await
                .unwrap()[0]
                .tool_name
                .as_deref(),
            Some("Read")
        );
        assert_eq!(
            number(
                db,
                "SELECT COUNT(*) n FROM gateway_analytics_requests WHERE contributions_dirty = 1"
            )
            .await,
            1
        );
        reader.close().await.unwrap();
    }

    #[tokio::test]
    async fn scopes_isolate_owners_providers_and_missing_owners() {
        let db = crate::request_metrics::tests::database().await;
        owners(&db).await;
        capture(&db, "a", "p", body(vec![call("A")])).await;
        capture(&db, "b", "p", body(vec![call("B")])).await;
        capture(&db, "a", "q", body(vec![call("C")])).await;
        capture(&db, "missing", "p", body(vec![call("D")])).await;
        capture(&db, "missing", "p", body(vec![result()])).await;
        assert_eq!(
            number(
                &db,
                "SELECT COUNT(*) n FROM gateway_analytics_tool_identities"
            )
            .await,
            5
        );
        assert_eq!(
            number(
                &db,
                "SELECT COUNT(*) n FROM gateway_analytics_tool_identities WHERE state = 'ambiguous'"
            )
            .await,
            0
        );
        assert_eq!(number(&db, "SELECT COUNT(*) n FROM gateway_analytics_tool_identities WHERE state = 'unresolved'").await, 1);
    }

    #[tokio::test]
    async fn conflicting_pairs_are_ambiguous_and_replacement_can_resolve_again() {
        let db = crate::request_metrics::tests::database().await;
        owners(&db).await;
        let r = capture(&db, "a", "p", body(vec![result()])).await;
        capture(&db, "a", "p", body(vec![call("Read")])).await;
        let conflicting = capture(&db, "a", "p", body(vec![call("Write")])).await;
        assert_eq!(number(&db, "SELECT COUNT(*) n FROM gateway_analytics_tool_identities WHERE state = 'ambiguous' AND tool_id IS NULL").await, 1);
        assert_eq!(number(&db, "SELECT COUNT(*) n FROM gateway_analytics_tool_contributions c JOIN gateway_analytics_tools t ON t.id = c.tool_id WHERE t.tool_name IS NULL").await, 2);
        let surrogate = id(
            &db,
            "SELECT id FROM gateway_analytics_requests WHERE request_id = ?",
            vec![r.clone().into()],
        )
        .await
        .unwrap();
        assert_eq!(
            snapshot_contributions(&db.begin().await.unwrap(), surrogate)
                .await
                .unwrap()[0]
                .tool_name,
            None
        );
        assert_eq!(
            number(
                &db,
                "SELECT COUNT(*) n FROM gateway_analytics_requests WHERE contributions_dirty = 1"
            )
            .await,
            2
        );
        db.execute_raw(sql("UPDATE gateway_analytics_revisions SET first_pending_revision = NULL WHERE request_id = ?", vec![r.clone().into()])).await.unwrap();
        let before = revision(&db, &r).await;
        replace(&db, &conflicting, json!({})).await;
        let after = revision(&db, &r).await;
        assert!(after.0 > before.0);
        assert_eq!(after.1, Some(after.0));
        assert_eq!(
            number(
                &db,
                "SELECT COUNT(*) n FROM gateway_analytics_tool_identities WHERE state = 'resolved'"
            )
            .await,
            1
        );
        assert_eq!(number(&db, "SELECT COUNT(*) n FROM gateway_analytics_tool_contributions c JOIN gateway_analytics_tools t ON t.id = c.tool_id WHERE t.tool_name = 'Read'").await, 1);
        assert_eq!(
            snapshot_contributions(&db.begin().await.unwrap(), surrogate)
                .await
                .unwrap()[0]
                .tool_name
                .as_deref(),
            Some("Read")
        );
    }

    #[tokio::test]
    async fn withdrawing_the_last_call_keeps_an_unresolved_dimension_for_readers() {
        let db = crate::request_metrics::tests::database().await;
        owners(&db).await;
        let call_request = capture(&db, "a", "p", body(vec![call("Read")])).await;
        let result_request = capture(&db, "a", "p", body(vec![result()])).await;
        replace(&db, &call_request, json!({})).await;
        let snapshot = db.begin().await.unwrap();
        let surrogate = id(
            &snapshot,
            "SELECT id FROM gateway_analytics_requests WHERE request_id = ?",
            vec![result_request.into()],
        )
        .await
        .unwrap();
        let facts = snapshot_contributions(&snapshot, surrogate).await.unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].tool_name, None);
        assert_eq!(facts[0].skill_name, None);
        assert_eq!(number(&snapshot, "SELECT COUNT(*) n FROM gateway_analytics_tools WHERE attribution_key = '[null,null]'").await, 1);
        assert_eq!(
            number(
                &snapshot,
                "SELECT COUNT(*) n FROM gateway_analytics_requests WHERE contributions_dirty = 1"
            )
            .await,
            1
        );
        snapshot.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn skill_pairs_conflict_and_null_is_not_empty() {
        let db = crate::request_metrics::tests::database().await;
        owners(&db).await;
        let mut one = call("Skill");
        one["input"] = json!({"skill":"one"});
        let mut two = call("Skill");
        two["input"] = json!({"skill":"two"});
        capture(&db, "a", "p", body(vec![one, two])).await;
        assert_eq!(
            number(
                &db,
                "SELECT COUNT(*) n FROM gateway_analytics_tool_identities WHERE state = 'ambiguous'"
            )
            .await,
            1
        );
        let tx = begin_immediate(&db).await.unwrap();
        assert_ne!(
            dimension(&tx, &(None, None)).await.unwrap(),
            dimension(&tx, &(Some(String::new()), None)).await.unwrap()
        );
        assert_ne!(
            dimension(&tx, &(Some("Skill".into()), None)).await.unwrap(),
            dimension(&tx, &(Some("Skill".into()), Some(String::new())))
                .await
                .unwrap()
        );
        tx.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn missing_ids_use_tagged_hash_and_replays_keep_multiplicity() {
        let db = crate::request_metrics::tests::database().await;
        owners(&db).await;
        let mut missing = call("Read");
        missing.as_object_mut().unwrap().remove("id");
        let payload = split_request(
            &serde_json::to_vec(&body(vec![missing.clone()])).unwrap(),
            Provider::Anthropic.protocol(),
        )
        .unwrap();
        let hash = payload
            .parts
            .iter()
            .find(|p| p.kind == "tool_use")
            .unwrap()
            .payload
            .id
            .clone();
        let mut literal = call("Read");
        literal["id"] = hash.into();
        capture(&db, "a", "p", body(vec![missing.clone(), missing, literal])).await;
        assert_eq!(
            number(
                &db,
                "SELECT COUNT(*) n FROM gateway_analytics_tool_identities"
            )
            .await,
            2
        );
        assert_eq!(number(&db, "SELECT COUNT(*) n FROM gateway_analytics_tool_identities WHERE identity_kind = 'content_hash'").await, 1);
        assert_eq!(
            number(
                &db,
                "SELECT MAX(multiplicity) n FROM gateway_analytics_tool_appearances"
            )
            .await,
            2
        );
    }

    #[tokio::test]
    async fn definition_only_capture_stores_exact_weights_without_appearances() {
        let db = crate::request_metrics::tests::database().await;
        capture(
            &db,
            "missing",
            "p",
            json!({"tools":[{"name":"Read"},{"name":"Read"}]}),
        )
        .await;
        assert_eq!(
            number(
                &db,
                "SELECT COUNT(*) n FROM gateway_analytics_tool_contributions"
            )
            .await,
            1
        );
        assert_eq!(
            number(
                &db,
                "SELECT definition_count n FROM gateway_analytics_tool_contributions"
            )
            .await,
            2
        );
        assert_eq!(
            number(
                &db,
                "SELECT COUNT(*) n FROM gateway_analytics_tool_appearances"
            )
            .await,
            0
        );
        assert_eq!(
            number(
                &db,
                "SELECT CAST(definition_num AS INTEGER) n FROM gateway_analytics_tool_contributions"
            )
            .await,
            2 * serde_json::to_vec(&json!({"name":"Read"})).unwrap().len() as i64
        );
    }

    #[tokio::test]
    async fn replacement_failure_rolls_back_facts_metrics_and_revisions() {
        let fixture = crate::db::tests::FileDatabase::new().await;
        let db = &fixture.database;
        owners(db).await;
        let request = capture(db, "a", "p", body(vec![call("Read")])).await;
        let before = revision(db, &request).await;
        let metric = crate::request_metrics::tests::metrics(db, &request).await;
        let prepared = PreparedTools::semantic(
            &split_request(
                &serde_json::to_vec(&body(vec![call("Write")])).unwrap(),
                Provider::Anthropic.protocol(),
            )
            .unwrap(),
        )
        .unwrap();
        db.execute_unprepared("CREATE TRIGGER reject_tool_facts BEFORE INSERT ON gateway_analytics_tool_appearances BEGIN SELECT RAISE(ABORT, 'injected'); END").await.unwrap();
        let tx = begin_immediate(db).await.unwrap();
        tx.execute_raw(sql(
            "DELETE FROM gateway_request_metrics WHERE request_id = ?",
            vec![request.clone().into()],
        ))
        .await
        .unwrap();
        assert!(prepared.store(&tx, &request).await.is_err());
        tx.rollback().await.unwrap();
        assert_eq!(revision(db, &request).await, before);
        assert_eq!(
            crate::request_metrics::tests::metrics(db, &request).await,
            metric
        );
        assert_eq!(number(db, "SELECT COUNT(*) n FROM gateway_analytics_tool_contributions c JOIN gateway_analytics_tools t ON t.id = c.tool_id WHERE t.tool_name = 'Read'").await, 1);
        assert_eq!(
            number(
                db,
                "SELECT COUNT(*) n FROM gateway_analytics_tool_appearances"
            )
            .await,
            1
        );
    }

    #[tokio::test]
    async fn empty_and_unsplit_requests_have_empty_facts() {
        let db = crate::request_metrics::tests::database().await;
        for bytes in [b"".as_slice(), b"not json".as_slice()] {
            crate::request_metrics::tests::started(&db, Provider::Anthropic, bytes).await;
        }
        assert_eq!(
            number(&db, "SELECT COUNT(*) n FROM gateway_analytics_requests").await,
            2
        );
        assert_eq!(
            number(
                &db,
                "SELECT COUNT(*) n FROM gateway_analytics_tool_contributions"
            )
            .await,
            0
        );
    }

    #[test]
    fn rational_overflow_is_an_error_not_float_or_saturation() {
        let mut w = Weight {
            num: i128::MAX,
            den: 1,
        };
        assert!(w.add(1, 1).is_err());
        assert_eq!(w.num, i128::MAX);
    }

    #[tokio::test]
    async fn creation_tool_failure_rolls_back_the_whole_capture() {
        let db = crate::request_metrics::tests::database().await;
        db.execute_unprepared("CREATE TRIGGER reject_capture_tools BEFORE INSERT ON gateway_analytics_tool_appearances BEGIN SELECT RAISE(ABORT, 'injected'); END").await.unwrap();
        let bytes = serde_json::to_vec(&body(vec![call("Read")])).unwrap();
        let result = SqliteSink::new(db.clone())
            .start(StartRecord {
                request_id: "external",
                key_id: "missing",
                key_version_id: "v",
                provider_id: "p",
                provider: Provider::Anthropic,
                method: "POST",
                endpoint: "/messages",
                requested_model: None,
                request_body: &bytes,
            })
            .await;
        assert!(result.is_err());
        for table in [
            "gateway_requests",
            "gateway_request_metrics",
            "gateway_analytics_requests",
            "gateway_analytics_tool_identities",
            "gateway_analytics_tool_contributions",
            "gateway_analytics_revisions",
            "gateway_payload_blobs",
        ] {
            assert_eq!(
                number(&db, &format!("SELECT COUNT(*) n FROM {table}")).await,
                0,
                "{table}"
            );
        }
    }

    #[tokio::test]
    async fn many_dependents_are_invalidated_without_contribution_writes() {
        let fixture = crate::db::tests::FileDatabase::new().await;
        let db = &fixture.database;
        owners(db).await;
        for _ in 0..64 {
            capture(db, "a", "p", json!({"tools":[{"name":"Read"}],"messages":[{"role":"user","content":[result()]}]})).await;
        }
        for event in ["INSERT", "UPDATE", "DELETE"] {
            let row = if event == "DELETE" { "OLD" } else { "NEW" };
            db.execute_unprepared(&format!("CREATE TRIGGER reject_dependent_{event} BEFORE {event} ON gateway_analytics_tool_contributions WHEN {row}.request_id <= 64 BEGIN SELECT RAISE(ABORT, 'dependent cache rewritten'); END")).await.unwrap();
        }
        capture(db, "a", "p", body(vec![call("Read")])).await;
        assert_eq!(
            number(
                db,
                "SELECT COUNT(*) n FROM gateway_analytics_requests WHERE contributions_dirty = 1"
            )
            .await,
            64
        );
        assert_eq!(
            number(
                db,
                "SELECT COUNT(DISTINCT revision) n FROM gateway_analytics_revisions"
            )
            .await,
            1
        );
        for surrogate in 1..=64 {
            let facts = snapshot_contributions(&db.begin().await.unwrap(), surrogate)
                .await
                .unwrap();
            assert_eq!(facts.len(), 1);
            assert_eq!(facts[0].tool_name.as_deref(), Some("Read"));
            assert_eq!(facts[0].definition_count, 1);
            assert_eq!(
                facts[0].definition_weight.num,
                serde_json::to_vec(&json!({"name":"Read"})).unwrap().len() as i128
            );
            assert_eq!(
                facts[0].transmission_weight.num,
                serde_json::to_vec(&result()).unwrap().len() as i128
            );
            assert_eq!(facts[0].transmission_weight.den, 1);
        }
        assert_eq!(
            number(
                db,
                "SELECT COUNT(*) n FROM gateway_analytics_requests WHERE contributions_dirty = 1"
            )
            .await,
            64
        );
    }

    #[tokio::test]
    async fn deletion_with_appearances_is_blocked_but_definitions_can_leave_tombstones() {
        let db = crate::request_metrics::tests::database().await;
        let request = capture(&db, "missing", "p", body(vec![call("Read")])).await;
        let before = revision(&db, &request).await;
        let clock = number(&db, "SELECT revision n FROM gateway_analytics_clock").await;
        let error = db
            .execute_raw(sql(
                "DELETE FROM gateway_requests WHERE id = ?",
                vec![request.clone().into()],
            ))
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("analytics fact withdrawal required")
        );
        assert_eq!(revision(&db, &request).await, before);
        assert_eq!(
            number(&db, "SELECT revision n FROM gateway_analytics_clock").await,
            clock
        );
        assert_eq!(
            number(
                &db,
                "SELECT COUNT(*) n FROM gateway_analytics_tool_identities WHERE state = 'resolved'"
            )
            .await,
            1
        );
        let definition = capture(&db, "missing", "p", json!({"tools":[{"name":"Read"}]})).await;
        db.execute_raw(sql(
            "DELETE FROM gateway_requests WHERE id = ?",
            vec![definition.into()],
        ))
        .await
        .unwrap();
        assert_eq!(
            number(
                &db,
                "SELECT COUNT(*) n FROM gateway_analytics_revisions WHERE deleted = 1"
            )
            .await,
            1
        );
        assert_eq!(
            number(
                &db,
                "SELECT SUM(definition_count) n FROM gateway_analytics_tool_contributions"
            )
            .await,
            1
        );
    }

    #[tokio::test]
    async fn clock_exhaustion_rolls_back_replacement() {
        let db = crate::request_metrics::tests::database().await;
        let request = capture(&db, "missing", "p", body(vec![call("Read")])).await;
        db.execute_unprepared(
            "UPDATE gateway_analytics_clock SET revision = 9223372036854775807 WHERE id = 1",
        )
        .await
        .unwrap();
        let tx = begin_immediate(&db).await.unwrap();
        assert!(PreparedTools::default().store(&tx, &request).await.is_err());
        tx.rollback().await.unwrap();
        assert_eq!(
            number(
                &db,
                "SELECT COUNT(*) n FROM gateway_analytics_tool_appearances"
            )
            .await,
            1
        );
    }
}
