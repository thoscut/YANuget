//! Hardening end to end: forwarding-header trust, response headers, auth
//! gaps, CSRF, conditional downloads and cross-feed payload integrity.

mod common;

use std::io::{Cursor, Write};

use zip::write::SimpleFileOptions;

use common::*;

// ---------------------------------------------------------------------------
// Hardening: forwarding-header trust, response headers, auth gaps, CSRF,
// conditional downloads and cross-feed payload integrity.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forwarded_headers_are_ignored_from_an_untrusted_peer() {
    // Nobody is trusted, so the loopback test client is not a proxy either.
    let server = spawn_with(|c| c.trusted_proxies = Vec::new()).await;

    let index: serde_json::Value = server
        .client
        .get(server.url("/v3/index.json"))
        .header("X-Forwarded-Host", "evil.example.com")
        .header("X-Forwarded-Proto", "https")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // Every advertised resource must still point at this server. A client that
    // followed a poisoned packageContent URL would fetch its packages from
    // whoever set the header.
    let base = package_base_address(&index);
    assert!(
        !base.contains("evil.example.com"),
        "spoofed forwarded host reached a generated URL: {base}"
    );
    assert!(base.starts_with(&server.base), "unexpected base: {base}");
}

#[tokio::test]
async fn rate_limit_survives_spoofed_forwarded_for() {
    // With no trusted proxy, X-Forwarded-For is stripped, so every request is
    // attributed to the real peer and the throttle actually holds.
    let server = spawn_with(|c| {
        c.trusted_proxies = Vec::new();
        c.rate_limit.enabled = true;
        c.rate_limit.max_requests = 3;
        c.rate_limit.window_secs = 60;
    })
    .await;

    let mut statuses = Vec::new();
    for i in 0..8u8 {
        let resp = server
            .client
            .get(server.url("/v3/index.json"))
            // A fresh "client IP" every time — which is the whole point of the
            // bypass this guards against.
            .header("X-Forwarded-For", format!("203.0.113.{i}"))
            .send()
            .await
            .unwrap();
        statuses.push(resp.status());
    }
    assert!(
        statuses.contains(&reqwest::StatusCode::TOO_MANY_REQUESTS),
        "rotating X-Forwarded-For bypassed the rate limit: {statuses:?}"
    );
}

#[tokio::test]
async fn responses_carry_baseline_security_headers() {
    let server = spawn().await;

    let resp = server.client.get(server.url("/")).send().await.unwrap();
    let headers = resp.headers().clone();
    assert_eq!(headers.get("x-content-type-options").unwrap(), "nosniff");
    assert_eq!(headers.get("x-frame-options").unwrap(), "DENY");
    assert_eq!(headers.get("referrer-policy").unwrap(), "no-referrer");
    // Several `Vary` lines (the CORS layer adds its own), all of which count.
    let vary: Vec<&str> = headers
        .get_all("vary")
        .iter()
        .map(|v| v.to_str().unwrap())
        .collect();
    assert!(
        vary.iter().any(|v| v.contains("X-Forwarded-Host")),
        "vary was {vary:?}"
    );

    // The gallery renders package-supplied metadata, so it gets a policy that
    // denies everything except the two inline assets the server itself emits.
    let csp = headers
        .get("content-security-policy")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(csp.contains("default-src 'none'"), "csp was {csp}");
    assert!(csp.contains("frame-ancestors 'none'"), "csp was {csp}");
    assert!(csp.contains("script-src 'sha256-"), "csp was {csp}");

    // JSON protocol documents get the headers too, but no gallery CSP.
    let json = server
        .client
        .get(server.url("/v3/index.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        json.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
}

#[tokio::test]
async fn symbol_download_and_settings_require_read_auth() {
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        let mut private = feed("private");
        private.read_api_key = Some("read-key".into());
        c.feeds = vec![private];
    })
    .await;

    // A PDB carries source paths and, with embedded sources, code — so a gated
    // feed must gate it like any other package content.
    let sym = server
        .client
        .get(server.url("/private/download/symbols/app.pdb/ABCDEF01FFFFFFFF/app.pdb"))
        .send()
        .await
        .unwrap();
    assert_eq!(sym.status(), reqwest::StatusCode::UNAUTHORIZED);

    let settings = server
        .client
        .get(server.url("/private/settings"))
        .send()
        .await
        .unwrap();
    assert_eq!(settings.status(), reqwest::StatusCode::UNAUTHORIZED);

    // With the read key the settings page is reachable again.
    let ok = server
        .client
        .get(server.url("/private/settings"))
        .header("X-NuGet-ApiKey", "read-key")
        .send()
        .await
        .unwrap();
    assert!(ok.status().is_success());
}

#[tokio::test]
async fn admin_actions_require_a_csrf_token() {
    let server = spawn_admin().await;
    push_multipart(&server, API_KEY, build_nupkg("Csrf.Pkg", "1.0.0", b"x")).await;
    let client = no_redirect();
    let url = server.url("/admin/packages/csrf.pkg/1.0.0/disable");

    // Authenticated but with no token: this is what a cross-site form POST
    // looks like, since the browser attaches the Basic credentials by itself.
    let no_token = client
        .post(&url)
        .basic_auth("admin", Some(ADMIN_KEY))
        .send()
        .await
        .unwrap();
    assert_eq!(no_token.status(), reqwest::StatusCode::BAD_REQUEST);

    // A guessed token is no better.
    let bad_token = client
        .post(&url)
        .basic_auth("admin", Some(ADMIN_KEY))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("_csrf=not-the-token")
        .send()
        .await
        .unwrap();
    assert_eq!(bad_token.status(), reqwest::StatusCode::BAD_REQUEST);
    // The page a browser gets says what to do about it.
    let page = bad_token.text().await.unwrap();
    assert!(page.contains("(reload the page)"), "{page}");
    assert!(page.contains("<html"), "{page}");

    // A browser that tells us the request came from another site is refused
    // even when it somehow carries the token — by the server-wide guard on
    // cross-site writes, before the admin area sees it.
    let cross_site = client
        .post(&url)
        .basic_auth("admin", Some(ADMIN_KEY))
        .header("sec-fetch-site", "cross-site")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(admin_body(ADMIN_KEY))
        .send()
        .await
        .unwrap();
    assert_eq!(cross_site.status(), reqwest::StatusCode::FORBIDDEN);

    // The real thing, as the admin page submits it, still works.
    let good = client
        .post(&url)
        .basic_auth("admin", Some(ADMIN_KEY))
        .header("sec-fetch-site", "same-origin")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(admin_body(ADMIN_KEY))
        .send()
        .await
        .unwrap();
    assert_eq!(good.status(), reqwest::StatusCode::SEE_OTHER);

    // And the page really does hand out that token, so the UI keeps working.
    let page = server
        .client
        .get(server.url("/admin/packages/csrf.pkg"))
        .basic_auth("admin", Some(ADMIN_KEY))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // The token carries the second it was issued, so compare by checking it
    // rather than against a fresh one, which differs once the clock ticks.
    let marker = "<input type=\"hidden\" name=\"_csrf\" value=\"";
    let start = page.find(marker).expect("the page has a CSRF field") + marker.len();
    let token = &page[start..start + page[start..].find('"').unwrap()];
    assert!(yanuget::auth::AdminAuth::new(Some(ADMIN_KEY.to_string())).check_csrf(Some(token)));
}

#[tokio::test]
async fn package_download_is_conditional_and_cacheable() {
    let server = spawn().await;
    push_multipart(
        &server,
        API_KEY,
        build_nupkg("Cache.Pkg", "1.0.0", &vec![7u8; 4096]),
    )
    .await;
    let url = server.url("/v3/package/cache.pkg/1.0.0/cache.pkg.1.0.0.nupkg");

    let first = server.client.get(&url).send().await.unwrap();
    assert!(first.status().is_success());
    let etag = first
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let cache_control = first
        .headers()
        .get("cache-control")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(cache_control.contains("immutable"), "{cache_control}");

    // A restore that already holds the package pays for a header exchange, not
    // for the payload again.
    let second = server
        .client
        .get(&url)
        .header("If-None-Match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), reqwest::StatusCode::NOT_MODIFIED);
    assert!(second.bytes().await.unwrap().is_empty());

    // A stale validator still gets the bytes.
    let changed = server
        .client
        .get(&url)
        .header("If-None-Match", "\"something-else\"")
        .send()
        .await
        .unwrap();
    assert!(changed.status().is_success());
}

#[tokio::test]
async fn a_second_feed_cannot_republish_different_bytes_under_a_taken_version() {
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        c.feeds = vec![feed("one"), feed("two")];
    })
    .await;

    // The payload is stored once and shared by every feed that holds the
    // version, so the *first* upload's bytes are what all of them serve.
    let original = build_nupkg("Shared.Pkg", "1.0.0", b"original-payload");
    let resp = push_to(&server, "/one/api/v2/package", API_KEY, original.clone()).await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    // Pushing *different* bytes for the same id/version into another feed must
    // not succeed: the advertised hash would stop describing the served bytes,
    // and a client verifying packageHash would fail the restore.
    let different = build_nupkg("Shared.Pkg", "1.0.0", b"a-completely-different-payload");
    let resp = push_to(&server, "/two/api/v2/package", API_KEY, different).await;
    assert_eq!(resp.status(), reqwest::StatusCode::CONFLICT);

    // The identical package is still free to join a second feed.
    let resp = push_to(&server, "/two/api/v2/package", API_KEY, original).await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    // ...and what that feed advertises matches what it serves.
    let reg: serde_json::Value = server
        .client
        .get(server.url("/two/v3/registration/shared.pkg/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let advertised = reg["items"][0]["items"][0]["packageContent"]
        .as_str()
        .unwrap()
        .to_string();
    let served = server.client.get(&advertised).send().await.unwrap();
    assert!(served.status().is_success());
}

#[tokio::test]
async fn health_reports_readiness_and_liveness() {
    let server = spawn().await;
    for path in ["/health", "/health/ready", "/health/live"] {
        let resp = server.client.get(server.url(path)).send().await.unwrap();
        assert!(resp.status().is_success(), "{path} was {}", resp.status());
    }
}

#[tokio::test]
async fn symbols_are_scoped_to_the_feed_that_owns_the_package() {
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        c.feeds = vec![feed("one"), feed("two")];
    })
    .await;

    // Publish a package and its symbols into /one only.
    let pdb = build_portable_pdb(&[42u8; 16]);
    let resp = push_to(
        &server,
        "/one/api/v2/package",
        API_KEY,
        build_nupkg_for_pdb("Sym.Scoped", "1.0.0", "sym.scoped.pdb", &pdb),
    )
    .await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    let snupkg = build_snupkg("Sym.Scoped", "1.0.0", "sym.scoped.pdb", &pdb);
    let part = reqwest::multipart::Part::bytes(snupkg)
        .file_name("symbols.snupkg")
        .mime_str("application/octet-stream")
        .unwrap();
    let resp = server
        .client
        .put(server.url("/one/api/v2/symbol"))
        .header("X-NuGet-ApiKey", API_KEY)
        .multipart(reqwest::multipart::Form::new().part("package", part))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    // The SSQP key is a property of the PDB itself, so anyone holding the .pdb
    // can compute it and ask any feed for it. The store is global — a symbol is
    // addressed by its signature, not by feed — so the feed that owns the
    // package is what has to decide who may fetch it.
    let ssqp = symbol_key_for(&pdb);
    let path = format!("/download/symbols/sym.scoped.pdb/{ssqp}/sym.scoped.pdb");

    let from_owner = server
        .client
        .get(server.url(&format!("/one{path}")))
        .send()
        .await
        .unwrap();
    assert!(
        from_owner.status().is_success(),
        "the owning feed should serve its own symbols: {}",
        from_owner.status()
    );

    // /two never received this package, so it must not serve its symbols.
    let from_other = server
        .client
        .get(server.url(&format!("/two{path}")))
        .send()
        .await
        .unwrap();
    assert_eq!(
        from_other.status(),
        reqwest::StatusCode::NOT_FOUND,
        "a feed without the package leaked its symbols"
    );
}

/// The SSQP key a debugger computes for a Portable PDB: the GUID in canonical
/// order, upper-case hex, followed by the literal age `FFFFFFFF`.
fn symbol_key_for(pdb: &[u8]) -> String {
    // The fixture writes the GUID immediately after the 8-byte `#Pdb` name.
    let guid_at = pdb.len() - 20;
    let g = &pdb[guid_at..guid_at + 16];
    let order = [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];
    let mut s = String::new();
    for i in order {
        s.push_str(&format!("{:02X}", g[i]));
    }
    s.push_str("FFFFFFFF");
    s
}

#[tokio::test]
async fn require_license_acceptance_is_reported_as_the_author_declared_it() {
    let server = spawn().await;
    push_multipart(&server, API_KEY, build_nupkg("Lic.Accept", "1.0.0", b"x")).await;

    // The fixture's nuspec sets <requireLicenseAcceptance>true</...>. Reporting
    // it as false would tell clients the author asked for no acceptance step
    // when they did.
    let reg: serde_json::Value = server
        .client
        .get(server.url("/v3/registration/lic.accept/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        reg["items"][0]["items"][0]["catalogEntry"]["requireLicenseAcceptance"],
        serde_json::Value::Bool(true)
    );
}

#[tokio::test]
async fn embedded_icons_are_served_only_when_they_really_are_images() {
    let server = spawn().await;

    // A package with a genuine PNG icon.
    push_multipart(
        &server,
        API_KEY,
        build_nupkg_with_icon("Icon.Pkg", "1.0.0", b"x", Some(TINY_PNG)),
    )
    .await;

    let icon = server
        .client
        .get(server.url("/packages/icon.pkg/1.0.0/icon"))
        .send()
        .await
        .unwrap();
    assert!(icon.status().is_success());
    assert_eq!(icon.headers().get("content-type").unwrap(), "image/png");
    // Icon bytes come from an uploaded package, so they are attacker-controlled
    // content served same-origin. They must not be sniffable into a document,
    // framable, or able to load anything of their own.
    assert_eq!(
        icon.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
    assert_eq!(
        icon.headers().get("content-security-policy").unwrap(),
        "default-src 'none'"
    );
    assert_eq!(icon.bytes().await.unwrap().as_ref(), TINY_PNG);

    // An "icon" that is really an SVG — a script-bearing document — must not be
    // handed to a browser, whatever the manifest calls it.
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" onload="alert(1)"/>"#;
    push_multipart(
        &server,
        API_KEY,
        build_nupkg_with_icon("Evil.Icon", "1.0.0", b"x", Some(svg)),
    )
    .await;
    let refused = server
        .client
        .get(server.url("/packages/evil.icon/1.0.0/icon"))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), reqwest::StatusCode::NOT_FOUND);

    // A package with no icon simply has none.
    push_multipart(&server, API_KEY, build_nupkg("Plain.Pkg", "1.0.0", b"x")).await;
    let none = server
        .client
        .get(server.url("/packages/plain.pkg/1.0.0/icon"))
        .send()
        .await
        .unwrap();
    assert_eq!(none.status(), reqwest::StatusCode::NOT_FOUND);

    // The detail page links the icon only for the package that has one.
    let with_icon = server
        .client
        .get(server.url("/packages/icon.pkg"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(with_icon.contains("/packages/icon.pkg/1.0.0/icon"));
    let without = server
        .client
        .get(server.url("/packages/plain.pkg"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!without.contains("/icon\""));
}

#[tokio::test]
async fn the_semver1_hive_withholds_versions_that_client_cannot_parse() {
    let server = spawn().await;

    // `1.0.0` is SemVer1-safe. `2.0.0-alpha.1` has a dotted pre-release label
    // and `3.0.0+build` carries build metadata — both are SemVer2-only.
    for version in ["1.0.0", "2.0.0-alpha.1", "3.0.0+build"] {
        let resp = push_multipart(&server, API_KEY, build_nupkg("Hive.Pkg", version, b"x")).await;
        assert_eq!(resp.status(), reqwest::StatusCode::CREATED, "{version}");
    }

    // The service index points the two hives at different bases.
    let index: serde_json::Value = server
        .client
        .get(server.url("/v3/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let resource = |ty: &str| -> String {
        index["resources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["@type"] == ty)
            .unwrap_or_else(|| panic!("{ty} missing from the service index"))["@id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let sv1_base = resource("RegistrationsBaseUrl/3.4.0");
    let sv2_base = resource("RegistrationsBaseUrl/3.6.0");
    assert_ne!(
        sv1_base, sv2_base,
        "advertising one hive under both @types hands a SemVer1 client versions it cannot parse"
    );

    let versions_in = |doc: &serde_json::Value| -> Vec<String> {
        doc["items"][0]["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["catalogEntry"]["version"].as_str().unwrap().to_string())
            .collect()
    };

    let sv1: serde_json::Value = server
        .client
        .get(format!("{sv1_base}hive.pkg/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(versions_in(&sv1), vec!["1.0.0".to_string()]);

    let sv2: serde_json::Value = server
        .client
        .get(format!("{sv2_base}hive.pkg/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // Displayed as published: the SemVer2 hive keeps the build metadata.
    assert_eq!(
        versions_in(&sv2),
        vec![
            "1.0.0".to_string(),
            "2.0.0-alpha.1".to_string(),
            "3.0.0+build".to_string()
        ]
    );

    // A hive's documents must keep their self-references inside that hive,
    // otherwise a client following them crosses over and sees the versions the
    // hive just withheld.
    let sv1_leaf = sv1["items"][0]["items"][0]["@id"].as_str().unwrap();
    assert!(sv1_leaf.starts_with(&sv1_base), "leaked hive: {sv1_leaf}");
    let sv2_leaf = sv2["items"][0]["items"][0]["@id"].as_str().unwrap();
    assert!(sv2_leaf.starts_with(&sv2_base), "leaked hive: {sv2_leaf}");

    // A SemVer2-only version simply has no leaf in the SemVer1 hive.
    let missing = server
        .client
        .get(format!("{sv1_base}hive.pkg/2.0.0-alpha.1.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
    let present = server
        .client
        .get(format!("{sv2_base}hive.pkg/2.0.0-alpha.1.json"))
        .send()
        .await
        .unwrap();
    assert!(present.status().is_success());

    // Search links into the hive matching the caller's semVerLevel.
    let sv1_search: serde_json::Value = server
        .client
        .get(server.url("/v3/search?q=Hive.Pkg&prerelease=true"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let reg = sv1_search["data"][0]["registration"].as_str().unwrap();
    assert!(reg.starts_with(&sv1_base), "search linked to {reg}");

    let sv2_search: serde_json::Value = server
        .client
        .get(server.url("/v3/search?q=Hive.Pkg&prerelease=true&semVerLevel=2.0.0"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let reg = sv2_search["data"][0]["registration"].as_str().unwrap();
    assert!(reg.starts_with(&sv2_base), "search linked to {reg}");
}

#[tokio::test]
async fn http2_clients_are_given_this_servers_urls_not_localhost() {
    // HTTP/1.1 carries the target host in `Host`; HTTP/2 carries it in the
    // `:authority` pseudo-header, which hyper surfaces on the URI rather than as
    // a header. A server that only reads `Host` falls through to its default and
    // hands an HTTP/2 client absolute package URLs pointing at `localhost` —
    // every restore over HTTP/2 then fails.
    let server = spawn().await;
    let h2 = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .unwrap();

    let index: serde_json::Value = h2
        .get(server.url("/v3/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let base = package_base_address(&index);
    assert!(
        base.starts_with(&server.base),
        "HTTP/2 request produced {base}, expected it under {}",
        server.base
    );
    assert!(!base.contains("localhost"), "fell back to the default host");

    // And the URL it produced is actually fetchable.
    push_multipart(&server, API_KEY, build_nupkg("H2.Pkg", "1.0.0", b"x")).await;
    let versions = h2
        .get(format!("{base}h2.pkg/index.json"))
        .send()
        .await
        .unwrap();
    assert!(versions.status().is_success());
}

#[tokio::test]
async fn autocomplete_does_not_suggest_packages_the_filters_exclude() {
    let server = spawn().await;
    // One package with only a pre-release version, one with a stable release.
    push_multipart(
        &server,
        API_KEY,
        build_nupkg("Pre.Only", "1.0.0-alpha", b"x"),
    )
    .await;
    push_multipart(&server, API_KEY, build_nupkg("Stable.One", "1.0.0", b"x")).await;

    let ids = |q: &str| {
        let url = server.url(q);
        let client = server.client.clone();
        async move {
            let doc: serde_json::Value =
                client.get(url).send().await.unwrap().json().await.unwrap();
            doc["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        }
    };

    // With pre-releases included, both are suggested.
    let all = ids("/v3/autocomplete?q=&prerelease=true").await;
    assert!(all.contains(&"Pre.Only".to_string()));
    assert!(all.contains(&"Stable.One".to_string()));

    // Without them, suggesting `Pre.Only` sends the caller to a package it will
    // then find nothing in.
    let stable = ids("/v3/autocomplete?q=&prerelease=false").await;
    assert!(
        !stable.contains(&"Pre.Only".to_string()),
        "suggested a package with no matching version: {stable:?}"
    );
    assert!(stable.contains(&"Stable.One".to_string()));
}

/// A pre-release label differing only in case is the *same* version, and the
/// server has to treat it as one everywhere or it hands clients bytes that do
/// not match the hash it published for them.
///
/// Before this was fixed the two pushes below produced two database rows that
/// shared a single file on disk: the second push was accepted even with
/// `allow_overwrite` off, it replaced the first package's bytes, and the flat
/// container listed the version twice.
#[tokio::test]
async fn a_case_variant_of_a_published_prerelease_is_the_same_version() {
    let server = spawn().await;

    let first = build_nupkg("Case.Probe", "1.0.0-Beta", b"AAAA");
    let response = push_multipart(&server, API_KEY, first.clone()).await;
    assert_eq!(response.status(), 201);

    // Same version, different case, different bytes: a conflict, not a push.
    let second = build_nupkg("Case.Probe", "1.0.0-beta", b"BBBB");
    let response = push_multipart(&server, API_KEY, second).await;
    assert_eq!(
        response.status(),
        409,
        "a case-variant re-push must conflict, not silently overwrite"
    );

    // The flat container lists the version once, in its canonical form.
    let versions: serde_json::Value = server
        .client
        .get(server.url("/v3/package/case.probe/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(versions["versions"], serde_json::json!(["1.0.0-beta"]));

    // Both spellings resolve to the originally published bytes.
    for spelling in ["1.0.0-Beta", "1.0.0-beta"] {
        let response = server
            .client
            .get(server.url(&format!(
                "/v3/package/case.probe/{}/case.probe.{}.nupkg",
                spelling.to_ascii_lowercase(),
                spelling.to_ascii_lowercase()
            )))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{spelling}");
        let body = response.bytes().await.unwrap();
        assert_eq!(
            body.as_ref(),
            first.as_slice(),
            "{spelling} served other bytes"
        );
    }
}

/// The SSQP key comes out of the uploaded PDB and the filename out of the zip
/// entry name, so both are chosen by whoever pushes. The symbol store is global
/// — a debugger asks for a key and nothing else — so a package must not be able
/// to claim a key another package already owns.
///
/// Before this was fixed, the second push below overwrote both the stored bytes
/// and the ownership row, and a debugger asking for the victim's key was served
/// the attacker's PDB.
#[tokio::test]
async fn a_package_cannot_claim_another_packages_symbols() {
    let server = spawn().await;

    // The victim publishes a package and its symbols.
    let guid = [7u8; 16];
    let victim_pdb = build_portable_pdb(&guid);
    let key = yanuget::pdb::portable_pdb_signature(&victim_pdb).expect("portable pdb key");
    push_multipart(
        &server,
        API_KEY,
        build_nupkg_for_pdb("Victim.Lib", "1.0.0", "victim.pdb", &victim_pdb),
    )
    .await;
    let response = push_symbol(
        &server,
        API_KEY,
        build_snupkg("Victim.Lib", "1.0.0", "victim.pdb", &victim_pdb),
    )
    .await;
    assert_eq!(response.status(), 201);

    // The path a debugger actually requests.
    let symbol_url = server.url(&format!("/download/symbols/victim.pdb/{key}/victim.pdb"));
    let fetch = || server.client.get(&symbol_url).send();

    let original = fetch().await.unwrap();
    assert_eq!(original.status(), 200);
    assert_eq!(
        original.bytes().await.unwrap().as_ref(),
        victim_pdb.as_slice()
    );

    // The attacker publishes their own package carrying a copy of the victim's
    // assembly — it is public, as is the debug GUID and PDB name in it — and
    // then a symbol package with a PDB under the victim's key and name.
    push_multipart(
        &server,
        API_KEY,
        build_nupkg_for_pdb("Attacker.Lib", "1.0.0", "victim.pdb", &victim_pdb),
    )
    .await;

    // A PDB of their own under that id fails the assembly's checksum...
    let mut forged_pdb = build_portable_pdb(&guid);
    forged_pdb[23] = b'9'; // "PDB v1.0" -> "PDB v1.9": other bytes, same id
    let response = push_symbol(
        &server,
        API_KEY,
        build_snupkg("Attacker.Lib", "1.0.0", "victim.pdb", &forged_pdb),
    )
    .await;
    assert_eq!(response.status(), 400, "a forged PDB must be refused");

    // ...and even the genuine bytes cannot move the key to their package.
    let attacker_pdb = build_portable_pdb(&guid);
    let response = push_symbol(
        &server,
        API_KEY,
        build_snupkg("Attacker.Lib", "1.0.0", "victim.pdb", &attacker_pdb),
    )
    .await;
    assert_eq!(
        response.status(),
        400,
        "claiming another package's symbol key must be refused"
    );

    // And the victim's symbols are untouched and still theirs.
    let after = fetch().await.unwrap();
    assert_eq!(after.status(), 200);
    assert_eq!(after.bytes().await.unwrap().as_ref(), victim_pdb.as_slice());
}

/// A symbol package names as many entries as it likes, and each one used to be
/// extracted by re-opening the archive and re-scanning its central directory —
/// quadratic in the entry count. A 200 KiB upload naming a couple of thousand
/// PDBs occupied a blocking thread for tens of seconds, and `tokio::fs` shares
/// that pool, so enough of them stall file I/O server-wide.
///
/// The entry count is now capped, and the check has to be cheap: this test
/// fails on the time as well as the status.
#[tokio::test]
async fn a_symbol_package_with_absurdly_many_pdbs_is_refused_quickly() {
    let server = spawn().await;
    push_multipart(&server, API_KEY, build_nupkg("Many.Pdbs", "1.0.0", b"dll")).await;

    // Entry names compress away to almost nothing, so this is a small upload.
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let opts =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("Many.Pdbs.nuspec", opts).unwrap();
        zip.write_all(
            br#"<?xml version="1.0"?><package><metadata><id>Many.Pdbs</id>
                <version>1.0.0</version><authors>a</authors>
                <description>d</description></metadata></package>"#,
        )
        .unwrap();
        for i in 0..4000 {
            zip.start_file(format!("lib/net8.0/f{i}.pdb"), opts)
                .unwrap();
            zip.write_all(&[0u8; 64]).unwrap();
        }
        zip.finish().unwrap();
    }
    let snupkg = cursor.into_inner();
    assert!(
        snupkg.len() < 1024 * 1024,
        "the point is that the upload is small"
    );

    let started = std::time::Instant::now();
    let response = push_symbol(&server, API_KEY, snupkg).await;
    let elapsed = started.elapsed();

    assert_eq!(
        response.status(),
        400,
        "an absurd entry count must be refused"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "took {elapsed:?}; the entry count should be rejected before any extraction"
    );
}

/// Entry names that differ only in case or separator used to count once
/// against the PDB cap — the listing is normalized — yet each was extracted
/// and verified, so a few hundred spellings of one PDB multiplied the work.
/// Such an archive is now refused before anything is extracted.
#[tokio::test]
async fn a_symbol_package_spelling_one_pdb_many_ways_is_refused() {
    let server = spawn().await;
    push_multipart(&server, API_KEY, build_nupkg("Same.Pdb", "1.0.0", b"dll")).await;

    let mut cursor = Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let opts =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("Same.Pdb.nuspec", opts).unwrap();
        zip.write_all(
            br#"<?xml version="1.0"?><package><metadata><id>Same.Pdb</id>
                <version>1.0.0</version><authors>a</authors>
                <description>d</description></metadata></package>"#,
        )
        .unwrap();
        for name in ["lib/net8.0/a.pdb", "LIB/net8.0/A.pdb", "lib\\net8.0\\a.PDB"] {
            zip.start_file(name, opts).unwrap();
            zip.write_all(&[0u8; 64]).unwrap();
        }
        zip.finish().unwrap();
    }

    let response = push_symbol(&server, API_KEY, cursor.into_inner()).await;
    assert_eq!(response.status(), 400);
    let body = response.text().await.unwrap();
    assert!(body.contains("differing only in case"), "{body}");
}

/// Overwriting a version must not leave the previous build's symbols behind.
///
/// The replacement build's PDBs carry different SSQP keys, so the old mappings
/// survived an overwrite and still resolved to an id/version that existed —
/// meaning a debugger attached to the new build was served the old build's PDB,
/// with its stale source mapping.
#[tokio::test]
async fn overwriting_a_version_retires_its_old_symbols() {
    let server = spawn_with(|c| c.allow_overwrite = yanuget::config::OverwriteMode::Enabled).await;

    let old_pdb = build_portable_pdb(&[0xA1; 16]);
    let old_key = yanuget::pdb::portable_pdb_signature(&old_pdb).unwrap();
    let new_pdb = build_portable_pdb(&[0xB2; 16]);
    push_multipart(
        &server,
        API_KEY,
        build_nupkg_for_pdb("Rebuilt.Lib", "1.0.0", "rebuilt.lib.pdb", &old_pdb),
    )
    .await;
    let response = push_symbol(
        &server,
        API_KEY,
        build_snupkg("Rebuilt.Lib", "1.0.0", "rebuilt.lib.pdb", &old_pdb),
    )
    .await;
    assert_eq!(response.status(), 201);

    let old_url = server.url(&format!(
        "/download/symbols/rebuilt.lib.pdb/{old_key}/rebuilt.lib.pdb"
    ));
    assert_eq!(
        server.client.get(&old_url).send().await.unwrap().status(),
        200,
        "the first build's symbols should be servable before the rebuild"
    );

    // Same version, rebuilt with different content.
    let response = push_multipart(
        &server,
        API_KEY,
        build_nupkg_for_pdb("Rebuilt.Lib", "1.0.0", "rebuilt.lib.pdb", &new_pdb),
    )
    .await;
    assert_eq!(response.status(), 201);

    assert_eq!(
        server.client.get(&old_url).send().await.unwrap().status(),
        404,
        "the superseded build's PDB must no longer be served"
    );

    // And the rebuild can publish its own symbols under a new key.
    let new_key = yanuget::pdb::portable_pdb_signature(&new_pdb).unwrap();
    let response = push_symbol(
        &server,
        API_KEY,
        build_snupkg("Rebuilt.Lib", "1.0.0", "rebuilt.lib.pdb", &new_pdb),
    )
    .await;
    assert_eq!(response.status(), 201);
    let new_url = server.url(&format!(
        "/download/symbols/rebuilt.lib.pdb/{new_key}/rebuilt.lib.pdb"
    ));
    let served = server.client.get(&new_url).send().await.unwrap();
    assert_eq!(served.status(), 200);
    assert_eq!(served.bytes().await.unwrap().as_ref(), new_pdb.as_slice());
}

/// The gallery is read in a browser, so its errors have to be pages.
///
/// Every one of them used to answer with a bare `{"error":"package not found"}`
/// — no chrome, no styling, no way back. Not a rare path either: the detail page
/// links every dependency by id, and on a private feed most dependencies come
/// from nuget.org and are not held locally, so the most obvious click on the
/// page produced raw JSON.
#[tokio::test]
async fn gallery_errors_are_pages_but_api_errors_stay_json() {
    let server = spawn().await;
    push_multipart(&server, API_KEY, build_nupkg("Real.Pkg", "1.0.0", b"x")).await;

    let response = server
        .client
        .get(server.url("/packages/no.such.package"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .starts_with("text/html"),
        "gallery 404 should be HTML"
    );
    let body = response.text().await.unwrap();
    assert!(body.contains("<html"), "{body}");
    assert!(body.contains("Not found"), "{body}");
    assert!(body.contains("Back to the package list"), "{body}");
    assert!(!body.contains(r#"{"error""#), "{body}");

    // A NuGet client still gets JSON, because that is what it parses.
    let response = server
        .client
        .get(server.url("/v3/package/no.such.package/index.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    let body = response.text().await.unwrap();
    assert!(
        !body.contains("<html"),
        "v3 errors must not be HTML: {body}"
    );
}

/// Promotion is how a version enters the target feed, so it has to clear that
/// feed's license policy — not the source feed's.
///
/// Without this, a permissive `dev` ring is a way around `stable`'s
/// `action = "block"`, and whoever holds `dev`'s admin key effectively has
/// write access to `stable`.
#[tokio::test]
async fn promotion_is_held_to_the_target_feeds_license_policy() {
    use yanuget::config::LicensePolicyConfig;

    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        c.admin_api_key = Some(ADMIN_KEY.into());
        let mut dev = feed("dev");
        dev.promotes_to = Some("stable".into());
        let mut stable = feed("stable");
        // `stable` accepts MIT only; `dev` accepts anything.
        stable.license_policy = LicensePolicyConfig {
            enabled: true,
            allowed: vec!["MIT".into()],
            action: yanuget::config::PolicyAction::Block,
            ..LicensePolicyConfig::default()
        };
        c.feeds = vec![dev, stable];
    })
    .await;

    // A GPL package is fine in dev.
    let nupkg = build_nupkg_with_license("Gpl.Pkg", "1.0.0", "GPL-3.0-only");
    let response = push_to(&server, "/dev/api/v2/package", API_KEY, nupkg).await;
    assert_eq!(response.status(), 201);

    let promote = no_redirect()
        .post(server.url("/dev/admin/packages/gpl.pkg/1.0.0/promote"))
        .basic_auth("admin", Some(ADMIN_KEY))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(admin_body(ADMIN_KEY))
        .send()
        .await
        .unwrap();
    assert_eq!(
        promote.status(),
        403,
        "stable blocks GPL, so promoting into it must be refused"
    );

    // And it really did not land there.
    let listed = server
        .client
        .get(server.url("/stable/v3/package/gpl.pkg/index.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(listed.status(), 404);
}

/// The three defaults that decide whether an out-of-the-box deployment is safe.
#[tokio::test]
async fn the_defaults_do_not_trust_the_network_around_them() {
    let server = spawn().await;

    // 1. Forwarding headers from an untrusted peer are ignored, so a client
    //    cannot steer the URLs handed to other clients — nor pick its own
    //    rate-limit bucket, which is what makes the throttle meaningful.
    let index: serde_json::Value = server
        .client
        .get(server.url("/v3/index.json"))
        .header("X-Forwarded-Proto", "https")
        .header("X-Forwarded-Host", "attacker.example.com")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let advertised = index["resources"][0]["@id"].as_str().unwrap();
    assert!(
        !advertised.contains("attacker.example.com"),
        "an untrusted peer steered a generated URL: {advertised}"
    );

    // 2. No CORS headers, so a page the operator's browser happens to load
    //    cannot read this feed's inventory cross-origin.
    let response = server
        .client
        .get(server.url("/v3/search?q="))
        .header("Origin", "https://evil.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(
        response
            .headers()
            .get("access-control-allow-origin")
            .is_none(),
        "a network-gated feed answered a cross-origin read"
    );

    // 3. The rate limit is on, but high enough that a large restore cannot
    //    trip it — NuGet treats 429 as terminal and will not retry.
    let config = yanuget::config::Config::default();
    assert!(config.rate_limit.enabled);
    assert!(config.rate_limit.max_requests >= 5_000);
}
