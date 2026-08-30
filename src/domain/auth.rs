use std::{sync::Arc, time::Duration};

use argon2::password_hash::phc::PasswordHash;
use argon2::{
    Algorithm, Argon2, Params, Version,
    password_hash::{PasswordHasher, PasswordVerifier},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use rand::Rng;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait,
    QueryFilter, QueryOrder, Set, Statement, TransactionTrait, sea_query::Expr,
};
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use super::{
    Clock,
    entity::{auth_sessions, password_credentials, users},
    error::DomainError,
    secrets::{CsrfToken, Password, SessionToken, hmac_digest},
};

const PREFIX: &str = "ag_s1";
const SESSION_DOMAIN: &[u8] = b"aegis/browser-session/v1\0";
const CSRF_DOMAIN: &[u8] = b"aegis/browser-csrf/v1\0";

#[derive(Clone)]
pub struct AuthConfig {
    pub session_hmac_key: [u8; 32],
    pub session_key_id: String,
    pub password_pepper: Vec<u8>,
    pub pepper_key_id: String,
    pub password_concurrency: usize,
    pub idle_lifetime: Duration,
    pub absolute_lifetime: Duration,
}
impl Drop for AuthConfig {
    fn drop(&mut self) {
        self.session_hmac_key.zeroize();
        self.password_pepper.zeroize();
    }
}

#[derive(Clone)]
pub struct AuthService {
    db: DatabaseConnection,
    config: Arc<AuthConfig>,
    workers: Arc<Semaphore>,
    dummy_hash: Arc<String>,
    clock: Arc<dyn Clock>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub(crate) user_id: Uuid,
    pub(crate) role: String,
    pub(crate) auth_version: i64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthPrincipal {
    pub(crate) client_id: String,
    pub(crate) issuer: String,
    pub(crate) resource: String,
    pub(crate) scope: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    pub(crate) identity: Identity,
    pub(crate) transport: PrincipalTransport,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PrincipalTransport {
    BrowserSession {
        session_id: Uuid,
    },
    OAuthAccessToken {
        token_id: Uuid,
        context: OAuthPrincipal,
    },
}
impl Principal {
    pub fn user_id(&self) -> Uuid {
        self.identity.user_id
    }
    pub fn role(&self) -> &str {
        &self.identity.role
    }
    pub fn session_id(&self) -> Option<Uuid> {
        match &self.transport {
            PrincipalTransport::BrowserSession { session_id } => Some(*session_id),
            PrincipalTransport::OAuthAccessToken { .. } => None,
        }
    }
    #[cfg(test)]
    pub fn oauth(&self) -> Option<&OAuthPrincipal> {
        match &self.transport {
            PrincipalTransport::OAuthAccessToken { context, .. } => Some(context),
            PrincipalTransport::BrowserSession { .. } => None,
        }
    }
}

pub struct IssuedSession {
    pub(crate) token: SessionToken,
    pub(crate) csrf: CsrfToken,
}
impl IssuedSession {
    pub fn token(&self) -> &str {
        self.token.expose()
    }
    pub fn csrf(&self) -> &str {
        self.csrf.expose()
    }
}

pub fn normalize_email(input: &str) -> Result<(String, String), DomainError> {
    let display = input.trim();
    if display.is_empty()
        || display.len() > 254
        || !display.is_ascii()
        || display
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(DomainError::InvalidInput("invalid email"));
    }
    if display.parse::<email_address::EmailAddress>().is_err() {
        return Err(DomainError::InvalidInput("invalid email"));
    }
    let domain = display
        .rsplit_once('@')
        .map(|(_, domain)| domain)
        .ok_or(DomainError::InvalidInput("invalid email"))?;
    if !domain.contains('.')
        || domain.split('.').any(|label| {
            label.is_empty()
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return Err(DomainError::InvalidInput("invalid email"));
    }
    Ok((display.to_ascii_lowercase(), display.to_owned()))
}

fn argon(pepper: &[u8]) -> Argon2<'_> {
    Argon2::new_with_secret(
        pepper,
        Algorithm::Argon2id,
        Version::V0x13,
        Params::new(19_456, 2, 1, None).expect("valid Argon2 parameters"),
    )
    .expect("validated pepper length")
}

fn has_expected_argon_parameters(hash: &PasswordHash) -> bool {
    hash.algorithm.as_str() == "argon2id"
        && hash.version == Some(19)
        && hash.params.iter().count() == 3
        && hash.params.get_decimal("m") == Some(19_456)
        && hash.params.get_decimal("t") == Some(2)
        && hash.params.get_decimal("p") == Some(1)
        && hash.salt.is_some()
        && hash.hash.as_ref().is_some_and(|output| output.len() == 32)
}

impl AuthService {
    pub fn absolute_lifetime(&self) -> Duration {
        self.config.absolute_lifetime
    }

    pub async fn new(
        db: DatabaseConnection,
        config: AuthConfig,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, DomainError> {
        if config.password_concurrency == 0 {
            return Err(DomainError::InvalidInput(
                "password concurrency must be positive",
            ));
        }
        let pepper = Zeroizing::new(config.password_pepper.clone());
        let dummy_hash = tokio::task::spawn_blocking(move || {
            argon(&pepper)
                .hash_password(b"fixed-invalid-password")
                .map(|v| v.to_string())
        })
        .await
        .map_err(|_| DomainError::TemporarilyUnavailable)?
        .map_err(|_| DomainError::TemporarilyUnavailable)?;
        Ok(Self {
            db,
            workers: Arc::new(Semaphore::new(config.password_concurrency)),
            config: Arc::new(config),
            dummy_hash: Arc::new(dummy_hash),
            clock,
        })
    }

    async fn verify_hash(&self, password: &Password, hash: String) -> Result<bool, DomainError> {
        let permit = self
            .workers
            .clone()
            .try_acquire_owned()
            .map_err(|_| DomainError::TemporarilyUnavailable)?;
        let password = Zeroizing::new(password.bytes().to_vec());
        let pepper = Zeroizing::new(self.config.password_pepper.clone());
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            PasswordHash::new(&hash).ok().is_some_and(|parsed| {
                has_expected_argon_parameters(&parsed)
                    && argon(&pepper).verify_password(&password, &parsed).is_ok()
            })
        })
        .await
        .map_err(|_| DomainError::TemporarilyUnavailable)
    }

    pub async fn verify_password_identity(
        &self,
        email: &str,
        password: &Password,
    ) -> Result<Identity, DomainError> {
        let normalized = match normalize_email(email) {
            Ok(v) => v.0,
            Err(_) => String::new(),
        };
        let row = users::Entity::find()
            .filter(users::Column::EmailNormalized.eq(normalized))
            .find_also_related(password_credentials::Entity)
            .one(&self.db)
            .await?;
        let usable = row.and_then(|(user, credential)| credential.map(|c| (user, c)));
        let hash = usable
            .as_ref()
            .filter(|(user, credential)| {
                user.status == "active"
                    && credential.pepper_key_id == self.config.pepper_key_id
                    && PasswordHash::new(&credential.password_hash)
                        .is_ok_and(|hash| has_expected_argon_parameters(&hash))
            })
            .map_or_else(
                || (*self.dummy_hash).clone(),
                |(_, credential)| credential.password_hash.clone(),
            );
        let valid = self.verify_hash(password, hash).await?;
        let Some((user, credential)) = usable else {
            return Err(DomainError::InvalidCredentials);
        };
        if !valid
            || user.status != "active"
            || credential.pepper_key_id != self.config.pepper_key_id
            || !PasswordHash::new(&credential.password_hash)
                .is_ok_and(|hash| has_expected_argon_parameters(&hash))
        {
            return Err(DomainError::InvalidCredentials);
        }
        let user_id = Uuid::parse_str(&user.id).map_err(|_| DomainError::InvalidCredentials)?;
        Ok(Identity {
            user_id,
            role: user.role,
            auth_version: user.auth_version,
        })
    }

    pub async fn login(
        &self,
        email: &str,
        password: &Password,
        user_agent: Option<&str>,
    ) -> Result<IssuedSession, DomainError> {
        let identity = self.verify_password_identity(email, password).await?;
        self.issue(identity.user_id, identity.auth_version, user_agent)
            .await
    }

    async fn issue(
        &self,
        user_id: Uuid,
        auth_version: i64,
        user_agent: Option<&str>,
    ) -> Result<IssuedSession, DomainError> {
        let mut selector = [0u8; 16];
        let mut secret = [0u8; 32];
        let mut csrf = [0u8; 32];
        rand::rng().fill_bytes(&mut selector);
        rand::rng().fill_bytes(&mut secret);
        rand::rng().fill_bytes(&mut csrf);
        let id = Uuid::from_bytes(selector);
        let secret_digest = hmac_digest(
            &self.config.session_hmac_key,
            SESSION_DOMAIN,
            &selector,
            &secret,
        );
        let csrf_digest = hmac_digest(&self.config.session_hmac_key, CSRF_DOMAIN, &selector, &csrf);
        let now = self.clock.now();
        let idle = now
            + chrono::Duration::from_std(self.config.idle_lifetime)
                .map_err(|_| DomainError::InvalidInput("invalid lifetime"))?;
        let absolute = now
            + chrono::Duration::from_std(self.config.absolute_lifetime)
                .map_err(|_| DomainError::InvalidInput("invalid lifetime"))?;
        auth_sessions::ActiveModel {
            id: Set(id.to_string()),
            user_id: Set(user_id.to_string()),
            transport: Set("browser_cookie".to_owned()),
            secret_digest: Set(secret_digest.to_vec()),
            csrf_digest: Set(Some(csrf_digest.to_vec())),
            key_id: Set(self.config.session_key_id.clone()),
            auth_version: Set(auth_version),
            authenticated_at: Set(now.to_rfc3339()),
            created_at: Set(now.to_rfc3339()),
            last_seen_at: Set(now.to_rfc3339()),
            idle_expires_at: Set(idle.to_rfc3339()),
            absolute_expires_at: Set(absolute.to_rfc3339()),
            revoked_at: Set(None),
            revocation_reason: Set(None),
            user_agent: Set(user_agent.and_then(sanitize_user_agent)),
        }
        .insert(&self.db)
        .await?;
        Ok(IssuedSession {
            token: SessionToken(format!(
                "{PREFIX}.{}.{}",
                URL_SAFE_NO_PAD.encode(selector),
                URL_SAFE_NO_PAD.encode(secret)
            )),
            csrf: CsrfToken(URL_SAFE_NO_PAD.encode(csrf)),
        })
    }

    pub async fn rotate_csrf(&self, token: &str) -> Result<CsrfToken, DomainError> {
        let principal = self.authenticate(token, None).await?;
        let PrincipalTransport::BrowserSession { session_id } = principal.transport else {
            return Err(DomainError::InvalidCredentials);
        };
        let (_, selector, _) = parse_token(token)?;
        let mut csrf = [0u8; 32];
        rand::rng().fill_bytes(&mut csrf);
        let digest = hmac_digest(&self.config.session_hmac_key, CSRF_DOMAIN, &selector, &csrf);
        let updated = auth_sessions::Entity::update_many()
            .col_expr(
                auth_sessions::Column::CsrfDigest,
                Expr::value(digest.to_vec()),
            )
            .filter(auth_sessions::Column::Id.eq(session_id.to_string()))
            .filter(auth_sessions::Column::RevokedAt.is_null())
            .exec(&self.db)
            .await?;
        if updated.rows_affected == 0 {
            return Err(DomainError::InvalidCredentials);
        }
        Ok(CsrfToken(URL_SAFE_NO_PAD.encode(csrf)))
    }

    pub async fn authenticate(
        &self,
        token: &str,
        csrf: Option<&str>,
    ) -> Result<Principal, DomainError> {
        let (id, selector, secret) = parse_token(token)?;
        let row = auth_sessions::Entity::find_by_id(id.to_string())
            .filter(auth_sessions::Column::Transport.eq("browser_cookie"))
            .find_also_related(users::Entity)
            .one(&self.db)
            .await?;
        let Some((session, Some(user))) = row else {
            return Err(DomainError::InvalidCredentials);
        };
        let stored = session.secret_digest.clone();
        let expected = hmac_digest(
            &self.config.session_hmac_key,
            SESSION_DOMAIN,
            &selector,
            &secret,
        );
        let now = self.clock.now();
        let idle = parse_date(&session.idle_expires_at)?;
        let absolute = parse_date(&session.absolute_expires_at)?;
        let session_version = session.auth_version;
        let user_version = user.auth_version;
        if stored.len() != 32
            || expected.ct_eq(stored.as_slice()).unwrap_u8() != 1
            || session.key_id != self.config.session_key_id
            || session.revoked_at.is_some()
            || user.status != "active"
            || session_version != user_version
            || now >= idle
            || now >= absolute
        {
            return Err(DomainError::InvalidCredentials);
        }
        if let Some(csrf_value) = csrf {
            if csrf_value.contains('=') || csrf_value.len() > 64 {
                return Err(DomainError::InvalidCredentials);
            }
            let decoded = URL_SAFE_NO_PAD
                .decode(csrf_value)
                .map_err(|_| DomainError::InvalidCredentials)?;
            let stored_csrf = session
                .csrf_digest
                .clone()
                .ok_or(DomainError::InvalidCredentials)?;
            let actual = hmac_digest(
                &self.config.session_hmac_key,
                CSRF_DOMAIN,
                &selector,
                &decoded,
            );
            if decoded.len() != 32
                || stored_csrf.len() != 32
                || actual.ct_eq(stored_csrf.as_slice()).unwrap_u8() != 1
            {
                return Err(DomainError::InvalidCredentials);
            }
        }
        let user_id =
            Uuid::parse_str(&session.user_id).map_err(|_| DomainError::InvalidCredentials)?;
        let last_seen = parse_date(&session.last_seen_at)?;
        let touch_interval = chrono::Duration::seconds(
            (self.config.idle_lifetime.as_secs() / 4).clamp(1, 300) as i64,
        );
        if now - last_seen >= touch_interval {
            let renewed_idle = std::cmp::min(
                now + chrono::Duration::from_std(self.config.idle_lifetime)
                    .map_err(|_| DomainError::InvalidCredentials)?,
                absolute,
            );
            auth_sessions::Entity::update_many()
                .col_expr(
                    auth_sessions::Column::LastSeenAt,
                    Expr::value(now.to_rfc3339()),
                )
                .col_expr(
                    auth_sessions::Column::IdleExpiresAt,
                    Expr::value(renewed_idle.to_rfc3339()),
                )
                .filter(auth_sessions::Column::Id.eq(id.to_string()))
                .filter(auth_sessions::Column::UserId.eq(user_id.to_string()))
                .filter(auth_sessions::Column::RevokedAt.is_null())
                .exec(&self.db)
                .await?;
        }
        Ok(Principal {
            identity: Identity {
                user_id,
                role: user.role,
                auth_version: user_version,
            },
            transport: PrincipalTransport::BrowserSession { session_id: id },
        })
    }

    pub async fn logout(&self, principal: &Principal) -> Result<(), DomainError> {
        let PrincipalTransport::BrowserSession { session_id } = &principal.transport else {
            return Err(DomainError::InvalidCredentials);
        };
        auth_sessions::Entity::update_many()
            .col_expr(
                auth_sessions::Column::RevokedAt,
                Expr::value(self.clock.now().to_rfc3339()),
            )
            .col_expr(
                auth_sessions::Column::RevocationReason,
                Expr::value("logout"),
            )
            .filter(auth_sessions::Column::Id.eq(session_id.to_string()))
            .filter(auth_sessions::Column::UserId.eq(principal.user_id().to_string()))
            .filter(auth_sessions::Column::RevokedAt.is_null())
            .exec(&self.db)
            .await?;
        Ok(())
    }

    pub async fn account_email(&self, principal: &Principal) -> Result<String, DomainError> {
        let user = users::Entity::find_by_id(principal.user_id().to_string())
            .one(&self.db)
            .await?
            .ok_or(DomainError::InvalidCredentials)?;
        Ok(user.email_display)
    }

    pub async fn list_sessions(
        &self,
        principal: &Principal,
    ) -> Result<Vec<SessionSummary>, DomainError> {
        let current = principal.session_id();
        let now = self.clock.now();
        let rows = auth_sessions::Entity::find()
            .filter(auth_sessions::Column::UserId.eq(principal.user_id().to_string()))
            .filter(auth_sessions::Column::Transport.eq("browser_cookie"))
            .filter(auth_sessions::Column::RevokedAt.is_null())
            .order_by_desc(auth_sessions::Column::CreatedAt)
            .all(&self.db)
            .await?;
        let mut sessions = Vec::new();
        for row in rows {
            let (Ok(id), Ok(created_at), Ok(last_seen_at), Ok(idle), Ok(absolute)) = (
                Uuid::parse_str(&row.id),
                parse_date(&row.created_at),
                parse_date(&row.last_seen_at),
                parse_date(&row.idle_expires_at),
                parse_date(&row.absolute_expires_at),
            ) else {
                continue;
            };
            if row.auth_version != principal.identity.auth_version || now >= idle || now >= absolute
            {
                continue;
            }
            sessions.push(SessionSummary {
                id,
                created_at,
                last_seen_at,
                current: current == Some(id),
                user_agent: row.user_agent,
            });
        }
        Ok(sessions)
    }

    pub async fn revoke_session(
        &self,
        principal: &Principal,
        session_id: Uuid,
    ) -> Result<(), DomainError> {
        let result = auth_sessions::Entity::update_many()
            .col_expr(
                auth_sessions::Column::RevokedAt,
                Expr::value(self.clock.now().to_rfc3339()),
            )
            .col_expr(
                auth_sessions::Column::RevocationReason,
                Expr::value("user_revoked"),
            )
            .filter(auth_sessions::Column::Id.eq(session_id.to_string()))
            .filter(auth_sessions::Column::UserId.eq(principal.user_id().to_string()))
            .filter(auth_sessions::Column::RevokedAt.is_null())
            .exec(&self.db)
            .await?;
        if result.rows_affected == 0 {
            return Err(DomainError::NotFound);
        }
        Ok(())
    }

    pub async fn verify_user_password(
        &self,
        user_id: Uuid,
        password: &Password,
    ) -> Result<(), DomainError> {
        let row = users::Entity::find_by_id(user_id.to_string())
            .find_also_related(password_credentials::Entity)
            .one(&self.db)
            .await?;
        let Some((user, Some(credential))) = row else {
            return Err(DomainError::InvalidCredentials);
        };
        if user.status != "active"
            || credential.pepper_key_id != self.config.pepper_key_id
            || !PasswordHash::new(&credential.password_hash)
                .is_ok_and(|hash| has_expected_argon_parameters(&hash))
        {
            return Err(DomainError::InvalidCredentials);
        }
        if !self.verify_hash(password, credential.password_hash).await? {
            return Err(DomainError::InvalidCredentials);
        }
        Ok(())
    }
}

pub struct SessionSummary {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub current: bool,
    pub user_agent: Option<String>,
}

const USER_AGENT_LIMIT: usize = 256;

fn sanitize_user_agent(raw: &str) -> Option<String> {
    let mut value = String::new();
    for character in raw.chars().filter(|c| !c.is_control()) {
        if value.len() + character.len_utf8() > USER_AGENT_LIMIT {
            break;
        }
        value.push(character);
    }
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_owned())
}

pub async fn bootstrap_superuser(
    db: &DatabaseConnection,
    password_pepper: &[u8],
    pepper_key_id: &str,
    email: &str,
    password: &Password,
) -> Result<Uuid, DomainError> {
    super::account::check_password_policy(password)?;
    let (normalized, display) = normalize_email(email)?;
    let hash = hash_password(password_pepper, password).await?;
    let now = Utc::now().to_rfc3339();
    let id = Uuid::now_v7();
    let tx = db.begin().await?;
    let result = tx.execute_raw(Statement::from_sql_and_values(DbBackend::Sqlite,
        "INSERT INTO users(id,email_normalized,email_display,role,status,auth_version,email_verified_at,created_at,updated_at) SELECT ?,?,?,'superuser','active',0,?,?,? WHERE NOT EXISTS (SELECT 1 FROM users WHERE role='superuser' AND status='active')",
        vec![id.to_string().into(), normalized.into(), display.into(), now.clone().into(), now.clone().into(), now.clone().into()])).await?;
    if result.rows_affected() != 1 {
        tx.rollback().await?;
        return Err(DomainError::Conflict);
    }
    let credential = password_credentials::ActiveModel {
        user_id: Set(id.to_string()),
        password_hash: Set(hash),
        pepper_key_id: Set(pepper_key_id.to_owned()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
    };
    if let Err(error) = credential.insert(&tx).await {
        tx.rollback().await?;
        return Err(DomainError::Internal(error));
    }
    tx.commit().await?;
    Ok(id)
}

pub(super) async fn hash_password(
    pepper: &[u8],
    password: &Password,
) -> Result<String, DomainError> {
    let password = Zeroizing::new(password.bytes().to_vec());
    let pepper = Zeroizing::new(pepper.to_vec());
    tokio::task::spawn_blocking(move || {
        argon(&pepper)
            .hash_password(&password)
            .map(|v| v.to_string())
    })
    .await
    .map_err(|_| DomainError::TemporarilyUnavailable)?
    .map_err(|_| DomainError::TemporarilyUnavailable)
}

fn parse_date(value: &str) -> Result<DateTime<Utc>, DomainError> {
    DateTime::parse_from_rfc3339(value)
        .map(|v| v.with_timezone(&Utc))
        .map_err(|_| DomainError::InvalidCredentials)
}
fn parse_token(value: &str) -> Result<(Uuid, [u8; 16], [u8; 32]), DomainError> {
    if value.len() > 128 || value.contains('=') {
        return Err(DomainError::InvalidCredentials);
    }
    let mut parts = value.split('.');
    if parts.next() != Some(PREFIX) {
        return Err(DomainError::InvalidCredentials);
    }
    let a = parts.next().ok_or(DomainError::InvalidCredentials)?;
    let b = parts.next().ok_or(DomainError::InvalidCredentials)?;
    if parts.next().is_some() {
        return Err(DomainError::InvalidCredentials);
    }
    let selector: [u8; 16] = URL_SAFE_NO_PAD
        .decode(a)
        .map_err(|_| DomainError::InvalidCredentials)?
        .try_into()
        .map_err(|_| DomainError::InvalidCredentials)?;
    let secret: [u8; 32] = URL_SAFE_NO_PAD
        .decode(b)
        .map_err(|_| DomainError::InvalidCredentials)?
        .try_into()
        .map_err(|_| DomainError::InvalidCredentials)?;
    Ok((Uuid::from_bytes(selector), selector, secret))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use chrono::TimeZone;
    use sea_orm::Database;
    use sea_orm_migration::MigratorTrait;

    struct FixedClock(DateTime<Utc>);
    impl Clock for FixedClock {
        fn now(&self) -> DateTime<Utc> {
            self.0
        }
    }

    fn config() -> AuthConfig {
        AuthConfig {
            session_hmac_key: [7; 32],
            session_key_id: "session-1".to_owned(),
            password_pepper: b"test pepper".to_vec(),
            pepper_key_id: "pepper-1".to_owned(),
            password_concurrency: 2,
            idle_lifetime: Duration::from_secs(60),
            absolute_lifetime: Duration::from_secs(120),
        }
    }

    async fn service() -> (DatabaseConnection, AuthService) {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        db.execute_unprepared("PRAGMA foreign_keys=ON")
            .await
            .unwrap();
        Migrator::up(&db, None).await.unwrap();
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 8, 15, 12, 0, 0).unwrap(),
        ));
        let auth = AuthService::new(db.clone(), config(), clock).await.unwrap();
        (db, auth)
    }

    #[test]
    fn email_rules() {
        assert_eq!(normalize_email(" A@B.COM ").unwrap().0, "a@b.com");
        assert!(normalize_email("é@x").is_err());
        assert!(normalize_email("not-an-email").is_err());
        assert!(normalize_email("a@@example.com").is_err());
        assert!(normalize_email("a..b@example.com").is_err());
        assert!(normalize_email("a@-example.com").is_err());
        assert!(normalize_email("a@example_com").is_err());
        assert!(normalize_email("a@example/com").is_err());
        assert!(normalize_email(&"a".repeat(255)).is_err());
    }
    #[test]
    fn rejects_argon_hashes_with_unexpected_work_factors() {
        let valid = argon(b"test pepper")
            .hash_password(b"password")
            .unwrap()
            .to_string();
        let parsed = PasswordHash::new(&valid).unwrap();
        assert!(has_expected_argon_parameters(&parsed));

        let expensive = valid.replacen("m=19456", "m=1048576", 1);
        let parsed = PasswordHash::new(&expensive).unwrap();
        assert!(!has_expected_argon_parameters(&parsed));
    }

    #[test]
    fn token_rejects_bad_shapes() {
        for s in [
            "",
            "ag_s1.a.b",
            "ag_s1.aaaa.bbbb=",
            "ag_s2.a.b",
            "ag_s1.a.b.extra",
        ] {
            assert!(parse_token(s).is_err());
        }
    }

    #[tokio::test]
    async fn password_bootstrap_and_opaque_session_lifecycle() {
        let (db, auth) = service().await;
        let password = Password::new("  c0rrect horse battery staple!  ".to_owned()).unwrap();
        let trimmed = Password::new("c0rrect horse battery staple!".to_owned()).unwrap();
        let short = Password::new("too short".to_owned()).unwrap();

        assert!(matches!(
            bootstrap_superuser(&db, b"test pepper", "pepper-1", "admin@example.com", &short).await,
            Err(DomainError::InvalidInput(_))
        ));
        let first_hash = hash_password(b"test pepper", &password).await.unwrap();
        let second_hash = hash_password(b"test pepper", &password).await.unwrap();
        assert_ne!(first_hash, second_hash, "every hash needs a fresh salt");
        assert!(auth.verify_hash(&password, first_hash).await.unwrap());
        assert!(!auth.verify_hash(&trimmed, second_hash).await.unwrap());

        let user_id = bootstrap_superuser(
            &db,
            b"test pepper",
            "pepper-1",
            " Admin@Example.COM ",
            &password,
        )
        .await
        .unwrap();
        assert!(matches!(
            bootstrap_superuser(
                &db,
                b"test pepper",
                "pepper-1",
                "other@example.com",
                &password
            )
            .await,
            Err(DomainError::Conflict)
        ));
        let identity = auth
            .verify_password_identity("admin@example.com", &password)
            .await
            .unwrap();
        assert_eq!(identity.user_id, user_id);
        assert_eq!(identity.role, "superuser");
        let session_count: i64 = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT count(*) AS n FROM auth_sessions",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "n")
            .unwrap();
        assert_eq!(
            session_count, 0,
            "identity verification must not issue a session"
        );

        assert!(matches!(
            auth.login("missing@example.com", &password, None).await,
            Err(DomainError::InvalidCredentials)
        ));
        assert!(matches!(
            auth.login("admin@example.com", &trimmed, None).await,
            Err(DomainError::InvalidCredentials)
        ));

        let issued = auth
            .login("admin@example.com", &password, None)
            .await
            .unwrap();
        assert!(!issued.token().contains(&user_id.to_string()));
        let principal = auth
            .authenticate(issued.token(), Some(issued.csrf()))
            .await
            .unwrap();
        assert_eq!(principal.user_id(), user_id);

        db.execute_unprepared(
            "UPDATE auth_sessions
             SET last_seen_at='2026-08-15T11:59:30Z',
                 idle_expires_at='2026-08-15T12:00:30Z'",
        )
        .await
        .unwrap();
        auth.authenticate(issued.token(), None).await.unwrap();
        let renewed_idle: String = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT idle_expires_at FROM auth_sessions",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "idle_expires_at")
            .unwrap();
        assert_eq!(
            parse_date(&renewed_idle).unwrap(),
            Utc.with_ymd_and_hms(2026, 8, 15, 12, 1, 0).unwrap()
        );

        assert!(matches!(
            auth.authenticate(issued.token(), Some("wrong")).await,
            Err(DomainError::InvalidCredentials)
        ));

        let row = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT secret_digest, csrf_digest, key_id FROM auth_sessions",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.try_get::<Vec<u8>>("", "secret_digest").unwrap().len(),
            32
        );
        assert_eq!(row.try_get::<Vec<u8>>("", "csrf_digest").unwrap().len(), 32);
        assert_eq!(row.try_get::<String>("", "key_id").unwrap(), "session-1");

        db.execute_unprepared("UPDATE users SET status='disabled'")
            .await
            .unwrap();
        assert!(matches!(
            auth.authenticate(issued.token(), None).await,
            Err(DomainError::InvalidCredentials)
        ));
        db.execute_unprepared("UPDATE users SET status='active', auth_version=1")
            .await
            .unwrap();
        assert!(matches!(
            auth.authenticate(issued.token(), None).await,
            Err(DomainError::InvalidCredentials)
        ));
        db.execute_unprepared("UPDATE users SET auth_version=0; UPDATE auth_sessions SET idle_expires_at='2020-01-01T00:00:00Z'")
            .await
            .unwrap();
        assert!(matches!(
            auth.authenticate(issued.token(), None).await,
            Err(DomainError::InvalidCredentials)
        ));
        db.execute_unprepared("UPDATE auth_sessions SET idle_expires_at='2026-08-15T12:01:00Z'")
            .await
            .unwrap();

        auth.logout(&principal).await.unwrap();
        assert!(matches!(
            auth.authenticate(issued.token(), None).await,
            Err(DomainError::InvalidCredentials)
        ));
    }

    #[test]
    fn user_agent_is_trimmed_stripped_and_capped() {
        assert_eq!(sanitize_user_agent(""), None);
        assert_eq!(sanitize_user_agent("  \t\n "), None);
        assert_eq!(
            sanitize_user_agent("curl/8.9.1\u{0}\n"),
            Some("curl/8.9.1".to_owned())
        );
        let long = sanitize_user_agent(&"a".repeat(1000)).unwrap();
        assert_eq!(long.len(), USER_AGENT_LIMIT);
        let wide = sanitize_user_agent(&"é".repeat(1000)).unwrap();
        assert!(wide.len() <= USER_AGENT_LIMIT && wide.chars().all(|c| c == 'é'));
    }

    #[tokio::test]
    async fn login_stores_the_capped_user_agent() {
        let (db, auth) = service().await;
        let password = Password::new("passw0rd long enough!".to_owned()).unwrap();
        bootstrap_superuser(&db, b"test pepper", "pepper-1", "a@example.com", &password)
            .await
            .unwrap();
        let agent = format!("Firefox/{}", "9".repeat(1000));
        auth.login("a@example.com", &password, Some(&agent))
            .await
            .unwrap();
        let issued = auth.login("a@example.com", &password, None).await.unwrap();
        let principal = auth.authenticate(issued.token(), None).await.unwrap();
        let sessions = auth.list_sessions(&principal).await.unwrap();
        let mut agents: Vec<Option<usize>> = sessions
            .iter()
            .map(|session| session.user_agent.as_ref().map(|value| value.len()))
            .collect();
        agents.sort();
        assert_eq!(agents, vec![None, Some(USER_AGENT_LIMIT)]);
        let stored = sessions
            .iter()
            .find_map(|session| session.user_agent.clone())
            .unwrap();
        assert!(stored.starts_with("Firefox/9"));
    }

    #[tokio::test]
    async fn bootstrap_rolls_back_when_credential_insert_fails() {
        let (db, _auth) = service().await;
        db.execute_unprepared(
            "CREATE TRIGGER reject_credentials BEFORE INSERT ON password_credentials BEGIN SELECT RAISE(FAIL, 'test'); END",
        )
        .await
        .unwrap();
        let password = Password::new("passw0rd long enough!".to_owned()).unwrap();
        assert!(matches!(
            bootstrap_superuser(
                &db,
                b"test pepper",
                "pepper-1",
                "admin@example.com",
                &password
            )
            .await,
            Err(DomainError::Internal(_))
        ));
        let count: i64 = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT count(*) AS n FROM users",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "n")
            .unwrap();
        assert_eq!(count, 0);
    }
}
