use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, Set,
    sea_query::Expr,
};
use subtle::ConstantTimeEq;
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use super::{
    GrantContext, IssuedAccessToken, OAuthConfig, OAuthService, add_duration, keep_earliest,
    new_opaque_value, parse_opaque_value, parse_stored_date, pkce_challenge, scope_is_subset,
    validate_client_id, validate_code_verifier, validate_issuer, validate_redirect_uri,
    validate_resource, validate_scope,
};
use crate::domain::{
    auth::Identity,
    entity::{oauth_access_tokens, oauth_authorization_codes, users},
    error::DomainError,
    secrets::hmac_digest,
};

const CODE_PREFIX: &str = "ag_ac1";
const CODE_DOMAIN: &[u8] = b"aegis/oauth-authorization-code/v1\0";

pub struct AuthorizationCodeRequest<'a> {
    pub identity: &'a Identity,
    pub client_id: &'a str,
    pub issuer: &'a str,
    pub redirect_uri: &'a str,
    pub resource: &'a str,
    pub scope: &'a str,
    pub code_challenge: &'a str,
}

pub struct AuthorizationCodeRedemption<'a> {
    pub code: &'a str,
    pub client_id: &'a str,
    pub issuer: &'a str,
    pub redirect_uri: &'a str,
    pub resource: &'a str,
    pub code_verifier: &'a str,
}

pub struct IssuedAuthorizationCode {
    value: String,
}
impl IssuedAuthorizationCode {
    pub fn expose(&self) -> &str {
        &self.value
    }
}
impl Drop for IssuedAuthorizationCode {
    fn drop(&mut self) {
        self.value.zeroize();
    }
}

impl OAuthService {
    pub async fn issue_authorization_code(
        &self,
        request: AuthorizationCodeRequest<'_>,
    ) -> Result<IssuedAuthorizationCode, DomainError> {
        let scope =
            super::normalize_scope(&super::consent_scope(request.scope, &request.identity.role));
        validate_bound_values(
            request.issuer,
            request.redirect_uri,
            request.resource,
            &scope,
        )?;
        validate_client_id(request.client_id)?;
        validate_code_challenge(request.code_challenge)?;
        let (client, registered_redirect_uri) = self
            .find_authorization_client(request.client_id, request.issuer, request.redirect_uri)
            .await?;
        if !scope_is_subset(&scope, client.scope()) {
            return Err(DomainError::InvalidInput("invalid scope"));
        }
        let verified = users::Entity::find_by_id(request.identity.user_id.to_string())
            .filter(users::Column::Status.eq("active"))
            .filter(users::Column::AuthVersion.eq(request.identity.auth_version))
            .one(&self.db)
            .await?
            .is_some();
        if !verified {
            return Err(DomainError::InvalidCredentials);
        }

        let (id, selector, secret, value) = new_opaque_value(CODE_PREFIX);
        let secret = Zeroizing::new(secret);
        let digest = hmac_digest(
            &self.config.hmac_key,
            CODE_DOMAIN,
            &selector,
            secret.as_slice(),
        );
        let now = self.clock.now();
        let expires_at = add_duration(now, OAuthConfig::CODE_LIFETIME)?;
        oauth_authorization_codes::ActiveModel {
            id: Set(id.to_string()),
            secret_digest: Set(digest.to_vec()),
            key_id: Set(self.config.key_id.clone()),
            user_id: Set(request.identity.user_id.to_string()),
            client_id: Set(request.client_id.to_owned()),
            issuer: Set(request.issuer.to_owned()),
            redirect_uri: Set(request.redirect_uri.to_owned()),
            registered_redirect_uri: Set(registered_redirect_uri),
            resource: Set(request.resource.to_owned()),
            scope: Set(scope),
            auth_version: Set(request.identity.auth_version),
            code_challenge: Set(request.code_challenge.to_owned()),
            code_challenge_method: Set("S256".to_owned()),
            created_at: Set(now.to_rfc3339()),
            expires_at: Set(expires_at.to_rfc3339()),
            consumed_at: Set(None),
            issued_access_token_id: Set(None),
        }
        .insert(&self.db)
        .await?;
        Ok(IssuedAuthorizationCode { value })
    }

    pub async fn redeem_authorization_code(
        &self,
        request: AuthorizationCodeRedemption<'_>,
    ) -> Result<IssuedAccessToken, DomainError> {
        validate_issuer(request.issuer).map_err(|_| DomainError::InvalidCredentials)?;
        validate_redirect_uri(request.redirect_uri).map_err(|_| DomainError::InvalidCredentials)?;
        validate_resource(request.resource).map_err(|_| DomainError::InvalidCredentials)?;
        validate_client_id(request.client_id).map_err(|_| DomainError::InvalidCredentials)?;
        validate_code_verifier(request.code_verifier)
            .map_err(|_| DomainError::InvalidCredentials)?;
        let (id, selector, secret) = parse_opaque_value(request.code, CODE_PREFIX)?;
        let secret = Zeroizing::new(secret);
        let tx = self.begin_immediate().await?;
        let row = oauth_authorization_codes::Entity::find_by_id(id.to_string())
            .find_also_related(users::Entity)
            .one(&tx)
            .await?;
        let Some((code, Some(user))) = row else {
            return Err(DomainError::InvalidCredentials);
        };
        let stored_digest = code.secret_digest.clone();
        let expected_digest = hmac_digest(
            &self.config.hmac_key,
            CODE_DOMAIN,
            &selector,
            secret.as_slice(),
        );
        let expected_challenge = pkce_challenge(request.code_verifier);
        let expires_at = parse_stored_date(&code.expires_at)?;
        let auth_version = code.auth_version;
        let user_auth_version = user.auth_version;
        let now = self.clock.now();
        if stored_digest.len() != 32
            || expected_digest.ct_eq(stored_digest.as_slice()).unwrap_u8() != 1
            || code
                .code_challenge
                .as_bytes()
                .ct_eq(expected_challenge.as_bytes())
                .unwrap_u8()
                != 1
            || code.key_id != self.config.key_id
            || code.client_id != request.client_id
            || code.issuer != request.issuer
            || code.redirect_uri != request.redirect_uri
            || code.resource != request.resource
            || code.code_challenge_method != "S256"
        {
            return Err(DomainError::InvalidCredentials);
        }
        if code.consumed_at.is_some() {
            Self::revoke_code_grant(&tx, code.issued_access_token_id.clone(), now).await?;
            tx.commit().await?;
            return Err(DomainError::InvalidCredentials);
        }
        if user.status != "active" || auth_version != user_auth_version || now >= expires_at {
            return Err(DomainError::InvalidCredentials);
        }

        let consumed = oauth_authorization_codes::Entity::update_many()
            .col_expr(
                oauth_authorization_codes::Column::ConsumedAt,
                Expr::value(now.to_rfc3339()),
            )
            .filter(oauth_authorization_codes::Column::Id.eq(id.to_string()))
            .filter(oauth_authorization_codes::Column::ConsumedAt.is_null())
            .filter(oauth_authorization_codes::Column::ExpiresAt.gt(now.to_rfc3339()))
            .exec(&tx)
            .await?;
        if consumed.rows_affected != 1 {
            let issued_token_id = oauth_authorization_codes::Entity::find_by_id(id.to_string())
                .one(&tx)
                .await?
                .and_then(|code| code.issued_access_token_id);
            Self::revoke_code_grant(&tx, issued_token_id, now).await?;
            tx.commit().await?;
            return Err(DomainError::InvalidCredentials);
        }

        let (token_id, issued) = self
            .issue_initial_grant(
                &tx,
                GrantContext {
                    user_id: &code.user_id,
                    client_id: request.client_id,
                    issuer: request.issuer,
                    redirect_uri: Some(&code.registered_redirect_uri),
                    resource: request.resource,
                    scope: &code.scope,
                    auth_version,
                    now,
                },
            )
            .await?;
        oauth_authorization_codes::Entity::update_many()
            .col_expr(
                oauth_authorization_codes::Column::IssuedAccessTokenId,
                Expr::value(token_id.to_string()),
            )
            .filter(oauth_authorization_codes::Column::Id.eq(id.to_string()))
            .exec(&tx)
            .await?;
        tx.commit().await?;
        Ok(issued)
    }

    async fn revoke_code_grant(
        tx: &DatabaseTransaction,
        issued_token_id: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        let Some(token_id) = issued_token_id else {
            return Ok(());
        };
        let family_id = oauth_access_tokens::Entity::find_by_id(token_id.clone())
            .one(tx)
            .await?
            .and_then(|token| token.family_id);
        oauth_access_tokens::Entity::update_many()
            .col_expr(
                oauth_access_tokens::Column::RevokedAt,
                keep_earliest(oauth_access_tokens::Column::RevokedAt, now),
            )
            .filter(oauth_access_tokens::Column::Id.eq(token_id))
            .exec(tx)
            .await?;
        if let Some(family_id) = family_id {
            let family_id =
                Uuid::parse_str(&family_id).map_err(|_| DomainError::InvalidCredentials)?;
            Self::revoke_refresh_family(tx, family_id, now).await?;
        }
        Ok(())
    }
}

fn validate_bound_values(
    issuer: &str,
    redirect_uri: &str,
    resource: &str,
    scope: &str,
) -> Result<(), DomainError> {
    validate_issuer(issuer)?;
    validate_redirect_uri(redirect_uri)?;
    validate_resource(resource)?;
    validate_scope(scope)
}

fn validate_code_challenge(challenge: &str) -> Result<(), DomainError> {
    if challenge.len() != 43 || challenge.contains('=') {
        return Err(DomainError::InvalidInput("invalid PKCE challenge"));
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(challenge)
        .map_err(|_| DomainError::InvalidInput("invalid PKCE challenge"))?;
    if decoded.len() != 32 {
        return Err(DomainError::InvalidInput("invalid PKCE challenge"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{code, code_with_scope, redemption, refresh_request, setup};
    use super::*;
    use sea_orm::{ConnectionTrait, DbBackend, QueryResult, Statement};

    #[tokio::test]
    async fn pkce_redemption_is_one_time_and_secrets_are_only_digests() {
        let (db, oauth, _, _, identity, client) = setup().await;
        let verifier = "correct-verifier-with-at-least-forty-three-characters-123";
        let issued_code = code(&oauth, &identity, &client, verifier).await;
        assert!(issued_code.expose().starts_with("ag_ac1."));
        assert!(
            oauth
                .redeem_authorization_code(redemption(
                    issued_code.expose(),
                    &client,
                    "wrong-verifier-with-at-least-forty-three-characters"
                ))
                .await
                .is_err()
        );
        let token = oauth
            .redeem_authorization_code(redemption(issued_code.expose(), &client, verifier))
            .await
            .unwrap();
        assert!(token.expose().starts_with("ag_at1."));
        assert_eq!(token.scope(), "usage:read");
        assert!(
            oauth
                .redeem_authorization_code(redemption(issued_code.expose(), &client, verifier))
                .await
                .is_err()
        );

        for table in ["oauth_authorization_codes", "oauth_access_tokens"] {
            let row = db
                .query_one_raw(Statement::from_string(
                    DbBackend::Sqlite,
                    format!("SELECT secret_digest FROM {table}"),
                ))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                row.try_get::<Vec<u8>>("", "secret_digest").unwrap().len(),
                32
            );
        }
        let dump: String = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT hex(secret_digest) AS digest FROM oauth_access_tokens",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "digest")
            .unwrap();
        assert!(!token.expose().contains(&dump));
    }

    #[tokio::test]
    async fn authorization_accepts_only_loopback_port_variation() {
        let (_, oauth, _, _, identity, client) = setup().await;
        let verifier = "correct-verifier-with-at-least-forty-three-characters-123";
        let challenge = pkce_challenge(verifier);
        let actual_redirect = "http://127.0.0.1:54321/callback";
        let issued = oauth
            .issue_authorization_code(AuthorizationCodeRequest {
                identity: &identity,
                client_id: client.client_id(),
                issuer: &client.issuer,
                redirect_uri: actual_redirect,
                resource: "https://aegis.example/v1",
                scope: "usage:read",
                code_challenge: &challenge,
            })
            .await
            .unwrap();
        assert!(
            oauth
                .redeem_authorization_code(AuthorizationCodeRedemption {
                    code: issued.expose(),
                    client_id: client.client_id(),
                    issuer: &client.issuer,
                    redirect_uri: actual_redirect,
                    resource: "https://aegis.example/v1",
                    code_verifier: verifier,
                })
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn concurrent_authorization_code_redemption_succeeds_once() {
        let (_, oauth, _, _, identity, client) = setup().await;
        let verifier = "correct-verifier-with-at-least-forty-three-characters-123";
        let issued = code(&oauth, &identity, &client, verifier).await;
        let (left, right) = tokio::join!(
            oauth.redeem_authorization_code(redemption(issued.expose(), &client, verifier)),
            oauth.redeem_authorization_code(redemption(issued.expose(), &client, verifier)),
        );
        assert_ne!(left.is_ok(), right.is_ok());
    }

    struct TempDatabase {
        directory: std::path::PathBuf,
        path: std::path::PathBuf,
    }

    impl TempDatabase {
        fn new() -> Self {
            let directory =
                std::env::temp_dir().join(format!("aegis-oauth-race-{}", Uuid::now_v7()));
            std::fs::create_dir(&directory).unwrap();
            Self {
                path: directory.join("aegis.db"),
                directory,
            }
        }
    }

    impl Drop for TempDatabase {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    async fn redeem_owned(
        oauth: OAuthService,
        code: String,
        client: crate::domain::oauth::RegisteredClient,
        verifier: &'static str,
    ) -> Result<IssuedAccessToken, DomainError> {
        oauth
            .redeem_authorization_code(AuthorizationCodeRedemption {
                code: &code,
                client_id: client.client_id(),
                issuer: &client.issuer,
                redirect_uri: &client.redirect_uris()[0],
                resource: "https://aegis.example/v1",
                code_verifier: verifier,
            })
            .await
    }

    #[tokio::test]
    async fn concurrent_redemption_replay_revokes_the_winning_grant() {
        let temp = TempDatabase::new();
        let database = crate::db::connect(&format!("sqlite://{}?mode=rwc", temp.path.display()))
            .await
            .unwrap();
        let (db, oauth, _, _, identity, client) = super::super::tests::setup_with(database).await;
        let verifier = "correct-verifier-with-at-least-forty-three-characters-123";
        let code = code_with_scope(
            &oauth,
            &identity,
            &client,
            verifier,
            "usage:read offline_access",
        )
        .await;
        let (left, right) = tokio::join!(
            tokio::spawn(redeem_owned(
                oauth.clone(),
                code.expose().to_owned(),
                client.clone(),
                verifier,
            )),
            tokio::spawn(redeem_owned(
                oauth.clone(),
                code.expose().to_owned(),
                client.clone(),
                verifier,
            )),
        );
        let (left, right) = (left.unwrap(), right.unwrap());
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
        let revoked: Option<String> = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT revoked_at FROM oauth_access_tokens",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "revoked_at")
            .unwrap();
        assert!(revoked.is_some());
    }

    #[tokio::test]
    async fn redemption_rejects_wrong_resource_client_redirect_and_expiry() {
        let (_, oauth, _, clock, identity, client) = setup().await;
        let verifier = "correct-verifier-with-at-least-forty-three-characters-123";
        for kind in ["resource", "client", "redirect"] {
            let issued = code(&oauth, &identity, &client, verifier).await;
            let mut request = redemption(issued.expose(), &client, verifier);
            match kind {
                "resource" => request.resource = "https://aegis.example/mcp",
                "client" => request.client_id = "00000000-0000-0000-0000-000000000000",
                "redirect" => request.redirect_uri = "http://127.0.0.1:49152/other",
                _ => unreachable!(),
            }
            assert!(
                oauth.redeem_authorization_code(request).await.is_err(),
                "{kind}"
            );
        }
        let expired = code(&oauth, &identity, &client, verifier).await;
        clock.advance(chrono::Duration::minutes(5));
        assert!(
            oauth
                .redeem_authorization_code(redemption(expired.expose(), &client, verifier))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn authorization_code_replay_revokes_the_issued_token_family() {
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
        let issued = oauth
            .redeem_authorization_code(redemption(code.expose(), &client, verifier))
            .await
            .unwrap();
        let refresh = issued.refresh_token().unwrap().to_owned();
        assert!(
            oauth
                .redeem_authorization_code(redemption(code.expose(), &client, verifier))
                .await
                .is_err()
        );
        assert!(
            oauth
                .refresh_access_token(refresh_request(&refresh, &client))
                .await
                .is_err()
        );
        assert!(
            oauth
                .authenticate_access_token(
                    issued.expose(),
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
    }

    #[tokio::test]
    async fn authorization_code_replay_revokes_a_family_less_access_token() {
        let (db, oauth, _, _, identity, client) = setup().await;
        let verifier = "correct-verifier-with-at-least-forty-three-characters-123";
        let issued_code = code(&oauth, &identity, &client, verifier).await;
        let token = oauth
            .redeem_authorization_code(redemption(issued_code.expose(), &client, verifier))
            .await
            .unwrap();
        assert!(
            oauth
                .redeem_authorization_code(redemption(issued_code.expose(), &client, verifier))
                .await
                .is_err()
        );
        assert!(
            oauth
                .authenticate_access_token(
                    token.expose(),
                    "https://aegis.example",
                    "https://aegis.example/v1",
                )
                .await
                .is_err()
        );
        let revoked: Option<String> = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT revoked_at FROM oauth_access_tokens",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "revoked_at")
            .unwrap();
        assert!(revoked.is_some());
    }
}
