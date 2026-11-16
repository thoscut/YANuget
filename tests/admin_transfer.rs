//! Copying and moving versions into another feed from the admin page: the
//! target feed's license policy holds for them as it does for a push or a
//! promotion.

mod common;

use yanuget::config::{LicensePolicyConfig, PolicyAction};

use common::*;

/// `dev` takes anything, `stable` blocks every license but MIT, and both
/// share the admin key, so the admin page of `dev` offers `stable` as a
/// target. `dev` holds `Mixed.Pkg` 1.0.0 (MIT) and 2.0.0 (GPL).
async fn dev_and_a_strict_stable() -> TestServer {
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        c.admin_api_key = Some(ADMIN_KEY.into());
        let mut stable = feed("stable");
        stable.license_policy = LicensePolicyConfig {
            enabled: true,
            allowed: vec!["MIT".into()],
            action: PolicyAction::Block,
            ..LicensePolicyConfig::default()
        };
        c.feeds = vec![feed("dev"), stable];
    })
    .await;
    for (version, license) in [("1.0.0", "MIT"), ("2.0.0", "GPL-3.0-only")] {
        let nupkg = build_nupkg_with_license("Mixed.Pkg", version, license);
        let pushed = push_to(&server, "/dev/api/v2/package", API_KEY, nupkg).await;
        assert_eq!(pushed.status(), reqwest::StatusCode::CREATED, "{version}");
    }
    server
}

fn flat(feed: &str, version: &str) -> String {
    format!("/{feed}/v3/package/mixed.pkg/{version}/mixed.pkg.{version}.nupkg")
}

/// `TEST-25`: a feed without a policy is no way around the target's
/// `action = "block"`. A copy or a move of several versions that the target
/// refuses one of is refused as a whole and up front: the MIT version,
/// listed first, does not arrive either, and a move leaves both in `dev`.
#[tokio::test]
async fn a_copy_or_move_is_held_to_the_target_feeds_license_policy() {
    let server = dev_and_a_strict_stable().await;

    for op in ["copy", "move"] {
        let resp = admin_bulk(
            &server,
            "/dev",
            "mixed.pkg",
            ADMIN_KEY,
            &format!("op={op}&target=stable&v=1.0.0&v=2.0.0"),
        )
        .await;
        assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN, "{op}");
        let said = resp.text().await.unwrap();
        assert!(said.contains("stable rejects"), "{op}: {said}");

        for version in ["1.0.0", "2.0.0"] {
            assert_eq!(
                status_of(&server, &flat("stable", version)).await,
                reqwest::StatusCode::NOT_FOUND,
                "{op} {version} arrived in stable"
            );
            assert!(
                status_of(&server, &flat("dev", version)).await.is_success(),
                "{op} {version} left dev"
            );
        }
    }

    // What the policy allows still goes through the same form.
    let resp = admin_bulk(
        &server,
        "/dev",
        "mixed.pkg",
        ADMIN_KEY,
        "op=move&target=stable&v=1.0.0",
    )
    .await;
    assert_eq!(resp.status(), reqwest::StatusCode::SEE_OTHER);
    assert!(status_of(&server, &flat("stable", "1.0.0"))
        .await
        .is_success());
    assert_eq!(
        status_of(&server, &flat("dev", "1.0.0")).await,
        reqwest::StatusCode::NOT_FOUND
    );
}
