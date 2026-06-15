//! Error types for YANuget.
//!
//! A single [`Error`] enum is used across the crate. It implements
//! [`axum::response::IntoResponse`] so handlers can return `Result<_, Error>`
//! directly and get sensible HTTP status codes and problem bodies.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// The crate-wide result type.
pub type Result<T> = std::result::Result<T, Error>;

/// All errors surfaced by YANuget.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The requested package (or package version) does not exist.
    #[error("package not found")]
    PackageNotFound,

    /// A package with the same id and version already exists.
    #[error("package already exists")]
    PackageAlreadyExists,

    /// The uploaded package was malformed or could not be read.
    #[error("invalid package: {0}")]
    InvalidPackage(String),

    /// The upload exceeded the configured maximum size.
    #[error("payload too large: {0}")]
    PayloadTooLarge(String),

    /// The supplied API key was missing or incorrect.
    #[error("unauthorized")]
    Unauthorized,

    /// The request was syntactically or semantically invalid.
    #[error("invalid request: {0}")]
    BadRequest(String),

    /// A version string could not be parsed.
    #[error("invalid version: {0}")]
    InvalidVersion(String),

    /// Underlying storage failure.
    #[error("storage error: {0}")]
    Storage(String),

    /// Underlying database failure.
    #[error(transparent)]
    Database(#[from] sqlx::Error),

    /// I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Any other unexpected error.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl Error {
    /// The HTTP status code that best represents this error.
    pub fn status(&self) -> StatusCode {
        match self {
            Error::PackageNotFound => StatusCode::NOT_FOUND,
            Error::PackageAlreadyExists => StatusCode::CONFLICT,
            Error::InvalidPackage(_) => StatusCode::BAD_REQUEST,
            Error::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Error::Unauthorized => StatusCode::UNAUTHORIZED,
            Error::BadRequest(_) => StatusCode::BAD_REQUEST,
            Error::InvalidVersion(_) => StatusCode::BAD_REQUEST,
            Error::Database(sqlx::Error::RowNotFound) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = self.status();
        // Log server-side faults; client errors are expected and stay quiet.
        if status.is_server_error() {
            tracing::error!(error = %self, "request failed");
        }
        let body = Json(json!({
            "error": self.to_string(),
        }));
        (status, body).into_response()
    }
}
