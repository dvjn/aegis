//! AGS enrollment and server-hosted distribution. Provider OAuth never enters this interface.
use std::{path::PathBuf, sync::Arc};

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use sha2::{Digest, Sha256};
use tokio_util::io::ReaderStream;

use crate::{
    domain::{Domain, Principal, scope_allows},
    gateway::Gateway,
    origin::OriginPolicy,
    relay_wire::{Connection, Enrollment, RESOURCE_PATH, Release, SCOPE},
};

#[derive(Clone)]
pub struct RelayState {
    pub domain: Arc<Domain>,
    pub gateway: Gateway,
    pub origin: OriginPolicy,
    pub artifacts: PathBuf,
}

pub fn router(state: RelayState) -> Router {
    Router::new()
        .route("/api/relay/connection", get(connection))
        .route("/api/relay/enroll", post(enroll))
        .route("/api/relay/disconnect", post(disconnect))
        .route("/connect", get(connect_page))
        .route("/install.sh", get(installer))
        .route("/install-mise.sh", get(mise_installer))
        .route("/.well-known/aegis-relay", get(manifest))
        .route(
            "/downloads/ags/version",
            get(|| async { env!("CARGO_PKG_VERSION") }),
        )
        .route("/downloads/ags/{version}/{target}/{file}", get(download))
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

const TARGETS: &[&str] = &["x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"];

async fn manifest(State(state): State<RelayState>) -> Json<Release> {
    Json(Release {
        version: env!("CARGO_PKG_VERSION").into(),
        protocol: crate::relay_wire::PROTOCOL_VERSION,
        targets: TARGETS
            .iter()
            .filter(|target| state.artifacts.join(target).join("ags").is_file())
            .map(|target| (*target).into())
            .collect(),
    })
}

async fn download(
    State(state): State<RelayState>,
    Path((version, target, file)): Path<(String, String, String)>,
) -> Response {
    if version != env!("CARGO_PKG_VERSION") || !["ags", "ags.sha256"].contains(&file.as_str()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let target = match target.as_str() {
        "linux-x64" | "x86_64-unknown-linux-musl" => "x86_64-unknown-linux-musl",
        "linux-arm64" | "aarch64-unknown-linux-musl" => "aarch64-unknown-linux-musl",
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let path = state.artifacts.join(target).join("ags");
    if file == "ags.sha256" {
        let result = tokio::task::spawn_blocking(move || -> std::io::Result<String> {
            use std::io::Read;
            let mut file = std::fs::File::open(path)?;
            let mut hash = Sha256::new();
            let mut buffer = [0u8; 65536];
            loop {
                let size = file.read(&mut buffer)?;
                if size == 0 {
                    break;
                }
                hash.update(&buffer[..size]);
            }
            let digest: String = hash
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            Ok(format!("{digest}  ags\n"))
        })
        .await;
        return match result {
            Ok(Ok(body)) => body.into_response(),
            _ => StatusCode::NOT_FOUND.into_response(),
        };
    }
    match tokio::fs::File::open(path).await {
        Ok(file) => (
            [(header::CONTENT_TYPE, "application/octet-stream")],
            Body::from_stream(ReaderStream::new(file)),
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

async fn installer(State(state): State<RelayState>, headers: HeaderMap) -> Response {
    let origin = match state.origin.effective_origin(&headers) {
        Ok(origin) => origin,
        Err(()) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let script = include_str!("relay/install.sh").replace("@SERVER@", &shell_quote(&origin));
    (
        [(header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")],
        script,
    )
        .into_response()
}

async fn mise_installer(State(state): State<RelayState>, headers: HeaderMap) -> Response {
    let origin = match state.origin.effective_origin(&headers) {
        Ok(origin) => origin,
        Err(()) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let script = include_str!("relay/install-mise.sh").replace("@SERVER@", &shell_quote(&origin));
    (
        [(header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")],
        script,
    )
        .into_response()
}

async fn connect_page(State(state): State<RelayState>, headers: HeaderMap) -> Response {
    let origin = match state.origin.effective_origin(&headers) {
        Ok(origin) => origin,
        Err(()) => return StatusCode::BAD_REQUEST.into_response(),
    };
    use maud::html;
    crate::web::views::layout("Connect a machine · Aegis", html! {
        main class="auth-shell" { article class="auth-card" {
            h1 { "Connect a machine" }
            p { "Install AGS, approve this machine, and choose default providers for Claude Code, Codex, pi, and Antigravity." }
            pre { code { "curl -fsSL " (shell_quote(&format!("{origin}/install.sh"))) " | sh" } }
            p { "Or install this version with mise:" }
            pre { code {
                "curl -fsSL " (shell_quote(&format!("{origin}/install-mise.sh"))) " | sh"
            } }
            p { "Linux x86_64 and ARM64. Existing provider sign-ins stay with each CLI." }
            a href="/account" { "Manage connected clients" }
        } }
    }).into_response()
}

#[cfg(test)]
mod tests;
