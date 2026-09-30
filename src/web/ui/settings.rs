//! The read-only settings page.

use super::escape::escape_html;
use super::format::{human_size, kv, kv_html};
use super::layout::{layout, Nav, VERSION};
use crate::config::Config;
use crate::nuget::UrlBuilder;

/// A read-only overview of the server's relevant settings.
///
/// Deliberately omits secrets and infrastructure details (the API key, data
/// paths and bind address) because the gallery is unauthenticated — it only
/// surfaces policy that affects how clients interact with the feed.
pub fn settings_page(urls: &UrlBuilder, config: &Config, feed: &crate::web::FeedContext) -> String {
    let yes_no = |b: bool| if b { "Yes" } else { "No" };
    let on_off = |b: bool| if b { "Enabled" } else { "Disabled" };

    let auth = if feed.auth.is_enabled() {
        "Required (API key)"
    } else {
        "Open \u{2014} no API key set"
    };
    let read_auth = if feed.read_auth.is_enabled() {
        "Required (credential)"
    } else {
        "Open"
    };
    let max_size = match config.max_package_size_bytes {
        Some(n) => human_size(n),
        None => "Unlimited".to_string(),
    };
    let delete_mode = if feed.hard_delete_enabled {
        "Hard delete (removes files)"
    } else {
        "Unlist (restorable)"
    };

    let mut server = String::from("<div class=\"kv wide\">");
    server.push_str(&kv("YANuget version", VERSION));
    server.push_str(&kv("Feed", &feed.name));
    server.push_str(&kv("Push / delete auth", auth));
    server.push_str(&kv("Download auth", read_auth));
    server.push_str(&kv("Max package size", &max_size));
    server.push_str(&kv(
        "Overwrite existing version",
        feed.allow_overwrite.label(),
    ));
    server.push_str(&kv("Delete behaviour", delete_mode));
    server.push_str(&kv("Approval required", yes_no(feed.requires_approval)));
    if let Some(target) = &feed.promotes_to {
        server.push_str(&kv("Promotes to", target));
    }
    server.push_str(&kv("Symbol server", on_off(config.enable_symbol_server)));
    server.push_str(&kv("Web gallery", on_off(config.enable_web_ui)));
    server.push_str(&kv("Preferred client", &config.primary_client));
    server.push_str("</div>");

    // Upstream mirroring + license policy (per feed).
    let mut policy = String::from("<div class=\"kv wide\">");
    match &feed.mirror {
        Some(m) => {
            policy.push_str(&kv("Upstream mirror", "Enabled"));
            policy.push_str(&kv("Upstream", &crate::config::url_origin(m.upstream())));
        }
        None => policy.push_str(&kv("Upstream mirror", "Disabled")),
    }
    let lp = &feed.license_policy;
    policy.push_str(&kv("License policy", on_off(lp.enabled)));
    if lp.enabled {
        let action = match lp.action {
            crate::config::PolicyAction::Block => "Block",
            crate::config::PolicyAction::Warn => "Warn (flag)",
        };
        policy.push_str(&kv("On violation", action));
        if !lp.allowed.is_empty() {
            policy.push_str(&kv("Allowed", &lp.allowed.join(", ")));
        }
        if !lp.blocked.is_empty() {
            policy.push_str(&kv("Blocked", &lp.blocked.join(", ")));
        }
        policy.push_str(&kv("Allow unlicensed", yes_no(lp.allow_unlicensed)));
    }
    policy.push_str("</div>");

    let r = &feed.retention;
    let mut retention = String::from("<div class=\"kv wide\">");
    retention.push_str(&kv("Retention", on_off(r.enabled)));
    if r.enabled {
        retention.push_str(&kv("Prune after each push", yes_no(r.prune_on_push)));
        let sweep = if r.interval_hours > 0 {
            format!("every {} h", r.interval_hours)
        } else {
            "off".to_string()
        };
        retention.push_str(&kv("Scheduled sweep", &sweep));
        retention.push_str(&kv("Keep newest stable", &opt_count(r.keep_latest_stable)));
        retention.push_str(&kv(
            "Keep newest pre-release",
            &opt_count(r.keep_latest_prerelease),
        ));
        retention.push_str(&kv(
            "Max version age",
            &match r.max_age_days {
                Some(d) => format!("{d} days"),
                None => "No limit".to_string(),
            },
        ));
    }
    retention.push_str("</div>");
    if feed.admin.is_enabled() {
        retention.push_str(&format!(
            "<p><a href=\"{}\">See what the next cleanup would delete</a></p>",
            escape_html(&urls.app("/admin/retention"))
        ));
    }

    let fc = &config.files;
    let mut files = String::from("<div class=\"kv wide\">");
    files.push_str(&kv("Attached files", on_off(fc.enabled)));
    if fc.enabled {
        files.push_str(&kv(
            "Uploads",
            if feed.auth.is_enabled() {
                "Accepted with the push key"
            } else {
                "Refused: this feed has no push key"
            },
        ));
        files.push_str(&kv(
            "Largest file",
            &match config.max_file_size_bytes() {
                Some(n) => human_size(n),
                None => "Unlimited".to_string(),
            },
        ));
        files.push_str(&kv("File types", &fc.allowed_extensions.join(", ")));
        files.push_str(&kv(
            "Unfinished uploads kept",
            &format!("{} h", fc.upload_expiry_hours),
        ));
        // Whether there is an inbox, not where: the path is infrastructure.
        files.push_str(&kv(
            "SSH inbox",
            if fc.inbox_dir.is_some() {
                "Enabled"
            } else {
                "Disabled"
            },
        ));
    }
    files.push_str("</div>");

    // Said even when the admin area is off: otherwise nothing on any page
    // tells an operator that disabling, deleting and moving versions exist.
    let admin = if feed.admin.is_enabled() {
        format!(
            "<div class=\"card\"><h2>Administration</h2>\
             <p>Approve, disable, delete and move package versions between feeds in the \
             <a href=\"{}\">admin area</a>. Sign in with the admin key.</p></div>",
            escape_html(&urls.app("/admin"))
        )
    } else {
        "<div class=\"card\"><h2>Administration</h2>\
         <p>Administration is turned off for this feed. Set <code>admin_api_key</code> \
         (or <code>YANUGET_ADMIN_API_KEY</code>) to approve, disable, delete and move \
         package versions from the gallery.</p></div>"
            .to_string()
    };

    let body = format!(
        "<h1 class=\"title\">Settings</h1>\
         <p class=\"muted\">Read-only overview of this feed's policy. \
         Secrets and storage paths are not shown.</p>\
         <div class=\"card\"><h2>Server</h2>{server}</div>\
         <div class=\"card\"><h2>Mirror &amp; policy</h2>{policy}</div>\
         <div class=\"card\"><h2>Retention</h2>{retention}</div>\
         <div class=\"card\"><h2>Attached files</h2>{files}</div>\
         <div class=\"card\"><h2>Endpoints</h2><div class=\"kv wide\">\
         {svc}{sym}</div></div>{admin}",
        svc = kv_html(
            "Service index",
            &format!(
                "<a href=\"{u}\">{u}</a>",
                u = escape_html(&urls.service_index())
            )
        ),
        sym = if config.enable_symbol_server {
            kv_html(
                "Symbol server",
                &format!("<code>{}</code>", escape_html(&urls.symbol_server())),
            )
        } else {
            String::new()
        },
    );
    layout(
        urls,
        "Settings \u{2014} YANuget",
        Nav {
            active: "settings",
            admin: feed.admin.is_enabled(),
        },
        &body,
    )
}

fn opt_count(n: Option<usize>) -> String {
    match n {
        Some(n) => n.to_string(),
        None => "No limit".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::ui::fixtures::feed_ctx;

    #[test]
    fn credentials_in_an_upstream_url_are_not_displayed() {
        // Only scheme and host: tokens live in userinfo, in paths
        // (`/_auth/TOKEN/`) and in queries, and this page is readable by
        // anyone on a feed without a read key.
        let urls = UrlBuilder::new("https://host");
        let config = crate::config::Config::default();
        let mut feed = feed_ctx(None, None);
        feed.mirror = crate::mirror::MirrorClient::from_config(&crate::config::MirrorConfig {
            enabled: true,
            upstream:
                "https://ci:pw-secret@feed.example.com/_auth/path-secret/v3/index.json?k=q-secret"
                    .into(),
            ..crate::config::MirrorConfig::default()
        });
        assert!(feed.mirror.is_some());
        let html = settings_page(&urls, &config, &feed);
        assert!(html.contains("https://feed.example.com"), "{html}");
        assert!(!html.contains("secret"), "{html}");
    }

    #[test]
    fn settings_page_hides_secrets_and_shows_admin_link() {
        let urls = UrlBuilder::new("https://host");
        let config = crate::config::Config::default();
        let feed = feed_ctx(Some("super-secret"), Some("admin-secret"));
        let html = settings_page(&urls, &config, &feed);
        assert!(html.contains("Required (API key)"));
        assert!(!html.contains("super-secret"));
        assert!(!html.contains("admin-secret"));
        assert!(html.contains("/admin")); // admin link present when configured

        let no_admin = feed_ctx(None, None);
        assert!(!settings_page(&urls, &config, &no_admin).contains("admin area"));
    }
}
