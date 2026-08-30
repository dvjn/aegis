use chrono::{DateTime, Utc};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, Set,
    sea_query::Expr,
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
        let (token_id, value) = self.insert_access_token(tx, grant, family_id).await?;
        Ok((
            token_id,
            IssuedAccessToken {
                value,
                refresh_token,
                scope: grant.scope.to_owned(),
            },
        ))
    }

    pub(super) async fn insert_access_token(
        &self,
        tx: &DatabaseTransaction,
        grant: GrantContext<'_>,
        family_id: Option<Uuid>,
    ) -> Result<(Uuid, String), DomainError> {
        let (token_id, selector, secret, value) = new_opaque_value(TOKEN_PREFIX);
        let secret = Zeroizing::new(secret);
        let digest = hmac_digest(
            &self.config.hmac_key,
            TOKEN_DOMAIN,
            &selector,
            secret.as_slice(),
        );
        let expires_at = add_duration(grant.now, OAuthConfig::ACCESS_TOKEN_LIFETIME)?;
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
        Ok((token_id, value))
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
        let mut connected: BTreeMap<String, ConnectedClient> = BTreeMap::new();
        let mut record =
            |client_id: String, scope: String, created_at: DateTime<Utc>, used: bool| {
                let entry = connected
                    .entry(client_id.clone())
                    .or_insert(ConnectedClient {
                        client_id,
                        client_name: None,
                        scope: scope.clone(),
                        created_at,
                        last_used_at: None,
                    });
                if created_at < entry.created_at {
                    entry.created_at = created_at;
                    entry.scope = scope;
                }
                if used && entry.last_used_at.is_none_or(|last| created_at > last) {
                    entry.last_used_at = Some(created_at);
                }
            };

        let families = oauth_refresh_token_families::Entity::find()
            .filter(oauth_refresh_token_families::Column::UserId.eq(user_id.clone()))
            .filter(oauth_refresh_token_families::Column::Issuer.eq(issuer))
            .filter(oauth_refresh_token_families::Column::AuthVersion.eq(auth_version))
            .filter(oauth_refresh_token_families::Column::RevokedAt.is_null())
            .all(&self.db)
            .await?;
        for family in families {
            let (Ok(created_at), Ok(expires_at)) = (
                parse_stored_date(&family.created_at),
                parse_stored_date(&family.absolute_expires_at),
            ) else {
                continue;
            };
            if now >= expires_at {
                continue;
            }
            record(family.client_id, family.scope, created_at, false);
        }

        let tokens = oauth_access_tokens::Entity::find()
            .filter(oauth_access_tokens::Column::UserId.eq(user_id))
            .filter(oauth_access_tokens::Column::Issuer.eq(issuer))
            .filter(oauth_access_tokens::Column::AuthVersion.eq(auth_version))
            .filter(oauth_access_tokens::Column::RevokedAt.is_null())
            .all(&self.db)
            .await?;
        for token in tokens {
            let (Ok(created_at), Ok(expires_at)) = (
                parse_stored_date(&token.created_at),
                parse_stored_date(&token.expires_at),
            ) else {
                continue;
            };
            if token.family_id.is_none() && now >= expires_at {
                continue;
            }
            record(token.client_id, token.scope, created_at, true);
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
            .filter(oauth_access_tokens::Column::UserId.eq(user_id))
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
        tx.commit().await?;
        Ok(())
    }
}
