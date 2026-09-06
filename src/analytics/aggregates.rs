//! Rebuildable hourly aggregate helpers. Compact request facts remain authoritative.
use super::{
    arithmetic::{Interval, ReportBudget, allocate_weight, parse_rational, tool_allocations},
    sqlite::sql,
};
use anyhow::{Context, Result};
use num_bigint::BigInt;
use num_traits::Zero;
use sea_orm::{ConnectionTrait, Value};
use std::{collections::BTreeMap, time::Duration};

pub(crate) const CONTEXT_COMPONENTS: [(&str, &str); 8] = [
    ("tool_definition", "tool_definition_bytes"),
    ("system", "system_bytes"),
    ("user_text", "user_text_bytes"),
    ("assistant_text", "assistant_text_bytes"),
    ("thinking", "thinking_bytes"),
    ("tool_use", "tool_use_bytes"),
    ("tool_result", "tool_result_bytes"),
    ("other", "other_bytes"),
];

/// A tagged encoding keeps `NULL` distinct from every text value, including the
/// empty string. The length makes the representation stable and unambiguous.
pub(crate) fn dimension_key(value: Option<&str>) -> String {
    match value {
        None => "n".to_owned(),
        Some(value) => format!("s{}:{value}", value.len()),
    }
}

/// Rebuild every aggregate grain for one affected owner/hour. No aggregate is
/// subtracted during replay, so replacement, deletion, and bucket moves use the
/// same path and remain in `apply_batch`'s transaction.
pub async fn repair_owner_hour(
    db: &impl ConnectionTrait,
    owner: Option<&str>,
    hour: &str,
) -> Result<()> {
    let owner_key = dimension_key(owner);
    for table in [
        "hourly_owner_overview",
        "hourly_owner_model",
        "hourly_owner_provider",
        "hourly_owner_key",
        "hourly_context",
        "hourly_context_cost",
        "hourly_tool_contribution",
        "hourly_identity_presence",
    ] {
        db.execute_raw(sql(
            &format!("DELETE FROM {table} WHERE owner_key=? AND hour=?"),
            vec![owner_key.clone().into(), hour.into()],
        ))
        .await?;
    }

    let request_where = "r.owner_id IS ? AND strftime('%Y-%m-%dT%H:00:00Z',r.started_at)=?";
    let values = || vec![Value::from(owner.map(str::to_owned)), hour.into()];
    let base = "COUNT(*),COALESCE(SUM(CASE WHEN r.status<400 AND NOT r.has_error THEN 1 ELSE 0 END),0),COALESCE(SUM(CASE WHEN r.status>=400 OR r.has_error THEN 1 ELSE 0 END),0),COALESCE(SUM(u.input_tokens),0),COALESCE(SUM(u.cache_read_tokens),0),COALESCE(SUM(u.cache_write_tokens),0),COALESCE(SUM(u.output_tokens),0),COALESCE(SUM(u.cost_nanos),0),COALESCE(SUM(u.cost_nanos IS NULL),0)";
    db.execute_raw(sql(
        &format!("INSERT INTO hourly_owner_overview SELECT ?,r.owner_id,?,{base} FROM requests r LEFT JOIN usage u ON u.request_id=r.id WHERE {request_where} GROUP BY r.owner_id"),
        vec![owner_key.clone().into(), hour.into(), Value::from(owner.map(str::to_owned)), hour.into()],
    )).await?;

    for (table, column) in [
        ("hourly_owner_model", "r.requested_model"),
        ("hourly_owner_provider", "r.provider"),
        ("hourly_owner_key", "r.key_id"),
    ] {
        let rows = db.query_all_raw(sql(
            &format!("SELECT {column} dimension,COUNT(*) requests,COALESCE(SUM(CASE WHEN r.status<400 AND NOT r.has_error THEN 1 ELSE 0 END),0) succeeded,COALESCE(SUM(CASE WHEN r.status>=400 OR r.has_error THEN 1 ELSE 0 END),0) failed,COALESCE(SUM(u.input_tokens),0) input_tokens,COALESCE(SUM(u.cache_read_tokens),0) cache_read_tokens,COALESCE(SUM(u.cache_write_tokens),0) cache_write_tokens,COALESCE(SUM(u.output_tokens),0) output_tokens,COALESCE(SUM(u.cost_nanos),0) cost_nanos,COALESCE(SUM(u.cost_nanos IS NULL),0) unpriced FROM requests r LEFT JOIN usage u ON u.request_id=r.id WHERE {request_where} GROUP BY {column}"),
            values(),
        )).await?;
        for row in rows {
            let dimension: Option<String> = row.try_get("", "dimension")?;
            let encoded = dimension_key(dimension.as_deref());
            db.execute_raw(sql(
                &format!("INSERT INTO {table} VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?)"),
                vec![
                    owner_key.clone().into(),
                    Value::from(owner.map(str::to_owned)),
                    hour.into(),
                    encoded.into(),
                    dimension.into(),
                    row.try_get::<i64>("", "requests")?.into(),
                    row.try_get::<i64>("", "succeeded")?.into(),
                    row.try_get::<i64>("", "failed")?.into(),
                    row.try_get::<i64>("", "input_tokens")?.into(),
                    row.try_get::<i64>("", "cache_read_tokens")?.into(),
                    row.try_get::<i64>("", "cache_write_tokens")?.into(),
                    row.try_get::<i64>("", "output_tokens")?.into(),
                    row.try_get::<i64>("", "cost_nanos")?.into(),
                    row.try_get::<i64>("", "unpriced")?.into(),
                ],
            ))
            .await?;
        }
    }

    db.execute_raw(sql(&format!("INSERT INTO hourly_context SELECT ?,r.owner_id,?,COUNT(*),COALESCE(SUM(c.tool_definition_bytes),0),COALESCE(SUM(c.system_bytes),0),COALESCE(SUM(c.user_text_bytes),0),COALESCE(SUM(c.assistant_text_bytes),0),COALESCE(SUM(c.thinking_bytes),0),COALESCE(SUM(c.tool_use_bytes),0),COALESCE(SUM(c.tool_result_bytes),0),COALESCE(SUM(c.other_bytes),0),COALESCE(SUM(c.total_bytes),0),COALESCE(SUM(c.tools_offered),0),COALESCE(SUM(c.tools_invoked),0),COALESCE(SUM(c.tool_result_errors),0),COALESCE(SUM(c.cache_breakpoints),0) FROM requests r JOIN context c ON c.request_id=r.id WHERE {request_where} GROUP BY r.owner_id"),vec![owner_key.clone().into(),hour.into(),Value::from(owner.map(str::to_owned)),hour.into()])).await?;

    let context_rows = db.query_all_raw(sql(
        &format!("SELECT u.cost_nanos,c.tool_definition_bytes,c.system_bytes,c.user_text_bytes,c.assistant_text_bytes,c.thinking_bytes,c.tool_use_bytes,c.tool_result_bytes,c.other_bytes,c.total_bytes FROM requests r JOIN context c ON c.request_id=r.id LEFT JOIN usage u ON u.request_id=r.id WHERE {request_where}"),
        values(),
    )).await?;
    let mut context_costs = vec![(Interval::default(), 0_i64); CONTEXT_COMPONENTS.len()];
    for row in context_rows {
        let cost: Option<i64> = row.try_get("", "cost_nanos")?;
        let total: i64 = row.try_get("", "total_bytes")?;
        for (index, (_, column)) in CONTEXT_COMPONENTS.iter().enumerate() {
            let bytes: i64 = row.try_get("", column)?;
            if cost.is_none() {
                context_costs[index].1 = context_costs[index]
                    .1
                    .checked_add(1)
                    .context("context unknown-price count overflow")?;
            } else if let Some(value) = allocate_weight(cost, &bytes.to_string(), "1", total)? {
                context_costs[index].0.add(&Interval::from_rational(&value));
            }
        }
    }
    for ((component, _), (interval, unpriced)) in CONTEXT_COMPONENTS.iter().zip(context_costs) {
        let (lower, remainders) = interval.decimal_parts();
        db.execute_raw(sql(
            "INSERT INTO hourly_context_cost VALUES(?,?,?,?,?,?,?)",
            vec![
                owner_key.clone().into(),
                Value::from(owner.map(str::to_owned)),
                hour.into(),
                (*component).into(),
                lower.into(),
                remainders.into(),
                unpriced.into(),
            ],
        ))
        .await?;
    }

    let contribution_rows = db.query_all_raw(sql(
        &format!("SELECT c.tool_id,c.definition_count,c.definition_num,c.definition_den,c.transmission_num,c.transmission_den,u.cost_nanos,x.total_bytes FROM requests r JOIN contributions c ON c.request_id=r.id JOIN context x ON x.request_id=r.id LEFT JOIN usage u ON u.request_id=r.id WHERE {request_where}"),
        values(),
    )).await?;
    let mut budget = ReportBudget::new(Duration::from_secs(30), usize::MAX, usize::MAX, usize::MAX);
    let mut tools: BTreeMap<i64, (i64, num_rational::BigRational, Interval, Interval, i64)> =
        BTreeMap::new();
    for row in contribution_rows {
        let tool_id: i64 = row.try_get("", "tool_id")?;
        let definition_num: String = row.try_get("", "definition_num")?;
        let definition_den: String = row.try_get("", "definition_den")?;
        let transmission_num: String = row.try_get("", "transmission_num")?;
        let transmission_den: String = row.try_get("", "transmission_den")?;
        let cost: Option<i64> = row.try_get("", "cost_nanos")?;
        let total: i64 = row.try_get("", "total_bytes")?;
        let entry = tools.entry(tool_id).or_default();
        entry.0 = entry
            .0
            .checked_add(row.try_get("", "definition_count")?)
            .context("tool definition count overflow")?;
        entry.1 += parse_rational(&definition_num, &definition_den)?;
        let (definition, transmission) = tool_allocations(
            cost,
            total,
            &definition_num,
            &definition_den,
            &transmission_num,
            &transmission_den,
            &mut budget,
        )?;
        // Both allocations are absent exactly when the request had no price. A
        // tool that contributed no bytes loses nothing to that, so it is not
        // counted as understated.
        match (definition, transmission) {
            (Some(definition), Some(transmission)) => {
                entry.2.add(&Interval::from_rational(&definition));
                entry.3.add(&Interval::from_rational(&transmission));
            }
            _ => {
                let contributed = definition_num.parse::<BigInt>()? != BigInt::zero()
                    || transmission_num.parse::<BigInt>()? != BigInt::zero();
                if contributed {
                    entry.4 = entry
                        .4
                        .checked_add(1)
                        .context("tool unknown-price count overflow")?;
                }
            }
        }
    }
    for (tool_id, (count, definition_bytes, definition, transmission, unpriced)) in tools {
        let (definition_lower, definition_remainders) = definition.decimal_parts();
        let (transmission_lower, transmission_remainders) = transmission.decimal_parts();
        db.execute_raw(sql(
            "INSERT INTO hourly_tool_contribution VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
            vec![
                owner_key.clone().into(),
                Value::from(owner.map(str::to_owned)),
                hour.into(),
                tool_id.into(),
                count.into(),
                definition_bytes.numer().to_string().into(),
                definition_bytes.denom().to_string().into(),
                definition_lower.into(),
                definition_remainders.into(),
                transmission_lower.into(),
                transmission_remainders.into(),
                unpriced.into(),
            ],
        ))
        .await?;
    }
    db.execute_raw(sql(&format!("INSERT INTO hourly_identity_presence SELECT ?,r.owner_id,?,v.identity_id,v.kind FROM requests r JOIN appearances a ON a.request_id=r.id JOIN variants v ON v.id=a.variant_id WHERE {request_where} GROUP BY r.owner_id,v.identity_id,v.kind"),vec![owner_key.into(),hour.into(),Value::from(owner.map(str::to_owned)),hour.into()])).await?;
    Ok(())
}
