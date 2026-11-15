//! Providers and guardrails stored in the database at two layers: the instance,
//! managed by superusers, and each user's own.
//!
//! Provider IDs share one namespace per user: a user sees their own providers
//! and the instance's, and an ID can't exist in both. Deleted providers stay
//! as rows with `deleted_at` set, only as history: nothing reads them back, and
//! their IDs are free to reuse. Guardrails combine so a
//! user can only make them stricter (see [`effective_guardrails`]).
use crate::{
    config::{
        Config, GuardrailConfig, GuardrailsConfig, GuardrailsMode, ProviderConfig, ProviderKind,
        RegexGuardrailConfig, validate_detectors, validate_providers,
    },
    telemetry::timestamp,
};
use anyhow::Context;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, FromQueryResult, Statement, Value};
use std::collections::{BTreeSet, HashMap};
use uuid::Uuid;

const IMPORT_PENDING: &str = "config_import_pending";
const INSTANCE_GUARDRAILS: &str = "guardrails";
/// Users with an active API key or connected app granted provider `?`.
const GRANT_HOLDERS: &str = "SELECT c.user_id FROM gateway_clients c
    JOIN json_each(c.allowed_providers) j LEFT JOIN gateway_keys k ON k.id = c.id
    WHERE j.value = ? AND (c.kind = 'oauth' OR k.revoked_at IS NULL)";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Instance,
    User(Uuid),
}

impl Scope {
    fn user_value(self) -> Value {
        match self {
            Self::Instance => Value::String(None),
            Self::User(id) => id.to_string().into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    /// Safe to show to the person who made the change.
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Backend(#[from] anyhow::Error),
}

impl From<sea_orm::DbErr> for SettingsError {
    fn from(error: sea_orm::DbErr) -> Self {
        Self::Backend(error.into())
    }
}

fn invalid(message: impl Into<String>) -> SettingsError {
    SettingsError::Invalid(message.into())
}

/// A provider with the user that owns it, `None` for the instance.
#[derive(Clone, Debug)]
pub struct StoredProvider {
    pub owner: Option<String>,
    pub config: ProviderConfig,
}

#[derive(FromQueryResult)]
struct ProviderRow {
    id: String,
    user_id: Option<String>,
    kind: String,
    base_url: String,
}

#[derive(Clone)]
pub struct SettingsStore {
    database: DatabaseConnection,
}

impl SettingsStore {
    pub fn new(database: DatabaseConnection) -> Self {
        Self { database }
    }

    /// Every provider at every layer, for the gateway's in-memory registry.
    pub async fn all_providers(&self) -> anyhow::Result<Vec<StoredProvider>> {
        ProviderRow::find_by_statement(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT id, user_id, kind, base_url FROM providers WHERE deleted_at IS NULL ORDER BY id",
        ))
        .all(&self.database)
        .await
        .context("failed to load providers")?
        .into_iter()
        .map(|row| {
            Ok(StoredProvider {
                config: ProviderConfig {
                    kind: ProviderKind::from_parts(&row.kind, Some(row.base_url))
                        .with_context(|| format!("stored provider {:?} is invalid", row.id))?,
                    id: row.id,
                },
                owner: row.user_id,
            })
        })
        .collect()
    }

    /// The providers of one layer only.
    pub async fn providers(&self, scope: Scope) -> anyhow::Result<Vec<ProviderConfig>> {
        let owner = match scope {
            Scope::Instance => None,
            Scope::User(id) => Some(id.to_string()),
        };
        Ok(self
            .all_providers()
            .await?
            .into_iter()
            .filter(|provider| provider.owner == owner)
            .map(|provider| provider.config)
            .collect())
    }

    /// The IDs a user can grant: their own providers and the instance's.
    pub async fn visible_provider_ids(&self, user: Uuid) -> anyhow::Result<Vec<String>> {
        let user = user.to_string();
        let mut ids: Vec<_> = self
            .all_providers()
            .await?
            .into_iter()
            .filter(|provider| provider.owner.as_ref().is_none_or(|owner| *owner == user))
            .map(|provider| provider.config.id)
            .collect();
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    pub async fn create_provider(
        &self,
        scope: Scope,
        provider: &ProviderConfig,
    ) -> Result<(), SettingsError> {
        validate_providers(std::slice::from_ref(provider))
            .map_err(|error| invalid(format!("{error:#}")))?;
        let tx = crate::db::begin_immediate(&self.database).await?;
        let (sql, values): (&str, Vec<Value>) = match scope {
            Scope::Instance => (
                "SELECT (SELECT COUNT(*) FROM providers WHERE id = ? AND user_id IS NULL AND deleted_at IS NULL) instance_taken,
                        (SELECT COUNT(*) FROM providers WHERE id = ? AND user_id IS NOT NULL AND deleted_at IS NULL) user_taken",
                vec![provider.id.clone().into(), provider.id.clone().into()],
            ),
            Scope::User(user) => (
                "SELECT (SELECT COUNT(*) FROM providers WHERE id = ? AND user_id IS NULL AND deleted_at IS NULL) instance_taken,
                        (SELECT COUNT(*) FROM providers WHERE id = ? AND user_id = ? AND deleted_at IS NULL) user_taken",
                vec![
                    provider.id.clone().into(),
                    provider.id.clone().into(),
                    user.to_string().into(),
                ],
            ),
        };
        let row = tx
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                sql,
                values,
            ))
            .await?
            .context("collision check returned no row")?;
        let instance_taken: i64 = row.try_get("", "instance_taken")?;
        let user_taken: i64 = row.try_get("", "user_taken")?;
        match scope {
            _ if instance_taken > 0 => {
                return Err(invalid(format!(
                    "An instance provider named \"{}\" already exists.",
                    provider.id
                )));
            }
            Scope::Instance if user_taken > 0 => {
                return Err(invalid(format!(
                    "{user_taken} user(s) already have a provider named \"{}\". Choose another ID.",
                    provider.id
                )));
            }
            Scope::User(_) if user_taken > 0 => {
                return Err(invalid(format!(
                    "You already have a provider named \"{}\".",
                    provider.id
                )));
            }
            _ => {}
        }
        let now = timestamp();
        tx.execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO providers (id, user_id, kind, base_url, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
            [
                provider.id.clone().into(),
                scope.user_value(),
                provider.kind.type_name().into(),
                provider.kind.base_url().to_owned().into(),
                now.clone().into(),
                now.into(),
            ],
        ))
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Changes the upstream of a provider. The ID and type stay fixed because
    /// client URLs and usage history refer to them. `None` restores the type's
    /// default upstream. Returns whether the provider exists.
    pub async fn update_provider(
        &self,
        scope: Scope,
        id: &str,
        base_url: Option<String>,
    ) -> Result<bool, SettingsError> {
        let Some(current) = self
            .providers(scope)
            .await?
            .into_iter()
            .find(|provider| provider.id == id)
        else {
            return Ok(false);
        };
        let updated = ProviderConfig {
            id: current.id,
            kind: ProviderKind::from_parts(current.kind.type_name(), base_url)?,
        };
        validate_providers(std::slice::from_ref(&updated))
            .map_err(|error| invalid(format!("{error:#}")))?;
        let (filter, mut values) = owner_filter(scope);
        values.insert(0, updated.kind.base_url().to_owned().into());
        values.insert(1, timestamp().into());
        values.insert(2, updated.id.clone().into());
        crate::db::writer(&self.database)
            .await?
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                format!(
                    "UPDATE providers SET base_url = ?, updated_at = ? WHERE id = ? AND deleted_at IS NULL AND {filter}"
                ),
                values,
            ))
            .await?;
        Ok(true)
    }

    /// Deletes a provider and removes it from every API key and connected app
    /// that was granted it, so a later provider with the same ID does not
    /// inherit those grants. The row stays, marked `deleted_at`, as a record of
    /// what the ID pointed at for past usage. Returns whether it existed.
    pub async fn delete_provider(&self, scope: Scope, id: &str) -> Result<bool, SettingsError> {
        let tx = crate::db::begin_immediate(&self.database).await?;
        let (filter, mut values) = owner_filter(scope);
        let now = timestamp();
        values.splice(0..0, [now.clone().into(), now.into(), id.to_owned().into()]);
        let deleted = tx
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                format!(
                    "UPDATE providers SET deleted_at = ?, updated_at = ? WHERE id = ? AND deleted_at IS NULL AND {filter}"
                ),
                values,
            ))
            .await?
            .rows_affected()
            > 0;
        if deleted {
            // gateway_keys triggers keep the api_key rows of gateway_clients in step.
            for (table, extra) in [
                ("gateway_keys", ""),
                ("gateway_clients", " AND kind = 'oauth'"),
            ] {
                let user_filter = match scope {
                    Scope::Instance => "",
                    Scope::User(_) => " AND user_id = ?",
                };
                let mut values: Vec<Value> = vec![id.to_owned().into(), id.to_owned().into()];
                if let Scope::User(user) = scope {
                    values.push(user.to_string().into());
                }
                tx.execute_raw(Statement::from_sql_and_values(
                    DbBackend::Sqlite,
                    format!(
                        "UPDATE {table} SET allowed_providers = (
                            SELECT json_group_array(value) FROM json_each({table}.allowed_providers) WHERE value <> ?)
                        WHERE EXISTS (SELECT 1 FROM json_each({table}.allowed_providers) WHERE value = ?){user_filter}{extra}"
                    ),
                    values,
                ))
                .await?;
            }
        }
        tx.commit().await?;
        Ok(deleted)
    }

    /// The users holding an active API key or connected app grant to an
    /// instance provider, as (user ID, email), sorted by email.
    pub async fn instance_provider_users(&self, id: &str) -> anyhow::Result<Vec<(String, String)>> {
        #[derive(FromQueryResult)]
        struct Row {
            id: String,
            email: String,
        }
        Ok(Row::find_by_statement(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            format!(
                "SELECT u.id, u.email_display email FROM users u WHERE u.id IN ({GRANT_HOLDERS}) ORDER BY u.email_display"
            ),
            [id.into()],
        ))
        .all(&self.database)
        .await?
        .into_iter()
        .map(|row| (row.id, row.email))
        .collect())
    }

    /// Replaces an instance provider with a copy owned by each user who holds
    /// a grant to it, so their keys and apps keep working, and removes it from
    /// everyone else's grants. Returns how many users got a copy, or `None`
    /// when no such instance provider exists.
    pub async fn move_to_users(&self, id: &str) -> Result<Option<usize>, SettingsError> {
        let tx = crate::db::begin_immediate(&self.database).await?;
        let Some(row) = tx
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT kind, base_url FROM providers WHERE id = ? AND user_id IS NULL AND deleted_at IS NULL",
                [id.into()],
            ))
            .await?
        else {
            return Ok(None);
        };
        let kind: String = row.try_get("", "kind")?;
        let base_url: String = row.try_get("", "base_url")?;
        let now = timestamp();
        tx.execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE providers SET deleted_at = ?, updated_at = ? WHERE id = ? AND user_id IS NULL AND deleted_at IS NULL",
            [now.clone().into(), now.clone().into(), id.into()],
        ))
        .await?;
        let moved = tx
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                format!(
                    "INSERT INTO providers (id, user_id, kind, base_url, created_at, updated_at)
                    SELECT ?, u.id, ?, ?, ?, ? FROM users u WHERE u.id IN ({GRANT_HOLDERS})"
                ),
                [
                    id.into(),
                    kind.into(),
                    base_url.into(),
                    now.clone().into(),
                    now.into(),
                    id.into(),
                ],
            ))
            .await?
            .rows_affected() as usize;
        // Revoked keys of the new owners keep their grant; it is theirs now.
        for (table, extra) in [
            ("gateway_keys", ""),
            ("gateway_clients", " AND kind = 'oauth'"),
        ] {
            tx.execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                format!(
                    "UPDATE {table} SET allowed_providers = (
                        SELECT json_group_array(value) FROM json_each({table}.allowed_providers) WHERE value <> ?)
                    WHERE EXISTS (SELECT 1 FROM json_each({table}.allowed_providers) WHERE value = ?)
                    AND user_id NOT IN (SELECT user_id FROM providers WHERE id = ? AND user_id IS NOT NULL AND deleted_at IS NULL){extra}"
                ),
                [id.into(), id.into(), id.into()],
            ))
            .await?;
        }
        tx.commit().await?;
        Ok(Some(moved))
    }

    /// How many active API keys and connected apps hold a grant to each
    /// provider ID of this layer. Instance grants are counted across users.
    pub async fn grant_counts(&self, scope: Scope) -> anyhow::Result<HashMap<String, (i64, i64)>> {
        #[derive(FromQueryResult)]
        struct Row {
            id: String,
            keys: i64,
            apps: i64,
        }
        let (filter, values) = match scope {
            Scope::Instance => ("", Vec::new()),
            Scope::User(user) => (" AND c.user_id = ?", vec![Value::from(user.to_string())]),
        };
        Ok(Row::find_by_statement(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            format!(
                "SELECT j.value id,
                    SUM(CASE WHEN c.kind = 'api_key' THEN 1 ELSE 0 END) keys,
                    SUM(CASE WHEN c.kind = 'oauth' THEN 1 ELSE 0 END) apps
                FROM gateway_clients c JOIN json_each(c.allowed_providers) j
                LEFT JOIN gateway_keys k ON k.id = c.id
                WHERE (c.kind = 'oauth' OR k.revoked_at IS NULL){filter}
                GROUP BY j.value"
            ),
            values,
        ))
        .all(&self.database)
        .await?
        .into_iter()
        .map(|row| (row.id, (row.keys, row.apps)))
        .collect())
    }

    pub async fn instance_guardrails(&self) -> anyhow::Result<GuardrailsConfig> {
        Ok(self
            .setting(INSTANCE_GUARDRAILS)
            .await?
            .map(|json| {
                serde_json::from_str(&json).context("stored instance guardrails are invalid")
            })
            .transpose()?
            .unwrap_or_default())
    }

    pub async fn user_guardrails(&self, user: Uuid) -> anyhow::Result<GuardrailsConfig> {
        Ok(self
            .all_user_guardrails()
            .await?
            .remove(&user.to_string())
            .unwrap_or_default())
    }

    pub async fn all_user_guardrails(&self) -> anyhow::Result<HashMap<String, GuardrailsConfig>> {
        #[derive(FromQueryResult)]
        struct Row {
            user_id: String,
            config: String,
        }
        Row::find_by_statement(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT user_id, config FROM user_guardrails",
        ))
        .all(&self.database)
        .await?
        .into_iter()
        .map(|row| {
            let config = serde_json::from_str(&row.config).with_context(|| {
                format!("stored guardrails of user {} are invalid", row.user_id)
            })?;
            Ok((row.user_id, config))
        })
        .collect()
    }

    pub async fn guardrails(&self, scope: Scope) -> anyhow::Result<GuardrailsConfig> {
        match scope {
            Scope::Instance => self.instance_guardrails().await,
            Scope::User(user) => self.user_guardrails(user).await,
        }
    }

    /// Validates and stores one layer. A user's regex detectors may not reuse
    /// an instance detector's name, and must compile alongside the instance's.
    pub async fn set_guardrails(
        &self,
        scope: Scope,
        guardrails: &GuardrailsConfig,
    ) -> Result<(), SettingsError> {
        validate_detectors(guardrails).map_err(|error| invalid(format!("{error:#}")))?;
        let instance = self.instance_guardrails().await?;
        if let Scope::User(_) = scope {
            if let Some(name) = guardrails
                .regex
                .detectors
                .keys()
                .find(|name| instance.regex.detectors.contains_key(*name))
            {
                return Err(invalid(format!(
                    "The instance already defines a pattern named \"{name}\". Choose another name."
                )));
            }
            validate_detectors(&effective_guardrails(&instance, Some(guardrails)))
                .map_err(|error| invalid(format!("{error:#}")))?;
        }
        let json = serde_json::to_string(guardrails).context("failed to encode guardrails")?;
        match scope {
            Scope::Instance => self.set_setting(INSTANCE_GUARDRAILS, &json).await?,
            Scope::User(user) => {
                crate::db::writer(&self.database)
                    .await?
                    .execute_raw(Statement::from_sql_and_values(
                        DbBackend::Sqlite,
                        "INSERT INTO user_guardrails (user_id, config, updated_at) VALUES (?, ?, ?)
                         ON CONFLICT(user_id) DO UPDATE SET config = excluded.config, updated_at = excluded.updated_at",
                        [user.to_string().into(), json.into(), timestamp().into()],
                    ))
                    .await?;
            }
        }
        Ok(())
    }

    /// Imports the configuration file's providers and guardrails into the
    /// instance layer, once, for a database the providers migration marked as
    /// coming from an older release. Returns whether it imported.
    pub async fn import_config_once(&self, config: &Config) -> anyhow::Result<bool> {
        if self.setting(IMPORT_PENDING).await?.is_none() {
            if !config.providers.is_empty() || config.guardrails.enabled {
                tracing::warn!(
                    "providers and guardrails in the configuration file are ignored; manage them in the web interface or with `aegis provider` and `aegis guardrails`"
                );
            }
            return Ok(false);
        }
        validate_providers(&config.providers)?;
        let tx = crate::db::begin_immediate(&self.database).await?;
        let now = timestamp();
        for provider in &config.providers {
            tx.execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT OR IGNORE INTO providers (id, user_id, kind, base_url, created_at, updated_at) VALUES (?, NULL, ?, ?, ?, ?)",
                [
                    provider.id.clone().into(),
                    provider.kind.type_name().into(),
                    provider.kind.base_url().to_owned().into(),
                    now.clone().into(),
                    now.clone().into(),
                ],
            ))
            .await?;
        }
        tx.execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT OR IGNORE INTO settings (key, value, updated_at) VALUES (?, ?, ?)",
            [
                INSTANCE_GUARDRAILS.into(),
                serde_json::to_string(&config.guardrails)?.into(),
                now.into(),
            ],
        ))
        .await?;
        tx.execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "DELETE FROM settings WHERE key = ?",
            [IMPORT_PENDING.into()],
        ))
        .await?;
        tx.commit().await?;
        tracing::info!(
            providers = config.providers.len(),
            "imported instance providers and guardrails from the configuration file"
        );
        Ok(true)
    }

    async fn setting(&self, key: &str) -> anyhow::Result<Option<String>> {
        self.database
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT value FROM settings WHERE key = ?",
                [key.into()],
            ))
            .await?
            .map(|row| row.try_get("", "value").map_err(Into::into))
            .transpose()
    }

    async fn set_setting(&self, key: &str, value: &str) -> anyhow::Result<()> {
        crate::db::writer(&self.database)
            .await?
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO settings (key, value, updated_at) VALUES (?, ?, ?)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                [key.into(), value.into(), timestamp().into()],
            ))
            .await?;
        Ok(())
    }
}

fn owner_filter(scope: Scope) -> (&'static str, Vec<Value>) {
    match scope {
        Scope::Instance => ("user_id IS NULL", Vec::new()),
        Scope::User(user) => ("user_id = ?", vec![user.to_string().into()]),
    }
}

/// The guardrails that apply to a user's requests. The instance layer is a
/// floor: a layer that is on stays on, mask beats observe, and detectors from
/// both layers run. On a regex name clash the instance pattern wins.
pub fn effective_guardrails(
    instance: &GuardrailsConfig,
    user: Option<&GuardrailsConfig>,
) -> GuardrailsConfig {
    let layers: Vec<&GuardrailsConfig> = [Some(instance), user]
        .into_iter()
        .flatten()
        .filter(|layer| layer.enabled)
        .collect();
    if layers.is_empty() {
        return GuardrailsConfig::default();
    }
    let mode = if layers
        .iter()
        .any(|layer| layer.mode == GuardrailsMode::Mask)
    {
        GuardrailsMode::Mask
    } else {
        GuardrailsMode::Observe
    };
    let secret_layers: Vec<_> = layers
        .iter()
        .filter(|layer| layer.secrets.enabled)
        .collect();
    let detectors = if secret_layers
        .iter()
        .any(|layer| layer.secrets.detectors.is_none())
    {
        None
    } else {
        Some(
            secret_layers
                .iter()
                .flat_map(|layer| layer.secrets.detectors.iter().flatten().cloned())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        )
    };
    let mut regex = RegexGuardrailConfig::default();
    // Users first, so the instance's pattern replaces a user's of the same name.
    for layer in layers.iter().rev().filter(|layer| layer.regex.enabled) {
        regex.enabled = true;
        regex.detectors.extend(layer.regex.detectors.clone());
    }
    GuardrailsConfig {
        enabled: true,
        mode,
        secrets: GuardrailConfig {
            enabled: !secret_layers.is_empty(),
            detectors,
        },
        regex,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    async fn store() -> (SettingsStore, DatabaseConnection) {
        use sea_orm_migration::MigratorTrait;
        let database = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
        crate::migration::Migrator::up(&database, None)
            .await
            .unwrap();
        (SettingsStore::new(database.clone()), database)
    }

    async fn user(database: &DatabaseConnection) -> Uuid {
        let id = Uuid::now_v7();
        database
            .execute_unprepared(&format!(
                "INSERT INTO users(id,email_normalized,email_display,role,status,auth_version,created_at,updated_at) VALUES('{id}','{id}@example.com','{id}@example.com','user','active',0,'2026-01-01','2026-01-01')"
            ))
            .await
            .unwrap();
        id
    }

    fn claude(id: &str) -> ProviderConfig {
        ProviderConfig {
            id: id.into(),
            kind: ProviderKind::from_parts("claude_subscription", None).unwrap(),
        }
    }

    #[tokio::test]
    async fn users_see_their_own_providers_and_the_instance_ones() {
        let (store, database) = store().await;
        let (alice, bob) = (user(&database).await, user(&database).await);
        store
            .create_provider(Scope::Instance, &claude("shared"))
            .await
            .unwrap();
        store
            .create_provider(Scope::User(alice), &claude("work"))
            .await
            .unwrap();
        store
            .create_provider(Scope::User(bob), &claude("work"))
            .await
            .unwrap();
        assert_eq!(
            store.visible_provider_ids(alice).await.unwrap(),
            ["shared", "work"]
        );
        assert!(matches!(
            store
                .create_provider(Scope::User(alice), &claude("shared"))
                .await,
            Err(SettingsError::Invalid(_))
        ));
        assert!(matches!(
            store.create_provider(Scope::Instance, &claude("work")).await,
            Err(SettingsError::Invalid(message)) if message.starts_with("2 user(s)")
        ));
        assert!(matches!(
            store
                .create_provider(Scope::User(alice), &claude("work"))
                .await,
            Err(SettingsError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn deleting_a_provider_removes_it_from_grants() {
        let (store, database) = store().await;
        let alice = user(&database).await;
        store
            .create_provider(Scope::User(alice), &claude("work"))
            .await
            .unwrap();
        store
            .create_provider(Scope::User(alice), &claude("home"))
            .await
            .unwrap();
        let keys = crate::api_keys::KeyStore::new(database.clone());
        let (key, _) = keys
            .create(alice, "laptop", &["home".into(), "work".into()])
            .await
            .unwrap();
        assert!(
            store
                .delete_provider(Scope::User(alice), "work")
                .await
                .unwrap()
        );
        assert!(
            !store
                .delete_provider(Scope::User(alice), "work")
                .await
                .unwrap()
        );
        let listed = keys.list_for_user(alice).await.unwrap();
        let summary = listed.iter().find(|k| k.id == key).unwrap();
        assert_eq!(summary.allowed_providers, r#"["home"]"#);

        let deleted_rows = || async {
            database
                .query_one_raw(Statement::from_string(
                    DbBackend::Sqlite,
                    "SELECT COUNT(*) n FROM providers WHERE id = 'work' AND deleted_at IS NOT NULL",
                ))
                .await
                .unwrap()
                .unwrap()
                .try_get::<i64>("", "n")
                .unwrap()
        };
        assert_eq!(deleted_rows().await, 1, "the deleted row stays as history");
        assert_eq!(store.visible_provider_ids(alice).await.unwrap(), ["home"]);

        // The ID is free again, and deleting the new one keeps both records.
        store
            .create_provider(Scope::User(alice), &claude("work"))
            .await
            .unwrap();
        assert!(
            store
                .delete_provider(Scope::User(alice), "work")
                .await
                .unwrap()
        );
        assert_eq!(deleted_rows().await, 2);
        store
            .create_provider(Scope::Instance, &claude("work"))
            .await
            .expect("a deleted user provider does not block an instance one");
    }

    #[tokio::test]
    async fn moving_an_instance_provider_gives_each_user_of_it_a_copy() {
        let (store, database) = store().await;
        let (alice, bob, carol) = (
            user(&database).await,
            user(&database).await,
            user(&database).await,
        );
        let mut shared = claude("shared");
        shared.kind =
            ProviderKind::from_parts("claude_subscription", Some("https://proxy.example".into()))
                .unwrap();
        store
            .create_provider(Scope::Instance, &shared)
            .await
            .unwrap();
        store
            .create_provider(Scope::Instance, &claude("other"))
            .await
            .unwrap();
        let keys = crate::api_keys::KeyStore::new(database.clone());
        keys.create(alice, "a", &["shared".into(), "other".into()])
            .await
            .unwrap();
        keys.create(bob, "b", &["shared".into()]).await.unwrap();
        let (revoked, _) = keys
            .create(carol, "c", &["shared".into(), "other".into()])
            .await
            .unwrap();
        keys.revoke(carol, &revoked).await.unwrap();

        let users = store.instance_provider_users("shared").await.unwrap();
        assert_eq!(users.len(), 2);
        assert_eq!(store.move_to_users("shared").await.unwrap(), Some(2));
        assert_eq!(store.move_to_users("shared").await.unwrap(), None);

        assert_eq!(store.providers(Scope::Instance).await.unwrap().len(), 1);
        for owner in [alice, bob] {
            let mine = store.providers(Scope::User(owner)).await.unwrap();
            assert_eq!(mine.len(), 1);
            assert_eq!(mine[0].kind.base_url(), "https://proxy.example");
        }
        assert!(
            store
                .providers(Scope::User(carol))
                .await
                .unwrap()
                .is_empty()
        );
        let grants = |user| {
            let keys = keys.clone();
            async move {
                keys.list_for_user(user).await.unwrap()[0]
                    .allowed_providers
                    .clone()
            }
        };
        assert_eq!(grants(alice).await, r#"["other","shared"]"#);
        assert_eq!(grants(bob).await, r#"["shared"]"#);
        assert_eq!(grants(carol).await, r#"["other"]"#);
    }

    #[tokio::test]
    async fn only_the_upstream_of_a_provider_changes() {
        let (store, _) = store().await;
        store
            .create_provider(Scope::Instance, &claude("claude"))
            .await
            .unwrap();
        assert!(
            store
                .update_provider(
                    Scope::Instance,
                    "claude",
                    Some("https://proxy.example".into())
                )
                .await
                .unwrap()
        );
        assert_eq!(
            store.providers(Scope::Instance).await.unwrap()[0]
                .kind
                .base_url(),
            "https://proxy.example"
        );
        store
            .update_provider(Scope::Instance, "claude", None)
            .await
            .unwrap();
        assert_eq!(
            store.providers(Scope::Instance).await.unwrap()[0]
                .kind
                .base_url(),
            "https://api.anthropic.com"
        );
        assert!(
            !store
                .update_provider(Scope::Instance, "missing", None)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn the_import_runs_only_for_databases_from_an_older_release() {
        let (store, database) = store().await;
        let mut config = Config::test_default();
        config.providers = vec![claude("claude")];
        config.guardrails.enabled = true;
        // A new database: the migration ran before any user existed.
        assert!(!store.import_config_once(&config).await.unwrap());
        assert!(store.providers(Scope::Instance).await.unwrap().is_empty());

        // An upgraded database: the migration marked it.
        database
            .execute_unprepared(
                "INSERT INTO settings (key, value, updated_at) VALUES ('config_import_pending', '', '')",
            )
            .await
            .unwrap();
        assert!(store.import_config_once(&config).await.unwrap());
        assert_eq!(store.providers(Scope::Instance).await.unwrap().len(), 1);
        assert!(store.instance_guardrails().await.unwrap().enabled);
        assert!(!store.import_config_once(&config).await.unwrap());
    }

    #[tokio::test]
    async fn user_patterns_cannot_reuse_instance_names() {
        let (store, database) = store().await;
        let alice = user(&database).await;
        let mut instance = GuardrailsConfig {
            enabled: true,
            ..Default::default()
        };
        instance.regex.enabled = true;
        instance
            .regex
            .detectors
            .insert("employee_id".into(), r"\bEMP-\d{6}\b".into());
        store
            .set_guardrails(Scope::Instance, &instance)
            .await
            .unwrap();
        let mut mine = GuardrailsConfig::default();
        mine.regex
            .detectors
            .insert("employee_id".into(), "x".into());
        assert!(matches!(
            store.set_guardrails(Scope::User(alice), &mine).await,
            Err(SettingsError::Invalid(_))
        ));
        mine.regex.detectors = BTreeMap::from([("ticket".into(), r"\bTKT-\d+\b".into())]);
        store
            .set_guardrails(Scope::User(alice), &mine)
            .await
            .unwrap();
        assert_eq!(
            store
                .user_guardrails(alice)
                .await
                .unwrap()
                .regex
                .detectors
                .len(),
            1
        );
    }

    #[test]
    fn users_can_only_tighten_the_instance_guardrails() {
        let instance = GuardrailsConfig {
            enabled: true,
            mode: GuardrailsMode::Mask,
            secrets: GuardrailConfig {
                enabled: true,
                detectors: Some(vec!["aws_access_key".into()]),
            },
            regex: RegexGuardrailConfig {
                enabled: true,
                detectors: BTreeMap::from([("employee_id".into(), "EMP".into())]),
            },
        };
        let user = GuardrailsConfig {
            enabled: true,
            mode: GuardrailsMode::Observe,
            secrets: GuardrailConfig {
                enabled: true,
                detectors: Some(vec!["github_token".into()]),
            },
            regex: RegexGuardrailConfig {
                enabled: true,
                detectors: BTreeMap::from([
                    ("employee_id".into(), "mine".into()),
                    ("ticket".into(), "TKT".into()),
                ]),
            },
        };
        let effective = effective_guardrails(&instance, Some(&user));
        assert_eq!(effective.mode, GuardrailsMode::Mask);
        assert_eq!(
            effective.secrets.detectors.as_deref(),
            Some(&["aws_access_key".to_owned(), "github_token".to_owned()][..])
        );
        assert_eq!(effective.regex.detectors["employee_id"], "EMP");
        assert_eq!(effective.regex.detectors.len(), 2);

        let off = GuardrailsConfig::default();
        assert!(!effective_guardrails(&off, Some(&off)).enabled);
        let only_user = effective_guardrails(&off, Some(&user));
        assert_eq!(only_user.mode, GuardrailsMode::Observe);
        assert!(only_user.enabled);
    }
}
