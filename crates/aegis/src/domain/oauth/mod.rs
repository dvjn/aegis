mod clients;
mod codes;
mod device;
mod grants;
mod refresh;
mod tokens;

pub use clients::ClientRegistration;
#[cfg(test)]
pub use clients::RegisteredClient;
#[cfg(test)]
pub use codes::IssuedAuthorizationCode;
pub use codes::{AuthorizationCodeRedemption, AuthorizationCodeRequest};
pub use device::{
    DeviceAuthorizationRequest, DevicePollError, DeviceTokenRequest, normalize_user_code,
};
pub use grants::ConnectedClient;
pub(super) use grants::GrantContext;
pub use refresh::RefreshTokenRequest;

use std::{collections::HashSet, net::IpAddr, sync::Arc, time::Duration};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use rand::Rng;
use sea_orm::{
    ColumnTrait, DatabaseConnection,
    sea_query::{Expr, Func, SimpleExpr},
};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;
use zeroize::Zeroize;

use super::{Clock, error::DomainError};

pub const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

pub const SCOPES: [&str; 5] = [
    "usage:read",
    "requests:read",
    "payloads:read",
    "keys:read",
    "offline_access",
];

pub fn role_allows_scope(scope: &str, role: &str) -> bool {
    scope != "payloads:read" || role == "superuser"
}

pub const OFFLINE_ACCESS: &str = "offline_access";

pub fn resource_scopes() -> Vec<&'static str> {
    SCOPES
        .into_iter()
        .filter(|scope| *scope != OFFLINE_ACCESS)
        .collect()
}

pub fn default_registration_scope(refresh_token_grant: bool) -> String {
    SCOPES
        .into_iter()
        .filter(|scope| refresh_token_grant || *scope != OFFLINE_ACCESS)
        .collect::<Vec<_>>()
        .join(" ")
}
const TOKEN_PREFIX: &str = "ag_at1";
const TOKEN_DOMAIN: &[u8] = b"aegis/oauth-access-token/v1\0";
const MAX_URI_LEN: usize = 2048;
const MAX_SCOPE_LEN: usize = 512;

#[derive(Clone)]
pub struct OAuthConfig {
    pub hmac_key: [u8; 32],
    pub key_id: String,
}
impl OAuthConfig {
    pub const CODE_LIFETIME: Duration = Duration::from_secs(5 * 60);
    pub const DEVICE_LIFETIME: Duration = Duration::from_secs(10 * 60);
    pub const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(5);
    pub const ACCESS_TOKEN_LIFETIME: Duration = Duration::from_secs(60 * 60);
    pub const REFRESH_IDLE_LIFETIME: Duration = Duration::from_secs(180 * 24 * 60 * 60);
    pub const REFRESH_ABSOLUTE_LIFETIME: Duration = Duration::from_secs(365 * 24 * 60 * 60);
}
impl Drop for OAuthConfig {
    fn drop(&mut self) {
        self.hmac_key.zeroize();
    }
}

#[derive(Clone)]
pub struct OAuthService {
    db: DatabaseConnection,
    config: Arc<OAuthConfig>,
    clock: Arc<dyn Clock>,
}

pub struct IssuedAccessToken {
    value: String,
    refresh_token: Option<String>,
    scope: String,
}
impl IssuedAccessToken {
    pub fn expose(&self) -> &str {
        &self.value
    }
    pub fn scope(&self) -> &str {
        &self.scope
    }
    pub fn refresh_token(&self) -> Option<&str> {
        self.refresh_token.as_deref()
    }
    pub fn expires_in(&self) -> u64 {
        OAuthConfig::ACCESS_TOKEN_LIFETIME.as_secs()
    }
}
impl Drop for IssuedAccessToken {
    fn drop(&mut self) {
        self.value.zeroize();
        if let Some(value) = &mut self.refresh_token {
            value.zeroize();
        }
    }
}

impl OAuthService {
    pub fn new(
        db: DatabaseConnection,
        config: OAuthConfig,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, DomainError> {
        if config.key_id.is_empty() || config.key_id.len() > 64 {
            return Err(DomainError::InvalidInput("invalid OAuth configuration"));
        }
        Ok(Self {
            db,
            config: Arc::new(config),
            clock,
        })
    }

    pub(super) async fn begin_immediate(&self) -> Result<crate::db::WriteTransaction, DomainError> {
        crate::db::begin_immediate(&self.db).await.map_err(|error| {
            tracing::debug!(%error, "could not start immediate transaction");
            DomainError::TemporarilyUnavailable
        })
    }
}

fn scope_is_subset(requested: &str, registered: &str) -> bool {
    validate_scope(requested).is_ok()
        && requested
            .split(' ')
            .all(|item| scope_allows(registered, item))
}

fn redirect_uri_matches(registered: &str, requested: &str) -> bool {
    if registered == requested {
        return true;
    }
    let (Ok(registered), Ok(requested)) = (Url::parse(registered), Url::parse(requested)) else {
        return false;
    };
    if !is_loopback_redirect(&registered) || !is_loopback_redirect(&requested) {
        return false;
    }
    registered.scheme() == requested.scheme()
        && registered.host_str() == requested.host_str()
        && registered.path() == requested.path()
        && registered.query() == requested.query()
        && registered.username() == requested.username()
        && registered.password() == requested.password()
}

fn validate_issuer(value: &str) -> Result<(), DomainError> {
    let url = parse_bounded_uri(value, "invalid issuer")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(DomainError::InvalidInput("invalid issuer"));
    }
    Ok(())
}

fn validate_redirect_uri(value: &str) -> Result<(), DomainError> {
    let url = parse_bounded_uri(value, "invalid redirect URI")?;
    if url.fragment().is_some() || !url.username().is_empty() || url.password().is_some() {
        return Err(DomainError::InvalidInput("invalid redirect URI"));
    }
    if (url.scheme() == "https" && url.host_str().is_some())
        || is_loopback_redirect(&url)
        || is_private_use_redirect(&url)
    {
        Ok(())
    } else {
        Err(DomainError::InvalidInput("invalid redirect URI"))
    }
}

fn validate_resource(value: &str) -> Result<(), DomainError> {
    let url = parse_bounded_uri(value, "invalid resource")?;
    if url.fragment().is_some() || !url.username().is_empty() || url.password().is_some() {
        return Err(DomainError::InvalidInput("invalid resource"));
    }
    Ok(())
}

fn parse_bounded_uri(value: &str, message: &'static str) -> Result<Url, DomainError> {
    if value.is_empty()
        || value.len() > MAX_URI_LEN
        || !value.is_ascii()
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(DomainError::InvalidInput(message));
    }
    Url::parse(value).map_err(|_| DomainError::InvalidInput(message))
}

fn is_loopback_redirect(url: &Url) -> bool {
    url.scheme() == "http"
        && url.port().is_some()
        && url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .trim_matches(['[', ']'])
                    .parse::<IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        })
}

fn is_private_use_redirect(url: &Url) -> bool {
    let scheme = url.scheme();
    scheme.contains('.')
        && scheme.len() <= 128
        && scheme.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.')
        })
        && url.host_str().is_none()
        && !url.path().is_empty()
}

fn validate_scope(scope: &str) -> Result<(), DomainError> {
    if scope.is_empty()
        || scope.len() > MAX_SCOPE_LEN
        || scope.starts_with(' ')
        || scope.ends_with(' ')
        || scope.contains("  ")
    {
        return Err(DomainError::InvalidInput("invalid scope"));
    }
    let mut unique = HashSet::new();
    for item in scope.split(' ') {
        if !SCOPES.contains(&item) || !unique.insert(item) {
            return Err(DomainError::InvalidInput("invalid scope"));
        }
    }
    Ok(())
}

pub fn scope_allows(granted: &str, required: &str) -> bool {
    granted.split(' ').any(|item| item == required)
}

pub fn normalize_scope(scope: &str) -> String {
    let mut items: Vec<&str> = Vec::new();
    for item in scope.split(' ').filter(|item| !item.is_empty()) {
        if !items.contains(&item) {
            items.push(item);
        }
    }
    items.join(" ")
}

pub fn consent_scope(scope: &str, role: &str) -> String {
    scope
        .split(' ')
        .filter(|item| role_allows_scope(item, role))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn granted_scope(requested: &str, selected: &[String], role: &str) -> Option<String> {
    let mut kept_items: Vec<&str> = Vec::new();
    for choice in selected {
        let item = choice.as_str();
        if !item.is_empty()
            && !kept_items.contains(&item)
            && !item.contains(' ')
            && scope_allows(requested, item)
        {
            kept_items.push(item);
        }
    }
    if scope_allows(requested, "offline_access") && !kept_items.contains(&"offline_access") {
        kept_items.push("offline_access");
    }
    let kept = kept_items.join(" ");
    let scope = normalize_scope(&consent_scope(&kept, role));
    validate_scope(&scope).is_ok().then_some(scope)
}

fn validate_client_id(client_id: &str) -> Result<(), DomainError> {
    if client_id.len() > 64 || Uuid::parse_str(client_id).is_err() {
        return Err(DomainError::InvalidInput("invalid client id"));
    }
    Ok(())
}

fn validate_code_verifier(verifier: &str) -> Result<(), DomainError> {
    if !(43..=128).contains(&verifier.len())
        || !verifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~'))
    {
        return Err(DomainError::InvalidInput("invalid PKCE verifier"));
    }
    Ok(())
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn new_opaque_value(prefix: &str) -> (Uuid, [u8; 16], [u8; 32], String) {
    let mut selector = [0_u8; 16];
    let mut secret = [0_u8; 32];
    rand::rng().fill_bytes(&mut selector);
    rand::rng().fill_bytes(&mut secret);
    let id = Uuid::from_bytes(selector);
    let value = format!(
        "{prefix}.{}.{}",
        URL_SAFE_NO_PAD.encode(selector),
        URL_SAFE_NO_PAD.encode(secret)
    );
    (id, selector, secret, value)
}

fn parse_opaque_value(
    value: &str,
    prefix: &str,
) -> Result<(Uuid, [u8; 16], [u8; 32]), DomainError> {
    if value.len() > 128 || value.contains('=') {
        return Err(DomainError::InvalidCredentials);
    }
    let mut parts = value.split('.');
    if parts.next() != Some(prefix) {
        return Err(DomainError::InvalidCredentials);
    }
    let selector = parts.next().ok_or(DomainError::InvalidCredentials)?;
    let secret = parts.next().ok_or(DomainError::InvalidCredentials)?;
    if parts.next().is_some() {
        return Err(DomainError::InvalidCredentials);
    }
    let selector: [u8; 16] = URL_SAFE_NO_PAD
        .decode(selector)
        .map_err(|_| DomainError::InvalidCredentials)?
        .try_into()
        .map_err(|_| DomainError::InvalidCredentials)?;
    let secret: [u8; 32] = URL_SAFE_NO_PAD
        .decode(secret)
        .map_err(|_| DomainError::InvalidCredentials)?
        .try_into()
        .map_err(|_| DomainError::InvalidCredentials)?;
    Ok((Uuid::from_bytes(selector), selector, secret))
}

fn add_duration(now: DateTime<Utc>, duration: Duration) -> Result<DateTime<Utc>, DomainError> {
    now.checked_add_signed(
        chrono::Duration::from_std(duration)
            .map_err(|_| DomainError::InvalidInput("invalid OAuth lifetime"))?,
    )
    .ok_or(DomainError::InvalidInput("invalid OAuth lifetime"))
}

fn parse_stored_date(value: &str) -> Result<DateTime<Utc>, DomainError> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| DomainError::InvalidCredentials)
}

fn keep_earliest<C: ColumnTrait>(column: C, now: DateTime<Utc>) -> SimpleExpr {
    SimpleExpr::from(Func::coalesce([
        Expr::col(column),
        Expr::val(now.to_rfc3339()),
    ]))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::{
        domain::{
            AuthConfig, AuthService, Password, auth::Identity, bootstrap_superuser,
            oauth::codes::AuthorizationCodeRedemption,
        },
        migration::Migrator,
    };
    use chrono::TimeZone;
    use sea_orm::{ConnectionTrait, Database};
    use sea_orm_migration::MigratorTrait;
    use std::sync::{Arc, RwLock};

    pub(super) struct MutableClock(RwLock<DateTime<Utc>>);
    impl Clock for MutableClock {
        fn now(&self) -> DateTime<Utc> {
            *self.0.read().unwrap()
        }
    }
    impl MutableClock {
        pub(super) fn advance(&self, duration: chrono::Duration) {
            *self.0.write().unwrap() += duration;
        }
    }

    fn oauth_config() -> OAuthConfig {
        OAuthConfig {
            hmac_key: [8; 32],
            key_id: "oauth-1".into(),
        }
    }

    pub(super) async fn setup() -> (
        DatabaseConnection,
        OAuthService,
        AuthService,
        Arc<MutableClock>,
        Identity,
        RegisteredClient,
    ) {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        setup_with(db).await
    }

    pub(super) async fn setup_with(
        db: DatabaseConnection,
    ) -> (
        DatabaseConnection,
        OAuthService,
        AuthService,
        Arc<MutableClock>,
        Identity,
        RegisteredClient,
    ) {
        db.execute_unprepared("PRAGMA foreign_keys=ON")
            .await
            .unwrap();
        Migrator::up(&db, None).await.unwrap();
        let clock = Arc::new(MutableClock(RwLock::new(
            Utc.with_ymd_and_hms(2026, 8, 16, 12, 0, 0).unwrap(),
        )));
        let auth = AuthService::new(
            db.clone(),
            AuthConfig {
                session_hmac_key: [3; 32],
                session_key_id: "session".into(),
                password_pepper: b"pepper".to_vec(),
                pepper_key_id: "pepper".into(),
                password_concurrency: 2,
                idle_lifetime: Duration::from_secs(60),
                absolute_lifetime: Duration::from_secs(120),
            },
            clock.clone(),
        )
        .await
        .unwrap();
        let password = Password::new("c0rrect horse battery staple!".into()).unwrap();
        bootstrap_superuser(&db, b"pepper", "pepper", "admin@example.com", &password)
            .await
            .unwrap();
        let identity = auth
            .verify_password_identity("admin@example.com", &password)
            .await
            .unwrap();
        let oauth = OAuthService::new(db.clone(), oauth_config(), clock.clone()).unwrap();
        let client = oauth
            .register_public_client(ClientRegistration {
                issuer: "https://aegis.example".into(),
                redirect_uris: vec!["http://127.0.0.1:49152/callback".into()],
                client_name: Some("Test CLI".into()),
                application_type: Some("native".into()),
                grant_types: vec!["authorization_code".into(), "refresh_token".into()],
                response_types: vec!["code".into()],
                scope: "usage:read offline_access".into(),
            })
            .await
            .unwrap();
        (db, oauth, auth, clock, identity, client)
    }

    pub(super) fn request<'a>(
        identity: &'a Identity,
        client: &'a RegisteredClient,
        challenge: &'a str,
    ) -> AuthorizationCodeRequest<'a> {
        AuthorizationCodeRequest {
            identity,
            client_id: client.client_id(),
            issuer: &client.issuer,
            redirect_uri: &client.redirect_uris()[0],
            resource: "https://aegis.example/v1",
            scope: "usage:read",
            code_challenge: challenge,
        }
    }

    pub(super) async fn code(
        oauth: &OAuthService,
        identity: &Identity,
        client: &RegisteredClient,
        verifier: &str,
    ) -> IssuedAuthorizationCode {
        code_with_scope(oauth, identity, client, verifier, "usage:read").await
    }

    pub(super) async fn code_with_scope(
        oauth: &OAuthService,
        identity: &Identity,
        client: &RegisteredClient,
        verifier: &str,
        scope: &str,
    ) -> IssuedAuthorizationCode {
        let challenge = pkce_challenge(verifier);
        let mut request = request(identity, client, &challenge);
        request.scope = scope;
        oauth.issue_authorization_code(request).await.unwrap()
    }

    pub(super) fn redemption<'a>(
        code: &'a str,
        client: &'a RegisteredClient,
        verifier: &'a str,
    ) -> AuthorizationCodeRedemption<'a> {
        AuthorizationCodeRedemption {
            code,
            client_id: client.client_id(),
            issuer: &client.issuer,
            redirect_uri: &client.redirect_uris()[0],
            resource: "https://aegis.example/v1",
            code_verifier: verifier,
        }
    }

    pub(super) fn refresh_request<'a>(
        token: &'a str,
        client: &'a RegisteredClient,
    ) -> RefreshTokenRequest<'a> {
        RefreshTokenRequest {
            refresh_token: token,
            client_id: client.client_id(),
            issuer: &client.issuer,
            resource: "https://aegis.example/v1",
        }
    }

    #[test]
    fn strict_uri_scope_pkce_and_token_parsing() {
        assert!(validate_redirect_uri("https://client.example/cb").is_ok());
        assert!(validate_redirect_uri("http://127.0.0.1:3000/cb").is_ok());
        assert!(validate_redirect_uri("http://[::1]:3000/cb").is_ok());
        assert!(validate_redirect_uri("http://localhost:3000/cb").is_ok());
        assert!(validate_redirect_uri("com.example.aegis:/oauth/callback").is_ok());
        assert!(validate_redirect_uri("com.example.aegis:/oauth/callback?source=mcp").is_ok());
        assert!(validate_redirect_uri("myapp:/oauth/callback").is_err());
        assert!(validate_redirect_uri("http://client.example/cb").is_err());
        assert!(validate_redirect_uri("https://client.example/cb#fragment").is_err());
        assert!(validate_redirect_uri("file:///tmp/token").is_err());
        assert!(redirect_uri_matches(
            "http://127.0.0.1:1/cb?fixed=yes",
            "http://127.0.0.1:49152/cb?fixed=yes"
        ));
        assert!(redirect_uri_matches(
            "http://localhost:1/cb?fixed=yes",
            "http://localhost:49152/cb?fixed=yes"
        ));
        assert!(!redirect_uri_matches(
            "http://127.0.0.1:1/cb?fixed=yes",
            "http://127.0.0.1:49152/other?fixed=yes"
        ));
        assert!(!redirect_uri_matches(
            "http://127.0.0.1:1/cb?fixed=yes",
            "http://127.0.0.1:49152/cb?fixed=no"
        ));
        assert!(validate_scope("usage:read offline_access").is_ok());
        assert!(validate_scope("usage:read requests:read").is_ok());
        assert!(validate_scope("usage:read payloads:read").is_ok());
        assert!(validate_scope("usage:read keys:read").is_ok());
        assert!(validate_scope("requests:read").is_ok());
        assert!(validate_scope("keys:read").is_ok());
        assert!(validate_scope("offline_access").is_ok());
        assert!(validate_scope("").is_err());
        assert!(validate_scope("usage:read usage:read").is_err());
        assert!(validate_scope("usage:read  requests:read").is_err());
        assert!(validate_scope("usage:read aegis:read").is_err());
        assert!(validate_code_verifier(&"a".repeat(43)).is_ok());
        assert!(validate_code_verifier("short").is_err());
        assert!(validate_scope(&SCOPES.join(" ")).is_ok());
        assert!(validate_scope(&resource_scopes().join(" ")).is_ok());
        assert!(!resource_scopes().contains(&OFFLINE_ACCESS));
        assert_eq!(default_registration_scope(true), SCOPES.join(" "));
        assert_eq!(
            default_registration_scope(false),
            resource_scopes().join(" ")
        );
        assert!(validate_scope(&default_registration_scope(true)).is_ok());
        assert!(validate_scope(&default_registration_scope(false)).is_ok());
        for token in ["", "ag_ac1.a.b", "ag_at1.a.b.extra", "ag_at1.a.b="] {
            assert!(parse_opaque_value(token, TOKEN_PREFIX).is_err());
        }
    }

    #[test]
    fn a_grant_allows_the_exact_scopes_that_it_names() {
        assert!(scope_allows("usage:read", "usage:read"));
        assert!(scope_allows(
            "usage:read keys:read payloads:read",
            "payloads:read"
        ));
        assert!(!scope_allows("requests:read", "invalid:write"));
        assert!(!scope_allows("requests:read", "usage:read"));
        assert!(!scope_allows("keys:read", "payloads:read"));
        assert!(!scope_allows("usage:read", "requests:read"));
        assert!(!scope_allows("offline_access", "usage:read"));
        assert!(!scope_allows("usage:read", "offline_access"));
        assert!(scope_allows("usage:read offline_access", "offline_access"));
    }

    #[test]
    fn consent_removes_payload_scope_for_non_superusers() {
        let requested = "usage:read requests:read payloads:read offline_access";
        assert_eq!(consent_scope(requested, "superuser"), requested);
        assert_eq!(
            consent_scope(requested, "user"),
            "usage:read requests:read offline_access"
        );
        assert!(
            validate_scope(&consent_scope("usage:read keys:read payloads:read", "user")).is_ok()
        );
    }

    #[test]
    fn normalization_drops_only_a_repeated_scope() {
        assert_eq!(
            normalize_scope("usage:read requests:read offline_access"),
            "usage:read requests:read offline_access"
        );
        assert_eq!(
            normalize_scope("usage:read requests:read usage:read"),
            "usage:read requests:read"
        );
        assert_eq!(
            normalize_scope("usage:read offline_access"),
            "usage:read offline_access"
        );
    }

    #[test]
    fn consent_issues_only_the_selected_permissions() {
        let requested = "usage:read requests:read keys:read payloads:read offline_access";
        let all: Vec<String> = requested.split(' ').map(ToOwned::to_owned).collect();
        assert_eq!(
            granted_scope(requested, &all, "superuser").as_deref(),
            Some(requested)
        );
        assert_eq!(
            granted_scope(requested, &["usage:read".to_owned()], "user").as_deref(),
            Some("usage:read offline_access")
        );
        assert_eq!(
            granted_scope(requested, &["offline_access".to_owned()], "user").as_deref(),
            Some("offline_access")
        );
        assert_eq!(
            granted_scope(requested, &[], "user").as_deref(),
            Some("offline_access")
        );
        assert_eq!(
            granted_scope(requested, &["payloads:read".to_owned()], "superuser").as_deref(),
            Some("payloads:read offline_access")
        );
        assert_eq!(
            granted_scope(
                requested,
                &[
                    "usage:read".to_owned(),
                    "payloads:read".to_owned(),
                    "keys:read".to_owned()
                ],
                "user"
            )
            .as_deref(),
            Some("usage:read keys:read offline_access")
        );
        assert_eq!(
            granted_scope(
                "usage:read offline_access",
                &["usage:read".to_owned(), "keys:read".to_owned()],
                "superuser"
            )
            .as_deref(),
            Some("usage:read offline_access")
        );
    }

    #[test]
    fn a_grant_holds_offline_access_only_when_the_request_asks_for_it() {
        assert_eq!(
            granted_scope(
                "usage:read",
                &["usage:read".to_owned(), "offline_access".to_owned()],
                "user"
            )
            .as_deref(),
            Some("usage:read")
        );
        assert_eq!(
            granted_scope(
                "usage:read offline_access",
                &["usage:read".to_owned()],
                "user"
            )
            .as_deref(),
            Some("usage:read offline_access")
        );
    }

    #[test]
    fn each_resource_scope_can_be_granted_independently() {
        let requested = "usage:read requests:read";
        assert_eq!(
            granted_scope(requested, &["usage:read".to_owned()], "user").as_deref(),
            Some("usage:read")
        );
        assert_eq!(
            granted_scope(
                requested,
                &["usage:read".to_owned(), "requests:read".to_owned()],
                "user"
            )
            .as_deref(),
            Some("usage:read requests:read")
        );
        assert_eq!(
            granted_scope(requested, &["requests:read".to_owned()], "user").as_deref(),
            Some("requests:read")
        );
        assert_eq!(
            granted_scope("usage:read", &["requests:read".to_owned()], "user"),
            None
        );
    }
}
