use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use sea_orm::{DatabaseConnection, DbBackend, FromQueryResult, Statement};
use serde::Serialize;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Range {
    #[default]
    Day,
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
        match self {
            Self::Day => "24h",
            Self::Week => "7d",
            Self::Month => "1m",
        }
    }

    pub fn from_slug(value: Option<&str>) -> Self {
        match value {
            Some("7d") => Self::Week,
            Some("1m") => Self::Month,
            _ => Self::Day,
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
}

#[derive(Clone)]
pub struct UsageStore {
    database: DatabaseConnection,
}

const TOTALS_SQL: &str = "SELECT COUNT(*) requests, \
     COALESCE(SUM(CASE WHEN r.http_status < 400 AND r.error_message IS NULL THEN 1 ELSE 0 END), 0) succeeded, \
     COALESCE(SUM(CASE WHEN r.http_status >= 400 OR r.error_message IS NOT NULL THEN 1 ELSE 0 END), 0) failed, \
     COALESCE(SUM(u.input_tokens), 0) input_tokens, \
     COALESCE(SUM(u.cache_read_tokens), 0) cache_read_tokens, \
     COALESCE(SUM(u.cache_write_tokens), 0) cache_write_tokens, \
     COALESCE(SUM(u.output_tokens), 0) output_tokens \
     FROM gateway_requests r \
     JOIN gateway_keys k ON k.id = r.key_id \
     LEFT JOIN gateway_usage u ON u.request_id = r.id \
     WHERE k.user_id = ? AND r.started_at >= ?";

const BREAKDOWN_SQL: &str = "SELECT {column} label, COUNT(*) requests, \
     COALESCE(SUM(u.input_tokens), 0) + \
     COALESCE(SUM(u.cache_read_tokens), 0) + \
     COALESCE(SUM(u.cache_write_tokens), 0) + \
     COALESCE(SUM(u.output_tokens), 0) tokens \
     FROM gateway_requests r \
     JOIN gateway_keys k ON k.id = r.key_id \
     LEFT JOIN gateway_usage u ON u.request_id = r.id \
     WHERE k.user_id = ? AND r.started_at >= ? \
     GROUP BY {column} ORDER BY tokens DESC, requests DESC, label";

impl UsageStore {
    pub fn new(database: DatabaseConnection) -> Self {
        Self { database }
    }

    pub async fn totals(&self, user_id: Uuid, range: Range) -> Result<UsageTotals, sea_orm::DbErr> {
        Ok(
            UsageTotals::find_by_statement(self.statement(TOTALS_SQL, user_id, range))
                .one(&self.database)
                .await?
                .unwrap_or_default(),
        )
    }

    pub async fn by_model(
        &self,
        user_id: Uuid,
        range: Range,
    ) -> Result<Vec<UsageGroup>, sea_orm::DbErr> {
        self.breakdown("r.requested_model", user_id, range).await
    }

    pub async fn by_provider(
        &self,
        user_id: Uuid,
        range: Range,
    ) -> Result<Vec<UsageGroup>, sea_orm::DbErr> {
        self.breakdown("r.provider", user_id, range).await
    }

    async fn breakdown(
        &self,
        column: &str,
        user_id: Uuid,
        range: Range,
    ) -> Result<Vec<UsageGroup>, sea_orm::DbErr> {
        let sql = BREAKDOWN_SQL.replace("{column}", column);
        UsageGroup::find_by_statement(self.statement(&sql, user_id, range))
            .all(&self.database)
            .await
    }

    fn statement(&self, sql: &str, user_id: Uuid, range: Range) -> Statement {
        Statement::from_sql_and_values(
            DbBackend::Sqlite,
            sql,
            [user_id.to_string().into(), cutoff(range).into()],
        )
    }
}

fn cutoff(range: Range) -> String {
    let start: DateTime<Utc> = Utc::now() - range.span();
    start.to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use sea_orm::{ConnectionTrait, Database};
    use sea_orm_migration::MigratorTrait;

    const KEY: &str = "key-1";

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

    async fn fixture() -> (UsageStore, Uuid) {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let user = Uuid::now_v7();
        db.execute_unprepared(&format!("INSERT INTO users(id,email_normalized,email_display,role,status,auth_version,created_at,updated_at) VALUES('{user}','user@example.com','user@example.com','user','active',0,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')")).await.unwrap();
        db.execute_unprepared(&format!("INSERT INTO gateway_keys(id,user_id,name,allowed_providers,created_at) VALUES('{KEY}','{user}','agent','[\"claude\"]','2026-01-01T00:00:00Z')")).await.unwrap();

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

    #[tokio::test]
    async fn totals_cover_only_the_selected_range() {
        let (store, user) = fixture().await;
        for (range, requests, tokens, succeeded, failed) in [
            (Range::Day, 1, 1310, 1, 0),
            (Range::Week, 2, 1530, 1, 1),
            (Range::Month, 3, 1970, 2, 1),
        ] {
            let totals = store.totals(user, range).await.unwrap();
            assert_eq!(totals.requests, requests, "{} requests", range.slug());
            assert_eq!(totals.tokens(), tokens, "{} tokens", range.slug());
            assert_eq!(totals.succeeded, succeeded, "{} succeeded", range.slug());
            assert_eq!(totals.failed, failed, "{} failed", range.slug());
            assert_eq!(totals.unfinished(), 0, "{} unfinished", range.slug());
        }
    }

    #[tokio::test]
    async fn breakdowns_group_by_model_and_provider() {
        let (store, user) = fixture().await;
        let models = store.by_model(user, Range::Month).await.unwrap();
        assert_eq!(
            models
                .iter()
                .map(|row| (row.label.clone(), row.requests, row.tokens))
                .collect::<Vec<_>>(),
            vec![(Some("opus".to_owned()), 2, 1530), (None, 1, 440),],
            "a request that named no model groups under an absent label"
        );

        let providers = store.by_provider(user, Range::Month).await.unwrap();
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
    async fn a_request_without_an_outcome_counts_as_unfinished() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let user = Uuid::now_v7();
        db.execute_unprepared(&format!("INSERT INTO users(id,email_normalized,email_display,role,status,auth_version,created_at,updated_at) VALUES('{user}','open@example.com','open@example.com','user','active',0,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')")).await.unwrap();
        db.execute_unprepared(&format!("INSERT INTO gateway_keys(id,user_id,name,allowed_providers,created_at) VALUES('{KEY}','{user}','agent','[\"claude\"]','2026-01-01T00:00:00Z')")).await.unwrap();
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        record(&db, "r-open", "claude", Some("opus"), &now, 5, 5).await;
        let store = UsageStore::new(db);

        let totals = store.totals(user, Range::Day).await.unwrap();
        assert_eq!(totals.requests, 1);
        assert_eq!(totals.succeeded, 0);
        assert_eq!(totals.failed, 0);
        assert_eq!(totals.unfinished(), 1);
    }

    #[tokio::test]
    async fn traffic_of_another_user_is_not_counted() {
        let (store, _) = fixture().await;
        let stranger = Uuid::now_v7();
        let totals = store.totals(stranger, Range::Month).await.unwrap();
        assert_eq!(totals.requests, 0);
        assert_eq!(totals.tokens(), 0);
        assert!(
            store
                .by_model(stranger, Range::Month)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
