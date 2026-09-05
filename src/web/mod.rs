mod error;
mod pages;
pub(crate) mod views;

use crate::{
    api_keys::KeyStore,
    domain::{Domain, Principal},
    gateway::Gateway,
    origin::OriginPolicy,
    usage::UsageStore,
};
use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::{HeaderName, HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use error::WebError;
use rand::Rng;
use std::{
    ops::Deref,
    sync::{Arc, LazyLock},
};
use subtle::ConstantTimeEq;

pub const SESSION_COOKIE: &str = "__Host-aegis_session";
pub const CSRF_COOKIE: &str = "__Host-aegis_csrf";
pub const INSECURE_SESSION_COOKIE: &str = "aegis_session";
pub const INSECURE_CSRF_COOKIE: &str = "aegis_csrf";

#[derive(Clone)]
pub struct SessionState {
    pub domain: Arc<Domain>,
    pub origin: OriginPolicy,
}

impl SessionState {
    pub fn new(domain: Arc<Domain>, origin: OriginPolicy) -> Self {
        Self { domain, origin }
    }

    fn secure_cookies(&self) -> bool {
        self.origin.secure_cookies()
    }

    pub(crate) fn session_cookie_name(&self) -> &'static str {
        if self.secure_cookies() {
            SESSION_COOKIE
        } else {
            INSECURE_SESSION_COOKIE
        }
    }

    pub(crate) fn csrf_cookie_name(&self) -> &'static str {
        if self.secure_cookies() {
            CSRF_COOKIE
        } else {
            INSECURE_CSRF_COOKIE
        }
    }

    fn cookie_lifetime(&self) -> time::Duration {
        time::Duration::seconds(
            self.domain
                .auth()
                .absolute_lifetime()
                .as_secs()
                .try_into()
                .unwrap_or(i64::MAX),
        )
    }

    fn session_cookie(&self, value: &str) -> Cookie<'static> {
        Cookie::build((self.session_cookie_name(), value.to_owned()))
            .path("/")
            .secure(self.secure_cookies())
            .http_only(true)
            .same_site(SameSite::Lax)
            .max_age(self.cookie_lifetime())
            .build()
    }

    pub(crate) fn csrf_cookie(&self, value: &str) -> Cookie<'static> {
        Cookie::build((self.csrf_cookie_name(), value.to_owned()))
            .path("/")
            .secure(self.secure_cookies())
            .http_only(false)
            .same_site(SameSite::Lax)
            .max_age(self.cookie_lifetime())
            .build()
    }

    pub(crate) fn expired_session_cookie(&self) -> Cookie<'static> {
        Cookie::build((self.session_cookie_name(), ""))
            .path("/")
            .secure(self.secure_cookies())
            .http_only(true)
            .same_site(SameSite::Lax)
            .removal()
            .build()
    }

    pub(crate) fn expired_csrf_cookie(&self) -> Cookie<'static> {
        Cookie::build((self.csrf_cookie_name(), ""))
            .path("/")
            .secure(self.secure_cookies())
            .http_only(false)
            .same_site(SameSite::Lax)
            .removal()
            .build()
    }

    pub(crate) fn csrf_value<'a>(&self, jar: &'a CookieJar) -> Option<&'a str> {
        jar.get(self.csrf_cookie_name())
            .map(|cookie| cookie.value())
    }
}

#[derive(Clone)]
pub struct AppState {
    session: SessionState,
    pub gateway: Gateway,
    pub keys: KeyStore,
    pub usage: UsageStore,
}

impl AppState {
    pub fn new(session: SessionState, gateway: Gateway, keys: KeyStore, usage: UsageStore) -> Self {
        Self {
            session,
            gateway,
            keys,
            usage,
        }
    }
}

impl Deref for AppState {
    type Target = SessionState;

    fn deref(&self) -> &Self::Target {
        &self.session
    }
}

pub fn asset_router() -> Router {
    common(
        Router::new()
            .route(views::STYLES_PATH, get(styles_css))
            .route(views::FONT_PATH, get(inter_font))
            .route(FONT_LICENSE_PATH, get(inter_font_license)),
    )
}

pub fn auth_router(state: AppState) -> Router {
    common(
        Router::new()
            .route("/", get(pages::root))
            .route("/login", get(pages::login_page).post(pages::login_submit))
            .route("/logout", post(pages::logout_submit))
            .route(
                "/register",
                get(pages::register_page).post(pages::register_submit),
            )
            .route("/verify", post(pages::verify_submit))
            .route(
                "/reset",
                get(pages::reset_page).post(pages::reset_request_submit),
            )
            .route("/reset/confirm", post(pages::reset_confirm_submit))
            .route("/account", get(pages::account_page))
            .route(
                "/account/api-keys",
                get(pages::api_keys_page).post(pages::api_key_create),
            )
            .route("/account/api-keys/{id}/rotate", post(pages::api_key_rotate))
            .route("/account/api-keys/{id}/revoke", post(pages::api_key_revoke))
            .route("/account/password", post(pages::account_password_submit))
            .route(
                "/account/sessions/{id}/revoke",
                post(pages::account_session_revoke),
            )
            .route(
                "/account/apps/{client_id}/revoke",
                post(pages::account_app_revoke),
            )
            .layer(middleware::map_response(auth_no_store))
            .with_state(state),
    )
}

#[cfg(test)]
fn router(state: AppState) -> Router {
    Router::new()
        .merge(auth_router(state.clone()))
        .merge(crate::oauth::router(crate::oauth::State::new(
            state.session.clone(),
        )))
        .merge(asset_router())
        .merge(crate::health::router(state.domain.clone()))
}

pub(crate) fn common(router: Router) -> Router {
    router
        .layer(DefaultBodyLimit::max(4 * 1024))
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_secs(30),
        ))
        .layer(
            tower_http::compression::CompressionLayer::new()
                .gzip(true)
                .br(true)
                .compress_when(tower_http::compression::predicate::Predicate::and(
                    tower_http::compression::predicate::DefaultPredicate::new(),
                    tower_http::compression::predicate::NotForContentType::const_new("font/"),
                )),
        )
}

fn inline_hash(text: &str) -> String {
    base64::engine::general_purpose::STANDARD
        .encode(<sha2::Sha256 as sha2::Digest>::digest(text.as_bytes()))
}

static AUTH_CONTENT_SECURITY_POLICY: LazyLock<HeaderValue> = LazyLock::new(|| {
    let font_style = inline_hash(views::FONT_STYLE.as_str());
    HeaderValue::from_str(&format!(
        "default-src 'none'; style-src 'self' 'sha256-{font_style}'; font-src 'self'; img-src data:; script-src 'none'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'"
    ))
    .expect("the content security policy is a valid header value")
});

pub(crate) async fn auth_no_store(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        AUTH_CONTENT_SECURITY_POLICY.clone(),
    );
    headers.insert(
        HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("DENY"),
    );
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    response
}

const IMMUTABLE_CACHE: &str = "public, max-age=31536000, immutable";

async fn styles_css() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, IMMUTABLE_CACHE),
        ],
        include_str!("views/assets/styles.css"),
    )
}
async fn inter_font() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "font/woff2"),
            (header::CACHE_CONTROL, IMMUTABLE_CACHE),
        ],
        include_bytes!("views/assets/fonts/inter/inter-latin-wght-normal.woff2").as_slice(),
    )
}

pub const FONT_LICENSE_PATH: &str = "/assets/fonts/inter/LICENSE";

async fn inter_font_license() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("views/assets/fonts/inter/LICENSE"),
    )
}

pub(crate) fn new_csrf_value() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub(crate) fn csrf_from_token<'a>(
    state: &SessionState,
    token: &'a str,
    jar: &CookieJar,
) -> Result<&'a str, WebError> {
    let cookie = state
        .csrf_value(jar)
        .ok_or_else(WebError::invalid_credentials)?;
    let matches =
        token.len() == cookie.len() && token.as_bytes().ct_eq(cookie.as_bytes()).unwrap_u8() == 1;
    if !matches {
        return Err(WebError::invalid_credentials());
    }
    Ok(token)
}
pub(crate) async fn authenticate(
    state: &SessionState,
    jar: &CookieJar,
    csrf: Option<&str>,
) -> Result<Principal, WebError> {
    let token = jar
        .get(state.session_cookie_name())
        .map(|v| v.value())
        .ok_or_else(WebError::invalid_credentials)?;
    state
        .domain
        .auth()
        .authenticate(token, csrf)
        .await
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api_keys::KeyStore,
        domain::{AuthConfig, Domain, OAuthConfig, Password, bootstrap_superuser},
        gateway::Gateway,
        migration::Migrator,
        origin::OriginPolicy,
        policies::Pipeline,
        telemetry::SqliteSink,
    };
    use axum::{
        body::Body,
        http::{Request, header},
    };
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use sea_orm::Database;
    use sea_orm_migration::MigratorTrait;
    use sha2::{Digest, Sha256};
    use std::{sync::Arc, time::Duration};
    use tower::ServiceExt;

    async fn application() -> Router {
        application_with_policy(OriginPolicy::new(Some("https://127.0.0.1:3000".into()))).await
    }

    async fn account_application(
        registration_enabled: bool,
    ) -> (Router, Arc<crate::domain::CapturingMailer>) {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let mailer = Arc::new(crate::domain::CapturingMailer::default());
        let domain = Arc::new(
            Domain::with_options(
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
                OAuthConfig {
                    hmac_key: [4; 32],
                    key_id: "oauth".into(),
                },
                crate::domain::DomainOptions {
                    registration_enabled,
                    mailer: mailer.clone(),
                },
            )
            .await
            .unwrap(),
        );
        let keys = KeyStore::new(db.clone());
        let usage = crate::usage::UsageStore::new(db.clone());
        let gateway = Gateway::new(
            SqliteSink::new(db),
            keys.clone(),
            Pipeline::default(),
            vec![],
            1024,
        )
        .unwrap();
        let router = router(AppState::new(
            SessionState::new(domain, OriginPolicy::new(None)),
            gateway,
            keys,
            usage,
        ));
        (router, mailer)
    }

    #[tokio::test]
    async fn browser_registration_pages_are_hidden_when_registration_is_closed() {
        let host = "127.0.0.1:3000";
        let (app, _mailer) = account_application(false).await;

        let page = app
            .clone()
            .oneshot(
                Request::get("/login")
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(page.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert!(!std::str::from_utf8(&body).unwrap().contains("/register"));

        let hidden = app
            .clone()
            .oneshot(
                Request::get("/register")
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(hidden.status(), StatusCode::NOT_FOUND);

        for (path, body) in [
            (
                "/register",
                "email=user%40example.com&password=passw0rd%21&csrf=x",
            ),
            ("/verify", "email=user%40example.com&code=000000&csrf=x"),
        ] {
            let posted = app
                .clone()
                .oneshot(
                    Request::post(path)
                        .header(header::HOST, host)
                        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(posted.status(), StatusCode::NOT_FOUND);
        }
    }

    #[tokio::test]
    async fn browser_register_verify_and_sign_in_flow() {
        let host = "127.0.0.1:3000";
        let (app, mailer) = account_application(true).await;

        let login_page = app
            .clone()
            .oneshot(
                Request::get("/login")
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(login_page.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains(r#"href="/register""#)
        );

        let page = app
            .clone()
            .oneshot(
                Request::get("/register")
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(page.status(), StatusCode::OK);
        let csrf_cookie = page.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_owned();
        let csrf_pair = csrf_cookie.split(';').next().unwrap().to_owned();
        let csrf_value = csrf_pair.split_once('=').unwrap().1.to_owned();
        let body = axum::body::to_bytes(page.into_body(), 64 * 1024)
            .await
            .unwrap();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(html.contains(r#"form method="post" action="/register""#));
        assert!(html.contains(&format!(
            r#"input type="hidden" name="csrf" value="{csrf_value}""#
        )));

        let register_body = |password: &str, csrf: &str| {
            url::form_urlencoded::Serializer::new(String::new())
                .append_pair("email", "user@example.com")
                .append_pair("password", password)
                .append_pair("csrf", csrf)
                .finish()
        };
        let register = |password: &'static str, csrf: &str| {
            Request::post("/register")
                .header(header::HOST, host)
                .header(header::COOKIE, &csrf_pair)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(register_body(password, csrf)))
                .unwrap()
        };

        let bad_csrf = app
            .clone()
            .oneshot(register("passw0rd!", "wrong-token"))
            .await
            .unwrap();
        assert_eq!(bad_csrf.status(), StatusCode::UNAUTHORIZED);

        let weak = app
            .clone()
            .oneshot(register("weakpassword", &csrf_value))
            .await
            .unwrap();
        assert_eq!(weak.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = axum::body::to_bytes(weak.into_body(), 64 * 1024)
            .await
            .unwrap();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(html.contains("at least 8 characters"));
        assert!(html.contains(r#"value="user@example.com""#));
        assert!(mailer.take().is_empty());

        let created = app
            .clone()
            .oneshot(register("passw0rd!", &csrf_value))
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::OK);
        let body = axum::body::to_bytes(created.into_body(), 64 * 1024)
            .await
            .unwrap();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(html.contains("Check your email"));
        assert!(html.contains(r#"form method="post" action="/verify""#));
        assert!(html.contains(r#"inputmode="numeric""#));
        assert!(html.contains(r#"pattern="[0-9]{6}""#));
        assert!(html.contains(r#"<input type="hidden" name="email" value="user@example.com">"#));
        let code = crate::domain::extract_code(&mailer.take()[0].body);

        let verify = |code: &str| {
            let body = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("email", "user@example.com")
                .append_pair("code", code)
                .append_pair("csrf", &csrf_value)
                .finish();
            Request::post("/verify")
                .header(header::HOST, host)
                .header(header::COOKIE, &csrf_pair)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap()
        };

        let invalid = app.clone().oneshot(verify("000000")).await.unwrap();
        assert_eq!(invalid.status(), StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(invalid.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains("That code is invalid or expired.")
        );

        let verified = app.clone().oneshot(verify(&code)).await.unwrap();
        assert_eq!(verified.status(), StatusCode::SEE_OTHER);
        assert_eq!(verified.headers()[header::LOCATION], "/login?verified=1");

        let notice = app
            .clone()
            .oneshot(
                Request::get("/login?verified=1")
                    .header(header::HOST, host)
                    .header(header::COOKIE, &csrf_pair)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(notice.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains("Email verified. Sign in to continue.")
        );

        let sign_in_body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("email", "user@example.com")
            .append_pair("password", "passw0rd!")
            .append_pair("csrf", &csrf_value)
            .finish();
        let sign_in = app
            .oneshot(
                Request::post("/login")
                    .header(header::HOST, host)
                    .header(header::COOKIE, &csrf_pair)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(sign_in_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(sign_in.status(), StatusCode::SEE_OTHER);
        assert_eq!(sign_in.headers()[header::LOCATION], "/");
    }

    #[tokio::test]
    async fn browser_reset_flow_changes_the_password() {
        let host = "127.0.0.1:3000";
        let (app, mailer) = account_application(true).await;

        let page = app
            .clone()
            .oneshot(
                Request::get("/register")
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let csrf_pair = page.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let csrf = csrf_pair.split_once('=').unwrap().1.to_owned();
        let form = |path: &str, pairs: &[(&str, &str)]| {
            let mut body = url::form_urlencoded::Serializer::new(String::new());
            for (name, value) in pairs {
                body.append_pair(name, value);
            }
            form_post(path, host, &csrf_pair, body.finish())
        };
        let registered = app
            .clone()
            .oneshot(form(
                "/register",
                &[
                    ("email", "user@example.com"),
                    ("password", "passw0rd!"),
                    ("csrf", &csrf),
                ],
            ))
            .await
            .unwrap();
        assert_eq!(registered.status(), StatusCode::OK);
        let code = crate::domain::extract_code(&mailer.take()[0].body);
        let verified = app
            .clone()
            .oneshot(form(
                "/verify",
                &[
                    ("email", "user@example.com"),
                    ("code", &code),
                    ("csrf", &csrf),
                ],
            ))
            .await
            .unwrap();
        assert_eq!(verified.status(), StatusCode::SEE_OTHER);

        let login_page = app
            .clone()
            .oneshot(
                Request::get("/login")
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(login_page.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains(r#"href="/reset""#)
        );
        let reset_page = app
            .clone()
            .oneshot(
                Request::get("/reset")
                    .header(header::HOST, host)
                    .header(header::COOKIE, &csrf_pair)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(reset_page.status(), StatusCode::OK);

        let unknown = app
            .clone()
            .oneshot(form(
                "/reset",
                &[("email", "missing@example.com"), ("csrf", &csrf)],
            ))
            .await
            .unwrap();
        assert_eq!(unknown.status(), StatusCode::OK);
        assert!(mailer.take().is_empty());

        let requested = app
            .clone()
            .oneshot(form(
                "/reset",
                &[("email", "user@example.com"), ("csrf", &csrf)],
            ))
            .await
            .unwrap();
        assert_eq!(requested.status(), StatusCode::OK);
        let reset_code = crate::domain::extract_code(&mailer.take()[0].body);
        let wrong = app
            .clone()
            .oneshot(form(
                "/reset/confirm",
                &[
                    ("email", "user@example.com"),
                    ("code", "000000"),
                    ("password", "n3w-passw0rd!"),
                    ("password_confirm", "n3w-passw0rd!"),
                    ("csrf", &csrf),
                ],
            ))
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

        let mismatch = app
            .clone()
            .oneshot(form(
                "/reset/confirm",
                &[
                    ("email", "user@example.com"),
                    ("code", &reset_code),
                    ("password", "n3w-passw0rd!"),
                    ("password_confirm", "n3w-passw0rd?"),
                    ("csrf", &csrf),
                ],
            ))
            .await
            .unwrap();
        assert_eq!(mismatch.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = axum::body::to_bytes(mismatch.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains("The two passwords are not the same.")
        );

        let reset = app
            .clone()
            .oneshot(form(
                "/reset/confirm",
                &[
                    ("email", "user@example.com"),
                    ("code", &reset_code),
                    ("password", "n3w-passw0rd!"),
                    ("password_confirm", "n3w-passw0rd!"),
                    ("csrf", &csrf),
                ],
            ))
            .await
            .unwrap();
        assert_eq!(reset.status(), StatusCode::SEE_OTHER);
        assert_eq!(reset.headers()[header::LOCATION], "/login?reset=1");

        let old = app
            .clone()
            .oneshot(form(
                "/login",
                &[
                    ("email", "user@example.com"),
                    ("password", "passw0rd!"),
                    ("csrf", &csrf),
                ],
            ))
            .await
            .unwrap();
        assert_eq!(old.status(), StatusCode::UNAUTHORIZED);
        sign_in(&app, host, "user@example.com", "n3w-passw0rd!").await;
    }

    async fn application_with_policy(origin: OriginPolicy) -> Router {
        application_parts(origin).await.0
    }

    async fn application_parts(origin: OriginPolicy) -> (Router, Arc<Domain>) {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let domain = Arc::new(
            Domain::new(
                db.clone(),
                AuthConfig {
                    session_hmac_key: [9; 32],
                    session_key_id: "session".into(),
                    password_pepper: b"pepper".to_vec(),
                    pepper_key_id: "pepper".into(),
                    password_concurrency: 2,
                    idle_lifetime: Duration::from_secs(60),
                    absolute_lifetime: Duration::from_secs(120),
                },
                OAuthConfig {
                    hmac_key: [8; 32],
                    key_id: "oauth".into(),
                },
            )
            .await
            .unwrap(),
        );
        bootstrap_superuser(
            &db,
            b"pepper",
            "pepper",
            "admin@example.com",
            &Password::new("correct passw0rd!".into()).unwrap(),
        )
        .await
        .unwrap();
        let keys = KeyStore::new(db.clone());
        let usage = crate::usage::UsageStore::new(db.clone());
        let gateway = Gateway::new(
            SqliteSink::new(db),
            keys.clone(),
            Pipeline::default(),
            vec![],
            1024,
        )
        .unwrap();
        let router = router(AppState::new(
            SessionState::new(domain.clone(), origin),
            gateway,
            keys,
            usage,
        ));
        (router, domain)
    }

    async fn sign_in(app: &Router, host: &str, email: &str, password: &str) -> (String, String) {
        sign_in_with_agent(app, host, email, password, None).await
    }

    async fn sign_in_with_agent(
        app: &Router,
        host: &str,
        email: &str,
        password: &str,
        agent: Option<&str>,
    ) -> (String, String) {
        let page = app
            .clone()
            .oneshot(
                Request::get("/login")
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let csrf_pair = page.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let csrf_value = csrf_pair.split_once('=').unwrap().1.to_owned();
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("email", email)
            .append_pair("password", password)
            .append_pair("csrf", &csrf_value)
            .finish();
        let mut request = Request::post("/login")
            .header(header::HOST, host)
            .header(header::COOKIE, &csrf_pair)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(agent) = agent {
            request = request.header(header::USER_AGENT, agent);
        }
        let login = app
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::SEE_OTHER);
        let cookies: Vec<String> = login
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|value| {
                value
                    .to_str()
                    .unwrap()
                    .split(';')
                    .next()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        let csrf = cookies
            .iter()
            .find_map(|pair| {
                pair.strip_prefix("__Host-aegis_csrf=")
                    .or_else(|| pair.strip_prefix("aegis_csrf="))
            })
            .unwrap()
            .to_owned();
        (cookies.join("; "), csrf)
    }

    fn form_post(path: &str, host: &str, cookies: &str, body: String) -> Request<Body> {
        Request::post(path)
            .header(header::HOST, host)
            .header(header::COOKIE, cookies)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap()
    }

    async fn get_account(app: &Router, host: &str, cookies: &str) -> (StatusCode, String, String) {
        let response = app
            .clone()
            .oneshot(
                Request::get("/account")
                    .header(header::HOST, host)
                    .header(header::COOKIE, cookies)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .map(|value| value.to_str().unwrap().to_owned())
            .unwrap_or_default();
        let body = axum::body::to_bytes(response.into_body(), 128 * 1024)
            .await
            .unwrap();
        (status, location, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn a_lost_csrf_cookie_keeps_the_session_and_mints_a_new_token() {
        let host = "127.0.0.1:3000";
        let app = application().await;
        let (cookies, csrf) = sign_in(&app, host, "admin@example.com", "correct passw0rd!").await;

        let session_only = cookies
            .split("; ")
            .find(|pair| pair.contains("aegis_session="))
            .unwrap()
            .to_owned();
        let login = app
            .clone()
            .oneshot(
                Request::get("/login")
                    .header(header::HOST, host)
                    .header(header::COOKIE, &session_only)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::SEE_OTHER);
        assert_eq!(login.headers()[header::LOCATION], "/");
        let issued: Vec<String> = login
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|value| value.to_str().unwrap().to_owned())
            .collect();
        assert!(
            issued
                .iter()
                .all(|value| !value.contains("aegis_session=;")),
            "the session cookie must survive a lost CSRF cookie: {issued:?}"
        );
        let fresh_csrf = issued
            .iter()
            .find_map(|value| {
                value
                    .split(';')
                    .next()
                    .unwrap()
                    .strip_prefix("__Host-aegis_csrf=")
                    .map(ToOwned::to_owned)
            })
            .expect("a fresh CSRF cookie");

        let paired = format!("{session_only}; __Host-aegis_csrf={fresh_csrf}");
        let (status, _, body) = get_account(&app, host, &paired).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Active sessions"));
        let (status, location, _) = get_account(
            &app,
            host,
            &format!("{session_only}; __Host-aegis_csrf={csrf}"),
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(location, "/login");
    }

    #[tokio::test]
    async fn session_cookies_outlive_the_browser() {
        let host = "127.0.0.1:3000";
        let app = application().await;
        let page = app
            .clone()
            .oneshot(
                Request::get("/login")
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let csrf_cookie = page.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(csrf_cookie.contains("Max-Age="), "{csrf_cookie}");
        let csrf_pair = csrf_cookie.split(';').next().unwrap().to_owned();
        let csrf_value = csrf_pair.split_once('=').unwrap().1.to_owned();

        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("email", "admin@example.com")
            .append_pair("password", "correct passw0rd!")
            .append_pair("csrf", &csrf_value)
            .finish();
        let login = app
            .clone()
            .oneshot(
                Request::post("/login")
                    .header(header::HOST, host)
                    .header(header::COOKIE, &csrf_pair)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::SEE_OTHER);
        for value in login.headers().get_all(header::SET_COOKIE) {
            let value = value.to_str().unwrap();
            assert!(value.contains("Max-Age="), "{value}");
        }
    }

    #[tokio::test]
    async fn root_health_form_login_account_and_logout() {
        let host = "127.0.0.1:3000";
        let app = application().await;
        let root = app
            .clone()
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(root.status(), StatusCode::SEE_OTHER);
        assert_eq!(root.headers()[header::LOCATION], "/login");
        let health = app
            .clone()
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);
        let alias = app
            .clone()
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(alias.status(), StatusCode::NOT_FOUND);

        let (cookies, csrf) = sign_in(&app, host, "admin@example.com", "correct passw0rd!").await;
        let (status, _, body) = get_account(&app, host, &cookies).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Active sessions"));

        let logout = app
            .clone()
            .oneshot(form_post(
                "/logout",
                host,
                &cookies,
                url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("csrf", &csrf)
                    .finish(),
            ))
            .await
            .unwrap();
        assert_eq!(logout.status(), StatusCode::SEE_OTHER);
        let (status, location, _) = get_account(&app, host, &cookies).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(location, "/login");
    }

    #[tokio::test]
    async fn http_origin_issues_plain_cookies_and_login_still_works() {
        let host = "127.0.0.1:3000";
        let app = application_with_policy(OriginPolicy::new(None)).await;
        let page = app
            .clone()
            .oneshot(
                Request::get("/login")
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let csrf_cookie = page.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(csrf_cookie.starts_with("aegis_csrf="));
        assert!(!csrf_cookie.contains("Secure"));
        let csrf_pair = csrf_cookie.split(';').next().unwrap().to_owned();
        let csrf_value = csrf_pair.split_once('=').unwrap().1.to_owned();

        let login = app
            .clone()
            .oneshot(form_post(
                "/login",
                host,
                &csrf_pair,
                url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("email", "admin@example.com")
                    .append_pair("password", "correct passw0rd!")
                    .append_pair("csrf", &csrf_value)
                    .finish(),
            ))
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::SEE_OTHER);
        let cookies: Vec<String> = login
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|value| value.to_str().unwrap().to_owned())
            .collect();
        let session = cookies
            .iter()
            .find(|value| value.starts_with("aegis_session="))
            .unwrap();
        assert!(!session.contains("Secure"));
        assert!(session.contains("; HttpOnly"));
        assert!(session.contains("; SameSite=Lax"));
        assert!(session.contains("; Path=/"));
        let csrf = cookies
            .iter()
            .find(|value| value.starts_with("aegis_csrf="))
            .unwrap();
        assert!(!csrf.contains("Secure"));
        let cookie_header = format!(
            "{}; {}",
            session.split(';').next().unwrap(),
            csrf.split(';').next().unwrap()
        );
        let csrf_value = csrf
            .split(';')
            .next()
            .unwrap()
            .split_once('=')
            .unwrap()
            .1
            .to_owned();

        let (status, _, _) = get_account(&app, host, &cookie_header).await;
        assert_eq!(status, StatusCode::OK);

        let logout = app
            .oneshot(form_post(
                "/logout",
                host,
                &cookie_header,
                url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("csrf", &csrf_value)
                    .finish(),
            ))
            .await
            .unwrap();
        assert_eq!(logout.status(), StatusCode::SEE_OTHER);
        for value in logout.headers().get_all(header::SET_COOKIE) {
            let value = value.to_str().unwrap();
            assert!(value.starts_with("aegis_session=") || value.starts_with("aegis_csrf="));
            assert!(!value.contains("Secure"));
        }
    }

    #[tokio::test]
    async fn the_font_license_is_served_with_the_font() {
        let app = application().await;
        let response = app
            .oneshot(
                Request::get(FONT_LICENSE_PATH)
                    .header(header::HOST, "127.0.0.1:3000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("SIL OPEN FONT LICENSE"));
        assert!(text.contains("Inter Project Authors"));
    }

    #[tokio::test]
    async fn the_policy_forbids_every_script() {
        let app = application().await;
        let host = "127.0.0.1:3000";
        let policy = AUTH_CONTENT_SECURITY_POLICY.to_str().unwrap();
        assert!(policy.contains("script-src 'none'"));
        assert!(!policy.contains("unsafe-inline"));

        for path in ["/", "/login", "/oauth/device"] {
            let page = app
                .clone()
                .oneshot(
                    Request::get(path)
                        .header(header::HOST, host)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let body = String::from_utf8(
                axum::body::to_bytes(page.into_body(), usize::MAX)
                    .await
                    .unwrap()
                    .to_vec(),
            )
            .unwrap();
            assert!(!body.contains("<script"), "{path} carries a script");
        }
    }

    #[test]
    fn font_face_style_matches_its_policy_hash() {
        let hash = inline_hash(views::FONT_STYLE.as_str());
        let policy = AUTH_CONTENT_SECURITY_POLICY.to_str().unwrap();
        assert!(policy.contains(&format!("style-src 'self' 'sha256-{hash}'")));
        assert!(!views::FONT_STYLE.contains("</style"));
    }

    #[tokio::test]
    async fn login_page_and_form_login_flow() {
        assert!(pages::valid_return_to(Some("https://evil.example/oauth/authorize?x=1")).is_none());
        assert!(pages::valid_return_to(Some("/other?x=1")).is_none());
        assert!(pages::valid_return_to(Some("/oauth/authorize")).is_none());
        assert_eq!(
            pages::valid_return_to(Some("/oauth/authorize?client_id=test")),
            Some("/oauth/authorize?client_id=test".to_owned())
        );

        let app = application().await;
        let host = "127.0.0.1:3000";

        let asset = app
            .clone()
            .oneshot(
                Request::get(views::STYLES_PATH)
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(asset.status(), StatusCode::OK);
        assert!(
            asset.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/css")
        );
        assert_eq!(
            asset.headers()[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        assert!(!asset.headers().contains_key(header::PRAGMA));
        assert!(views::STYLES_PATH.starts_with("/assets/styles."));
        assert!(views::STYLES_PATH.ends_with(".css"));
        let font = app
            .clone()
            .oneshot(
                Request::get(views::FONT_PATH)
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(font.status(), StatusCode::OK);
        assert_eq!(font.headers()[header::CONTENT_TYPE], "font/woff2");
        assert_eq!(font.headers()[header::CACHE_CONTROL], IMMUTABLE_CACHE);

        let home = app
            .clone()
            .oneshot(
                Request::get("/")
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(home.status(), StatusCode::SEE_OTHER);
        assert_eq!(home.headers()[header::LOCATION], "/login");

        let page = app
            .clone()
            .oneshot(
                Request::get("/login")
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(page.status(), StatusCode::OK);
        assert_eq!(page.headers()[header::CACHE_CONTROL], "no-store");
        assert!(
            page.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        let csrf_cookie = page.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(csrf_cookie.starts_with("__Host-aegis_csrf="));
        assert!(csrf_cookie.contains("; SameSite=Lax"));
        assert!(csrf_cookie.contains("; Secure"));
        let csrf_pair = csrf_cookie.split(';').next().unwrap().to_owned();
        let csrf_value = csrf_pair.split_once('=').unwrap().1.to_owned();
        let body = axum::body::to_bytes(page.into_body(), 64 * 1024)
            .await
            .unwrap();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(html.contains(r#"form method="post" action="/login""#));
        assert!(html.contains(r#"autocapitalize="none" spellcheck="false""#));
        assert!(html.contains(&format!(
            r#"input type="hidden" name="csrf" value="{csrf_value}""#
        )));
        assert!(!html.contains("Invalid email or password."));
        assert!(!html.contains("/account"));

        let form = |password: &str, csrf: &str| {
            url::form_urlencoded::Serializer::new(String::new())
                .append_pair("email", "admin@example.com")
                .append_pair("password", password)
                .append_pair("csrf", csrf)
                .finish()
        };

        let bad_csrf = app
            .clone()
            .oneshot(
                Request::post("/login")
                    .header(header::HOST, host)
                    .header(header::COOKIE, &csrf_pair)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(form("correct passw0rd!", "wrong-token")))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad_csrf.status(), StatusCode::UNAUTHORIZED);

        let bad_password = app
            .clone()
            .oneshot(
                Request::post("/login")
                    .header(header::HOST, host)
                    .header(header::ORIGIN, "null")
                    .header(header::COOKIE, &csrf_pair)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(form("wrong", &csrf_value)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad_password.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(bad_password.headers()[header::CACHE_CONTROL], "no-store");
        let body = axum::body::to_bytes(bad_password.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains("Invalid email or password.")
        );

        let login = app
            .clone()
            .oneshot(
                Request::post("/login")
                    .header(header::HOST, host)
                    .header(header::ORIGIN, "https://127.0.0.1:3000")
                    .header(header::COOKIE, &csrf_pair)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(form("correct passw0rd!", &csrf_value)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::SEE_OTHER);
        assert_eq!(login.headers()[header::LOCATION], "/");
        assert_eq!(login.headers()[header::CACHE_CONTROL], "no-store");
        let cookies: Vec<_> = login
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|value| value.to_str().unwrap().to_owned())
            .collect();
        let session = cookies
            .iter()
            .find(|value| value.starts_with("__Host-aegis_session="))
            .unwrap();
        assert!(session.contains("; HttpOnly"));
        let new_csrf = cookies
            .iter()
            .find(|value| value.starts_with("__Host-aegis_csrf="))
            .unwrap();
        let new_csrf_pair = new_csrf.split(';').next().unwrap();
        let new_csrf_value = new_csrf_pair.split_once('=').unwrap().1;
        let cookie_header = format!("{}; {}", session.split(';').next().unwrap(), new_csrf_pair);

        let authenticated_home = app
            .clone()
            .oneshot(
                Request::get("/")
                    .header(header::HOST, host)
                    .header(header::COOKIE, &cookie_header)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authenticated_home.status(), StatusCode::OK);
        let authenticated_home = axum::body::to_bytes(authenticated_home.into_body(), 64 * 1024)
            .await
            .unwrap();
        let authenticated_home = std::str::from_utf8(&authenticated_home).unwrap();
        assert!(authenticated_home.contains(r#"class="nav-icon" href="/account""#));
        assert!(!authenticated_home.contains("Sign out"));
        assert!(!authenticated_home.contains(r#"action="/logout""#));
        assert!(!authenticated_home.contains(r#"href="/login" role="button""#));

        let redirected = app
            .clone()
            .oneshot(
                Request::get("/login")
                    .header(header::HOST, host)
                    .header(header::COOKIE, &cookie_header)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(redirected.status(), StatusCode::SEE_OTHER);
        assert_eq!(redirected.headers()[header::LOCATION], "/");

        let logout_body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("csrf", new_csrf_value)
            .finish();
        let logout = app
            .oneshot(
                Request::post("/logout")
                    .header(header::HOST, host)
                    .header(header::ORIGIN, "https://127.0.0.1:3000")
                    .header(header::COOKIE, &cookie_header)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(logout_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(logout.status(), StatusCode::SEE_OTHER);
        assert_eq!(logout.headers()[header::LOCATION], "/");
        assert_eq!(logout.headers()[header::CACHE_CONTROL], "no-store");
    }

    #[tokio::test]
    async fn oauth_discovery_dcr_session_consent_pkce_token_replay_and_refresh_http_flow() {
        let app = application().await;
        let host = "127.0.0.1:3000";
        let issuer = "https://127.0.0.1:3000";
        let resource = "https://127.0.0.1:3000/mcp";

        let metadata = app
            .clone()
            .oneshot(
                Request::get("/.well-known/oauth-authorization-server")
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(metadata.status(), StatusCode::OK);
        let metadata: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(metadata.into_body(), 16 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(metadata["issuer"], issuer);
        assert_eq!(metadata["grant_types_supported"][1], "refresh_token");
        assert_eq!(
            metadata["authorization_response_iss_parameter_supported"],
            true
        );
        assert_eq!(metadata["code_challenge_methods_supported"][0], "S256");
        assert_eq!(
            metadata["scopes_supported"],
            serde_json::json!([
                "usage:read",
                "requests:read",
                "payloads:read",
                "keys:read",
                "offline_access"
            ])
        );

        let protected = app
            .clone()
            .oneshot(
                Request::get("/.well-known/oauth-protected-resource/mcp")
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let protected: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(protected.into_body(), 16 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(protected["resource"], resource);
        assert_eq!(protected["authorization_servers"][0], issuer);
        assert_eq!(
            protected["scopes_supported"],
            serde_json::json!(["usage:read", "requests:read", "payloads:read", "keys:read"])
        );

        let registration = app
            .clone()
            .oneshot(
                Request::post("/oauth/register")
                    .header(header::HOST, host)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"redirect_uris":["http://127.0.0.1:49152/callback"],"client_name":"CLI","client_uri":"https://github.com/badlogic/pi-mono","logo_uri":"https://example.com/logo.png","token_endpoint_auth_method":"none","grant_types":["authorization_code","refresh_token"],"response_types":["code"]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(registration.status(), StatusCode::CREATED);
        let registration: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(registration.into_body(), 16 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(registration.get("client_secret").is_none());
        assert_eq!(registration["token_endpoint_auth_method"], "none");
        assert_eq!(
            registration["scope"],
            "usage:read requests:read payloads:read keys:read offline_access"
        );
        let client_id = registration["client_id"].as_str().unwrap();

        let verifier = "correct-verifier-with-at-least-forty-three-characters-123";
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut authorize_url = url::Url::parse(&format!("{issuer}/oauth/authorize")).unwrap();
        authorize_url
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", "http://127.0.0.1:49152/callback")
            .append_pair("state", "opaque-state")
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("resource", resource);
        let path = authorize_url[url::Position::BeforePath..].to_owned();

        let challenged = app
            .clone()
            .oneshot(
                Request::get(&path)
                    .header(header::HOST, host)
                    .header(header::COOKIE, "__Host-aegis_session=ignored")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(challenged.status(), StatusCode::SEE_OTHER);
        assert_eq!(challenged.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(challenged.headers()[header::PRAGMA], "no-cache");
        assert!(!challenged.headers().contains_key(header::WWW_AUTHENTICATE));
        let login_location = challenged.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .to_owned();
        let login_url = url::Url::parse(&format!("{issuer}{login_location}")).unwrap();
        let login_parameters: std::collections::HashMap<_, _> =
            login_url.query_pairs().into_owned().collect();
        assert_eq!(login_parameters["return_to"], path);
        let invalid_client = app
            .clone()
            .oneshot(
                Request::get(path.replace(client_id, "00000000-0000-0000-0000-000000000000"))
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid_client.status(), StatusCode::BAD_REQUEST);
        assert!(
            !invalid_client
                .headers()
                .contains_key(header::WWW_AUTHENTICATE)
        );
        assert!(!invalid_client.headers().contains_key(header::LOCATION));

        let mut invalid_redirect_url =
            url::Url::parse(&format!("{issuer}/oauth/authorize")).unwrap();
        invalid_redirect_url
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", "http://127.0.0.1:49152/other")
            .append_pair("scope", "usage:read offline_access")
            .append_pair("state", "opaque-state")
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("resource", resource);
        let invalid_redirect = app
            .clone()
            .oneshot(
                Request::get(&invalid_redirect_url[url::Position::BeforePath..])
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid_redirect.status(), StatusCode::BAD_REQUEST);
        assert!(
            !invalid_redirect
                .headers()
                .contains_key(header::WWW_AUTHENTICATE)
        );
        assert!(!invalid_redirect.headers().contains_key(header::LOCATION));

        let invalid_response = app
            .clone()
            .oneshot(
                Request::get(path.replace("response_type=code", "response_type=token"))
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid_response.status(), StatusCode::SEE_OTHER);
        assert!(
            !invalid_response
                .headers()
                .contains_key(header::WWW_AUTHENTICATE)
        );
        let invalid_location = url::Url::parse(
            invalid_response.headers()[header::LOCATION]
                .to_str()
                .unwrap(),
        )
        .unwrap();
        let invalid_parameters: std::collections::HashMap<_, _> =
            invalid_location.query_pairs().into_owned().collect();
        assert_eq!(invalid_parameters["error"], "unsupported_response_type");
        assert_eq!(invalid_parameters["state"], "opaque-state");
        assert_eq!(invalid_parameters["iss"], issuer);

        let registration_b = app
            .clone()
            .oneshot(
                Request::post("/oauth/register")
                    .header(header::HOST, host)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"redirect_uris":["com.example.clientb:/callback"],"client_name":"CLI B","application_type":"native","grant_types":["authorization_code"],"response_types":["code"],"scope":"usage:read"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(registration_b.status(), StatusCode::CREATED);
        let registration_b: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(registration_b.into_body(), 16 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            registration_b["grant_types"],
            serde_json::json!(["authorization_code"])
        );
        assert_eq!(registration_b["scope"], "usage:read");
        assert_eq!(registration_b["application_type"], "native");
        let client_b = registration_b["client_id"].as_str().unwrap();

        let mut escalation = url::Url::parse(&format!("{issuer}/oauth/authorize")).unwrap();
        escalation
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", client_b)
            .append_pair("redirect_uri", "com.example.clientb:/callback")
            .append_pair("scope", "usage:read requests:read")
            .append_pair("state", "escalation-state")
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("resource", resource);
        let escalation_response = app
            .clone()
            .oneshot(
                Request::get(&escalation[url::Position::BeforePath..])
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(escalation_response.status(), StatusCode::SEE_OTHER);
        assert!(
            !escalation_response
                .headers()
                .contains_key(header::WWW_AUTHENTICATE)
        );
        let escalation_location = url::Url::parse(
            escalation_response.headers()[header::LOCATION]
                .to_str()
                .unwrap(),
        )
        .unwrap();
        let escalation_parameters: std::collections::HashMap<_, _> =
            escalation_location.query_pairs().into_owned().collect();
        assert_eq!(escalation_parameters["error"], "invalid_scope");
        assert_eq!(escalation_parameters["state"], "escalation-state");

        let login_page = app
            .clone()
            .oneshot(
                Request::get(&login_location)
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login_page.status(), StatusCode::OK);
        let login_csrf_cookie = login_page
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find(|value| value.starts_with("__Host-aegis_csrf="))
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let csrf_value = login_csrf_cookie.split_once('=').unwrap().1;
        let login_body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("email", "admin@example.com")
            .append_pair("password", "correct passw0rd!")
            .append_pair("csrf", csrf_value)
            .append_pair("return_to", &path)
            .finish();
        let login = app
            .clone()
            .oneshot(
                Request::post("/login")
                    .header(header::HOST, host)
                    .header(header::ORIGIN, issuer)
                    .header(header::COOKIE, &login_csrf_cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(login_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::SEE_OTHER);
        assert_eq!(login.headers()[header::LOCATION], path);
        let browser_cookies = login
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|value| value.to_str().unwrap().split(';').next().unwrap())
            .collect::<Vec<_>>()
            .join("; ");
        let csrf_value = browser_cookies
            .split("; ")
            .find_map(|pair| pair.strip_prefix("__Host-aegis_csrf="))
            .unwrap();

        let consent = app
            .clone()
            .oneshot(
                Request::get(&path)
                    .header(header::HOST, host)
                    .header(header::COOKIE, &browser_cookies)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(consent.status(), StatusCode::OK);
        assert!(!consent.headers().contains_key(header::WWW_AUTHENTICATE));
        assert_eq!(consent.headers()["x-frame-options"], "DENY");
        assert!(
            consent.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("frame-ancestors 'none'")
        );
        let consent_body = axum::body::to_bytes(consent.into_body(), 32 * 1024)
            .await
            .unwrap();
        let consent_body = std::str::from_utf8(&consent_body).unwrap();
        assert!(consent_body.contains("Authorize access"));
        assert!(consent_body.contains("CLI"));
        assert!(consent_body.contains(client_id));
        assert!(consent_body.contains("requests:read"));
        assert!(consent_body.contains("Read request records"));
        assert!(consent_body.contains("payloads:read"));
        assert!(consent_body.contains("Read captured payloads"));
        assert!(consent_body.contains("Stay connected"));
        assert!(consent_body.contains("Read API key metadata"));
        assert!(consent_body.contains("http://127.0.0.1:49152/callback"));

        let malformed_consent = app
            .clone()
            .oneshot(
                Request::post(&path)
                    .header(header::HOST, host)
                    .header(header::ORIGIN, issuer)
                    .header(header::COOKIE, &browser_cookies)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!("csrf={csrf_value}")))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(malformed_consent.status(), StatusCode::SEE_OTHER);
        let malformed_location = url::Url::parse(
            malformed_consent.headers()[header::LOCATION]
                .to_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            malformed_location
                .query_pairs()
                .find(|(key, _)| key == "error")
                .unwrap()
                .1,
            "invalid_request"
        );

        let decision = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("csrf", csrf_value)
            .append_pair("decision", "allow")
            .finish();
        let authorized = app
            .clone()
            .oneshot(
                Request::post(&path)
                    .header(header::HOST, host)
                    .header(header::ORIGIN, issuer)
                    .header(header::COOKIE, &browser_cookies)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(decision))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authorized.status(), StatusCode::SEE_OTHER);
        let redirect =
            url::Url::parse(authorized.headers()[header::LOCATION].to_str().unwrap()).unwrap();
        assert_eq!(
            redirect.as_str().split('?').next().unwrap(),
            "http://127.0.0.1:49152/callback"
        );
        let response_params: std::collections::HashMap<_, _> =
            redirect.query_pairs().into_owned().collect();
        assert_eq!(response_params["state"], "opaque-state");
        assert_eq!(response_params["iss"], issuer);
        let code = &response_params["code"];

        let token_body = |resource_value: &str| {
            url::form_urlencoded::Serializer::new(String::new())
                .append_pair("grant_type", "authorization_code")
                .append_pair("code", code)
                .append_pair("client_id", client_id)
                .append_pair("redirect_uri", "http://127.0.0.1:49152/callback")
                .append_pair("code_verifier", verifier)
                .append_pair("resource", resource_value)
                .finish()
        };
        let unsupported_client_auth = app
            .clone()
            .oneshot(
                Request::post("/oauth/token")
                    .header(header::HOST, host)
                    .header(header::AUTHORIZATION, "bAsIc Y2xpZW50OnNlY3JldA==")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(token_body(resource)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unsupported_client_auth.status(), StatusCode::UNAUTHORIZED);
        assert!(
            unsupported_client_auth.headers()[header::WWW_AUTHENTICATE]
                .to_str()
                .unwrap()
                .to_ascii_lowercase()
                .starts_with("basic ")
        );

        let duplicate_client = app
            .clone()
            .oneshot(
                Request::post("/oauth/token")
                    .header(header::HOST, host)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "{}&client_id={client_id}",
                        token_body(resource)
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(duplicate_client.status(), StatusCode::BAD_REQUEST);

        let wrong_resource = app
            .clone()
            .oneshot(
                Request::post("/oauth/token")
                    .header(header::HOST, host)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(token_body("http://127.0.0.1:3000/v1")))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong_resource.status(), StatusCode::BAD_REQUEST);

        let issued = app
            .clone()
            .oneshot(
                Request::post("/oauth/token")
                    .header(header::HOST, host)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(token_body(resource)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(issued.status(), StatusCode::OK);
        let issued: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(issued.into_body(), 16 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(issued["token_type"], "Bearer");
        assert_eq!(issued["expires_in"], 3600);
        let refresh = issued["refresh_token"].as_str().unwrap();

        let refresh_body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "refresh_token")
            .append_pair("refresh_token", refresh)
            .append_pair("client_id", client_id)
            .append_pair("resource", resource)
            .finish();
        let rotated = app
            .clone()
            .oneshot(
                Request::post("/oauth/token")
                    .header(header::HOST, host)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(refresh_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rotated.status(), StatusCode::OK);

        let replay = app
            .oneshot(
                Request::post("/oauth/token")
                    .header(header::HOST, host)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(token_body(resource)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn oauth_device_http_flow_registers_verifies_approves_denies_and_issues_tokens() {
        let app = application().await;
        let host = "127.0.0.1:3000";
        let issuer = "https://127.0.0.1:3000";
        let resource = "https://127.0.0.1:3000/mcp";

        let metadata = app
            .clone()
            .oneshot(
                Request::get("/.well-known/oauth-authorization-server")
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let metadata: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(metadata.into_body(), 16 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            metadata["device_authorization_endpoint"],
            format!("{issuer}/oauth/device_authorization")
        );
        assert!(
            metadata["grant_types_supported"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == crate::domain::DEVICE_GRANT_TYPE)
        );

        let registration = app.clone().oneshot(
            Request::post("/oauth/register").header(header::HOST, host).header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(r#"{{"client_name":"Living room","grant_types":["{}","refresh_token"],"response_types":[],"scope":"usage:read offline_access","unknown_metadata":"tolerated"}}"#, crate::domain::DEVICE_GRANT_TYPE))).unwrap()
        ).await.unwrap();
        assert_eq!(registration.status(), StatusCode::CREATED);
        let registration: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(registration.into_body(), 16 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(registration["redirect_uris"], serde_json::json!([]));
        assert_eq!(registration["response_types"], serde_json::json!([]));
        let client_id = registration["client_id"].as_str().unwrap().to_owned();

        let issue = |client_id: &str| {
            url::form_urlencoded::Serializer::new(String::new())
                .append_pair("client_id", client_id)
                .append_pair("resource", resource)
                .finish()
        };
        let issued = app
            .clone()
            .oneshot(
                Request::post("/oauth/device_authorization")
                    .header(header::HOST, host)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(issue(&client_id)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(issued.status(), StatusCode::OK);
        assert_eq!(issued.headers()[header::CACHE_CONTROL], "no-store");
        let issued: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(issued.into_body(), 16 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(issued["expires_in"], 600);
        assert_eq!(issued["interval"], 5);
        let device_code = issued["device_code"].as_str().unwrap().to_owned();
        let user_code = issued["user_code"].as_str().unwrap().to_owned();
        let complete =
            url::Url::parse(issued["verification_uri_complete"].as_str().unwrap()).unwrap();
        let complete_path = complete[url::Position::BeforePath..].to_owned();

        let entry = app
            .clone()
            .oneshot(
                Request::get("/oauth/device")
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(entry.status(), StatusCode::OK);
        let entry_cookie = entry.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let entry_csrf = entry_cookie.split_once('=').unwrap().1;
        let entry_form = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("csrf", entry_csrf)
            .append_pair("user_code", &user_code.to_ascii_lowercase())
            .finish();
        let entered = app
            .clone()
            .oneshot(
                Request::post("/oauth/device")
                    .header(header::HOST, host)
                    .header(header::COOKIE, &entry_cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(entry_form))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(entered.status(), StatusCode::SEE_OTHER);
        assert_eq!(entered.headers()[header::LOCATION], complete_path);

        let challenged = app
            .clone()
            .oneshot(
                Request::get(&complete_path)
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(challenged.status(), StatusCode::SEE_OTHER);
        assert!(
            challenged.headers()[header::LOCATION]
                .to_str()
                .unwrap()
                .starts_with("/login?")
        );

        let (browser_cookies, csrf) =
            sign_in(&app, host, "admin@example.com", "correct passw0rd!").await;
        let csrf = csrf.as_str();

        let consent = app
            .clone()
            .oneshot(
                Request::get(&complete_path)
                    .header(header::HOST, host)
                    .header(header::COOKIE, &browser_cookies)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(consent.status(), StatusCode::OK);
        let consent = axum::body::to_bytes(consent.into_body(), 32 * 1024)
            .await
            .unwrap();
        let consent = std::str::from_utf8(&consent).unwrap();
        assert!(consent.contains("Living room"));
        assert!(consent.contains("Read usage totals"));
        assert!(consent.contains("Stay connected"));
        assert!(!consent.contains("Read captured payloads"));
        assert!(consent.contains(resource));

        let allow = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("csrf", csrf)
            .append_pair("decision", "allow")
            .finish();
        let approved = app
            .clone()
            .oneshot(
                Request::post(&complete_path)
                    .header(header::HOST, host)
                    .header(header::COOKIE, &browser_cookies)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(allow))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(approved.status(), StatusCode::OK);
        let approved = axum::body::to_bytes(approved.into_body(), 16 * 1024)
            .await
            .unwrap();
        assert!(
            std::str::from_utf8(&approved)
                .unwrap()
                .contains("Device connected")
        );

        let poll = |code: &str| {
            url::form_urlencoded::Serializer::new(String::new())
                .append_pair("grant_type", crate::domain::DEVICE_GRANT_TYPE)
                .append_pair("client_id", &client_id)
                .append_pair("device_code", code)
                .finish()
        };
        let token = app
            .clone()
            .oneshot(
                Request::post("/oauth/token")
                    .header(header::HOST, host)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(poll(&device_code)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(token.status(), StatusCode::OK);
        let token: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(token.into_body(), 16 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(
            token["access_token"]
                .as_str()
                .unwrap()
                .starts_with("ag_at1.")
        );
        assert!(
            token["refresh_token"]
                .as_str()
                .unwrap()
                .starts_with("ag_rt1.")
        );
        let replay = app
            .clone()
            .oneshot(
                Request::post("/oauth/token")
                    .header(header::HOST, host)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(poll(&device_code)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
        let replay: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(replay.into_body(), 4096)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(replay["error"], "invalid_grant");

        let denied = app
            .clone()
            .oneshot(
                Request::post("/oauth/device_authorization")
                    .header(header::HOST, host)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(issue(&client_id)))
                    .unwrap(),
            )
            .await
            .unwrap();
        let denied: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(denied.into_body(), 16 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let denied_code = denied["device_code"].as_str().unwrap().to_owned();
        let denied_url =
            url::Url::parse(denied["verification_uri_complete"].as_str().unwrap()).unwrap();
        let denied_path = denied_url[url::Position::BeforePath..].to_owned();
        let deny = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("csrf", csrf)
            .append_pair("decision", "deny")
            .finish();
        let denied_decision = app
            .clone()
            .oneshot(
                Request::post(&denied_path)
                    .header(header::HOST, host)
                    .header(header::COOKIE, &browser_cookies)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(deny))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied_decision.status(), StatusCode::OK);
        let denied_poll = app
            .oneshot(
                Request::post("/oauth/token")
                    .header(header::HOST, host)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(poll(&denied_code)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied_poll.status(), StatusCode::BAD_REQUEST);
        let denied_poll: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(denied_poll.into_body(), 4096)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(denied_poll["error"], "access_denied");
    }

    #[tokio::test]
    async fn account_page_needs_a_session_and_changes_the_password() {
        let app = application().await;
        let host = "127.0.0.1:3000";

        let (status, location, _) = get_account(&app, host, "").await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(location, "/login");

        let page = app
            .clone()
            .oneshot(
                Request::get("/login")
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let stray_pair = page.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let stray_csrf = stray_pair.split_once('=').unwrap().1.to_owned();
        for path in [
            "/account/password",
            "/account/sessions/00000000-0000-0000-0000-000000000000/revoke",
            "/account/apps/00000000-0000-0000-0000-000000000000/revoke",
        ] {
            let body = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("csrf", &stray_csrf)
                .append_pair("current_password", "correct passw0rd!")
                .append_pair("new_password", "second passw0rd!")
                .finish();
            let response = app
                .clone()
                .oneshot(form_post(path, host, &stray_pair, body))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SEE_OTHER);
            assert_eq!(response.headers()[header::LOCATION], "/login");
        }

        let (first, first_csrf) =
            sign_in(&app, host, "admin@example.com", "correct passw0rd!").await;
        let (second, _) = sign_in(&app, host, "admin@example.com", "correct passw0rd!").await;

        let (status, _, html) = get_account(&app, host, &first).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Change password"));
        assert!(html.contains("Active sessions"));
        assert!(html.contains("Connected apps"));
        assert!(html.contains(r#"form method="post" action="/account/password""#));
        assert!(html.contains(&format!(
            r#"input type="hidden" name="csrf" value="{first_csrf}""#
        )));

        let change = |current: &str, new: &str| {
            let body = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("csrf", &first_csrf)
                .append_pair("current_password", current)
                .append_pair("new_password", new)
                .finish();
            form_post("/account/password", host, &first, body)
        };

        let wrong = app
            .clone()
            .oneshot(change("wrong passw0rd!", "second passw0rd!"))
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(wrong.into_body(), 128 * 1024)
            .await
            .unwrap();
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains("Your current password is not correct.")
        );

        let weak = app
            .clone()
            .oneshot(change("correct passw0rd!", "weakpassword"))
            .await
            .unwrap();
        assert_eq!(weak.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = axum::body::to_bytes(weak.into_body(), 128 * 1024)
            .await
            .unwrap();
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains("at least 8 characters")
        );

        let changed = app
            .clone()
            .oneshot(change("correct passw0rd!", "second passw0rd!"))
            .await
            .unwrap();
        assert_eq!(changed.status(), StatusCode::SEE_OTHER);
        assert_eq!(changed.headers()[header::LOCATION], "/account?changed=1");

        let notice = app
            .clone()
            .oneshot(
                Request::get("/account?changed=1")
                    .header(header::HOST, host)
                    .header(header::COOKIE, &first)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(notice.status(), StatusCode::OK);
        let body = axum::body::to_bytes(notice.into_body(), 128 * 1024)
            .await
            .unwrap();
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains("Your password changed. Your other sessions ended.")
        );

        let (status, _, _) = get_account(&app, host, &first).await;
        assert_eq!(status, StatusCode::OK);

        let (status, location, _) = get_account(&app, host, &second).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(location, "/login");

        let (third, _) = sign_in(&app, host, "admin@example.com", "second passw0rd!").await;
        let (status, _, _) = get_account(&app, host, &third).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn account_sessions_list_and_revoke_only_the_own_account() {
        let host = "127.0.0.1:3000";
        let (app, mailer) = account_application(true).await;

        let create_user = |email: &'static str| {
            let app = app.clone();
            let mailer = mailer.clone();
            async move {
                let page = app
                    .clone()
                    .oneshot(
                        Request::get("/register")
                            .header(header::HOST, host)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let csrf_pair = page.headers()[header::SET_COOKIE]
                    .to_str()
                    .unwrap()
                    .split(';')
                    .next()
                    .unwrap()
                    .to_owned();
                let csrf_value = csrf_pair.split_once('=').unwrap().1.to_owned();
                let body = url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("email", email)
                    .append_pair("password", "passw0rd!")
                    .append_pair("csrf", &csrf_value)
                    .finish();
                let created = app
                    .clone()
                    .oneshot(form_post("/register", host, &csrf_pair, body))
                    .await
                    .unwrap();
                assert_eq!(created.status(), StatusCode::OK);
                let code = crate::domain::extract_code(&mailer.take()[0].body);
                let body = url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("email", email)
                    .append_pair("code", &code)
                    .append_pair("csrf", &csrf_value)
                    .finish();
                let verified = app
                    .oneshot(form_post("/verify", host, &csrf_pair, body))
                    .await
                    .unwrap();
                assert_eq!(verified.status(), StatusCode::SEE_OTHER);
            }
        };
        create_user("one@example.com").await;
        create_user("two@example.com").await;

        let firefox = "Mozilla/5.0 (X11; Linux x86_64; rv:130.0) Gecko/20100101 Firefox/130.0";
        let (first, first_csrf) =
            sign_in_with_agent(&app, host, "one@example.com", "passw0rd!", Some(firefox)).await;
        let (second, _) = sign_in_with_agent(
            &app,
            host,
            "one@example.com",
            "passw0rd!",
            Some("curl/8.9.1"),
        )
        .await;
        let (other, _) = sign_in(&app, host, "two@example.com", "passw0rd!").await;

        let (status, _, html) = get_account(&app, host, &first).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Firefox on Linux"));
        assert!(html.contains(r#"<span class="status-tag">This session</span>"#));
        assert!(html.contains("curl"));

        let session_id = |html: &str, marker: &str| {
            html.split("<tr>")
                .find(|item| item.contains(marker))
                .unwrap()
                .split(r#"action="/account/sessions/"#)
                .nth(1)
                .unwrap()
                .split('/')
                .next()
                .unwrap()
                .to_owned()
        };
        let other_id = session_id(&html, "curl");
        let current_id = session_id(&html, "This session");

        let (_, _, other_html) = get_account(&app, host, &other).await;
        assert!(other_html.contains("Unknown client"));
        let foreign_id = session_id(&other_html, "This session");
        let revoke = |id: &str, cookies: &str, csrf: &str| {
            let body = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("csrf", csrf)
                .finish();
            form_post(
                &format!("/account/sessions/{id}/revoke"),
                host,
                cookies,
                body,
            )
        };

        let foreign = app
            .clone()
            .oneshot(revoke(&foreign_id, &first, &first_csrf))
            .await
            .unwrap();
        assert_eq!(foreign.status(), StatusCode::NOT_FOUND);
        let (status, _, _) = get_account(&app, host, &other).await;
        assert_eq!(status, StatusCode::OK);

        let revoked = app
            .clone()
            .oneshot(revoke(&other_id, &first, &first_csrf))
            .await
            .unwrap();
        assert_eq!(revoked.status(), StatusCode::SEE_OTHER);
        assert_eq!(revoked.headers()[header::LOCATION], "/account");
        let (status, location, _) = get_account(&app, host, &second).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(location, "/login");

        let repeated = app
            .clone()
            .oneshot(revoke(&other_id, &first, &first_csrf))
            .await
            .unwrap();
        assert_eq!(repeated.status(), StatusCode::NOT_FOUND);

        let signed_out = app
            .clone()
            .oneshot(revoke(&current_id, &first, &first_csrf))
            .await
            .unwrap();
        assert_eq!(signed_out.status(), StatusCode::SEE_OTHER);
        assert_eq!(signed_out.headers()[header::LOCATION], "/login");
        let (status, location, _) = get_account(&app, host, &first).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(location, "/login");
    }

    #[tokio::test]
    async fn account_page_signs_the_user_out() {
        let host = "127.0.0.1:3000";
        let app = application().await;
        let (cookies, csrf) = sign_in(&app, host, "admin@example.com", "correct passw0rd!").await;

        let (status, _, html) = get_account(&app, host, &cookies).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains(r#"class="nav-icon" href="/account""#));
        assert!(html.contains(r#"form method="post" action="/logout""#));

        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("csrf", &csrf)
            .finish();
        let signed_out = app
            .clone()
            .oneshot(form_post("/logout", host, &cookies, body))
            .await
            .unwrap();
        assert_eq!(signed_out.status(), StatusCode::SEE_OTHER);
        assert_eq!(signed_out.headers()[header::LOCATION], "/");

        let (status, location, _) = get_account(&app, host, &cookies).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(location, "/login");
    }

    async fn consent_token(
        app: &Router,
        host: &str,
        issuer: &str,
        cookies: &str,
        csrf: &str,
        client_scope: &str,
    ) -> (String, serde_json::Value) {
        let resource = format!("{issuer}/mcp");
        let registration = app
            .clone()
            .oneshot(
                Request::post("/oauth/register")
                    .header(header::HOST, host)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r#"{{"redirect_uris":["http://127.0.0.1:49152/callback"],"client_name":"Studio","grant_types":["authorization_code","refresh_token"],"response_types":["code"],"scope":"{client_scope}"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(registration.status(), StatusCode::CREATED);
        let registration: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(registration.into_body(), 16 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let client_id = registration["client_id"].as_str().unwrap().to_owned();

        let verifier = "correct-verifier-with-at-least-forty-three-characters-123";
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut authorize_url = url::Url::parse(&format!("{issuer}/oauth/authorize")).unwrap();
        authorize_url
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", "http://127.0.0.1:49152/callback")
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("resource", &resource);
        let path = authorize_url[url::Position::BeforePath..].to_owned();
        let consent = app
            .clone()
            .oneshot(
                Request::get(&path)
                    .header(header::HOST, host)
                    .header(header::COOKIE, cookies)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(consent.status(), StatusCode::OK);
        let consent = axum::body::to_bytes(consent.into_body(), 64 * 1024)
            .await
            .unwrap();
        let consent = std::str::from_utf8(&consent).unwrap().to_owned();

        let mut decision = url::form_urlencoded::Serializer::new(String::new());
        decision
            .append_pair("csrf", csrf)
            .append_pair("decision", "allow");
        let authorized = app
            .clone()
            .oneshot(form_post(&path, host, cookies, decision.finish()))
            .await
            .unwrap();
        assert_eq!(authorized.status(), StatusCode::SEE_OTHER);
        let redirect =
            url::Url::parse(authorized.headers()[header::LOCATION].to_str().unwrap()).unwrap();
        let parameters: std::collections::HashMap<_, _> =
            redirect.query_pairs().into_owned().collect();
        let token_body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", &parameters["code"])
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", "http://127.0.0.1:49152/callback")
            .append_pair("code_verifier", verifier)
            .append_pair("resource", &resource)
            .finish();
        let issued = app
            .clone()
            .oneshot(
                Request::post("/oauth/token")
                    .header(header::HOST, host)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(token_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(issued.status(), StatusCode::OK);
        let issued: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(issued.into_body(), 16 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        (consent, issued)
    }

    async fn register_user(
        app: &Router,
        host: &str,
        mailer: &Arc<crate::domain::CapturingMailer>,
        email: &str,
    ) {
        let page = app
            .clone()
            .oneshot(
                Request::get("/register")
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let csrf_pair = page.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let csrf_value = csrf_pair.split_once('=').unwrap().1.to_owned();
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("email", email)
            .append_pair("password", "passw0rd!")
            .append_pair("csrf", &csrf_value)
            .finish();
        let created = app
            .clone()
            .oneshot(form_post("/register", host, &csrf_pair, body))
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::OK);
        let code = crate::domain::extract_code(&mailer.take()[0].body);
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("email", email)
            .append_pair("code", &code)
            .append_pair("csrf", &csrf_value)
            .finish();
        let verified = app
            .clone()
            .oneshot(form_post("/verify", host, &csrf_pair, body))
            .await
            .unwrap();
        assert_eq!(verified.status(), StatusCode::SEE_OTHER);
    }

    #[tokio::test]
    async fn consent_switch_signs_out_and_grants_to_the_next_account() {
        let host = "127.0.0.1:3000";
        let issuer = "http://127.0.0.1:3000";
        let resource = format!("{issuer}/mcp");
        let (app, mailer) = account_application(true).await;
        register_user(&app, host, &mailer, "one@example.com").await;
        register_user(&app, host, &mailer, "two@example.com").await;

        let registration = app
            .clone()
            .oneshot(
                Request::post("/oauth/register")
                    .header(header::HOST, host)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"redirect_uris":["http://127.0.0.1:49152/callback"],"client_name":"Studio","grant_types":["authorization_code"],"response_types":["code"],"scope":"usage:read requests:read"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(registration.status(), StatusCode::CREATED);
        let registration: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(registration.into_body(), 16 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let client_id = registration["client_id"].as_str().unwrap().to_owned();

        let verifier = "correct-verifier-with-at-least-forty-three-characters-123";
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut authorize_url = url::Url::parse(&format!("{issuer}/oauth/authorize")).unwrap();
        authorize_url
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", "http://127.0.0.1:49152/callback")
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("resource", &resource);
        let path = authorize_url[url::Position::BeforePath..].to_owned();
        let consent_page = |cookies: String| {
            let app = app.clone();
            let path = path.clone();
            async move {
                let response = app
                    .oneshot(
                        Request::get(&path)
                            .header(header::HOST, host)
                            .header(header::COOKIE, cookies)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let status = response.status();
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .map(|value| value.to_str().unwrap().to_owned())
                    .unwrap_or_default();
                let body = axum::body::to_bytes(response.into_body(), 128 * 1024)
                    .await
                    .unwrap();
                (status, location, String::from_utf8(body.to_vec()).unwrap())
            }
        };

        let (first, first_csrf) = sign_in(&app, host, "one@example.com", "passw0rd!").await;
        let (status, _, html) = consent_page(first.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("one@example.com"));

        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("csrf", &first_csrf)
            .append_pair("return_to", &path)
            .finish();
        let switched = app
            .clone()
            .oneshot(form_post("/logout", host, &first, body))
            .await
            .unwrap();
        assert_eq!(switched.status(), StatusCode::SEE_OTHER);
        let expected = format!(
            "/login?{}",
            url::form_urlencoded::Serializer::new(String::new())
                .append_pair("return_to", &path)
                .finish()
        );
        assert_eq!(switched.headers()[header::LOCATION], expected);

        let (status, location, _) = consent_page(first).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(location, expected);

        let (second, second_csrf) = sign_in(&app, host, "two@example.com", "passw0rd!").await;
        let (status, _, html) = consent_page(second.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("two@example.com"));
        assert!(!html.contains("one@example.com"));

        let decision = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("csrf", &second_csrf)
            .append_pair("decision", "allow")
            .append_pair("scope", "usage:read")
            .finish();
        let authorized = app
            .clone()
            .oneshot(form_post(&path, host, &second, decision))
            .await
            .unwrap();
        assert_eq!(authorized.status(), StatusCode::SEE_OTHER);
        let redirect =
            url::Url::parse(authorized.headers()[header::LOCATION].to_str().unwrap()).unwrap();
        let parameters: std::collections::HashMap<_, _> =
            redirect.query_pairs().into_owned().collect();
        let token_body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", &parameters["code"])
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", "http://127.0.0.1:49152/callback")
            .append_pair("code_verifier", verifier)
            .append_pair("resource", &resource)
            .finish();
        let issued = app
            .clone()
            .oneshot(
                Request::post("/oauth/token")
                    .header(header::HOST, host)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(token_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(issued.status(), StatusCode::OK);

        let (status, _, html) = get_account(&app, host, &second).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Studio"));
        let (again, _) = sign_in(&app, host, "one@example.com", "passw0rd!").await;
        let (status, _, html) = get_account(&app, host, &again).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!html.contains("Studio"));
        assert!(html.contains("No application is connected."));
    }

    #[tokio::test]
    async fn logout_ignores_a_return_to_that_leaves_the_site() {
        let app = application_with_policy(OriginPolicy::new(None)).await;
        let host = "127.0.0.1:3000";
        let (cookies, csrf) = sign_in(&app, host, "admin@example.com", "correct passw0rd!").await;
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("csrf", &csrf)
            .append_pair("return_to", "https://evil.example/oauth/authorize?x=1")
            .finish();
        let switched = app
            .clone()
            .oneshot(form_post("/logout", host, &cookies, body))
            .await
            .unwrap();
        assert_eq!(switched.status(), StatusCode::SEE_OTHER);
        assert_eq!(switched.headers()[header::LOCATION], "/");
    }

    #[tokio::test]
    async fn consent_grants_every_requested_scope_to_a_superuser() {
        let (app, _domain) =
            application_parts(OriginPolicy::new(Some("https://127.0.0.1:3000".into()))).await;
        let host = "127.0.0.1:3000";
        let (cookies, csrf) = sign_in(&app, host, "admin@example.com", "correct passw0rd!").await;
        let (consent, issued) = consent_token(
            &app,
            host,
            "https://127.0.0.1:3000",
            &cookies,
            &csrf,
            "usage:read requests:read keys:read payloads:read offline_access",
        )
        .await;
        assert!(consent.contains("Read request records"));
        assert!(consent.contains("Read captured payloads"));
        assert!(consent.contains("Stay connected"));
        assert!(consent.contains("Read usage totals"));
        assert_eq!(
            issued["scope"],
            "usage:read requests:read payloads:read keys:read offline_access"
        );
        assert!(issued["refresh_token"].is_string());
    }

    #[tokio::test]
    async fn a_read_and_a_write_are_granted_side_by_side() {
        let (app, _domain) =
            application_parts(OriginPolicy::new(Some("https://127.0.0.1:3000".into()))).await;
        let host = "127.0.0.1:3000";
        let (cookies, csrf) = sign_in(&app, host, "admin@example.com", "correct passw0rd!").await;
        let (consent, issued) = consent_token(
            &app,
            host,
            "https://127.0.0.1:3000",
            &cookies,
            &csrf,
            "usage:read requests:read",
        )
        .await;
        assert!(consent.contains("Read request records"));
        assert!(!consent.contains("Stay connected"));
        assert_eq!(issued["scope"], "usage:read requests:read");
        assert!(issued["refresh_token"].is_null());
    }

    #[tokio::test]
    async fn consent_drops_a_payload_scope_for_a_regular_user() {
        let host = "127.0.0.1:3000";
        let (app, mailer) = account_application(true).await;
        let page = app
            .clone()
            .oneshot(
                Request::get("/register")
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let csrf_pair = page.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let csrf_value = csrf_pair.split_once('=').unwrap().1.to_owned();
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("email", "member@example.com")
            .append_pair("password", "passw0rd!")
            .append_pair("csrf", &csrf_value)
            .finish();
        assert_eq!(
            app.clone()
                .oneshot(form_post("/register", host, &csrf_pair, body))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let code = crate::domain::extract_code(&mailer.take()[0].body);
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("email", "member@example.com")
            .append_pair("code", &code)
            .append_pair("csrf", &csrf_value)
            .finish();
        assert_eq!(
            app.clone()
                .oneshot(form_post("/verify", host, &csrf_pair, body))
                .await
                .unwrap()
                .status(),
            StatusCode::SEE_OTHER
        );

        let (cookies, csrf) = sign_in(&app, host, "member@example.com", "passw0rd!").await;
        let (consent, issued) = consent_token(
            &app,
            host,
            "http://127.0.0.1:3000",
            &cookies,
            &csrf,
            "usage:read requests:read keys:read payloads:read offline_access",
        )
        .await;
        assert!(!consent.contains("Read captured payloads"));
        assert!(consent.contains("Read API key metadata"));
        assert!(consent.contains("Read request records"));
        assert_eq!(
            issued["scope"],
            "usage:read requests:read keys:read offline_access"
        );
        assert!(issued["refresh_token"].is_string());
    }

    #[tokio::test]
    async fn account_lists_a_connected_app_and_disconnects_it() {
        let (app, domain) =
            application_parts(OriginPolicy::new(Some("https://127.0.0.1:3000".into()))).await;
        let host = "127.0.0.1:3000";
        let issuer = "https://127.0.0.1:3000";
        let resource = "https://127.0.0.1:3000/mcp";

        let registration = app
            .clone()
            .oneshot(
                Request::post("/oauth/register")
                    .header(header::HOST, host)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"redirect_uris":["http://127.0.0.1:49152/callback"],"client_name":"Studio","token_endpoint_auth_method":"none","grant_types":["authorization_code","refresh_token"],"response_types":["code"]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(registration.status(), StatusCode::CREATED);
        let registration: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(registration.into_body(), 16 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let client_id = registration["client_id"].as_str().unwrap().to_owned();

        let (cookies, csrf) = sign_in(&app, host, "admin@example.com", "correct passw0rd!").await;

        let verifier = "correct-verifier-with-at-least-forty-three-characters-123";
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut authorize_url = url::Url::parse(&format!("{issuer}/oauth/authorize")).unwrap();
        authorize_url
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", "http://127.0.0.1:49152/callback")
            .append_pair("state", "opaque-state")
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("resource", resource);
        let path = authorize_url[url::Position::BeforePath..].to_owned();
        let decision = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("csrf", &csrf)
            .append_pair("decision", "allow")
            .append_pair("scope", "usage:read")
            .append_pair("scope", "offline_access")
            .finish();
        let authorized = app
            .clone()
            .oneshot(form_post(&path, host, &cookies, decision))
            .await
            .unwrap();
        assert_eq!(authorized.status(), StatusCode::SEE_OTHER);
        let redirect =
            url::Url::parse(authorized.headers()[header::LOCATION].to_str().unwrap()).unwrap();
        let parameters: std::collections::HashMap<_, _> =
            redirect.query_pairs().into_owned().collect();
        let token_body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", &parameters["code"])
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", "http://127.0.0.1:49152/callback")
            .append_pair("code_verifier", verifier)
            .append_pair("resource", resource)
            .finish();
        let issued = app
            .clone()
            .oneshot(
                Request::post("/oauth/token")
                    .header(header::HOST, host)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(token_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(issued.status(), StatusCode::OK);
        let issued: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(issued.into_body(), 16 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let access_token = issued["access_token"].as_str().unwrap().to_owned();
        let refresh_token = issued["refresh_token"].as_str().unwrap().to_owned();
        assert!(
            domain
                .oauth()
                .authenticate_access_token(&access_token, issuer, resource)
                .await
                .is_ok()
        );

        let (status, _, html) = get_account(&app, host, &cookies).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Studio"));
        assert!(html.contains(&client_id));
        for text in [
            "requests:read",
            "Read request records",
            "offline_access",
            "Stay connected",
        ] {
            assert!(html.contains(text), "the card does not name {text}");
        }
        assert!(html.contains(&format!(r#"action="/account/apps/{client_id}/revoke""#)));

        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("csrf", &csrf)
            .finish();
        let disconnected = app
            .clone()
            .oneshot(form_post(
                &format!("/account/apps/{client_id}/revoke"),
                host,
                &cookies,
                body,
            ))
            .await
            .unwrap();
        assert_eq!(disconnected.status(), StatusCode::SEE_OTHER);
        assert_eq!(disconnected.headers()[header::LOCATION], "/account");

        let (status, _, html) = get_account(&app, host, &cookies).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!html.contains("Studio"));
        assert!(html.contains("No application is connected."));

        assert!(
            domain
                .oauth()
                .authenticate_access_token(&access_token, issuer, resource)
                .await
                .is_err()
        );
        let refresh_body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "refresh_token")
            .append_pair("refresh_token", &refresh_token)
            .append_pair("client_id", &client_id)
            .append_pair("resource", resource)
            .finish();
        let refused = app
            .clone()
            .oneshot(
                Request::post("/oauth/token")
                    .header(header::HOST, host)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(refresh_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);

        let repeated = app
            .clone()
            .oneshot(form_post(
                &format!("/account/apps/{client_id}/revoke"),
                host,
                &cookies,
                url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("csrf", &csrf)
                    .finish(),
            ))
            .await
            .unwrap();
        assert_eq!(repeated.status(), StatusCode::NOT_FOUND);
    }
}
