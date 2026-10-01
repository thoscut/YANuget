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
    // Two registration hives, as nuget.org exposes them. The SemVer1 hive omits
    // versions a pre-SemVer2 client cannot parse (dotted pre-release labels,
    // build metadata); advertising one hive under both sets of `@type`s would
    // hand such a client versions it chokes on.
    push(
        urls.registration_base_semver1(),
        &[
            "RegistrationsBaseUrl",
            "RegistrationsBaseUrl/3.0.0-beta",
            "RegistrationsBaseUrl/3.0.0-rc",
            "RegistrationsBaseUrl/3.4.0",
        ],
        "Base URL of package registration info (SemVer1).",
    );
    push(
        urls.registration_base_semver2(),
        &[
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
///
/// This is not the item a registration page inlines: the spec gives the
/// standalone leaf its own shape, with `catalogEntry` as a URL rather than an
/// object and `listed`/`published` at the top level. YANuget has no catalog, so
/// `catalogEntry` names the leaf itself — the one resource describing this
/// version — as the page items' `catalogEntry.@id` already does.
pub fn registration_leaf(urls: &UrlBuilder, id: &str, package: &Package) -> Value {
    let lower_id = id.to_lowercase();
    let version = package.normalized_version();
    let leaf_url = urls.registration_leaf(&lower_id, &version);
    json!({
        "@id": leaf_url,
        "@type": ["Package", "http://schema.nuget.org/catalog#Permalink"],
        "catalogEntry": leaf_url,
        "listed": package.listed,
        "packageContent": urls.package_download(&lower_id, &version),
        "published": published(package),
        "registration": urls.registration_index(&lower_id),
    })
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

/// NuGet signals an unlisted version by reporting a `published` date in the
/// year 1900, in addition to the explicit `listed: false` flag.
fn published(p: &Package) -> String {
    if p.listed {
        p.published.to_rfc3339_opts(SecondsFormat::Millis, true)
    } else {
        "1900-01-01T00:00:00.000Z".to_string()
    }
}

/// The icon a client should show: the embedded one, served by this feed, in
/// preference to an external `<iconUrl>` (as nuget.org does). A package whose
/// only icon is embedded used to get no `iconUrl` at all.
fn icon_url(urls: &UrlBuilder, lower_id: &str, p: &Package) -> Option<String> {
    p.has_embedded_icon
        .then(|| urls.package_icon(lower_id, &p.normalized_version()))
        .flatten()
        .or_else(|| p.icon_url.clone())
}

/// The license URL a client should link: the nuspec's own, or for a license
/// expression without one, the expression's page on licenses.nuget.org —
/// which is the URL `dotnet pack` itself writes into such a nuspec, encoded the
/// same way (`WebUtility.UrlEncode`). Clients that predate license expressions
/// only know `licenseUrl`, and showed nothing.
fn license_url(p: &Package) -> Option<String> {
    p.license_url.clone().or_else(|| {
        p.license_expression
            .as_deref()
            .map(|expression| format!("https://licenses.nuget.org/{}", url_encode(expression)))
    })
}

/// .NET's `WebUtility.UrlEncode`: alphanumerics and `-_.!*()` stay, a space
/// becomes `+`, everything else is `%XX`.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z'
            | b'A'..=b'Z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'*'
            | b'('
            | b')' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn catalog_entry(urls: &UrlBuilder, lower_id: &str, p: &Package, content_url: &str) -> Value {
    let version = p.normalized_version();
    let mut entry = json!({
        "@id": urls.registration_leaf(lower_id, &version),
        "@type": "PackageDetails",
        "id": p.id,
        // For display: the publisher's casing and the build metadata, as
        // nuget.org reports it. URLs keep the normalized, lower-cased form.
        "version": p.version.to_full_string(),
        "authors": p.authors.join(", "),
        "description": p.description,
        "iconUrl": icon_url(urls, lower_id, p),
        "language": p.language,
        "licenseExpression": p.license_expression,
        "licenseUrl": license_url(p),
        "listed": p.listed,
        "minClientVersion": p.min_client_version,
        "packageContent": content_url,
        "projectUrl": p.project_url,
        "published": published(p),
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
    // The fragments are percent-encoded like any other URL segment: a target
    // framework such as `portable-net45+win8` or `.NETFramework4.7.2` carries
    // characters that do not belong raw in a URL.
    let items: Vec<Value> = groups
        .iter()
        .map(|g| {
            let tfm = g.target_framework.clone();
            let group_id = match &tfm {
                Some(f) => format!("{base}/{}", urls::enc(&f.to_lowercase())),
                None => base.clone(),
            };
            let deps: Vec<Value> = g
                .dependencies
                .iter()
                .map(|d| {
                    json!({
                        "@id": format!("{group_id}/{}", urls::enc(&d.id.to_lowercase())),
                        "@type": "PackageDependency",
                        "id": d.id,
                        // `<dependency id="X" />` with no `version` attribute is
                        // legal and means "any version". Emitting it as JSON
                        // `null` is not: nuget.org writes the unbounded range as
                        // `(, )`, and NuGet.Protocol hands the value straight to
                        // `VersionRange.Parse`, which throws on null rather than
                        // degrading to `VersionRange.All` — so reading the
                        // metadata fails instead of the dependency being open.
                        "range": d.version_range.clone().unwrap_or_else(|| "(, )".into()),
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
            json!({
                "@id": urls.registration_leaf(&lower_id, &p.normalized_version()),
                "version": p.version.to_full_string(),
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
        "version": latest.version.to_full_string(),
        "description": latest.description,
        "summary": latest.summary,
        "title": latest.title,
        "iconUrl": icon_url(urls, &lower_id, latest),
        "licenseUrl": license_url(latest),
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
            total_downloads: 0,
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

    /// `totalDownloads` is the package's total, including versions the
    /// search's filters left out (pre-releases, say).
    #[test]
    fn search_total_downloads_covers_every_version() {
        let page = SearchPage {
            total_hits: 1,
            groups: vec![SearchGroup {
                packages: vec![pkg("Contoso.Utils", "1.0.0")],
                total_downloads: 1_000,
            }],
        };
        let resp = search_response(&urls(), &page);
        assert_eq!(resp["data"][0]["totalDownloads"], 1_000);
        assert_eq!(resp["data"][0]["versions"][0]["downloads"], 3);
    }

    /// Display fields carry the version as published — casing and build
    /// metadata — while every URL keeps the normalized, lower-cased identity.
    #[test]
    fn versions_are_displayed_as_published() {
        let p = pkg("Contoso.Utils", "1.0.0-Beta.1+Build.7");
        let reg = registration_index(&urls(), "Contoso.Utils", std::slice::from_ref(&p));
        let leaf = &reg["items"][0]["items"][0];
        assert_eq!(leaf["catalogEntry"]["version"], "1.0.0-Beta.1+Build.7");
        assert!(leaf["@id"]
            .as_str()
            .unwrap()
            .ends_with("/1.0.0-beta.1.json"));
        assert_eq!(reg["items"][0]["lower"], "1.0.0-beta.1");

        let page = SearchPage {
            total_hits: 1,
            groups: vec![SearchGroup {
                packages: vec![p],
                total_downloads: 0,
            }],
        };
        let item = &search_response(&urls(), &page)["data"][0];
        assert_eq!(item["version"], "1.0.0-Beta.1+Build.7");
        assert_eq!(item["versions"][0]["version"], "1.0.0-Beta.1+Build.7");
        assert!(item["versions"][0]["@id"]
            .as_str()
            .unwrap()
            .ends_with("/1.0.0-beta.1.json"));
    }

    /// The standalone leaf has the spec's leaf shape, not a page item's.
    #[test]
    fn a_standalone_leaf_has_the_leaf_shape() {
        let mut p = pkg("Contoso.Utils", "1.0.0");
        let leaf = registration_leaf(&urls(), "Contoso.Utils", &p);
        let url = "https://nuget.example.com/v3/registration/contoso.utils/1.0.0.json";
        assert_eq!(leaf["@id"], url);
        assert_eq!(leaf["catalogEntry"], url, "catalogEntry is a URL here");
        assert_eq!(leaf["listed"], true);
        assert!(leaf["published"].as_str().unwrap().ends_with('Z'));
        assert_eq!(
            leaf["registration"],
            "https://nuget.example.com/v3/registration/contoso.utils/index.json"
        );
        assert!(leaf["packageContent"]
            .as_str()
            .unwrap()
            .ends_with("/contoso.utils.1.0.0.nupkg"));
        assert!(leaf["@type"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("Package")));

        p.listed = false;
        let leaf = registration_leaf(&urls(), "Contoso.Utils", &p);
        assert_eq!(leaf["listed"], false);
        assert_eq!(leaf["published"], "1900-01-01T00:00:00.000Z");
    }

    #[test]
    fn embedded_icons_get_an_icon_url() {
        let mut p = pkg("Contoso.Utils", "1.0.0-RC");
        p.has_embedded_icon = true;
        p.icon_url = Some("https://example.com/old.png".into());
        let entry = |urls: &UrlBuilder, p: &Package| {
            registration_index(urls, "Contoso.Utils", std::slice::from_ref(p))["items"][0]["items"]
                [0]["catalogEntry"]["iconUrl"]
                .clone()
        };
        // Served by this feed, the embedded icon wins.
        let served = urls().with_icons(true);
        assert_eq!(
            entry(&served, &p),
            "https://nuget.example.com/packages/contoso.utils/1.0.0-rc/icon"
        );
        // Without the gallery (which serves icons), fall back to <iconUrl>.
        assert_eq!(entry(&urls(), &p), "https://example.com/old.png");
        p.icon_url = None;
        assert!(entry(&urls(), &p).is_null());
    }

    #[test]
    fn license_expressions_get_a_license_url() {
        let mut p = pkg("Contoso.Utils", "1.0.0");
        p.license_expression = Some("MIT OR (Apache-2.0 WITH LLVM-exception)".into());
        let reg = registration_index(&urls(), "Contoso.Utils", std::slice::from_ref(&p));
        assert_eq!(
            reg["items"][0]["items"][0]["catalogEntry"]["licenseUrl"],
            "https://licenses.nuget.org/MIT+OR+(Apache-2.0+WITH+LLVM-exception)"
        );
        // A nuspec's own licenseUrl is kept as written.
        p.license_url = Some("https://example.com/LICENSE".into());
        let page = SearchPage {
            total_hits: 1,
            groups: vec![SearchGroup {
                packages: vec![p],
                total_downloads: 0,
            }],
        };
        assert_eq!(
            search_response(&urls(), &page)["data"][0]["licenseUrl"],
            "https://example.com/LICENSE"
        );
    }

    #[test]
    fn dependency_group_ids_are_percent_encoded() {
        let mut p = pkg("Contoso.Utils", "1.0.0");
        p.dependencies = vec![DependencyGroup {
            target_framework: Some("portable-net45+win8".into()),
            dependencies: vec![crate::models::Dependency {
                id: "Some.Dep".into(),
                version_range: None,
                include: None,
                exclude: None,
            }],
        }];
        let reg = registration_index(&urls(), "Contoso.Utils", std::slice::from_ref(&p));
        let group = &reg["items"][0]["items"][0]["catalogEntry"]["dependencyGroups"][0];
        let id = group["@id"].as_str().unwrap();
        assert!(
            id.ends_with("#dependencygroup/portable-net45%2Bwin8"),
            "{id}"
        );
        assert!(group["dependencies"][0]["@id"]
            .as_str()
            .unwrap()
            .ends_with("/portable-net45%2Bwin8/some.dep"));
        // The framework itself is reported as written.
        assert_eq!(group["targetFramework"], "portable-net45+win8");
    }

    #[test]
    fn flat_container_lists_versions() {
        let v = vec!["1.0.0".to_string(), "1.1.0".to_string()];
        let doc = flat_container_index(&v);
        assert_eq!(doc["versions"][0], "1.0.0");
        assert_eq!(doc["versions"][1], "1.1.0");
    }
}
