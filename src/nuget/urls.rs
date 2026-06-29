//! Construction of absolute resource URLs for the V3 protocol.

use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};

/// Builds absolute URLs rooted at the server's externally visible base.
///
/// The base may come from configuration or be derived per-request from the
/// `Host`/forwarded headers, so the feed works behind reverse proxies and at
/// arbitrary path prefixes.
#[derive(Debug, Clone)]
pub struct UrlBuilder {
    /// Base URL with any trailing slash removed (e.g. `https://host/nuget`).
    base: String,
    /// App-relative path prefix for the current feed (`""` or `/{feed}`), used
    /// to build the gallery/admin links that stay within the feed.
    prefix: String,
}

impl UrlBuilder {
    /// Create a builder from a base URL. A trailing slash is normalized away.
    pub fn new(base: impl Into<String>) -> Self {
        let mut base = base.into();
        while base.ends_with('/') {
            base.pop();
        }
        Self {
            base,
            prefix: String::new(),
        }
    }

    /// Create a builder rooted at `root` with an app path `prefix` (e.g.
    /// `/stable`). Absolute resource URLs include the prefix; [`Self::app`]
    /// builds prefix-aware links for the HTML UI.
    pub fn with_prefix(root: impl Into<String>, prefix: &str) -> Self {
        let root = root.into();
        let prefix = prefix.trim_end_matches('/').to_string();
        let mut b = Self::new(format!("{}{}", root.trim_end_matches('/'), prefix));
        b.prefix = prefix;
        b
    }

    /// The configured base URL (no trailing slash).
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Build an app-relative link within the current feed. `path` must start
    /// with `/`; the feed prefix is prepended. The feed root (`/`) maps to the
    /// bare prefix (e.g. `/stable`) for a prefixed feed — which is where a
    /// nested feed's index route lives — or to `/` for the root feed.
    pub fn app(&self, path: &str) -> String {
        if path == "/" {
            if self.prefix.is_empty() {
                "/".to_string()
            } else {
                self.prefix.clone()
            }
        } else {
            format!("{}{}", self.prefix, path)
        }
    }

    /// `/v3/index.json`
    pub fn service_index(&self) -> String {
        format!("{}/v3/index.json", self.base)
    }

    /// Flat-container base, with trailing slash (`PackageBaseAddress`).
    pub fn package_base_address(&self) -> String {
        format!("{}/v3/package/", self.base)
    }

    /// `/v3/package/{id}/index.json`
    pub fn package_versions_index(&self, lower_id: &str) -> String {
        format!("{}/v3/package/{}/index.json", self.base, enc(lower_id))
    }

    /// `/v3/package/{id}/{version}/{id}.{version}.nupkg`
    pub fn package_download(&self, lower_id: &str, normalized_version: &str) -> String {
        let v = normalized_version.to_lowercase();
        format!(
            "{}/v3/package/{}/{}/{}.{}.nupkg",
            self.base,
            enc(lower_id),
            enc(&v),
            enc(lower_id),
            enc(&v),
        )
    }

    /// Registration base, with trailing slash (`RegistrationsBaseUrl`).
    pub fn registration_base(&self) -> String {
        format!("{}/v3/registration/", self.base)
    }

    /// `/v3/registration/{id}/index.json`
    pub fn registration_index(&self, lower_id: &str) -> String {
        format!("{}/v3/registration/{}/index.json", self.base, enc(lower_id))
    }

    /// `/v3/registration/{id}/{version}.json`
    pub fn registration_leaf(&self, lower_id: &str, normalized_version: &str) -> String {
        format!(
            "{}/v3/registration/{}/{}.json",
            self.base,
            enc(lower_id),
            enc(&normalized_version.to_lowercase()),
        )
    }

    /// `/v3/registration/{id}/page/{lower}/{upper}.json` — a registration page
    /// covering the version range `[lower, upper]`.
    pub fn registration_page(&self, lower_id: &str, lower: &str, upper: &str) -> String {
        format!(
            "{}/v3/registration/{}/page/{}/{}.json",
            self.base,
            enc(lower_id),
            enc(&lower.to_lowercase()),
            enc(&upper.to_lowercase()),
        )
    }

    /// `/v3/search`
    pub fn search(&self) -> String {
        format!("{}/v3/search", self.base)
    }

    /// `/v3/autocomplete`
    pub fn autocomplete(&self) -> String {
        format!("{}/v3/autocomplete", self.base)
    }

    /// `/api/v2/package` — the publish endpoint.
    pub fn publish(&self) -> String {
        format!("{}/api/v2/package", self.base)
    }

    /// `/api/v2/symbol` — the symbol publish endpoint.
    pub fn symbol_publish(&self) -> String {
        format!("{}/api/v2/symbol", self.base)
    }

    /// Symbol-server read base, with trailing slash. A debugger appends
    /// `{file}/{key}/{file}` to fetch a PDB.
    pub fn symbol_server(&self) -> String {
        format!("{}/download/symbols/", self.base)
    }

    /// URI *template* for a package's gallery detail page, with literal `{id}`
    /// and `{version}` placeholders the client substitutes
    /// (`PackageDetailsUriTemplate`).
    pub fn package_details_template(&self) -> String {
        format!("{}/packages/{{id}}/{{version}}", self.base)
    }
}

/// Percent-encode a single path segment. Ids/versions are already restricted to
/// a safe character set, but encoding keeps URLs valid for any stray symbols.
fn enc(segment: &str) -> String {
    // Keep the characters legal in NuGet ids/versions readable.
    const KEEP: &percent_encoding::AsciiSet = &NON_ALPHANUMERIC
        .remove(b'.')
        .remove(b'-')
        .remove(b'_')
        .remove(b'~');
    utf8_percent_encode(segment, KEEP).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_trailing_slash() {
        let u = UrlBuilder::new("https://host/nuget/");
        assert_eq!(u.base(), "https://host/nuget");
        assert_eq!(u.service_index(), "https://host/nuget/v3/index.json");
    }

    #[test]
    fn builds_download_url() {
        let u = UrlBuilder::new("https://host");
        assert_eq!(
            u.package_download("contoso.utils", "1.0.0"),
            "https://host/v3/package/contoso.utils/1.0.0/contoso.utils.1.0.0.nupkg"
        );
    }

    #[test]
    fn registration_urls() {
        let u = UrlBuilder::new("https://host");
        assert_eq!(
            u.registration_index("contoso.utils"),
            "https://host/v3/registration/contoso.utils/index.json"
        );
        assert_eq!(
            u.registration_leaf("contoso.utils", "1.0.0"),
            "https://host/v3/registration/contoso.utils/1.0.0.json"
        );
    }
}
