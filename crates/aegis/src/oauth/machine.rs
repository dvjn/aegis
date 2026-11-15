//! A paired machine reads and ends its own grant with its access token.
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};

use crate::{
    domain::{DomainError, Principal},
    oauth::State as AppState,
};

struct Failure(StatusCode, &'static str);

impl From<DomainError> for Failure {
    fn from(error: DomainError) -> Self {
        match error {
            DomainError::TemporarilyUnavailable => {
                Self(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable")
            }
            DomainError::Internal(_) => {
                tracing::error!(%error, "machine request failed");
                Self(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
            }
            _ => Self(StatusCode::UNAUTHORIZED, "invalid_token"),
        }
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let Self(status, code) = self;
        let mut response = (
            status,
            Json(aegis_wire::OAuthError {
                error: code.to_owned(),
                interval: None,
            }),
        )
            .into_response();
        if status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer error=\"invalid_token\""),
            );
        }
        response
    }
}

async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(Principal, String), Failure> {
    let issuer = state
        .origin
        .effective_origin(headers)
        .map_err(|()| Failure(StatusCode::BAD_REQUEST, "invalid_request"))?;
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, token)| scheme.eq_ignore_ascii_case("bearer") && !token.is_empty())
        .map(|(_, token)| token)
        .ok_or(Failure(StatusCode::UNAUTHORIZED, "invalid_token"))?;
    let principal = state
        .domain
        .oauth()
        .authenticate_machine_token(token, &issuer)
        .await?;
    Ok((principal, issuer))
}

async fn read(state: &AppState, headers: &HeaderMap) -> Result<Response, Failure> {
    let (principal, issuer) = authenticate(state, headers).await?;
    let machine = state.domain.oauth().machine(&principal, &issuer).await?;
    Ok(Json(machine).into_response())
}

async fn end(state: &AppState, headers: &HeaderMap) -> Result<Response, Failure> {
    let (principal, issuer) = authenticate(state, headers).await?;
    state
        .domain
        .oauth()
        .unpair_machine(&principal, &issuer)
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub async fn machine(State(state): State<AppState>, headers: HeaderMap) -> Response {
    read(&state, &headers)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

pub async fn unpair(State(state): State<AppState>, headers: HeaderMap) -> Response {
    end(&state, &headers)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}
