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

    /// A push named an id/version this feed cannot take, with why and what to
    /// do instead. Same meaning as [`Error::PackageAlreadyExists`] to callers
    /// that only need to know it is already there.
    #[error("{0}")]
    VersionExists(String),

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

    /// An upload stalled: no bytes arrived for longer than the idle limit.
    #[error("upload timed out: {0}")]
    UploadTimeout(String),

    /// Accepting the request would leave the storage volume too full.
    #[error("insufficient storage: {0}")]
    InsufficientStorage(String),

    /// The request conflicts with the resource's current state (for example
    /// a resumable upload continued from the wrong offset).
    #[error("conflict: {0}")]
    Conflict(String),

    /// The caller is known, but the action is not allowed here.
    #[error("forbidden: {0}")]
    Forbidden(String),

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
            Error::PackageAlreadyExists | Error::VersionExists(_) => StatusCode::CONFLICT,
            Error::InvalidPackage(_) => StatusCode::BAD_REQUEST,
            Error::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Error::Unauthorized => StatusCode::UNAUTHORIZED,
            Error::AdminUnauthorized => StatusCode::UNAUTHORIZED,
            Error::BadRequest(_) => StatusCode::BAD_REQUEST,
            Error::PolicyViolation(_) => StatusCode::FORBIDDEN,
            Error::InvalidVersion(_) => StatusCode::BAD_REQUEST,
            Error::UploadTimeout(_) => StatusCode::REQUEST_TIMEOUT,
            Error::InsufficientStorage(_) => StatusCode::INSUFFICIENT_STORAGE,
            Error::Conflict(_) => StatusCode::CONFLICT,
            Error::Forbidden(_) => StatusCode::FORBIDDEN,
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
        let reason = status
            .is_client_error()
            .then(|| reason_phrase(&message))
            .flatten();
        let body = Json(json!({ "error": message }));
        let mut response = (status, body).into_response();
        // NuGet and Chocolatey print only the status line of a failed push —
        // `409 (Conflict)` said nothing about why, or what to do. They print a
        // server's own reason phrase in its place, as nuget.org uses it, so a
        // client error carries its message there too (HTTP/1 only; HTTP/2 has
        // no reason phrase). The JSON body is unchanged.
        if let Some(reason) = reason {
            response.extensions_mut().insert(reason);
        }
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

/// Longest reason phrase sent; clients print it on one line.
const MAX_REASON: usize = 400;

/// `message` as an HTTP/1 reason phrase: printable ASCII only (a CR or LF
/// could otherwise end the status line early), anything else replaced, and
/// cut to [`MAX_REASON`].
fn reason_phrase(message: &str) -> Option<hyper::ext::ReasonPhrase> {
    let mut text: String = message
        .chars()
        .map(|c| match c {
            ' '..='~' => c,
            '\u{2014}' | '\u{2013}' => '-',
            '\u{201c}' | '\u{201d}' => '"',
            '\u{2018}' | '\u{2019}' => '\'',
            _ => '?',
        })
        .take(MAX_REASON)
        .collect();
    text = text.trim().to_string();
    if text.is_empty() {
        return None;
    }
    hyper::ext::ReasonPhrase::try_from(text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_error_says_why_in_its_status_line() {
        let response = Error::VersionExists("pkg 1.0.0 already exists".into()).into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let reason = response
            .extensions()
            .get::<hyper::ext::ReasonPhrase>()
            .expect("a reason phrase");
        assert_eq!(reason.as_bytes(), b"pkg 1.0.0 already exists");
        // A server fault keeps its details to the log.
        let fault = Error::Other(anyhow::anyhow!("/srv/secret/path")).into_response();
        assert!(fault
            .extensions()
            .get::<hyper::ext::ReasonPhrase>()
            .is_none());
        // Nothing can end the status line early or smuggle a header.
        let nasty = reason_phrase("a\r\nX-Evil: 1\u{2014}\u{e9}").unwrap();
        assert_eq!(nasty.as_bytes(), b"a??X-Evil: 1-?");
    }

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
