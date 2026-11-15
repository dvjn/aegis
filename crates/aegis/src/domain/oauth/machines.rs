//! Paired machines use their access token at the gateway in place of an API key.
use chrono::TimeDelta;
use sea_orm::{
    ConnectionTrait, DatabaseTransaction, DbBackend, EntityTrait, FromQueryResult, Statement,
};

use super::{
    GATEWAY_SCOPE, GrantContext, OAuthService, gateway_resource, parse_stored_date, scope_allows,
    validate_client_id, validate_issuer,
};
use crate::domain::{
    auth::{Principal, PrincipalTransport},
    entity::users,
    error::DomainError,
};

/// A busy machine updates its last use about once a minute, not on every request.
const LAST_USED_PRECISION: TimeDelta = TimeDelta::minutes(1);

/// The gateway client a request is recorded against.
#[derive(Clone, Debug)]
pub struct MachineClient {
    pub id: String,
    pub user_id: String,
}

#[derive(FromQueryResult)]
struct GatewayClientRow {
    id: String,
    name: String,
    allowed_providers: String,
    last_used_at: Option<String>,
}

pub(super) fn provider_list(providers: &[String]) -> String {
    let mut values = providers.to_vec();
    values.sort();
    values.dedup();
    serde_json::to_string(&values).expect("a list of strings serializes")
}

fn parse_providers(value: &str) -> Result<Vec<String>, DomainError> {
    serde_json::from_str(value).map_err(|_| DomainError::InvalidInput("invalid stored providers"))
}

fn oauth_client_id(principal: &Principal) -> Result<&str, DomainError> {
    match &principal.transport {
        PrincipalTransport::OAuthAccessToken { context, .. }
            if scope_allows(&context.scope, GATEWAY_SCOPE) =>
        {
            Ok(&context.client_id)
        }
        _ => Err(DomainError::InvalidCredentials),
    }
}

impl OAuthService {
    pub(super) async fn pair_machine(
        &self,
        tx: &DatabaseTransaction,
        grant: GrantContext<'_>,
        name: Option<&str>,
        providers: &str,
    ) -> Result<(), DomainError> {
        tx.execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO gateway_clients
                (id,user_id,kind,name,oauth_client_id,oauth_issuer,allowed_providers,created_at)
            VALUES (?,?,'oauth',?,?,?,?,?)
            ON CONFLICT (user_id,oauth_client_id,oauth_issuer)
            DO UPDATE SET name=excluded.name, allowed_providers=excluded.allowed_providers",
            [
                uuid::Uuid::now_v7().to_string().into(),
                grant.user_id.to_owned().into(),
                name.unwrap_or("Unnamed application").to_owned().into(),
                grant.client_id.to_owned().into(),
                grant.issuer.to_owned().into(),
                providers.to_owned().into(),
                grant.now.to_rfc3339().into(),
            ],
        ))
        .await?;
        Ok(())
    }

    async fn gateway_client(
        &self,
        db: &impl ConnectionTrait,
        user_id: &str,
        issuer: &str,
        client_id: &str,
    ) -> Result<Option<GatewayClientRow>, DomainError> {
        Ok(
            GatewayClientRow::find_by_statement(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT id, name, allowed_providers, last_used_at FROM gateway_clients
            WHERE user_id = ? AND kind = 'oauth' AND oauth_client_id = ? AND oauth_issuer = ?",
                [
                    user_id.to_owned().into(),
                    client_id.to_owned().into(),
                    issuer.to_owned().into(),
                ],
            ))
            .one(db)
            .await?,
        )
    }

    /// A paired machine's token, checked for the gateway resource and permission.
    pub async fn authenticate_machine_token(
        &self,
        token: &str,
        issuer: &str,
    ) -> Result<Principal, DomainError> {
        let principal = self
            .authenticate_access_token(token, issuer, &gateway_resource(issuer))
            .await?;
        oauth_client_id(&principal)?;
        Ok(principal)
    }

    /// The gateway client of a machine's token. `Forbidden` when `provider` is not granted to it.
    pub async fn authenticate_machine(
        &self,
        token: &str,
        issuer: &str,
        provider: &str,
    ) -> Result<MachineClient, DomainError> {
        let principal = self.authenticate_machine_token(token, issuer).await?;
        let user_id = principal.user_id().to_string();
        let row = self
            .gateway_client(&self.db, &user_id, issuer, oauth_client_id(&principal)?)
            .await?
            .ok_or(DomainError::InvalidCredentials)?;
        if !parse_providers(&row.allowed_providers)?
            .iter()
            .any(|id| id == provider)
        {
            return Err(DomainError::Forbidden);
        }
        let now = self.clock.now();
        let stale = row
            .last_used_at
            .as_deref()
            .and_then(|value| parse_stored_date(value).ok())
            .is_none_or(|used| now - used >= LAST_USED_PRECISION);
        if stale {
            let updated = async {
                crate::db::writer(&self.db)
                    .await?
                    .execute_raw(Statement::from_sql_and_values(
                        DbBackend::Sqlite,
                        "UPDATE gateway_clients SET last_used_at = ? WHERE id = ?",
                        [now.to_rfc3339().into(), row.id.clone().into()],
                    ))
                    .await
            }
            .await;
            if let Err(error) = updated {
                tracing::warn!(%error, "failed to update connected app last use");
            }
        }
        Ok(MachineClient {
            id: row.id,
            user_id,
        })
    }

    pub async fn machine(
        &self,
        principal: &Principal,
        issuer: &str,
    ) -> Result<aegis_wire::Machine, DomainError> {
        let user_id = principal.user_id().to_string();
        let row = self
            .gateway_client(&self.db, &user_id, issuer, oauth_client_id(principal)?)
            .await?
            .ok_or(DomainError::InvalidCredentials)?;
        let account = users::Entity::find_by_id(user_id)
            .one(&self.db)
            .await?
            .ok_or(DomainError::InvalidCredentials)?
            .email_display;
        Ok(aegis_wire::Machine {
            account,
            name: row.name,
            providers: parse_providers(&row.allowed_providers)?,
        })
    }

    /// Revokes the machine's own grant, as disconnecting it on the account page does.
    pub async fn unpair_machine(
        &self,
        principal: &Principal,
        issuer: &str,
    ) -> Result<(), DomainError> {
        let client_id = oauth_client_id(principal)?.to_owned();
        self.revoke_client_grants(principal, issuer, &client_id)
            .await
    }

    pub async fn set_machine_providers(
        &self,
        principal: &Principal,
        issuer: &str,
        client_id: &str,
        providers: &[String],
    ) -> Result<(), DomainError> {
        validate_issuer(issuer).map_err(|_| DomainError::InvalidInput("invalid issuer"))?;
        validate_client_id(client_id).map_err(|_| DomainError::NotFound)?;
        if providers.is_empty() {
            return Err(DomainError::InvalidInput("no providers selected"));
        }
        let connected = self
            .list_connected_clients(principal, issuer)
            .await?
            .into_iter()
            .any(|app| app.client_id == client_id && app.providers.is_some());
        if !connected {
            return Err(DomainError::NotFound);
        }
        crate::db::writer(&self.db)
            .await?
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "UPDATE gateway_clients SET allowed_providers = ?
                WHERE user_id = ? AND kind = 'oauth' AND oauth_client_id = ? AND oauth_issuer = ?",
                [
                    provider_list(providers).into(),
                    principal.user_id().to_string().into(),
                    client_id.to_owned().into(),
                    issuer.to_owned().into(),
                ],
            ))
            .await?;
        Ok(())
    }

    /// Providers and last use of the user's paired machines, by OAuth client ID.
    pub(super) async fn machine_grants(
        &self,
        user_id: &str,
        issuer: &str,
    ) -> Result<Vec<(String, Vec<String>, Option<String>)>, DomainError> {
        #[derive(FromQueryResult)]
        struct Row {
            oauth_client_id: String,
            allowed_providers: String,
            last_used_at: Option<String>,
        }
        Row::find_by_statement(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT oauth_client_id, allowed_providers, last_used_at FROM gateway_clients
            WHERE user_id = ? AND kind = 'oauth' AND oauth_issuer = ? AND oauth_client_id IS NOT NULL",
            [user_id.to_owned().into(), issuer.to_owned().into()],
        ))
        .all(&self.db)
        .await?
        .into_iter()
        .map(|row| {
            Ok((
                row.oauth_client_id,
                parse_providers(&row.allowed_providers)?,
                row.last_used_at,
            ))
        })
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::oauth::{
        ClientRegistration, DEVICE_GRANT_TYPE, DeviceAuthorizationRequest, DeviceTokenRequest,
        RefreshTokenRequest, tests::setup,
    };

    const ISSUER: &str = "https://aegis.example";
    const GATEWAY: &str = "https://aegis.example/providers";

    async fn machine_client(oauth: &OAuthService) -> super::super::RegisteredClient {
        oauth
            .register_public_client(ClientRegistration {
                issuer: ISSUER.into(),
                redirect_uris: vec![],
                client_name: Some("ags on laptop".into()),
                application_type: Some("native".into()),
                grant_types: vec![DEVICE_GRANT_TYPE.into(), "refresh_token".into()],
                response_types: vec![],
                scope: "gateway:use offline_access".into(),
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_paired_machine_reaches_only_its_providers_until_it_is_revoked() {
        let (_, oauth, _, clock, identity, _) = setup().await;
        let client = machine_client(&oauth).await;
        let issued = oauth
            .issue_device_authorization(DeviceAuthorizationRequest {
                client_id: client.client_id(),
                issuer: ISSUER,
                resource: GATEWAY,
                scope: None,
            })
            .await
            .unwrap();
        let found = oauth
            .find_device_authorization(issued.user_code())
            .await
            .unwrap();
        assert!(found.gateway);
        let scope = Some("gateway:use offline_access");
        assert!(matches!(
            oauth
                .decide_device_authorization(issued.user_code(), &identity, scope, Some(&[]))
                .await,
            Err(DomainError::InvalidInput(_))
        ));
        assert!(matches!(
            oauth
                .decide_device_authorization(issued.user_code(), &identity, scope, None)
                .await,
            Err(DomainError::InvalidInput(_))
        ));
        let providers = ["claude".to_owned(), "codex".to_owned(), "claude".to_owned()];
        oauth
            .decide_device_authorization(issued.user_code(), &identity, scope, Some(&providers))
            .await
            .unwrap();
        let token = oauth
            .redeem_device_code(DeviceTokenRequest {
                device_code: issued.device_code(),
                client_id: client.client_id(),
                issuer: ISSUER,
                resource: Some(GATEWAY),
            })
            .await
            .unwrap();

        let machine = oauth
            .authenticate_machine(token.expose(), ISSUER, "claude")
            .await
            .unwrap();
        assert!(matches!(
            oauth
                .authenticate_machine(token.expose(), ISSUER, "typesafe")
                .await,
            Err(DomainError::Forbidden)
        ));
        assert!(matches!(
            oauth
                .authenticate_machine(token.expose(), "https://other.example", "claude")
                .await,
            Err(DomainError::InvalidCredentials)
        ));
        let principal = oauth
            .authenticate_machine_token(token.expose(), ISSUER)
            .await
            .unwrap();
        let status = oauth.machine(&principal, ISSUER).await.unwrap();
        assert_eq!(status.account, "admin@example.com");
        assert_eq!(status.name, "ags on laptop");
        assert_eq!(status.providers, ["claude", "codex"]);

        // An edit on the web applies from the next request.
        oauth
            .set_machine_providers(
                &principal,
                ISSUER,
                client.client_id(),
                &["typesafe".to_owned()],
            )
            .await
            .unwrap();
        assert!(
            oauth
                .authenticate_machine(token.expose(), ISSUER, "typesafe")
                .await
                .is_ok()
        );

        // The refresh token outlives the access token and keeps the same gateway client.
        clock.advance(chrono::Duration::hours(2));
        assert!(
            oauth
                .authenticate_machine(token.expose(), ISSUER, "typesafe")
                .await
                .is_err()
        );
        let refreshed = oauth
            .refresh_access_token(RefreshTokenRequest {
                refresh_token: token.refresh_token().unwrap(),
                client_id: client.client_id(),
                issuer: ISSUER,
                resource: GATEWAY,
            })
            .await
            .unwrap();
        let again = oauth
            .authenticate_machine(refreshed.expose(), ISSUER, "typesafe")
            .await
            .unwrap();
        assert_eq!(again.id, machine.id);

        let principal = oauth
            .authenticate_machine_token(refreshed.expose(), ISSUER)
            .await
            .unwrap();
        oauth.unpair_machine(&principal, ISSUER).await.unwrap();
        assert!(matches!(
            oauth
                .authenticate_machine(refreshed.expose(), ISSUER, "typesafe")
                .await,
            Err(DomainError::InvalidCredentials)
        ));
        assert!(
            oauth
                .machine_grants(&identity.user_id.to_string(), ISSUER)
                .await
                .unwrap()[0]
                .1
                .is_empty(),
            "a revoked machine holds no provider"
        );
    }

    #[tokio::test]
    async fn gateway_access_needs_the_gateway_resource_and_the_device_flow() {
        let (_, oauth, _, _, identity, _) = setup().await;
        let client = machine_client(&oauth).await;
        let issue = |resource, scope| DeviceAuthorizationRequest {
            client_id: client.client_id(),
            issuer: ISSUER,
            resource,
            scope,
        };
        for (resource, scope) in [
            ("https://aegis.example/mcp", None),
            (GATEWAY, Some("offline_access")),
        ] {
            assert!(matches!(
                oauth
                    .issue_device_authorization(issue(resource, scope))
                    .await,
                Err(DomainError::InvalidInput("invalid scope"))
            ));
        }

        // An account-data grant never takes providers.
        let mcp_client = oauth
            .register_public_client(ClientRegistration {
                issuer: ISSUER.into(),
                redirect_uris: vec![],
                client_name: None,
                application_type: Some("native".into()),
                grant_types: vec![DEVICE_GRANT_TYPE.into()],
                response_types: vec![],
                scope: "usage:read".into(),
            })
            .await
            .unwrap();
        let issued = oauth
            .issue_device_authorization(DeviceAuthorizationRequest {
                client_id: mcp_client.client_id(),
                issuer: ISSUER,
                resource: "https://aegis.example/mcp",
                scope: None,
            })
            .await
            .unwrap();
        assert!(matches!(
            oauth
                .decide_device_authorization(
                    issued.user_code(),
                    &identity,
                    Some("usage:read"),
                    Some(&["claude".to_owned()]),
                )
                .await,
            Err(DomainError::InvalidInput(_))
        ));
    }
}
