use sea_orm_migration::prelude::*;

mod m20260815_000001_identity_oauth;
mod m20260829_000002_gateway_capture;
mod m20260830_000003_gateway_keys;
mod m20260830_000004_backfill_anthropic_usage;
mod m20260830_000005_deduplicate_payloads;
mod m20260830_000006_remove_empty_usage;
mod m20260830_000007_semantic_payload_parts;
mod m20260831_000008_reset_oauth_scopes;
mod m20260901_000009_normalize_codex_input_tokens;
mod m20260901_000010_gateway_usage_cost;
mod m20260901_000011_model_prices;
mod m20260905_000012_message_content_parts;
mod m20260905_000013_background_jobs;
mod m20260905_000014_payload_blob_facts;
mod m20260905_000015_request_metrics;
mod m20260905_000017_part_refs_request_index;
mod m20260906_000018_analytics_revisions;
mod m20260906_000019_analytics_tool_facts;
mod m20260906_000020_analytics_source_identity;
mod m20260906_000021_analytics_ownership_epoch;
mod m20260906_000022_analytics_key_revisions;
mod m20260906_000023_analytics_generation_queues;
mod m20260906_000024_analytics_backfill_checkpoint;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260815_000001_identity_oauth::Migration),
            Box::new(m20260829_000002_gateway_capture::Migration),
            Box::new(m20260830_000003_gateway_keys::Migration),
            Box::new(m20260830_000004_backfill_anthropic_usage::Migration),
            Box::new(m20260830_000005_deduplicate_payloads::Migration),
            Box::new(m20260830_000006_remove_empty_usage::Migration),
            Box::new(m20260830_000007_semantic_payload_parts::Migration),
            Box::new(m20260831_000008_reset_oauth_scopes::Migration),
            Box::new(m20260901_000009_normalize_codex_input_tokens::Migration),
            Box::new(m20260901_000010_gateway_usage_cost::Migration),
            Box::new(m20260901_000011_model_prices::Migration),
            Box::new(m20260905_000012_message_content_parts::Migration),
            Box::new(m20260905_000013_background_jobs::Migration),
            Box::new(m20260905_000014_payload_blob_facts::Migration),
            Box::new(m20260905_000015_request_metrics::Migration),
            Box::new(m20260905_000017_part_refs_request_index::Migration),
            Box::new(m20260906_000018_analytics_revisions::Migration),
            Box::new(m20260906_000019_analytics_tool_facts::Migration),
            Box::new(m20260906_000020_analytics_source_identity::Migration),
            Box::new(m20260906_000021_analytics_ownership_epoch::Migration),
            Box::new(m20260906_000022_analytics_key_revisions::Migration),
            Box::new(m20260906_000023_analytics_generation_queues::Migration),
            Box::new(m20260906_000024_analytics_backfill_checkpoint::Migration),
        ]
    }
}
