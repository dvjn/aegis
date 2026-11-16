use chrono::{DateTime, Utc};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseTransaction, DbBackend, EntityTrait,
    QueryFilter, Set, Statement, sea_query::Expr,
};
use std::collections::BTreeMap;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{
    IssuedAccessToken, OAuthConfig, OAuthService, TOKEN_DOMAIN, TOKEN_PREFIX, add_duration,
    new_opaque_value, parse_stored_date, scope_allows, validate_client_id, validate_issuer,
};
use crate::domain::{
    auth::Principal,
    entity::{
        oauth_access_tokens, oauth_clients, oauth_refresh_token_families, oauth_refresh_tokens,
    },
    error::DomainError,
    secrets::hmac_digest,
};

pub struct ConnectedClient {
    pub client_id: String,
    pub client_name: Option<String>,
    pub scope: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    /// The providers of a machine paired with the gateway.
    pub providers: Option<Vec<String>>,
}

#[derive(Clone, Copy)]
pub(crate) struct GrantContext<'a> {
    pub user_id: &'a str,
    pub client_id: &'a str,
    pub issuer: &'a str,
    pub redirect_uri: Option<&'a str>,
    pub resource: &'a str,
    pub scope: &'a str,
    pub auth_version: i64,
    pub now: DateTime<Utc>,
}

impl OAuthService {
    pub(super) async fn issue_initial_grant(
        &self,
        tx: &DatabaseTransaction,
        grant: GrantContext<'_>,
    ) -> Result<(Uuid, IssuedAccessToken), DomainError> {
        let (family_id, refresh_token) = if scope_allows(grant.scope, "offline_access") {
            let family_id = Uuid::now_v7();
            let absolute_expires_at =
                add_duration(grant.now, OAuthConfig::REFRESH_ABSOLUTE_LIFETIME)?;
            oauth_refresh_token_families::ActiveModel {
                id: Set(family_id.to_string()),
                user_id: Set(grant.user_id.to_owned()),
                client_id: Set(grant.client_id.to_owned()),
                issuer: Set(grant.issuer.to_owned()),
                resource: Set(grant.resource.to_owned()),
                scope: Set(grant.scope.to_owned()),
                auth_version: Set(grant.auth_version),
                created_at: Set(grant.now.to_rfc3339()),
                absolute_expires_at: Set(absolute_expires_at.to_rfc3339()),
                revoked_at: Set(None),
                revocation_reason: Set(None),
            }
            .insert(tx)
            .await?;
            let refresh = self
                .insert_refresh_token(tx, family_id, 0, grant.now, absolute_expires_at)
                .await?;
            (Some(family_id), Some(refresh))
        } else {
            (None, None)
        };
        let (token_id, value, expires_in) = self.insert_access_token(tx, grant, family_id).await?;
        Ok((
            token_id,
            IssuedAccessToken {
                value,
                refresh_token,
                scope: grant.scope.to_owned(),
                expires_in,
            },
        ))
    }

    pub(super) async fn insert_access_token(
        &self,
        tx: &DatabaseTransaction,
        grant: GrantContext<'_>,
        family_id: Option<Uuid>,
    ) -> Result<(Uuid, String, u64), DomainError> {
        let (token_id, selector, secret, value) = new_opaque_value(TOKEN_PREFIX);
        let secret = Zeroizing::new(secret);
        let digest = hmac_digest(
            &self.config.hmac_key,
            TOKEN_DOMAIN,
            &selector,
            secret.as_slice(),
        );
        // Only tokens that come with a refresh token follow the configured lifetime.
        let lifetime = if family_id.is_some() {
            self.config.access_token_lifetime
        } else {
            OAuthConfig::UNREFRESHABLE_ACCESS_TOKEN_LIFETIME
        };
        let expires_at = add_duration(grant.now, lifetime)?;
        oauth_access_tokens::ActiveModel {
            id: Set(token_id.to_string()),
            secret_digest: Set(digest.to_vec()),
            key_id: Set(self.config.key_id.clone()),
            user_id: Set(grant.user_id.to_owned()),
            client_id: Set(grant.client_id.to_owned()),
            issuer: Set(grant.issuer.to_owned()),
            redirect_uri: Set(grant.redirect_uri.map(str::to_owned)),
            resource: Set(grant.resource.to_owned()),
            scope: Set(grant.scope.to_owned()),
            family_id: Set(family_id.map(|value| value.to_string())),
            auth_version: Set(grant.auth_version),
            created_at: Set(grant.now.to_rfc3339()),
            expires_at: Set(expires_at.to_rfc3339()),
            revoked_at: Set(None),
        }
        .insert(tx)
        .await?;
        Ok((token_id, value, lifetime.as_secs()))
    }
}

impl OAuthService {
    pub async fn list_connected_clients(
        &self,
        principal: &Principal,
        issuer: &str,
    ) -> Result<Vec<ConnectedClient>, DomainError> {
        validate_issuer(issuer).map_err(|_| DomainError::InvalidInput("invalid issuer"))?;
        let user_id = principal.user_id().to_string();
        let auth_version = principal.identity.auth_version;
        let now = self.clock.now();
        let grants = self
            .load_grants(&self.db, vec![user_id.clone()], None, now)
            .await?;
        let any_use = self.token_use(issuer, None, now);
        let mut connected: BTreeMap<String, ConnectedClient> = BTreeMap::new();
        let usable = grants
            .usable_families(&user_id, &any_use)
            .map(|family| (&family.client_id, &family.scope, &family.created_at))
            .chain(
                grants
                    .usable_access_tokens(&user_id, &any_use)
                    .map(|token| (&token.client_id, &token.scope, &token.created_at)),
            );
        for (client_id, scope, created_at) in usable {
            let Ok(created_at) = parse_stored_date(created_at) else {
                continue;
            };
            let entry = connected
                .entry(client_id.clone())
                .or_insert_with(|| ConnectedClient {
                    client_id: client_id.clone(),
                    client_name: None,
                    scope: scope.clone(),
                    created_at,
                    last_used_at: None,
                    providers: None,
                });
            if created_at < entry.created_at {
                entry.created_at = created_at;
                entry.scope = scope.clone();
            }
        }

        // Every token a connected client was issued counts as a use, expired or not.
        let tokens = oauth_access_tokens::Entity::find()
            .filter(oauth_access_tokens::Column::UserId.eq(user_id.clone()))
            .filter(oauth_access_tokens::Column::Issuer.eq(issuer))
            .filter(oauth_access_tokens::Column::AuthVersion.eq(auth_version))
            .filter(oauth_access_tokens::Column::RevokedAt.is_null())
            .all(&self.db)
            .await?;
        for token in tokens {
            let Some(entry) = connected.get_mut(&token.client_id) else {
                continue;
            };
            if let Ok(used) = parse_stored_date(&token.created_at)
                && entry.last_used_at.is_none_or(|last| used > last)
            {
                entry.last_used_at = Some(used);
            }
        }

        if connected.is_empty() {
            return Ok(Vec::new());
        }
        let names = oauth_clients::Entity::find()
            .filter(oauth_clients::Column::Id.is_in(connected.keys().cloned()))
            .filter(oauth_clients::Column::Issuer.eq(issuer))
            .all(&self.db)
            .await?;
        for client in names {
            if let Some(entry) = connected.get_mut(&client.id) {
                entry.client_name = client.client_name;
            }
        }
        let live = self
            .live_machine_ids(&self.db, Some(principal.user_id()))
            .await?;
        for machine in self.machine_grants(&user_id, issuer).await? {
            let Some(entry) = connected.get_mut(&machine.client_id) else {
                continue;
            };
            if !live.contains(&machine.id) {
                continue;
            }
            entry.providers = Some(machine.providers);
            if let Some(used) = machine
                .last_used_at
                .as_deref()
                .and_then(|v| parse_stored_date(v).ok())
                && entry.last_used_at.is_none_or(|last| used > last)
            {
                entry.last_used_at = Some(used);
            }
        }
        Ok(connected.into_values().collect())
    }

    pub async fn revoke_client_grants(
        &self,
        principal: &Principal,
        issuer: &str,
        client_id: &str,
    ) -> Result<(), DomainError> {
        validate_issuer(issuer).map_err(|_| DomainError::InvalidInput("invalid issuer"))?;
        validate_client_id(client_id).map_err(|_| DomainError::NotFound)?;
        let user_id = principal.user_id().to_string();
        let now = self.clock.now().to_rfc3339();
        let tx = self.begin_immediate().await?;
        let families = oauth_refresh_token_families::Entity::find()
            .filter(oauth_refresh_token_families::Column::UserId.eq(user_id.clone()))
            .filter(oauth_refresh_token_families::Column::ClientId.eq(client_id))
            .filter(oauth_refresh_token_families::Column::Issuer.eq(issuer))
            .filter(oauth_refresh_token_families::Column::RevokedAt.is_null())
            .all(&tx)
            .await?;
        let family_ids: Vec<String> = families.iter().map(|family| family.id.clone()).collect();
        let mut affected = 0;
        if !family_ids.is_empty() {
            affected += oauth_refresh_token_families::Entity::update_many()
                .col_expr(
                    oauth_refresh_token_families::Column::RevokedAt,
                    Expr::value(now.clone()),
                )
                .col_expr(
                    oauth_refresh_token_families::Column::RevocationReason,
                    Expr::value("user_revoked"),
                )
                .filter(oauth_refresh_token_families::Column::Id.is_in(family_ids.clone()))
                .exec(&tx)
                .await?
                .rows_affected;
            oauth_refresh_tokens::Entity::update_many()
                .col_expr(
                    oauth_refresh_tokens::Column::RevokedAt,
                    Expr::value(now.clone()),
                )
                .filter(oauth_refresh_tokens::Column::FamilyId.is_in(family_ids))
                .filter(oauth_refresh_tokens::Column::RevokedAt.is_null())
                .exec(&tx)
                .await?;
        }
        affected += oauth_access_tokens::Entity::update_many()
            .col_expr(oauth_access_tokens::Column::RevokedAt, Expr::value(now))
            .filter(oauth_access_tokens::Column::UserId.eq(user_id.clone()))
            .filter(oauth_access_tokens::Column::ClientId.eq(client_id))
            .filter(oauth_access_tokens::Column::Issuer.eq(issuer))
            .filter(oauth_access_tokens::Column::RevokedAt.is_null())
            .exec(&tx)
            .await?
            .rows_affected;
        if affected == 0 {
            tx.rollback().await?;
            return Err(DomainError::NotFound);
        }
        // The row stays for the requests recorded against it, but holds no provider.
        tx.execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE gateway_clients SET allowed_providers = '[]'
            WHERE user_id = ? AND kind = 'oauth' AND oauth_client_id = ? AND oauth_issuer = ?",
            [
                user_id.into(),
                client_id.to_owned().into(),
                issuer.to_owned().into(),
            ],
        ))
        .await?;
        tx.commit().await?;
        Ok(())
    }
}
