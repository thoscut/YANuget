//! NuGet v2 (OData): `FindPackagesById()` answers an Atom feed, one entry per
//! version, paged by a `<link rel="next">`. The Chocolatey Community
//! Repository speaks this.

use quick_xml::events::{BytesStart, Event};
use quick_xml::XmlVersion;
use reqwest::Url;
use yanuget_pkg::NuGetVersion;

use super::{DependencyRef, HashAlgorithm, PackageHash, PackageInfo, UpstreamError};

/// Deeper than any real feed nests.
const MAX_DEPTH: usize = 32;

/// The query for every version of `id`.
pub fn find_url(base: &Url, id: &str) -> Result<Url, UpstreamError> {
    // A NuGet id holds no quote; doubling one is OData's escape all the same.
    let mut url = base
        .join("FindPackagesById()")
        .map_err(|e| UpstreamError::Invalid(format!("upstream URL: {e}")))?;
    url.query_pairs_mut()
        .append_pair("id", &format!("'{}'", id.replace('\'', "''")))
        .append_pair("semVerLevel", "2.0.0");
    Ok(url)
}

/// Where a version downloads from when its entry names no content URL.
pub fn package_url(base: &Url, id: &str, version: &str) -> Result<Url, UpstreamError> {
    base.join(&format!("package/{id}/{version}"))
        .map_err(|e| UpstreamError::Invalid(format!("upstream URL: {e}")))
}

/// One page: its entries for `id`, and the next page if there is one.
/// Entries for another id are dropped; an entry without a parsable version
/// is skipped with a debug log rather than failing the whole list.
pub fn parse_page(
    xml: &str,
    base: &Url,
    id: &str,
) -> Result<(Vec<PackageInfo>, Option<Url>), UpstreamError> {
    let invalid = |m: String| UpstreamError::Invalid(format!("upstream v2 feed: {m}"));
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut stack: Vec<String> = Vec::new();
    let mut entries = Vec::new();
    let mut next = None;
    let mut entry: Option<Entry> = None;
    let mut text = String::new();

    loop {
        let event = reader
            .read_event()
            .map_err(|e| invalid(format!("malformed XML: {e}")))?;
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(event, Event::Empty(_));
                if stack.len() >= MAX_DEPTH {
                    return Err(invalid(format!("nests deeper than {MAX_DEPTH} elements")));
                }
                let local = e.local_name().as_ref().to_string();
                match (stack.len(), local.as_str()) {
                    (1, "entry") if stack[0] == "feed" => entry = Some(Entry::default()),
                    (1, "link") if stack[0] == "feed" => {
                        if attr(e, "rel")?.as_deref() == Some("next") {
                            if let Some(href) = attr(e, "href")? {
                                next =
                                    Some(base.join(&href).map_err(|e| {
                                        invalid(format!("next link {href:?}: {e}"))
                                    })?);
                            }
                        }
                    }
                    (2, "content") if stack[1] == "entry" => {
                        if let Some(entry) = entry.as_mut() {
                            entry.content = attr(e, "src")?;
                        }
                    }
                    _ => {}
                }
                if !empty {
                    stack.push(local);
                    text.clear();
                }
            }
            Event::Text(e) => text.push_str(&e.xml10_content()),
            Event::CData(e) => text.push_str(&e.xml10_content()),
            Event::GeneralRef(e) => {
                let c = match e.resolve_char_ref() {
                    Ok(Some(c)) => c,
                    _ => match &*e {
                        "amp" => '&',
                        "lt" => '<',
                        "gt" => '>',
                        "quot" => '"',
                        "apos" => '\'',
                        other => return Err(invalid(format!("undefined entity &{other};"))),
                    },
                };
                text.push(c);
            }
            Event::End(_) => {
                let Some(local) = stack.pop() else {
                    return Err(invalid("unbalanced end tag".into()));
                };
                if let Some(e) = entry.as_mut() {
                    match (stack.len(), local.as_str()) {
                        (2, "title") if stack[1] == "entry" => e.title = Some(take(&mut text)),
                        (3, field) if stack[2] == "properties" => {
                            e.properties.push((field.to_string(), take(&mut text)))
                        }
                        (1, "entry") => {
                            if let Some(info) = entry.take().and_then(|e| e.into_info(base, id)) {
                                entries.push(info);
                            }
                        }
                        _ => {}
                    }
                }
                text.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err(invalid("the document ends inside an element".into()));
    }
    Ok((entries, next))
}

fn take(text: &mut String) -> String {
    std::mem::take(text).trim().to_string()
}

fn attr(e: &BytesStart, name: &str) -> Result<Option<String>, UpstreamError> {
    for a in e.attributes() {
        let a = a.map_err(|err| UpstreamError::Invalid(format!("malformed attribute: {err}")))?;
        if a.key.local_name().as_ref() == name {
            let v = a
                .normalized_value(XmlVersion::Implicit1_0)
                .map_err(|err| UpstreamError::Invalid(format!("malformed attribute: {err}")))?;
            return Ok(Some(v.into_owned()));
        }
    }
    Ok(None)
}

#[derive(Debug, Default)]
struct Entry {
    title: Option<String>,
    content: Option<String>,
    properties: Vec<(String, String)>,
}

impl Entry {
    fn prop(&self, name: &str) -> Option<&str> {
        self.properties
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    }

    fn into_info(self, base: &Url, want: &str) -> Option<PackageInfo> {
        let id = self.prop("Id").or(self.title.as_deref())?.to_string();
        if !id.eq_ignore_ascii_case(want) {
            return None;
        }
        let raw = self
            .prop("NormalizedVersion")
            .or(self.prop("Version"))?
            .to_string();
        let version = match NuGetVersion::parse(&raw) {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(id, version = raw, error = %e, "skipping an upstream entry");
                return None;
            }
        };
        // `Listed` where the feed has it; otherwise NuGet's convention, a
        // publish date in 1900, marks an unlisted version.
        let listed = match self.prop("Listed") {
            Some(v) => v.eq_ignore_ascii_case("true"),
            None => !self
                .prop("Published")
                .is_some_and(|p| p.starts_with("1900")),
        };
        let hash = self.prop("PackageHash").and_then(|h| {
            let algorithm =
                HashAlgorithm::parse(self.prop("PackageHashAlgorithm").unwrap_or("SHA512"))?;
            PackageHash::from_base64(algorithm, h)
        });
        let size = self.prop("PackageSize").and_then(|s| s.parse().ok());
        let dependencies = self
            .prop("Dependencies")
            .map(parse_dependencies)
            .unwrap_or_default();
        let download_url = match self.content.as_deref().map(|c| base.join(c)) {
            Some(Ok(url)) => url,
            _ => package_url(base, &id, &version.normalized()).ok()?,
        };
        Some(PackageInfo {
            id,
            version,
            listed,
            hash,
            size,
            dependencies,
            download_url,
        })
    }
}

/// v2's dependency list: `id:range:framework` joined by `|`, any part
/// possibly empty. The same id under several frameworks is listed once per
/// distinct range.
pub fn parse_dependencies(s: &str) -> Vec<DependencyRef> {
    let mut out: Vec<DependencyRef> = Vec::new();
    for item in s.split('|') {
        let mut parts = item.split(':');
        let id = parts.next().unwrap_or("").trim();
        if id.is_empty() {
            continue;
        }
        let range = parts
            .next()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(str::to_string);
        let dep = DependencyRef {
            id: id.to_string(),
            range,
        };
        if !out.contains(&dep) {
            out.push(dep);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xml:base="https://community.chocolatey.org/api/v2/" xmlns="http://www.w3.org/2005/Atom"
      xmlns:d="http://schemas.microsoft.com/ado/2007/08/dataservices"
      xmlns:m="http://schemas.microsoft.com/ado/2007/08/dataservices/metadata">
  <title type="text">FindPackagesById</title>
  <entry>
    <id>https://community.chocolatey.org/api/v2/Packages(Id='git',Version='2.50.1')</id>
    <title type="text">git</title>
    <content type="application/zip" src="https://community.chocolatey.org/api/v2/package/git/2.50.1" />
    <m:properties>
      <d:Version>2.50.1</d:Version>
      <d:Published m:type="Edm.DateTime">2025-07-08T10:00:00</d:Published>
      <d:PackageHash>q6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urqw==</d:PackageHash>
      <d:PackageHashAlgorithm>SHA512</d:PackageHashAlgorithm>
      <d:PackageSize m:type="Edm.Int64">5821</d:PackageSize>
      <d:Dependencies>git.install:[2.50.1]:|chocolatey-core.extension:1.3.3:net45|git.install:[2.50.1]:net45</d:Dependencies>
    </m:properties>
  </entry>
  <entry>
    <title type="text">git</title>
    <m:properties>
      <d:Version>2.51.0-rc1</d:Version>
      <d:Published m:type="Edm.DateTime">1900-01-01T00:00:00</d:Published>
      <d:Dependencies></d:Dependencies>
    </m:properties>
  </entry>
  <entry>
    <title type="text">other</title>
    <m:properties><d:Version>1.0.0</d:Version></m:properties>
  </entry>
  <entry>
    <title type="text">git</title>
    <m:properties><d:Version>not a version</d:Version></m:properties>
  </entry>
  <link rel="next" href="https://community.chocolatey.org/api/v2/FindPackagesById?id='git'&amp;$skiptoken='git','2.51.0'" />
</feed>"#;

    #[test]
    fn a_page_parses() {
        let base = Url::parse("https://community.chocolatey.org/api/v2/").unwrap();
        let (entries, next) = parse_page(FEED, &base, "Git").unwrap();
        assert_eq!(entries.len(), 2);

        let a = &entries[0];
        assert_eq!(a.id, "git");
        assert_eq!(a.version.normalized(), "2.50.1");
        assert!(a.listed);
        assert_eq!(a.size, Some(5821));
        let hash = a.hash.as_ref().unwrap();
        assert_eq!(hash.algorithm, HashAlgorithm::Sha512);
        assert_eq!(hash.digest, [0xab; 64]);
        assert_eq!(
            a.download_url.as_str(),
            "https://community.chocolatey.org/api/v2/package/git/2.50.1"
        );
        let deps: Vec<(&str, Option<&str>)> = a
            .dependencies
            .iter()
            .map(|d| (d.id.as_str(), d.range.as_deref()))
            .collect();
        assert_eq!(
            deps,
            [
                ("git.install", Some("[2.50.1]")),
                ("chocolatey-core.extension", Some("1.3.3"))
            ]
        );

        let b = &entries[1];
        assert!(!b.listed);
        assert!(b.version.is_prerelease());
        assert!(b.hash.is_none() && b.dependencies.is_empty());
        assert_eq!(
            b.download_url.as_str(),
            "https://community.chocolatey.org/api/v2/package/git/2.51.0-rc1"
        );

        let next = next.unwrap();
        assert!(next.as_str().contains("skiptoken"), "{next}");
    }

    #[test]
    fn the_query_quotes_the_id() {
        let base = Url::parse("https://feed.example/api/v2/").unwrap();
        let url = find_url(&base, "git.install").unwrap();
        assert_eq!(
            url.as_str(),
            "https://feed.example/api/v2/FindPackagesById()?id=%27git.install%27&semVerLevel=2.0.0"
        );
    }

    #[test]
    fn broken_documents_are_errors() {
        let base = Url::parse("https://feed.example/api/v2/").unwrap();
        for bad in ["<feed><entry>", "<feed>&bogus;</feed>", "</feed>"] {
            assert!(parse_page(bad, &base, "git").is_err(), "{bad}");
        }
        let deep = "<a>".repeat(100);
        assert!(parse_page(&deep, &base, "git").is_err());
    }
}
