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
}
