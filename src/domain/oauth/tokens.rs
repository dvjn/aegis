use sea_orm::EntityTrait;
use subtle::ConstantTimeEq;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{
    OAuthService, TOKEN_DOMAIN, TOKEN_PREFIX, parse_opaque_value, parse_stored_date,
    validate_issuer, validate_resource,
};
use crate::domain::{
    auth::{Identity, OAuthPrincipal, Principal, PrincipalTransport},
    entity::{oauth_access_tokens, oauth_refresh_token_families, users},
    error::DomainError,
    secrets::hmac_digest,
};

impl OAuthService {
    pub async fn authenticate_access_token(
        &self,
        token: &str,
        issuer: &str,
        resource: &str,
    ) -> Result<Principal, DomainError> {
        validate_issuer(issuer).map_err(|_| DomainError::InvalidCredentials)?;
        validate_resource(resource).map_err(|_| DomainError::InvalidCredentials)?;
        let (id, selector, secret) = parse_opaque_value(token, TOKEN_PREFIX)?;
        let secret = Zeroizing::new(secret);
        let row = oauth_access_tokens::Entity::find_by_id(id.to_string())
            .find_also_related(users::Entity)
            .one(&self.db)
            .await?;
        let Some((token, Some(user))) = row else {
            return Err(DomainError::InvalidCredentials);
        };
        let family_revoked_at = match &token.family_id {
            Some(family_id) => oauth_refresh_token_families::Entity::find_by_id(family_id.clone())
                .one(&self.db)
                .await?
                .and_then(|family| family.revoked_at),
            None => None,
        };
        let stored_digest = token.secret_digest.clone();
        let expected_digest = hmac_digest(
            &self.config.hmac_key,
            TOKEN_DOMAIN,
            &selector,
            secret.as_slice(),
        );
        let auth_version = token.auth_version;
        if stored_digest.len() != 32
            || expected_digest.ct_eq(stored_digest.as_slice()).unwrap_u8() != 1
            || token.key_id != self.config.key_id
            || token.issuer != issuer
            || token.resource != resource
            || token.revoked_at.is_some()
            || family_revoked_at.is_some()
            || user.status != "active"
            || auth_version != user.auth_version
            || self.clock.now() >= parse_stored_date(&token.expires_at)?
        {
            return Err(DomainError::InvalidCredentials);
        }
        let user_id =
            Uuid::parse_str(&token.user_id).map_err(|_| DomainError::InvalidCredentials)?;
        Ok(Principal {
            identity: Identity {
                user_id,
                role: user.role,
                auth_version,
            },
            transport: PrincipalTransport::OAuthAccessToken {
                token_id: id,
                context: OAuthPrincipal {
                    client_id: token.client_id,
                    issuer: issuer.to_owned(),
                    resource: resource.to_owned(),
                    scope: token.scope,
                },
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{code, redemption, setup};
    use sea_orm::ConnectionTrait;

    #[tokio::test]
    async fn token_authentication_binds_audience_expiry_and_user_auth_version() {
        let (db, oauth, _, clock, identity, client) = setup().await;
        let verifier = "correct-verifier-with-at-least-forty-three-characters-123";
        let issued = code(&oauth, &identity, &client, verifier).await;
        let token = oauth
            .redeem_authorization_code(redemption(issued.expose(), &client, verifier))
            .await
            .unwrap();
        let principal = oauth
            .authenticate_access_token(
                token.expose(),
                "https://aegis.example",
                "https://aegis.example/v1",
            )
            .await
            .unwrap();
        assert_eq!(principal.user_id(), identity.user_id);
        assert_eq!(principal.oauth().unwrap().client_id, client.client_id());
        assert_eq!(principal.oauth().unwrap().scope, "usage:read");
        assert!(
            oauth
                .authenticate_access_token(
                    token.expose(),
                    "https://aegis.example",
                    "https://aegis.example/mcp"
                )
                .await
                .is_err()
        );
        assert!(
            oauth
                .authenticate_access_token(
                    token.expose(),
                    "https://other.example",
                    "https://aegis.example/v1"
                )
                .await
                .is_err()
        );

        db.execute_unprepared("UPDATE users SET auth_version=auth_version+1")
            .await
            .unwrap();
        assert!(
            oauth
                .authenticate_access_token(
                    token.expose(),
                    "https://aegis.example",
                    "https://aegis.example/v1"
                )
                .await
                .is_err()
        );
        db.execute_unprepared("UPDATE users SET auth_version=auth_version-1")
            .await
            .unwrap();
        clock.advance(chrono::Duration::hours(1));
        assert!(
            oauth
                .authenticate_access_token(
                    token.expose(),
                    "https://aegis.example",
                    "https://aegis.example/v1"
                )
                .await
                .is_err()
        );
    }
}
