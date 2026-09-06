#![allow(dead_code)]
//! Read-only reports over hourly aggregates and compact boundary facts.
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Duration, SecondsFormat, Timelike, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, DatabaseTransaction, TransactionTrait, Value};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

use super::{
    aggregates::{CONTEXT_COMPONENTS, dimension_key},
    arithmetic::{
        Interval, ReportBudget, allocate_weight, parse_rational, tool_allocations, truncate_to_i64,
    },
    sqlite::sql,
};
use crate::usage::{
    ContextTotals, LabeledSeries, SkillCalls, ToolCalls, ToolUsage, TotalsSeries, UsageGroup,
    UsageTotals, Window,
};
use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::Zero;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RequestInterval {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub end_inclusive: bool,
}

/// Exact partition of an inclusive request-start window. `full_hours` is
/// half-open. Boundary intervals never overlap it or each other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WindowPlan {
    pub boundaries: Vec<RequestInterval>,
    pub full_hours: std::ops::Range<DateTime<Utc>>,
}

impl WindowPlan {
    pub(crate) fn new(window: Window) -> Result<Self> {
        ensure!(
            window.start <= window.end,
            "report window starts after it ends"
        );
        let floor = |value: DateTime<Utc>| {
            value
                .with_minute(0)
                .expect("valid minute")
                .with_second(0)
                .expect("valid second")
                .with_nanosecond(0)
                .expect("valid nanosecond")
        };
        let start_floor = floor(window.start);
        let first_full = if window.start == start_floor {
            start_floor
        } else {
            start_floor + Duration::hours(1)
        };
        let full_end = floor(window.end);
        if first_full > full_end {
            return Ok(Self {
                boundaries: vec![RequestInterval {
                    start: window.start,
                    end: window.end,
                    end_inclusive: true,
                }],
                full_hours: first_full..first_full,
            });
        }
        let mut boundaries = Vec::with_capacity(2);
        if window.start < first_full {
            boundaries.push(RequestInterval {
                start: window.start,
                end: first_full,
                end_inclusive: false,
            });
        }
        // The final exact endpoint belongs to compact facts. When `end` is not
        // aligned this interval also contains the partial final hour.
        boundaries.push(RequestInterval {
            start: full_end,
            end: window.end,
            end_inclusive: true,
        });
        Ok(Self {
            boundaries,
            full_hours: first_full..full_end,
        })
    }

    fn has_full_hours(&self) -> bool {
        self.full_hours.start < self.full_hours.end
    }
}

#[derive(Clone, Debug, Default)]
struct Additive {
    requests: i64,
    succeeded: i64,
    failed: i64,
    input: i64,
    cache_read: i64,
    cache_write: i64,
    output: i64,
    cost: i64,
    unpriced: i64,
}

impl Additive {
    fn add_row(&mut self, row: &sea_orm::QueryResult) -> Result<()> {
        macro_rules! add {
            ($field:ident, $column:literal) => {
                self.$field = self
                    .$field
                    .checked_add(row.try_get::<i64>("", $column)?)
                    .context(concat!($column, " overflow"))?;
            };
        }
        add!(requests, "requests");
        add!(succeeded, "succeeded");
        add!(failed, "failed");
        add!(input, "input_tokens");
        add!(cache_read, "cache_read_tokens");
        add!(cache_write, "cache_write_tokens");
        add!(output, "output_tokens");
        add!(cost, "cost_nanos");
        add!(unpriced, "unpriced");
        Ok(())
    }
    fn tokens(&self) -> Result<i64> {
        self.input
            .checked_add(self.cache_read)
            .and_then(|v| v.checked_add(self.cache_write))
            .and_then(|v| v.checked_add(self.output))
            .context("token total overflow")
    }
}

#[derive(Default)]
struct ContextAccum {
    requests: i64,
    bytes: [i64; 8],
    total_bytes: i64,
    tools_offered: i64,
    tools_invoked: i64,
    tool_result_errors: i64,
    cache_breakpoints: i64,
    costs: [Interval; 8],
    unpriced: [i64; 8],
}

impl ContextAccum {
    fn add_counts(&mut self, row: &sea_orm::QueryResult) -> Result<()> {
        self.requests = checked_row_add(self.requests, row, "requests")?;
        for (index, (_, column)) in CONTEXT_COMPONENTS.iter().enumerate() {
            self.bytes[index] = checked_row_add(self.bytes[index], row, column)?;
        }
        self.total_bytes = checked_row_add(self.total_bytes, row, "total_bytes")?;
        self.tools_offered = checked_row_add(self.tools_offered, row, "tools_offered")?;
        self.tools_invoked = checked_row_add(self.tools_invoked, row, "tools_invoked")?;
        self.tool_result_errors =
            checked_row_add(self.tool_result_errors, row, "tool_result_errors")?;
        self.cache_breakpoints = checked_row_add(self.cache_breakpoints, row, "cache_breakpoints")?;
        Ok(())
    }

    fn add_request(&mut self, row: &sea_orm::QueryResult, budget: &mut ReportBudget) -> Result<()> {
        self.requests = self
            .requests
            .checked_add(1)
            .context("context request count overflow")?;
        let total_bytes: i64 = row.try_get("", "total_bytes")?;
        self.total_bytes = self
            .total_bytes
            .checked_add(total_bytes)
            .context("context total bytes overflow")?;
        self.tools_offered = checked_row_add(self.tools_offered, row, "tools_offered")?;
        self.tools_invoked = checked_row_add(self.tools_invoked, row, "tools_invoked")?;
        self.tool_result_errors =
            checked_row_add(self.tool_result_errors, row, "tool_result_errors")?;
        self.cache_breakpoints = checked_row_add(self.cache_breakpoints, row, "cache_breakpoints")?;
        let cost: Option<i64> = row.try_get("", "cost_nanos")?;
        for (index, (_, column)) in CONTEXT_COMPONENTS.iter().enumerate() {
            let bytes: i64 = row.try_get("", column)?;
            self.bytes[index] = self.bytes[index]
                .checked_add(bytes)
                .context("context component bytes overflow")?;
            budget.consume_exact_terms(1)?;
            match allocate_weight(cost, &bytes.to_string(), "1", total_bytes)? {
                Some(value) => self.costs[index].add(&Interval::from_rational(&value)),
                None => {
                    self.unpriced[index] = self.unpriced[index]
                        .checked_add(1)
                        .context("context unknown-price count overflow")?;
                }
            }
        }
        Ok(())
    }

    fn finish(self, costs: [i64; 8]) -> Result<ContextTotals> {
        // Every component of a request shares that request's price, so an
        // unpriced request is counted once per component and the eight counts
        // must agree.
        let unpriced_requests = self.unpriced[0];
        ensure!(
            self.unpriced
                .iter()
                .all(|count| *count == unpriced_requests),
            "context unknown-price counts disagree across components"
        );
        Ok(ContextTotals {
            requests: self.requests,
            unpriced_requests,
            tool_definition_bytes: self.bytes[0],
            system_bytes: self.bytes[1],
            user_text_bytes: self.bytes[2],
            assistant_text_bytes: self.bytes[3],
            thinking_bytes: self.bytes[4],
            tool_use_bytes: self.bytes[5],
            tool_result_bytes: self.bytes[6],
            other_bytes: self.bytes[7],
            total_bytes: self.total_bytes,
            tools_offered: self.tools_offered,
            tools_invoked: self.tools_invoked,
            tool_result_errors: self.tool_result_errors,
            cache_breakpoints: self.cache_breakpoints,
            tool_definition_cost_nanodollars: costs[0],
            system_cost_nanodollars: costs[1],
            user_text_cost_nanodollars: costs[2],
            assistant_text_cost_nanodollars: costs[3],
            thinking_cost_nanodollars: costs[4],
            tool_use_cost_nanodollars: costs[5],
            tool_result_cost_nanodollars: costs[6],
            other_cost_nanodollars: costs[7],
        })
    }
}

fn checked_row_add(current: i64, row: &sea_orm::QueryResult, column: &str) -> Result<i64> {
    current
        .checked_add(row.try_get::<i64>("", column)?)
        .with_context(|| format!("{column} overflow"))
}

fn context_component_index(component: &str) -> Result<usize> {
    CONTEXT_COMPONENTS
        .iter()
        .position(|(name, _)| *name == component)
        .ok_or_else(|| anyhow::anyhow!("unknown context aggregate component {component}"))
}

#[derive(Clone, Debug, Default)]
struct ToolAccum {
    definition_bytes: BigRational,
    definition_cost: Interval,
    transmission_cost: Interval,
    calls: i64,
    appearance_bytes: i64,
    unpriced: i64,
}

/// How one reported tool row is identified. `tools.tool_name` is nullable, so
/// an unnamed tool is kept apart from the bucket for results whose call was
/// never captured. Both report a null label; neither absorbs the other.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ToolRow {
    Named(String),
    Unnamed(i64),
    Uncaptured,
}

impl ToolRow {
    fn label(self) -> Option<String> {
        match self {
            Self::Named(label) => Some(label),
            Self::Unnamed(_) | Self::Uncaptured => None,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct IdentityAccum {
    assignments: BTreeSet<Option<i64>>,
    /// Set by the `ambiguous` attribution state, which is ambiguous even when
    /// it is the identity's only row.
    ambiguous: bool,
    /// Occurrences of this call in the request that carried the most of them.
    /// Replaying a conversation repeats the same occurrences, so they are
    /// maximized across requests rather than summed.
    calls: i64,
    call_bytes: i64,
    result_bytes: i64,
}

pub struct Snapshot {
    tx: DatabaseTransaction,
}

impl Snapshot {
    /// Opens the one read transaction every report of this snapshot runs in.
    /// Reading the generation here both enforces the baseline and takes the WAL
    /// read snapshot, which a deferred `BEGIN` alone would defer to the first
    /// report. Pass `allow_incomplete_baseline` only for staging callers that
    /// accept partial data.
    pub async fn begin(
        database: &DatabaseConnection,
        allow_incomplete_baseline: bool,
    ) -> Result<Self> {
        let tx = database.begin().await?;
        let baseline_complete: bool = tx
            .query_one_raw(sql(
                "SELECT baseline_complete FROM generation WHERE singleton=1",
                vec![],
            ))
            .await?
            .context("missing analytics generation")?
            .try_get("", "baseline_complete")?;
        ensure!(
            baseline_complete || allow_incomplete_baseline,
            "analytics generation baseline is incomplete; staging reports are not dashboard-ready"
        );
        Ok(Self { tx })
    }
    pub async fn commit(self) -> Result<()> {
        self.tx.commit().await?;
        Ok(())
    }

    pub async fn totals(&self, owner: Uuid, window: Window) -> Result<UsageTotals> {
        let plan = WindowPlan::new(window)?;
        let mut budget = ReportBudget::default();
        let mut total = Additive::default();
        for row in self.overview_rows(owner, &plan, &mut budget).await? {
            total.add_row(&row)?;
        }
        budget.consume_groups(1)?;
        Ok(UsageTotals {
            requests: total.requests,
            succeeded: total.succeeded,
            failed: total.failed,
            input_tokens: total.input,
            cache_read_tokens: total.cache_read,
            cache_write_tokens: total.cache_write,
            output_tokens: total.output,
            cost_nanodollars: total.cost,
            unpriced: total.unpriced,
        })
    }

    pub async fn context(&self, owner: Uuid, window: Window) -> Result<ContextTotals> {
        let plan = WindowPlan::new(window)?;
        let mut budget = ReportBudget::default();
        let owner_id = owner.to_string();
        let owner_key = dimension_key(Some(&owner_id));
        let mut totals = ContextAccum::default();

        if plan.has_full_hours() {
            let rows = self.tx.query_all_raw(sql(
                "SELECT requests,tool_definition_bytes,system_bytes,user_text_bytes,assistant_text_bytes,thinking_bytes,tool_use_bytes,tool_result_bytes,other_bytes,total_bytes,tools_offered,tools_invoked,tool_result_errors,cache_breakpoints FROM hourly_context WHERE owner_key=? AND hour>=? AND hour<?",
                vec![owner_key.clone().into(), hour_bound(plan.full_hours.start).into(), hour_bound(plan.full_hours.end).into()],
            )).await?;
            budget.consume_rows(rows.len())?;
            for row in rows {
                totals.add_counts(&row)?;
            }
            let rows = self.tx.query_all_raw(sql(
                "SELECT component,lower_scaled,remainders,unpriced FROM hourly_context_cost WHERE owner_key=? AND hour>=? AND hour<?",
                vec![owner_key.into(), hour_bound(plan.full_hours.start).into(), hour_bound(plan.full_hours.end).into()],
            )).await?;
            budget.consume_rows(rows.len())?;
            for row in rows {
                let component: String = row.try_get("", "component")?;
                let index = context_component_index(&component)?;
                totals.costs[index].add(&Interval::from_decimal(
                    &row.try_get::<String>("", "lower_scaled")?,
                    &row.try_get::<String>("", "remainders")?,
                )?);
                totals.unpriced[index] = totals.unpriced[index]
                    .checked_add(row.try_get("", "unpriced")?)
                    .context("context unknown-price count overflow")?;
            }
        }

        let (predicate, mut values) = boundary_predicate(&plan, "r.started_at");
        if !predicate.is_empty() {
            values.insert(0, owner_id.clone().into());
            let rows = self.tx.query_all_raw(sql(
                &format!("SELECT c.*,u.cost_nanos FROM requests r JOIN context c ON c.request_id=r.id LEFT JOIN usage u ON u.request_id=r.id WHERE r.owner_id=? AND ({predicate})"),
                values,
            )).await?;
            budget.consume_rows(rows.len())?;
            for row in rows {
                totals.add_request(&row, &mut budget)?;
            }
        }
        budget.consume_groups(1)?;

        let mut costs = [0_i64; 8];
        let mut ambiguous = [false; 8];
        for index in 0..8 {
            match totals.costs[index].proven_i64()? {
                Some(value) => costs[index] = value,
                None => ambiguous[index] = true,
            }
        }
        if ambiguous.iter().any(|value| *value) {
            let rows = self.tx.query_all_raw(sql(
                "SELECT c.*,u.cost_nanos FROM requests r JOIN context c ON c.request_id=r.id LEFT JOIN usage u ON u.request_id=r.id WHERE r.owner_id=? AND r.started_at>=? AND r.started_at<=?",
                vec![owner_id.into(), timestamp(window.start).into(), timestamp(window.end).into()],
            )).await?;
            budget.consume_rows(rows.len())?;
            let mut exact: [BigRational; 8] = std::array::from_fn(|_| BigRational::zero());
            for row in rows {
                let cost: Option<i64> = row.try_get("", "cost_nanos")?;
                let total_bytes: i64 = row.try_get("", "total_bytes")?;
                for (index, (_, column)) in CONTEXT_COMPONENTS.iter().enumerate() {
                    if !ambiguous[index] {
                        continue;
                    }
                    budget.consume_exact_terms(1)?;
                    let bytes: i64 = row.try_get("", column)?;
                    if let Some(value) =
                        allocate_weight(cost, &bytes.to_string(), "1", total_bytes)?
                    {
                        exact[index] += value;
                    }
                }
            }
            for index in 0..8 {
                if ambiguous[index] {
                    costs[index] = truncate_to_i64(&exact[index])?;
                }
            }
        }
        totals.finish(costs)
    }

    pub async fn tool_usage(&self, owner: Uuid, window: Window) -> Result<ToolUsage> {
        let plan = WindowPlan::new(window)?;
        let owner_id = owner.to_string();
        let owner_key = dimension_key(Some(&owner_id));
        let mut budget = ReportBudget::default();
        let mut groups: BTreeMap<Option<i64>, ToolAccum> = BTreeMap::new();
        let mut metadata: BTreeMap<i64, (Option<String>, Option<String>)> = BTreeMap::new();

        if plan.has_full_hours() {
            let rows = self.tx.query_all_raw(sql(
                "SELECT h.*,t.tool_name,t.skill_name FROM hourly_tool_contribution h JOIN tools t ON t.id=h.tool_id WHERE h.owner_key=? AND h.hour>=? AND h.hour<?",
                vec![owner_key.clone().into(), hour_bound(plan.full_hours.start).into(), hour_bound(plan.full_hours.end).into()],
            )).await?;
            budget.consume_rows(rows.len())?;
            for row in rows {
                let tool_id: i64 = row.try_get("", "tool_id")?;
                metadata.insert(
                    tool_id,
                    (
                        row.try_get("", "tool_name")?,
                        row.try_get("", "skill_name")?,
                    ),
                );
                let group = groups.entry(Some(tool_id)).or_default();
                group.definition_bytes += parse_rational(
                    &row.try_get::<String>("", "definition_bytes_num")?,
                    &row.try_get::<String>("", "definition_bytes_den")?,
                )?;
                group.definition_cost.add(&Interval::from_decimal(
                    &row.try_get::<String>("", "definition_lower_scaled")?,
                    &row.try_get::<String>("", "definition_remainders")?,
                )?);
                group.transmission_cost.add(&Interval::from_decimal(
                    &row.try_get::<String>("", "transmission_lower_scaled")?,
                    &row.try_get::<String>("", "transmission_remainders")?,
                )?);
                group.unpriced = group
                    .unpriced
                    .checked_add(row.try_get("", "unpriced")?)
                    .context("tool unknown-price count overflow")?;
            }
        }

        let (predicate, mut values) = boundary_predicate(&plan, "r.started_at");
        if !predicate.is_empty() {
            values.insert(0, owner_id.clone().into());
            let rows = self.tx.query_all_raw(sql(
                &format!("SELECT c.*,u.cost_nanos,x.total_bytes,t.tool_name,t.skill_name FROM requests r JOIN contributions c ON c.request_id=r.id JOIN context x ON x.request_id=r.id LEFT JOIN usage u ON u.request_id=r.id JOIN tools t ON t.id=c.tool_id WHERE r.owner_id=? AND ({predicate})"),
                values,
            )).await?;
            budget.consume_rows(rows.len())?;
            for row in rows {
                let tool_id: i64 = row.try_get("", "tool_id")?;
                metadata.insert(
                    tool_id,
                    (
                        row.try_get("", "tool_name")?,
                        row.try_get("", "skill_name")?,
                    ),
                );
                let group = groups.entry(Some(tool_id)).or_default();
                let definition_num: String = row.try_get("", "definition_num")?;
                let definition_den: String = row.try_get("", "definition_den")?;
                let transmission_num: String = row.try_get("", "transmission_num")?;
                let transmission_den: String = row.try_get("", "transmission_den")?;
                group.definition_bytes += parse_rational(&definition_num, &definition_den)?;
                let (definition, transmission) = tool_allocations(
                    row.try_get("", "cost_nanos")?,
                    row.try_get("", "total_bytes")?,
                    &definition_num,
                    &definition_den,
                    &transmission_num,
                    &transmission_den,
                    &mut budget,
                )?;
                match (definition, transmission) {
                    (Some(definition), Some(transmission)) => {
                        group
                            .definition_cost
                            .add(&Interval::from_rational(&definition));
                        group
                            .transmission_cost
                            .add(&Interval::from_rational(&transmission));
                    }
                    _ => {
                        let contributed = definition_num.parse::<BigInt>()? != BigInt::zero()
                            || transmission_num.parse::<BigInt>()? != BigInt::zero();
                        if contributed {
                            group.unpriced = group
                                .unpriced
                                .checked_add(1)
                                .context("tool unknown-price count overflow")?;
                        }
                    }
                }
            }
        }

        // Presence rows merge complete hours with boundary appearances and deduplicate
        // scoped identities across the selected window. They are a staleness guard, not
        // a data source: the scan below reads appearance detail from compact facts, and
        // an identity it finds that presence does not know means the aggregates no
        // longer match the facts, so the report fails instead of under-reporting.
        //
        // Reading the detail itself from aggregates would be sound — per-identity byte
        // and multiplicity maxima and the assignment set all decompose over hours — but
        // it needs columns the presence grain does not carry yet. Deferred to the
        // performance work rather than widened here.
        let mut identities = BTreeSet::new();
        if plan.has_full_hours() {
            let rows = self.tx.query_all_raw(sql(
                "SELECT identity_id FROM hourly_identity_presence WHERE owner_key=? AND hour>=? AND hour<?",
                vec![owner_key.into(), hour_bound(plan.full_hours.start).into(), hour_bound(plan.full_hours.end).into()],
            )).await?;
            budget.consume_rows(rows.len())?;
            for row in rows {
                identities.insert(row.try_get::<i64>("", "identity_id")?);
            }
        }
        let (predicate, mut values) = boundary_predicate(&plan, "r.started_at");
        if !predicate.is_empty() {
            values.insert(0, owner_id.clone().into());
            let rows = self.tx.query_all_raw(sql(
                &format!("SELECT DISTINCT v.identity_id FROM requests r JOIN appearances a ON a.request_id=r.id JOIN variants v ON v.id=a.variant_id WHERE r.owner_id=? AND ({predicate})"), values,
            )).await?;
            budget.consume_rows(rows.len())?;
            for row in rows {
                identities.insert(row.try_get::<i64>("", "identity_id")?);
            }
        }

        let rows = self.tx.query_all_raw(sql(
            // Attribution is left-joined: nothing ties an appearance to an
            // attribution row, and an inner join would drop an unattributed one
            // out of every bucket instead of reporting it as uncaptured.
            "SELECT v.identity_id,v.kind,v.bytes,a.multiplicity,ria.state,ria.tool_id,t.tool_name,t.skill_name FROM requests r JOIN appearances a ON a.request_id=r.id JOIN variants v ON v.id=a.variant_id LEFT JOIN request_identity_attribution ria ON ria.request_id=r.id AND ria.identity_id=v.identity_id LEFT JOIN tools t ON t.id=ria.tool_id WHERE r.owner_id=? AND r.started_at>=? AND r.started_at<=?",
            vec![owner_id.clone().into(), timestamp(window.start).into(), timestamp(window.end).into()],
        )).await?;
        budget.consume_rows(rows.len())?;
        let mut appearances: BTreeMap<i64, IdentityAccum> = BTreeMap::new();
        for row in rows {
            let identity_id: i64 = row.try_get("", "identity_id")?;
            ensure!(
                identities.contains(&identity_id),
                "appearance missing merged identity presence"
            );
            let state: Option<String> = row.try_get("", "state")?;
            let tool_id: Option<i64> = row.try_get("", "tool_id")?;
            let assignment = (state.as_deref() == Some("resolved"))
                .then_some(tool_id)
                .flatten();
            if let Some(tool_id) = assignment {
                metadata.insert(
                    tool_id,
                    (
                        row.try_get("", "tool_name")?,
                        row.try_get("", "skill_name")?,
                    ),
                );
            }
            let appearance = appearances.entry(identity_id).or_default();
            appearance.assignments.insert(assignment);
            appearance.ambiguous |= state.as_deref() == Some("ambiguous");
            let bytes: i64 = row.try_get("", "bytes")?;
            let multiplicity: i64 = row.try_get("", "multiplicity")?;
            match row.try_get::<String>("", "kind")?.as_str() {
                "tool_use" => {
                    appearance.calls = appearance.calls.max(multiplicity);
                    appearance.call_bytes = appearance.call_bytes.max(bytes);
                }
                "tool_result" => appearance.result_bytes = appearance.result_bytes.max(bytes),
                kind => bail!("unknown tool appearance kind {kind}"),
            }
        }
        budget.consume_groups(appearances.len())?;
        let mut ambiguous_calls = 0_i64;
        let mut ambiguous_bytes = 0_i64;
        for appearance in appearances.into_values() {
            let bytes = appearance
                .call_bytes
                .checked_add(appearance.result_bytes)
                .context("tool appearance bytes overflow")?;
            // Disagreement across requests is as ambiguous as the stored state:
            // charging either candidate would be a guess, so neither is charged.
            if appearance.ambiguous || appearance.assignments.len() > 1 {
                ambiguous_calls = ambiguous_calls
                    .checked_add(appearance.calls)
                    .context("ambiguous call count overflow")?;
                ambiguous_bytes = ambiguous_bytes
                    .checked_add(bytes)
                    .context("ambiguous appearance bytes overflow")?;
                continue;
            }
            let assignment = appearance.assignments.iter().next().copied().flatten();
            let group = groups.entry(assignment).or_default();
            group.calls = group
                .calls
                .checked_add(appearance.calls)
                .context("tool call count overflow")?;
            group.appearance_bytes = group
                .appearance_bytes
                .checked_add(bytes)
                .context("tool appearance bytes overflow")?;
        }
        budget.consume_groups(groups.len())?;

        // An overflow is an error, not an ambiguity: recomputing exactly would
        // not make the value fit.
        let mut ambiguous_tools = BTreeSet::new();
        for (tool, group) in &groups {
            if group.definition_cost.proven_i64()?.is_none()
                || group.transmission_cost.proven_i64()?.is_none()
            {
                ambiguous_tools.insert(*tool);
            }
        }
        let mut exact_costs: BTreeMap<Option<i64>, BigRational> = BTreeMap::new();
        if !ambiguous_tools.is_empty() {
            let rows = self.tx.query_all_raw(sql(
                "SELECT c.*,u.cost_nanos,x.total_bytes FROM requests r JOIN contributions c ON c.request_id=r.id JOIN context x ON x.request_id=r.id LEFT JOIN usage u ON u.request_id=r.id WHERE r.owner_id=? AND r.started_at>=? AND r.started_at<=?",
                vec![owner_id.into(), timestamp(window.start).into(), timestamp(window.end).into()],
            )).await?;
            budget.consume_rows(rows.len())?;
            for row in rows {
                let tool = Some(row.try_get::<i64>("", "tool_id")?);
                if !ambiguous_tools.contains(&tool) {
                    continue;
                }
                let (definition, transmission) = tool_allocations(
                    row.try_get("", "cost_nanos")?,
                    row.try_get("", "total_bytes")?,
                    &row.try_get::<String>("", "definition_num")?,
                    &row.try_get::<String>("", "definition_den")?,
                    &row.try_get::<String>("", "transmission_num")?,
                    &row.try_get::<String>("", "transmission_den")?,
                    &mut budget,
                )?;
                let exact = exact_costs.entry(tool).or_default();
                if let Some(value) = definition {
                    *exact += value;
                }
                if let Some(value) = transmission {
                    *exact += value;
                }
            }
        }

        let mut tools_by_label: BTreeMap<ToolRow, ToolCalls> = BTreeMap::new();
        let mut skills: BTreeMap<String, SkillCalls> = BTreeMap::new();
        for (tool_id, group) in groups {
            let (label, skill) = match tool_id {
                Some(id) => metadata.get(&id).cloned().unwrap_or_default(),
                None => (None, None),
            };
            let row = match (tool_id, label) {
                (_, Some(label)) => ToolRow::Named(label),
                (Some(id), None) => ToolRow::Unnamed(id),
                (None, None) => ToolRow::Uncaptured,
            };
            let cost = if let Some(exact) = exact_costs.get(&tool_id) {
                truncate_to_i64(exact)?
            } else {
                group
                    .definition_cost
                    .proven_i64()?
                    .context("ambiguous tool definition cost without fallback")?
                    .checked_add(
                        group
                            .transmission_cost
                            .proven_i64()?
                            .context("ambiguous tool transmission cost without fallback")?,
                    )
                    .context("tool cost overflow")?
            };
            let bytes = truncate_to_i64(&group.definition_bytes)?
                .checked_add(group.appearance_bytes)
                .context("tool bytes overflow")?;
            let tool = tools_by_label.entry(row).or_default();
            tool.calls = tool
                .calls
                .checked_add(group.calls)
                .context("tool call count overflow")?;
            tool.unpriced_requests = tool
                .unpriced_requests
                .checked_add(group.unpriced)
                .context("tool unknown-price count overflow")?;
            tool.bytes = tool
                .bytes
                .checked_add(bytes)
                .context("tool bytes overflow")?;
            tool.cost_nanodollars = tool
                .cost_nanodollars
                .checked_add(cost)
                .context("tool cost overflow")?;
            if let Some(skill) = skill {
                let value = skills.entry(skill).or_default();
                value.calls = value
                    .calls
                    .checked_add(group.calls)
                    .context("skill call count overflow")?;
                value.bytes = value
                    .bytes
                    .checked_add(bytes)
                    .context("skill bytes overflow")?;
                value.cost_nanodollars = value
                    .cost_nanodollars
                    .checked_add(cost)
                    .context("skill cost overflow")?;
                value.unpriced_requests = value
                    .unpriced_requests
                    .checked_add(group.unpriced)
                    .context("skill unknown-price count overflow")?;
            }
        }
        let mut tools = tools_by_label
            .into_iter()
            .map(|(row, value)| ToolCalls {
                label: row.label(),
                ..value
            })
            .collect::<Vec<_>>();
        tools.sort_by(|left, right| {
            right
                .calls
                .cmp(&left.calls)
                .then_with(|| left.label.cmp(&right.label))
        });
        let mut skills = skills
            .into_iter()
            .map(|(label, value)| SkillCalls { label, ..value })
            .collect::<Vec<_>>();
        skills.sort_by(|left, right| {
            right
                .calls
                .cmp(&left.calls)
                .then_with(|| left.label.cmp(&right.label))
        });
        Ok(ToolUsage {
            tools,
            skills,
            ambiguous_calls,
            ambiguous_bytes,
        })
    }

    pub async fn by_model(&self, owner: Uuid, window: Window) -> Result<Vec<UsageGroup>> {
        self.breakdown(Dimension::Model, owner, window).await
    }
    pub async fn by_provider(&self, owner: Uuid, window: Window) -> Result<Vec<UsageGroup>> {
        self.breakdown(Dimension::Provider, owner, window).await
    }
    pub async fn by_key(&self, owner: Uuid, window: Window) -> Result<Vec<UsageGroup>> {
        self.breakdown(Dimension::Key, owner, window).await
    }

    async fn breakdown(
        &self,
        dimension: Dimension,
        owner: Uuid,
        window: Window,
    ) -> Result<Vec<UsageGroup>> {
        let plan = WindowPlan::new(window)?;
        let mut budget = ReportBudget::default();
        let groups = self
            .grouped_rows(dimension, owner, &plan, &mut budget)
            .await?;
        budget.consume_groups(groups.len())?;
        let labels = self
            .key_labels(dimension, groups.keys(), &mut budget)
            .await?;
        let mut out = groups
            .into_iter()
            .map(|(key, value)| -> Result<_> {
                Ok(UsageGroup {
                    label: key.label(&labels),
                    requests: value.requests,
                    tokens: value.tokens()?,
                    cost_nanodollars: value.cost,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        out.sort_by(|a, b| {
            b.tokens
                .cmp(&a.tokens)
                .then_with(|| b.requests.cmp(&a.requests))
                .then_with(|| a.label.cmp(&b.label))
        });
        Ok(out)
    }

    pub async fn totals_series(&self, owner: Uuid, window: Window) -> Result<TotalsSeries> {
        let mut budget = ReportBudget::default();
        let data = self.series_groups(None, owner, window, &mut budget).await?;
        budget.consume_groups(data.len())?;
        let keys = window.bucket_keys();
        Ok(TotalsSeries {
            requests: keys
                .iter()
                .map(|k| {
                    data.get(&(GroupKey::Overview, k.clone()))
                        .map_or(0, |v| v.requests)
                })
                .collect(),
            tokens: keys
                .iter()
                .map(|k| {
                    data.get(&(GroupKey::Overview, k.clone()))
                        .map_or(Ok(0), Additive::tokens)
                })
                .collect::<Result<_>>()?,
            cost_nanodollars: keys
                .iter()
                .map(|k| {
                    data.get(&(GroupKey::Overview, k.clone()))
                        .map_or(0, |v| v.cost)
                })
                .collect(),
        })
    }
    pub async fn series_by_model(&self, owner: Uuid, window: Window) -> Result<Vec<LabeledSeries>> {
        self.labeled_series(Dimension::Model, owner, window).await
    }
    pub async fn series_by_provider(
        &self,
        owner: Uuid,
        window: Window,
    ) -> Result<Vec<LabeledSeries>> {
        self.labeled_series(Dimension::Provider, owner, window)
            .await
    }
    pub async fn series_by_key(&self, owner: Uuid, window: Window) -> Result<Vec<LabeledSeries>> {
        self.labeled_series(Dimension::Key, owner, window).await
    }

    async fn labeled_series(
        &self,
        dimension: Dimension,
        owner: Uuid,
        window: Window,
    ) -> Result<Vec<LabeledSeries>> {
        let mut budget = ReportBudget::default();
        let data = self
            .series_groups(Some(dimension), owner, window, &mut budget)
            .await?;
        budget.consume_groups(data.len())?;
        let keys = window.bucket_keys();
        let identities = data.keys().map(|(k, _)| k.clone()).collect::<BTreeSet<_>>();
        let labels = self
            .key_labels(dimension, identities.iter(), &mut budget)
            .await?;
        let mut out = Vec::new();
        for identity in identities {
            let tokens = keys
                .iter()
                .map(|bucket| {
                    data.get(&(identity.clone(), bucket.clone()))
                        .map_or(Ok(0), Additive::tokens)
                })
                .collect::<Result<_>>()?;
            out.push(LabeledSeries {
                label: identity.label(&labels),
                tokens,
            });
        }
        out.sort_by(|a, b| {
            b.total()
                .cmp(&a.total())
                .then_with(|| a.label.cmp(&b.label))
        });
        Ok(out)
    }

    async fn overview_rows(
        &self,
        owner: Uuid,
        plan: &WindowPlan,
        budget: &mut ReportBudget,
    ) -> Result<Vec<sea_orm::QueryResult>> {
        let mut rows = Vec::new();
        if plan.has_full_hours() {
            rows.extend(self.tx.query_all_raw(sql("SELECT requests,succeeded,failed,input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,cost_nanos,unpriced FROM hourly_owner_overview WHERE owner_key=? AND hour>=? AND hour<?", vec![dimension_key(Some(&owner.to_string())).into(), hour_bound(plan.full_hours.start).into(), hour_bound(plan.full_hours.end).into()])).await?);
        }
        let (predicate, mut values) = boundary_predicate(plan, "r.started_at");
        if !predicate.is_empty() {
            values.insert(0, owner.to_string().into());
            rows.extend(self.tx.query_all_raw(sql(&format!("SELECT COUNT(*) requests,COALESCE(SUM(CASE WHEN r.status<400 AND NOT r.has_error THEN 1 ELSE 0 END),0) succeeded,COALESCE(SUM(CASE WHEN r.status>=400 OR r.has_error THEN 1 ELSE 0 END),0) failed,COALESCE(SUM(u.input_tokens),0) input_tokens,COALESCE(SUM(u.cache_read_tokens),0) cache_read_tokens,COALESCE(SUM(u.cache_write_tokens),0) cache_write_tokens,COALESCE(SUM(u.output_tokens),0) output_tokens,COALESCE(SUM(u.cost_nanos),0) cost_nanos,COALESCE(SUM(u.cost_nanos IS NULL),0) unpriced FROM requests r LEFT JOIN usage u ON u.request_id=r.id WHERE r.owner_id=? AND ({predicate})"), values)).await?);
        }
        budget.consume_rows(rows.len())?;
        Ok(rows)
    }

    async fn grouped_rows(
        &self,
        dimension: Dimension,
        owner: Uuid,
        plan: &WindowPlan,
        budget: &mut ReportBudget,
    ) -> Result<BTreeMap<GroupKey, Additive>> {
        let mut out = BTreeMap::new();
        if plan.has_full_hours() {
            let rows = self.tx.query_all_raw(sql(&format!("SELECT dimension,requests,succeeded,failed,input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,cost_nanos,unpriced FROM {} WHERE owner_key=? AND hour>=? AND hour<?", dimension.table()), vec![dimension_key(Some(&owner.to_string())).into(),hour_bound(plan.full_hours.start).into(),hour_bound(plan.full_hours.end).into()])).await?;
            budget.consume_rows(rows.len())?;
            merge_group_rows(&mut out, dimension, rows)?;
        }
        let (predicate, mut values) = boundary_predicate(plan, "r.started_at");
        values.insert(0, owner.to_string().into());
        let rows = self.tx.query_all_raw(sql(&format!("SELECT {} dimension,COUNT(*) requests,COALESCE(SUM(CASE WHEN r.status<400 AND NOT r.has_error THEN 1 ELSE 0 END),0) succeeded,COALESCE(SUM(CASE WHEN r.status>=400 OR r.has_error THEN 1 ELSE 0 END),0) failed,COALESCE(SUM(u.input_tokens),0) input_tokens,COALESCE(SUM(u.cache_read_tokens),0) cache_read_tokens,COALESCE(SUM(u.cache_write_tokens),0) cache_write_tokens,COALESCE(SUM(u.output_tokens),0) output_tokens,COALESCE(SUM(u.cost_nanos),0) cost_nanos,COALESCE(SUM(u.cost_nanos IS NULL),0) unpriced FROM requests r LEFT JOIN usage u ON u.request_id=r.id WHERE r.owner_id=? AND ({predicate}) GROUP BY {}",dimension.column(),dimension.column()),values)).await?;
        budget.consume_rows(rows.len())?;
        merge_group_rows(&mut out, dimension, rows)?;
        Ok(out)
    }

    async fn series_groups(
        &self,
        dimension: Option<Dimension>,
        owner: Uuid,
        window: Window,
        budget: &mut ReportBudget,
    ) -> Result<BTreeMap<(GroupKey, String), Additive>> {
        let plan = WindowPlan::new(window)?;
        let mut out = BTreeMap::new();
        let table = dimension.map_or("hourly_owner_overview", Dimension::table);
        if plan.has_full_hours() {
            let dim = dimension.map_or("NULL", |_| "dimension");
            let rows=self.tx.query_all_raw(sql(&format!("SELECT {dim} dimension,hour,requests,succeeded,failed,input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,cost_nanos,unpriced FROM {table} WHERE owner_key=? AND hour>=? AND hour<?"),vec![dimension_key(Some(&owner.to_string())).into(),hour_bound(plan.full_hours.start).into(),hour_bound(plan.full_hours.end).into()])).await?;
            budget.consume_rows(rows.len())?;
            merge_series_rows(&mut out, dimension, window, rows)?;
        }
        let (predicate, mut values) = boundary_predicate(&plan, "r.started_at");
        values.insert(0, owner.to_string().into());
        let dim = dimension.map_or("NULL", Dimension::column);
        let bucket = window.bucket.sql();
        let rows=self.tx.query_all_raw(sql(&format!("SELECT {dim} dimension,{bucket} bucket,COUNT(*) requests,COALESCE(SUM(CASE WHEN r.status<400 AND NOT r.has_error THEN 1 ELSE 0 END),0) succeeded,COALESCE(SUM(CASE WHEN r.status>=400 OR r.has_error THEN 1 ELSE 0 END),0) failed,COALESCE(SUM(u.input_tokens),0) input_tokens,COALESCE(SUM(u.cache_read_tokens),0) cache_read_tokens,COALESCE(SUM(u.cache_write_tokens),0) cache_write_tokens,COALESCE(SUM(u.output_tokens),0) output_tokens,COALESCE(SUM(u.cost_nanos),0) cost_nanos,COALESCE(SUM(u.cost_nanos IS NULL),0) unpriced FROM requests r LEFT JOIN usage u ON u.request_id=r.id WHERE r.owner_id=? AND ({predicate}) GROUP BY dimension,bucket"),values)).await?;
        budget.consume_rows(rows.len())?;
        for row in rows {
            let key = group_key(dimension, row.try_get("", "dimension")?);
            let bucket = row.try_get("", "bucket")?;
            out.entry((key, bucket)).or_default().add_row(&row)?;
        }
        Ok(out)
    }

    async fn key_labels<'a>(
        &self,
        dimension: Dimension,
        keys: impl Iterator<Item = &'a GroupKey>,
        budget: &mut ReportBudget,
    ) -> Result<BTreeMap<String, String>> {
        if dimension != Dimension::Key {
            return Ok(BTreeMap::new());
        }
        let ids = keys
            .filter_map(|k| {
                if let GroupKey::Key(Some(id)) = k {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect::<BTreeSet<_>>();
        let mut labels = BTreeMap::new();
        for id in ids {
            let row = self
                .tx
                .query_one_raw(sql(
                    "SELECT name FROM keys WHERE id=?",
                    vec![id.clone().into()],
                ))
                .await?;
            budget.consume_rows(usize::from(row.is_some()))?;
            if let Some(row) = row {
                labels.insert(id, row.try_get("", "name")?);
            }
        }
        Ok(labels)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dimension {
    Model,
    Provider,
    Key,
}
impl Dimension {
    fn table(self) -> &'static str {
        match self {
            Self::Model => "hourly_owner_model",
            Self::Provider => "hourly_owner_provider",
            Self::Key => "hourly_owner_key",
        }
    }
    fn column(self) -> &'static str {
        match self {
            Self::Model => "r.requested_model",
            Self::Provider => "r.provider",
            Self::Key => "r.key_id",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum GroupKey {
    Overview,
    Text(Option<String>),
    Key(Option<String>),
}
impl GroupKey {
    fn label(&self, labels: &BTreeMap<String, String>) -> Option<String> {
        match self {
            Self::Overview => None,
            Self::Text(v) => v.clone(),
            Self::Key(Some(id)) => labels.get(id).cloned().or_else(|| Some(id.clone())),
            Self::Key(None) => None,
        }
    }
}
fn group_key(dimension: Option<Dimension>, value: Option<String>) -> GroupKey {
    match dimension {
        None => GroupKey::Overview,
        Some(Dimension::Key) => GroupKey::Key(value),
        Some(_) => GroupKey::Text(value),
    }
}
fn merge_group_rows(
    out: &mut BTreeMap<GroupKey, Additive>,
    dimension: Dimension,
    rows: Vec<sea_orm::QueryResult>,
) -> Result<()> {
    for row in rows {
        let key = group_key(Some(dimension), row.try_get("", "dimension")?);
        out.entry(key).or_default().add_row(&row)?;
    }
    Ok(())
}
fn merge_series_rows(
    out: &mut BTreeMap<(GroupKey, String), Additive>,
    dimension: Option<Dimension>,
    window: Window,
    rows: Vec<sea_orm::QueryResult>,
) -> Result<()> {
    for row in rows {
        let hour: String = row.try_get("", "hour")?;
        let moment = DateTime::parse_from_rfc3339(&hour)?.with_timezone(&Utc);
        let key = group_key(dimension, row.try_get("", "dimension")?);
        out.entry((key, window.bucket.key(moment)))
            .or_default()
            .add_row(&row)?;
    }
    Ok(())
}
/// Canonical request-start form. Capture stores every `started_at` in this
/// shape, so text order is chronological order.
fn timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// The `strftime('%Y-%m-%dT%H:00:00Z', ...)` form written into every hourly
/// aggregate. Bounds must use it so comparisons never depend on how `Z` sorts
/// against a fractional-second separator.
fn hour_bound(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}
fn boundary_predicate(plan: &WindowPlan, column: &str) -> (String, Vec<Value>) {
    let mut clauses = Vec::new();
    let mut values = Vec::new();
    for interval in &plan.boundaries {
        clauses.push(if interval.end_inclusive {
            format!("({column}>=? AND {column}<=?)")
        } else {
            format!("({column}>=? AND {column}<?)")
        });
        values.push(timestamp(interval.start).into());
        values.push(timestamp(interval.end).into());
    }
    (clauses.join(" OR "), values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::arithmetic::SCALE;
    use crate::usage::Bucket;
    fn at(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }
    fn plan(start: &str, end: &str) -> WindowPlan {
        WindowPlan::new(Window {
            start: at(start),
            end: at(end),
            bucket: Bucket::ThreeHours,
        })
        .unwrap()
    }
    #[test]
    fn same_hour_is_one_boundary() {
        let p = plan("2026-01-01T10:15:00Z", "2026-01-01T10:45:00Z");
        assert_eq!(p.boundaries.len(), 1);
        assert!(!p.has_full_hours());
        assert!(p.boundaries[0].end_inclusive);
    }
    #[test]
    fn aligned_start_and_end_keep_endpoint_once() {
        let p = plan("2026-01-01T10:00:00Z", "2026-01-01T11:00:00Z");
        assert_eq!(
            p.full_hours,
            at("2026-01-01T10:00:00Z")..at("2026-01-01T11:00:00Z")
        );
        assert_eq!(
            p.boundaries,
            vec![RequestInterval {
                start: at("2026-01-01T11:00:00Z"),
                end: at("2026-01-01T11:00:00Z"),
                end_inclusive: true
            }]
        );
    }
    #[test]
    fn partial_edges_do_not_overlap_full_hours() {
        let p = plan("2026-01-01T10:15:00Z", "2026-01-01T12:30:00Z");
        assert_eq!(
            p.full_hours,
            at("2026-01-01T11:00:00Z")..at("2026-01-01T12:00:00Z")
        );
        assert_eq!(p.boundaries.len(), 2);
        assert!(!p.boundaries[0].end_inclusive);
        assert!(p.boundaries[1].end_inclusive);
    }
    #[test]
    fn exact_point_is_one_boundary() {
        let p = plan("2026-01-01T10:00:00Z", "2026-01-01T10:00:00Z");
        assert_eq!(p.boundaries.len(), 1);
        assert!(!p.has_full_hours());
    }

    #[test]
    fn ambiguous_hour_and_boundary_fixture_recomputes_the_whole_window() {
        let third = allocate_weight(Some(1), "1", "1", 3).unwrap().unwrap();
        let mut merged = Interval::default();
        merged.add(&Interval::from_rational(&third));
        merged.add(&Interval::from_rational(&third));
        merged.add(&Interval::from_rational(&third));
        assert_eq!(merged.proven_i64().unwrap(), None);
        let entire_window = third.clone() + third.clone() + third;
        assert_eq!(truncate_to_i64(&entire_window).unwrap(), 1);
    }

    #[test]
    fn context_unknown_price_is_not_priced_zero() {
        let mut unknown = ContextAccum::default();
        unknown.unpriced[0] = 1;
        let priced_zero = ContextAccum::default();
        assert_ne!(unknown.unpriced, priced_zero.unpriced);
        assert_eq!(unknown.costs[0].proven_i64().unwrap(), Some(0));
        assert_eq!(priced_zero.costs[0].proven_i64().unwrap(), Some(0));
    }

    #[test]
    fn merged_interval_overflow_is_an_error_and_not_an_ambiguity() {
        let beyond = Interval {
            lower: (BigInt::from(i64::MAX) + BigInt::from(1)) * BigInt::from(SCALE),
            remainders: BigInt::zero(),
        };
        assert!(beyond.proven_truncation().is_some());
        assert!(
            beyond
                .proven_i64()
                .unwrap_err()
                .to_string()
                .contains("signed 64-bit range")
        );
    }

    #[test]
    fn unnamed_tool_and_uncaptured_results_are_separate_rows_reporting_no_label() {
        let mut rows: BTreeMap<ToolRow, i64> = BTreeMap::new();
        *rows.entry(ToolRow::Unnamed(7)).or_default() += 2;
        *rows.entry(ToolRow::Uncaptured).or_default() += 5;
        *rows.entry(ToolRow::Unnamed(8)).or_default() += 1;
        assert_eq!(rows.len(), 3);
        assert!(rows.keys().all(|row| row.clone().label().is_none()));
        assert_ne!(ToolRow::Unnamed(7), ToolRow::Uncaptured);
        assert_eq!(
            ToolRow::Named("read".into()).label().as_deref(),
            Some("read")
        );
    }

    #[test]
    fn hour_bounds_drop_milliseconds_that_request_starts_keep() {
        let moment = at("2026-01-01T10:00:00.000Z");
        assert_eq!(hour_bound(moment), "2026-01-01T10:00:00Z");
        assert_eq!(timestamp(moment), "2026-01-01T10:00:00.000Z");
        // `Z` sorts after the fractional-second separator, so an hour bound
        // written in millisecond form would exclude its own hour.
        assert!(hour_bound(moment) > timestamp(moment));
    }

    #[test]
    fn boundary_predicate_uses_millisecond_starts_on_both_edges() {
        let p = plan("2026-01-01T10:30:00Z", "2026-01-01T12:00:00Z");
        let (predicate, values) = boundary_predicate(&p, "r.started_at");
        assert_eq!(predicate.matches("OR").count(), 1);
        assert!(predicate.ends_with("r.started_at>=? AND r.started_at<=?)"));
        assert_eq!(
            values
                .into_iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>(),
            [
                "'2026-01-01T10:30:00.000Z'",
                "'2026-01-01T11:00:00.000Z'",
                "'2026-01-01T12:00:00.000Z'",
                "'2026-01-01T12:00:00.000Z'"
            ]
        );
    }

    #[test]
    fn context_zero_total_bytes_allocates_no_known_cost() {
        let value = allocate_weight(Some(i64::MAX), "999999999999999999999", "1", 0)
            .unwrap()
            .unwrap();
        assert!(value.is_zero());
    }
}
