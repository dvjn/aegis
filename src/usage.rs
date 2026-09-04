use std::collections::BTreeMap;

use chrono::{DateTime, NaiveTime, SecondsFormat, TimeDelta, Utc};
use sea_orm::{DatabaseConnection, DbBackend, FromQueryResult, Statement};
use serde::Serialize;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Range {
    Day,
    #[default]
    Week,
    Month,
}

impl Range {
    pub fn slug(self) -> &'static str {
        match self {
            Self::Day => "24h",
            Self::Week => "7d",
            Self::Month => "1m",
        }
    }

    pub fn label(self) -> &'static str {
        self.slug()
    }

    pub fn from_slug(value: Option<&str>) -> Self {
        match value {
            Some("24h") => Self::Day,
            Some("1m") => Self::Month,
            _ => Self::Week,
        }
    }

    pub const ALL: [Self; 3] = [Self::Day, Self::Week, Self::Month];

    fn span(self) -> TimeDelta {
        match self {
            Self::Day => TimeDelta::hours(24),
            Self::Week => TimeDelta::days(7),
            Self::Month => TimeDelta::days(30),
        }
    }

    /// Eight to ten points give a range a readable curve: three hours for a
    /// day, a day for a week, three days for a month.
    fn bucket(self) -> Bucket {
        match self {
            Self::Day => Bucket::ThreeHours,
            Self::Week => Bucket::Day,
            Self::Month => Bucket::ThreeDays,
        }
    }

    pub fn window(self, now: DateTime<Utc>) -> Window {
        Window {
            start: now - self.span(),
            end: now,
            bucket: self.bucket(),
        }
    }
}

/// The reporting window, inclusive at both ends, and how it is cut into
/// series points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub bucket: Bucket,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bucket {
    /// Aligned to UTC hours divisible by three: 00, 03, ..., 21.
    ThreeHours,
    Day,
    /// Aligned to whole multiples of three days since 1970-01-01 UTC.
    ThreeDays,
}

impl Bucket {
    /// The SQLite expression that maps `r.started_at` to a bucket key. It must
    /// produce exactly what `key` produces for the same moment.
    fn sql(self) -> &'static str {
        match self {
            Self::ThreeHours => {
                "strftime('%Y-%m-%dT', r.started_at) \
                 || printf('%02d', (CAST(strftime('%H', r.started_at) AS INTEGER) / 3) * 3) \
                 || ':00:00Z'"
            }
            Self::Day => "strftime('%Y-%m-%d', r.started_at)",
            // Days since the epoch, modulo three, is how far the day sits past
            // its bucket start.
            Self::ThreeDays => concat!(
                "date(r.started_at, '-' || ",
                "((CAST(julianday(date(r.started_at)) - 2440587.5 AS INTEGER)) % 3)",
                " || ' days')"
            ),
        }
    }

    fn key(self, moment: DateTime<Utc>) -> String {
        let aligned = self.align(moment);
        match self {
            Self::ThreeHours => aligned.format("%Y-%m-%dT%H:00:00Z").to_string(),
            Self::Day | Self::ThreeDays => aligned.format("%Y-%m-%d").to_string(),
        }
    }

    /// Rounds a moment down to the start of its bucket.
    fn align(self, moment: DateTime<Utc>) -> DateTime<Utc> {
        let day_start = moment.date_naive().and_time(NaiveTime::MIN).and_utc();
        match self {
            Self::ThreeHours => {
                let hours_into_day = (moment - day_start).num_hours();
                day_start + TimeDelta::hours(hours_into_day - hours_into_day % 3)
            }
            Self::Day => day_start,
            Self::ThreeDays => {
                let days_since_epoch = (day_start - DateTime::<Utc>::UNIX_EPOCH).num_days();
                day_start - TimeDelta::days(days_since_epoch.rem_euclid(3))
            }
        }
    }

    fn step(self) -> TimeDelta {
        match self {
            Self::ThreeHours => TimeDelta::hours(3),
            Self::Day => TimeDelta::days(1),
            Self::ThreeDays => TimeDelta::days(3),
        }
    }
}

impl Window {
    /// Every bucket key inside the window, in order, whether or not it has data.
    pub fn bucket_keys(&self) -> Vec<String> {
        self.moments()
            .map(|moment| self.bucket.key(moment))
            .collect()
    }

    /// Walks bucket starts from the one holding `start` to the one holding
    /// `end`, so a window that opens mid-bucket still gets that bucket.
    fn moments(&self) -> impl Iterator<Item = DateTime<Utc>> + '_ {
        let step = self.bucket.step();
        let first = self.bucket.align(self.start);
        std::iter::successors(Some(first), move |moment| Some(*moment + step))
            .take_while(|moment| *moment <= self.end)
    }

    fn bounds(&self) -> [String; 2] {
        [
            self.start.to_rfc3339_opts(SecondsFormat::Millis, true),
            self.end.to_rfc3339_opts(SecondsFormat::Millis, true),
        ]
    }
}

#[derive(Debug, Default, FromQueryResult, Serialize)]
pub struct UsageTotals {
    pub requests: i64,
    pub succeeded: i64,
    pub failed: i64,
    pub input_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub output_tokens: i64,
    pub cost_nanodollars: i64,
    pub unpriced: i64,
}

impl UsageTotals {
    pub fn tokens(&self) -> i64 {
        self.input_tokens + self.cache_read_tokens + self.cache_write_tokens + self.output_tokens
    }

    pub fn unfinished(&self) -> i64 {
        self.requests - self.succeeded - self.failed
    }
}

/// Request context composition, summed over the requests of the window.
/// Sizes are the stored JSON bytes of each part; tokens are estimated at read
/// time and never stored. A part's cost is the request's cost apportioned by
/// the part's share of the request's bytes.
#[derive(Debug, Default, PartialEq, Eq, FromQueryResult, Serialize)]
pub struct ContextTotals {
    pub requests: i64,
    pub tool_definition_bytes: i64,
    pub system_bytes: i64,
    pub user_text_bytes: i64,
    pub assistant_text_bytes: i64,
    pub thinking_bytes: i64,
    pub tool_use_bytes: i64,
    pub tool_result_bytes: i64,
    pub other_bytes: i64,
    pub total_bytes: i64,
    pub tools_offered: i64,
    pub tools_invoked: i64,
    pub tool_result_errors: i64,
    pub cache_breakpoints: i64,
    pub tool_definition_cost_nanodollars: i64,
    pub system_cost_nanodollars: i64,
    pub user_text_cost_nanodollars: i64,
    pub assistant_text_cost_nanodollars: i64,
    pub thinking_cost_nanodollars: i64,
    pub tool_use_cost_nanodollars: i64,
    pub tool_result_cost_nanodollars: i64,
    pub other_cost_nanodollars: i64,
}

/// Measured over this gateway's captured traffic: stored request bytes divided
/// by the input tokens the providers reported.
pub const BYTES_PER_TOKEN: f64 = 3.529;

impl ContextTotals {
    pub fn estimated_tokens(bytes: i64) -> i64 {
        (bytes as f64 / BYTES_PER_TOKEN).round() as i64
    }
}

/// One tool over the window. Bytes are what the tool put into the context:
/// its calls and their results, once per call, and its definition in every
/// request that carried it. Cost is what those bytes cost in every request
/// that carried them, apportioned by their share of the request's bytes.
/// Results whose call was not captured group under no label.
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct ToolCalls {
    pub label: Option<String>,
    pub calls: i64,
    pub bytes: i64,
    pub cost_nanodollars: i64,
}

/// One skill's Skill tool calls over the window, measured like a tool but
/// without a definition: the calls and the skill bodies they returned.
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct SkillCalls {
    pub label: String,
    pub calls: i64,
    pub bytes: i64,
    pub cost_nanodollars: i64,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct McpServer {
    pub label: String,
    pub calls: i64,
    pub tools: i64,
    pub bytes: i64,
    pub cost_nanodollars: i64,
}

/// Every tool part of the window's requests, read once.
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct ToolUsage {
    /// Ordered by calls, most first, then by name. A tool that was only
    /// defined, or whose calls fall outside the window, has zero calls.
    pub tools: Vec<ToolCalls>,
    /// Ordered by calls, most first, then by name.
    pub skills: Vec<SkillCalls>,
}

impl ToolUsage {
    /// Servers ordered by calls, most first, then by name.
    pub fn mcp_servers(&self) -> Vec<McpServer> {
        let mut by_server: BTreeMap<&str, McpServer> = BTreeMap::new();
        for tool in &self.tools {
            let Some(name) = tool
                .label
                .as_deref()
                .and_then(crate::payload_facts::mcp_server)
            else {
                continue;
            };
            let server = by_server.entry(name).or_insert_with(|| McpServer {
                label: name.to_owned(),
                ..McpServer::default()
            });
            server.calls += tool.calls;
            server.tools += 1;
            server.bytes += tool.bytes;
            server.cost_nanodollars += tool.cost_nanodollars;
        }
        let mut servers: Vec<McpServer> = by_server.into_values().collect();
        servers.sort_by(|left, right| {
            right
                .calls
                .cmp(&left.calls)
                .then_with(|| left.label.cmp(&right.label))
        });
        servers
    }
}

/// One aggregated row of `TOOL_PARTS_SQL`: a block type of one tool, or of
/// one skill when the part belongs to a Skill call.
#[derive(Debug, FromQueryResult)]
struct ToolPart {
    block_type: String,
    label: Option<String>,
    skill: Option<String>,
    parts: i64,
    bytes: i64,
    cost_nanodollars: i64,
}

#[derive(Debug, FromQueryResult, Serialize)]
pub struct UsageGroup {
    pub label: Option<String>,
    pub requests: i64,
    pub tokens: i64,
    pub cost_nanodollars: i64,
}

/// One value per bucket of the window, for each of the three overview tiles.
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct TotalsSeries {
    pub requests: Vec<i64>,
    pub tokens: Vec<i64>,
    pub cost_nanodollars: Vec<i64>,
}

/// Tokens per bucket for one model or provider, ordered by total tokens.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct LabeledSeries {
    pub label: Option<String>,
    pub tokens: Vec<i64>,
}

impl LabeledSeries {
    pub fn total(&self) -> i64 {
        self.tokens.iter().sum()
    }
}

#[derive(Debug, FromQueryResult)]
struct TotalsPoint {
    bucket: String,
    requests: i64,
    tokens: i64,
    cost_nanodollars: i64,
}

#[derive(Debug, FromQueryResult)]
struct LabeledPoint {
    label: Option<String>,
    bucket: String,
    tokens: i64,
}

#[derive(Clone)]
pub struct UsageStore {
    database: DatabaseConnection,
}

const FROM_WINDOW_SQL: &str = "FROM gateway_requests r \
     JOIN gateway_keys k ON k.id = r.key_id \
     LEFT JOIN gateway_usage u ON u.request_id = r.id \
     WHERE k.user_id = ? AND r.started_at >= ? AND r.started_at <= ?";

const TOKENS_SQL: &str = "COALESCE(SUM(u.input_tokens), 0) + \
     COALESCE(SUM(u.cache_read_tokens), 0) + \
     COALESCE(SUM(u.cache_write_tokens), 0) + \
     COALESCE(SUM(u.output_tokens), 0)";

const TOTALS_SQL: &str = "SELECT COUNT(*) requests, \
     COALESCE(SUM(CASE WHEN r.http_status < 400 AND r.error_message IS NULL THEN 1 ELSE 0 END), 0) succeeded, \
     COALESCE(SUM(CASE WHEN r.http_status >= 400 OR r.error_message IS NOT NULL THEN 1 ELSE 0 END), 0) failed, \
     COALESCE(SUM(u.input_tokens), 0) input_tokens, \
     COALESCE(SUM(u.cache_read_tokens), 0) cache_read_tokens, \
     COALESCE(SUM(u.cache_write_tokens), 0) cache_write_tokens, \
     COALESCE(SUM(u.output_tokens), 0) output_tokens, \
     COALESCE(SUM(u.cost_nanodollars), 0) cost_nanodollars, \
     COALESCE(SUM(CASE WHEN u.cost_nanodollars IS NULL THEN 1 ELSE 0 END), 0) unpriced \
     {from}";

const CONTEXT_SQL: &str = "SELECT COUNT(m.request_id) requests, \
     COALESCE(SUM(m.tool_definition_bytes), 0) tool_definition_bytes, \
     COALESCE(SUM(m.system_bytes), 0) system_bytes, \
     COALESCE(SUM(m.user_text_bytes), 0) user_text_bytes, \
     COALESCE(SUM(m.assistant_text_bytes), 0) assistant_text_bytes, \
     COALESCE(SUM(m.thinking_bytes), 0) thinking_bytes, \
     COALESCE(SUM(m.tool_use_bytes), 0) tool_use_bytes, \
     COALESCE(SUM(m.tool_result_bytes), 0) tool_result_bytes, \
     COALESCE(SUM(m.other_bytes), 0) other_bytes, \
     COALESCE(SUM(m.total_bytes), 0) total_bytes, \
     COALESCE(SUM(m.tools_offered), 0) tools_offered, \
     COALESCE(SUM(m.tools_invoked), 0) tools_invoked, \
     COALESCE(SUM(m.tool_result_errors), 0) tool_result_errors, \
     COALESCE(SUM(m.cache_breakpoints), 0) cache_breakpoints, \
     {part_cost:tool_definition_bytes} tool_definition_cost_nanodollars, \
     {part_cost:system_bytes} system_cost_nanodollars, \
     {part_cost:user_text_bytes} user_text_cost_nanodollars, \
     {part_cost:assistant_text_bytes} assistant_text_cost_nanodollars, \
     {part_cost:thinking_bytes} thinking_cost_nanodollars, \
     {part_cost:tool_use_bytes} tool_use_cost_nanodollars, \
     {part_cost:tool_result_bytes} tool_result_cost_nanodollars, \
     {part_cost:other_bytes} other_cost_nanodollars \
     FROM gateway_requests r \
     JOIN gateway_keys k ON k.id = r.key_id \
     JOIN gateway_request_metrics m ON m.request_id = r.id \
     LEFT JOIN gateway_usage u ON u.request_id = r.id \
     WHERE k.user_id = ? AND r.started_at >= ? AND r.started_at <= ?";

const PART_COST_SQL: &str = "COALESCE(CAST(SUM(CASE WHEN m.total_bytes > 0 \
     THEN u.cost_nanodollars * 1.0 * m.{part} / m.total_bytes ELSE 0 END) AS INTEGER), 0)";

const CONTEXT_PARTS: [&str; 8] = [
    "tool_definition_bytes",
    "system_bytes",
    "user_text_bytes",
    "assistant_text_bytes",
    "thinking_bytes",
    "tool_use_bytes",
    "tool_result_bytes",
    "other_bytes",
];

fn context_sql() -> String {
    CONTEXT_PARTS
        .iter()
        .fold(CONTEXT_SQL.to_owned(), |sql, part| {
            sql.replace(
                &format!("{{part_cost:{part}}}"),
                &PART_COST_SQL.replace("{part}", part),
            )
        })
}

/// Calls and results count once per call, however many later requests
/// replay them; definitions count once per request that carried them, and
/// every part costs its byte share of every request that carried it. The
/// window's references are grouped by blob first, so each blob and its facts
/// are read once. A result carries no tool name or skill, so it borrows them
/// from the call it answers. A blob holding several definitions splits its
/// bytes between them.
const TOOL_PARTS_SQL: &str = "WITH refs AS MATERIALIZED ( \
     SELECT p.part_id, COUNT(*) sent, \
     SUM(COALESCE(u.cost_nanodollars * 1.0 / m.total_bytes, 0)) cost_per_byte \
     FROM gateway_requests r \
     JOIN gateway_keys k ON k.id = r.key_id \
     JOIN gateway_payload_part_refs p ON p.request_id = r.id AND p.direction = 'request' \
     LEFT JOIN gateway_request_metrics m ON m.request_id = r.id AND m.total_bytes > 0 \
     LEFT JOIN gateway_usage u ON u.request_id = r.id \
     WHERE k.user_id = ? AND r.started_at >= ? AND r.started_at <= ? \
     GROUP BY p.part_id), \
     parts AS MATERIALIZED ( \
     SELECT f.block_type, \
     CASE WHEN f.block_type = 'tool_result' THEN (SELECT MIN(x.tool_name) FROM gateway_payload_blob_facts x \
     WHERE x.tool_use_id = f.tool_use_id AND x.block_type = 'tool_use') ELSE f.tool_name END label, \
     CASE WHEN f.block_type = 'tool_result' THEN (SELECT MIN(x.skill_name) FROM gateway_payload_blob_facts x \
     WHERE x.tool_use_id = f.tool_use_id AND x.block_type = 'tool_use') ELSE f.skill_name END skill, \
     COALESCE(f.tool_use_id, f.blob_id) call_id, \
     b.original_bytes bytes, \
     refs.sent, \
     refs.cost_per_byte, \
     CASE WHEN f.block_type = 'tool_definition' THEN (SELECT COUNT(*) FROM gateway_payload_blob_facts x \
     WHERE x.blob_id = f.blob_id) ELSE 1 END facts \
     FROM refs \
     JOIN gateway_payload_blob_facts f ON f.blob_id = refs.part_id \
     JOIN gateway_payload_blobs b ON b.id = refs.part_id \
     WHERE f.block_type IN ('tool_definition', 'tool_use', 'tool_result')) \
     SELECT block_type, label, skill, SUM(sent) parts, \
     CAST(SUM(bytes * sent * 1.0 / facts) AS INTEGER) bytes, \
     CAST(SUM(bytes * cost_per_byte / facts) AS INTEGER) cost_nanodollars \
     FROM parts WHERE block_type = 'tool_definition' GROUP BY label \
     UNION ALL \
     SELECT block_type, label, skill, COUNT(*) parts, \
     COALESCE(SUM(bytes), 0) bytes, \
     CAST(COALESCE(SUM(cost), 0) AS INTEGER) cost_nanodollars \
     FROM (SELECT block_type, call_id, MIN(label) label, MIN(skill) skill, MAX(bytes) bytes, \
     SUM(bytes * cost_per_byte) cost \
     FROM parts WHERE block_type <> 'tool_definition' GROUP BY block_type, call_id) \
     GROUP BY block_type, label, skill";

const BREAKDOWN_SQL: &str = "SELECT {column} label, COUNT(*) requests, \
     {tokens} tokens, \
     COALESCE(SUM(u.cost_nanodollars), 0) cost_nanodollars \
     {from} \
     GROUP BY {column} ORDER BY tokens DESC, requests DESC, label";

const TOTALS_SERIES_SQL: &str = "SELECT {bucket} bucket, COUNT(*) requests, \
     {tokens} tokens, \
     COALESCE(SUM(u.cost_nanodollars), 0) cost_nanodollars \
     {from} \
     GROUP BY bucket";

const LABELED_SERIES_SQL: &str = "SELECT {column} label, {bucket} bucket, \
     {tokens} tokens \
     {from} \
     GROUP BY {column}, bucket";

fn render_sql(template: &str, column: &str, bucket: Bucket) -> String {
    template
        .replace("{from}", FROM_WINDOW_SQL)
        .replace("{tokens}", TOKENS_SQL)
        .replace("{column}", column)
        .replace("{bucket}", bucket.sql())
}

impl UsageStore {
    pub fn new(database: DatabaseConnection) -> Self {
        Self { database }
    }

    pub async fn totals(
        &self,
        user_id: Uuid,
        window: Window,
    ) -> Result<UsageTotals, sea_orm::DbErr> {
        let sql = render_sql(TOTALS_SQL, "", window.bucket);
        Ok(
            UsageTotals::find_by_statement(self.statement(&sql, user_id, window))
                .one(&self.database)
                .await?
                .unwrap_or_default(),
        )
    }

    pub async fn context(
        &self,
        user_id: Uuid,
        window: Window,
    ) -> Result<ContextTotals, sea_orm::DbErr> {
        Ok(
            ContextTotals::find_by_statement(self.statement(&context_sql(), user_id, window))
                .one(&self.database)
                .await?
                .unwrap_or_default(),
        )
    }

    pub async fn tool_usage(
        &self,
        user_id: Uuid,
        window: Window,
    ) -> Result<ToolUsage, sea_orm::DbErr> {
        let parts = ToolPart::find_by_statement(self.statement(TOOL_PARTS_SQL, user_id, window))
            .all(&self.database)
            .await?;
        let mut tools: BTreeMap<Option<String>, ToolCalls> = BTreeMap::new();
        let mut skills: BTreeMap<String, SkillCalls> = BTreeMap::new();
        for part in parts {
            let tool = tools.entry(part.label.clone()).or_default();
            tool.bytes += part.bytes;
            tool.cost_nanodollars += part.cost_nanodollars;
            if part.block_type == "tool_use" {
                tool.calls += part.parts;
            }
            if let Some(skill) = part.skill {
                let skill = skills.entry(skill).or_default();
                skill.bytes += part.bytes;
                skill.cost_nanodollars += part.cost_nanodollars;
                if part.block_type == "tool_use" {
                    skill.calls += part.parts;
                }
            }
        }
        let mut tools: Vec<ToolCalls> = tools
            .into_iter()
            .map(|(label, tool)| ToolCalls { label, ..tool })
            .collect();
        tools.sort_by(|left, right| {
            right
                .calls
                .cmp(&left.calls)
                .then_with(|| left.label.cmp(&right.label))
        });
        let mut skills: Vec<SkillCalls> = skills
            .into_iter()
            .map(|(label, skill)| SkillCalls { label, ..skill })
            .collect();
        skills.sort_by(|left, right| {
            right
                .calls
                .cmp(&left.calls)
                .then_with(|| left.label.cmp(&right.label))
        });
        Ok(ToolUsage { tools, skills })
    }

    pub async fn by_model(
        &self,
        user_id: Uuid,
        window: Window,
    ) -> Result<Vec<UsageGroup>, sea_orm::DbErr> {
        self.breakdown("r.requested_model", user_id, window).await
    }

    pub async fn by_provider(
        &self,
        user_id: Uuid,
        window: Window,
    ) -> Result<Vec<UsageGroup>, sea_orm::DbErr> {
        self.breakdown("r.provider", user_id, window).await
    }

    pub async fn by_key(
        &self,
        user_id: Uuid,
        window: Window,
    ) -> Result<Vec<UsageGroup>, sea_orm::DbErr> {
        self.breakdown("k.name", user_id, window).await
    }

    pub async fn totals_series(
        &self,
        user_id: Uuid,
        window: Window,
    ) -> Result<TotalsSeries, sea_orm::DbErr> {
        let sql = render_sql(TOTALS_SERIES_SQL, "", window.bucket);
        let points = TotalsPoint::find_by_statement(self.statement(&sql, user_id, window))
            .all(&self.database)
            .await?;
        let by_bucket: BTreeMap<&str, &TotalsPoint> = points
            .iter()
            .map(|point| (point.bucket.as_str(), point))
            .collect();
        let keys = window.bucket_keys();
        let pick = |field: fn(&TotalsPoint) -> i64| -> Vec<i64> {
            keys.iter()
                .map(|key| by_bucket.get(key.as_str()).map_or(0, |point| field(point)))
                .collect()
        };
        Ok(TotalsSeries {
            requests: pick(|point| point.requests),
            tokens: pick(|point| point.tokens),
            cost_nanodollars: pick(|point| point.cost_nanodollars),
        })
    }

    pub async fn series_by_model(
        &self,
        user_id: Uuid,
        window: Window,
    ) -> Result<Vec<LabeledSeries>, sea_orm::DbErr> {
        self.labeled_series("r.requested_model", user_id, window)
            .await
    }

    pub async fn series_by_provider(
        &self,
        user_id: Uuid,
        window: Window,
    ) -> Result<Vec<LabeledSeries>, sea_orm::DbErr> {
        self.labeled_series("r.provider", user_id, window).await
    }

    pub async fn series_by_key(
        &self,
        user_id: Uuid,
        window: Window,
    ) -> Result<Vec<LabeledSeries>, sea_orm::DbErr> {
        self.labeled_series("k.name", user_id, window).await
    }

    async fn breakdown(
        &self,
        column: &str,
        user_id: Uuid,
        window: Window,
    ) -> Result<Vec<UsageGroup>, sea_orm::DbErr> {
        let sql = render_sql(BREAKDOWN_SQL, column, window.bucket);
        UsageGroup::find_by_statement(self.statement(&sql, user_id, window))
            .all(&self.database)
            .await
    }

    async fn labeled_series(
        &self,
        column: &str,
        user_id: Uuid,
        window: Window,
    ) -> Result<Vec<LabeledSeries>, sea_orm::DbErr> {
        let sql = render_sql(LABELED_SERIES_SQL, column, window.bucket);
        let points = LabeledPoint::find_by_statement(self.statement(&sql, user_id, window))
            .all(&self.database)
            .await?;
        let keys = window.bucket_keys();
        let positions: BTreeMap<&str, usize> = keys
            .iter()
            .enumerate()
            .map(|(index, key)| (key.as_str(), index))
            .collect();

        let mut by_label: BTreeMap<Option<String>, Vec<i64>> = BTreeMap::new();
        for point in points {
            let Some(&index) = positions.get(point.bucket.as_str()) else {
                continue;
            };
            by_label
                .entry(point.label)
                .or_insert_with(|| vec![0; keys.len()])[index] += point.tokens;
        }
        let mut series: Vec<LabeledSeries> = by_label
            .into_iter()
            .map(|(label, tokens)| LabeledSeries { label, tokens })
            .collect();
        series.sort_by(|left, right| {
            right
                .total()
                .cmp(&left.total())
                .then_with(|| left.label.cmp(&right.label))
        });
        Ok(series)
    }

    fn statement(&self, sql: &str, user_id: Uuid, window: Window) -> Statement {
        let [start, end] = window.bounds();
        Statement::from_sql_and_values(
            DbBackend::Sqlite,
            sql,
            [user_id.to_string().into(), start.into(), end.into()],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use sea_orm::{ConnectionTrait, Database};
    use sea_orm_migration::MigratorTrait;

    const KEY: &str = "key-1";

    fn window(from: &str, to: &str, bucket: Bucket) -> Window {
        let parse = |text: &str| {
            DateTime::parse_from_rfc3339(text)
                .unwrap()
                .with_timezone(&Utc)
        };
        Window {
            start: parse(from),
            end: parse(to),
            bucket,
        }
    }

    fn daily(from: &str, to: &str) -> Window {
        window(
            &format!("{from}T00:00:00Z"),
            &format!("{to}T23:59:59.999Z"),
            Bucket::Day,
        )
    }

    fn last(range: Range) -> Window {
        range.window(Utc::now())
    }

    async fn record(
        db: &DatabaseConnection,
        id: &str,
        provider: &str,
        model: Option<&str>,
        started_at: &str,
        input: i64,
        output: i64,
    ) {
        let model = match model {
            Some(model) => format!("'{model}'"),
            None => "NULL".to_owned(),
        };
        db.execute_unprepared(&format!(
            "INSERT INTO gateway_requests(id,request_id,provider,protocol,method,endpoint,requested_model,started_at,request_bytes,response_bytes,client_disconnected,key_id) \
             VALUES('{id}','{id}','{provider}','anthropic_messages','POST','/providers/{provider}/v1/messages',{model},'{started_at}',0,0,FALSE,'{KEY}')"
        ))
        .await
        .unwrap();
        db.execute_unprepared(&format!(
            "INSERT INTO gateway_usage(request_id,input_tokens,output_tokens) VALUES('{id}',{input},{output})"
        ))
        .await
        .unwrap();
    }

    async fn complete(db: &DatabaseConnection, id: &str, status: i32) {
        db.execute_unprepared(&format!(
            "UPDATE gateway_requests SET http_status = {status} WHERE id = '{id}'"
        ))
        .await
        .unwrap();
    }

    async fn empty_store(email: &str) -> (DatabaseConnection, Uuid) {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let user = Uuid::now_v7();
        db.execute_unprepared(&format!("INSERT INTO users(id,email_normalized,email_display,role,status,auth_version,created_at,updated_at) VALUES('{user}','{email}','{email}','user','active',0,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')")).await.unwrap();
        db.execute_unprepared(&format!("INSERT INTO gateway_keys(id,user_id,name,allowed_providers,created_at) VALUES('{KEY}','{user}','agent','[\"claude\"]','2026-01-01T00:00:00Z')")).await.unwrap();
        (db, user)
    }

    async fn fixture() -> (UsageStore, Uuid) {
        let (db, user) = empty_store("user@example.com").await;

        let at = |ago: TimeDelta| (Utc::now() - ago).to_rfc3339_opts(SecondsFormat::Millis, true);
        record(
            &db,
            "r-hour",
            "claude",
            Some("opus"),
            &at(TimeDelta::hours(1)),
            100,
            10,
        )
        .await;
        db.execute_unprepared(
            "UPDATE gateway_usage SET cache_read_tokens = 1000, cache_write_tokens = 200 WHERE request_id = 'r-hour'",
        )
        .await
        .unwrap();
        record(
            &db,
            "r-days",
            "claude",
            Some("opus"),
            &at(TimeDelta::days(3)),
            200,
            20,
        )
        .await;
        record(
            &db,
            "r-weeks",
            "codex",
            None,
            &at(TimeDelta::days(20)),
            400,
            40,
        )
        .await;
        record(
            &db,
            "r-old",
            "codex",
            Some("gpt"),
            &at(TimeDelta::days(90)),
            800,
            80,
        )
        .await;
        complete(&db, "r-hour", 200).await;
        complete(&db, "r-days", 429).await;
        complete(&db, "r-weeks", 200).await;
        (UsageStore::new(db), user)
    }

    /// A fixture pinned to fixed dates, so bucket keys can be asserted exactly.
    async fn dated_fixture() -> (UsageStore, Uuid) {
        let (db, user) = empty_store("dated@example.com").await;
        record(
            &db,
            "d-1",
            "claude",
            Some("opus"),
            "2026-03-01T10:15:00.000Z",
            10,
            1,
        )
        .await;
        record(
            &db,
            "d-2",
            "claude",
            Some("opus"),
            "2026-03-01T10:45:00.000Z",
            20,
            2,
        )
        .await;
        record(
            &db,
            "d-3",
            "codex",
            Some("gpt"),
            "2026-03-02T23:59:59.999Z",
            40,
            4,
        )
        .await;
        record(&db, "d-4", "codex", None, "2026-03-04T00:00:00.000Z", 80, 8).await;
        record(
            &db,
            "d-before",
            "codex",
            Some("gpt"),
            "2026-02-28T23:59:59.999Z",
            1000,
            0,
        )
        .await;
        record(
            &db,
            "d-after",
            "codex",
            Some("gpt"),
            "2026-03-05T00:00:00.000Z",
            1000,
            0,
        )
        .await;
        (UsageStore::new(db), user)
    }

    #[test]
    fn an_unknown_or_missing_slug_selects_the_week() {
        assert_eq!(Range::from_slug(None), Range::Week);
        assert_eq!(Range::from_slug(Some("forever")), Range::Week);
        assert_eq!(Range::from_slug(Some("24h")), Range::Day);
        assert_eq!(Range::from_slug(Some("1m")), Range::Month);
    }

    #[test]
    fn the_day_buckets_by_three_hours_and_longer_ranges_by_day() {
        let now = DateTime::parse_from_rfc3339("2026-03-04T14:23:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let day = Range::Day.window(now);
        assert_eq!(day.bucket, Bucket::ThreeHours);
        let keys = day.bucket_keys();
        assert_eq!(
            keys.len(),
            9,
            "an unaligned start adds a partial bucket at each edge"
        );
        assert_eq!(keys[0], "2026-03-03T12:00:00Z");
        assert_eq!(keys[1], "2026-03-03T15:00:00Z");
        assert_eq!(keys[8], "2026-03-04T12:00:00Z");

        // A 24h window always touches nine buckets: eight full ones plus the
        // one holding its closing instant, since the end bound is inclusive.
        let aligned = DateTime::parse_from_rfc3339("2026-03-04T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let aligned_keys = Range::Day.window(aligned).bucket_keys();
        assert_eq!(aligned_keys.len(), 9);
        assert_eq!(aligned_keys[0], "2026-03-03T15:00:00Z");
        assert_eq!(aligned_keys[8], "2026-03-04T15:00:00Z");

        let week = Range::Week.window(now);
        assert_eq!(week.bucket, Bucket::Day);
        assert_eq!(week.bucket_keys().len(), 8);
        assert_eq!(week.bucket_keys()[0], "2026-02-25");

        // Thirty days is ten three-day steps, so the closing instant always
        // falls ten buckets after the opening one: eleven keys, aligned or not.
        let month = Range::Month.window(now);
        assert_eq!(month.bucket, Bucket::ThreeDays);
        let month_keys = month.bucket_keys();
        assert_eq!(month_keys.len(), 11);
        assert_eq!(
            month_keys[0], "2026-01-31",
            "the window opens on 2026-02-02, and 2026-01-31 is 20484 days past the epoch, a multiple of 3"
        );
        assert_eq!(month_keys[10], "2026-03-02");
    }

    #[tokio::test]
    async fn sqlite_and_rust_agree_on_every_bucket_key() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        for (bucket, moments) in [
            (
                Bucket::ThreeHours,
                &[
                    "2026-03-04T00:00:00.000Z",
                    "2026-03-04T02:59:59.999Z",
                    "2026-03-04T14:23:00.000Z",
                    "2026-12-31T23:00:00.000Z",
                ][..],
            ),
            (
                Bucket::Day,
                &["2026-03-04T23:59:59.999Z", "2024-02-29T12:00:00.000Z"][..],
            ),
            (
                Bucket::ThreeDays,
                &[
                    "2026-02-01T00:00:00.000Z",
                    "2026-02-02T10:00:00.000Z",
                    "2026-02-03T23:59:59.999Z",
                    "2026-03-04T14:23:00.000Z",
                    "1970-01-02T00:00:00.000Z",
                ][..],
            ),
        ] {
            for moment in moments {
                let sql = format!(
                    "SELECT {}",
                    bucket.sql().replace("r.started_at", &format!("'{moment}'"))
                );
                let row = db
                    .query_one_raw(Statement::from_string(DbBackend::Sqlite, sql))
                    .await
                    .unwrap()
                    .unwrap();
                let from_sqlite: String = row.try_get_by_index(0).unwrap();
                let parsed = DateTime::parse_from_rfc3339(moment)
                    .unwrap()
                    .with_timezone(&Utc);
                assert_eq!(from_sqlite, bucket.key(parsed), "{bucket:?} {moment}");
            }
        }
        assert_eq!(
            Bucket::ThreeDays.key(
                DateTime::parse_from_rfc3339("2026-02-02T10:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc)
            ),
            "2026-01-31",
            "an unaligned day rounds down to its bucket start"
        );
    }

    #[tokio::test]
    async fn totals_cover_only_the_selected_window() {
        let (store, user) = fixture().await;
        for (range, requests, tokens, succeeded, failed) in [
            (Range::Day, 1, 1310, 1, 0),
            (Range::Week, 2, 1530, 1, 1),
            (Range::Month, 3, 1970, 2, 1),
        ] {
            let totals = store.totals(user, last(range)).await.unwrap();
            let slug = range.slug();
            assert_eq!(totals.requests, requests, "{slug} requests");
            assert_eq!(totals.tokens(), tokens, "{slug} tokens");
            assert_eq!(totals.succeeded, succeeded, "{slug} succeeded");
            assert_eq!(totals.failed, failed, "{slug} failed");
            assert_eq!(totals.unfinished(), 0, "{slug} unfinished");
        }
    }

    #[tokio::test]
    async fn both_bounds_of_the_window_are_respected() {
        let (store, user) = dated_fixture().await;
        let totals = store
            .totals(user, daily("2026-03-01", "2026-03-04"))
            .await
            .unwrap();
        assert_eq!(
            totals.requests, 4,
            "the day before and the day after stay out"
        );
        assert_eq!(totals.tokens(), 165);

        let edge = store
            .totals(user, daily("2026-03-02", "2026-03-02"))
            .await
            .unwrap();
        assert_eq!(
            edge.requests, 1,
            "the last millisecond of the closing day is inside the window"
        );
    }

    #[tokio::test]
    async fn breakdowns_group_by_model_and_provider() {
        let (store, user) = fixture().await;
        let models = store.by_model(user, last(Range::Month)).await.unwrap();
        assert_eq!(
            models
                .iter()
                .map(|row| (row.label.clone(), row.requests, row.tokens))
                .collect::<Vec<_>>(),
            vec![(Some("opus".to_owned()), 2, 1530), (None, 1, 440),],
            "a request that named no model groups under an absent label"
        );

        let providers = store.by_provider(user, last(Range::Month)).await.unwrap();
        assert_eq!(
            providers
                .iter()
                .map(|row| (row.label.clone(), row.requests, row.tokens))
                .collect::<Vec<_>>(),
            vec![
                (Some("claude".to_owned()), 2, 1530),
                (Some("codex".to_owned()), 1, 440),
            ]
        );
    }

    #[tokio::test]
    async fn the_key_breakdown_rolls_every_version_of_a_key_into_one_row() {
        let (store, user) = fixture().await;
        let db = &store.database;
        db.execute_unprepared(&format!(
            "INSERT INTO gateway_keys(id,user_id,name,allowed_providers,created_at) VALUES('key-2','{user}','batch','[\"claude\"]','2026-01-01T00:00:00Z')"
        ))
        .await
        .unwrap();
        db.execute_unprepared("UPDATE gateway_requests SET key_id = 'key-2' WHERE id = 'r-weeks'")
            .await
            .unwrap();
        db.execute_unprepared(
            "UPDATE gateway_requests SET key_version_id = 'v-1' WHERE id = 'r-hour'",
        )
        .await
        .unwrap();
        db.execute_unprepared(
            "UPDATE gateway_requests SET key_version_id = 'v-2' WHERE id = 'r-days'",
        )
        .await
        .unwrap();

        let keys = store.by_key(user, last(Range::Month)).await.unwrap();
        assert_eq!(
            keys.iter()
                .map(|row| (row.label.clone(), row.requests, row.tokens))
                .collect::<Vec<_>>(),
            vec![
                (Some("agent".to_owned()), 2, 1530),
                (Some("batch".to_owned()), 1, 440),
            ]
        );

        let series = store.series_by_key(user, last(Range::Month)).await.unwrap();
        assert_eq!(
            series
                .iter()
                .map(|line| (line.label.clone(), line.total()))
                .collect::<Vec<_>>(),
            vec![
                (Some("agent".to_owned()), 1530),
                (Some("batch".to_owned()), 440)
            ]
        );
    }

    #[tokio::test]
    async fn breakdowns_order_by_tokens_even_when_cost_disagrees() {
        let (store, user) = dated_fixture().await;
        store
            .database
            .execute_unprepared(
                "UPDATE gateway_usage SET cost_nanodollars = 9000000000 WHERE request_id = 'd-1'",
            )
            .await
            .unwrap();
        let models = store
            .by_model(user, daily("2026-03-01", "2026-03-04"))
            .await
            .unwrap();
        assert_eq!(
            models
                .iter()
                .map(|row| (row.label.clone(), row.tokens, row.cost_nanodollars))
                .collect::<Vec<_>>(),
            vec![
                (None, 88, 0),
                (Some("gpt".to_owned()), 44, 0),
                (Some("opus".to_owned()), 33, 9_000_000_000),
            ],
            "the unnamed model has the most tokens and leads despite costing nothing"
        );
    }

    #[tokio::test]
    async fn totals_series_fills_every_bucket() {
        let (store, user) = dated_fixture().await;
        let daily = store
            .totals_series(user, daily("2026-03-01", "2026-03-04"))
            .await
            .unwrap();
        assert_eq!(daily.requests, vec![2, 1, 0, 1]);
        assert_eq!(daily.tokens, vec![33, 44, 0, 88]);
        assert_eq!(daily.cost_nanodollars, vec![0, 0, 0, 0]);

        let three_hourly = store
            .totals_series(
                user,
                window(
                    "2026-03-01T00:00:00Z",
                    "2026-03-01T23:59:59.999Z",
                    Bucket::ThreeHours,
                ),
            )
            .await
            .unwrap();
        assert_eq!(three_hourly.requests.len(), 8);
        assert_eq!(
            three_hourly.requests[3], 2,
            "both morning requests land in the 09:00 bucket"
        );
        assert_eq!(three_hourly.requests.iter().sum::<i64>(), 2);

        let unaligned = store
            .totals_series(
                user,
                window(
                    "2026-03-01T10:30:00Z",
                    "2026-03-02T10:29:59.999Z",
                    Bucket::ThreeHours,
                ),
            )
            .await
            .unwrap();
        assert_eq!(unaligned.requests.len(), 9);
        assert_eq!(
            unaligned.requests[0], 1,
            "the 10:45 request lands in the partial 09:00 bucket; the SQL key and the Rust key agree"
        );
    }

    #[tokio::test]
    async fn labeled_series_fill_buckets_and_order_by_tokens() {
        let (store, user) = dated_fixture().await;
        let models = store
            .series_by_model(user, daily("2026-03-01", "2026-03-04"))
            .await
            .unwrap();
        assert_eq!(
            models,
            vec![
                LabeledSeries {
                    label: None,
                    tokens: vec![0, 0, 0, 88]
                },
                LabeledSeries {
                    label: Some("gpt".to_owned()),
                    tokens: vec![0, 44, 0, 0]
                },
                LabeledSeries {
                    label: Some("opus".to_owned()),
                    tokens: vec![33, 0, 0, 0]
                },
            ]
        );

        let providers = store
            .series_by_provider(user, daily("2026-03-01", "2026-03-04"))
            .await
            .unwrap();
        assert_eq!(
            providers
                .iter()
                .map(|series| (series.label.clone(), series.tokens.clone()))
                .collect::<Vec<_>>(),
            vec![
                (Some("codex".to_owned()), vec![0, 44, 0, 88]),
                (Some("claude".to_owned()), vec![33, 0, 0, 0]),
            ]
        );
    }

    #[tokio::test]
    async fn a_request_without_an_outcome_counts_as_unfinished() {
        let (db, user) = empty_store("open@example.com").await;
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        record(&db, "r-open", "claude", Some("opus"), &now, 5, 5).await;
        let store = UsageStore::new(db);

        let totals = store.totals(user, last(Range::Day)).await.unwrap();
        assert_eq!(totals.requests, 1);
        assert_eq!(totals.succeeded, 0);
        assert_eq!(totals.failed, 0);
        assert_eq!(totals.unfinished(), 1);
    }

    #[tokio::test]
    async fn codex_cached_tokens_are_counted_once() {
        use crate::providers::{Provider, extract_usage};

        let (db, user) = empty_store("codex@example.com").await;
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        db.execute_unprepared(&format!(
            "INSERT INTO gateway_requests(id,request_id,provider,protocol,method,endpoint,requested_model,started_at,request_bytes,response_bytes,client_disconnected,key_id) \
             VALUES('r-codex','r-codex','codex','openai_responses','POST','/providers/codex/v1/responses','gpt','{now}',0,0,FALSE,'{KEY}')"
        ))
        .await
        .unwrap();

        let usage = extract_usage(
            Provider::Codex,
            br#"{"usage":{"input_tokens":1000,"output_tokens":20,"input_tokens_details":{"cached_tokens":400}}}"#,
        );
        db.execute_unprepared(&format!(
            "INSERT INTO gateway_usage(request_id,input_tokens,output_tokens,cache_read_tokens) \
             VALUES('r-codex',{},{},{})",
            usage.input_tokens.unwrap(),
            usage.output_tokens.unwrap(),
            usage.cache_read_tokens.unwrap()
        ))
        .await
        .unwrap();

        let totals = UsageStore::new(db)
            .totals(user, last(Range::Day))
            .await
            .unwrap();
        assert_eq!(
            totals.tokens(),
            1020,
            "the cached tokens the Responses API reports inside input_tokens must not be added twice"
        );
    }

    #[tokio::test]
    async fn cost_sums_across_totals_and_breakdowns() {
        let (store, user) = fixture().await;
        for (id, cost) in [("r-hour", 1_500_000_i64), ("r-days", 2_500_000)] {
            store
                .database
                .execute_unprepared(&format!(
                    "UPDATE gateway_usage SET cost_nanodollars = {cost}, cost_source = 'calculated' WHERE request_id = '{id}'"
                ))
                .await
                .unwrap();
        }

        let totals = store.totals(user, last(Range::Week)).await.unwrap();
        assert_eq!(totals.cost_nanodollars, 4_000_000);
        assert_eq!(totals.unpriced, 0, "both requests in range carry a cost");

        let month = store.totals(user, last(Range::Month)).await.unwrap();
        assert_eq!(
            month.cost_nanodollars, 4_000_000,
            "the unpriced request adds nothing"
        );
        assert_eq!(month.unpriced, 1);

        let models = store.by_model(user, last(Range::Month)).await.unwrap();
        assert_eq!(
            models
                .iter()
                .map(|row| (row.label.clone(), row.cost_nanodollars))
                .collect::<Vec<_>>(),
            vec![(Some("opus".to_owned()), 4_000_000), (None, 0)]
        );

        let series = store.totals_series(user, last(Range::Month)).await.unwrap();
        assert_eq!(series.cost_nanodollars.iter().sum::<i64>(), 4_000_000);
        assert_eq!(series.cost_nanodollars.len(), 11);
    }

    #[tokio::test]
    async fn captured_traffic_is_priced_when_the_migration_runs() {
        let (store, user) = fixture().await;
        store
            .database
            .execute_unprepared(
                "UPDATE gateway_requests SET requested_model = 'claude-sonnet-4-5' WHERE id = 'r-hour'",
            )
            .await
            .unwrap();
        let usage = crate::providers::Usage {
            input_tokens: Some(100),
            output_tokens: Some(10),
            cache_read_tokens: Some(1_000),
            cache_write_tokens: Some(200),
            reasoning_tokens: None,
            raw_json: None,
        };
        let cost = crate::pricing::cost(Some("claude-sonnet-4-5"), &usage);
        let nanodollars = cost.nanodollars.expect("a current model is priced");
        store
            .database
            .execute_unprepared(&format!(
                "UPDATE gateway_usage SET cost_nanodollars = {nanodollars}, cost_source = 'calculated' WHERE request_id = 'r-hour'"
            ))
            .await
            .unwrap();

        let totals = store.totals(user, last(Range::Day)).await.unwrap();
        assert_eq!(totals.cost_nanodollars, nanodollars);
        assert!(totals.cost_nanodollars > 0);
    }

    #[tokio::test]
    async fn traffic_of_another_user_is_not_counted() {
        let (store, _) = fixture().await;
        let stranger = Uuid::now_v7();
        let totals = store.totals(stranger, last(Range::Month)).await.unwrap();
        assert_eq!(totals.requests, 0);
        assert_eq!(totals.tokens(), 0);
        assert!(
            store
                .by_model(stranger, last(Range::Month))
                .await
                .unwrap()
                .is_empty()
        );
        let series = store
            .totals_series(stranger, last(Range::Month))
            .await
            .unwrap();
        assert_eq!(
            series.requests,
            vec![0; 11],
            "an empty window still has every bucket"
        );
    }

    /// `(block_type, tool_name, skill_name, tool_use_id, is_error)`.
    type Fact<'a> = (
        &'a str,
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
        Option<bool>,
    );

    /// One captured request part: a blob referenced by the request, carrying
    /// its facts.
    async fn part(
        db: &DatabaseConnection,
        request_id: &str,
        position: i64,
        blob_id: &str,
        bytes: i64,
        facts: &[Fact<'_>],
    ) {
        db.execute_unprepared(&format!(
            "INSERT OR IGNORE INTO gateway_payload_blobs(id,body,encoding,original_bytes,created_at) \
             VALUES('{blob_id}',x'00','identity',{bytes},'2026-03-01T00:00:00Z')"
        ))
        .await
        .unwrap();
        db.execute_unprepared(&format!(
            "INSERT INTO gateway_payload_part_refs(request_id,direction,path,position,kind,part_id) \
             VALUES('{request_id}','request','tools',{position},'tools','{blob_id}')"
        ))
        .await
        .unwrap();
        for (ordinal, (block_type, tool_name, skill_name, tool_use_id, is_error)) in
            facts.iter().enumerate()
        {
            let quoted =
                |value: Option<&str>| value.map_or("NULL".to_owned(), |value| format!("'{value}'"));
            let is_error = is_error.map_or("NULL".to_owned(), |flag| i32::from(flag).to_string());
            let mcp_server = quoted(tool_name.and_then(crate::payload_facts::mcp_server));
            db.execute_unprepared(&format!(
                "INSERT OR IGNORE INTO gateway_payload_blob_facts(blob_id,ordinal,block_type,tool_name,mcp_server,skill_name,tool_use_id,is_error) \
                 VALUES('{blob_id}',{ordinal},'{block_type}',{},{mcp_server},{},{},{is_error})",
                quoted(*tool_name),
                quoted(*skill_name),
                quoted(*tool_use_id),
            ))
            .await
            .unwrap();
        }
    }

    async fn tool_fixture() -> (UsageStore, Uuid) {
        let (db, user) = empty_store("tools@example.com").await;
        for (id, started_at) in [
            ("t-1", "2026-03-01T10:00:00.000Z"),
            ("t-2", "2026-03-02T10:00:00.000Z"),
            ("t-out", "2026-03-09T10:00:00.000Z"),
        ] {
            record(&db, id, "claude", Some("opus"), started_at, 1, 1).await;
        }
        let definition = |name: &'static str| ("tool_definition", Some(name), None, None, None);
        for (request, position) in [("t-1", 0), ("t-2", 0), ("t-out", 0)] {
            part(
                &db,
                request,
                position,
                "def-bash",
                1_000,
                &[definition("Bash")],
            )
            .await;
            part(
                &db,
                request,
                position + 1,
                "def-mcp",
                3_000,
                &[definition(
                    "mcp__claude_ai_Microsoft_365__outlook_send_mail",
                )],
            )
            .await;
        }
        part(&db, "t-1", 2, "def-read", 500, &[definition("Read")]).await;
        part(
            &db,
            "t-2",
            2,
            "def-codex",
            4_000,
            &[definition("exec"), definition("wait")],
        )
        .await;
        part(
            &db,
            "t-1",
            3,
            "call-bash-1",
            100,
            &[("tool_use", Some("Bash"), None, Some("toolu_1"), None)],
        )
        .await;
        part(
            &db,
            "t-1",
            4,
            "result-bash-1",
            5_000,
            &[("tool_result", None, None, Some("toolu_1"), Some(true))],
        )
        .await;
        for request in ["t-1", "t-2"] {
            part(
                &db,
                request,
                5,
                "call-bash-2",
                100,
                &[("tool_use", Some("Bash"), None, Some("toolu_2"), None)],
            )
            .await;
            part(
                &db,
                request,
                6,
                "result-bash-2",
                2_000,
                &[("tool_result", None, None, Some("toolu_2"), Some(false))],
            )
            .await;
        }
        part(
            &db,
            "t-2",
            7,
            "call-skill",
            100,
            &[(
                "tool_use",
                Some("Skill"),
                Some("unslop"),
                Some("toolu_3"),
                None,
            )],
        )
        .await;
        for request in ["t-1", "t-2"] {
            part(
                &db,
                request,
                12,
                "result-skill",
                900,
                &[("tool_result", None, None, Some("toolu_3"), Some(false))],
            )
            .await;
        }
        db.execute_unprepared(
            "INSERT INTO gateway_request_metrics(request_id,total_bytes,created_at) VALUES('t-1',10000,'2026-03-01T10:00:00Z'),('t-2',20000,'2026-03-02T10:00:00Z'); \
             UPDATE gateway_usage SET cost_nanodollars = 1000000 WHERE request_id IN ('t-1','t-2')",
        )
        .await
        .unwrap();
        part(
            &db,
            "t-2",
            8,
            "result-orphan",
            700,
            &[("tool_result", None, None, Some("toolu_lost"), Some(true))],
        )
        .await;
        part(
            &db,
            "t-out",
            9,
            "call-read",
            100,
            &[("tool_use", Some("Read"), None, Some("toolu_4"), None)],
        )
        .await;
        part(
            &db,
            "t-2",
            10,
            "result-read",
            300,
            &[("tool_result", None, None, Some("toolu_4"), Some(false))],
        )
        .await;
        for request in ["t-1", "t-2"] {
            part(
                &db,
                request,
                11,
                "call-glob",
                100,
                &[("tool_use", Some("Glob"), None, None, None)],
            )
            .await;
        }
        (UsageStore::new(db), user)
    }

    #[tokio::test]
    async fn tool_usage_counts_each_call_once_and_costs_every_request_that_carried_it() {
        let (store, user) = tool_fixture().await;
        let usage = store
            .tool_usage(user, daily("2026-03-01", "2026-03-02"))
            .await
            .unwrap();
        let tool = |label: Option<&str>, calls, bytes, cost_nanodollars| ToolCalls {
            label: label.map(str::to_owned),
            calls,
            bytes,
            cost_nanodollars,
        };
        assert_eq!(
            usage.tools,
            vec![
                tool(Some("Bash"), 2, 9_200, 975_000),
                tool(Some("Glob"), 1, 100, 15_000),
                tool(Some("Skill"), 1, 1_000, 140_000),
                tool(None, 0, 700, 35_000),
                tool(Some("Read"), 0, 800, 65_000),
                tool(Some("exec"), 0, 2_000, 100_000),
                tool(
                    Some("mcp__claude_ai_Microsoft_365__outlook_send_mail"),
                    0,
                    6_000,
                    450_000,
                ),
                tool(Some("wait"), 0, 2_000, 100_000),
            ],
            "Bash: its definition in both requests (2,000 bytes), two calls counted once each \
             (200), and their results (7,000); every part costs its byte share of each request \
             that carried it, 100 per byte in t-1 and 50 in t-2. A call without an id counts \
             once by blob. A result whose call fell before the window, or was never captured, \
             still adds its bytes. The codex container splits between exec and wait."
        );
        assert_eq!(
            usage.skills,
            vec![SkillCalls {
                label: "unslop".to_owned(),
                calls: 1,
                bytes: 1_000,
                cost_nanodollars: 140_000,
            }],
            "the Skill call and the body it returned, once each; the body replayed by t-1 \
             costs that request its share too"
        );
        assert_eq!(
            usage.mcp_servers(),
            vec![McpServer {
                label: "claude_ai_Microsoft_365".to_owned(),
                calls: 0,
                tools: 1,
                bytes: 6_000,
                cost_nanodollars: 450_000,
            }]
        );

        let stranger = store
            .tool_usage(Uuid::now_v7(), daily("2026-03-01", "2026-03-02"))
            .await
            .unwrap();
        assert_eq!(stranger, ToolUsage::default());
    }

    #[test]
    fn mcp_servers_order_by_calls_then_name() {
        let tool = |label: &str, calls, bytes| ToolCalls {
            label: Some(label.to_owned()),
            calls,
            bytes,
            cost_nanodollars: bytes * 10,
        };
        let usage = ToolUsage {
            tools: vec![
                tool("mcp__b__x", 5, 1),
                tool("mcp__a__y", 2, 10),
                tool("Bash", 100, 500),
                tool("mcp__c__z", 0, 900),
                tool("mcp__d__z", 0, 100),
                tool("mcp__a__w", 0, 5),
            ],
            skills: vec![],
        };
        assert_eq!(
            usage
                .mcp_servers()
                .iter()
                .map(|server| (
                    server.label.as_str(),
                    server.calls,
                    server.tools,
                    server.bytes,
                    server.cost_nanodollars
                ))
                .collect::<Vec<_>>(),
            [
                ("b", 5, 1, 1, 10),
                ("a", 2, 2, 15, 150),
                ("c", 0, 1, 900, 9_000),
                ("d", 0, 1, 100, 1_000)
            ],
            "a server with no calls sorts by name, not by size; a plain tool belongs to no server"
        );
    }

    #[tokio::test]
    async fn context_totals_sum_the_metrics_of_the_window_for_one_user() {
        let (db, user) = empty_store("user@example.com").await;
        let other = Uuid::now_v7();
        db.execute_unprepared(&format!("INSERT INTO users(id,email_normalized,email_display,role,status,auth_version,created_at,updated_at) VALUES('{other}','o@example.com','o@example.com','user','active',0,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')")).await.unwrap();
        db.execute_unprepared(&format!("INSERT INTO gateway_keys(id,user_id,name,allowed_providers,created_at) VALUES('key-2','{other}','agent','[\"claude\"]','2026-01-01T00:00:00Z')")).await.unwrap();
        let metrics = |id: &str, key: &str, started_at: &str, tools: i64, errors: i64| {
            let db = db.clone();
            let statement = format!(
                "INSERT INTO gateway_requests(id,request_id,provider,protocol,method,endpoint,started_at,request_bytes,response_bytes,client_disconnected,key_id) \
                 VALUES('{id}','{id}','claude','anthropic_messages','POST','/v1/messages','{started_at}',0,0,FALSE,'{key}'); \
                 INSERT INTO gateway_request_metrics(request_id,tool_definition_bytes,system_bytes,user_text_bytes,assistant_text_bytes,thinking_bytes,tool_use_bytes,tool_result_bytes,other_bytes,total_bytes,tools_offered,tools_invoked,tool_result_errors,cache_breakpoints,created_at) \
                 VALUES('{id}',100,20,30,40,5,10,60,1,266,{tools},2,{errors},3,'{started_at}')"
            );
            async move { db.execute_unprepared(&statement).await.unwrap() }
        };
        metrics("in-1", KEY, "2026-03-02T10:00:00.000Z", 7, 1).await;
        metrics("in-2", KEY, "2026-03-03T10:00:00.000Z", 8, 0).await;
        db.execute_unprepared(
            "INSERT INTO gateway_usage(request_id,input_tokens,output_tokens,cost_nanodollars) VALUES('in-1',1,1,266000)",
        )
        .await
        .unwrap();
        metrics("before", KEY, "2026-02-27T10:00:00.000Z", 9, 4).await;
        metrics("theirs", "key-2", "2026-03-02T11:00:00.000Z", 9, 4).await;

        let store = UsageStore::new(db);
        let totals = store
            .context(user, daily("2026-03-01", "2026-03-03"))
            .await
            .unwrap();
        assert_eq!(
            totals,
            ContextTotals {
                requests: 2,
                tool_definition_bytes: 200,
                system_bytes: 40,
                user_text_bytes: 60,
                assistant_text_bytes: 80,
                thinking_bytes: 10,
                tool_use_bytes: 20,
                tool_result_bytes: 120,
                other_bytes: 2,
                total_bytes: 532,
                tools_offered: 15,
                tools_invoked: 4,
                tool_result_errors: 1,
                cache_breakpoints: 6,
                tool_definition_cost_nanodollars: 100_000,
                system_cost_nanodollars: 20_000,
                user_text_cost_nanodollars: 30_000,
                assistant_text_cost_nanodollars: 40_000,
                thinking_cost_nanodollars: 5_000,
                tool_use_cost_nanodollars: 10_000,
                tool_result_cost_nanodollars: 60_000,
                other_cost_nanodollars: 1_000,
            },
            "the priced request's cost is split by each part's share of its bytes; the unpriced one adds nothing"
        );
        assert_eq!(ContextTotals::estimated_tokens(3529), 1000);

        let theirs = store
            .context(other, daily("2026-03-01", "2026-03-03"))
            .await
            .unwrap();
        assert_eq!(theirs.requests, 1);
        assert_eq!(
            store
                .context(user, daily("2026-04-01", "2026-04-03"))
                .await
                .unwrap(),
            ContextTotals::default()
        );
    }
}
