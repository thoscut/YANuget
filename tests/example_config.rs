//! The shipped example configuration must stay loadable.
//!
//! Unknown keys are a hard error, so a setting renamed in code but not in
//! `yanuget.example.toml` (or the reverse) breaks every operator who starts
//! from that file. These tests catch the drift at test time.

fn manifest_file(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

#[test]
fn shipped_example_config_parses() {
    let config: yanuget::config::Config =
        toml::from_str(&manifest_file("yanuget.example.toml")).expect("example config parses");

    // Spot-check that it really deserialized rather than falling back to
    // defaults for everything.
    assert!(config.rate_limit.enabled);
    assert_eq!(config.rate_limit.max_requests, 10_000);
    // Empty is the *default*, so it proves nothing on its own; the port does.
    assert_eq!(config.port, 5000);
    assert!(
        config.trusted_proxies.is_empty(),
        "example ships the secure default"
    );
    assert!(config.cors_allowed_origins.is_empty());
}

/// The `[[feeds]]` documentation is all commented out in the example file, so
/// the test above never exercises it. A feed's sub-tables attach to the most
/// recently declared `[[feeds]]` entry and are written `[feeds.mirror]` — the
/// `[feeds.<name>.mirror]` form that the docs used to show declares a table
/// called `<name>` *inside* the feed, which silently did nothing before unknown
/// keys became an error and is now rejected outright.
#[test]
fn the_documented_multi_feed_shape_parses_and_applies() {
    let toml = r#"
        api_key = "push"

        [[feeds]]
        name = "dev"
        promotes_to = "stable"

          [feeds.mirror]
          enabled = true
          upstream = "https://api.nuget.org/v3/index.json"

            [feeds.mirror.auth]
            token = "tok"

        [[feeds]]
        name = "stable"
        requires_approval = true

          [feeds.license_policy]
          enabled = true
          allowed = ["MIT"]
          action = "block"

          [feeds.retention]
          enabled = true
          keep_latest_stable = 20
    "#;
    let config: yanuget::config::Config = toml::from_str(toml).expect("documented shape parses");

    // Each sub-table must land on the feed it follows, not on some other one.
    let feeds = config.resolved_feeds().expect("feeds resolve");
    assert_eq!(feeds.len(), 2);

    let dev = &feeds[0];
    assert_eq!(dev.name, "dev");
    assert!(dev.mirror.enabled, "mirror did not attach to `dev`");
    assert_eq!(dev.mirror.auth.token.as_deref(), Some("tok"));
    assert!(!dev.license_policy.enabled);

    let stable = &feeds[1];
    assert_eq!(stable.name, "stable");
    assert!(stable.requires_approval);
    assert!(!stable.mirror.enabled);
    assert!(stable.license_policy.enabled);
    assert_eq!(stable.retention.keep_latest_stable, Some(20));
}

/// The wrong form is now an error rather than a silent no-op.
#[test]
fn a_named_feed_sub_table_is_rejected() {
    let toml = r#"
        [[feeds]]
        name = "dev"

          [feeds.dev.mirror]
          enabled = true
    "#;
    let err = toml::from_str::<yanuget::config::Config>(toml)
        .expect_err("`[feeds.dev.mirror]` should not be accepted")
        .to_string();
    assert!(err.contains("dev"), "unhelpful error: {err}");
}
