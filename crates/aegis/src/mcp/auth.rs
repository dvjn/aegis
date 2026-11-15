use crate::{mcp::State as McpState, origin::normalize_origin, request_id};
use axum::{
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use url::Url;

pub(super) async fn require_mcp_oauth(
    State(state): State<McpState>,
    mut request: Request,
    next: Next,
) -> Response {
    let origin = match state.origin.effective_origin(request.headers()) {
        Ok(origin) => origin,
        Err(()) => return StatusCode::BAD_REQUEST.into_response(),
    };
    if let Some(supplied_origin) = request.headers().get(header::ORIGIN) {
        let expected = normalize_origin(&origin);
        let matches = supplied_origin
            .to_str()
            .ok()
            .and_then(|value| Url::parse(value).ok())
            .is_some_and(|value| {
                normalize_origin(&value.origin().ascii_serialization()) == expected
            });
        if !matches {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    let metadata = format!("{origin}/.well-known/oauth-protected-resource/mcp");
    let resource = format!("{origin}/mcp");
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(split_authorization)
        .filter(|(scheme, value)| {
            scheme.eq_ignore_ascii_case("Bearer")
                && !value.is_empty()
                && !value.contains(char::is_whitespace)
        })
        .map(|(_, value)| value);
    let Some(token) = token else {
        return bearer_error(StatusCode::UNAUTHORIZED, &metadata, None);
    };
    let principal = match state
        .oauth
        .authenticate_access_token(token, &origin, &resource)
        .await
    {
        Ok(principal) => principal,
        Err(_) => {
            return bearer_error(StatusCode::UNAUTHORIZED, &metadata, Some("invalid_token"));
        }
    };
    request.extensions_mut().insert(principal);
    next.run(request).await
}

fn split_authorization(value: &str) -> Option<(&str, &str)> {
    let (scheme, credentials) = value.split_once(' ')?;
    if scheme.is_empty() || credentials.is_empty() || credentials.starts_with(' ') {
        return None;
    }
    Some((scheme, credentials))
}

fn bearer_error(status: StatusCode, metadata: &str, error: Option<&str>) -> Response {
    let mut challenge = format!("Bearer resource_metadata=\"{metadata}\"");
    if let Some(error) = error {
        challenge.push_str(&format!(", error=\"{error}\""));
    }
    let code = error.unwrap_or("unauthorized");
    let mut body = serde_json::json!({
        "error": code,
        "error_description": match code {
            "invalid_token" => "the access token is invalid or expired",
            _ => "an OAuth 2.1 bearer token is required",
        },
        "resource_metadata": metadata,
    });
    if let (Some(object), Some(id)) = (body.as_object_mut(), request_id::current()) {
        object.insert("request_id".into(), serde_json::Value::String(id));
    }
    let mut response = (status, axum::Json(body)).into_response();
    if let Ok(value) = HeaderValue::from_str(&challenge) {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, value);
    }
    response
}
