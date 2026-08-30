use thiserror::Error;

#[derive(Debug, Error)]
pub enum DomainError {
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("not found")]
    NotFound,
    #[error("conflict")]
    Conflict,
    #[error("forbidden")]
    Forbidden,
    #[error("invalid input: {0}")]
    InvalidInput(&'static str),
    #[error("temporarily unavailable")]
    TemporarilyUnavailable,
    #[error("internal domain error")]
    Internal(#[source] sea_orm::DbErr),
}

impl From<sea_orm::DbErr> for DomainError {
    fn from(value: sea_orm::DbErr) -> Self {
        Self::Internal(value)
    }
}
