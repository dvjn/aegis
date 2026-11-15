use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use rand::RngExt;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, DatabaseTransaction, EntityTrait,
    QueryFilter, Set, sea_query::Expr,
};
use subtle::ConstantTimeEq;
use uuid::Uuid;
use zeroize::Zeroize;

use super::{
    Clock,
    auth::{hash_password, normalize_email},
    entity::{auth_one_time_tokens, auth_sessions, password_credentials, users},
    error::DomainError,
    mailer::{Mail, Mailer},
    secrets::{Password, hmac_digest},
};

const CODE_DOMAIN: &[u8] = b"aegis/one-time-code/v1\0";
const MAX_CODE_ATTEMPTS: i32 = 5;

const VERIFY_EMAIL: &str = "verify_email";
const RESET_PASSWORD: &str = "reset_password";
const VERIFY_EMAIL_LIFETIME: Duration = Duration::hours(24);
const RESET_PASSWORD_LIFETIME: Duration = Duration::minutes(30);
const VERIFY_EMAIL_LIFETIME_TEXT: &str = "24 hours";
const RESET_PASSWORD_LIFETIME_TEXT: &str = "30 minutes";

const MIN_PASSWORD_CHARACTERS: usize = 8;

pub fn check_password_policy(password: &Password) -> Result<(), DomainError> {
    let value = std::str::from_utf8(password.bytes()).expect("Password is constructed from UTF-8");
    if value.chars().count() < MIN_PASSWORD_CHARACTERS {
        return Err(DomainError::InvalidInput(
            "new passwords must contain at least 8 characters, 1 letter, 1 digit, and 1 special character",
        ));
    }
    let letter = value.chars().any(char::is_alphabetic);
    let digit = value.chars().any(|item| item.is_ascii_digit());
    let special = value
        .chars()
        .any(|item| !item.is_alphanumeric() && !item.is_whitespace());
    if !letter || !digit || !special {
        return Err(DomainError::InvalidInput(
            "new passwords must contain at least 8 characters, 1 letter, 1 digit, and 1 special character",
        ));
    }
    Ok(())
}

#[derive(Clone)]
pub struct AccountConfig {
    pub hmac_key: [u8; 32],
    pub key_id: String,
    pub password_pepper: Vec<u8>,
    pub pepper_key_id: String,
    pub registration_enabled: bool,
}
impl Drop for AccountConfig {
    fn drop(&mut self) {
        self.hmac_key.zeroize();
        self.password_pepper.zeroize();
    }
}

#[derive(Clone)]
pub struct AccountService {
    db: DatabaseConnection,
    config: Arc<AccountConfig>,
    mailer: Arc<dyn Mailer>,
    clock: Arc<dyn Clock>,
}

struct IssuedCode {
    value: String,
}
impl Drop for IssuedCode {
    fn drop(&mut self) {
        self.value.zeroize();
    }
}

impl AccountService {
    pub fn new(
        db: DatabaseConnection,
        config: AccountConfig,
        mailer: Arc<dyn Mailer>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, DomainError> {
        if config.key_id.is_empty() || config.key_id.len() > 64 {
            return Err(DomainError::InvalidInput("invalid account configuration"));
        }
        Ok(Self {
            db,
            config: Arc::new(config),
            mailer,
            clock,
        })
    }

    pub fn registration_enabled(&self) -> bool {
        self.config.registration_enabled
    }

    async fn begin_immediate(&self) -> Result<crate::db::WriteTransaction, DomainError> {
        crate::db::begin_immediate(&self.db).await.map_err(|error| {
            tracing::debug!(%error, "could not start immediate transaction");
            DomainError::TemporarilyUnavailable
        })
    }

    pub async fn register(&self, email: &str, password: &Password) -> Result<(), DomainError> {
        if !self.config.registration_enabled {
            return Err(DomainError::Forbidden);
        }
        check_password_policy(password)?;
        let (normalized, display) = normalize_email(email)?;
        let hash = hash_password(&self.config.password_pepper, password).await?;
        let now = self.clock.now();
        let user_id = Uuid::now_v7();

        let tx = self.begin_immediate().await?;
        let existing = users::Entity::find()
            .filter(users::Column::EmailNormalized.eq(normalized.clone()))
            .one(&tx)
            .await?;
        if existing.is_some() {
            tx.commit().await?;
            return Ok(());
        }
        users::ActiveModel {
            id: Set(user_id.to_string()),
            email_normalized: Set(normalized),
            email_display: Set(display.clone()),
            role: Set("user".to_owned()),
            status: Set("pending_verification".to_owned()),
            auth_version: Set(0),
            email_verified_at: Set(None),
            created_at: Set(now.to_rfc3339()),
            updated_at: Set(now.to_rfc3339()),
        }
        .insert(&tx)
        .await?;
        password_credentials::ActiveModel {
            user_id: Set(user_id.to_string()),
            password_hash: Set(hash),
            pepper_key_id: Set(self.config.pepper_key_id.clone()),
            created_at: Set(now.to_rfc3339()),
            updated_at: Set(now.to_rfc3339()),
        }
        .insert(&tx)
        .await?;
        let code = self
            .store_code(&tx, user_id, VERIFY_EMAIL, VERIFY_EMAIL_LIFETIME, now)
            .await?;
        tx.commit().await?;
        self.send_code(&display, VERIFY_EMAIL, &code).await
    }

    pub async fn verify_email(&self, email: &str, code: &str) -> Result<(), DomainError> {
        let tx = self.begin_immediate().await?;
        let user_id = match self.user_for_code(&tx, email).await {
            Ok(user_id) => user_id,
            Err(error) => {
                tx.rollback().await?;
                return Err(error);
            }
        };
        if let Err(error) = self.consume_code(&tx, user_id, code, VERIFY_EMAIL).await {
            tx.commit().await?;
            return Err(error);
        }
        let now = self.clock.now();
        let updated = users::Entity::update_many()
            .col_expr(users::Column::Status, Expr::value("active"))
            .col_expr(
                users::Column::EmailVerifiedAt,
                Expr::value(now.to_rfc3339()),
            )
            .col_expr(users::Column::UpdatedAt, Expr::value(now.to_rfc3339()))
            .filter(users::Column::Id.eq(user_id.to_string()))
            .filter(users::Column::Status.eq("pending_verification"))
            .exec(&tx)
            .await?;
        if updated.rows_affected == 0 {
            tx.rollback().await?;
            return Err(DomainError::InvalidCredentials);
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn request_password_reset(&self, email: &str) -> Result<(), DomainError> {
        let Ok((normalized, _)) = normalize_email(email) else {
            return Ok(());
        };
        let now = self.clock.now();
        let tx = self.begin_immediate().await?;
        let user = users::Entity::find()
            .filter(users::Column::EmailNormalized.eq(normalized))
            .one(&tx)
            .await?;
        let Some(user) = user.filter(|user| user.status == "active") else {
            tx.commit().await?;
            return Ok(());
        };
        let Ok(user_id) = Uuid::parse_str(&user.id) else {
            tx.commit().await?;
            return Ok(());
        };
        let code = self
            .store_code(&tx, user_id, RESET_PASSWORD, RESET_PASSWORD_LIFETIME, now)
            .await?;
        tx.commit().await?;
        self.send_code(&user.email_display, RESET_PASSWORD, &code)
            .await
    }

    pub async fn reset_password(
        &self,
        email: &str,
        code: &str,
        password: &Password,
    ) -> Result<(), DomainError> {
        check_password_policy(password)?;
        let hash = hash_password(&self.config.password_pepper, password).await?;
        let now = self.clock.now();
        let tx = self.begin_immediate().await?;
        let user_id = match self.user_for_code(&tx, email).await {
            Ok(user_id) => user_id,
            Err(error) => {
                tx.rollback().await?;
                return Err(error);
            }
        };
        if let Err(error) = self.consume_code(&tx, user_id, code, RESET_PASSWORD).await {
            tx.commit().await?;
            return Err(error);
        }
        password_credentials::Entity::update_many()
            .col_expr(
                password_credentials::Column::PasswordHash,
                Expr::value(hash),
            )
            .col_expr(
                password_credentials::Column::PepperKeyId,
                Expr::value(self.config.pepper_key_id.clone()),
            )
            .col_expr(
                password_credentials::Column::UpdatedAt,
                Expr::value(now.to_rfc3339()),
            )
            .filter(password_credentials::Column::UserId.eq(user_id.to_string()))
            .exec(&tx)
            .await?;
        users::Entity::update_many()
            .col_expr(users::Column::UpdatedAt, Expr::value(now.to_rfc3339()))
            .filter(users::Column::Id.eq(user_id.to_string()))
            .exec(&tx)
            .await?;
        auth_sessions::Entity::update_many()
            .col_expr(
                auth_sessions::Column::RevokedAt,
                Expr::value(now.to_rfc3339()),
            )
            .col_expr(
                auth_sessions::Column::RevocationReason,
                Expr::value("password_reset"),
            )
            .filter(auth_sessions::Column::UserId.eq(user_id.to_string()))
            .filter(auth_sessions::Column::RevokedAt.is_null())
            .exec(&tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn change_password(
        &self,
        user_id: Uuid,
        keep_session_id: Uuid,
        password: &Password,
    ) -> Result<(), DomainError> {
        check_password_policy(password)?;
        let hash = hash_password(&self.config.password_pepper, password).await?;
        let now = self.clock.now();
        let tx = self.begin_immediate().await?;
        users::Entity::find_by_id(user_id.to_string())
            .one(&tx)
            .await?
            .ok_or(DomainError::NotFound)?;
        password_credentials::Entity::update_many()
            .col_expr(
                password_credentials::Column::PasswordHash,
                Expr::value(hash),
            )
            .col_expr(
                password_credentials::Column::PepperKeyId,
                Expr::value(self.config.pepper_key_id.clone()),
            )
            .col_expr(
                password_credentials::Column::UpdatedAt,
                Expr::value(now.to_rfc3339()),
            )
            .filter(password_credentials::Column::UserId.eq(user_id.to_string()))
            .exec(&tx)
            .await?;
        users::Entity::update_many()
            .col_expr(users::Column::UpdatedAt, Expr::value(now.to_rfc3339()))
            .filter(users::Column::Id.eq(user_id.to_string()))
            .exec(&tx)
            .await?;
        auth_sessions::Entity::update_many()
            .col_expr(
                auth_sessions::Column::RevokedAt,
                Expr::value(now.to_rfc3339()),
            )
            .col_expr(
                auth_sessions::Column::RevocationReason,
                Expr::value("password_change"),
            )
            .filter(auth_sessions::Column::UserId.eq(user_id.to_string()))
            .filter(auth_sessions::Column::Id.ne(keep_session_id.to_string()))
            .filter(auth_sessions::Column::RevokedAt.is_null())
            .exec(&tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn store_code(
        &self,
        tx: &DatabaseTransaction,
        user_id: Uuid,
        purpose: &str,
        lifetime: Duration,
        now: DateTime<Utc>,
    ) -> Result<IssuedCode, DomainError> {
        let value = format!("{:06}", rand::rng().random_range(0..1_000_000u32));
        let digest = self.code_digest(user_id, purpose, &value);
        auth_one_time_tokens::Entity::delete_many()
            .filter(auth_one_time_tokens::Column::UserId.eq(user_id.to_string()))
            .filter(auth_one_time_tokens::Column::Purpose.eq(purpose))
            .exec(tx)
            .await?;
        auth_one_time_tokens::ActiveModel {
            id: Set(Uuid::now_v7().to_string()),
            user_id: Set(user_id.to_string()),
            purpose: Set(purpose.to_owned()),
            code_digest: Set(digest.to_vec()),
            key_id: Set(self.config.key_id.clone()),
            attempts: Set(0),
            created_at: Set(now.to_rfc3339()),
            expires_at: Set((now + lifetime).to_rfc3339()),
            consumed_at: Set(None),
        }
        .insert(tx)
        .await?;
        Ok(IssuedCode { value })
    }

    fn code_digest(&self, user_id: Uuid, purpose: &str, code: &str) -> [u8; 32] {
        hmac_digest(
            &self.config.hmac_key,
            CODE_DOMAIN,
            format!("{user_id}\0{purpose}").as_bytes(),
            code.as_bytes(),
        )
    }

    async fn consume_code(
        &self,
        tx: &DatabaseTransaction,
        user_id: Uuid,
        code: &str,
        purpose: &str,
    ) -> Result<(), DomainError> {
        let row = auth_one_time_tokens::Entity::find()
            .filter(auth_one_time_tokens::Column::UserId.eq(user_id.to_string()))
            .filter(auth_one_time_tokens::Column::Purpose.eq(purpose))
            .one(tx)
            .await?
            .ok_or(DomainError::InvalidCredentials)?;
        let now = self.clock.now();
        let expires_at = parse_date(&row.expires_at)?;
        if row.key_id != self.config.key_id
            || row.consumed_at.is_some()
            || row.attempts >= MAX_CODE_ATTEMPTS
            || now >= expires_at
        {
            return Err(DomainError::InvalidCredentials);
        }
        let expected = self.code_digest(user_id, purpose, code);
        if row.code_digest.len() != 32
            || expected.ct_eq(row.code_digest.as_slice()).unwrap_u8() != 1
        {
            let attempts = row.attempts + 1;
            let mut update = auth_one_time_tokens::Entity::update_many().col_expr(
                auth_one_time_tokens::Column::Attempts,
                Expr::value(attempts),
            );
            if attempts >= MAX_CODE_ATTEMPTS {
                update = update.col_expr(
                    auth_one_time_tokens::Column::ConsumedAt,
                    Expr::value(now.to_rfc3339()),
                );
            }
            update
                .filter(auth_one_time_tokens::Column::Id.eq(row.id.clone()))
                .exec(tx)
                .await?;
            return Err(DomainError::InvalidCredentials);
        }
        let consumed = auth_one_time_tokens::Entity::update_many()
            .col_expr(
                auth_one_time_tokens::Column::ConsumedAt,
                Expr::value(now.to_rfc3339()),
            )
            .filter(auth_one_time_tokens::Column::Id.eq(row.id.clone()))
            .filter(auth_one_time_tokens::Column::ConsumedAt.is_null())
            .filter(auth_one_time_tokens::Column::ExpiresAt.gt(now.to_rfc3339()))
            .exec(tx)
            .await?;
        if consumed.rows_affected != 1 {
            return Err(DomainError::InvalidCredentials);
        }
        Ok(())
    }

    async fn user_for_code(
        &self,
        tx: &DatabaseTransaction,
        email: &str,
    ) -> Result<Uuid, DomainError> {
        let Ok((normalized, _)) = normalize_email(email) else {
            return Err(DomainError::InvalidCredentials);
        };
        let user = users::Entity::find()
            .filter(users::Column::EmailNormalized.eq(normalized))
            .one(tx)
            .await?
            .ok_or(DomainError::InvalidCredentials)?;
        Uuid::parse_str(&user.id).map_err(|_| DomainError::InvalidCredentials)
    }

    async fn send_code(
        &self,
        recipient: &str,
        purpose: &str,
        code: &IssuedCode,
    ) -> Result<(), DomainError> {
        let (subject, action, lifetime) = if purpose == VERIFY_EMAIL {
            (
                "Verify your Aegis email address",
                "verify your email address",
                VERIFY_EMAIL_LIFETIME_TEXT,
            )
        } else {
            (
                "Reset your Aegis password",
                "reset your password",
                RESET_PASSWORD_LIFETIME_TEXT,
            )
        };
        self.mailer
            .send(Mail {
                to: recipient.to_owned(),
                subject: subject.to_owned(),
                body: format!(
                    "Your Aegis code is {}\n\nEnter it to {action}. It expires in {lifetime}.\n",
                    code.value
                ),
            })
            .await
    }
}

fn parse_date(value: &str) -> Result<DateTime<Utc>, DomainError> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| DomainError::InvalidCredentials)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{
            AuthConfig, AuthorizationCodeRedemption, AuthorizationCodeRequest, CapturingMailer,
            ClientRegistration, Domain, DomainOptions, OAuthConfig, extract_code,
        },
        migration::Migrator,
    };
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use chrono::TimeZone;
    use sea_orm::{ConnectionTrait, Database, Statement};
    use sea_orm_migration::MigratorTrait;
    use sha2::{Digest, Sha256};
    use std::{sync::RwLock, time::Duration as StdDuration};

    struct MutableClock(RwLock<DateTime<Utc>>);
    impl Clock for MutableClock {
        fn now(&self) -> DateTime<Utc> {
            *self.0.read().expect("clock lock")
        }
    }
    impl MutableClock {
        fn advance(&self, delta: Duration) {
            let mut value = self.0.write().expect("clock lock");
            *value += delta;
        }
    }

    async fn fixture(
        registration_enabled: bool,
    ) -> (
        DatabaseConnection,
        Arc<Domain>,
        Arc<CapturingMailer>,
        Arc<MutableClock>,
    ) {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        db.execute_unprepared("PRAGMA foreign_keys=ON")
            .await
            .unwrap();
        Migrator::up(&db, None).await.unwrap();
        let mailer = Arc::new(CapturingMailer::default());
        let clock = Arc::new(MutableClock(RwLock::new(
            Utc.with_ymd_and_hms(2026, 8, 16, 12, 0, 0).unwrap(),
        )));
        let domain = Arc::new(
            Domain::with_clock(
                db.clone(),
                AuthConfig {
                    session_hmac_key: [5; 32],
                    session_key_id: "session".into(),
                    password_pepper: b"pepper".to_vec(),
                    pepper_key_id: "pepper".into(),
                    password_concurrency: 2,
                    idle_lifetime: StdDuration::from_secs(60),
                    absolute_lifetime: StdDuration::from_secs(120),
                },
                OAuthConfig {
                    hmac_key: [6; 32],
                    key_id: "oauth".into(),
                },
                DomainOptions {
                    registration_enabled,
                    mailer: mailer.clone(),
                },
                clock.clone(),
            )
            .await
            .unwrap(),
        );
        (db, domain, mailer, clock)
    }

    fn password(value: &str) -> Password {
        Password::new(value.to_owned()).unwrap()
    }

    #[test]
    fn policy_requires_length_letter_digit_and_special() {
        for weak in [
            "aB3!def",
            "abcdefgh!",
            "abcdefgh1",
            "12345678!",
            "abcdefghij",
            "12345678",
        ] {
            assert!(
                check_password_policy(&password(weak)).is_err(),
                "accepted {weak}"
            );
        }
        for strong in ["abcdefg1!", "Sup3r-secret", "passw0rd?"] {
            assert!(
                check_password_policy(&password(strong)).is_ok(),
                "rejected {strong}"
            );
        }
    }

    #[tokio::test]
    async fn registration_is_refused_when_disabled() {
        let (_db, domain, mailer, _clock) = fixture(false).await;
        assert!(matches!(
            domain
                .account()
                .register("user@example.com", &password("passw0rd!"))
                .await,
            Err(DomainError::Forbidden)
        ));
        assert!(mailer.take().is_empty());
    }

    #[tokio::test]
    async fn register_verify_and_login_round_trip() {
        let (db, domain, mailer, _clock) = fixture(true).await;
        assert!(matches!(
            domain
                .account()
                .register("user@example.com", &password("weakpassword"))
                .await,
            Err(DomainError::InvalidInput(_))
        ));

        domain
            .account()
            .register(" User@Example.com ", &password("passw0rd!"))
            .await
            .unwrap();
        let mails = mailer.take();
        assert_eq!(mails.len(), 1);
        let code = extract_code(&mails[0].body);
        assert!(!mails[0].body.is_empty());

        assert!(matches!(
            domain
                .auth()
                .login("user@example.com", &password("passw0rd!"), None)
                .await,
            Err(DomainError::InvalidCredentials)
        ));

        domain
            .account()
            .register("user@example.com", &password("other-p4ss!"))
            .await
            .unwrap();
        assert!(mailer.take().is_empty());

        domain
            .account()
            .verify_email("user@example.com", &code)
            .await
            .unwrap();
        domain
            .auth()
            .login("user@example.com", &password("passw0rd!"), None)
            .await
            .unwrap();

        assert!(matches!(
            domain
                .account()
                .verify_email("user@example.com", &code)
                .await,
            Err(DomainError::InvalidCredentials)
        ));

        let digest: Vec<u8> = db
            .query_one_raw(Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                "SELECT code_digest FROM auth_one_time_tokens",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "code_digest")
            .unwrap();
        assert_eq!(digest.len(), 32);
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|item| item.is_ascii_digit()));
    }

    #[tokio::test]
    async fn verification_code_expires() {
        let (_db, domain, mailer, clock) = fixture(true).await;
        domain
            .account()
            .register("user@example.com", &password("passw0rd!"))
            .await
            .unwrap();
        let code = extract_code(&mailer.take()[0].body);
        clock.advance(Duration::hours(25));
        assert!(matches!(
            domain
                .account()
                .verify_email("user@example.com", &code)
                .await,
            Err(DomainError::InvalidCredentials)
        ));
    }

    fn wrong_code(code: &str) -> String {
        if code == "000000" { "111111" } else { "000000" }.to_owned()
    }

    #[tokio::test]
    async fn wrong_code_is_refused_for_an_unknown_address() {
        let (_db, domain, mailer, _clock) = fixture(true).await;
        domain
            .account()
            .register("user@example.com", &password("passw0rd!"))
            .await
            .unwrap();
        let code = extract_code(&mailer.take()[0].body);
        for email in ["other@example.com", "not-an-email"] {
            assert!(matches!(
                domain.account().verify_email(email, &code).await,
                Err(DomainError::InvalidCredentials)
            ));
        }
        domain
            .account()
            .verify_email("user@example.com", &code)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn five_failed_attempts_kill_the_code() {
        let (_db, domain, mailer, _clock) = fixture(true).await;
        domain
            .account()
            .register("user@example.com", &password("passw0rd!"))
            .await
            .unwrap();
        let code = extract_code(&mailer.take()[0].body);
        let wrong = wrong_code(&code);
        for _ in 0..4 {
            assert!(matches!(
                domain
                    .account()
                    .verify_email("user@example.com", &wrong)
                    .await,
                Err(DomainError::InvalidCredentials)
            ));
        }
        domain
            .account()
            .verify_email("user@example.com", &code)
            .await
            .unwrap();

        domain
            .account()
            .request_password_reset("user@example.com")
            .await
            .unwrap();
        let reset_code = extract_code(&mailer.take()[0].body);
        let wrong = wrong_code(&reset_code);
        for _ in 0..5 {
            assert!(matches!(
                domain
                    .account()
                    .reset_password("user@example.com", &wrong, &password("n3w-passw0rd!"))
                    .await,
                Err(DomainError::InvalidCredentials)
            ));
        }
        assert!(matches!(
            domain
                .account()
                .reset_password("user@example.com", &reset_code, &password("n3w-passw0rd!"))
                .await,
            Err(DomainError::InvalidCredentials)
        ));

        domain
            .account()
            .request_password_reset("user@example.com")
            .await
            .unwrap();
        let fresh_code = extract_code(&mailer.take()[0].body);
        domain
            .account()
            .reset_password("user@example.com", &fresh_code, &password("n3w-passw0rd!"))
            .await
            .unwrap();
        domain
            .auth()
            .login("user@example.com", &password("n3w-passw0rd!"), None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn reset_replaces_the_password_and_ends_browser_sessions_only() {
        let (db, domain, mailer, clock) = fixture(true).await;
        domain
            .account()
            .register("user@example.com", &password("passw0rd!"))
            .await
            .unwrap();
        let code = extract_code(&mailer.take()[0].body);
        domain
            .account()
            .verify_email("user@example.com", &code)
            .await
            .unwrap();
        let issued = domain
            .auth()
            .login("user@example.com", &password("passw0rd!"), None)
            .await
            .unwrap();
        let access_token = connect_app(&domain, "passw0rd!").await;

        domain
            .account()
            .request_password_reset("missing@example.com")
            .await
            .unwrap();
        domain
            .account()
            .request_password_reset("not-an-email")
            .await
            .unwrap();
        assert!(mailer.take().is_empty());

        domain
            .account()
            .request_password_reset("user@example.com")
            .await
            .unwrap();
        let reset_code = extract_code(&mailer.take()[0].body);

        assert!(matches!(
            domain
                .account()
                .reset_password("user@example.com", &reset_code, &password("weakpassword"))
                .await,
            Err(DomainError::InvalidInput(_))
        ));
        assert!(matches!(
            domain
                .account()
                .reset_password("user@example.com", &code, &password("n3w-passw0rd!"))
                .await,
            Err(DomainError::InvalidCredentials)
        ));

        domain
            .account()
            .reset_password("user@example.com", &reset_code, &password("n3w-passw0rd!"))
            .await
            .unwrap();
        assert!(matches!(
            domain.auth().authenticate(issued.token(), None).await,
            Err(DomainError::InvalidCredentials)
        ));
        assert!(matches!(
            domain
                .auth()
                .login("user@example.com", &password("passw0rd!"), None)
                .await,
            Err(DomainError::InvalidCredentials)
        ));
        domain
            .auth()
            .login("user@example.com", &password("n3w-passw0rd!"), None)
            .await
            .unwrap();

        assert!(matches!(
            domain
                .account()
                .reset_password("user@example.com", &reset_code, &password("an0ther-pass!"))
                .await,
            Err(DomainError::InvalidCredentials)
        ));

        domain
            .account()
            .request_password_reset("user@example.com")
            .await
            .unwrap();
        let late_code = extract_code(&mailer.take()[0].body);
        clock.advance(Duration::minutes(31));
        assert!(matches!(
            domain
                .account()
                .reset_password("user@example.com", &late_code, &password("an0ther-pass!"))
                .await,
            Err(DomainError::InvalidCredentials)
        ));

        let revoked: i64 = db
            .query_one_raw(Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                "SELECT count(*) AS n FROM auth_sessions WHERE revocation_reason='password_reset'",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "n")
            .unwrap();
        assert_eq!(revoked, 1);

        domain
            .oauth()
            .authenticate_access_token(&access_token, ISSUER, RESOURCE)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn change_ends_other_browser_sessions_but_keeps_the_connected_apps() {
        let (_db, domain, mailer, _clock) = fixture(true).await;
        domain
            .account()
            .register("user@example.com", &password("passw0rd!"))
            .await
            .unwrap();
        let code = extract_code(&mailer.take()[0].body);
        domain
            .account()
            .verify_email("user@example.com", &code)
            .await
            .unwrap();
        let kept = domain
            .auth()
            .login("user@example.com", &password("passw0rd!"), None)
            .await
            .unwrap();
        let other = domain
            .auth()
            .login("user@example.com", &password("passw0rd!"), None)
            .await
            .unwrap();
        let principal = domain
            .auth()
            .authenticate(kept.token(), None)
            .await
            .unwrap();
        let access_token = connect_app(&domain, "passw0rd!").await;

        domain
            .account()
            .change_password(
                principal.user_id(),
                principal.session_id().unwrap(),
                &password("n3w-passw0rd!"),
            )
            .await
            .unwrap();

        domain
            .auth()
            .authenticate(kept.token(), None)
            .await
            .unwrap();
        assert!(matches!(
            domain.auth().authenticate(other.token(), None).await,
            Err(DomainError::InvalidCredentials)
        ));
        domain
            .oauth()
            .authenticate_access_token(&access_token, ISSUER, RESOURCE)
            .await
            .unwrap();
    }

    const ISSUER: &str = "https://aegis.example";
    const RESOURCE: &str = "https://aegis.example/mcp";

    async fn connect_app(domain: &Domain, current_password: &str) -> String {
        let identity = domain
            .auth()
            .verify_password_identity("user@example.com", &password(current_password))
            .await
            .unwrap();
        let client = domain
            .oauth()
            .register_public_client(ClientRegistration {
                issuer: ISSUER.into(),
                redirect_uris: vec!["http://127.0.0.1:49152/callback".into()],
                client_name: None,
                application_type: Some("native".into()),
                grant_types: vec!["authorization_code".into()],
                response_types: vec!["code".into()],
                scope: "usage:read".into(),
            })
            .await
            .unwrap();
        let verifier = "correct-verifier-with-at-least-forty-three-characters-123";
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let code = domain
            .oauth()
            .issue_authorization_code(AuthorizationCodeRequest {
                identity: &identity,
                client_id: client.client_id(),
                issuer: ISSUER,
                redirect_uri: "http://127.0.0.1:49152/callback",
                resource: RESOURCE,
                scope: "usage:read",
                code_challenge: &challenge,
            })
            .await
            .unwrap();
        domain
            .oauth()
            .redeem_authorization_code(AuthorizationCodeRedemption {
                code: code.expose(),
                client_id: client.client_id(),
                issuer: ISSUER,
                redirect_uri: "http://127.0.0.1:49152/callback",
                resource: RESOURCE,
                code_verifier: verifier,
            })
            .await
            .unwrap()
            .expose()
            .to_owned()
    }
}
