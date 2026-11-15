mod http;
mod machine;

use crate::{gateway::Gateway, web::SessionState};
use axum::{
    Router, middleware,
    routing::{get, post},
};
use std::ops::Deref;

#[derive(Clone)]
pub struct State {
    session: SessionState,
    /// Offers the providers a machine can be granted when it is paired.
    gateway: Gateway,
}

impl State {
    pub fn new(session: SessionState, gateway: Gateway) -> Self {
        Self { session, gateway }
    }
}

impl Deref for State {
    type Target = SessionState;

    fn deref(&self) -> &Self::Target {
        &self.session
    }
}

pub fn router(state: State) -> Router {
    crate::web::common(
        Router::new()
            .route(
                aegis_wire::METADATA_PATH,
                get(http::authorization_server_metadata),
            )
            .route(
                "/.well-known/oauth-protected-resource/mcp",
                get(http::protected_resource_metadata),
            )
            .route("/oauth/register", post(http::register))
            .route(
                "/oauth/device_authorization",
                post(http::device_authorization),
            )
            .route("/oauth/device", get(http::device).post(http::device_submit))
            .route(
                "/oauth/authorize",
                get(http::authorize).post(http::authorize_submit),
            )
            .route("/oauth/token", post(http::token))
            .route(
                aegis_wire::MACHINE_PATH,
                get(machine::machine).delete(machine::unpair),
            )
            .layer(middleware::map_response(crate::web::auth_no_store))
            .with_state(state),
    )
}
