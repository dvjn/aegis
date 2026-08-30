use super::Gateway;
use axum::{
    Router,
    extract::{Path, Request, State},
    response::Response,
    routing::any,
};

pub fn router(state: Gateway) -> Router {
    Router::new()
        .route("/providers/{provider_id}/{*path}", any(provider))
        .with_state(state)
}

async fn provider(
    State(gateway): State<Gateway>,
    Path((provider_id, _path)): Path<(String, String)>,
    request: Request,
) -> Response {
    gateway.forward(&provider_id, request).await
}
