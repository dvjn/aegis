//! Paired machines use their access token at the gateway in place of an API key.
use chrono::TimeDelta;
use sea_orm::{
    ConnectionTrait, DatabaseTransaction, DbBackend, EntityTrait, FromQueryResult, Statement,
};
use std::collections::HashSet;
use uuid::Uuid;

use super::{
    GATEWAY_SCOPE, GrantContext, OAuthService, gateway_resource, parse_stored_date, scope_allows,
    validate_client_id, validate_issuer, validate_resource,
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

pub(super) struct MachineGrant {
    pub id: String,
    pub client_id: String,
    pub providers: Vec<String>,
    pub last_used_at: Option<String>,
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
        validate_issuer(issuer).map_err(|_| DomainError::InvalidCredentials)?;
        let resource = gateway_resource(issuer);
        validate_resource(&resource).map_err(|_| DomainError::InvalidCredentials)?;
        self.authenticate_token(
            token,
            &self.machine_use(issuer, &resource, self.clock.now()),
        )
        .await
    }

    /// The gateway clients of paired machines, of `user` or of everyone, that can still authenticate.
    pub async fn live_machine_ids(
        &self,
        db: &impl ConnectionTrait,
        user: Option<Uuid>,
    ) -> Result<HashSet<String>, DomainError> {
        #[derive(FromQueryResult)]
        struct Row {
            id: String,
            user_id: String,
            oauth_client_id: String,
            oauth_issuer: String,
        }
        let user = user.map(|user| user.to_string());
        let rows = Row::find_by_statement(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT id, user_id, oauth_client_id, oauth_issuer FROM gateway_clients
            WHERE kind = 'oauth' AND oauth_client_id IS NOT NULL AND oauth_issuer IS NOT NULL
            AND (?1 IS NULL OR user_id = ?1)",
            [user.into()],
        ))
        .all(db)
        .await?;
        if rows.is_empty() {
            return Ok(HashSet::new());
        }
        let now = self.clock.now();
        let user_ids = rows.iter().map(|row| row.user_id.clone()).collect();
        let client_ids = rows.iter().map(|row| row.oauth_client_id.clone()).collect();
        let grants = self
            .load_grants(db, user_ids, Some(client_ids), now)
            .await?;
        Ok(rows
            .into_iter()
            .filter(|row| {
                let resource = gateway_resource(&row.oauth_issuer);
                let machine_use = self.machine_use(&row.oauth_issuer, &resource, now);
                grants.usable(&row.user_id, &row.oauth_client_id, &machine_use)
            })
            .map(|row| row.id)
            .collect())
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
            provider_types: Default::default(),
            provider_regions: Default::default(),
            provider_locations: Default::default(),
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

    /// The user's paired machines at `issuer`, whether or not they can still authenticate.
    pub(super) async fn machine_grants(
        &self,
        user_id: &str,
        issuer: &str,
    ) -> Result<Vec<MachineGrant>, DomainError> {
        #[derive(FromQueryResult)]
        struct Row {
            id: String,
            oauth_client_id: String,
            allowed_providers: String,
            last_used_at: Option<String>,
        }
        Row::find_by_statement(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT id, oauth_client_id, allowed_providers, last_used_at FROM gateway_clients
            WHERE user_id = ? AND kind = 'oauth' AND oauth_issuer = ? AND oauth_client_id IS NOT NULL",
            [user_id.to_owned().into(), issuer.to_owned().into()],
        ))
        .all(&self.db)
        .await?
        .into_iter()
        .map(|row| {
            Ok(MachineGrant {
                id: row.id,
                client_id: row.oauth_client_id,
                providers: parse_providers(&row.allowed_providers)?,
                last_used_at: row.last_used_at,
            })
        })
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        Clock,
        auth::Identity,
        oauth::{
            ClientRegistration, DEVICE_GRANT_TYPE, DeviceAuthorizationRequest, DeviceTokenRequest,
            OAuthConfig, RefreshTokenRequest, tests::setup,
        },
    };
    use crate::settings::{Scope, SettingsStore};
    use sea_orm::DatabaseConnection;

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
                .providers
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

    /// A machine paired for `claude`, and the tokens it holds now.
    struct Paired {
        id: String,
        client_id: String,
        access: String,
        refresh: Option<String>,
    }

    async fn pair(oauth: &OAuthService, identity: &Identity, offline: bool) -> Paired {
        let client = machine_client(oauth).await;
        pair_client(oauth, identity, client.client_id(), offline).await
    }

    async fn pair_client(
        oauth: &OAuthService,
        identity: &Identity,
        client_id: &str,
        offline: bool,
    ) -> Paired {
        let issued = oauth
            .issue_device_authorization(DeviceAuthorizationRequest {
                client_id,
                issuer: ISSUER,
                resource: GATEWAY,
                scope: None,
            })
            .await
            .unwrap();
        let scope = if offline {
            "gateway:use offline_access"
        } else {
            "gateway:use"
        };
        oauth
            .decide_device_authorization(
                issued.user_code(),
                identity,
                Some(scope),
                Some(&["claude".to_owned()]),
            )
            .await
            .unwrap();
        let token = oauth
            .redeem_device_code(DeviceTokenRequest {
                device_code: issued.device_code(),
                client_id,
                issuer: ISSUER,
                resource: Some(GATEWAY),
            })
            .await
            .unwrap();
        let machine = oauth
            .authenticate_machine(token.expose(), ISSUER, "claude")
            .await
            .unwrap();
        Paired {
            id: machine.id,
            client_id: client_id.to_owned(),
            access: token.expose().to_owned(),
            refresh: token.refresh_token().map(str::to_owned),
        }
    }

    impl Paired {
        async fn refresh(&mut self, oauth: &OAuthService) -> bool {
            let Some(refresh) = &self.refresh else {
                return false;
            };
            let Ok(token) = oauth
                .refresh_access_token(RefreshTokenRequest {
                    refresh_token: refresh,
                    client_id: &self.client_id,
                    issuer: ISSUER,
                    resource: GATEWAY,
                })
                .await
            else {
                return false;
            };
            self.access = token.expose().to_owned();
            self.refresh = token.refresh_token().map(str::to_owned);
            true
        }
    }

    /// Which of the user's machines are live, checked against authenticating,
    /// refreshing, the provider settings and the account page.
    async fn live_each(
        db: &DatabaseConnection,
        oauth: &OAuthService,
        identity: &Identity,
        machines: &mut [Paired],
    ) -> Vec<bool> {
        let all = oauth.live_machine_ids(db, None).await.unwrap();
        let mine = oauth
            .live_machine_ids(db, Some(identity.user_id))
            .await
            .unwrap();
        let settings = SettingsStore::new(db.clone());
        let counted = settings
            .grant_counts(Scope::Instance, oauth)
            .await
            .unwrap()
            .get("claude")
            .map_or(0, |&(_, machines)| machines);
        let users = settings
            .instance_provider_users("claude", oauth)
            .await
            .unwrap()
            .len();
        let principal = Principal {
            identity: identity.clone(),
            transport: PrincipalTransport::BrowserSession {
                session_id: Uuid::nil(),
            },
        };
        let apps = oauth
            .list_connected_clients(&principal, ISSUER)
            .await
            .unwrap();
        let mut each = Vec::new();
        for machine in machines {
            let live = all.contains(&machine.id);
            let shown = apps
                .iter()
                .any(|app| app.client_id == machine.client_id && app.providers.is_some());
            let editable = oauth
                .set_machine_providers(&principal, ISSUER, &machine.client_id, &["claude".into()])
                .await
                .is_ok();
            let authenticates = oauth
                .authenticate_machine(&machine.access, ISSUER, "claude")
                .await
                .is_ok();
            let refreshes = machine.refresh(oauth).await;
            assert_eq!(live, mine.contains(&machine.id));
            assert_eq!(live, authenticates || refreshes);
            assert_eq!(shown, live);
            assert_eq!(editable, live);
            each.push(live);
        }
        let live = each.iter().filter(|&&live| live).count();
        assert_eq!(counted, live as i64);
        assert_eq!(users, usize::from(live > 0));
        each
    }

    async fn live(
        db: &DatabaseConnection,
        oauth: &OAuthService,
        identity: &Identity,
        machine: &mut Paired,
    ) -> bool {
        live_each(db, oauth, identity, std::slice::from_mut(machine)).await[0]
    }

    enum Ending {
        Disconnected,
        AccessExpired,
        AccessRevoked,
        AccessRevokedRefreshKept,
        RefreshIdle,
        RefreshAbsolute,
        RefreshReused,
        AuthVersionChanged,
        UserInactive,
    }

    /// Ends a machine's grant in one way and checks every view agrees on whether it is live.
    async fn end(ending: Ending) {
        let day = chrono::Duration::days(1);
        let two_hours = chrono::Duration::hours(2);
        let (db, oauth, _, clock, identity, _) = setup().await;
        let offline = !matches!(ending, Ending::AccessExpired | Ending::AccessRevoked);
        let mut machine = pair(&oauth, &identity, offline).await;
        assert!(live(&db, &oauth, &identity, &mut machine).await);
        let still_live = matches!(ending, Ending::AccessRevokedRefreshKept);
        match ending {
            Ending::Disconnected => {
                let principal = oauth
                    .authenticate_machine_token(&machine.access, ISSUER)
                    .await
                    .unwrap();
                oauth.unpair_machine(&principal, ISSUER).await.unwrap();
            }
            Ending::AccessExpired => clock.advance(two_hours),
            Ending::AccessRevoked | Ending::AccessRevokedRefreshKept => {
                db.execute_unprepared(
                    "UPDATE oauth_access_tokens SET revoked_at = '2026-08-16T12:00:00+00:00'",
                )
                .await
                .unwrap();
            }
            Ending::RefreshIdle => {
                clock.advance(two_hours);
                assert!(live(&db, &oauth, &identity, &mut machine).await);
                clock.advance(day * 180);
            }
            Ending::RefreshAbsolute => {
                for _ in 0..2 {
                    clock.advance(day * 170);
                    assert!(live(&db, &oauth, &identity, &mut machine).await);
                }
                clock.advance(day * 24);
                assert!(live(&db, &oauth, &identity, &mut machine).await);
                clock.advance(day * 2);
            }
            Ending::RefreshReused => reuse_refresh_token(&oauth, &mut machine).await,
            Ending::AuthVersionChanged => {
                db.execute_unprepared("UPDATE users SET auth_version=auth_version+1")
                    .await
                    .unwrap();
            }
            Ending::UserInactive => {
                db.execute_unprepared("UPDATE users SET status='disabled'")
                    .await
                    .unwrap();
            }
        }
        assert_eq!(live(&db, &oauth, &identity, &mut machine).await, still_live);
        assert_eq!(
            oauth
                .machine_grants(&identity.user_id.to_string(), ISSUER)
                .await
                .unwrap()
                .len(),
            1,
            "the machine stays for its recorded usage"
        );
    }

    /// Refreshes, then presents the replaced refresh token again, which revokes the family.
    async fn reuse_refresh_token(oauth: &OAuthService, machine: &mut Paired) {
        let replaced = machine.refresh.clone().unwrap();
        assert!(machine.refresh(oauth).await);
        assert!(
            oauth
                .refresh_access_token(RefreshTokenRequest {
                    refresh_token: &replaced,
                    client_id: &machine.client_id,
                    issuer: ISSUER,
                    resource: GATEWAY,
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_disconnected_machine_is_not_live() {
        end(Ending::Disconnected).await;
    }

    #[tokio::test]
    async fn a_machine_without_a_refresh_token_ends_with_its_access_token() {
        end(Ending::AccessExpired).await;
    }

    #[tokio::test]
    async fn a_machine_whose_only_access_token_is_revoked_is_not_live() {
        end(Ending::AccessRevoked).await;
    }

    #[tokio::test]
    async fn a_machine_whose_access_token_is_revoked_stays_live_through_its_refresh_token() {
        end(Ending::AccessRevokedRefreshKept).await;
    }

    #[tokio::test]
    async fn a_machine_ends_when_its_refresh_token_goes_idle() {
        end(Ending::RefreshIdle).await;
    }

    #[tokio::test]
    async fn a_machine_ends_when_its_refresh_family_reaches_its_absolute_lifetime() {
        end(Ending::RefreshAbsolute).await;
    }

    #[tokio::test]
    async fn a_machine_ends_when_refresh_token_reuse_revokes_its_family() {
        end(Ending::RefreshReused).await;
    }

    #[tokio::test]
    async fn a_machine_ends_when_the_user_auth_version_changes() {
        end(Ending::AuthVersionChanged).await;
    }

    #[tokio::test]
    async fn a_machine_ends_when_its_user_is_inactive() {
        end(Ending::UserInactive).await;
    }

    #[tokio::test]
    async fn one_machine_ending_leaves_the_users_other_machine_live() {
        let (db, oauth, _, _, identity, _) = setup().await;
        let mut machines = [
            pair(&oauth, &identity, true).await,
            pair(&oauth, &identity, true).await,
        ];
        assert_eq!(
            live_each(&db, &oauth, &identity, &mut machines).await,
            [true, true]
        );
        reuse_refresh_token(&oauth, &mut machines[0]).await;
        assert_eq!(
            live_each(&db, &oauth, &identity, &mut machines).await,
            [false, true]
        );
    }

    #[tokio::test]
    async fn a_connected_app_dates_from_its_usable_grant() {
        let (db, oauth, _, clock, identity, _) = setup().await;
        let mut first = pair(&oauth, &identity, true).await;
        clock.advance(chrono::Duration::days(200));
        assert!(!live(&db, &oauth, &identity, &mut first).await);
        let paired_again_at = clock.now();
        let again = pair_client(&oauth, &identity, &first.client_id, true).await;
        assert_eq!(again.id, first.id);
        let principal = Principal {
            identity: identity.clone(),
            transport: PrincipalTransport::BrowserSession {
                session_id: Uuid::nil(),
            },
        };
        let apps = oauth
            .list_connected_clients(&principal, ISSUER)
            .await
            .unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].created_at, paired_again_at);
    }

    #[tokio::test]
    async fn a_replaced_refresh_token_for_another_use_is_refused_without_revoking_its_family() {
        let (db, oauth, _, clock, identity, _) = setup().await;
        let mut machine = pair(&oauth, &identity, true).await;
        let replaced = machine.refresh.clone().unwrap();
        assert!(machine.refresh(&oauth).await);
        let other_client = machine_client(&oauth).await;
        let other_key = OAuthService::new(
            db.clone(),
            OAuthConfig {
                hmac_key: [8; 32],
                key_id: "oauth-2".into(),
                access_token_lifetime: OAuthConfig::DEFAULT_ACCESS_TOKEN_LIFETIME,
            },
            clock.clone(),
        )
        .unwrap();
        let other_issuer = "https://other.example";
        for (service, client_id, issuer, resource) in [
            (&other_key, machine.client_id.as_str(), ISSUER, GATEWAY),
            (
                &oauth,
                machine.client_id.as_str(),
                other_issuer,
                "https://other.example/providers",
            ),
            (
                &oauth,
                machine.client_id.as_str(),
                ISSUER,
                "https://aegis.example/mcp",
            ),
            (&oauth, other_client.client_id(), ISSUER, GATEWAY),
        ] {
            assert!(
                service
                    .refresh_access_token(RefreshTokenRequest {
                        refresh_token: &replaced,
                        client_id,
                        issuer,
                        resource,
                    })
                    .await
                    .is_err()
            );
        }
        assert!(machine.refresh(&oauth).await, "the family is not revoked");
    }

    #[tokio::test]
    async fn keeping_an_instance_provider_skips_users_whose_machines_ended() {
        let (db, oauth, _, _, admin, _) = setup().await;
        let bob = Uuid::now_v7();
        db.execute_unprepared(&format!(
            "INSERT INTO users(id,email_normalized,email_display,role,status,auth_version,created_at,updated_at) VALUES('{bob}','bob@example.com','bob@example.com','user','active',0,'2026-01-01','2026-01-01')"
        ))
        .await
        .unwrap();
        let bob_identity = Identity {
            user_id: bob,
            role: "user".into(),
            auth_version: 0,
        };
        pair(&oauth, &admin, true).await;
        pair(&oauth, &bob_identity, true).await;
        let settings = SettingsStore::new(db.clone());
        settings
            .create_provider(
                Scope::Instance,
                &crate::config::ProviderConfig {
                    id: "claude".into(),
                    kind: crate::config::ProviderKind::from_parts("claude_subscription", None)
                        .unwrap(),
                },
            )
            .await
            .unwrap();
        db.execute_unprepared(&format!(
            "UPDATE users SET auth_version=auth_version+1 WHERE id='{bob}'"
        ))
        .await
        .unwrap();

        assert_eq!(
            settings.move_to_users("claude", &oauth).await.unwrap(),
            Some(1)
        );
        assert_eq!(
            settings
                .providers(Scope::User(admin.user_id))
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            settings
                .providers(Scope::User(bob))
                .await
                .unwrap()
                .is_empty()
        );
        for (user, providers) in [(admin.user_id, vec!["claude"]), (bob, vec![])] {
            let machines = oauth
                .machine_grants(&user.to_string(), ISSUER)
                .await
                .unwrap();
            assert_eq!(machines[0].providers, providers);
        }
    }
}
