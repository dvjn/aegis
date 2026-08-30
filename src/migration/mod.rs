use sea_orm_migration::prelude::*;

mod m20260815_000001_identity_oauth;
mod m20260829_000002_gateway_capture;
mod m20260830_000003_gateway_keys;
mod m20260830_000004_backfill_anthropic_usage;
mod m20260830_000005_deduplicate_payloads;
mod m20260830_000006_remove_empty_usage;
mod m20260830_000007_semantic_payload_parts;
mod m20260831_000008_reset_oauth_scopes;

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
        ]
    }
}
