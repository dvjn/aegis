use sea_orm::ConnectionTrait;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// No payload foreign keys: compact source facts survive raw-payload retention.
const SCHEMA: &str = "
CREATE TABLE gateway_analytics_tools (
 id INTEGER PRIMARY KEY,
 attribution_key TEXT NOT NULL UNIQUE,
 tool_name TEXT,
 skill_name TEXT
);
INSERT INTO gateway_analytics_tools(attribution_key, tool_name, skill_name)
 VALUES ('[null,null]', NULL, NULL);
CREATE TABLE gateway_analytics_requests (
 id INTEGER PRIMARY KEY AUTOINCREMENT,
 request_id TEXT NOT NULL UNIQUE,
 fact_version INTEGER NOT NULL DEFAULT 1,
 contributions_dirty INTEGER NOT NULL DEFAULT 0 CHECK(contributions_dirty IN (0, 1))
);
CREATE TABLE gateway_analytics_tool_contributions (
 request_id INTEGER NOT NULL REFERENCES gateway_analytics_requests(id),
 tool_id INTEGER NOT NULL REFERENCES gateway_analytics_tools(id),
 definition_count INTEGER NOT NULL CHECK(definition_count >= 0),
 definition_num TEXT NOT NULL,
 definition_den TEXT NOT NULL,
 transmission_num TEXT NOT NULL,
 transmission_den TEXT NOT NULL,
 PRIMARY KEY(request_id, tool_id)
) WITHOUT ROWID;
CREATE INDEX gateway_analytics_contribution_tool ON gateway_analytics_tool_contributions(tool_id, request_id);
CREATE TABLE gateway_analytics_tool_identities (
 id INTEGER PRIMARY KEY,
 scope_key TEXT NOT NULL,
 provider TEXT NOT NULL,
 identity_kind TEXT NOT NULL CHECK(identity_kind IN ('call_id', 'content_hash')),
 identity_key TEXT NOT NULL,
 conversation_available INTEGER NOT NULL DEFAULT 0 CHECK(conversation_available = 0),
 state TEXT NOT NULL CHECK(state IN ('unresolved', 'resolved', 'ambiguous')),
 tool_id INTEGER REFERENCES gateway_analytics_tools(id),
 UNIQUE(scope_key, provider, identity_kind, identity_key),
 CHECK((state = 'resolved') = (tool_id IS NOT NULL))
);
CREATE TABLE gateway_analytics_tool_variants (
 id INTEGER PRIMARY KEY,
 identity_id INTEGER NOT NULL REFERENCES gateway_analytics_tool_identities(id),
 kind TEXT NOT NULL CHECK(kind IN ('tool_use', 'tool_result')),
 bytes INTEGER NOT NULL CHECK(bytes >= 0),
 observed_tool_id INTEGER NOT NULL REFERENCES gateway_analytics_tools(id),
 UNIQUE(identity_id, kind, bytes, observed_tool_id)
);
CREATE TABLE gateway_analytics_tool_appearances (
 request_id INTEGER NOT NULL REFERENCES gateway_analytics_requests(id),
 variant_id INTEGER NOT NULL REFERENCES gateway_analytics_tool_variants(id),
 multiplicity INTEGER NOT NULL CHECK(typeof(multiplicity) = 'integer' AND multiplicity > 0),
 PRIMARY KEY(request_id, variant_id)
) WITHOUT ROWID;
CREATE INDEX gateway_analytics_appearance_dependency ON gateway_analytics_tool_appearances(variant_id, request_id);
CREATE TRIGGER gateway_analytics_require_withdrawal BEFORE DELETE ON gateway_requests
WHEN EXISTS (SELECT 1 FROM gateway_analytics_requests r JOIN gateway_analytics_tool_appearances a ON a.request_id = r.id WHERE r.request_id = OLD.id)
BEGIN SELECT RAISE(ABORT, 'analytics fact withdrawal required before request deletion'); END;
";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(SCHEMA).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DROP TRIGGER gateway_analytics_require_withdrawal;
DROP TABLE gateway_analytics_tool_appearances;
DROP TABLE gateway_analytics_tool_variants;
DROP TABLE gateway_analytics_tool_identities;
DROP TABLE gateway_analytics_tool_contributions;
DROP TABLE gateway_analytics_requests;
DROP TABLE gateway_analytics_tools;",
            )
            .await?;
        Ok(())
    }
}
