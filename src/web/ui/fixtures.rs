//! Sample data shared by the page tests.

use super::gallery::GalleryView;
use crate::models::Package;

/// What a browser's `innerText` gives for `html`: the text with the tags
/// dropped and the five escapes undone. It is what the copy button puts
/// on the clipboard.
pub(super) fn text_of(html: &str) -> String {
    let mut text = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

pub(super) fn page_of(ids: &[&str]) -> crate::database::SearchPage {
    let groups = ids
        .iter()
        .map(|id| crate::database::SearchGroup {
            packages: vec![{
                let mut p = sample();
                p.id = (*id).into();
                p
            }],
            total_downloads: 0,
        })
        .collect::<Vec<_>>();
    crate::database::SearchPage {
        total_hits: ids.len() as i64,
        groups,
    }
}

/// A gallery request on a server whose configured page size is 20.
pub(super) fn view(query: &str, skip: i64, take: i64) -> GalleryView<'_> {
    GalleryView {
        query,
        skip,
        take,
        default_take: 20,
        ..Default::default()
    }
}

pub(super) fn feed_ctx(api: Option<&str>, admin: Option<&str>) -> crate::web::FeedContext {
    crate::web::FeedContext {
        name: "default".into(),
        prefix: String::new(),
        auth: crate::auth::ApiKeyAuth::new(api.map(str::to_string)),
        read_auth: crate::auth::ReadAuth::new(None),
        admin: crate::auth::AdminAuth::new(admin.map(str::to_string)),
        allow_overwrite: crate::config::OverwriteMode::Disabled,
        hard_delete_enabled: false,
        requires_approval: false,
        promotes_to: None,
        mirror: None,
        license_policy: crate::config::LicensePolicyConfig::default(),
        retention: crate::config::RetentionConfig::default(),
        reserved_elsewhere: Vec::new(),
        cleanup: Default::default(),
    }
}

pub(super) fn feed_version(
    p: Package,
    pending: bool,
    flagged: bool,
) -> crate::database::FeedVersion {
    crate::database::FeedVersion {
        package: p,
        pending,
        flagged,
        flag_reason: flagged.then(|| "license MIT is blocked".to_string()),
        pinned: false,
    }
}

pub(super) fn sample() -> Package {
    use crate::version::NuGetVersion;
    use chrono::Utc;
    Package {
        id: "Contoso.Utils".into(),
        version: NuGetVersion::parse("1.0.0").unwrap(),
        listed: true,
        enabled: true,
        authors: vec!["Alice".into()],
        description: "Helpers".into(),
        icon_url: None,
        license_url: None,
        license_expression: Some("MIT".into()),
        project_url: None,
        repository_url: None,
        repository_type: None,
        min_client_version: None,
        release_notes: None,
        language: None,
        title: None,
        summary: None,
        tags: vec!["util".into()],
        has_readme: false,
        has_embedded_icon: false,
        is_development_dependency: false,
        require_license_acceptance: false,
        is_semver2: false,
        package_size: 2048,
        package_hash: "h".into(),
        package_hash_algorithm: "SHA512".into(),
        published: Utc::now(),
        downloads: 5,
        package_types: vec![],
        dependencies: vec![],
    }
}
