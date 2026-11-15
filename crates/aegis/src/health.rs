use crate::domain::Domain;
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use serde::Serialize;
use std::sync::Arc;

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

pub fn router(domain: Arc<Domain>) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .with_state(domain)
}

async fn health(State(domain): State<Arc<Domain>>) -> Result<Json<Health>, StatusCode> {
    domain.health().await.map_err(|error| {
        tracing::error!(%error, "database health check failed");
        StatusCode::SERVICE_UNAVAILABLE
    })?;
    Ok(Json(Health { status: "ok" }))
}
