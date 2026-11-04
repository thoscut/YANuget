//! Multiple feeds end to end: isolation, approval rings, promotion,
//! transfers, license policy, read authentication and preview keys.

mod common;

use yanuget::database::{PackageDatabase, SqliteDatabase};
use yanuget::version::NuGetVersion;

use common::*;

// ---------------------------------------------------------------------------
// Feeds: multi-feed isolation, approval rings, promotion, license policy, read auth
// ---------------------------------------------------------------------------

#[tokio::test]
async fn feeds_are_isolated_and_prefixed() {
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        c.feeds = vec![feed("stable"), feed("dev")];
    })
    .await;

    // Push only into /stable.
    let resp = push_to(
        &server,
        "/stable/api/v2/package",
        API_KEY,
        build_nupkg("Iso.Pkg", "1.0.0", b"x"),
    )
    .await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    // Visible in stable, absent in dev.
    let in_stable = server
        .client
        .get(server.url("/stable/v3/package/iso.pkg/index.json"))
        .send()
        .await
        .unwrap();
    assert!(in_stable.status().is_success());
    let in_dev = server
        .client
        .get(server.url("/dev/v3/package/iso.pkg/index.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(in_dev.status(), reqwest::StatusCode::NOT_FOUND);

    // The service index advertises feed-prefixed resource URLs.
    let index: serde_json::Value = server
        .client
        .get(server.url("/stable/v3/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        package_base_address(&index),
        format!("{}/stable/v3/package/", server.base)
    );

    // Root lists the feeds; each feed's gallery lives at its prefix `/{name}`.
    let root = server.client.get(server.url("/")).send().await.unwrap();
    assert!(root.text().await.unwrap().contains("href=\"/stable\""));
    let gallery = server
        .client
        .get(server.url("/stable"))
        .send()
        .await
        .unwrap();
    assert!(gallery.status().is_success());
}

#[tokio::test]
async fn approval_ring_withholds_until_approved() {
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        c.admin_api_key = Some(ADMIN_KEY.into());
        let mut gated = feed("gated");
        gated.requires_approval = true;
        c.feeds = vec![gated];
    })
    .await;

    assert_eq!(
        push_to(
            &server,
            "/gated/api/v2/package",
            API_KEY,
            build_nupkg("Ring.Pkg", "1.0.0", b"x")
        )
        .await
        .status(),
        reqwest::StatusCode::CREATED
    );

    // Pending: not visible to clients yet.
    let pending = server
        .client
        .get(server.url("/gated/v3/package/ring.pkg/index.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(pending.status(), reqwest::StatusCode::NOT_FOUND);

    // Admin approves it.
    let approve = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .post(server.url("/gated/admin/packages/ring.pkg/1.0.0/approve"))
        .basic_auth("admin", Some(ADMIN_KEY))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(admin_body(ADMIN_KEY))
        .send()
        .await
        .unwrap();
    assert_eq!(approve.status(), reqwest::StatusCode::SEE_OTHER);

    // Now servable.
    let live = server
        .client
        .get(server.url("/gated/v3/package/ring.pkg/index.json"))
        .send()
        .await
        .unwrap();
    assert!(live.status().is_success());
}

#[tokio::test]
async fn promotion_moves_a_version_into_the_next_ring() {
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        c.admin_api_key = Some(ADMIN_KEY.into());
        let mut dev = feed("dev");
        dev.promotes_to = Some("stable".into());
        c.feeds = vec![dev, feed("stable")];
    })
    .await;

    push_to(
        &server,
        "/dev/api/v2/package",
        API_KEY,
        build_nupkg("Prom.Pkg", "1.0.0", b"x"),
    )
    .await;

    // Not in stable yet.
    assert_eq!(
        server
            .client
            .get(server.url("/stable/v3/package/prom.pkg/index.json"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NOT_FOUND
    );

    // Promote dev -> stable.
    let promote = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .post(server.url("/dev/admin/packages/prom.pkg/1.0.0/promote"))
        .basic_auth("admin", Some(ADMIN_KEY))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(admin_body(ADMIN_KEY))
        .send()
        .await
        .unwrap();
    assert_eq!(promote.status(), reqwest::StatusCode::SEE_OTHER);
    let location = promote.headers()["location"].to_str().unwrap();
    assert!(
        location.starts_with("/dev/admin/packages/prom.pkg?done=promote&to=stable"),
        "{location}"
    );

    // Now present in both rings (shared payload, two memberships).
    assert!(server
        .client
        .get(server.url("/stable/v3/package/prom.pkg/index.json"))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    assert!(server
        .client
        .get(server.url("/dev/v3/package/prom.pkg/index.json"))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
}

/// Promoting into a feed that gates arrivals used to look like nothing
/// happened: the button sent the admin back to the same page, and the version
/// sat pending in the target, invisible to clients. The page now says what
/// happened and where to approve it, and marks the version as held there.
#[tokio::test]
async fn promotion_into_a_gated_feed_is_reported_on_the_admin_page() {
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        c.admin_api_key = Some(ADMIN_KEY.into());
        let mut dev = feed("dev");
        dev.promotes_to = Some("stable".into());
        let mut stable = feed("stable");
        stable.requires_approval = true;
        c.feeds = vec![dev, stable];
    })
    .await;
    push_to(
        &server,
        "/dev/api/v2/package",
        API_KEY,
        build_nupkg("Gate.Pkg", "1.0.0", b"x"),
    )
    .await;
    let promote_path = "/dev/admin/packages/gate.pkg/1.0.0/promote";
    let first = admin_action(&server, promote_path).await;
    assert_eq!(first.status(), reqwest::StatusCode::SEE_OTHER);
    let shown = admin_page(&server, first.headers()["location"].to_str().unwrap()).await;
    assert!(
        shown.contains("Promoted Gate.Pkg 1.0.0 to stable. It is pending there"),
        "{shown}"
    );
    assert!(shown.contains("href=\"/stable/admin/packages/gate.pkg\""));
    assert!(shown.contains("pending in stable"));
    assert!(!shown.contains("/promote\""), "nothing left to promote");
    assert_eq!(
        status_of(&server, "/stable/v3/package/gate.pkg/index.json").await,
        reqwest::StatusCode::NOT_FOUND,
        "pending until stable approves it"
    );

    // A second promotion (a stale page, a script) changes nothing and says so.
    let again = admin_action(&server, promote_path).await;
    let shown = admin_page(&server, again.headers()["location"].to_str().unwrap()).await;
    assert!(
        shown.contains("stable already holds Gate.Pkg 1.0.0"),
        "{shown}"
    );

    // Approved in stable, the dev page shows it as live there.
    let approve = admin_action(&server, "/stable/admin/packages/gate.pkg/1.0.0/approve").await;
    assert_eq!(approve.status(), reqwest::StatusCode::SEE_OTHER);
    assert!(status_of(&server, "/stable/v3/package/gate.pkg/index.json")
        .await
        .is_success());
    let dev = admin_page(&server, "/dev/admin/packages/gate.pkg").await;
    assert!(dev.contains(">in stable<"), "{dev}");

    // Whatever the query claims, only a real feed is named, and nothing else
    // from it is echoed.
    let forged = admin_page(
        &server,
        "/dev/admin/packages/gate.pkg?done=promote&to=%3Cb%3E&v=1.0.0&pending=1",
    )
    .await;
    assert!(!forged.contains("Promoted"), "{forged}");
}

/// GET an admin page as the admin.
async fn admin_page(server: &TestServer, path: &str) -> String {
    server
        .client
        .get(server.url(path))
        .basic_auth("admin", Some(ADMIN_KEY))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

/// A version unlisted by hand keeps that mark when it is promoted, copied or
/// moved: in a mirror feed the mark is what stops the upstream's listing from
/// relisting it (COR-38). One whose listing nobody set arrives without it.
#[tokio::test]
async fn a_transfer_keeps_whether_the_listing_was_set_by_hand() {
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        c.admin_api_key = Some(ADMIN_KEY.into());
        let mut dev = feed("dev");
        dev.promotes_to = Some("stable".into());
        c.feeds = vec![dev, feed("stable")];
    })
    .await;
    for v in ["1.0.0", "2.0.0", "3.0.0", "4.0.0"] {
        push_to(
            &server,
            "/dev/api/v2/package",
            API_KEY,
            build_nupkg("Hidden.Pkg", v, b"x"),
        )
        .await;
    }
    for v in ["1.0.0", "2.0.0", "3.0.0"] {
        let resp = server
            .client
            .delete(server.url(&format!("/dev/api/v2/package/hidden.pkg/{v}")))
            .header("X-NuGet-ApiKey", API_KEY)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
    }
    for v in ["1.0.0", "4.0.0"] {
        let promote = admin_action(
            &server,
            &format!("/dev/admin/packages/hidden.pkg/{v}/promote"),
        )
        .await;
        assert_eq!(promote.status(), reqwest::StatusCode::SEE_OTHER);
    }
    for body in [
        "op=copy&target=stable&v=2.0.0",
        "op=move&target=stable&v=3.0.0",
    ] {
        let resp = admin_bulk(&server, "/dev", "hidden.pkg", ADMIN_KEY, body).await;
        assert_eq!(resp.status(), reqwest::StatusCode::SEE_OTHER);
    }

    let db = SqliteDatabase::connect(&server._dir.path().join("yanuget.db").to_string_lossy())
        .await
        .unwrap();
    for (v, listed) in [
        ("1.0.0", false),
        ("2.0.0", false),
        ("3.0.0", false),
        ("4.0.0", true),
    ] {
        let m = db
            .get_membership("stable", "hidden.pkg", &NuGetVersion::parse(v).unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.listed, listed, "{v}");
        assert_eq!(m.listing_set, !listed, "{v}");
    }
}

#[tokio::test]
async fn admin_can_move_and_copy_versions_into_a_feed_it_administers() {
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        c.admin_api_key = Some(ADMIN_KEY.into());
        c.feeds = vec![feed("dev"), feed("stable")];
    })
    .await;
    for v in ["1.0.0", "2.0.0"] {
        push_to(
            &server,
            "/dev/api/v2/package",
            API_KEY,
            build_nupkg("Mv.Pkg", v, b"x"),
        )
        .await;
    }

    // Both feeds share the admin key, so the page offers the other one.
    let page = server
        .client
        .get(server.url("/dev/admin/packages/mv.pkg"))
        .basic_auth("admin", Some(ADMIN_KEY))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        page.contains("<option value=\"stable\">stable</option>"),
        "{page}"
    );
    assert!(!page.contains("<option value=\"dev\">"), "{page}");

    // Move 1.0.0: it leaves dev and arrives in stable.
    let resp = admin_bulk(
        &server,
        "/dev",
        "mv.pkg",
        ADMIN_KEY,
        "op=move&target=stable&v=1.0.0",
    )
    .await;
    assert_eq!(resp.status(), reqwest::StatusCode::SEE_OTHER);
    let flat = |feed: &str| format!("/{feed}/v3/package/mv.pkg/1.0.0/mv.pkg.1.0.0.nupkg");
    assert!(status_of(&server, &flat("stable")).await.is_success());
    assert_eq!(
        status_of(&server, &flat("dev")).await,
        reqwest::StatusCode::NOT_FOUND
    );

    // Copy 2.0.0: it is in both afterwards.
    let resp = admin_bulk(
        &server,
        "/dev",
        "mv.pkg",
        ADMIN_KEY,
        "op=copy&target=stable&v=2.0.0",
    )
    .await;
    assert_eq!(resp.status(), reqwest::StatusCode::SEE_OTHER);
    for feed in ["dev", "stable"] {
        assert!(
            status_of(
                &server,
                &format!("/{feed}/v3/package/mv.pkg/2.0.0/mv.pkg.2.0.0.nupkg")
            )
            .await
            .is_success(),
            "{feed}"
        );
    }

    // Moving the last version out sends the admin back to the package list.
    let resp = admin_bulk(
        &server,
        "/dev",
        "mv.pkg",
        ADMIN_KEY,
        "op=move&target=stable&v=2.0.0",
    )
    .await;
    assert_eq!(resp.status(), reqwest::StatusCode::SEE_OTHER);
    assert_eq!(resp.headers()["location"], "/dev/admin");
    // The files stayed: stable serves both versions.
    let versions: serde_json::Value = server
        .client
        .get(server.url("/stable/v3/package/mv.pkg/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(versions["versions"], serde_json::json!(["1.0.0", "2.0.0"]));
}

#[tokio::test]
async fn moving_into_a_feed_takes_that_feeds_admin_key() {
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        let mut dev = feed("dev");
        dev.admin_api_key = Some("dev-admin".into());
        let mut stable = feed("stable");
        stable.admin_api_key = Some("stable-admin".into());
        c.feeds = vec![dev, stable];
    })
    .await;
    push_to(
        &server,
        "/dev/api/v2/package",
        API_KEY,
        build_nupkg("Guard.Pkg", "1.0.0", b"x"),
    )
    .await;

    // dev's key is no key to stable: the page does not offer it...
    let page = server
        .client
        .get(server.url("/dev/admin/packages/guard.pkg"))
        .basic_auth("admin", Some("dev-admin"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!page.contains("value=\"move\""), "{page}");

    // ...and a hand-made request is refused, leaving both feeds as they were.
    let resp = admin_bulk(
        &server,
        "/dev",
        "guard.pkg",
        "dev-admin",
        "op=move&target=stable&v=1.0.0",
    )
    .await;
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(
        status_of(&server, "/stable/v3/package/guard.pkg/index.json").await,
        reqwest::StatusCode::NOT_FOUND
    );
    assert!(status_of(&server, "/dev/v3/package/guard.pkg/index.json")
        .await
        .is_success());
}

#[tokio::test]
async fn blocking_license_policy_rejects_push() {
    use yanuget::config::PolicyAction;
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        let mut strict = feed("strict");
        strict.license_policy.enabled = true;
        strict.license_policy.allow_unlicensed = false;
        strict.license_policy.action = PolicyAction::Block;
        c.feeds = vec![strict];
    })
    .await;

    // build_nupkg declares no license -> blocked.
    let resp = push_to(
        &server,
        "/strict/api/v2/package",
        API_KEY,
        build_nupkg("Lic.Pkg", "1.0.0", b"x"),
    )
    .await;
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn read_auth_gates_downloads() {
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        let mut private = feed("private");
        private.read_api_key = Some("read-key".into());
        c.feeds = vec![private];
    })
    .await;

    push_to(
        &server,
        "/private/api/v2/package",
        API_KEY,
        build_nupkg("Priv.Pkg", "1.0.0", b"x"),
    )
    .await;

    let url = "/private/v3/package/priv.pkg/index.json";
    // No credential -> 401.
    assert_eq!(
        server
            .client
            .get(server.url(url))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    // With the read key -> 200.
    assert!(server
        .client
        .get(server.url(url))
        .header("X-NuGet-ApiKey", "read-key")
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
}

/// `yanuget admin` drives the same routes as the admin page: it reads the
/// CSRF token off the package's admin page and posts the action with it.
#[tokio::test]
async fn the_admin_client_promotes_and_approves_across_rings() {
    use yanuget::admin_client::{Action, AdminClient, CertPolicy};
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        c.admin_api_key = Some(ADMIN_KEY.into());
        let mut dev = feed("dev");
        dev.promotes_to = Some("stable".into());
        let mut stable = feed("stable");
        stable.requires_approval = true;
        c.feeds = vec![dev, stable];
    })
    .await;
    push_to(
        &server,
        "/dev/api/v2/package",
        API_KEY,
        build_nupkg("Cli.Pkg", "1.0.0", b"x"),
    )
    .await;
    let client = |feed: &str, key: &str| {
        AdminClient::new(&server.url(feed), key, CertPolicy::System).unwrap()
    };

    let promoted = client("/dev", ADMIN_KEY)
        .run(Action::Promote, "Cli.Pkg", "1.0.0")
        .await
        .unwrap();
    assert!(promoted.starts_with("promoted Cli.Pkg 1.0.0"), "{promoted}");
    assert_eq!(
        status_of(&server, "/stable/v3/package/cli.pkg/index.json").await,
        reqwest::StatusCode::NOT_FOUND,
        "pending in stable until approved"
    );
    let approved = client("/stable", ADMIN_KEY)
        .run(Action::Approve, "Cli.Pkg", "1.0.0")
        .await
        .unwrap();
    assert_eq!(approved, "approved Cli.Pkg 1.0.0");
    assert!(status_of(&server, "/stable/v3/package/cli.pkg/index.json")
        .await
        .is_success());

    // The server's own reason comes through.
    let err = client("/stable", ADMIN_KEY)
        .run(Action::Promote, "Cli.Pkg", "1.0.0")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no promotion target"), "{err}");

    let err = client("/dev", "wrong-key")
        .run(Action::Disable, "Cli.Pkg", "1.0.0")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("admin key was refused"), "{err}");

    let err = client("/dev", ADMIN_KEY)
        .run(Action::Disable, "No.Such.Pkg", "1.0.0")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("No.Such.Pkg is not in the feed"), "{err}");

    let err = client("/nowhere", ADMIN_KEY)
        .run(Action::Disable, "Cli.Pkg", "1.0.0")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("has no admin area"), "{err}");

    let disabled = client("/dev", ADMIN_KEY)
        .run(Action::Disable, "Cli.Pkg", "1.0.0")
        .await
        .unwrap();
    assert_eq!(disabled, "disabled Cli.Pkg 1.0.0");
    assert_eq!(
        status_of(&server, "/dev/v3/package/cli.pkg/index.json").await,
        reqwest::StatusCode::NOT_FOUND
    );
}

const PREVIEW_KEY: &str = "preview-key-of-the-intake-feed";

/// `SEC-28`: on a feed with open reads, a wrong preview key is a failed
/// authentication, not a miss, so `max_failed_auth` counts it.
#[tokio::test]
async fn a_wrong_preview_key_is_unauthorized_on_an_open_feed() {
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        let mut intake = feed("intake");
        intake.requires_approval = true;
        intake.preview_api_key = Some(PREVIEW_KEY.into());
        c.feeds = vec![intake];
    })
    .await;
    let pushed = push_to(
        &server,
        "/intake/api/v2/package",
        API_KEY,
        build_nupkg("Guess.Pkg", "1.0.0", b"x"),
    )
    .await;
    assert_eq!(pushed.status(), reqwest::StatusCode::CREATED);
    let nupkg = "/intake/v3/package/guess.pkg/1.0.0/guess.pkg.1.0.0.nupkg";
    let get = |key: Option<&str>| {
        let mut req = server.client.get(server.url(nupkg));
        if let Some(key) = key {
            req = req.header("X-NuGet-ApiKey", key);
        }
        req.send()
    };
    assert_eq!(
        get(Some("a-guess")).await.unwrap().status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get(None).await.unwrap().status(),
        reqwest::StatusCode::NOT_FOUND
    );
    assert!(get(Some(PREVIEW_KEY)).await.unwrap().status().is_success());
}

/// `SEC-37`: only a key offered where the preview key goes, and none of the
/// feed's, is a guess. A client that sends the push key with every request,
/// a gateway's `Bearer` token or the admin's credentials gets the `404` of
/// a miss, which `max_failed_auth` does not count.
#[tokio::test]
async fn only_a_wrong_key_where_the_preview_key_goes_is_unauthorized() {
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        c.admin_api_key = Some(ADMIN_KEY.into());
        let mut intake = feed("intake");
        intake.requires_approval = true;
        intake.preview_api_key = Some(PREVIEW_KEY.into());
        c.feeds = vec![intake];
    })
    .await;
    let pushed = push_to(
        &server,
        "/intake/api/v2/package",
        API_KEY,
        build_nupkg("Guess.Pkg", "1.0.0", b"x"),
    )
    .await;
    assert_eq!(pushed.status(), reqwest::StatusCode::CREATED);
    let nupkg = server.url("/intake/v3/package/guess.pkg/1.0.0/guess.pkg.1.0.0.nupkg");
    let status = |req: reqwest::RequestBuilder| async move { req.send().await.unwrap().status() };
    let get = || server.client.get(&nupkg);
    use reqwest::StatusCode;
    assert_eq!(
        status(get().header("X-NuGet-ApiKey", API_KEY)).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status(get().bearer_auth("a-gateway-token")).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status(get().basic_auth("admin", Some(ADMIN_KEY))).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status(get().basic_auth("user", Some(""))).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status(get().basic_auth("user", Some("a-guess"))).await,
        StatusCode::UNAUTHORIZED
    );
    // The push key does not cover a guess sent along with it.
    assert_eq!(
        status(
            get()
                .header("X-NuGet-ApiKey", API_KEY)
                .basic_auth("user", Some("a-guess"))
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
    assert!(status(get().basic_auth("user", Some(PREVIEW_KEY)))
        .await
        .is_success());
}

/// `INT-32`: the preview key downloads a pending version and its files, and
/// nothing else.
#[tokio::test]
async fn the_preview_key_downloads_pending_versions_and_nothing_else() {
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        c.admin_api_key = Some(ADMIN_KEY.into());
        let mut intake = feed("intake");
        intake.requires_approval = true;
        intake.read_api_key = Some("reader-key".into());
        intake.preview_api_key = Some(PREVIEW_KEY.into());
        c.feeds = vec![intake];
    })
    .await;
    let pushed = push_to(
        &server,
        "/intake/api/v2/package",
        API_KEY,
        build_nupkg("Preview.Pkg", "1.0.0", b"x"),
    )
    .await;
    assert_eq!(pushed.status(), reqwest::StatusCode::CREATED);
    let image = image_bytes(9);
    assert_eq!(
        put_feed_file(
            &server,
            "/intake",
            "Preview.Pkg",
            "1.0.0",
            "base.wim",
            image.clone()
        )
        .await
        .status(),
        reqwest::StatusCode::CREATED
    );

    let get = |path: &str, key: &str| {
        server
            .client
            .get(server.url(path))
            .header("X-NuGet-ApiKey", key)
            .send()
    };
    let nupkg = "/intake/v3/package/preview.pkg/1.0.0/preview.pkg.1.0.0.nupkg";
    let nuspec = "/intake/v3/package/preview.pkg/1.0.0/preview.pkg.nuspec";
    let file = "/intake/files/preview.pkg/1.0.0/base.wim";
    let index = "/intake/files/preview.pkg/1.0.0/index.json";

    // Readers do not see it while it is pending.
    for path in [nupkg, nuspec, file, index] {
        assert_eq!(
            get(path, "reader-key").await.unwrap().status(),
            reqwest::StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    // The preview key does, kept out of shared caches.
    for path in [nupkg, nuspec, file, index] {
        let r = get(path, PREVIEW_KEY).await.unwrap();
        assert!(r.status().is_success(), "{path}: {}", r.status());
        let cache = r.headers()["cache-control"].to_str().unwrap().to_string();
        assert!(cache.starts_with("private"), "{path}: {cache}");
        assert!(!cache.contains("immutable"), "{path}: {cache}");
        let vary: Vec<_> = r
            .headers()
            .get_all("vary")
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect();
        assert!(
            vary.iter().any(|v| v.contains("X-NuGet-ApiKey")),
            "{path}: {vary:?}"
        );
    }
    let bytes = get(file, PREVIEW_KEY).await.unwrap().bytes().await.unwrap();
    assert_eq!(bytes.as_ref(), image.as_slice());
    // Basic credentials work as for the read key.
    let basic = server
        .client
        .get(server.url(nupkg))
        .basic_auth("anyone", Some(PREVIEW_KEY))
        .send()
        .await
        .unwrap();
    assert!(basic.status().is_success());

    // And nothing else: no listing, search, push or admin.
    for path in [
        "/intake/v3/package/preview.pkg/index.json",
        "/intake/v3/search?q=preview",
        "/intake/v3/registration/preview.pkg/index.json",
    ] {
        assert_eq!(
            get(path, PREVIEW_KEY).await.unwrap().status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "{path}"
        );
    }
    let push = push_to(
        &server,
        "/intake/api/v2/package",
        PREVIEW_KEY,
        build_nupkg("Preview.Other", "1.0.0", b"y"),
    )
    .await;
    assert_eq!(push.status(), reqwest::StatusCode::UNAUTHORIZED);
    let approve = no_redirect()
        .post(server.url("/intake/admin/packages/preview.pkg/1.0.0/approve"))
        .basic_auth("admin", Some(PREVIEW_KEY))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(admin_body(PREVIEW_KEY))
        .send()
        .await
        .unwrap();
    assert!(approve.status().is_client_error(), "{}", approve.status());
    assert_eq!(
        get(nupkg, "reader-key").await.unwrap().status(),
        reqwest::StatusCode::NOT_FOUND,
        "still pending"
    );

    // Once approved, the preview key still gets a preview: private, never
    // immutable, the same as for a pending version.
    let approve = admin_action(&server, "/intake/admin/packages/preview.pkg/1.0.0/approve").await;
    assert_eq!(approve.status(), reqwest::StatusCode::SEE_OTHER);
    let read = get(nupkg, "reader-key").await.unwrap();
    assert!(read.status().is_success());
    for path in [nupkg, file] {
        let r = get(path, PREVIEW_KEY).await.unwrap();
        assert!(r.status().is_success(), "{path}: {}", r.status());
        let cache = r.headers()["cache-control"].to_str().unwrap().to_string();
        assert!(cache.starts_with("private"), "{path}: {cache}");
        assert!(!cache.contains("immutable"), "{path}: {cache}");
    }

    // A disabled version stays withheld from the preview key too.
    let disable = admin_action(&server, "/intake/admin/packages/preview.pkg/1.0.0/disable").await;
    assert_eq!(disable.status(), reqwest::StatusCode::SEE_OTHER);
    assert_eq!(
        get(nupkg, PREVIEW_KEY).await.unwrap().status(),
        reqwest::StatusCode::NOT_FOUND
    );
}
