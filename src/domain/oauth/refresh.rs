use chrono::{DateTime, Utc};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, Set,
    TransactionTrait,
    sea_query::{Expr, Func, SimpleExpr},
};
use subtle::ConstantTimeEq;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{
    GrantContext, IssuedAccessToken, OAuthConfig, OAuthService, add_duration, keep_earliest,
    new_opaque_value, parse_opaque_value, parse_stored_date, validate_client_id, validate_issuer,
    validate_resource,
};
use crate::domain::{
    entity::{oauth_access_tokens, oauth_refresh_token_families, oauth_refresh_tokens, users},
    error::DomainError,
    secrets::hmac_digest,
};

const REFRESH_PREFIX: &str = "ag_rt1";
const REFRESH_DOMAIN: &[u8] = b"aegis/oauth-refresh-token/v1\0";

pub struct RefreshTokenRequest<'a> {
    pub refresh_token: &'a str,
    pub client_id: &'a str,
    pub issuer: &'a str,
    pub resource: &'a str,
}

impl OAuthService {
    pub(super) async fn insert_refresh_token(
        &self,
        tx: &DatabaseTransaction,
        family_id: Uuid,
        generation: i64,
        now: DateTime<Utc>,
        absolute_expires_at: DateTime<Utc>,
    ) -> Result<String, DomainError> {
        let (id, selector, secret, value) = new_opaque_value(REFRESH_PREFIX);
        let secret = Zeroizing::new(secret);
        let digest = hmac_digest(
            &self.config.hmac_key,
            REFRESH_DOMAIN,
            &selector,
            secret.as_slice(),
        );
        let idle_expires_at = std::cmp::min(
            add_duration(now, OAuthConfig::REFRESH_IDLE_LIFETIME)?,
            absolute_expires_at,
        );
        oauth_refresh_tokens::ActiveModel {
            id: Set(id.to_string()),
            family_id: Set(family_id.to_string()),
            secret_digest: Set(digest.to_vec()),
            key_id: Set(self.config.key_id.clone()),
            generation: Set(generation),
            created_at: Set(now.to_rfc3339()),
            idle_expires_at: Set(idle_expires_at.to_rfc3339()),
            rotated_at: Set(None),
            revoked_at: Set(None),
        }
        .insert(tx)
        .await?;
        Ok(value)
    }

    pub(super) async fn revoke_refresh_family(
        tx: &DatabaseTransaction,
        family_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        oauth_refresh_token_families::Entity::update_many()
            .col_expr(
                oauth_refresh_token_families::Column::RevokedAt,
                keep_earliest(oauth_refresh_token_families::Column::RevokedAt, now),
            )
            .col_expr(
                oauth_refresh_token_families::Column::RevocationReason,
                SimpleExpr::from(Func::coalesce([
                    Expr::col(oauth_refresh_token_families::Column::RevocationReason),
                    Expr::val("reuse"),
                ])),
            )
            .filter(oauth_refresh_token_families::Column::Id.eq(family_id.to_string()))
            .exec(tx)
            .await?;
        oauth_refresh_tokens::Entity::update_many()
            .col_expr(
                oauth_refresh_tokens::Column::RevokedAt,
                keep_earliest(oauth_refresh_tokens::Column::RevokedAt, now),
            )
            .filter(oauth_refresh_tokens::Column::FamilyId.eq(family_id.to_string()))
            .exec(tx)
            .await?;
        oauth_access_tokens::Entity::update_many()
            .col_expr(
                oauth_access_tokens::Column::RevokedAt,
                keep_earliest(oauth_access_tokens::Column::RevokedAt, now),
            )
            .filter(oauth_access_tokens::Column::FamilyId.eq(family_id.to_string()))
            .exec(tx)
            .await?;
        Ok(())
    }

    pub async fn refresh_access_token(
        &self,
        request: RefreshTokenRequest<'_>,
    ) -> Result<IssuedAccessToken, DomainError> {
        validate_client_id(request.client_id).map_err(|_| DomainError::InvalidCredentials)?;
        validate_issuer(request.issuer).map_err(|_| DomainError::InvalidCredentials)?;
        validate_resource(request.resource).map_err(|_| DomainError::InvalidCredentials)?;
        let client = self.find_client(request.client_id, request.issuer).await?;
        if !client.allows_grant("refresh_token") {
            return Err(DomainError::InvalidCredentials);
        }
        let (id, selector, secret) = parse_opaque_value(request.refresh_token, REFRESH_PREFIX)?;
        let secret = Zeroizing::new(secret);
        let row = oauth_refresh_tokens::Entity::find_by_id(id.to_string())
            .find_also_related(oauth_refresh_token_families::Entity)
            .one(&self.db)
            .await?;
        let Some((token, Some(family))) = row else {
            return Err(DomainError::InvalidCredentials);
        };
        let user = users::Entity::find_by_id(family.user_id.clone())
            .one(&self.db)
            .await?
            .ok_or(DomainError::InvalidCredentials)?;
        let stored_digest = token.secret_digest.clone();
        let expected_digest = hmac_digest(
            &self.config.hmac_key,
            REFRESH_DOMAIN,
            &selector,
            secret.as_slice(),
        );
        let family_id =
            Uuid::parse_str(&token.family_id).map_err(|_| DomainError::InvalidCredentials)?;
        if stored_digest.len() != 32
            || expected_digest.ct_eq(stored_digest.as_slice()).unwrap_u8() != 1
            || token.key_id != self.config.key_id
            || family.client_id != request.client_id
            || family.issuer != request.issuer
            || family.resource != request.resource
        {
            return Err(DomainError::InvalidCredentials);
        }
        let now = self.clock.now();
        if token.rotated_at.is_some() {
            let tx = self.db.begin().await?;
            Self::revoke_refresh_family(&tx, family_id, now).await?;
            tx.commit().await?;
            return Err(DomainError::InvalidCredentials);
        }
        let auth_version = family.auth_version;
        if token.revoked_at.is_some()
            || family.revoked_at.is_some()
            || user.status != "active"
            || auth_version != user.auth_version
            || now >= parse_stored_date(&token.idle_expires_at)?
            || now >= parse_stored_date(&family.absolute_expires_at)?
        {
            return Err(DomainError::InvalidCredentials);
        }

        let tx = self.db.begin().await?;
        let rotated = oauth_refresh_tokens::Entity::update_many()
            .col_expr(
                oauth_refresh_tokens::Column::RotatedAt,
                Expr::value(now.to_rfc3339()),
            )
            .filter(oauth_refresh_tokens::Column::Id.eq(id.to_string()))
            .filter(oauth_refresh_tokens::Column::RotatedAt.is_null())
            .filter(oauth_refresh_tokens::Column::RevokedAt.is_null())
            .exec(&tx)
            .await?;
        if rotated.rows_affected != 1 {
            Self::revoke_refresh_family(&tx, family_id, now).await?;
            tx.commit().await?;
            return Err(DomainError::InvalidCredentials);
        }
        let (token, family) = oauth_refresh_tokens::Entity::find_by_id(id.to_string())
            .find_also_related(oauth_refresh_token_families::Entity)
            .one(&tx)
            .await?
            .and_then(|(token, family)| family.map(|family| (token, family)))
            .ok_or(DomainError::InvalidCredentials)?;
        let user = users::Entity::find_by_id(family.user_id.clone())
            .one(&tx)
            .await?
            .ok_or(DomainError::InvalidCredentials)?;
        let auth_version = family.auth_version;
        if family.revoked_at.is_some()
            || user.status != "active"
            || auth_version != user.auth_version
            || now >= parse_stored_date(&token.idle_expires_at)?
            || now >= parse_stored_date(&family.absolute_expires_at)?
        {
            return Err(DomainError::InvalidCredentials);
        }
        let generation = token.generation;
        let absolute_expires_at = parse_stored_date(&family.absolute_expires_at)?;
        let refresh_token = self
            .insert_refresh_token(&tx, family_id, generation + 1, now, absolute_expires_at)
            .await?;
        let scope = family.scope.clone();
        let (_, value) = self
            .insert_access_token(
                &tx,
                GrantContext {
                    user_id: &family.user_id,
                    client_id: request.client_id,
                    issuer: request.issuer,
                    redirect_uri: None,
                    resource: request.resource,
                    scope: &scope,
                    auth_version,
                    now,
                },
                Some(family_id),
            )
            .await?;
        tx.commit().await?;
        Ok(IssuedAccessToken {
            value,
            refresh_token: Some(refresh_token),
            scope,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{code_with_scope, redemption, refresh_request, setup};
    use super::*;
    use sea_orm::{ConnectionTrait, DbBackend, QueryResult, Statement};

    #[tokio::test]
    async fn refresh_tokens_rotate_and_reuse_revokes_the_entire_family() {
        let (db, oauth, _, _, identity, client) = setup().await;
        let verifier = "correct-verifier-with-at-least-forty-three-characters-123";
        let code = code_with_scope(
            &oauth,
            &identity,
            &client,
            verifier,
            "usage:read offline_access",
        )
        .await;
        let first = oauth
            .redeem_authorization_code(redemption(code.expose(), &client, verifier))
            .await
            .unwrap();
        let old_refresh = first.refresh_token().unwrap().to_owned();
        let rotated = oauth
            .refresh_access_token(refresh_request(&old_refresh, &client))
            .await
            .unwrap();
        let new_refresh = rotated.refresh_token().unwrap().to_owned();
        assert_ne!(old_refresh, new_refresh);
        assert!(
            oauth
                .refresh_access_token(refresh_request(&old_refresh, &client))
                .await
                .is_err()
        );
        assert!(
            oauth
                .refresh_access_token(refresh_request(&new_refresh, &client))
                .await
                .is_err()
        );
        assert!(
            oauth
                .authenticate_access_token(
                    rotated.expose(),
                    "https://aegis.example",
                    "https://aegis.example/v1",
                )
                .await
                .is_err()
        );
        let family: QueryResult = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT revoked_at,revocation_reason FROM oauth_refresh_token_families",
            ))
            .await
            .unwrap()
            .unwrap();
        assert!(
            family
                .try_get::<Option<String>>("", "revoked_at")
                .unwrap()
                .is_some()
        );
        assert_eq!(
            family.try_get::<String>("", "revocation_reason").unwrap(),
            "reuse"
        );
        let dump = format!("{old_refresh}{new_refresh}");
        let stored: String = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT group_concat(hex(secret_digest), '') AS digests FROM oauth_refresh_tokens",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "digests")
            .unwrap();
        assert!(!dump.contains(&stored));
    }

    #[tokio::test]
    async fn concurrent_refresh_reuse_revokes_the_winning_family() {
        let (_, oauth, _, _, identity, client) = setup().await;
        let verifier = "correct-verifier-with-at-least-forty-three-characters-123";
        let code = code_with_scope(
            &oauth,
            &identity,
            &client,
            verifier,
            "usage:read offline_access",
        )
        .await;
        let issued = oauth
            .redeem_authorization_code(redemption(code.expose(), &client, verifier))
            .await
            .unwrap();
        let refresh = issued.refresh_token().unwrap().to_owned();
        let (left, right) = tokio::join!(
            oauth.refresh_access_token(refresh_request(&refresh, &client)),
            oauth.refresh_access_token(refresh_request(&refresh, &client)),
        );
        assert_ne!(left.is_ok(), right.is_ok());
        let winner = left.or(right).unwrap();
        assert!(
            oauth
                .authenticate_access_token(
                    winner.expose(),
                    "https://aegis.example",
                    "https://aegis.example/v1",
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn refresh_tokens_enforce_idle_absolute_and_auth_version() {
        let verifier = "correct-verifier-with-at-least-forty-three-characters-123";
        for expiration in ["idle", "absolute", "auth_version"] {
            let (db, oauth, _, clock, identity, client) = setup().await;
            let code = code_with_scope(
                &oauth,
                &identity,
                &client,
                verifier,
                "usage:read offline_access",
            )
            .await;
            let issued = oauth
                .redeem_authorization_code(redemption(code.expose(), &client, verifier))
                .await
                .unwrap();
            match expiration {
                "idle" => clock.advance(chrono::Duration::days(180)),
                "absolute" => {
                    db.execute_unprepared(
                        "UPDATE oauth_refresh_tokens SET idle_expires_at='2028-01-01T00:00:00Z'",
                    )
                    .await
                    .unwrap();
                    clock.advance(chrono::Duration::days(365));
                }
                "auth_version" => {
                    db.execute_unprepared("UPDATE users SET auth_version=auth_version+1")
                        .await
                        .unwrap();
                }
                _ => unreachable!(),
            }
            assert!(
                oauth
                    .refresh_access_token(RefreshTokenRequest {
                        refresh_token: issued.refresh_token().unwrap(),
                        client_id: client.client_id(),
                        issuer: &client.issuer,
                        resource: "https://aegis.example/v1",
                    })
                    .await
                    .is_err(),
                "{expiration}"
            );
        }
    }
}
