//! NuGet v3: the service index names the resources. Versions come from the
//! registration (which knows what is listed, the dependencies and the
//! download URL), or from the flat container when a feed has no registration.

use reqwest::Url;
use serde_json::Value;
use yanuget_pkg::NuGetVersion;

use super::{DependencyRef, HashAlgorithm, PackageHash, PackageInfo, UpstreamError};

/// The resources the client uses.
#[derive(Debug, Clone)]
pub struct Resources {
    pub registration: Option<Url>,
    pub flat_container: Option<Url>,
}

/// Registration types in order of preference: the SemVer 2.0.0 ones first,
/// since an older one hides SemVer 2.0.0 versions.
const REGISTRATION_TYPES: [&str; 4] = [
    "RegistrationsBaseUrl/3.6.0",
    "RegistrationsBaseUrl/Versioned",
    "RegistrationsBaseUrl/3.4.0",
    "RegistrationsBaseUrl",
];

/// Read the service index.
pub fn resources(index: &Value, base: &Url) -> Result<Resources, UpstreamError> {
    let list = index["resources"].as_array().ok_or_else(|| {
        UpstreamError::Invalid("upstream service index lists no resources".into())
    })?;
    let find = |types: &[&str]| -> Option<Url> {
        types.iter().find_map(|t| {
            list.iter()
                .filter(|r| match &r["@type"] {
                    Value::String(s) => s == t,
                    Value::Array(a) => a.iter().any(|s| s.as_str() == Some(t)),
                    _ => false,
                })
                .find_map(|r| r["@id"].as_str())
                .and_then(|id| base.join(id).ok())
                .map(with_slash)
        })
    };
    let resources = Resources {
        registration: find(&REGISTRATION_TYPES),
        flat_container: find(&["PackageBaseAddress/3.0.0"]),
    };
    if resources.registration.is_none() && resources.flat_container.is_none() {
        return Err(UpstreamError::Invalid(
            "upstream service index has neither a registration nor a flat container".into(),
        ));
    }
    Ok(resources)
}

fn with_slash(mut url: Url) -> Url {
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    url
}

pub fn registration_url(base: &Url, id: &str) -> Result<Url, UpstreamError> {
    join(base, &format!("{}/index.json", id.to_ascii_lowercase()))
}

pub fn flat_versions_url(base: &Url, id: &str) -> Result<Url, UpstreamError> {
    join(base, &format!("{}/index.json", id.to_ascii_lowercase()))
}

pub fn flat_package_url(base: &Url, id: &str, version: &str) -> Result<Url, UpstreamError> {
    let id = id.to_ascii_lowercase();
    let version = version.to_ascii_lowercase();
    join(base, &format!("{id}/{version}/{id}.{version}.nupkg"))
}

pub fn flat_nuspec_url(base: &Url, id: &str, version: &str) -> Result<Url, UpstreamError> {
    let id = id.to_ascii_lowercase();
    let version = version.to_ascii_lowercase();
    join(base, &format!("{id}/{version}/{id}.nuspec"))
}

fn join(base: &Url, path: &str) -> Result<Url, UpstreamError> {
    base.join(path)
        .map_err(|e| UpstreamError::Invalid(format!("upstream URL: {e}")))
}

/// A registration index's pages: each either inline (`items`) or to be
/// fetched (`@id`).
pub enum Page {
    Inline(Vec<Value>),
    Remote(Url),
}

pub fn pages(index: &Value, base: &Url) -> Result<Vec<Page>, UpstreamError> {
    let items = index["items"]
        .as_array()
        .ok_or_else(|| UpstreamError::Invalid("upstream registration has no items".into()))?;
    items
        .iter()
        .map(|page| match page["items"].as_array() {
            Some(leaves) => Ok(Page::Inline(leaves.clone())),
            None => page["@id"]
                .as_str()
                .and_then(|id| base.join(id).ok())
                .map(Page::Remote)
                .ok_or_else(|| {
                    UpstreamError::Invalid("upstream registration page has no @id".into())
                }),
        })
        .collect()
}

/// A fetched page's leaves.
pub fn page_leaves(page: &Value) -> Result<Vec<Value>, UpstreamError> {
    page["items"]
        .as_array()
        .cloned()
        .ok_or_else(|| UpstreamError::Invalid("upstream registration page has no items".into()))
}

/// One registration leaf, for `want`. `None` for another id or a version
/// that does not parse.
pub fn leaf(
    leaf: &Value,
    want: &str,
    base: &Url,
    flat: Option<&Url>,
) -> Result<Option<PackageInfo>, UpstreamError> {
    let entry = &leaf["catalogEntry"];
    let Some(id) = entry["id"].as_str() else {
        return Ok(None);
    };
    if !id.eq_ignore_ascii_case(want) {
        return Ok(None);
    }
    let Some(version) = entry["version"]
        .as_str()
        .and_then(|v| NuGetVersion::parse(v).ok())
    else {
        return Ok(None);
    };
    let content = leaf["packageContent"]
        .as_str()
        .or(entry["packageContent"].as_str())
        .and_then(|c| base.join(c).ok());
    let download_url = match (content, flat) {
        (Some(url), _) => url,
        (None, Some(flat)) => flat_package_url(flat, id, &version.normalized())?,
        (None, None) => return Ok(None),
    };
    let hash = entry["packageHash"].as_str().and_then(|h| {
        let algorithm = HashAlgorithm::parse(entry["packageHashAlgorithm"].as_str()?)?;
        PackageHash::from_base64(algorithm, h)
    });
    let mut dependencies: Vec<DependencyRef> = Vec::new();
    for group in entry["dependencyGroups"].as_array().into_iter().flatten() {
        for dep in group["dependencies"].as_array().into_iter().flatten() {
            let Some(dep_id) = dep["id"].as_str() else {
                continue;
            };
            let dep = DependencyRef {
                id: dep_id.to_string(),
                range: dep["range"].as_str().map(str::to_string),
            };
            if !dependencies.contains(&dep) {
                dependencies.push(dep);
            }
        }
    }
    Ok(Some(PackageInfo {
        id: id.to_string(),
        version,
        listed: entry["listed"].as_bool().unwrap_or(true),
        hash,
        size: entry["packageSize"].as_u64(),
        dependencies,
        download_url,
    }))
}

/// The flat container's version list, for a feed without a registration.
/// It cannot tell unlisted versions apart, so every version counts as listed.
pub fn flat_versions(doc: &Value, id: &str, flat: &Url) -> Result<Vec<PackageInfo>, UpstreamError> {
    let versions = doc["versions"].as_array().ok_or_else(|| {
        UpstreamError::Invalid("upstream flat container lists no versions".into())
    })?;
    let mut out = Vec::new();
    for v in versions.iter().filter_map(Value::as_str) {
        let Ok(version) = NuGetVersion::parse(v) else {
            continue;
        };
        out.push(PackageInfo {
            id: id.to_string(),
            download_url: flat_package_url(flat, id, &version.normalized())?,
            version,
            listed: true,
            hash: None,
            size: None,
            dependencies: Vec::new(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_service_index_prefers_semver2_registrations() {
        let base = Url::parse("https://feed.example/v3/index.json").unwrap();
        let index = json!({"resources": [
            {"@id": "https://feed.example/v3/registration/", "@type": "RegistrationsBaseUrl"},
            {"@id": "https://feed.example/v3/registration-semver2", "@type": ["RegistrationsBaseUrl/3.6.0"]},
            {"@id": "/v3/flat", "@type": "PackageBaseAddress/3.0.0"},
        ]});
        let r = resources(&index, &base).unwrap();
        assert_eq!(
            r.registration.unwrap().as_str(),
            "https://feed.example/v3/registration-semver2/"
        );
        assert_eq!(
            r.flat_container.unwrap().as_str(),
            "https://feed.example/v3/flat/"
        );
        assert!(resources(&json!({"resources": []}), &base).is_err());
        assert!(resources(&json!({}), &base).is_err());
    }

    #[test]
    fn leaves_parse() {
        let base = Url::parse("https://feed.example/v3/registration/git/index.json").unwrap();
        let flat = Url::parse("https://feed.example/v3/flat/").unwrap();
        let l = json!({
            "catalogEntry": {
                "id": "Git", "version": "2.51.0", "listed": false,
                "packageHash": "q6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6urqw==", "packageHashAlgorithm": "SHA512",
                "dependencyGroups": [
                    {"dependencies": [{"id": "git.install", "range": "[2.51.0, 2.51.0]"}]},
                    {"targetFramework": "net45", "dependencies": [{"id": "git.install", "range": "[2.51.0, 2.51.0]"}]}
                ]
            }
        });
        let info = leaf(&l, "git", &base, Some(&flat)).unwrap().unwrap();
        assert!(!info.listed);
        assert_eq!(info.dependencies.len(), 1);
        assert_eq!(info.hash.unwrap().digest, [0xab; 64]);
        assert_eq!(
            info.download_url.as_str(),
            "https://feed.example/v3/flat/git/2.51.0/git.2.51.0.nupkg"
        );
        assert!(leaf(&l, "other", &base, Some(&flat)).unwrap().is_none());
        // Without a content URL or a flat container there is nothing to fetch.
        assert!(leaf(&l, "git", &base, None).unwrap().is_none());
    }
}
