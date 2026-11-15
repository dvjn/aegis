use rand::RngExt;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, QuerySelect, Set,
    sea_query::Expr,
};
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, Zeroizing};

use super::{
    DEVICE_GRANT_TYPE, GrantContext, IssuedAccessToken, OAuthConfig, OAuthService, add_duration,
    parse_opaque_value, parse_stored_date, scope_is_subset, validate_client_id, validate_issuer,
    validate_resource, validate_scope,
};
use crate::domain::{
    auth::Identity,
    entity::{oauth_device_authorizations, users},
    error::DomainError,
    secrets::hmac_digest,
};

const DEVICE_CODE_PREFIX: &str = "ag_dc1";
const DEVICE_CODE_DOMAIN: &[u8] = b"aegis/oauth-device-code/v1\0";
const USER_CODE_DOMAIN: &[u8] = b"aegis/oauth-device-user-code/v1\0";
const USER_CODE_ALPHABET: &[u8; 32] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
const MAX_ACTIVE_DEVICE_AUTHORIZATIONS_PER_CLIENT: u64 = 100;
const MAX_ACTIVE_DEVICE_AUTHORIZATIONS_PER_ISSUER: u64 = 10_000;
const DEVICE_CLEANUP_BATCH: u64 = 100;
const DEVICE_RETENTION_HOURS: i64 = 24;
const USER_CODE_GENERATION_ATTEMPTS: usize = 5;

pub struct DeviceAuthorizationRequest<'a> {
    pub client_id: &'a str,
    pub issuer: &'a str,
    pub resource: &'a str,
    pub scope: Option<&'a str>,
}

pub struct IssuedDeviceAuthorization {
    device_code: String,
    user_code: String,
}
impl IssuedDeviceAuthorization {
    pub fn device_code(&self) -> &str {
        &self.device_code
    }
    pub fn user_code(&self) -> &str {
        &self.user_code
    }
    pub fn expires_in(&self) -> u64 {
        OAuthConfig::DEVICE_LIFETIME.as_secs()
    }
    pub fn interval(&self) -> u64 {
        OAuthConfig::DEVICE_POLL_INTERVAL.as_secs()
    }
}
impl Drop for IssuedDeviceAuthorization {
    fn drop(&mut self) {
        self.device_code.zeroize();
        self.user_code.zeroize();
    }
}

#[derive(Debug, Clone)]
pub struct DeviceAuthorization {
    pub user_code: String,
    pub client_id: String,
    pub client_name: Option<String>,
    pub resource: String,
    pub scope: String,
}

pub struct DeviceTokenRequest<'a> {
    pub device_code: &'a str,
    pub client_id: &'a str,
    pub issuer: &'a str,
    pub resource: Option<&'a str>,
}

#[derive(Debug)]
pub enum DevicePollError {
    AuthorizationPending,
    SlowDown { interval: u64 },
    AccessDenied,
    ExpiredToken,
    InvalidGrant,
    InvalidTarget,
    TemporarilyUnavailable,
    Internal(DomainError),
}

impl OAuthService {
    pub async fn issue_device_authorization(
        &self,
        request: DeviceAuthorizationRequest<'_>,
    ) -> Result<IssuedDeviceAuthorization, DomainError> {
        validate_client_id(request.client_id).map_err(|_| DomainError::InvalidCredentials)?;
        validate_issuer(request.issuer).map_err(|_| DomainError::InvalidCredentials)?;
        validate_resource(request.resource)?;
        let client = self.find_client(request.client_id, request.issuer).await?;
        if !client.allows_grant(DEVICE_GRANT_TYPE) {
            return Err(DomainError::InvalidCredentials);
        }
        let scope = request.scope.unwrap_or_else(|| client.scope());
        validate_scope(scope)?;
        if !scope_is_subset(scope, client.scope()) {
            return Err(DomainError::InvalidInput("invalid scope"));
        }
        let (id, selector, secret, device_code) = super::new_opaque_value(DEVICE_CODE_PREFIX);
        let secret = Zeroizing::new(secret);
        let device_digest = hmac_digest(
            &self.config.hmac_key,
            DEVICE_CODE_DOMAIN,
            &selector,
            secret.as_slice(),
        );
        let now = self.clock.now();
        let now_text = now.to_rfc3339();
        let expires_at = add_duration(now, OAuthConfig::DEVICE_LIFETIME)?;
        let retention_cutoff = now
            .checked_sub_signed(chrono::Duration::hours(DEVICE_RETENTION_HOURS))
            .ok_or(DomainError::InvalidInput(
                "invalid device authorization time",
            ))?
            .to_rfc3339();
        let tx = self.begin_immediate().await?;

        let expired_ids: Vec<String> = oauth_device_authorizations::Entity::find()
            .select_only()
            .column(oauth_device_authorizations::Column::Id)
            .filter(oauth_device_authorizations::Column::ExpiresAt.lt(retention_cutoff))
            .limit(DEVICE_CLEANUP_BATCH)
            .into_tuple()
            .all(&tx)
            .await?;
        if !expired_ids.is_empty() {
            oauth_device_authorizations::Entity::delete_many()
                .filter(oauth_device_authorizations::Column::Id.is_in(expired_ids))
                .exec(&tx)
                .await?;
        }
        let active_for_client = oauth_device_authorizations::Entity::find()
            .filter(oauth_device_authorizations::Column::ClientId.eq(request.client_id))
            .filter(oauth_device_authorizations::Column::Issuer.eq(request.issuer))
            .filter(oauth_device_authorizations::Column::ExpiresAt.gt(&now_text))
            .count(&tx)
            .await?;
        let active_for_issuer = oauth_device_authorizations::Entity::find()
            .filter(oauth_device_authorizations::Column::Issuer.eq(request.issuer))
            .filter(oauth_device_authorizations::Column::ExpiresAt.gt(&now_text))
            .count(&tx)
            .await?;
        if active_for_client >= MAX_ACTIVE_DEVICE_AUTHORIZATIONS_PER_CLIENT
            || active_for_issuer >= MAX_ACTIVE_DEVICE_AUTHORIZATIONS_PER_ISSUER
        {
            return Err(DomainError::TemporarilyUnavailable);
        }

        let mut user_code_and_digest = None;
        for _ in 0..USER_CODE_GENERATION_ATTEMPTS {
            let user_code = new_user_code();
            let normalized = normalize_user_code(&user_code)?;
            let user_digest = hmac_digest(
                &self.config.hmac_key,
                USER_CODE_DOMAIN,
                b"",
                normalized.as_bytes(),
            );
            let exists = oauth_device_authorizations::Entity::find()
                .filter(
                    oauth_device_authorizations::Column::UserCodeDigest.eq(user_digest.to_vec()),
                )
                .one(&tx)
                .await?
                .is_some();
            if !exists {
                user_code_and_digest = Some((user_code, user_digest));
                break;
            }
        }
        let Some((user_code, user_digest)) = user_code_and_digest else {
            return Err(DomainError::TemporarilyUnavailable);
        };
        oauth_device_authorizations::ActiveModel {
            id: Set(id.to_string()),
            device_code_digest: Set(device_digest.to_vec()),
            user_code_digest: Set(user_digest.to_vec()),
            key_id: Set(self.config.key_id.clone()),
            client_id: Set(request.client_id.to_owned()),
            issuer: Set(request.issuer.to_owned()),
            resource: Set(request.resource.to_owned()),
            scope: Set(scope.to_owned()),
            status: Set("pending".into()),
            user_id: Set(None),
            auth_version: Set(None),
            created_at: Set(now_text),
            expires_at: Set(expires_at.to_rfc3339()),
            interval_seconds: Set(OAuthConfig::DEVICE_POLL_INTERVAL.as_secs() as i64),
            last_poll_at: Set(None),
            decision_at: Set(None),
            consumed_at: Set(None),
        }
        .insert(&tx)
        .await?;
        tx.commit().await?;
        Ok(IssuedDeviceAuthorization {
            device_code,
            user_code,
        })
    }

    pub async fn find_device_authorization(
        &self,
        user_code: &str,
    ) -> Result<DeviceAuthorization, DomainError> {
        let normalized =
            normalize_user_code(user_code).map_err(|_| DomainError::InvalidCredentials)?;
        let expected = hmac_digest(
            &self.config.hmac_key,
            USER_CODE_DOMAIN,
            b"",
            normalized.as_bytes(),
        );
        let row = oauth_device_authorizations::Entity::find()
            .filter(oauth_device_authorizations::Column::UserCodeDigest.eq(expected.to_vec()))
            .one(&self.db)
            .await?
            .ok_or(DomainError::InvalidCredentials)?;
        if row.user_code_digest.len() != 32
            || expected.ct_eq(row.user_code_digest.as_slice()).unwrap_u8() != 1
            || row.key_id != self.config.key_id
            || row.status != "pending"
            || self.clock.now() >= parse_stored_date(&row.expires_at)?
        {
            return Err(DomainError::InvalidCredentials);
        }
        let client = self.find_client(&row.client_id, &row.issuer).await?;
        if !client.allows_grant(DEVICE_GRANT_TYPE) {
            return Err(DomainError::InvalidCredentials);
        }
        Ok(DeviceAuthorization {
            user_code: format_user_code(&normalized),
            client_id: row.client_id,
            client_name: client.client_name().map(str::to_owned),
            resource: row.resource,
            scope: row.scope,
        })
    }

    pub async fn decide_device_authorization(
        &self,
        user_code: &str,
        identity: &Identity,
        granted_scope: Option<&str>,
    ) -> Result<(), DomainError> {
        let approve = granted_scope.is_some();
        let normalized =
            normalize_user_code(user_code).map_err(|_| DomainError::InvalidCredentials)?;
        let expected = hmac_digest(
            &self.config.hmac_key,
            USER_CODE_DOMAIN,
            b"",
            normalized.as_bytes(),
        );
        let tx = self.begin_immediate().await?;
        let row = oauth_device_authorizations::Entity::find()
            .filter(oauth_device_authorizations::Column::UserCodeDigest.eq(expected.to_vec()))
            .one(&tx)
            .await?
            .ok_or(DomainError::InvalidCredentials)?;
        let user = users::Entity::find_by_id(identity.user_id.to_string())
            .one(&tx)
            .await?
            .ok_or(DomainError::InvalidCredentials)?;
        let now = self.clock.now();
        if row.user_code_digest.len() != 32
            || expected.ct_eq(row.user_code_digest.as_slice()).unwrap_u8() != 1
            || row.key_id != self.config.key_id
            || row.status != "pending"
            || now >= parse_stored_date(&row.expires_at)?
            || user.status != "active"
            || user.auth_version != identity.auth_version
        {
            return Err(DomainError::InvalidCredentials);
        }
        let granted_scope = match granted_scope {
            Some(requested) => {
                let granted = super::normalize_scope(&super::consent_scope(requested, &user.role));
                if !scope_is_subset(&granted, &row.scope) {
                    return Err(DomainError::InvalidInput("invalid scope"));
                }
                granted
            }
            None => row.scope.clone(),
        };
        let changed = oauth_device_authorizations::Entity::update_many()
            .col_expr(
                oauth_device_authorizations::Column::Scope,
                Expr::value(granted_scope),
            )
            .col_expr(
                oauth_device_authorizations::Column::Status,
                Expr::value(if approve { "approved" } else { "denied" }),
            )
            .col_expr(
                oauth_device_authorizations::Column::UserId,
                Expr::value(approve.then(|| identity.user_id.to_string())),
            )
            .col_expr(
                oauth_device_authorizations::Column::AuthVersion,
                Expr::value(approve.then_some(identity.auth_version)),
            )
            .col_expr(
                oauth_device_authorizations::Column::DecisionAt,
                Expr::value(now.to_rfc3339()),
            )
            .filter(oauth_device_authorizations::Column::Id.eq(row.id))
            .filter(oauth_device_authorizations::Column::Status.eq("pending"))
            .exec(&tx)
            .await?;
        if changed.rows_affected != 1 {
            return Err(DomainError::InvalidCredentials);
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn redeem_device_code(
        &self,
        request: DeviceTokenRequest<'_>,
    ) -> Result<IssuedAccessToken, DevicePollError> {
        validate_client_id(request.client_id).map_err(|_| DevicePollError::InvalidGrant)?;
        validate_issuer(request.issuer).map_err(|_| DevicePollError::InvalidGrant)?;
        let client = self
            .find_client(request.client_id, request.issuer)
            .await
            .map_err(map_poll_error)?;
        if !client.allows_grant(DEVICE_GRANT_TYPE) {
            return Err(DevicePollError::InvalidGrant);
        }
        let (id, selector, secret) = parse_opaque_value(request.device_code, DEVICE_CODE_PREFIX)
            .map_err(|_| DevicePollError::InvalidGrant)?;
        let secret = Zeroizing::new(secret);
        let expected = hmac_digest(
            &self.config.hmac_key,
            DEVICE_CODE_DOMAIN,
            &selector,
            secret.as_slice(),
        );
        let tx = self.begin_immediate().await.map_err(map_poll_error)?;
        let row = oauth_device_authorizations::Entity::find_by_id(id.to_string())
            .one(&tx)
            .await
            .map_err(|e| map_poll_error(e.into()))?
            .ok_or(DevicePollError::InvalidGrant)?;
        if row.device_code_digest.len() != 32
            || expected
                .ct_eq(row.device_code_digest.as_slice())
                .unwrap_u8()
                != 1
            || row.key_id != self.config.key_id
            || row.client_id != request.client_id
            || row.issuer != request.issuer
        {
            return Err(DevicePollError::InvalidGrant);
        }
        if request
            .resource
            .is_some_and(|resource| resource != row.resource)
        {
            return Err(DevicePollError::InvalidTarget);
        }
        let now = self.clock.now();
        if now >= parse_stored_date(&row.expires_at).map_err(map_poll_error)? {
            return Err(DevicePollError::ExpiredToken);
        }
        if row.status == "denied" {
            return Err(DevicePollError::AccessDenied);
        }
        if row.status == "consumed" {
            return Err(DevicePollError::InvalidGrant);
        }
        if let Some(last_poll) = row.last_poll_at.as_deref() {
            let next = add_duration(
                parse_stored_date(last_poll).map_err(map_poll_error)?,
                std::time::Duration::from_secs(row.interval_seconds as u64),
            )
            .map_err(map_poll_error)?;
            if now < next {
                let interval = row.interval_seconds.saturating_add(5);
                oauth_device_authorizations::Entity::update_many()
                    .col_expr(
                        oauth_device_authorizations::Column::IntervalSeconds,
                        Expr::value(interval),
                    )
                    .col_expr(
                        oauth_device_authorizations::Column::LastPollAt,
                        Expr::value(now.to_rfc3339()),
                    )
                    .filter(oauth_device_authorizations::Column::Id.eq(&row.id))
                    .exec(&tx)
                    .await
                    .map_err(|e| map_poll_error(e.into()))?;
                tx.commit().await.map_err(|e| map_poll_error(e.into()))?;
                return Err(DevicePollError::SlowDown {
                    interval: interval as u64,
                });
            }
        }
        if row.status == "pending" {
            oauth_device_authorizations::Entity::update_many()
                .col_expr(
                    oauth_device_authorizations::Column::LastPollAt,
                    Expr::value(now.to_rfc3339()),
                )
                .filter(oauth_device_authorizations::Column::Id.eq(&row.id))
                .exec(&tx)
                .await
                .map_err(|e| map_poll_error(e.into()))?;
            tx.commit().await.map_err(|e| map_poll_error(e.into()))?;
            return Err(DevicePollError::AuthorizationPending);
        }
        if row.status != "approved" {
            return Err(DevicePollError::InvalidGrant);
        }
        let user_id = row.user_id.clone().ok_or(DevicePollError::InvalidGrant)?;
        let auth_version = row.auth_version.ok_or(DevicePollError::InvalidGrant)?;
        let user = users::Entity::find_by_id(&user_id)
            .one(&tx)
            .await
            .map_err(|e| map_poll_error(e.into()))?
            .ok_or(DevicePollError::InvalidGrant)?;
        if user.status != "active" || user.auth_version != auth_version {
            return Err(DevicePollError::InvalidGrant);
        }
        let changed = oauth_device_authorizations::Entity::update_many()
            .col_expr(
                oauth_device_authorizations::Column::Status,
                Expr::value("consumed"),
            )
            .col_expr(
                oauth_device_authorizations::Column::ConsumedAt,
                Expr::value(now.to_rfc3339()),
            )
            .filter(oauth_device_authorizations::Column::Id.eq(&row.id))
            .filter(oauth_device_authorizations::Column::Status.eq("approved"))
            .exec(&tx)
            .await
            .map_err(|e| map_poll_error(e.into()))?;
        if changed.rows_affected != 1 {
            return Err(DevicePollError::InvalidGrant);
        }
        let (_, issued) = self
            .issue_initial_grant(
                &tx,
                GrantContext {
                    user_id: &user_id,
                    client_id: request.client_id,
                    issuer: request.issuer,
                    redirect_uri: None,
                    resource: &row.resource,
                    scope: &row.scope,
                    auth_version,
                    now,
                },
            )
            .await
            .map_err(map_poll_error)?;
        tx.commit().await.map_err(|e| map_poll_error(e.into()))?;
        Ok(issued)
    }
}

fn map_poll_error(error: DomainError) -> DevicePollError {
    match error {
        DomainError::TemporarilyUnavailable => DevicePollError::TemporarilyUnavailable,
        DomainError::InvalidCredentials
        | DomainError::InvalidInput(_)
        | DomainError::NotFound
        | DomainError::Forbidden
        | DomainError::Conflict => DevicePollError::InvalidGrant,
        error @ DomainError::Internal(_) => DevicePollError::Internal(error),
    }
}

fn new_user_code() -> String {
    let mut rng = rand::rng();
    let raw: String = (0..12)
        .map(|_| USER_CODE_ALPHABET[rng.random_range(0..USER_CODE_ALPHABET.len())] as char)
        .collect();
    format_user_code(&raw)
}
fn format_user_code(raw: &str) -> String {
    format!("{}-{}-{}", &raw[0..4], &raw[4..8], &raw[8..12])
}
pub fn normalize_user_code(value: &str) -> Result<String, DomainError> {
    let raw: String = value
        .chars()
        .filter(|character| *character != '-')
        .flat_map(char::to_uppercase)
        .collect();
    if raw.len() != 12 || !raw.bytes().all(|byte| USER_CODE_ALPHABET.contains(&byte)) {
        return Err(DomainError::InvalidInput("invalid user code"));
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::oauth::{ClientRegistration, tests::setup};
    use sea_orm::{ConnectionTrait, DbBackend, Statement};

    async fn device_client(oauth: &OAuthService) -> super::super::RegisteredClient {
        oauth
            .register_public_client(ClientRegistration {
                issuer: "https://aegis.example".into(),
                redirect_uris: vec![],
                client_name: Some("TV".into()),
                application_type: Some("native".into()),
                grant_types: vec![DEVICE_GRANT_TYPE.into(), "refresh_token".into()],
                response_types: vec![],
                scope: "usage:read offline_access".into(),
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn device_grant_stores_digests_polls_decides_and_consumes_once() {
        let (db, oauth, _, clock, identity, _) = setup().await;
        let client = device_client(&oauth).await;
        let issued = oauth
            .issue_device_authorization(DeviceAuthorizationRequest {
                client_id: client.client_id(),
                issuer: "https://aegis.example",
                resource: "https://aegis.example/mcp",
                scope: None,
            })
            .await
            .unwrap();
        assert!(issued.device_code().starts_with("ag_dc1."));
        assert_eq!(issued.user_code().len(), 14);
        let stored = db.query_one_raw(Statement::from_string(DbBackend::Sqlite, "SELECT length(device_code_digest) AS d,length(user_code_digest) AS u FROM oauth_device_authorizations")).await.unwrap().unwrap();
        assert_eq!(stored.try_get::<i64>("", "d").unwrap(), 32);
        assert_eq!(stored.try_get::<i64>("", "u").unwrap(), 32);
        assert!(matches!(
            oauth
                .redeem_device_code(DeviceTokenRequest {
                    device_code: issued.device_code(),
                    client_id: client.client_id(),
                    issuer: "https://aegis.example",
                    resource: Some("https://aegis.example/other"),
                })
                .await,
            Err(DevicePollError::InvalidTarget)
        ));
        let request = || DeviceTokenRequest {
            device_code: issued.device_code(),
            client_id: client.client_id(),
            issuer: "https://aegis.example",
            resource: None,
        };
        assert!(matches!(
            oauth.redeem_device_code(request()).await,
            Err(DevicePollError::AuthorizationPending)
        ));
        assert!(matches!(
            oauth.redeem_device_code(request()).await,
            Err(DevicePollError::SlowDown { interval: 10 })
        ));
        oauth
            .decide_device_authorization(
                &issued.user_code().to_ascii_lowercase(),
                &identity,
                Some("usage:read offline_access"),
            )
            .await
            .unwrap();
        clock.advance(chrono::Duration::seconds(10));
        let token = oauth.redeem_device_code(request()).await.unwrap();
        assert!(token.refresh_token().is_some());
        assert!(matches!(
            oauth.redeem_device_code(request()).await,
            Err(DevicePollError::InvalidGrant)
        ));
    }

    #[tokio::test]
    async fn device_grant_expiry_and_user_security_version_are_revalidated() {
        let (db, oauth, _, clock, identity, _) = setup().await;
        let client = device_client(&oauth).await;
        let issue = || DeviceAuthorizationRequest {
            client_id: client.client_id(),
            issuer: "https://aegis.example",
            resource: "https://aegis.example/mcp",
            scope: Some("usage:read"),
        };
        let expired = oauth.issue_device_authorization(issue()).await.unwrap();
        clock.advance(chrono::Duration::minutes(10));
        assert!(
            oauth
                .find_device_authorization(expired.user_code())
                .await
                .is_err()
        );
        assert!(matches!(
            oauth
                .redeem_device_code(DeviceTokenRequest {
                    device_code: expired.device_code(),
                    client_id: client.client_id(),
                    issuer: "https://aegis.example",
                    resource: None,
                })
                .await,
            Err(DevicePollError::ExpiredToken)
        ));

        let active = oauth.issue_device_authorization(issue()).await.unwrap();
        oauth
            .decide_device_authorization(active.user_code(), &identity, Some("usage:read"))
            .await
            .unwrap();
        db.execute_unprepared("UPDATE users SET auth_version=auth_version+1")
            .await
            .unwrap();
        assert!(matches!(
            oauth
                .redeem_device_code(DeviceTokenRequest {
                    device_code: active.device_code(),
                    client_id: client.client_id(),
                    issuer: "https://aegis.example",
                    resource: None,
                })
                .await,
            Err(DevicePollError::InvalidGrant)
        ));
    }
}
