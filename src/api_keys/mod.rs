use crate::telemetry::timestamp;
use anyhow::{Context, Result, bail};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::http::{HeaderMap, HeaderName};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, FromQueryResult, Statement};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use uuid::Uuid;

pub const API_KEY_HEADER: HeaderName = HeaderName::from_static("x-aegis-api-key");
const CACHE_TTL: Duration = Duration::from_secs(30);
const REVOKED_KEY_RETENTION: chrono::TimeDelta = chrono::TimeDelta::days(7);
const MAX_CACHE_ENTRIES: usize = 1024;

#[derive(Clone)]
pub struct KeyStore {
    database: DatabaseConnection,
    cache: Arc<RwLock<HashMap<[u8; 32], CachedKey>>>,
}

#[derive(Clone, Debug)]
pub struct AuthenticatedKey {
    pub id: String,
    pub version_id: String,
    pub user_id: String,
    pub allowed_providers: HashSet<String>,
}

#[derive(Clone)]
struct CachedKey {
    key: AuthenticatedKey,
    expires_at: Instant,
}

#[derive(Debug, FromQueryResult)]
struct KeyRow {
    key_id: String,
    version_id: String,
    user_id: String,
    key_hash: String,
    allowed_providers: String,
    key_revoked_at: Option<String>,
    version_revoked_at: Option<String>,
}

#[derive(Debug, FromQueryResult, Serialize)]
pub struct KeySummary {
    pub id: String,
    pub name: String,
    pub allowed_providers: String,
    pub created_at: String,
    pub revoked_at: Option<String>,
    pub active_versions: i64,
    pub last_used_at: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthenticationError {
    #[error("missing Aegis API key")]
    Missing,
    #[error("invalid Aegis API key")]
    Invalid,
    #[error("this key cannot use provider {0}")]
    ProviderNotAllowed(String),
    #[error("authentication backend failed")]
    Backend(#[source] sea_orm::DbErr),
}

impl KeyStore {
    pub fn new(database: DatabaseConnection) -> Self {
        Self {
            database,
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn create(
        &self,
        user_id: Uuid,
        name: &str,
        providers: &[String],
    ) -> Result<(String, String)> {
        validate(name, providers)?;
        let key_id = Uuid::now_v7().simple().to_string();
        let allowed = normalized_providers(providers)?;
        self.database.execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO gateway_keys (id, user_id, name, allowed_providers, created_at) VALUES (?, ?, ?, ?, ?)",
            [key_id.clone().into(), user_id.to_string().into(), name.trim().to_owned().into(), allowed.into(), timestamp().into()],
        )).await.context("failed to store API key")?;
        let plaintext = self.create_version(&key_id).await?;
        Ok((key_id, plaintext))
    }

    pub async fn rotate(&self, user_id: Uuid, key_id: &str) -> Result<String> {
        let row = self.database.execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE gateway_keys SET name = name WHERE id = ? AND user_id = ? AND revoked_at IS NULL",
            [key_id.to_owned().into(), user_id.to_string().into()],
        )).await?;
        if row.rows_affected() == 0 {
            bail!("active key not found");
        }
        let plaintext = self.create_version(key_id).await?;
        let version_id = parse_version_id(&plaintext).expect("generated key has a version id");
        self.database.execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE gateway_key_versions SET revoked_at = ? WHERE key_id = ? AND id != ? AND revoked_at IS NULL",
            [timestamp().into(), key_id.to_owned().into(), version_id.into()],
        )).await?;
        self.cache.write().await.clear();
        Ok(plaintext)
    }

    async fn create_version(&self, key_id: &str) -> Result<String> {
        let version_id = Uuid::now_v7().simple().to_string();
        let mut secret = [0_u8; 32];
        getrandom::fill(&mut secret)
            .map_err(|e| anyhow::anyhow!("failed to generate API key: {e}"))?;
        let encoded = URL_SAFE_NO_PAD.encode(secret);
        secret.fill(0);
        let plaintext = format!("aegis_sk_{version_id}_{encoded}");
        let key_hash = Argon2::default()
            .hash_password(plaintext.as_bytes())
            .map_err(|e| anyhow::anyhow!("failed to hash API key: {e}"))?
            .to_string();
        let prefix = plaintext.chars().take(24).collect::<String>();
        self.database.execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO gateway_key_versions (id, key_id, key_hash, prefix, created_at) VALUES (?, ?, ?, ?, ?)",
            [version_id.into(), key_id.to_owned().into(), key_hash.into(), prefix.into(), timestamp().into()],
        )).await.context("failed to store API key version")?;
        Ok(plaintext)
    }

    pub async fn authenticate(
        &self,
        headers: &mut HeaderMap,
        provider_id: &str,
    ) -> Result<AuthenticatedKey, AuthenticationError> {
        let presented = headers
            .remove(&API_KEY_HEADER)
            .ok_or(AuthenticationError::Missing)?
            .to_str()
            .ok()
            .and_then(parse_api_key)
            .ok_or(AuthenticationError::Invalid)?
            .to_owned();
        let cache_key: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        if let Some(key) = self.cached(&cache_key).await {
            return authorize_provider(key, provider_id);
        }
        let version_id = parse_version_id(&presented).ok_or(AuthenticationError::Invalid)?;
        let row = KeyRow::find_by_statement(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT k.id key_id, v.id version_id, k.user_id, v.key_hash, k.allowed_providers, k.revoked_at key_revoked_at, v.revoked_at version_revoked_at FROM gateway_key_versions v JOIN gateway_keys k ON k.id = v.key_id WHERE v.id = ?",
            [version_id.into()],
        )).one(&self.database).await.map_err(AuthenticationError::Backend)?.ok_or(AuthenticationError::Invalid)?;
        if row.key_revoked_at.is_some() || row.version_revoked_at.is_some() {
            return Err(AuthenticationError::Invalid);
        }
        let hash = row.key_hash;
        let candidate = presented;
        let valid = tokio::task::spawn_blocking(move || {
            PasswordHash::new(&hash).ok().is_some_and(|h| {
                Argon2::default()
                    .verify_password(candidate.as_bytes(), &h)
                    .is_ok()
            })
        })
        .await
        .unwrap_or(false);
        if !valid {
            return Err(AuthenticationError::Invalid);
        }
        let key = AuthenticatedKey {
            id: row.key_id,
            version_id: row.version_id,
            user_id: row.user_id,
            allowed_providers: serde_json::from_str(&row.allowed_providers)
                .map_err(|_| AuthenticationError::Invalid)?,
        };
        self.insert_cache(cache_key, key.clone()).await;
        let _ = self
            .database
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "UPDATE gateway_key_versions SET last_used_at = ? WHERE id = ?",
                [timestamp().into(), key.version_id.clone().into()],
            ))
            .await;
        authorize_provider(key, provider_id)
    }

    pub async fn list_for_user(&self, user_id: Uuid) -> Result<Vec<KeySummary>, sea_orm::DbErr> {
        let cutoff = (chrono::Utc::now() - REVOKED_KEY_RETENTION)
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        KeySummary::find_by_statement(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT k.id, k.name, k.allowed_providers, k.created_at, k.revoked_at, SUM(CASE WHEN v.revoked_at IS NULL THEN 1 ELSE 0 END) active_versions, MAX(v.last_used_at) last_used_at FROM gateway_keys k LEFT JOIN gateway_key_versions v ON v.key_id = k.id WHERE k.user_id = ? AND (k.revoked_at IS NULL OR k.revoked_at >= ?) GROUP BY k.id ORDER BY k.created_at",
            [user_id.to_string().into(), cutoff.into()],
        )).all(&self.database).await
    }

    pub async fn revoke(&self, user_id: Uuid, id: &str) -> Result<bool, sea_orm::DbErr> {
        let now = timestamp();
        let result = self.database.execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE gateway_keys SET revoked_at = ? WHERE id = ? AND user_id = ? AND revoked_at IS NULL",
            [now.clone().into(), id.to_owned().into(), user_id.to_string().into()],
        )).await?;
        let revoked = result.rows_affected() > 0;
        if revoked {
            self.database.execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "UPDATE gateway_key_versions SET revoked_at = ? WHERE key_id = ? AND revoked_at IS NULL",
                [now.into(), id.to_owned().into()],
            )).await?;
        }
        self.cache.write().await.clear();
        Ok(revoked)
    }

    async fn cached(&self, digest: &[u8; 32]) -> Option<AuthenticatedKey> {
        let mut cache = self.cache.write().await;
        let entry = cache.get(digest)?;
        if entry.expires_at <= Instant::now() {
            cache.remove(digest);
            return None;
        }
        Some(entry.key.clone())
    }

    async fn insert_cache(&self, digest: [u8; 32], key: AuthenticatedKey) {
        let mut cache = self.cache.write().await;
        if cache.len() >= MAX_CACHE_ENTRIES {
            cache.clear();
        }
        cache.insert(
            digest,
            CachedKey {
                key,
                expires_at: Instant::now() + CACHE_TTL,
            },
        );
    }
}

fn validate(name: &str, providers: &[String]) -> Result<()> {
    if name.trim().is_empty() || name.len() > 100 {
        bail!("key name must contain 1 to 100 characters");
    }
    if providers.is_empty() {
        bail!("at least one provider must be allowed");
    }
    Ok(())
}
fn normalized_providers(providers: &[String]) -> Result<String> {
    let mut values = providers.to_vec();
    values.sort();
    values.dedup();
    Ok(serde_json::to_string(&values)?)
}
fn parse_api_key(value: &str) -> Option<&str> {
    if let Some((scheme, credential)) = value.split_once(' ') {
        return (scheme.eq_ignore_ascii_case("bearer") && !credential.is_empty())
            .then_some(credential);
    }
    (!value.is_empty()).then_some(value)
}
fn parse_version_id(key: &str) -> Option<String> {
    let rest = key.strip_prefix("aegis_sk_")?;
    let (id, secret) = rest.split_once('_')?;
    (id.len() == 32 && !secret.is_empty()).then(|| id.to_owned())
}
fn authorize_provider(
    key: AuthenticatedKey,
    provider_id: &str,
) -> Result<AuthenticatedKey, AuthenticationError> {
    if !key.allowed_providers.contains(provider_id) {
        return Err(AuthenticationError::ProviderNotAllowed(
            provider_id.to_owned(),
        ));
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use axum::http::HeaderValue;
    use chrono::{SecondsFormat, TimeDelta, Utc};
    use sea_orm::Database;
    use sea_orm_migration::MigratorTrait;

    async fn fixture() -> (KeyStore, Uuid) {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&database, None).await.unwrap();
        let user = Uuid::now_v7();
        database.execute_unprepared(&format!("INSERT INTO users(id,email_normalized,email_display,role,status,auth_version,created_at,updated_at) VALUES('{user}','user@example.com','user@example.com','user','active',0,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')")).await.unwrap();
        (KeyStore::new(database), user)
    }

    fn headers(key: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(API_KEY_HEADER, HeaderValue::from_str(key).unwrap());
        headers
    }

    #[tokio::test]
    async fn rotation_keeps_the_logical_key_and_revokes_the_old_version() {
        let (store, user) = fixture().await;
        let (id, first) = store
            .create(user, "agent", &["claude".into()])
            .await
            .unwrap();
        let second = store.rotate(user, &id).await.unwrap();
        assert!(matches!(
            store.authenticate(&mut headers(&first), "claude").await,
            Err(AuthenticationError::Invalid)
        ));
        let second_auth = store
            .authenticate(&mut headers(&second), "claude")
            .await
            .unwrap();
        assert_eq!(second_auth.id, id);
        assert_eq!(
            store.list_for_user(user).await.unwrap()[0].active_versions,
            1
        );
    }

    #[tokio::test]
    async fn revoking_a_key_revokes_its_versions_and_ages_out_of_the_listing() {
        let (store, user) = fixture().await;
        let (id, secret) = store
            .create(user, "agent", &["claude".into()])
            .await
            .unwrap();
        assert!(store.revoke(user, &id).await.unwrap());
        assert!(matches!(
            store.authenticate(&mut headers(&secret), "claude").await,
            Err(AuthenticationError::Invalid)
        ));

        let listed = store.list_for_user(user).await.unwrap();
        assert_eq!(listed.len(), 1, "a fresh revocation stays visible");
        assert!(listed[0].revoked_at.is_some());
        assert_eq!(
            listed[0].active_versions, 0,
            "revoking the key revokes its versions"
        );

        let stale = (Utc::now() - REVOKED_KEY_RETENTION - TimeDelta::days(1))
            .to_rfc3339_opts(SecondsFormat::Millis, true);
        store
            .database
            .execute_unprepared(&format!(
                "UPDATE gateway_keys SET revoked_at = '{stale}' WHERE id = '{id}'"
            ))
            .await
            .unwrap();
        assert!(
            store.list_for_user(user).await.unwrap().is_empty(),
            "a long-revoked key drops out of the listing"
        );
    }

    #[tokio::test]
    async fn key_is_owner_scoped_and_provider_scoped() {
        let (store, user) = fixture().await;
        let (id, secret) = store
            .create(user, "agent", &["claude".into()])
            .await
            .unwrap();
        assert!(matches!(
            store.authenticate(&mut headers(&secret), "codex").await,
            Err(AuthenticationError::ProviderNotAllowed(_))
        ));
        assert!(store.rotate(Uuid::now_v7(), &id).await.is_err());
        assert!(!store.revoke(Uuid::now_v7(), &id).await.unwrap());
    }
}
