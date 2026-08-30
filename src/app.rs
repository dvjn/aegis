use crate::{
    access_log,
    api_keys::KeyStore,
    domain::Domain,
    gateway::{self, Gateway},
    health, mcp, oauth,
    origin::OriginPolicy,
    request_id,
    usage::UsageStore,
    web,
};
use axum::{Router, extract::DefaultBodyLimit, http::StatusCode, middleware};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use tower_http::timeout::TimeoutLayer;

pub fn router(
    domain: Arc<Domain>,
    gateway: Gateway,
    keys: KeyStore,
    usage: UsageStore,
    origin: OriginPolicy,
    cancellation: CancellationToken,
) -> Router {
    let session = web::SessionState::new(domain.clone(), origin.clone());
    let web_state = web::AppState::new(session.clone(), gateway.clone(), keys, usage);
    let oauth_state = oauth::State::new(session);
    let mcp_state = mcp::State::new(domain.oauth().clone(), origin);
    let web_and_gateway = Router::new()
        .merge(gateway::router(gateway).layer(access_log::layer!(
            access_log::GATEWAY,
            user_id,
            key_id,
            provider_id,
            model,
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
            reasoning_tokens,
        )))
        .merge(health::router(domain).layer(access_log::layer!(access_log::HEALTHZ)))
        .merge(web::asset_router().layer(access_log::layer!(access_log::ASSETS)))
        .merge(web::auth_router(web_state).layer(access_log::layer!(access_log::AUTH)))
        .merge(oauth::router(oauth_state).layer(access_log::layer!(access_log::OAUTH)))
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(600),
        ));

    Router::new()
        .merge(web_and_gateway)
        .merge(mcp::router(mcp_state, cancellation).layer(access_log::layer!(access_log::MCP)))
        .layer(middleware::from_fn(request_id::propagate))
}
