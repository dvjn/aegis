use std::collections::HashMap;

use axum::{
    Json,
    body::Bytes,
    extract::{RawQuery, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use axum_extra::extract::cookie::CookieJar;
use base64::Engine;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{
    domain::{
        AuthorizationCodeRedemption, AuthorizationCodeRequest, ClientRegistration,
        DEVICE_GRANT_TYPE, DeviceAuthorizationRequest, DevicePollError, DeviceTokenRequest,
        DomainError, IssuedAccessToken, RefreshTokenRequest, normalize_user_code,
    },
    oauth::State as AppState,
    web,
};

const TOKEN_BASIC_CHALLENGE: &str = "Basic realm=\"aegis OAuth token\", charset=\"UTF-8\"";
const MAX_PARAMETER_BYTES: usize = 4 * 1024;
const MAX_PARAMETERS: usize = 16;

#[derive(Serialize)]
struct OAuthError {
    error: &'static str,
}

fn error(status: StatusCode, code: &'static str) -> Response {
    (status, Json(OAuthError { error: code })).into_response()
}

fn error_with_challenge(
    status: StatusCode,
    code: &'static str,
    challenge: HeaderValue,
) -> Response {
    let mut response = error(status, code);
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, challenge);
    response
}

#[derive(Serialize)]
pub struct AuthorizationServerMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    device_authorization_endpoint: String,
    registration_endpoint: String,
    response_types_supported: [&'static str; 1],
    grant_types_supported: [&'static str; 3],
    token_endpoint_auth_methods_supported: [&'static str; 1],
    code_challenge_methods_supported: [&'static str; 1],
    scopes_supported: Vec<&'static str>,
    authorization_response_iss_parameter_supported: bool,
    resource_parameter_supported: bool,
}

pub async fn authorization_server_metadata(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let Ok(origin) = state.origin.effective_origin(&headers) else {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    Json(AuthorizationServerMetadata {
        issuer: origin.clone(),
        authorization_endpoint: format!("{origin}/oauth/authorize"),
        token_endpoint: format!("{origin}/oauth/token"),
        device_authorization_endpoint: format!("{origin}/oauth/device_authorization"),
        registration_endpoint: format!("{origin}/oauth/register"),
        response_types_supported: ["code"],
        grant_types_supported: ["authorization_code", "refresh_token", DEVICE_GRANT_TYPE],
        token_endpoint_auth_methods_supported: ["none"],
        code_challenge_methods_supported: ["S256"],
        scopes_supported: crate::domain::SCOPES.to_vec(),
        authorization_response_iss_parameter_supported: true,
        resource_parameter_supported: true,
    })
    .into_response()
}

#[derive(Serialize)]
pub struct ProtectedResourceMetadata {
    resource: String,
    authorization_servers: Vec<String>,
    scopes_supported: Vec<&'static str>,
    bearer_methods_supported: [&'static str; 1],
}

pub async fn protected_resource_metadata(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let Ok(origin) = state.origin.effective_origin(&headers) else {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    Json(ProtectedResourceMetadata {
        resource: format!("{origin}/mcp"),
        authorization_servers: vec![origin],
        scopes_supported: crate::domain::resource_scopes(),
        bearer_methods_supported: ["header"],
    })
    .into_response()
}

#[derive(Deserialize)]
pub struct RegistrationRequest {
    #[serde(default)]
    redirect_uris: Vec<String>,
    client_name: Option<String>,
    token_endpoint_auth_method: Option<String>,
    grant_types: Option<Vec<String>>,
    response_types: Option<Vec<String>>,
    scope: Option<String>,
    application_type: Option<String>,
}

#[derive(Serialize)]
pub struct RegistrationResponse {
    client_id: String,
    client_id_issued_at: i64,
    redirect_uris: Vec<String>,
    client_name: Option<String>,
    token_endpoint_auth_method: &'static str,
    grant_types: Vec<String>,
    response_types: Vec<String>,
    scope: String,
    application_type: String,
}

pub async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    input: Result<Json<RegistrationRequest>, JsonRejection>,
) -> Response {
    let Ok(issuer) = state.origin.effective_origin(&headers) else {
        return error(StatusCode::BAD_REQUEST, "invalid_client_metadata");
    };
    let Ok(Json(input)) = input else {
        return error(StatusCode::BAD_REQUEST, "invalid_client_metadata");
    };
    if !input
        .token_endpoint_auth_method
        .as_deref()
        .unwrap_or("none")
        .eq("none")
    {
        return error(StatusCode::BAD_REQUEST, "invalid_client_metadata");
    }
    let grant_types = input
        .grant_types
        .unwrap_or_else(|| vec!["authorization_code".into()]);
    let default_scope = crate::domain::default_registration_scope(
        grant_types.iter().any(|grant| grant == "refresh_token"),
    );
    let default_response_types = if grant_types
        .iter()
        .any(|grant| grant == "authorization_code")
    {
        vec!["code".into()]
    } else {
        Vec::new()
    };
    let registration = ClientRegistration {
        issuer,
        redirect_uris: input.redirect_uris,
        client_name: input.client_name,
        application_type: input.application_type,
        grant_types,
        response_types: input.response_types.unwrap_or(default_response_types),
        scope: input.scope.unwrap_or(default_scope),
    };
    match state
        .domain
        .oauth()
        .register_public_client(registration)
        .await
    {
        Ok(client) => (
            StatusCode::CREATED,
            Json(RegistrationResponse {
                client_id: client.client_id().to_owned(),
                client_id_issued_at: chrono::Utc::now().timestamp(),
                redirect_uris: client.redirect_uris().to_vec(),
                client_name: client.client_name().map(str::to_owned),
                token_endpoint_auth_method: client.token_endpoint_auth_method(),
                grant_types: client.grant_types().to_vec(),
                response_types: client.response_types().to_vec(),
                scope: client.scope().to_owned(),
                application_type: client.application_type().to_owned(),
            }),
        )
            .into_response(),
        Err(DomainError::InvalidInput(message)) if message.contains("redirect URI") => {
            error(StatusCode::BAD_REQUEST, "invalid_redirect_uri")
        }
        Err(DomainError::InvalidInput(_)) => {
            error(StatusCode::BAD_REQUEST, "invalid_client_metadata")
        }
        Err(DomainError::TemporarilyUnavailable) => {
            error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable")
        }
        Err(error_value) => {
            tracing::error!(error = %error_value, "OAuth client registration failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

#[derive(Serialize)]
pub struct DeviceAuthorizationResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: String,
    expires_in: u64,
    interval: u64,
}

pub async fn device_authorization(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Ok(issuer) = state.origin.effective_origin(&headers) else {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    if !is_form_content_type(&headers) {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    let Ok(body) = std::str::from_utf8(&body) else {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    let Ok(input) = UniqueParameters::parse(body) else {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    if input.any_duplicate() {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    let (Some(client_id), Some(resource)) = (input.one("client_id"), input.one("resource")) else {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    if resource != format!("{issuer}/mcp") {
        return error(StatusCode::BAD_REQUEST, "invalid_target");
    }
    match state
        .domain
        .oauth()
        .issue_device_authorization(DeviceAuthorizationRequest {
            client_id,
            issuer: &issuer,
            resource,
            scope: input.optional_one("scope"),
        })
        .await
    {
        Ok(issued) => {
            let verification_uri = format!("{issuer}/oauth/device");
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("user_code", issued.user_code())
                .finish();
            Json(DeviceAuthorizationResponse {
                device_code: issued.device_code().to_owned(),
                user_code: issued.user_code().to_owned(),
                verification_uri: verification_uri.clone(),
                verification_uri_complete: format!("{verification_uri}?{query}"),
                expires_in: issued.expires_in(),
                interval: issued.interval(),
            })
            .into_response()
        }
        Err(DomainError::InvalidInput(message)) if message.contains("scope") => {
            error(StatusCode::BAD_REQUEST, "invalid_scope")
        }
        Err(DomainError::InvalidCredentials | DomainError::InvalidInput(_)) => {
            error(StatusCode::BAD_REQUEST, "invalid_client")
        }
        Err(DomainError::TemporarilyUnavailable) => {
            error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable")
        }
        Err(error_value) => {
            tracing::error!(error = %error_value, "device authorization failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

pub struct ConsentForm {
    csrf: String,
    user_code: Option<String>,
    decision: Option<String>,
}

impl ConsentForm {
    fn parse(body: &str) -> Result<Self, ()> {
        let parameters = UniqueParameters::parse(body)?;
        Ok(Self {
            csrf: parameters.one("csrf").ok_or(())?.to_owned(),
            user_code: parameters.one("user_code").map(ToOwned::to_owned),
            decision: parameters.one("decision").map(ToOwned::to_owned),
        })
    }
}

pub async fn device(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
    RawQuery(raw_query): RawQuery,
) -> Response {
    device_inner(state, headers, jar, raw_query, None).await
}

pub async fn device_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
    RawQuery(raw_query): RawQuery,
    body: String,
) -> Response {
    device_inner(
        state,
        headers,
        jar,
        raw_query,
        Some(ConsentForm::parse(&body)),
    )
    .await
}

async fn device_inner(
    state: AppState,
    headers: HeaderMap,
    jar: CookieJar,
    raw_query: Option<String>,
    form: Option<Result<ConsentForm, ()>>,
) -> Response {
    let Ok(_issuer) = state.origin.effective_origin(&headers) else {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    let Ok(parameters) = UniqueParameters::parse(raw_query.as_deref().unwrap_or("")) else {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    if parameters.any_duplicate() {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    let query_code = parameters.optional_one("user_code");
    if query_code.is_none() {
        if !parameters.values.is_empty() {
            return error(StatusCode::BAD_REQUEST, "invalid_request");
        }
        if let Some(form) = form {
            let Ok(form) = form else {
                return entry_page(&state, jar, true);
            };
            if web::csrf_from_token(&state, &form.csrf, &jar).is_err() {
                return error(StatusCode::FORBIDDEN, "access_denied");
            }
            let Some(code) = form.user_code.as_deref() else {
                return entry_page(&state, jar, true);
            };
            let Ok(normalized) = normalize_user_code(code) else {
                return entry_page(&state, jar, true);
            };
            let formatted = format!(
                "{}-{}-{}",
                &normalized[0..4],
                &normalized[4..8],
                &normalized[8..12]
            );
            if state
                .domain
                .oauth()
                .find_device_authorization(&formatted)
                .await
                .is_err()
            {
                return entry_page(&state, jar, true);
            }
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("user_code", &formatted)
                .finish();
            return Redirect::to(&format!("/oauth/device?{query}")).into_response();
        }
        return entry_page(&state, jar, false);
    }
    if parameters.values.len() != 1 {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    let Some(user_code) = query_code else {
        unreachable!()
    };
    let authorization = match state
        .domain
        .oauth()
        .find_device_authorization(user_code)
        .await
    {
        Ok(value) => value,
        Err(DomainError::TemporarilyUnavailable) => {
            return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable");
        }
        Err(_) => return entry_page(&state, jar, true),
    };
    let return_to = format!(
        "/oauth/device?{}",
        url::form_urlencoded::Serializer::new(String::new())
            .append_pair("user_code", &authorization.user_code)
            .finish()
    );
    let principal = match web::authenticate(&state, &jar, None).await {
        Ok(value) => value,
        Err(value) if value.is_invalid_credentials() => return redirect_to_login(&return_to),
        Err(value) => return value.into_response(),
    };
    let Some(csrf) = state.csrf_value(&jar).map(ToOwned::to_owned) else {
        return redirect_to_login(&return_to);
    };
    let Ok(email) = state.domain.auth().account_email(&principal).await else {
        return redirect_to_login(&return_to);
    };
    let consent_page = || {
        crate::web::views::device::consent(crate::web::views::device::Consent {
            csrf: &csrf,
            email: &email,
            switch_to: &return_to,
            code: &authorization.user_code,
            client_name: authorization
                .client_name
                .as_deref()
                .unwrap_or("Unnamed application"),
            client_id: &authorization.client_id,
            scope: &authorization.scope,
            role: principal.role(),
            resource: &authorization.resource,
        })
        .into_response()
    };
    let Some(form) = form else {
        return consent_page();
    };
    let Ok(form) = form else {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    if web::csrf_from_token(&state, &form.csrf, &jar).is_err()
        || web::authenticate(&state, &jar, Some(&form.csrf))
            .await
            .is_err()
    {
        return error(StatusCode::FORBIDDEN, "access_denied");
    }
    let approve = match form.decision.as_deref() {
        Some("allow") => true,
        Some("deny") => false,
        _ => return error(StatusCode::BAD_REQUEST, "invalid_request"),
    };
    let granted =
        crate::web::views::permissions::consent_grant(&authorization.scope, principal.role());
    if approve && granted.is_none() {
        return error(StatusCode::BAD_REQUEST, "invalid_scope");
    }
    match state
        .domain
        .oauth()
        .decide_device_authorization(
            &authorization.user_code,
            &principal.identity,
            granted.filter(|_| approve).as_deref(),
        )
        .await
    {
        Ok(()) => crate::web::views::device::terminal(approve).into_response(),
        Err(DomainError::TemporarilyUnavailable) => {
            error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable")
        }
        Err(DomainError::InvalidCredentials | DomainError::InvalidInput(_)) => {
            error(StatusCode::BAD_REQUEST, "invalid_request")
        }
        Err(error_value) => {
            tracing::error!(error = %error_value, "device decision failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

fn entry_page(state: &AppState, jar: CookieJar, invalid: bool) -> Response {
    if let Some(csrf) = state.csrf_value(&jar) {
        return crate::web::views::device::entry(csrf, invalid).into_response();
    }
    let csrf = web::new_csrf_value();
    (
        jar.add(state.csrf_cookie(&csrf)),
        crate::web::views::device::entry(&csrf, invalid),
    )
        .into_response()
}

pub async fn authorize(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
    RawQuery(raw_query): RawQuery,
) -> Response {
    authorize_inner(state, headers, jar, raw_query, None).await
}

pub async fn authorize_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
    RawQuery(raw_query): RawQuery,
    body: String,
) -> Response {
    authorize_inner(
        state,
        headers,
        jar,
        raw_query,
        Some(ConsentForm::parse(&body)),
    )
    .await
}

async fn authorize_inner(
    state: AppState,
    headers: HeaderMap,
    jar: CookieJar,
    raw_query: Option<String>,
    decision: Option<Result<ConsentForm, ()>>,
) -> Response {
    let Ok(issuer) = state.origin.effective_origin(&headers) else {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    let Ok(parameters) = UniqueParameters::parse(raw_query.as_deref().unwrap_or("")) else {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    let (Some(client_id), Some(redirect_uri)) =
        (parameters.one("client_id"), parameters.one("redirect_uri"))
    else {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    };

    let client = match state
        .domain
        .oauth()
        .validate_authorization_client(client_id, &issuer, redirect_uri)
        .await
    {
        Ok(client) => client,
        Err(DomainError::TemporarilyUnavailable) => {
            return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable");
        }
        Err(error_value @ DomainError::Internal(_)) => {
            tracing::error!(error = %error_value, "OAuth client validation failed");
            return error(StatusCode::INTERNAL_SERVER_ERROR, "server_error");
        }
        Err(_) => return error(StatusCode::BAD_REQUEST, "invalid_request"),
    };

    let state_parameter = parameters
        .optional_one("state")
        .filter(|value| value.len() <= 512 && !value.contains(char::is_control));
    let scope_parameter = parameters.optional_one("scope");
    if parameters.is_duplicate_or_missing(&[
        "response_type",
        "client_id",
        "redirect_uri",
        "code_challenge",
        "code_challenge_method",
        "resource",
    ]) || (parameters.contains("state") && state_parameter.is_none())
        || (parameters.contains("scope") && scope_parameter.is_none())
    {
        return authorization_error(redirect_uri, "invalid_request", state_parameter, &issuer);
    }
    let response_type = parameters.one("response_type").expect("checked above");
    let scope = scope_parameter.unwrap_or_else(|| client.scope());
    let code_challenge = parameters.one("code_challenge").expect("checked above");
    let code_challenge_method = parameters
        .one("code_challenge_method")
        .expect("checked above");
    let resource = parameters.one("resource").expect("checked above");
    if response_type != "code" {
        return authorization_error(
            redirect_uri,
            "unsupported_response_type",
            state_parameter,
            &issuer,
        );
    }
    let expected_resource = format!("{issuer}/mcp");
    if resource != expected_resource {
        tracing::warn!(
            received_resource = resource,
            expected_resource,
            client_id,
            "OAuth authorization resource mismatch"
        );
        return authorization_error(redirect_uri, "invalid_target", state_parameter, &issuer);
    }
    if !client.allows_scope(scope) {
        return authorization_error(redirect_uri, "invalid_scope", state_parameter, &issuer);
    }
    if code_challenge_method != "S256" || !valid_pkce_challenge(code_challenge) {
        return authorization_error(redirect_uri, "invalid_request", state_parameter, &issuer);
    }

    let return_to = match raw_query.as_deref() {
        Some(query) => format!("/oauth/authorize?{query}"),
        None => "/oauth/authorize".to_owned(),
    };
    let principal = match web::authenticate(&state, &jar, None).await {
        Ok(principal) => principal,
        Err(error_value) if error_value.is_invalid_credentials() => {
            return redirect_to_login(&return_to);
        }
        Err(error_value) => return error_value.into_response(),
    };
    let Some(csrf_cookie) = state.csrf_value(&jar).map(ToOwned::to_owned) else {
        return (
            jar.add(state.expired_session_cookie())
                .add(state.expired_csrf_cookie()),
            redirect_to_login(&return_to),
        )
            .into_response();
    };

    let Ok(email) = state.domain.auth().account_email(&principal).await else {
        return redirect_to_login(&return_to);
    };
    let consent_page = || {
        crate::web::views::authorize::page(crate::web::views::authorize::Consent {
            csrf: &csrf_cookie,
            email: &email,
            switch_to: &return_to,
            client_name: client.client_name().unwrap_or("Unnamed application"),
            client_id,
            scope,
            role: principal.role(),
            resource,
            redirect_uri,
        })
        .into_response()
    };
    let Some(decision) = decision else {
        return consent_page();
    };
    let Ok(decision) = decision else {
        return authorization_error(redirect_uri, "invalid_request", state_parameter, &issuer);
    };
    if web::csrf_from_token(&state, &decision.csrf, &jar).is_err()
        || web::authenticate(&state, &jar, Some(&decision.csrf))
            .await
            .is_err()
    {
        return error(StatusCode::FORBIDDEN, "access_denied");
    }
    match decision.decision.as_deref() {
        Some("allow") => {}
        Some("deny") => {
            return authorization_error(redirect_uri, "access_denied", state_parameter, &issuer);
        }
        _ => return authorization_error(redirect_uri, "invalid_request", state_parameter, &issuer),
    }
    let Some(granted) = crate::web::views::permissions::consent_grant(scope, principal.role())
    else {
        return authorization_error(redirect_uri, "invalid_scope", state_parameter, &issuer);
    };

    let issued = match state
        .domain
        .oauth()
        .issue_authorization_code(AuthorizationCodeRequest {
            identity: &principal.identity,
            client_id,
            issuer: &issuer,
            redirect_uri,
            resource,
            scope: &granted,
            code_challenge,
        })
        .await
    {
        Ok(issued) => issued,
        Err(DomainError::InvalidInput(_)) | Err(DomainError::InvalidCredentials) => {
            return authorization_error(redirect_uri, "invalid_request", state_parameter, &issuer);
        }
        Err(DomainError::TemporarilyUnavailable) => {
            return authorization_error(
                redirect_uri,
                "temporarily_unavailable",
                state_parameter,
                &issuer,
            );
        }
        Err(error_value) => {
            tracing::error!(error = %error_value, "OAuth authorization failed");
            return authorization_error(redirect_uri, "server_error", state_parameter, &issuer);
        }
    };
    let Ok(mut location) = Url::parse(redirect_uri) else {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    {
        let mut pairs = location.query_pairs_mut();
        pairs.append_pair("code", issued.expose());
        if let Some(state) = state_parameter {
            pairs.append_pair("state", state);
        }
        pairs.append_pair("iss", &issuer);
    }
    Redirect::to(location.as_str()).into_response()
}

fn authorization_error(
    redirect_uri: &str,
    code: &'static str,
    state: Option<&str>,
    issuer: &str,
) -> Response {
    let Ok(mut location) = Url::parse(redirect_uri) else {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    {
        let mut pairs = location.query_pairs_mut();
        pairs.append_pair("error", code);
        if let Some(state) = state {
            pairs.append_pair("state", state);
        }
        pairs.append_pair("iss", issuer);
    }
    Redirect::to(location.as_str()).into_response()
}

fn redirect_to_login(return_to: &str) -> Response {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("return_to", return_to)
        .finish();
    Redirect::to(&format!("/login?{query}")).into_response()
}

#[derive(Serialize)]
pub struct TokenResponse {
    access_token: String,
    token_type: &'static str,
    expires_in: u64,
    scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
}

fn token_response(issued: &IssuedAccessToken) -> Response {
    Json(TokenResponse {
        access_token: issued.expose().to_owned(),
        token_type: "Bearer",
        expires_in: issued.expires_in(),
        scope: issued.scope().to_owned(),
        refresh_token: issued.refresh_token().map(str::to_owned),
    })
    .into_response()
}

#[derive(Serialize)]
struct DevicePollOAuthError {
    error: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    interval: Option<u64>,
}
fn device_poll_error(code: &'static str, interval: Option<u64>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(DevicePollOAuthError {
            error: code,
            interval,
        }),
    )
        .into_response()
}

fn is_form_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(';').next().is_some_and(|kind| {
                kind.trim()
                    .eq_ignore_ascii_case("application/x-www-form-urlencoded")
            })
        })
}

pub async fn token(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let Ok(issuer) = state.origin.effective_origin(&headers) else {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    if let Some(value) = headers.get(header::AUTHORIZATION) {
        let challenge = value
            .to_str()
            .ok()
            .and_then(|value| value.split_once(' ').map(|(scheme, _)| scheme))
            .filter(|scheme| scheme.bytes().all(|byte| byte.is_ascii_alphabetic()))
            .and_then(|scheme| {
                HeaderValue::from_str(&format!("{scheme} realm=\"aegis OAuth token\"")).ok()
            })
            .unwrap_or_else(|| HeaderValue::from_static(TOKEN_BASIC_CHALLENGE));
        return error_with_challenge(StatusCode::UNAUTHORIZED, "invalid_client", challenge);
    }
    if !is_form_content_type(&headers) {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    let Ok(body) = std::str::from_utf8(&body) else {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    let Ok(input) = UniqueParameters::parse(body) else {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    if input.contains("client_secret") {
        return error_with_challenge(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            HeaderValue::from_static(TOKEN_BASIC_CHALLENGE),
        );
    }
    if input.any_duplicate()
        || input.one("grant_type").is_none()
        || input.one("client_id").is_none()
    {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    let grant_type = input.one("grant_type").expect("checked above");
    let client_id = input.one("client_id").expect("checked above");
    let expected_resource = format!("{issuer}/mcp");
    if grant_type == DEVICE_GRANT_TYPE {
        let Some(device_code) = input.one("device_code") else {
            return error(StatusCode::BAD_REQUEST, "invalid_request");
        };
        return match state
            .domain
            .oauth()
            .redeem_device_code(DeviceTokenRequest {
                device_code,
                client_id,
                issuer: &issuer,
                resource: input.optional_one("resource"),
            })
            .await
        {
            Ok(issued) => token_response(&issued),
            Err(DevicePollError::AuthorizationPending) => {
                device_poll_error("authorization_pending", None)
            }
            Err(DevicePollError::SlowDown { interval }) => {
                device_poll_error("slow_down", Some(interval))
            }
            Err(DevicePollError::AccessDenied) => device_poll_error("access_denied", None),
            Err(DevicePollError::ExpiredToken) => device_poll_error("expired_token", None),
            Err(DevicePollError::InvalidGrant) => device_poll_error("invalid_grant", None),
            Err(DevicePollError::InvalidTarget) => error(StatusCode::BAD_REQUEST, "invalid_target"),
            Err(DevicePollError::TemporarilyUnavailable) => {
                error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable")
            }
            Err(DevicePollError::Internal(error_value)) => {
                tracing::error!(error = %error_value, "device token issuance failed");
                error(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
            }
        };
    }
    let Some(resource) = input.one("resource") else {
        return error(StatusCode::BAD_REQUEST, "invalid_target");
    };
    if resource != expected_resource {
        return error(StatusCode::BAD_REQUEST, "invalid_target");
    }
    let result = match grant_type {
        "authorization_code" => {
            let (Some(code), Some(redirect_uri), Some(code_verifier)) = (
                input.one("code"),
                input.one("redirect_uri"),
                input.one("code_verifier"),
            ) else {
                return error(StatusCode::BAD_REQUEST, "invalid_request");
            };
            state
                .domain
                .oauth()
                .redeem_authorization_code(AuthorizationCodeRedemption {
                    code,
                    client_id,
                    issuer: &issuer,
                    redirect_uri,
                    resource,
                    code_verifier,
                })
                .await
        }
        "refresh_token" => {
            let Some(refresh_token) = input.one("refresh_token") else {
                return error(StatusCode::BAD_REQUEST, "invalid_request");
            };
            state
                .domain
                .oauth()
                .refresh_access_token(RefreshTokenRequest {
                    refresh_token,
                    client_id,
                    issuer: &issuer,
                    resource,
                })
                .await
        }
        _ => return error(StatusCode::BAD_REQUEST, "unsupported_grant_type"),
    };
    match result {
        Ok(issued) => token_response(&issued),
        Err(DomainError::InvalidCredentials | DomainError::InvalidInput(_)) => {
            error(StatusCode::BAD_REQUEST, "invalid_grant")
        }
        Err(DomainError::TemporarilyUnavailable) => {
            error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable")
        }
        Err(error_value) => {
            tracing::error!(error = %error_value, "OAuth token issuance failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

struct UniqueParameters {
    values: HashMap<String, Vec<String>>,
}

impl UniqueParameters {
    fn parse(raw: &str) -> Result<Self, ()> {
        if raw.len() > MAX_PARAMETER_BYTES
            || raw.bytes().any(|byte| byte.is_ascii_control())
            || malformed_percent_encoding(raw)
        {
            return Err(());
        }
        let mut values: HashMap<String, Vec<String>> = HashMap::new();
        let mut count = 0;
        for (name, value) in url::form_urlencoded::parse(raw.as_bytes()) {
            count += 1;
            if count > MAX_PARAMETERS || name.len() > 64 || value.len() > 2048 || name.is_empty() {
                return Err(());
            }
            values
                .entry(name.into_owned())
                .or_default()
                .push(value.into_owned());
        }
        Ok(Self { values })
    }

    fn one(&self, name: &str) -> Option<&str> {
        match self.values.get(name).map(Vec::as_slice) {
            Some([value]) if !value.is_empty() => Some(value),
            _ => None,
        }
    }

    fn optional_one(&self, name: &str) -> Option<&str> {
        match self.values.get(name).map(Vec::as_slice) {
            Some([value]) => Some(value),
            _ => None,
        }
    }

    fn contains(&self, name: &str) -> bool {
        self.values.contains_key(name)
    }

    fn any_duplicate(&self) -> bool {
        self.values.values().any(|values| values.len() != 1)
    }

    fn is_duplicate_or_missing(&self, required: &[&str]) -> bool {
        required.iter().any(|name| self.one(name).is_none()) || self.any_duplicate()
    }
}

fn malformed_percent_encoding(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return true;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    false
}

fn valid_pkce_challenge(challenge: &str) -> bool {
    challenge.len() == 43
        && !challenge.contains('=')
        && base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(challenge)
            .is_ok_and(|decoded| decoded.len() == 32)
}
