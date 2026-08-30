mod auth;

use std::{borrow::Cow, time::Duration};

use rmcp::{
    ServerHandler,
    model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo},
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
    },
};
use tokio_util::sync::CancellationToken;

use crate::{domain::OAuthService, origin::OriginPolicy};

#[derive(Clone)]
pub struct State {
    pub oauth: OAuthService,
    pub origin: OriginPolicy,
}

impl State {
    pub fn new(oauth: OAuthService, origin: OriginPolicy) -> Self {
        Self { oauth, origin }
    }
}

pub(crate) const SUPPORTED_PROTOCOL_VERSIONS: &[ProtocolVersion] = &[ProtocolVersion::V_2026_07_28];

#[derive(Clone)]
pub struct McpServer;

impl ServerHandler for McpServer {
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(SUPPORTED_PROTOCOL_VERSIONS)
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().build())
            .with_protocol_version(ProtocolVersion::V_2026_07_28)
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions("Aegis analytics tools will be added in a later phase.")
    }
}

pub fn router(state: State, cancellation: CancellationToken) -> axum::Router {
    let (allowed_hosts, allowed_origins) = state.origin.mcp_security();
    let service = service(cancellation, allowed_hosts, allowed_origins);
    axum::Router::new()
        .nest_service("/mcp", service)
        .route_layer(axum::middleware::from_fn_with_state(
            state,
            auth::require_mcp_oauth,
        ))
}

pub fn service(
    cancellation_token: CancellationToken,
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
) -> StreamableHttpService<McpServer, NeverSessionManager> {
    let config = StreamableHttpServerConfig::default()
        .with_allowed_hosts(allowed_hosts)
        .with_allowed_origins(allowed_origins)
        .with_legacy_session_mode(false)
        .with_stateless_protocol_metadata_required(true)
        .with_json_response(false)
        .with_sse_keep_alive(Some(Duration::from_secs(10)))
        .with_cancellation_token(cancellation_token);

    StreamableHttpService::new(
        move || Ok(McpServer),
        NeverSessionManager::default().into(),
        config,
    )
}
