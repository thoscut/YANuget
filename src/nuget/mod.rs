//! The NuGet V3 HTTP protocol: URL generation and JSON response shapes.
//!
//! This module is pure data transformation — it turns domain [`Package`]s and
//! [`SearchPage`]s into the exact JSON documents the NuGet client expects,
//! given a [`UrlBuilder`] that knows the server's externally visible base URL.
//! Keeping it free of I/O makes the protocol easy to unit-test.

mod urls;

pub use urls::UrlBuilder;

use chrono::SecondsFormat;
use serde_json::{json, Value};

use crate::database::{SearchGroup, SearchPage};
use crate::models::{DependencyGroup, Package};

/// Build the `/v3/index.json` service index document. When `web_ui_enabled`, a
/// `PackageDetailsUriTemplate` pointing at the HTML gallery is advertised so
/// clients (and Visual Studio) can link to a package's details page.
pub fn service_index(urls: &UrlBuilder, web_ui_enabled: bool) -> Value {
    // Each logical resource is advertised under every `@type` alias clients
    // look up, so old and new clients alike resolve them.
    let mut resources = Vec::new();
    let mut push = |url: String, types: &[&str], comment: &str| {
        for t in types {
            resources.push(json!({
                "@id": url,
                "@type": t,
                "comment": comment,
            }));
        }
    };

    push(
        urls.package_base_address(),
        &["PackageBaseAddress/3.0.0"],
        "Base URL of where NuGet packages are stored.",
    );
    push(
        urls.registration_base(),
        &[
            "RegistrationsBaseUrl",
            "RegistrationsBaseUrl/3.0.0-beta",
            "RegistrationsBaseUrl/3.0.0-rc",
            "RegistrationsBaseUrl/3.4.0",
            "RegistrationsBaseUrl/3.6.0",
            "RegistrationsBaseUrl/Versioned",
        ],
        "Base URL of package registration info (SemVer2 supported).",
    );
    push(
        urls.search(),
        &[
            "SearchQueryService",
            "SearchQueryService/3.0.0-beta",
            "SearchQueryService/3.0.0-rc",
            "SearchQueryService/3.5.0",
        ],
        "Query endpoint of NuGet Search service.",
    );
    push(
        urls.autocomplete(),
        &[
            "SearchAutocompleteService",
            "SearchAutocompleteService/3.0.0-beta",
            "SearchAutocompleteService/3.0.0-rc",
            "SearchAutocompleteService/3.5.0",
        ],
        "Autocomplete endpoint of NuGet Search service.",
    );
    push(
        urls.publish(),
        &["PackagePublish/2.0.0"],
        "Endpoint for pushing packages.",
    );
    push(
        urls.symbol_publish(),
        &["SymbolPackagePublish/4.9.0"],
        "Endpoint for pushing symbol packages.",
    );
    push(
        urls.symbol_server(),
        &["SymbolServer/4.9.0"],
        "Base URL for downloading symbols (SSQP).",
    );
    if web_ui_enabled {
        push(
            urls.package_details_template(),
            &["PackageDetailsUriTemplate/5.1.0"],
            "URI template for a package's details page.",
        );
    }

    json!({
        "version": "3.0.0",
        "resources": resources,
    })
}

/// Build the flat-container versions document
/// (`/v3/package/{id}/index.json`).
///
/// `versions` should already be the visible versions, sorted ascending.
pub fn flat_container_index(versions: &[String]) -> Value {
    json!({ "versions": versions })
}

/// Packages with fewer than this many versions inline all leaves into a single
/// registration page; larger sets are split into external pages the client
/// fetches on demand (matching nuget.org's behaviour).
const REGISTRATION_INLINE_MAX: usize = 128;
/// Versions per external registration page.
const REGISTRATION_PAGE_SIZE: usize = 64;

/// Build the registration index (`/v3/registration/{id}/index.json`).
///
/// For small packages all leaves are inlined into one page. Once a package has
/// [`REGISTRATION_INLINE_MAX`] or more versions, the index instead lists
/// external pages of [`REGISTRATION_PAGE_SIZE`] versions each (served by
/// [`registration_page`]), so the index document stays small. `packages` must be
/// sorted ascending by version and contain at least one element.
pub fn registration_index(urls: &UrlBuilder, id: &str, packages: &[Package]) -> Value {
    let lower_id = id.to_lowercase();
    let index_url = urls.registration_index(&lower_id);

    let items: Vec<Value> = if packages.len() < REGISTRATION_INLINE_MAX {
        vec![inline_page(urls, &lower_id, &index_url, packages)]
    } else {
        packages
            .chunks(REGISTRATION_PAGE_SIZE)
            .map(|chunk| {
                let lower = chunk
                    .first()
                    .map(|p| p.normalized_version())
                    .unwrap_or_default();
                let upper = chunk
                    .last()
                    .map(|p| p.normalized_version())
                    .unwrap_or_default();
                json!({
                    "@id": urls.registration_page(&lower_id, &lower, &upper),
                    "count": chunk.len(),
                    "lower": lower,
                    "upper": upper,
                })
            })
            .collect()
    };

    json!({
        "@id": index_url,
        "@type": ["catalog:CatalogRoot", "PackageRegistration", "catalog:Permalink"],
        "count": items.len(),
        "items": items,
    })
}

/// Build a single inline registration page object (used inside the index for
/// small packages).
fn inline_page(urls: &UrlBuilder, lower_id: &str, index_url: &str, packages: &[Package]) -> Value {
    let leaves: Vec<Value> = packages
        .iter()
        .map(|p| registration_leaf_item(urls, lower_id, p))
        .collect();
    let lower = packages
        .first()
        .map(|p| p.normalized_version())
        .unwrap_or_default();
    let upper = packages
        .last()
        .map(|p| p.normalized_version())
        .unwrap_or_default();
    json!({
        "@id": format!("{index_url}#page/{lower}/{upper}"),
        "count": packages.len(),
        "lower": lower,
        "upper": upper,
        "items": leaves,
    })
}

/// Build a standalone registration page document
/// (`/v3/registration/{id}/page/{lower}/{upper}.json`) with its leaves inlined.
/// `packages` is the slice of versions covered by this page, sorted ascending.
pub fn registration_page(urls: &UrlBuilder, id: &str, packages: &[Package]) -> Value {
    let lower_id = id.to_lowercase();
    let leaves: Vec<Value> = packages
        .iter()
        .map(|p| registration_leaf_item(urls, &lower_id, p))
        .collect();
    let lower = packages
        .first()
        .map(|p| p.normalized_version())
        .unwrap_or_default();
    let upper = packages
        .last()
        .map(|p| p.normalized_version())
        .unwrap_or_default();
    json!({
        "@id": urls.registration_page(&lower_id, &lower, &upper),
        "@type": "catalog:CatalogPage",
        "count": packages.len(),
        "lower": lower,
        "upper": upper,
        "parent": urls.registration_index(&lower_id),
        "items": leaves,
    })
}

/// Build a standalone registration leaf document
/// (`/v3/registration/{id}/{version}.json`).
pub fn registration_leaf(urls: &UrlBuilder, id: &str, package: &Package) -> Value {
    registration_leaf_item(urls, &id.to_lowercase(), package)
}

fn registration_leaf_item(urls: &UrlBuilder, lower_id: &str, p: &Package) -> Value {
    let version = p.normalized_version();
    let leaf_url = urls.registration_leaf(lower_id, &version);
    let content_url = urls.package_download(lower_id, &version);

    json!({
        "@id": leaf_url,
        "@type": "Package",
        "packageContent": content_url,
        "registration": urls.registration_index(lower_id),
        "catalogEntry": catalog_entry(urls, lower_id, p, &content_url),
    })
}

fn catalog_entry(urls: &UrlBuilder, lower_id: &str, p: &Package, content_url: &str) -> Value {
    let version = p.normalized_version();
    // NuGet signals an unlisted version by reporting a `published` date in the
    // year 1900, in addition to the explicit `listed: false` flag.
    let published = if p.listed {
        p.published.to_rfc3339_opts(SecondsFormat::Millis, true)
    } else {
        "1900-01-01T00:00:00.000Z".to_string()
    };
    let mut entry = json!({
        "@id": urls.registration_leaf(lower_id, &version),
        "@type": "PackageDetails",
        "id": p.id,
        "version": version,
        "authors": p.authors.join(", "),
        "description": p.description,
        "iconUrl": p.icon_url,
        "language": p.language,
        "licenseExpression": p.license_expression,
        "licenseUrl": p.license_url,
        "listed": p.listed,
        "minClientVersion": p.min_client_version,
        "packageContent": content_url,
        "projectUrl": p.project_url,
        "published": published,
        "releaseNotes": p.release_notes,
        "requireLicenseAcceptance": p.require_license_acceptance,
        "summary": p.summary,
        "tags": p.tags,
        "title": p.title,
    });
    // `dependencyGroups` is omitted entirely when the package has no
    // dependencies, matching nuget.org.
    if !p.dependencies.is_empty() {
        entry["dependencyGroups"] = dependency_groups(urls, lower_id, &version, &p.dependencies);
    }
    entry
}

fn dependency_groups(
    urls: &UrlBuilder,
    lower_id: &str,
    version: &str,
    groups: &[DependencyGroup],
) -> Value {
    let base = format!(
        "{}#dependencygroup",
        urls.registration_leaf(lower_id, version)
    );
    let items: Vec<Value> = groups
        .iter()
        .map(|g| {
            let tfm = g.target_framework.clone();
            let group_id = match &tfm {
                Some(f) => format!("{base}/{}", f.to_lowercase()),
                None => base.clone(),
            };
            let deps: Vec<Value> = g
                .dependencies
                .iter()
                .map(|d| {
                    json!({
                        "@id": format!("{group_id}/{}", d.id.to_lowercase()),
                        "@type": "PackageDependency",
                        "id": d.id,
                        "range": d.version_range,
                        "registration": urls.registration_index(&d.id.to_lowercase()),
                    })
                })
                .collect();
            json!({
                "@id": group_id,
                "@type": "PackageDependencyGroup",
                "targetFramework": tfm,
                "dependencies": deps,
            })
        })
        .collect();
    Value::Array(items)
}

/// Build the search response (`/v3/search`).
pub fn search_response(urls: &UrlBuilder, page: &SearchPage) -> Value {
    let data: Vec<Value> = page.groups.iter().map(|g| search_result(urls, g)).collect();
    json!({
        "@context": {
            "@vocab": "http://schema.nuget.org/schema#",
            "@base": urls.registration_base(),
        },
        "totalHits": page.total_hits,
        "data": data,
    })
}

fn search_result(urls: &UrlBuilder, group: &SearchGroup) -> Value {
    let latest = group.latest();
    let lower_id = latest.lower_id();
    let versions: Vec<Value> = group
        .packages
        .iter()
        .map(|p| {
            let v = p.normalized_version();
            json!({
                "@id": urls.registration_leaf(&lower_id, &v),
                "version": v,
                "downloads": p.downloads,
            })
        })
        .collect();
    let package_types: Vec<Value> = if latest.package_types.is_empty() {
        vec![json!({ "name": "Dependency" })]
    } else {
        latest
            .package_types
            .iter()
            .map(|t| json!({ "name": t.name }))
            .collect()
    };

    json!({
        "@type": "Package",
        "registration": urls.registration_index(&lower_id),
        "id": latest.id,
        "version": latest.normalized_version(),
        "description": latest.description,
        "summary": latest.summary,
        "title": latest.title,
        "iconUrl": latest.icon_url,
        "licenseUrl": latest.license_url,
        "projectUrl": latest.project_url,
        "tags": latest.tags,
        "authors": latest.authors,
        "totalDownloads": group.total_downloads(),
        "verified": false,
        "packageTypes": package_types,
        "versions": versions,
    })
}

/// Build the id-autocomplete response (`/v3/autocomplete`).
pub fn autocomplete_response(ids: &[String], total_hits: i64) -> Value {
    json!({
        "@context": { "@vocab": "http://schema.nuget.org/schema#" },
        "totalHits": total_hits,
        "data": ids,
    })
}

/// Build the version-enumeration response
/// (`/v3/autocomplete?id={id}`).
pub fn enumerate_versions_response(versions: &[String]) -> Value {
    json!({
        "@context": { "@vocab": "http://schema.nuget.org/schema#" },
        "totalHits": versions.len(),
        "data": versions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::NuGetVersion;
    use chrono::Utc;

    fn urls() -> UrlBuilder {
        UrlBuilder::new("https://nuget.example.com")
    }

    fn pkg(id: &str, version: &str) -> Package {
        Package {
            id: id.to_string(),
            version: NuGetVersion::parse(version).unwrap(),
            listed: true,
            enabled: true,
            authors: vec!["Alice".into(), "Bob".into()],
            description: "A test package".into(),
            icon_url: None,
            license_url: None,
            license_expression: Some("MIT".into()),
            project_url: Some("https://example.com".into()),
            repository_url: None,
            repository_type: None,
            min_client_version: None,
            release_notes: None,
            language: None,
            title: Some("Test".into()),
            summary: None,
            tags: vec!["a".into(), "b".into()],
            has_readme: false,
            has_embedded_icon: false,
            is_development_dependency: false,
            require_license_acceptance: false,
            is_semver2: false,
            package_size: 10,
            package_hash: "aGFzaA==".into(),
            package_hash_algorithm: "SHA512".into(),
            published: Utc::now(),
            downloads: 3,
            package_types: vec![],
            dependencies: vec![],
        }
    }

    #[test]
    fn service_index_has_core_resources() {
        let idx = service_index(&urls(), true);
        assert_eq!(idx["version"], "3.0.0");
        let resources = idx["resources"].as_array().unwrap();
        let types: Vec<&str> = resources
            .iter()
            .map(|r| r["@type"].as_str().unwrap())
            .collect();
        assert!(types.contains(&"PackageBaseAddress/3.0.0"));
        assert!(types.contains(&"SearchQueryService"));
        assert!(types.contains(&"PackagePublish/2.0.0"));
        assert!(types.contains(&"RegistrationsBaseUrl/3.6.0"));
        // The package base address points where the client expects.
        let pba = resources
            .iter()
            .find(|r| r["@type"] == "PackageBaseAddress/3.0.0")
            .unwrap();
        assert_eq!(pba["@id"], "https://nuget.example.com/v3/package/");
        // The package-details template is advertised with its placeholders.
        let details = resources
            .iter()
            .find(|r| r["@type"] == "PackageDetailsUriTemplate/5.1.0")
            .unwrap();
        assert_eq!(
            details["@id"],
            "https://nuget.example.com/packages/{id}/{version}"
        );
    }

    #[test]
    fn service_index_omits_details_template_without_web_ui() {
        let idx = service_index(&urls(), false);
        let resources = idx["resources"].as_array().unwrap();
        assert!(resources
            .iter()
            .all(|r| r["@type"] != "PackageDetailsUriTemplate/5.1.0"));
    }

    #[test]
    fn registration_index_inlines_leaves() {
        let pkgs = vec![pkg("Contoso.Utils", "1.0.0"), pkg("Contoso.Utils", "1.1.0")];
        let reg = registration_index(&urls(), "Contoso.Utils", &pkgs);
        assert_eq!(reg["count"], 1);
        let page = &reg["items"][0];
        assert_eq!(page["count"], 2);
        assert_eq!(page["lower"], "1.0.0");
        assert_eq!(page["upper"], "1.1.0");
        let leaf = &page["items"][0];
        assert_eq!(
            leaf["packageContent"],
            "https://nuget.example.com/v3/package/contoso.utils/1.0.0/contoso.utils.1.0.0.nupkg"
        );
        let entry = &leaf["catalogEntry"];
        assert_eq!(entry["id"], "Contoso.Utils");
        assert_eq!(entry["version"], "1.0.0");
        assert_eq!(entry["authors"], "Alice, Bob");
        assert_eq!(entry["licenseExpression"], "MIT");
    }

    #[test]
    fn registration_index_paginates_large_packages() {
        let pkgs: Vec<Package> = (0..130)
            .map(|i| pkg("Big.Pkg", &format!("1.0.{i}")))
            .collect();
        let reg = registration_index(&urls(), "Big.Pkg", &pkgs);
        // 130 versions / 64 per page = 3 external pages.
        assert_eq!(reg["count"], 3);
        let pages = reg["items"].as_array().unwrap();
        assert_eq!(pages.len(), 3);
        // External pages reference a page URL and do NOT inline their leaves.
        assert!(pages[0]["items"].is_null());
        assert_eq!(pages[0]["count"], 64);
        assert_eq!(pages[0]["lower"], "1.0.0");
        assert!(pages[0]["@id"].as_str().unwrap().contains("/page/"));
    }

    #[test]
    fn registration_page_inlines_its_leaves() {
        let pkgs = vec![pkg("Big.Pkg", "1.0.0"), pkg("Big.Pkg", "1.0.1")];
        let page = registration_page(&urls(), "Big.Pkg", &pkgs);
        assert_eq!(page["count"], 2);
        assert_eq!(page["lower"], "1.0.0");
        assert_eq!(page["upper"], "1.0.1");
        assert_eq!(page["items"][0]["catalogEntry"]["version"], "1.0.0");
        assert!(page["@id"].as_str().unwrap().contains("/page/"));
        assert_eq!(
            page["parent"],
            "https://nuget.example.com/v3/registration/big.pkg/index.json"
        );
    }

    #[test]
    fn unlisted_version_reports_1900_and_listed_false() {
        let mut p = pkg("Contoso.Utils", "1.0.0");
        p.listed = false;
        let reg = registration_index(&urls(), "Contoso.Utils", std::slice::from_ref(&p));
        let entry = &reg["items"][0]["items"][0]["catalogEntry"];
        assert_eq!(entry["listed"], false);
        assert_eq!(entry["published"], "1900-01-01T00:00:00.000Z");
    }

    #[test]
    fn dependency_groups_omitted_when_empty() {
        // pkg() has no dependencies.
        let p = pkg("No.Deps", "1.0.0");
        let reg = registration_index(&urls(), "No.Deps", std::slice::from_ref(&p));
        let entry = &reg["items"][0]["items"][0]["catalogEntry"];
        assert!(entry.get("dependencyGroups").is_none());
    }

    #[test]
    fn search_response_groups_versions() {
        let group = SearchGroup {
            packages: vec![pkg("Contoso.Utils", "1.0.0"), pkg("Contoso.Utils", "1.1.0")],
        };
        let page = SearchPage {
            total_hits: 1,
            groups: vec![group],
        };
        let resp = search_response(&urls(), &page);
        assert_eq!(resp["totalHits"], 1);
        let item = &resp["data"][0];
        assert_eq!(item["id"], "Contoso.Utils");
        assert_eq!(item["version"], "1.1.0"); // latest
        assert_eq!(item["totalDownloads"], 6); // 3 + 3
        assert_eq!(item["versions"].as_array().unwrap().len(), 2);
        // Empty package type list defaults to "Dependency".
        assert_eq!(item["packageTypes"][0]["name"], "Dependency");
    }

    #[test]
    fn flat_container_lists_versions() {
        let v = vec!["1.0.0".to_string(), "1.1.0".to_string()];
        let doc = flat_container_index(&v);
        assert_eq!(doc["versions"][0], "1.0.0");
        assert_eq!(doc["versions"][1], "1.1.0");
    }
}
