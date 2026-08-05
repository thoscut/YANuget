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

    /// Admin credentials were missing or incorrect. Surfaced as a Basic-auth
    /// challenge so a browser prompts for the admin key.
    #[error("admin authentication required")]
    AdminUnauthorized,

    /// The request was syntactically or semantically invalid.
    #[error("invalid request: {0}")]
    BadRequest(String),

    /// The package violated a feed policy under a blocking action.
    #[error("policy violation: {0}")]
    PolicyViolation(String),

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
            Error::AdminUnauthorized => StatusCode::UNAUTHORIZED,
            Error::BadRequest(_) => StatusCode::BAD_REQUEST,
            Error::PolicyViolation(_) => StatusCode::FORBIDDEN,
            Error::InvalidVersion(_) => StatusCode::BAD_REQUEST,
            Error::Database(sqlx::Error::RowNotFound) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = self.status();
        // Client errors describe what the caller did wrong and are safe to
        // return verbatim. Server faults are not: `Io`, `Database` and `Other`
        // are transparent wrappers, so their text carries filesystem paths, SQL
        // and upstream URLs. Those go to the log, and the caller gets a generic
        // message with the status code as the only signal.
        let message = if status.is_server_error() {
            tracing::error!(error = %self, "request failed");
            "internal server error".to_string()
        } else {
            self.to_string()
        };
        // Both 401s carry a Basic challenge, and the feed one is not optional.
        // A NuGet client configured with `-u user -p key` hands the credential
        // to `HttpClientHandler.Credentials`, and .NET only attaches an
        // `Authorization` header once the server has actually challenged for
        // it. Answering a read-gated feed with a bare 401 means the client
        // retries unauthenticated forever and `dotnet restore` fails outright —
        // even though the key it was given is correct.
        let challenge = matches!(self, Error::AdminUnauthorized | Error::Unauthorized);
        let body = Json(json!({ "error": message }));
        let mut response = (status, body).into_response();
        if challenge {
            let realm = if matches!(self, Error::AdminUnauthorized) {
                "Basic realm=\"YANuget Admin\""
            } else {
                "Basic realm=\"YANuget\""
            };
            response.headers_mut().insert(
                axum::http::header::WWW_AUTHENTICATE,
                axum::http::HeaderValue::from_static(realm),
            );
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes_map_as_expected() {
        assert_eq!(Error::PackageNotFound.status(), StatusCode::NOT_FOUND);
        assert_eq!(Error::PackageAlreadyExists.status(), StatusCode::CONFLICT);
        assert_eq!(
            Error::InvalidPackage("x".into()).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            Error::PayloadTooLarge("x".into()).status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(Error::Unauthorized.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(Error::AdminUnauthorized.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            Error::InvalidVersion("x".into()).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            Error::Database(sqlx::Error::RowNotFound).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            Error::Other(anyhow::anyhow!("boom")).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn admin_unauthorized_sends_basic_challenge() {
        let resp = Error::AdminUnauthorized.into_response();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let header = resp
            .headers()
            .get(axum::http::header::WWW_AUTHENTICATE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(header.contains("Basic"));
    }

    #[tokio::test]
    async fn server_faults_do_not_leak_internals() {
        use axum::body::to_bytes;

        // An I/O error's text names a path; a database error names SQL. Neither
        // may reach the caller.
        let io = Error::Io(std::io::Error::other("/srv/data/packages/secret.nupkg"));
        let resp = io.into_response();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(!text.contains("secret.nupkg"), "leaked path: {text}");
        assert!(text.contains("internal server error"));

        // Client errors stay descriptive — they tell the caller what to fix.
        let bad = Error::InvalidVersion("not-a-version".into());
        let resp = bad.into_response();
        let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("not-a-version"), "over-redacted: {text}");
    }

    #[test]
    fn both_unauthorized_variants_challenge_for_basic() {
        // A read-gated feed that answers with a bare 401 is unusable from a
        // NuGet client: .NET only attaches the credential the user configured
        // after it has been challenged, so restore fails with a correct key.
        for (error, realm) in [
            (Error::Unauthorized, "Basic realm=\"YANuget\""),
            (Error::AdminUnauthorized, "Basic realm=\"YANuget Admin\""),
        ] {
            let resp = error.into_response();
            assert_eq!(
                resp.headers()
                    .get(axum::http::header::WWW_AUTHENTICATE)
                    .and_then(|v| v.to_str().ok()),
                Some(realm)
            );
        }
    }
}
