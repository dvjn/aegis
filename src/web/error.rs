use crate::{domain::DomainError, request_id};
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
#[error("web request failed")]
pub struct WebError {
    status: StatusCode,
    code: &'static str,
}
#[derive(Serialize)]
struct Body {
    error: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
}
impl WebError {
    pub fn invalid_credentials() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "invalid_credentials",
        }
    }
    pub fn is_invalid_credentials(&self) -> bool {
        self.code == "invalid_credentials"
    }
    pub fn forbidden_request() -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code: "invalid_request_origin",
        }
    }
}
impl From<DomainError> for WebError {
    fn from(value: DomainError) -> Self {
        match value {
            DomainError::InvalidCredentials => Self::invalid_credentials(),
            DomainError::NotFound => Self {
                status: StatusCode::NOT_FOUND,
                code: "not_found",
            },
            DomainError::Conflict => Self {
                status: StatusCode::CONFLICT,
                code: "conflict",
            },
            DomainError::Forbidden => Self {
                status: StatusCode::FORBIDDEN,
                code: "forbidden",
            },
            DomainError::InvalidInput(_) => Self {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                code: "invalid_request",
            },
            DomainError::TemporarilyUnavailable => Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                code: "temporarily_unavailable",
            },
            error @ DomainError::Internal(_) => {
                tracing::error!(?error, "domain operation failed");
                Self {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    code: "internal_error",
                }
            }
        }
    }
}
impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            Json(Body {
                error: self.code,
                request_id: request_id::current(),
            }),
        )
            .into_response();
        response.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-store"),
        );
        response
    }
}
