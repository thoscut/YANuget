//! The web gallery and the admin area end to end.

mod common;

use common::*;

// ---------------------------------------------------------------------------
// Web gallery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gallery_lists_and_details_packages() {
    let server = spawn().await;
    push_multipart(&server, API_KEY, build_nupkg("Web.Ui.Pkg", "1.2.3", b"x")).await;

    // Home page lists the package.
    let home = server.client.get(server.url("/")).send().await.unwrap();
    assert!(home.status().is_success());
    assert!(home
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .contains("text/html"));
    let html = home.text().await.unwrap();
    assert!(html.contains("Web.Ui.Pkg"));
    assert!(html.contains("/packages/web.ui.pkg"));

    // Detail page shows the Chocolatey install command (default primary client).
    let detail = server
        .client
        .get(server.url("/packages/web.ui.pkg"))
        .send()
        .await
        .unwrap();
    assert!(detail.status().is_success());
    let body = detail.text().await.unwrap();
    // Each flag sits on one line with its value (`.nw`); the text is unchanged.
    assert!(
        body.contains("choco install Web.Ui.Pkg <span class=\"nw\">--version 1.2.3</span>"),
        "{body}"
    );
    assert!(body.contains("Newtonsoft.Json")); // dependency rendered
                                               // Accessibility + UX affordances.
    assert!(body.contains("Skip to content"));
    assert!(body.contains("role=\"search\""));
    assert!(body.contains("class=\"copy\"")); // copy-to-clipboard button
    assert!(body.contains("aria-label=\"Breadcrumb\""));

    // Unknown package detail is a 404.
    let missing = server
        .client
        .get(server.url("/packages/nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn stats_page_aggregates_the_feed() {
    let server = spawn().await;
    push_multipart(&server, API_KEY, build_nupkg("Stat.A", "1.0.0", b"xx")).await;
    push_multipart(&server, API_KEY, build_nupkg("Stat.A", "1.1.0", b"xx")).await;
    push_multipart(&server, API_KEY, build_nupkg("Stat.B", "2.0.0", b"xx")).await;

    let resp = server
        .client
        .get(server.url("/stats"))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let body = resp.text().await.unwrap();
    assert!(body.contains("Statistics"));
    // 2 distinct packages, 3 versions, each value under its caption.
    assert!(body.contains("<div class=\"l\">Packages</div><div class=\"n\">2</div>"));
    assert!(body.contains("<div class=\"l\">Versions</div><div class=\"n\">3</div>"));
    // Both lists reference the published packages.
    assert!(body.contains("Most downloaded"));
    assert!(body.contains("Recently published"));
    assert!(body.contains("Stat.A"));
}

// ---------------------------------------------------------------------------
// Admin area
// ---------------------------------------------------------------------------

#[tokio::test]
async fn admin_selection_disables_then_deletes_a_whole_package() {
    let server = spawn_admin().await;
    for v in ["1.0.0", "2.0.0"] {
        push_multipart(&server, API_KEY, build_nupkg("Bulk.Pkg", v, b"data")).await;
    }
    let index = "/v3/package/bulk.pkg/index.json";

    // Nothing ticked: back to the page, nothing changed.
    let resp = admin_bulk(&server, "", "bulk.pkg", ADMIN_KEY, "op=disable").await;
    assert_eq!(resp.status(), reqwest::StatusCode::SEE_OTHER);
    assert_eq!(resp.headers()["location"], "/admin/packages/bulk.pkg");
    assert!(status_of(&server, index).await.is_success());

    // Both versions disabled in one go: the package is withheld.
    let resp = admin_bulk(
        &server,
        "",
        "bulk.pkg",
        ADMIN_KEY,
        "op=disable&v=1.0.0&v=2.0.0",
    )
    .await;
    assert_eq!(resp.status(), reqwest::StatusCode::SEE_OTHER);
    assert_eq!(
        status_of(&server, index).await,
        reqwest::StatusCode::NOT_FOUND
    );

    // A version the feed does not hold refuses the whole request.
    let resp = admin_bulk(
        &server,
        "",
        "bulk.pkg",
        ADMIN_KEY,
        "op=delete&v=1.0.0&v=9.9.9",
    )
    .await;
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);

    // Delete both: the package is gone, and the admin lands on the list.
    let resp = admin_bulk(
        &server,
        "",
        "bulk.pkg",
        ADMIN_KEY,
        "op=delete&v=1.0.0&v=2.0.0",
    )
    .await;
    assert_eq!(resp.status(), reqwest::StatusCode::SEE_OTHER);
    assert_eq!(resp.headers()["location"], "/admin");
    let gone = server
        .client
        .get(server.url("/admin/packages/bulk.pkg"))
        .basic_auth("admin", Some(ADMIN_KEY))
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status(), reqwest::StatusCode::NOT_FOUND);

    // The selection form is as guarded as the rest: no CSRF token, no change.
    let resp = no_redirect()
        .post(server.url("/admin/packages/bulk.pkg"))
        .basic_auth("admin", Some(ADMIN_KEY))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("op=delete&v=1.0.0")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_gallery_sorts_by_downloads_name_and_last_update() {
    let server = spawn().await;
    // Pushed oldest first; only Zulu gets a download.
    for id in ["Zulu.Pkg", "Mike.Pkg", "Alpha.Pkg"] {
        push_multipart(&server, API_KEY, build_nupkg(id, "1.0.0", b"x")).await;
    }
    let dl = server
        .client
        .get(server.url("/v3/package/zulu.pkg/1.0.0/zulu.pkg.1.0.0.nupkg"))
        .send()
        .await
        .unwrap();
    assert!(dl.status().is_success());
    await_downloads(&server, "zulu.pkg", 1).await;

    let order = |html: &str| {
        let mut ids: Vec<(usize, &str)> = ["Alpha.Pkg", "Mike.Pkg", "Zulu.Pkg"]
            .into_iter()
            .map(|id| (html.find(&format!(">{id}</a>")).expect(id), id))
            .collect();
        ids.sort();
        ids.into_iter().map(|(_, id)| id).collect::<Vec<_>>()
    };
    let server = &server;
    let page = |query: &'static str| async move {
        server
            .client
            .get(server.url(&format!("/packages{query}")))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    };
    // Downloads first, ties by name.
    assert_eq!(
        order(&page("").await),
        ["Zulu.Pkg", "Alpha.Pkg", "Mike.Pkg"]
    );
    assert_eq!(
        order(&page("?sort=name").await),
        ["Alpha.Pkg", "Mike.Pkg", "Zulu.Pkg"]
    );
    assert_eq!(
        order(&page("?sort=updated").await),
        ["Alpha.Pkg", "Mike.Pkg", "Zulu.Pkg"]
    );
    // An order the gallery does not know is the default one, not an error.
    assert_eq!(
        order(&page("?sort=sideways").await),
        ["Zulu.Pkg", "Alpha.Pkg", "Mike.Pkg"]
    );
}

#[tokio::test]
async fn the_gallery_filters_by_tag_and_lists_every_tag() {
    let server = spawn().await;
    // Every test package is tagged `integration test`.
    for id in ["Tag.One", "Tag.Two"] {
        push_multipart(&server, API_KEY, build_nupkg(id, "1.0.0", b"x")).await;
    }
    let page = |q: &'static str| {
        let url = server.url(q);
        let client = server.client.clone();
        async move { client.get(url).send().await.unwrap().text().await.unwrap() }
    };
    // Case does not matter; the heading names the tag.
    let tagged = page("/packages?tag=INTEGRATION").await;
    assert!(
        tagged.contains("2 packages tagged \u{201c}integration\u{201d}"),
        "{tagged}"
    );
    let none = page("/packages?tag=nothing-has-this").await;
    assert!(none.contains("No packages tagged"), "{none}");
    // A value no tag could be (it has a space) is no filter at all.
    let all = page("/packages?tag=two%20words").await;
    assert!(all.contains("2 packages<"), "{all}");

    let cloud = page("/tags").await;
    assert!(cloud.contains(">integration</a>"), "{cloud}");
    assert!(cloud.contains("<span class=\"n\">2<span class=\"vh\"> packages</span></span>"));
    // The landing page offers them too.
    let landing = page("/").await;
    assert!(landing.contains("aria-label=\"Popular tags\""), "{landing}");
}

#[tokio::test]
async fn the_gallery_font_is_served_once_for_every_feed_and_cached() {
    let server = spawn_feeds(|c| c.feeds = vec![feed("stable"), feed("dev")]).await;
    let html = server
        .client
        .get(server.url("/stable"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let url = html
        .split("<link rel=\"preload\" href=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("the page preloads its font")
        .to_string();
    assert!(url.starts_with("/_assets/"), "{url}");
    let resp = server.client.get(server.url(&url)).send().await.unwrap();
    assert!(resp.status().is_success());
    assert_eq!(resp.headers()["content-type"], "font/woff2");
    assert!(resp.headers()["cache-control"]
        .to_str()
        .unwrap()
        .contains("immutable"));
    assert!(resp.bytes().await.unwrap().starts_with(b"wOF2"));
}

/// The retention page, fetched as the admin.
async fn retention_page(server: &TestServer) -> String {
    server
        .client
        .get(server.url("/admin/retention"))
        .basic_auth("admin", Some(ADMIN_KEY))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

/// The plan fingerprint the retention page's delete button would send.
fn plan_of(page: &str) -> String {
    page.split("name=\"plan\" value=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .unwrap_or_else(|| panic!("no delete button: {page}"))
        .to_string()
}

async fn run_retention(server: &TestServer, fields: &str) -> reqwest::Response {
    no_redirect()
        .post(server.url("/admin/retention/run"))
        .basic_auth("admin", Some(ADMIN_KEY))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("{}&{fields}", admin_body(ADMIN_KEY)))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_cleanup_deletes_what_it_showed_and_keeps_pins() {
    let server = spawn_with(|c| {
        c.admin_api_key = Some(ADMIN_KEY.to_string());
        // Manual cleanups only: no schedule, nothing on push.
        c.retention.enabled = true;
        c.retention.interval_hours = 0;
        c.retention.keep_latest_stable = Some(1);
    })
    .await;
    for v in ["1.0.0", "2.0.0", "3.0.0"] {
        push_multipart(&server, API_KEY, build_nupkg("Clean.Pkg", v, b"x")).await;
    }
    // Pin the oldest: an LTS line that would otherwise go.
    let pin = admin_bulk(&server, "", "clean.pkg", ADMIN_KEY, "op=pin&v=1.0.0").await;
    assert_eq!(pin.status(), reqwest::StatusCode::SEE_OTHER);

    let page = retention_page(&server).await;
    assert!(page.contains("1 version would be deleted"), "{page}");
    assert!(
        page.contains("beyond the newest 1 stable version"),
        "{page}"
    );
    let plan = plan_of(&page);

    // A stale plan deletes nothing, and says so.
    let stale = run_retention(&server, "plan=not-what-was-shown").await;
    assert_eq!(stale.status(), reqwest::StatusCode::SEE_OTHER);
    assert_eq!(stale.headers()["location"], "/admin/retention?changed=1");
    let flat = |v: &str| format!("/v3/package/clean.pkg/{v}/clean.pkg.{v}.nupkg");
    assert!(status_of(&server, &flat("2.0.0")).await.is_success());

    // Without the CSRF token, nothing either.
    let forged = no_redirect()
        .post(server.url("/admin/retention/run"))
        .basic_auth("admin", Some(ADMIN_KEY))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("plan={plan}"))
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status(), reqwest::StatusCode::BAD_REQUEST);

    // The plan as shown: 2.0.0 goes; the pinned 1.0.0 and the newest stay.
    let run = run_retention(&server, &format!("plan={plan}")).await;
    assert_eq!(run.status(), reqwest::StatusCode::SEE_OTHER);
    let location = run.headers()["location"].to_str().unwrap().to_string();
    assert!(
        location.starts_with("/admin/retention?deleted=1&"),
        "{location}"
    );
    assert_eq!(
        status_of(&server, &flat("2.0.0")).await,
        reqwest::StatusCode::NOT_FOUND
    );
    for kept in ["1.0.0", "3.0.0"] {
        assert!(status_of(&server, &flat(kept)).await.is_success(), "{kept}");
    }
    let after = retention_page(&server).await;
    assert!(after.contains("Nothing to delete"), "{after}");
    assert!(after.contains("Last cleanup"), "{after}");
    assert!(after.contains("Pinned, kept regardless"), "{after}");
}

#[tokio::test]
async fn pruning_on_push_keeps_pinned_versions_and_an_overwrite_keeps_the_pin() {
    let server = spawn_with(|c| {
        c.admin_api_key = Some(ADMIN_KEY.to_string());
        c.allow_overwrite = yanuget::config::OverwriteMode::Enabled;
        c.retention.enabled = true;
        c.retention.interval_hours = 0;
        c.retention.prune_on_push = true;
        c.retention.keep_latest_stable = Some(1);
    })
    .await;
    push_multipart(&server, API_KEY, build_nupkg("Lts.Pkg", "1.0.0", b"x")).await;
    let pin = no_redirect()
        .post(server.url("/admin/packages/lts.pkg/1.0.0/pin"))
        .basic_auth("admin", Some(ADMIN_KEY))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(admin_body(ADMIN_KEY))
        .send()
        .await
        .unwrap();
    assert_eq!(pin.status(), reqwest::StatusCode::SEE_OTHER);
    // Re-pushing the pinned version replaces its bytes, not the pin.
    push_multipart(
        &server,
        API_KEY,
        build_nupkg("Lts.Pkg", "1.0.0", b"rebuilt"),
    )
    .await;
    for v in ["2.0.0", "3.0.0"] {
        push_multipart(&server, API_KEY, build_nupkg("Lts.Pkg", v, b"x")).await;
    }
    let versions: serde_json::Value = server
        .client
        .get(server.url("/v3/package/lts.pkg/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(versions["versions"], serde_json::json!(["1.0.0", "3.0.0"]));
}

#[tokio::test]
async fn admin_requires_authentication() {
    let server = spawn_admin().await;
    // No credentials -> 401 with a Basic-auth challenge.
    let resp = server
        .client
        .get(server.url("/admin"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert!(resp.headers()["www-authenticate"]
        .to_str()
        .unwrap()
        .contains("Basic"));
    // Correct credentials -> 200.
    let ok = server
        .client
        .get(server.url("/admin"))
        .basic_auth("admin", Some(ADMIN_KEY))
        .send()
        .await
        .unwrap();
    assert!(ok.status().is_success());
}

#[tokio::test]
async fn the_admin_dashboard_filters_and_reads_its_query_leniently() {
    let server = spawn_admin().await;
    push_multipart(&server, API_KEY, build_nupkg("Dash.Pkg", "1.0.0", b"data")).await;
    let get = |path: &'static str| {
        let request = server
            .client
            .get(server.url(path))
            .basic_auth("admin", Some(ADMIN_KEY));
        async move {
            let resp = request.send().await.unwrap();
            assert!(resp.status().is_success(), "{path}: {}", resp.status());
            resp.text().await.unwrap()
        }
    };
    // An unknown filter and a mistyped page size are the defaults, not a 400.
    let all = get("/admin?filter=bogus&take=&page=x").await;
    assert!(all.contains("/admin/packages/dash.pkg"), "{all}");
    assert!(all.contains("aria-current=\"true\">All (1)</a>"), "{all}");
    let pending = get("/admin?filter=pending").await;
    assert!(
        pending.contains("No version waits for approval"),
        "{pending}"
    );
    assert!(!pending.contains("/admin/packages/dash.pkg"), "{pending}");
}

/// A filter and a page together page through the filtered packages, and a
/// page size past the pager's largest is capped (`TEST-15`).
#[tokio::test]
async fn the_admin_dashboard_pages_through_a_filter_and_caps_the_page_size() {
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        c.admin_api_key = Some(ADMIN_KEY.into());
        let mut gated = feed("gated");
        gated.requires_approval = true;
        c.feeds = vec![gated];
    })
    .await;
    for id in ["Page.A", "Page.B", "Page.C"] {
        let pushed = push_to(
            &server,
            "/gated/api/v2/package",
            API_KEY,
            build_nupkg(id, "1.0.0", b"x"),
        )
        .await;
        assert_eq!(pushed.status(), reqwest::StatusCode::CREATED);
    }
    let get = |path: &'static str| {
        let request = server
            .client
            .get(server.url(path))
            .basic_auth("admin", Some(ADMIN_KEY));
        async move {
            let resp = request.send().await.unwrap();
            assert!(resp.status().is_success(), "{path}: {}", resp.status());
            resp.text().await.unwrap()
        }
    };
    let second = get("/gated/admin?filter=pending&take=1&page=2").await;
    let shown = |page: &str, id: &str| page.contains(&format!("/gated/admin/packages/{id}\""));
    assert!(shown(&second, "page.b"), "{second}");
    assert!(
        !shown(&second, "page.a") && !shown(&second, "page.c"),
        "{second}"
    );
    assert!(second.contains("Pending (3)"), "{second}");

    let capped = get("/gated/admin?filter=pending&take=100000").await;
    for id in ["page.a", "page.b", "page.c"] {
        assert!(shown(&capped, id), "{id}: {capped}");
    }
    assert!(capped.contains("take=100\""), "{capped}");
    assert!(!capped.contains("take=100000"), "{capped}");
}

#[tokio::test]
async fn admin_area_absent_without_key() {
    let server = spawn().await; // no admin key configured
    let resp = server
        .client
        .get(server.url("/admin"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn admin_disable_withholds_then_enable_restores() {
    let server = spawn_admin().await;
    push_multipart(&server, API_KEY, build_nupkg("Adm.Pkg", "1.0.0", b"data")).await;
    let client = no_redirect();
    let dl_url = "/v3/package/adm.pkg/1.0.0/adm.pkg.1.0.0.nupkg";

    // Disable the version.
    let resp = client
        .post(server.url("/admin/packages/adm.pkg/1.0.0/disable"))
        .basic_auth("admin", Some(ADMIN_KEY))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(admin_body(ADMIN_KEY))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::SEE_OTHER);

    // Disabled: not downloadable, and gone from the flat container / search.
    let dl = server.client.get(server.url(dl_url)).send().await.unwrap();
    assert_eq!(dl.status(), reqwest::StatusCode::NOT_FOUND);
    let fc = server
        .client
        .get(server.url("/v3/package/adm.pkg/index.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(fc.status(), reqwest::StatusCode::NOT_FOUND);

    // The admin page still shows it as disabled.
    let adm = server
        .client
        .get(server.url("/admin/packages/adm.pkg"))
        .basic_auth("admin", Some(ADMIN_KEY))
        .send()
        .await
        .unwrap();
    assert!(adm.text().await.unwrap().contains("disabled"));

    // Disable requires auth, too.
    let unauth = client
        .post(server.url("/admin/packages/adm.pkg/1.0.0/enable"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);

    // Re-enable: downloadable again.
    let resp = client
        .post(server.url("/admin/packages/adm.pkg/1.0.0/enable"))
        .basic_auth("admin", Some(ADMIN_KEY))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(admin_body(ADMIN_KEY))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::SEE_OTHER);
    let dl = server.client.get(server.url(dl_url)).send().await.unwrap();
    assert!(dl.status().is_success());
}

#[tokio::test]
async fn admin_delete_removes_version_and_symbols() {
    let server = spawn_admin().await;
    let pdb = build_portable_pdb(&[9u8; 16]);
    let key = yanuget::pdb::portable_pdb_signature(&pdb).unwrap();
    push_multipart(
        &server,
        API_KEY,
        build_nupkg_for_pdb("Del.Pkg", "1.0.0", "del.pkg.pdb", &pdb),
    )
    .await;

    // Attach symbols, then confirm they are reachable.
    push_symbol(
        &server,
        API_KEY,
        build_snupkg("Del.Pkg", "1.0.0", "del.pkg.pdb", &pdb),
    )
    .await;
    let sym_url = format!("/download/symbols/del.pkg.pdb/{key}/del.pkg.pdb");
    assert!(server
        .client
        .get(server.url(&sym_url))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());

    // Admin delete removes the version...
    let resp = no_redirect()
        .post(server.url("/admin/packages/del.pkg/1.0.0/delete"))
        .basic_auth("admin", Some(ADMIN_KEY))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(admin_body(ADMIN_KEY))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::SEE_OTHER);
    let dl = server
        .client
        .get(server.url("/v3/package/del.pkg/1.0.0/del.pkg.1.0.0.nupkg"))
        .send()
        .await
        .unwrap();
    assert_eq!(dl.status(), reqwest::StatusCode::NOT_FOUND);
    // ...and its symbols are gone too.
    let sym = server
        .client
        .get(server.url(&sym_url))
        .send()
        .await
        .unwrap();
    assert_eq!(sym.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn gallery_page_size_is_configurable() {
    let server = spawn_with(|c| c.gallery_page_size = 1).await;
    for id in ["Sz.A", "Sz.B"] {
        push_multipart(&server, API_KEY, build_nupkg(id, "1.0.0", b"x")).await;
    }
    // With a page size of 1 and 2 packages, a pager must appear.
    let body = server
        .client
        .get(server.url("/"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("class=\"pager\""));
    assert!(body.contains("of 2"));
    // The configured size is the one selected, and stays on offer.
    assert!(body.contains("<option value=\"1\" selected>"), "{body}");
}

#[tokio::test]
async fn settings_page_shows_policy_without_secrets() {
    let server = spawn_with(|c| {
        c.retention.enabled = true;
        c.retention.keep_latest_stable = Some(7);
    })
    .await;
    let resp = server
        .client
        .get(server.url("/settings"))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let body = resp.text().await.unwrap();
    assert!(body.contains("Settings"));
    assert!(body.contains("Required (API key)"));
    assert!(body.contains("Keep newest stable"));
    assert!(body.contains("7"));
    // The actual API key must never appear on the unauthenticated page.
    assert!(!body.contains(API_KEY));
}

#[tokio::test]
async fn gallery_paginates_results() {
    let server = spawn().await;
    for id in ["Pg.A", "Pg.B", "Pg.C"] {
        push_multipart(&server, API_KEY, build_nupkg(id, "1.0.0", b"x")).await;
    }
    // One result per page → a pager with a Next link must appear.
    let resp = server
        .client
        .get(server.url("/packages?take=1"))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(body.contains("class=\"pager\""));
    assert!(body.contains("of 3"));
    assert!(body.contains("skip=1")); // next page link
}

async fn get_text(server: &TestServer, path: &str) -> String {
    server
        .client
        .get(server.url(path))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

#[tokio::test]
async fn gallery_pager_goes_to_a_page_and_snaps_to_page_starts() {
    let server = spawn_with(|c| c.gallery_page_size = 2).await;
    for id in ["Pp.A", "Pp.B", "Pp.C", "Pp.D", "Pp.E"] {
        push_multipart(&server, API_KEY, build_nupkg(id, "1.0.0", b"x")).await;
    }

    // "Go to page" names a page, and the page is named in the title.
    let body = get_text(&server, "/packages?page=2").await;
    assert!(body.contains("3\u{2013}4 of 5"), "{body}");
    assert!(
        body.contains("page 2 of 3 \u{2014} YANuget</title>"),
        "{body}"
    );

    // `page` wins over `skip`.
    let body = get_text(&server, "/packages?page=3&skip=0").await;
    assert!(body.contains("5\u{2013}5 of 5"), "{body}");

    // Any offset snaps to the start of its page. That is what the page-size
    // form relies on: it sends the `skip` on screen with the new `take`.
    let body = get_text(&server, "/packages?skip=3").await;
    assert!(body.contains("3\u{2013}4 of 5"), "{body}");
    let body = get_text(&server, "/packages?skip=3&take=4").await;
    assert!(body.contains("1\u{2013}4 of 5"), "{body}");

    // With everything on one page, the page size is still on offer, so a
    // larger choice can be undone.
    let body = get_text(&server, "/packages?take=20").await;
    assert!(body.contains("<select id=\"pg-take\""), "{body}");
    assert!(!body.contains("pg-page"), "{body}");

    // No page is larger than the largest size the pager offers.
    let body = get_text(&server, "/packages?take=1000").await;
    assert!(body.contains("value=\"100\""), "{body}");
    assert!(!body.contains("value=\"1000\""), "{body}");

    // The search text survives the round trip through the forms.
    let body = get_text(&server, "/packages?q=pp&page=3").await;
    assert!(body.contains("5\u{2013}5 of 5"), "{body}");
    assert!(
        body.contains("<input type=\"hidden\" name=\"q\" value=\"pp\">"),
        "{body}"
    );

    // An offset past the end is still reported as such.
    let body = get_text(&server, "/packages?skip=99").await;
    assert!(body.contains("nothing on this page"), "{body}");
}

#[tokio::test]
async fn gallery_reads_its_query_string_leniently() {
    // The gallery's addresses are typed and edited by hand. An empty or
    // mistyped value falls back to its default; each of these was a 400.
    let server = spawn_with(|c| c.gallery_page_size = 2).await;
    for id in ["Ln.A", "Ln.B", "Ln.C"] {
        push_multipart(&server, API_KEY, build_nupkg(id, "1.0.0", b"x")).await;
    }
    for query in [
        "skip=",
        "take=",
        "take=abc",
        "page=",
        "page=two",
        "skip=-5",
        "prerelease=maybe",
    ] {
        let resp = server
            .client
            .get(server.url(&format!("/packages?{query}")))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK, "?{query}");
        let body = resp.text().await.unwrap();
        assert!(body.contains("1\u{2013}2 of 3"), "?{query}: {body}");
    }

    // A search past its last page is not a search without matches, and the
    // way back keeps the search and the page size.
    let body = get_text(&server, "/packages?q=ln&skip=100").await;
    assert!(!body.contains("No packages match"), "{body}");
    assert!(
        body.contains("?q=ln&amp;skip=2&amp;take=2\">Go to the last page (2)</a>"),
        "{body}"
    );
}

#[tokio::test]
async fn web_ui_can_be_disabled() {
    let server = spawn_with(|c| c.enable_web_ui = false).await;
    push_multipart(&server, API_KEY, build_nupkg("Hidden.Pkg", "1.0.0", b"x")).await;
    let resp = server
        .client
        .get(server.url("/packages/hidden.pkg"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
    // Root still serves the minimal fallback page.
    let root = server.client.get(server.url("/")).send().await.unwrap();
    assert!(root.status().is_success());
    // The docs site is part of the web UI, so it is gone too.
    let docs = server
        .client
        .get(server.url("/docs/"))
        .send()
        .await
        .unwrap();
    assert_eq!(docs.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn embedded_docs_site_is_served() {
    let server = spawn().await;
    // The embedded site is served at /docs/ (the placeholder in test builds, the
    // real mkdocs site in CI/release builds) as self-contained HTML.
    let resp = server
        .client
        .get(server.url("/docs/"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(content_type.starts_with("text/html"), "got {content_type}");
    assert!(!resp.text().await.unwrap().is_empty());

    // /docs (no trailing slash) redirects to /docs/ (reqwest follows it to 200).
    let red = server.client.get(server.url("/docs")).send().await.unwrap();
    assert!(red.status().is_success());

    // An unknown doc path is a 404, not a panic or a wildcard match.
    let missing = server
        .client
        .get(server.url("/docs/nope/not-here.html"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
}
