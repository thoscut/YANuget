//! A client for a running server's `/admin` actions — what `yanuget admin`
//! drives, so a promotion, approval or delete can be scripted without a
//! browser or hand-written `curl` calls.
//!
//! Each action is two requests: the package's admin page, which proves the
//! key, confirms the version's package is in the feed and carries the CSRF
//! token every admin POST needs (see `docs/api.md`, *CSRF*), then the POST
//! itself with that token in `X-CSRF-Token`. The server answers a successful
//! action with `303 See Other`; anything else carries a JSON `error` message,
//! which is shown as is.

use std::time::Duration;

use anyhow::{bail, Context};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use reqwest::{header, redirect, StatusCode};

/// One action on one version, as the admin page offers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Promote,
    Approve,
    Enable,
    Disable,
    Pin,
    Unpin,
    Delete,
}

impl Action {
    /// The last segment of the action's admin route.
    pub fn route(self) -> &'static str {
        match self {
            Self::Promote => "promote",
            Self::Approve => "approve",
            Self::Enable => "enable",
            Self::Disable => "disable",
            Self::Pin => "pin",
            Self::Unpin => "unpin",
            Self::Delete => "delete",
        }
    }

    fn past_tense(self) -> &'static str {
        match self {
            Self::Promote => "promoted",
            Self::Approve => "approved",
            Self::Enable => "enabled",
            Self::Disable => "disabled",
            Self::Pin => "pinned",
            Self::Unpin => "unpinned",
            Self::Delete => "deleted",
        }
    }
}

/// Where and as whom to act.
pub struct AdminClient {
    http: reqwest::Client,
    /// The feed's base URL, without a trailing slash (`https://host/stage`,
    /// or `https://host` for a single implicit feed).
    feed_url: String,
    key: String,
}

impl std::fmt::Debug for AdminClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminClient")
            .field("feed_url", &crate::mirror::redact_url(&self.feed_url))
            .field("key", &"<redacted>")
            .finish()
    }
}

/// How the client treats the server's certificate.
#[derive(Debug, Clone, Default)]
pub enum CertPolicy {
    /// The system trust store.
    #[default]
    System,
    /// The system store plus the CA certificates in this PEM bundle.
    ExtraCa(Vec<u8>),
    /// The server on this machine: any certificate is accepted (it names the
    /// public host, or is the generated self-signed one, and the connection
    /// never leaves the machine), no proxy is used, and with `pin` the URL's
    /// host name is resolved to the server's own address, so the request
    /// carries the name `allowed_hosts` expects.
    ThisMachine {
        pin: Option<(String, std::net::SocketAddr)>,
    },
}

impl AdminClient {
    pub fn new(feed_url: &str, key: &str, certs: CertPolicy) -> anyhow::Result<Self> {
        let key = key.trim();
        if key.is_empty() {
            bail!("the admin key is empty");
        }
        let mut builder = reqwest::Client::builder()
            // A 303 is the answer; following it would only fetch a page.
            .redirect(redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .user_agent(concat!("yanuget-admin/", env!("CARGO_PKG_VERSION")));
        match certs {
            CertPolicy::System => {}
            CertPolicy::ExtraCa(pem) => {
                for cert in reqwest::Certificate::from_pem_bundle(&pem)
                    .context("cannot read the CA certificate file")?
                {
                    builder = builder.add_root_certificate(cert);
                }
            }
            CertPolicy::ThisMachine { pin } => {
                // A proxy from the environment has no business carrying a
                // local call, least of all one with the admin key in it.
                builder = builder.danger_accept_invalid_certs(true).no_proxy();
                if let Some((name, addr)) = pin {
                    builder = builder.resolve(&name, addr);
                }
            }
        }
        Ok(Self {
            http: builder.build()?,
            feed_url: feed_url.trim_end_matches('/').to_string(),
            key: key.to_string(),
        })
    }

    /// Apply `action` to `id` `version` and say, in one line, what happened.
    pub async fn run(&self, action: Action, id: &str, version: &str) -> anyhow::Result<String> {
        let package = format!("{}/admin/packages/{}", self.feed_url, segment(id));
        let token = self.csrf_token(&package, id).await?;
        let url = format!("{package}/{}/{}", segment(version), action.route());
        let resp = self
            .http
            .post(&url)
            .basic_auth("admin", Some(&self.key))
            .header(CSRF_HEADER, token)
            .header(header::ACCEPT, "application/json")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body("")
            .send()
            .await
            .with_context(|| format!("no answer from {}", crate::mirror::redact_url(&url)))?;
        let status = resp.status();
        if !status.is_redirection() {
            bail!("{} {id} {version}: {}", action.route(), failure(resp).await);
        }
        let location = resp
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        Ok(describe(action, id, version, location))
    }

    /// Fetch the package's admin page and the CSRF token it carries.
    async fn csrf_token(&self, package_url: &str, id: &str) -> anyhow::Result<String> {
        let resp = self
            .http
            .get(package_url)
            .basic_auth("admin", Some(&self.key))
            // The page itself is HTML either way; this keeps an error JSON.
            .header(header::ACCEPT, "application/json")
            .send()
            .await
            .with_context(|| {
                format!(
                    "no answer from {}",
                    crate::mirror::redact_url(&self.feed_url)
                )
            })?;
        match resp.status() {
            s if s.is_success() => {}
            StatusCode::UNAUTHORIZED => bail!(
                "the admin key was refused by {} (check the feed's admin_api_key)",
                crate::mirror::redact_url(&self.feed_url)
            ),
            StatusCode::NOT_FOUND => {
                // The package page is missing both when the package is not
                // in this feed and when the feed has no admin area at all;
                // the dashboard tells the two apart.
                let dashboard = self
                    .http
                    .get(format!("{}/admin", self.feed_url))
                    .basic_auth("admin", Some(&self.key))
                    .send()
                    .await?;
                if dashboard.status() == StatusCode::NOT_FOUND {
                    bail!(
                        "{} has no admin area: the feed does not exist there, or it has \
                         no admin_api_key",
                        crate::mirror::redact_url(&self.feed_url)
                    );
                }
                bail!(
                    "{id} is not in the feed at {}",
                    crate::mirror::redact_url(&self.feed_url)
                );
            }
            _ => bail!("{}", failure(resp).await),
        }
        let page = resp.text().await?;
        extract_csrf(&page).context("the admin page carries no CSRF token")
    }
}

const CSRF_HEADER: &str = "x-csrf-token";

/// A path segment: ids and normalized versions are `[A-Za-z0-9._-]` plus a
/// `+` in build metadata, so anything else is encoded rather than sent raw.
fn segment(s: &str) -> String {
    const KEEP: &AsciiSet = &NON_ALPHANUMERIC.remove(b'.').remove(b'-').remove(b'_');
    utf8_percent_encode(s.trim(), KEEP).to_string()
}

/// The status and the server's own message for a failed request.
async fn failure(resp: reqwest::Response) -> String {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    // A proxy in front may still answer with a page; its markup says nothing.
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("error")?.as_str().map(str::to_string))
        .unwrap_or_else(|| {
            let text = body.trim();
            if text.starts_with('<') {
                String::new()
            } else {
                text.chars().take(200).collect()
            }
        });
    if message.is_empty() {
        status.to_string()
    } else {
        format!("{status}: {message}")
    }
}

/// The value of the admin page's hidden CSRF field.
fn extract_csrf(page: &str) -> Option<String> {
    // The field the admin forms embed (`ui::CSRF_FIELD`).
    const MARKER: &str = "name=\"_csrf\" value=\"";
    let start = page.find(MARKER)? + MARKER.len();
    let len = page[start..].find('"')?;
    let token = &page[start..start + len];
    // `{issued}.{base64url}`: nothing the page would have had to escape.
    (!token.is_empty()
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_')))
    .then(|| token.to_string())
}

/// One line on what the action did. After a promotion the server's redirect
/// says how the version arrived in the target feed (added, pending there, or
/// already there); a server that does not say so gets the plain line.
fn describe(action: Action, id: &str, version: &str, location: &str) -> String {
    let query = location.split_once('?').map_or("", |(_, q)| q);
    let field = |name: &str| {
        query
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .find(|(k, _)| *k == name)
            .map(|(_, v)| {
                percent_encoding::percent_decode_str(v)
                    .decode_utf8_lossy()
                    .into_owned()
            })
    };
    let count = |name: &str| field(name).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    if action == Action::Promote && field("done").as_deref() == Some("promote") {
        if let Some(target) = field("to") {
            if count("present") > 0 {
                return format!("{target} already holds {id} {version}; nothing changed there");
            }
            if count("pending") > 0 {
                return format!(
                    "promoted {id} {version} to {target}; it is pending there until approved \
                     (yanuget admin --feed {target} approve {id} {version})"
                );
            }
            return format!("promoted {id} {version} to {target}");
        }
    }
    format!("{} {id} {version}", action.past_tense())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_csrf_token_is_read_from_the_admin_form() {
        let page = r#"<form method="post" action="/x"><input type="hidden" name="_csrf" value="1760000000.abc-_DEF"><button>"#;
        assert_eq!(extract_csrf(page).as_deref(), Some("1760000000.abc-_DEF"));
        assert_eq!(extract_csrf("<p>no form</p>"), None);
        // Anything a token cannot contain means the page is not what we think.
        assert_eq!(
            extract_csrf(r#"name="_csrf" value="a&lt;b""#),
            None,
            "an escaped value is refused"
        );
    }

    #[test]
    fn segments_keep_ids_and_versions_readable() {
        assert_eq!(segment("Contoso.Utils"), "Contoso.Utils");
        assert_eq!(segment("1.0.0-beta.1"), "1.0.0-beta.1");
        assert_eq!(segment("1.0.0+abc"), "1.0.0%2Babc");
        assert_eq!(segment("../x"), "..%2Fx");
    }

    #[test]
    fn a_promotion_reports_how_the_version_arrived() {
        let base = "/dev/admin/packages/a?done=promote&to=stable&v=1.0.0";
        assert_eq!(
            describe(
                Action::Promote,
                "A",
                "1.0.0",
                &format!("{base}&added=0&pending=1&present=0")
            ),
            "promoted A 1.0.0 to stable; it is pending there until approved \
             (yanuget admin --feed stable approve A 1.0.0)"
        );
        assert_eq!(
            describe(
                Action::Promote,
                "A",
                "1.0.0",
                &format!("{base}&added=0&pending=0&present=1")
            ),
            "stable already holds A 1.0.0; nothing changed there"
        );
        assert_eq!(
            describe(
                Action::Promote,
                "A",
                "1.0.0",
                &format!("{base}&added=1&pending=0&present=0")
            ),
            "promoted A 1.0.0 to stable"
        );
        // A server that only redirects back to the page.
        assert_eq!(
            describe(Action::Promote, "A", "1.0.0", "/dev/admin/packages/a"),
            "promoted A 1.0.0"
        );
        assert_eq!(
            describe(Action::Approve, "A", "1.0.0", "/stable/admin/packages/a"),
            "approved A 1.0.0"
        );
    }
}
