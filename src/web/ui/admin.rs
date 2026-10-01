//! The admin area's pages: the dashboard, a package's versions with their
//! actions, and the retention page.

use super::escape::{enc_path, escape_html};
use super::format::{group_digits, human_size, kv, plural};
use super::layout::{layout, Nav};
use crate::database::PackageFile;
use crate::nuget::UrlBuilder;

/// The form field (and header) carrying the admin CSRF token.
pub const CSRF_FIELD: &str = "_csrf";

/// The admin area's place in the header: current, and present.
const ADMIN_NAV: Nav<'static> = Nav {
    active: "admin",
    admin: true,
};

/// The admin dashboard: every package id, linking to its management page.
pub fn admin_dashboard_page(urls: &UrlBuilder, ids: &[String]) -> String {
    let retention = format!(
        "<a href=\"{}\">Retention: preview and clean up</a>",
        escape_html(&urls.app("/admin/retention"))
    );
    let body = if ids.is_empty() {
        format!(
            "<div class=\"bar\"><h1 class=\"title\">Admin</h1>{retention}</div>\
             <p class=\"muted\">No packages published yet.</p>"
        )
    } else {
        let mut list = String::from("<ul class=\"rank\">");
        for id in ids {
            list.push_str(&format!(
                "<li><a href=\"{href}\">{id}</a></li>",
                href = escape_html(
                    &urls.app(&format!("/admin/packages/{}", enc_path(&id.to_lowercase())))
                ),
                id = escape_html(id),
            ));
        }
        list.push_str("</ul>");
        format!(
            "<div class=\"bar\"><h1 class=\"title\">Admin</h1>{retention}</div>\
             <p class=\"muted\">Pick a package to approve, disable, pin, delete or move its \
             versions.</p>\
             <div class=\"card\">{list}</div>"
        )
    };
    layout(urls, "Admin \u{2014} YANuget", ADMIN_NAV, &body)
}

/// What the per-package admin page offers besides the versions themselves.
#[derive(Debug, Default, Clone, Copy)]
pub struct AdminPackageExtras<'a> {
    /// The next release ring, when this feed has one.
    pub promote_target: Option<&'a str>,
    /// The other feeds this admin may copy or move versions into; with none,
    /// the page offers neither.
    pub transfer_targets: &'a [String],
    /// What the next retention cleanup would delete of this package.
    pub retention_plan: &'a [crate::retention::Pruned],
    /// Whether files can be attached (`[files].enabled`).
    pub files_enabled: bool,
    /// The files attached to any of the versions.
    pub files: &'a [PackageFile],
}

/// The per-package admin page: every version (incl. disabled, pending and
/// flagged) with moderation actions.
///
/// Each row has its own buttons for the common one-version case. Below the
/// table, one form acts on every ticked version at once — which is also how a
/// whole package is disabled, deleted or moved: tick them all. The boxes sit
/// in the table but belong to that form through their `form` attribute,
/// because a form cannot wrap the rows' own forms.
pub fn admin_package_page(
    urls: &UrlBuilder,
    id: &str,
    versions: &[crate::database::FeedVersion],
    extras: &AdminPackageExtras,
    csrf_token: &str,
) -> String {
    let mut ordered: Vec<&crate::database::FeedVersion> = versions.iter().collect();
    ordered.sort_by(|a, b| b.package.version.cmp(&a.package.version));

    let lower = id.to_lowercase();
    let action = |v: &str, op: &str| {
        escape_html(&urls.app(&format!(
            "/admin/packages/{}/{}/{}",
            enc_path(&lower),
            enc_path(v),
            op
        )))
    };
    // Every admin form carries the CSRF token; the handlers reject a POST
    // without it, so a cross-site form submission cannot ride the browser's
    // auto-replayed Basic credentials.
    let csrf = format!(
        "<input type=\"hidden\" name=\"{CSRF_FIELD}\" value=\"{}\">",
        escape_html(csrf_token)
    );
    let button = |v: &str, op: &str, label: &str| {
        format!(
            "<form method=\"post\" action=\"{}\">{csrf}<button type=\"submit\">{label}</button></form>",
            action(v, op)
        )
    };

    let mut rows = String::new();
    for fv in &ordered {
        let p = &fv.package;
        let v = p.normalized_version();
        let dv = escape_html(&v);
        let mut status = if fv.pending {
            "<span class=\"badge pre\">pending</span>".to_string()
        } else if !p.enabled {
            "<span class=\"badge un\">disabled</span>".to_string()
        } else if p.listed {
            "<span class=\"badge ok\">active</span>".to_string()
        } else {
            "<span class=\"badge\">unlisted</span>".to_string()
        };
        if fv.flagged {
            status.push_str(" <span class=\"badge\" title=\"policy\">flagged</span>");
        }
        if fv.pinned {
            status.push_str(" <span class=\"badge pin\">pinned</span>");
        }

        let mut actions = String::new();
        // Only what clients could fetch too: a disabled or pending version is
        // withheld by the download endpoint, and a link to a 404 helps nobody.
        if p.enabled && !fv.pending {
            actions.push_str(&format!(
                "<a class=\"btn\" href=\"{}\" aria-label=\"Download {dv}\">Download</a>",
                escape_html(&urls.package_download(&lower, &v)),
            ));
        }
        if fv.pending {
            actions.push_str(&button(&v, "approve", "Approve"));
        }
        if let Some(target) = extras.promote_target {
            actions.push_str(&button(
                &v,
                "promote",
                &format!("Promote \u{2192} {}", escape_html(target)),
            ));
        }
        // Enable/disable and pin/unpin toggle with the current state.
        if p.enabled {
            actions.push_str(&button(&v, "disable", "Disable"));
        } else {
            actions.push_str(&button(&v, "enable", "Enable"));
        }
        if fv.pinned {
            actions.push_str(&button(&v, "unpin", "Unpin"));
        } else {
            actions.push_str(&button(&v, "pin", "Pin"));
        }
        // A pin keeps a version from retention, not from an admin: say so
        // before deleting one.
        let confirm = if fv.pinned {
            format!(
                "{id} {v} is pinned, which only keeps it from retention. Remove it from this \
                 feed anyway? If no other feed uses it, the files are deleted."
            )
        } else {
            format!(
                "Remove {id} {v} from this feed? If no other feed uses it, the files are deleted."
            )
        };
        actions.push_str(&format!(
            "<form method=\"post\" action=\"{a}\" data-confirm=\"{confirm}\">{csrf}\
             <button type=\"submit\" class=\"danger\">Delete</button></form>",
            a = action(&v, "delete"),
            confirm = escape_html(&confirm),
        ));

        let mut notes = match (fv.flagged, fv.flag_reason.as_deref()) {
            (true, Some(r)) => format!("<div class=\"meta\">{}</div>", escape_html(r)),
            _ => String::new(),
        };
        if let Some(planned) = extras
            .retention_plan
            .iter()
            .find(|planned| planned.version == p.version)
        {
            notes.push_str(&format!(
                "<div class=\"meta doomed\">The next cleanup deletes this: {}.</div>",
                escape_html(&planned.reason.describe())
            ));
        }
        rows.push_str(&format!(
            "<tr><td class=\"pick\"><input type=\"checkbox\" name=\"v\" value=\"{dv}\" \
             form=\"bulk\" aria-label=\"Select {dv}\"></td>\
             <td>{pre}<span class=\"nw\">{dv}</span>{notes}</td><td>{status}</td>\
             <td class=\"muted\">{dl}</td>\
             <td><div class=\"actions\">{actions}</div></td></tr>",
            pre = if p.is_prerelease() {
                "<span class=\"badge pre\">pre</span> "
            } else {
                ""
            },
            dl = group_digits(p.downloads as i64),
        ));
    }

    // The selection form. "Enable" comes first on purpose: pressing Enter in a
    // form submits it with its first button, and that must never be Delete.
    let approve = if ordered.iter().any(|fv| fv.pending) {
        "<button type=\"submit\" name=\"op\" value=\"approve\">Approve</button>"
    } else {
        ""
    };
    let transfer = if extras.transfer_targets.is_empty() {
        String::new()
    } else {
        let options: String = extras
            .transfer_targets
            .iter()
            .map(|t| format!("<option value=\"{t}\">{t}</option>", t = escape_html(t)))
            .collect();
        format!(
            "<span class=\"to\"><label for=\"bulk-target\">Feed</label>\
             <select id=\"bulk-target\" name=\"target\">{options}</select>\
             <button type=\"submit\" name=\"op\" value=\"copy\">Copy to feed</button>\
             <button type=\"submit\" name=\"op\" value=\"move\">Move to feed</button></span>"
        )
    };
    let bulk = format!(
        "<form id=\"bulk\" class=\"bulk\" method=\"post\" action=\"{act}\" \
         aria-labelledby=\"bulk-l\">{csrf}\
         <span id=\"bulk-l\"><b>With the selected versions</b></span>\
         <button type=\"submit\" name=\"op\" value=\"enable\">Enable</button>\
         <button type=\"submit\" name=\"op\" value=\"disable\">Disable</button>{approve}\
         <button type=\"submit\" name=\"op\" value=\"pin\">Pin</button>\
         <button type=\"submit\" name=\"op\" value=\"unpin\">Unpin</button>{transfer}\
         <button type=\"submit\" name=\"op\" value=\"delete\" class=\"danger\" \
         data-confirm=\"{confirm}\">Delete</button>\
         <span id=\"bulk-note\" role=\"status\"></span></form>",
        act = escape_html(&urls.app(&format!("/admin/packages/{}", enc_path(&lower)))),
        confirm = escape_html(&format!(
            "Remove the selected versions of {id} from this feed, pinned or not? \
             Versions no other feed holds are deleted for good."
        )),
    );

    let body = format!(
        "<nav class=\"crumbs\" aria-label=\"Breadcrumb\">\
         <a href=\"{admin}\">Admin</a> <span aria-hidden=\"true\">/</span> <span>{eid}</span></nav>\
         <div class=\"bar\"><h1 class=\"title\">{eid}</h1>\
         <a href=\"{gallery}\">Open in the gallery</a></div>\
         <p class=\"muted\">Disabled and pending versions are hidden from clients and not \
         downloadable. A pinned version is never deleted by retention. Delete removes this \
         feed's membership; moving keeps the files, since the other feed then holds them.</p>\
         <div class=\"card\"><div class=\"scroll\"><table class=\"atbl vers\">\
         <thead><tr><th class=\"pick\"><input type=\"checkbox\" class=\"all\" \
         aria-label=\"Select every version\" hidden></th>\
         <th>Version</th><th>Status</th><th>Downloads</th><th>Actions</th></tr></thead>\
         <tbody>{rows}</tbody></table></div>{bulk}</div>{files}",
        admin = escape_html(&urls.app("/admin")),
        gallery = escape_html(&urls.app(&format!("/packages/{}", enc_path(&lower)))),
        eid = escape_html(id),
        files = admin_files(urls, id, extras, &csrf),
    );
    layout(urls, &format!("Admin \u{2014} {id}"), ADMIN_NAV, &body)
}

/// The admin page's list of a package's attached files, each downloadable and
/// deletable, and how to attach more.
fn admin_files(urls: &UrlBuilder, id: &str, extras: &AdminPackageExtras, csrf: &str) -> String {
    if !extras.files_enabled {
        return String::new();
    }
    let how = format!(
        "<p class=\"muted\">Attach a file with <code>PUT {put}</code> and the push key, resumably \
         with tus at <code>{tus}</code>, or over SSH through the inbox; \
         <a href=\"{docs}\">the documentation</a> has the details.</p>",
        put = escape_html(&urls.app("/api/v2/files/{id}/{version}/{name}")),
        tus = escape_html(&urls.app("/api/v2/uploads")),
        docs = escape_html(&urls.app("/docs/api/#attached-files")),
    );
    if extras.files.is_empty() {
        return format!("<div class=\"card\"><h2>Files</h2><p>No files attached.</p>{how}</div>");
    }
    let mut rows = String::new();
    for f in extras.files {
        let action = escape_html(&urls.app(&format!(
            "/admin/packages/{}/{}/files/{}/delete",
            enc_path(&f.lower_id),
            enc_path(&f.normalized_version),
            enc_path(&f.name)
        )));
        rows.push_str(&format!(
            "<tr><td><span class=\"nw\">{v}</span></td><td>{name}</td><td class=\"muted\">{size}</td>\
             <td><code title=\"{sha}\">{short}\u{2026}</code></td><td class=\"muted\">{dl}</td>\
             <td><div class=\"actions\"><a class=\"btn\" href=\"{href}\" aria-label=\"Download {name}\">Download</a>\
             <form method=\"post\" action=\"{action}\" data-confirm=\"{confirm}\">{csrf}\
             <button type=\"submit\" class=\"danger\">Delete</button></form></div></td></tr>",
            v = escape_html(&f.normalized_version),
            name = escape_html(&f.name),
            size = human_size(f.size),
            sha = escape_html(&f.sha256),
            short = escape_html(&f.sha256[..f.sha256.len().min(12)]),
            dl = group_digits(f.downloads as i64),
            href = escape_html(&urls.file_download(&f.lower_id, &f.normalized_version, &f.name)),
            confirm = escape_html(&format!(
                "Delete {} from {id} {}? Install scripts that fetch it will fail.",
                f.name, f.normalized_version
            )),
        ));
    }
    format!(
        "<div class=\"card\"><h2>Files</h2><div class=\"scroll\"><table class=\"atbl files\">\
         <thead><tr><th>Version</th><th>File</th><th>Size</th><th>SHA-256</th><th>Downloads</th>\
         <th>Actions</th></tr></thead><tbody>{rows}</tbody></table></div>{how}</div>"
    )
}

/// What the retention page says after a cleanup it was asked for.
#[derive(Debug, Clone, Copy)]
pub enum RetentionNotice {
    None,
    /// A cleanup ran and did this.
    Done(crate::retention::Outcome),
    /// The plan changed between looking and clicking; nothing was deleted.
    Changed,
    /// A cleanup was already running; nothing was deleted.
    Busy,
}

/// Everything the retention page shows.
pub struct RetentionView<'a> {
    pub rules: &'a crate::config::RetentionConfig,
    pub last: Option<crate::retention::Report>,
    pub running: bool,
    /// The next cleanup's plan, when there are rules to plan with.
    pub preview: Option<&'a crate::retention::Preview>,
    pub csrf_token: &'a str,
    pub notice: RetentionNotice,
}

/// How many planned deletions the retention page lists before summarising.
const MAX_PREVIEW_ROWS: usize = 500;

/// The retention page: the rules as configured, what the last cleanup did,
/// and exactly what the next one would delete — with a button that deletes
/// that and nothing else.
pub fn admin_retention_page(urls: &UrlBuilder, view: &RetentionView) -> String {
    use crate::retention::Trigger;
    let rules = view.rules;
    let on_off = |b: bool| if b { "On" } else { "Off" };
    let limit = |n: Option<usize>| match n {
        Some(n) => format!("{n} per package"),
        None => "No limit".to_string(),
    };
    let notice = match view.notice {
        RetentionNotice::None => String::new(),
        RetentionNotice::Done(o) => {
            let errors = if o.errors > 0 {
                format!(
                    " {} could not be deleted; the details are in the server log.",
                    o.errors
                )
            } else {
                String::new()
            };
            format!(
                "<p class=\"notice\" role=\"status\">Deleted {} version{}, freeing {}.{errors}</p>",
                o.deleted,
                plural(o.deleted as i64),
                human_size(o.freed),
            )
        }
        RetentionNotice::Changed => "<p class=\"notice warn\" role=\"status\">The feed changed \
            since you looked, so nothing was deleted. Below is the list as it is now.</p>"
            .to_string(),
        RetentionNotice::Busy => "<p class=\"notice warn\" role=\"status\">A cleanup was already \
            running, so nothing was deleted. Look again once it has finished.</p>"
            .to_string(),
    };

    let mut kv_rows = String::from("<div class=\"kv wide\">");
    kv_rows.push_str(&kv("Retention", on_off(rules.enabled)));
    kv_rows.push_str(&kv(
        "Newest stable versions kept",
        &limit(rules.keep_latest_stable),
    ));
    kv_rows.push_str(&kv(
        "Newest pre-release versions kept",
        &limit(rules.keep_latest_prerelease),
    ));
    kv_rows.push_str(&kv(
        "Maximum age",
        &match rules.max_age_days {
            Some(d) => format!("{d} day{}", plural(d as i64)),
            None => "No limit".to_string(),
        },
    ));
    kv_rows.push_str(&kv(
        "Scheduled cleanup",
        &if rules.enabled && rules.interval_hours > 0 {
            format!("Every {} h", rules.interval_hours)
        } else {
            "Off".to_string()
        },
    ));
    kv_rows.push_str(&kv(
        "After each push",
        on_off(rules.enabled && rules.prune_on_push),
    ));
    kv_rows.push_str("</div>");

    let last = if view.running {
        "<p>A cleanup is running right now.</p>".to_string()
    } else {
        match view.last {
            None => {
                "<p class=\"muted\">No cleanup has run since the server started.</p>".to_string()
            }
            Some(r) => format!(
                "<p>Last cleanup {when} ({how}): deleted {n} version{s}, freed {freed}.</p>",
                when = escape_html(&r.finished.format("%Y-%m-%d %H:%M UTC").to_string()),
                how = match r.trigger {
                    Trigger::Schedule => "scheduled",
                    Trigger::Manual => "from this page",
                },
                n = r.outcome.deleted,
                s = plural(r.outcome.deleted as i64),
                freed = human_size(r.outcome.freed),
            ),
        }
    };

    let next = match view.preview {
        None => "<p>No rules are set, so a cleanup deletes nothing. Set \
                 <code>keep_latest_stable</code>, <code>keep_latest_prerelease</code> or \
                 <code>max_age_days</code> to give it some.</p>"
            .to_string(),
        Some(plan) if plan.planned.is_empty() => {
            "<p>Nothing to delete: every version is within the rules.</p>".to_string()
        }
        Some(plan) => {
            let mut table = String::from(
                "<div class=\"scroll\"><table class=\"atbl plan\"><thead><tr><th>Package</th>\
                 <th>Version</th><th>Published</th><th>Why</th><th>Frees</th></tr></thead><tbody>",
            );
            for p in plan.planned.iter().take(MAX_PREVIEW_ROWS) {
                table.push_str(&format!(
                    "<tr><td><a class=\"id\" href=\"{href}\">{id}</a></td>\
                     <td><span class=\"nw\">{v}</span></td><td class=\"muted\">{when}</td>\
                     <td>{why}</td><td class=\"muted\">{frees}</td></tr>",
                    href = escape_html(&urls.app(&format!(
                        "/admin/packages/{}",
                        enc_path(&p.id.to_lowercase())
                    ))),
                    id = escape_html(&p.id),
                    v = escape_html(&p.version.normalized()),
                    when = escape_html(&p.published.format("%Y-%m-%d").to_string()),
                    why = escape_html(&p.reason.describe()),
                    frees = if p.frees > 0 {
                        human_size(p.frees)
                    } else {
                        "Kept by another feed".to_string()
                    },
                ));
            }
            table.push_str("</tbody></table></div>");
            let more = plan.planned.len().saturating_sub(MAX_PREVIEW_ROWS);
            let more = if more > 0 {
                format!("<p class=\"muted\">And {more} more not listed here.</p>")
            } else {
                String::new()
            };
            let n = plan.planned.len();
            let action = if rules.enabled {
                format!(
                    "<form class=\"run\" method=\"post\" action=\"{act}\" data-confirm=\"{confirm}\">\
                     <input type=\"hidden\" name=\"{CSRF_FIELD}\" value=\"{csrf}\">\
                     <input type=\"hidden\" name=\"plan\" value=\"{fp}\">\
                     <button type=\"submit\" class=\"danger\">{label}</button>\
                     </form>",
                    label = if n == 1 {
                        "Delete this version now".to_string()
                    } else {
                        format!("Delete these {n} versions now")
                    },
                    act = escape_html(&urls.app("/admin/retention/run")),
                    confirm = escape_html(&format!(
                        "Delete {n} version{} from this feed now? Versions no other feed \
                         holds are deleted for good.",
                        plural(n as i64)
                    )),
                    csrf = escape_html(view.csrf_token),
                    fp = escape_html(&plan.fingerprint()),
                )
            } else {
                "<p class=\"muted\">Retention is off (<code>enabled = false</code>), so nothing \
                 is deleted. To clean up only from this page, set <code>enabled = true</code> \
                 and <code>interval_hours = 0</code>.</p>"
                    .to_string()
            };
            format!(
                "<p>{n} version{s} would be deleted, freeing {freed}.</p>{table}{more}{action}",
                s = plural(n as i64),
                freed = human_size(plan.frees()),
            )
        }
    };

    let pinned = match view.preview {
        Some(plan) if !plan.pinned.is_empty() => {
            let mut list = String::from("<ul class=\"rank\">");
            for (id, v) in &plan.pinned {
                list.push_str(&format!(
                    "<li><span><a class=\"id\" href=\"{href}\">{eid}</a> \
                     <span class=\"muted\">{v}</span></span></li>",
                    href = escape_html(
                        &urls.app(&format!("/admin/packages/{}", enc_path(&id.to_lowercase())))
                    ),
                    eid = escape_html(id),
                    v = escape_html(&v.normalized()),
                ));
            }
            list.push_str("</ul>");
            format!("<div class=\"card\"><h2>Pinned, kept regardless</h2>{list}</div>")
        }
        _ => String::new(),
    };

    let body = format!(
        "<nav class=\"crumbs\" aria-label=\"Breadcrumb\">\
         <a href=\"{admin}\">Admin</a> <span aria-hidden=\"true\">/</span> <span>Retention</span></nav>\
         <h1 class=\"title\">Retention</h1>{notice}\
         <div class=\"card\"><h2>Rules</h2>{kv_rows}\
         <p class=\"muted\">Set in the configuration file, under <code>[retention]</code> or a \
         feed's <code>[feeds.retention]</code>. The newest version of every package is always \
         kept, and pinned versions are kept whatever the rules say. The rules count only what \
         clients can download: pending and disabled versions are neither counted nor \
         deleted.</p>{last}</div>\
         <div class=\"card\"><h2>Next cleanup</h2>{next}</div>{pinned}",
        admin = escape_html(&urls.app("/admin")),
    );
    layout(urls, "Retention \u{2014} YANuget", ADMIN_NAV, &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::ui::fixtures::{feed_version, sample, text_of};
    use crate::web::ui::layout::COPY_SCRIPT_BODY;
    use crate::web::ui::package::{detail_page, Detail};

    #[test]
    fn the_admin_page_acts_on_the_selected_versions() {
        let urls = UrlBuilder::new("https://host");
        let mut beta = sample();
        beta.version = crate::version::NuGetVersion::parse("2.0.0-beta").unwrap();
        let versions = vec![
            feed_version(sample(), false, false),
            feed_version(beta, false, false),
        ];
        let html = admin_package_page(
            &urls,
            "Contoso.Utils",
            &versions,
            &AdminPackageExtras::default(),
            "tok",
        );
        // One box per version, belonging to the form below the table.
        for v in ["1.0.0", "2.0.0-beta"] {
            assert!(
                html.contains(&format!(
                    "<input type=\"checkbox\" name=\"v\" value=\"{v}\" form=\"bulk\""
                )),
                "{v}: {html}"
            );
        }
        assert!(
            html.contains(
                "<form id=\"bulk\" class=\"bulk\" method=\"post\" \
                 action=\"/admin/packages/contoso.utils\""
            ),
            "{html}"
        );
        assert!(html.contains("<input type=\"hidden\" name=\"_csrf\" value=\"tok\">"));
        // Enter submits with the first button, so that one must be harmless;
        // Delete asks first.
        let first = html.split("id=\"bulk\"").nth(1).unwrap();
        let first = first.split("<button").nth(1).unwrap();
        assert!(first.contains("value=\"enable\""), "{first}");
        assert!(html.contains("value=\"delete\" class=\"danger\" data-confirm="));
        // No other feed to hand versions to: no copy or move.
        assert!(!html.contains("value=\"move\""), "{html}");
        // Each servable version links to its package file.
        assert!(
            html.contains(
                "<a class=\"btn\" href=\"https://host/v3/package/contoso.utils/2.0.0-beta/\
                 contoso.utils.2.0.0-beta.nupkg\" aria-label=\"Download 2.0.0-beta\">Download</a>"
            ),
            "{html}"
        );

        let targets = ["stable".to_string()];
        let html = admin_package_page(
            &urls,
            "Contoso.Utils",
            &versions,
            &AdminPackageExtras {
                transfer_targets: &targets,
                ..Default::default()
            },
            "tok",
        );
        assert!(
            html.contains("<option value=\"stable\">stable</option>"),
            "{html}"
        );
        assert!(html.contains("name=\"op\" value=\"copy\""), "{html}");
        assert!(html.contains("name=\"op\" value=\"move\""), "{html}");
        // The confirmation is read off the button that submitted.
        assert!(COPY_SCRIPT_BODY.contains("e.submitter"));
    }

    #[test]
    fn admin_pages_render_actions() {
        let urls = UrlBuilder::new("https://host");
        let dash = admin_dashboard_page(&urls, &["Contoso.Utils".into()]);
        assert!(dash.contains("/admin/packages/contoso.utils"));

        let mut disabled = sample();
        disabled.enabled = false;
        let versions = vec![
            feed_version(sample(), false, false),
            feed_version(disabled, false, false),
        ];
        let pkg = admin_package_page(
            &urls,
            "Contoso.Utils",
            &versions,
            &AdminPackageExtras::default(),
            "tok",
        );
        assert!(pkg.contains("/disable"));
        assert!(pkg.contains("/enable"));
        assert!(pkg.contains("/delete"));
        assert!(pkg.contains("badge un")); // the disabled one
        assert!(pkg.contains("badge ok")); // the active one
    }

    #[test]
    fn the_admin_page_pins_and_says_what_the_next_cleanup_deletes() {
        let urls = UrlBuilder::new("https://host");
        let mut old = sample();
        old.version = crate::version::NuGetVersion::parse("0.9.0").unwrap();
        let mut pinned = feed_version(sample(), false, false);
        pinned.pinned = true;
        let versions = vec![pinned, feed_version(old.clone(), false, false)];
        let plan = [crate::retention::Pruned {
            version: old.version.clone(),
            reason: crate::retention::PruneReason {
                beyond_newest: None,
                older_than_days: Some(30),
            },
        }];
        let html = admin_package_page(
            &urls,
            "Contoso.Utils",
            &versions,
            &AdminPackageExtras {
                retention_plan: &plan,
                ..Default::default()
            },
            "tok",
        );
        assert!(
            html.contains("<span class=\"badge pin\">pinned</span>"),
            "{html}"
        );
        assert!(
            html.contains("/admin/packages/contoso.utils/1.0.0/unpin"),
            "{html}"
        );
        assert!(
            html.contains("/admin/packages/contoso.utils/0.9.0/pin"),
            "{html}"
        );
        assert!(
            html.contains("The next cleanup deletes this: older than 30 days."),
            "{html}"
        );
        // Deleting a pinned version says what a pin does and does not do.
        assert!(
            html.contains("is pinned, which only keeps it from retention"),
            "{html}"
        );
        assert!(html.contains("name=\"op\" value=\"pin\""), "{html}");
    }

    #[test]
    fn attached_files_are_listed_with_a_script_and_can_be_deleted() {
        let urls = UrlBuilder::new("https://host");
        let p = sample();
        let file = PackageFile {
            lower_id: "contoso.utils".into(),
            normalized_version: "1.0.0".into(),
            name: "base.wim".into(),
            sha256: "ab".repeat(32),
            size: 4 * 1024 * 1024 * 1024,
            uploaded: chrono::Utc::now(),
            downloads: 3,
        };
        let files = [file];
        let page = detail_page(
            &urls,
            std::slice::from_ref(&p),
            &p,
            &Detail {
                primary_client: "choco",
                files: &files,
                ..Default::default()
            },
        );
        let url = "https://host/files/contoso.utils/1.0.0/base.wim";
        assert!(
            page.contains(&format!("<a class=\"id\" href=\"{url}\">base.wim</a>")),
            "{page}"
        );
        assert!(page.contains("4.0 GB"), "{page}");
        // The script is escaped once, so the clipboard gets exactly this.
        let script = page
            .split("<pre><code>")
            .find(|s| s.contains("Start-BitsTransfer"))
            .and_then(|s| s.split("</code>").next())
            .map(text_of)
            .unwrap_or_else(|| panic!("no script: {page}"));
        assert!(script.contains(&format!(
            "Start-BitsTransfer -Source '{url}' -Destination $file"
        )));
        assert!(script.contains(&format!(
            "-Checksum '{}' -ChecksumType sha256",
            "ab".repeat(32)
        )));
        assert!(page.contains("aria-label=\"Copy the download script\""));

        let admin = admin_package_page(
            &urls,
            "Contoso.Utils",
            &[feed_version(sample(), false, false)],
            &AdminPackageExtras {
                files_enabled: true,
                files: &files,
                ..Default::default()
            },
            "tok",
        );
        assert!(
            admin.contains("action=\"/admin/packages/contoso.utils/1.0.0/files/base.wim/delete\""),
            "{admin}"
        );
        assert!(
            admin.contains("Install scripts that fetch it will fail."),
            "{admin}"
        );
        // With the feature off, the admin page does not mention files.
        let off = admin_package_page(
            &urls,
            "Contoso.Utils",
            &[feed_version(sample(), false, false)],
            &AdminPackageExtras::default(),
            "tok",
        );
        assert!(!off.contains("<h2>Files</h2>"), "{off}");
    }

    #[test]
    fn the_retention_page_shows_the_plan_and_deletes_only_when_enabled() {
        use crate::retention::{Planned, Preview, PruneReason};
        let urls = UrlBuilder::new("https://host");
        let plan = Preview {
            planned: vec![Planned {
                id: "Old.Pkg".into(),
                version: crate::version::NuGetVersion::parse("1.0.0").unwrap(),
                published: chrono::Utc::now(),
                reason: PruneReason {
                    beyond_newest: Some((2, false)),
                    older_than_days: None,
                },
                frees: 2048,
            }],
            pinned: vec![(
                "Lts.Pkg".into(),
                crate::version::NuGetVersion::parse("3.1.0").unwrap(),
            )],
        };
        let mut rules = crate::config::RetentionConfig {
            keep_latest_stable: Some(2),
            ..Default::default()
        };
        let page = |rules: &crate::config::RetentionConfig, notice| {
            admin_retention_page(
                &urls,
                &RetentionView {
                    rules,
                    last: None,
                    running: false,
                    preview: Some(&plan),
                    csrf_token: "tok",
                    notice,
                },
            )
        };
        let off = page(&rules, RetentionNotice::None);
        assert!(off.contains("beyond the newest 2 stable versions"), "{off}");
        assert!(
            off.contains("1 version would be deleted, freeing 2.0 KB."),
            "{off}"
        );
        assert!(off.contains(">Lts.Pkg</a>"), "{off}");
        // Off: the plan is shown, but there is nothing to press.
        assert!(!off.contains("/admin/retention/run"), "{off}");

        rules.enabled = true;
        let on = page(&rules, RetentionNotice::None);
        assert!(on.contains("action=\"/admin/retention/run\""), "{on}");
        assert!(
            on.contains(&format!("name=\"plan\" value=\"{}\"", plan.fingerprint())),
            "{on}"
        );
        assert!(on.contains("<input type=\"hidden\" name=\"_csrf\" value=\"tok\">"));

        let changed = page(&rules, RetentionNotice::Changed);
        assert!(changed.contains("nothing was deleted"), "{changed}");
        let done = page(
            &rules,
            RetentionNotice::Done(crate::retention::Outcome {
                deleted: 3,
                freed: 1024,
                errors: 0,
            }),
        );
        assert!(
            done.contains("Deleted 3 versions, freeing 1.0 KB."),
            "{done}"
        );
    }

    #[test]
    fn admin_page_shows_pending_and_promote() {
        let urls = UrlBuilder::new("https://host");
        let versions = vec![feed_version(sample(), true, true)];
        let pkg = admin_package_page(
            &urls,
            "Contoso.Utils",
            &versions,
            &AdminPackageExtras {
                promote_target: Some("stable"),
                ..Default::default()
            },
            "tok",
        );
        assert!(pkg.contains("/approve"));
        assert!(pkg.contains("/promote"));
        assert!(pkg.contains("pending"));
        assert!(pkg.contains("flagged"));
    }
}
