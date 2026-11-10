//! AGS enrollment. Provider OAuth never enters this interface.
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};

use crate::{
    domain::{Domain, Principal, scope_allows},
    gateway::Gateway,
    origin::OriginPolicy,
    relay_wire::{Connection, Enrollment, RESOURCE_PATH, SCOPE},
};

#[derive(Clone)]
pub struct RelayState {
    pub domain: Arc<Domain>,
    pub gateway: Gateway,
    pub origin: OriginPolicy,
}

pub fn router(state: RelayState) -> Router {
    Router::new()
        .route("/api/relay/connection", get(connection))
        .route("/api/relay/enroll", post(enroll))
        .route("/api/relay/disconnect", post(disconnect))
        .layer(axum::middleware::map_response(crate::web::auth_no_store))
        .with_state(state)
}

async fn authenticate(
    state: &RelayState,
    headers: &HeaderMap,
) -> Result<(Principal, String), StatusCode> {
    let origin = state
        .origin
        .effective_origin(headers)
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let principal = state
        .domain
        .oauth()
        .authenticate_access_token(token, &origin, &format!("{origin}{RESOURCE_PATH}"))
        .await
        .map_err(|_| StatusCode::UNAUTHORIZED)?;
    if !principal
        .oauth()
        .is_some_and(|p| scope_allows(&p.scope, SCOPE))
    {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok((principal, origin))
}

async fn connection(State(state): State<RelayState>, headers: HeaderMap) -> Response {
    if let Err(status) = authenticate(&state, &headers).await {
        return status.into_response();
    }
    Json(Connection {
        version: crate::relay_wire::PROTOCOL_VERSION,
        providers: state.gateway.provider_catalog(),
    })
    .into_response()
}

async fn enroll(
    State(state): State<RelayState>,
    headers: HeaderMap,
    Json(input): Json<Enrollment>,
) -> Response {
    let (principal, _) = match authenticate(&state, &headers).await {
        Ok(value) => value,
        Err(status) => return status.into_response(),
    };
    let configured = state.gateway.provider_ids();
    if input.providers.is_empty()
        || input.providers.len() > configured.len()
        || input.providers.iter().any(|id| !configured.contains(id))
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match state
        .domain
        .oauth()
        .enroll_relay(&principal, &input.providers)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            tracing::error!(%error, "relay enrollment failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn disconnect(State(state): State<RelayState>, headers: HeaderMap) -> Response {
    let (principal, issuer) = match authenticate(&state, &headers).await {
        Ok(value) => value,
        Err(status) => return status.into_response(),
    };
    let client = &principal
        .oauth()
        .expect("authenticated OAuth principal")
        .client_id;
    match state
        .domain
        .oauth()
        .revoke_client_grants(&principal, &issuer, client)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[cfg(test)]
mod tests;
