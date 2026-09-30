//! The admin area: the dashboard, a package's versions and the actions on
//! them (enable, disable, approve, pin, delete, promote, bulk copy and move),
//! attached-file removal, and the retention page.
//!
//! Every route here is mounted behind [`admin_gate`] as a `route_layer` (see
//! `feed_routes`), so no handler checks the admin credentials itself. State
//! changes are [`admin_post`] routes and additionally call
//! [`require_admin_action`] for the CSRF check, since only the handler reads
//! the form body.

use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::post;
use serde::Deserialize;

use super::forms::{form_field, form_fields, lenient};
use super::helpers::{check_id, parse_version};
use super::{hosted, ui, AppState, FeedMeta};
use crate::database::{Membership, MembershipChange};
use crate::error::{Error, Result};
use crate::retention::{self, RetentionPolicy};
use crate::version::NuGetVersion;

/// An admin form POST, capped at a size a form can plausibly be.
///
/// The global body limit is disabled so package uploads can stream to disk, but
/// these handlers read the body into memory to check the CSRF field. Without a
/// cap of their own, an authenticated admin POST with a multi-gigabyte body
/// would be buffered in full.
pub(super) fn admin_post<H, T>(handler: H) -> axum::routing::MethodRouter<AppState>
where
    H: axum::handler::Handler<T, AppState>,
    T: 'static,
{
    const MAX_ADMIN_FORM_BYTES: usize = 64 * 1024;
    post(handler).layer(DefaultBodyLimit::max(MAX_ADMIN_FORM_BYTES))
}

/// Guard every admin route: valid admin credentials or a Basic-auth
/// challenge, and never a cached copy of what is behind it.
///
/// Admin pages embed a CSRF token and list what an operator can see; neither
/// belongs in a browser's disk cache or a shared cache, so every answer —
/// the challenge included — is `no-store`.
pub(super) async fn admin_gate(
    State(state): State<AppState>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    let mut response = if state.feed.admin.check_headers(request.headers()) {
        next.run(request).await
    } else {
        Error::AdminUnauthorized.into_response()
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Header alternative to the hidden form field, for scripted admin calls.
const CSRF_HEADER: &str = "x-csrf-token";

/// Check that an admin *state change* was actually issued from the admin UI.
/// The credentials themselves were already checked by [`admin_gate`].
///
/// HTTP Basic credentials are replayed by the browser on every request to this
/// origin, so authentication alone does not distinguish a click in `/admin`
/// from a form auto-submitted by a hostile page in another tab. Two independent
/// checks close that:
///
/// * `Sec-Fetch-Site` — browsers set it on every request; anything other than
///   same-origin is rejected outright. Non-browser callers omit it.
/// * The CSRF token, an HMAC under a per-process secret (see
///   [`crate::auth::AdminAuth::csrf_token`]). An attacker who cannot read an
///   admin page cannot produce it.
fn require_admin_action(state: &AppState, headers: &HeaderMap, body: &str) -> Result<()> {
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        if !matches!(site.trim(), "same-origin" | "none") {
            return Err(Error::BadRequest(
                "cross-site admin requests are refused".into(),
            ));
        }
    }

    let presented = headers
        .get(CSRF_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .or_else(|| form_field(body, ui::CSRF_FIELD));

    if state.feed.admin.check_csrf(presented.as_deref()) {
        Ok(())
    } else {
        Err(Error::BadRequest(format!(
            "missing, expired or invalid {} token (reload the page)",
            ui::CSRF_FIELD
        )))
    }
}

pub(super) async fn admin_dashboard(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Html<String>> {
    let ids = state.db.all_package_ids(state.feed()).await?;
    let urls = state.url_builder(&headers);
    Ok(Html(ui::admin_dashboard_page(&urls, &ids)))
}

pub(super) async fn admin_package(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Html<String>> {
    check_id(&id)?;
    let versions = state.db.find_all_versions(state.feed(), &id).await?;
    if versions.is_empty() {
        return Err(Error::PackageNotFound);
    }
    let urls = state.url_builder(&headers);
    let targets: Vec<String> = transfer_targets(&state, &headers)
        .map(|m| m.name.clone())
        .collect();
    // The id as published, not as typed into the address.
    let display_id = &versions[0].package.id;
    // What the next cleanup would do to this package, when one will run.
    let policy = RetentionPolicy::from(&state.feed.retention);
    let plan = if state.feed.retention.enabled {
        retention::plan_for(&versions, &policy, chrono::Utc::now())
    } else {
        Vec::new()
    };
    // One query for the whole id rather than one per version, newest version
    // first, and only the versions this feed holds.
    let mut files = Vec::new();
    if state.config.files.enabled {
        let mut all = state.db.files_for_id(&id).await?;
        for fv in versions.iter().rev() {
            let v = fv.package.normalized_version();
            files.extend(all.extract_if(.., |f| f.normalized_version == v));
        }
    }
    Ok(Html(ui::admin_package_page(
        &urls,
        display_id,
        &versions,
        &ui::AdminPackageExtras {
            promote_target: state.feed.promotes_to.as_deref(),
            transfer_targets: &targets,
            retention_plan: &plan,
            files_enabled: state.config.files.enabled,
            files: &files,
        },
        &state.feed.admin.csrf_token().unwrap_or_default(),
    )))
}

/// Detach a file from the admin page.
pub(super) async fn admin_file_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version, name)): Path<(String, String, String)>,
    body: String,
) -> Result<Response> {
    check_id(&id)?;
    require_admin_action(&state, &headers, &body)?;
    let v = parse_version(&version)?;
    hosted::detach(&state, &id, &v, &name).await?;
    Ok(Redirect::to(&admin_package_url(&state.feed.prefix, &id)).into_response())
}

/// The feeds this request may copy or move versions into: every other feed
/// whose own admin key the request also presents, and the promotion target,
/// which the configuration already trusts this feed's admin to fill.
///
/// Holding one feed's admin key must not be a way to write into another:
/// without the target's key, a `dev` admin could move anything into `stable`.
fn transfer_targets<'a>(
    state: &'a AppState,
    headers: &'a HeaderMap,
) -> impl Iterator<Item = &'a FeedMeta> + 'a {
    state.feeds.iter().filter(move |m| {
        m.name != state.feed.name
            && (m.admin.check_headers(headers)
                || state.feed.promotes_to.as_deref() == Some(m.name.as_str()))
    })
}

/// Whether [`transfer_version`] leaves the version in this feed too.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Transfer {
    /// Add it to the target and keep it here (what a promotion does).
    Copy,
    /// Add it to the target and take it out of here.
    Move,
}

/// Copy or move one version of this feed into `target`.
///
/// The target's license policy and approval gate apply, exactly as for a push
/// into it. Either way the version arrives with the listed and enabled state
/// it has here, and still pending if it is pending here: a copy or promotion
/// must not re-publish what this feed withholds (an admin who disabled a
/// broken build would otherwise see it go live in the next ring). A move
/// also keeps its pin, since it is the same version, elsewhere. A version the
/// target already holds keeps its state there. Its files are never touched:
/// the target holds them afterwards either way.
async fn transfer_version(
    state: &AppState,
    target: &FeedMeta,
    id: &str,
    v: &NuGetVersion,
    mode: Transfer,
) -> Result<()> {
    // You can only hand on what *this* feed holds — not any globally-known
    // version that happens to live in some other feed. Checked again under
    // the lock below; this first look keeps such a version from being judged
    // against the target's policy at all.
    if !state.db.exists(state.feed(), id, v).await? {
        return Err(Error::PackageNotFound);
    }
    let package = state
        .db
        .get_package_data(id, v)
        .await?
        .ok_or(Error::PackageNotFound)?;

    // The target feed's own license policy, not this one's. A promotion is how
    // a version enters that feed, so it has to clear the same rule a direct
    // push would: otherwise `dev` with no policy is a way around `stable`'s
    // `action = "block"`.
    let outcome = crate::policy::evaluate_license(&target.license_policy, &package);
    if !outcome.allowed {
        return Err(Error::PolicyViolation(format!(
            "{} rejects this package: {}",
            target.name,
            outcome.violation.unwrap_or_else(|| "license policy".into())
        )));
    }

    // Under the same lock as a push or a purge of this version, so the checks
    // below and the membership changes after them see one consistent state.
    // Without it a retention sweep that drops the last membership in between
    // deletes the shared data, and the insert then leaves a membership
    // pointing at a package row that no longer exists — invisible to every
    // query (they all inner-join) yet enough to make a later push conflict.
    let _guard = crate::locks::lock_version(id, &v.normalized()).await;
    let Some(here) = state.db.get_membership(state.feed(), id, v).await? else {
        return Err(Error::PackageNotFound);
    };
    if !state.db.package_data_exists(id, v).await? {
        return Err(Error::PackageNotFound);
    }
    let membership = Membership {
        pending: target.requires_approval || here.pending,
        flagged: outcome.violation.is_some(),
        flag_reason: outcome.violation,
        listed: here.listed,
        enabled: here.enabled,
        pinned: mode == Transfer::Move && here.pinned,
        ..Membership::active(&target.name, &package)
    };
    match state.db.add_membership(&membership).await {
        Ok(()) | Err(Error::PackageAlreadyExists) => {}
        Err(e) => return Err(e),
    }
    if mode == Transfer::Move {
        state.db.remove_membership(state.feed(), id, v).await?;
        // Moved out on purpose: this feed's mirror must not fetch it back.
        if let Err(e) = state.db.add_tombstone(state.feed(), id, v).await {
            tracing::error!(feed = %state.feed(), %id, error = %e, "could not record the move");
        }
    }
    Ok(())
}

/// What the admin page's selection form asks for.
#[derive(Clone, Copy)]
enum BulkOp {
    Enable,
    Disable,
    Approve,
    Pin,
    Unpin,
    Delete,
    Copy,
    Move,
}

impl BulkOp {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "enable" => Self::Enable,
            "disable" => Self::Disable,
            "approve" => Self::Approve,
            "pin" => Self::Pin,
            "unpin" => Self::Unpin,
            "delete" => Self::Delete,
            "copy" => Self::Copy,
            "move" => Self::Move,
            _ => return None,
        })
    }
}

/// Apply one action to every version ticked on the admin page — the way a
/// whole package is disabled, deleted, or moved to another feed.
///
/// Every version is checked before anything changes, so a typo'd version or
/// a target that refuses the package leaves the feed as it was rather than
/// half done, and flag changes are applied in one transaction. Nothing selected sends the admin back to the page unchanged.
pub(super) async fn admin_bulk(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: String,
) -> Result<Response> {
    check_id(&id)?;
    require_admin_action(&state, &headers, &body)?;
    let back = Redirect::to(&admin_package_url(&state.feed.prefix, &id)).into_response();
    let op = form_field(&body, "op")
        .as_deref()
        .and_then(BulkOp::parse)
        .ok_or_else(|| Error::BadRequest("unknown or missing action".into()))?;
    let versions = form_fields(&body, "v")
        .iter()
        .map(|v| parse_version(v))
        .collect::<Result<Vec<_>>>()?;
    if versions.is_empty() {
        return Ok(back);
    }
    for v in &versions {
        if !state.db.exists(state.feed(), &id, v).await? {
            return Err(Error::PackageNotFound);
        }
    }

    // Flag changes go through in one transaction, so a database error part
    // way leaves every version as it was. Deletes and transfers also move
    // files and take per-version locks, which a transaction cannot cover;
    // they are checked up front and then applied version by version.
    let flag = match op {
        BulkOp::Enable => Some(MembershipChange::Enabled(true)),
        BulkOp::Disable => Some(MembershipChange::Enabled(false)),
        BulkOp::Approve => Some(MembershipChange::Approve),
        BulkOp::Pin => Some(MembershipChange::Pinned(true)),
        BulkOp::Unpin => Some(MembershipChange::Pinned(false)),
        BulkOp::Delete | BulkOp::Copy | BulkOp::Move => None,
    };
    if let Some(change) = flag {
        state
            .db
            .update_memberships(state.feed(), &id, &versions, change)
            .await?;
    }

    match op {
        BulkOp::Enable | BulkOp::Disable | BulkOp::Approve | BulkOp::Pin | BulkOp::Unpin => {}
        BulkOp::Delete => {
            for v in &versions {
                retention::purge_version(
                    state.storage.as_ref(),
                    state.db.as_ref(),
                    state.feed(),
                    &id,
                    v,
                )
                .await?;
            }
        }
        BulkOp::Copy | BulkOp::Move => {
            let name = form_field(&body, "target").unwrap_or_default();
            let target = transfer_targets(&state, &headers)
                .find(|m| m.name == name)
                .ok_or_else(|| {
                    Error::BadRequest(format!(
                        "{name:?} is not a feed these credentials may add versions to"
                    ))
                })?;
            // Refuse the lot up front if the target's policy refuses any of
            // it, rather than moving half a package.
            for v in &versions {
                let package = state
                    .db
                    .get_package_data(&id, v)
                    .await?
                    .ok_or(Error::PackageNotFound)?;
                let outcome = crate::policy::evaluate_license(&target.license_policy, &package);
                if !outcome.allowed {
                    return Err(Error::PolicyViolation(format!(
                        "{} rejects {} {}: {}",
                        target.name,
                        package.id,
                        v.normalized(),
                        outcome.violation.unwrap_or_else(|| "license policy".into())
                    )));
                }
            }
            let mode = if matches!(op, BulkOp::Move) {
                Transfer::Move
            } else {
                Transfer::Copy
            };
            for v in &versions {
                transfer_version(&state, target, &id, v, mode).await?;
            }
            tracing::info!(
                from = %state.feed(),
                to = %target.name,
                %id,
                count = versions.len(),
                moved = mode == Transfer::Move,
                "transferred versions"
            );
        }
    }

    // Back to the package if this feed still holds any of it, else the list.
    if state
        .db
        .find_all_versions(state.feed(), &id)
        .await?
        .is_empty()
    {
        return Ok(Redirect::to(&format!("{}/admin", state.feed.prefix)).into_response());
    }
    Ok(back)
}

pub(super) async fn admin_disable(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
    body: String,
) -> Result<Response> {
    admin_set_enabled(&state, &headers, &body, &id, &version, false).await
}

pub(super) async fn admin_enable(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
    body: String,
) -> Result<Response> {
    admin_set_enabled(&state, &headers, &body, &id, &version, true).await
}

async fn admin_set_enabled(
    state: &AppState,
    headers: &HeaderMap,
    body: &str,
    id: &str,
    version: &str,
    enabled: bool,
) -> Result<Response> {
    check_id(id)?;
    require_admin_action(state, headers, body)?;
    let v = parse_version(version)?;
    if !state.db.set_enabled(state.feed(), id, &v, enabled).await? {
        return Err(Error::PackageNotFound);
    }
    Ok(Redirect::to(&admin_package_url(&state.feed.prefix, id)).into_response())
}

pub(super) async fn admin_approve(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
    body: String,
) -> Result<Response> {
    check_id(&id)?;
    require_admin_action(&state, &headers, &body)?;
    let v = parse_version(&version)?;
    if !state.db.approve_membership(state.feed(), &id, &v).await? {
        return Err(Error::PackageNotFound);
    }
    Ok(Redirect::to(&admin_package_url(&state.feed.prefix, &id)).into_response())
}

pub(super) async fn admin_promote(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
    body: String,
) -> Result<Response> {
    check_id(&id)?;
    require_admin_action(&state, &headers, &body)?;
    let Some(target) = &state.feed.promotes_to else {
        return Err(Error::BadRequest(
            "this feed has no promotion target".into(),
        ));
    };
    let v = parse_version(&version)?;
    // The target must be a known feed; the copy clears its policy and gate.
    let target_meta = state
        .feeds
        .iter()
        .find(|m| &m.name == target)
        .ok_or_else(|| Error::BadRequest(format!("unknown promotion target {target:?}")))?;
    transfer_version(&state, target_meta, &id, &v, Transfer::Copy).await?;
    tracing::info!(from = %state.feed(), to = %target, %id, version = %v.normalized(), "promoted version");
    Ok(Redirect::to(&admin_package_url(&state.feed.prefix, &id)).into_response())
}

pub(super) async fn admin_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
    body: String,
) -> Result<Response> {
    check_id(&id)?;
    require_admin_action(&state, &headers, &body)?;
    let v = parse_version(&version)?;
    if !retention::purge_version(
        state.storage.as_ref(),
        state.db.as_ref(),
        state.feed(),
        &id,
        &v,
    )
    .await?
    {
        return Err(Error::PackageNotFound);
    }
    // Back to the package page if other versions remain, else the dashboard.
    let remaining = state.db.find_all_versions(state.feed(), &id).await?;
    let target = if remaining.is_empty() {
        format!("{}/admin", state.feed.prefix)
    } else {
        admin_package_url(&state.feed.prefix, &id)
    };
    Ok(Redirect::to(&target).into_response())
}

pub(super) async fn admin_pin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
    body: String,
) -> Result<Response> {
    admin_set_pinned(&state, &headers, &body, &id, &version, true).await
}

pub(super) async fn admin_unpin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
    body: String,
) -> Result<Response> {
    admin_set_pinned(&state, &headers, &body, &id, &version, false).await
}

async fn admin_set_pinned(
    state: &AppState,
    headers: &HeaderMap,
    body: &str,
    id: &str,
    version: &str,
    pinned: bool,
) -> Result<Response> {
    check_id(id)?;
    require_admin_action(state, headers, body)?;
    let v = parse_version(version)?;
    if !state.db.set_pinned(state.feed(), id, &v, pinned).await? {
        return Err(Error::PackageNotFound);
    }
    Ok(Redirect::to(&admin_package_url(&state.feed.prefix, id)).into_response())
}

/// What the retention page reports after a cleanup it was asked for: numbers
/// only, parsed, never echoed text.
#[derive(Debug, Deserialize)]
pub(super) struct RetentionQuery {
    #[serde(default)]
    deleted: Option<String>,
    #[serde(default)]
    freed: Option<String>,
    #[serde(default)]
    errors: Option<String>,
    #[serde(default)]
    changed: Option<String>,
    #[serde(default)]
    busy: Option<String>,
}

pub(super) async fn admin_retention(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<RetentionQuery>,
) -> Result<Html<String>> {
    let rules = &state.feed.retention;
    let policy = RetentionPolicy::from(rules);
    let preview = if policy.has_limits() {
        Some(
            retention::preview(state.db.as_ref(), state.feed(), &policy, chrono::Utc::now())
                .await?,
        )
    } else {
        None
    };
    let notice = if q.busy.is_some() {
        ui::RetentionNotice::Busy
    } else if q.changed.is_some() {
        ui::RetentionNotice::Changed
    } else if let Some(deleted) = lenient::<usize>(q.deleted.as_deref()) {
        ui::RetentionNotice::Done(retention::Outcome {
            deleted,
            freed: lenient(q.freed.as_deref()).unwrap_or(0),
            errors: lenient(q.errors.as_deref()).unwrap_or(0),
        })
    } else {
        ui::RetentionNotice::None
    };
    let urls = state.url_builder(&headers);
    Ok(Html(ui::admin_retention_page(
        &urls,
        &ui::RetentionView {
            rules,
            last: state.feed.cleanup.last(),
            running: state.feed.cleanup.is_running(),
            preview: preview.as_ref(),
            csrf_token: &state.feed.admin.csrf_token().unwrap_or_default(),
            notice,
        },
    )))
}

/// Delete what the retention page showed — and only that.
///
/// The form carries the fingerprint of the plan it displayed. The plan is
/// recomputed here and applied only when it still matches, so a push landing
/// between looking and clicking can never widen what the click deletes. A
/// cleanup already running (the background sweep, or another admin) turns
/// this away rather than queueing a second one behind it.
pub(super) async fn admin_retention_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> Result<Response> {
    require_admin_action(&state, &headers, &body)?;
    let policy = RetentionPolicy::from(&state.feed.retention);
    if !state.feed.retention.enabled || !policy.has_limits() {
        return Err(Error::BadRequest(
            "retention is not enabled for this feed".into(),
        ));
    }
    let fingerprint = form_field(&body, "plan").unwrap_or_default();
    let page = format!("{}/admin/retention", state.feed.prefix);
    let result = state
        .feed
        .cleanup
        .run_shown(
            state.storage.as_ref(),
            state.db.as_ref(),
            state.feed(),
            &policy,
            &fingerprint,
        )
        .await?;
    let target = match result {
        None => format!("{page}?busy=1"),
        Some(Err(_)) => format!("{page}?changed=1"),
        Some(Ok(o)) => {
            tracing::info!(feed = %state.feed(), deleted = o.deleted, freed = o.freed, "manual retention cleanup");
            format!(
                "{page}?deleted={}&freed={}&errors={}",
                o.deleted, o.freed, o.errors
            )
        }
    };
    Ok(Redirect::to(&target).into_response())
}

fn admin_package_url(prefix: &str, id: &str) -> String {
    format!("{prefix}/admin/packages/{}", id.to_lowercase())
}
