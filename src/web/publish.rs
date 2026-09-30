//! Changing a feed through the NuGet push API: pushing a package or a symbol
//! package (streamed to a temp file, then indexed), deleting or unlisting a
//! version, and relisting it.

use axum::extract::{FromRequest, Multipart, Path, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;

use super::helpers::{
    check_id, content_length, detached, map_upload_err, parse_version, to_io_err,
};
use super::AppState;
use crate::error::{Error, Result};
use crate::indexing::{self, IndexOptions};
use crate::retention::{self, RetentionPolicy};
use crate::storage::TempPath;
use crate::streaming::{self, StreamSummary};
use crate::symbols;

pub(super) async fn push_package(
    State(state): State<AppState>,
    request: Request,
) -> Result<Response> {
    let (temp, summary) = receive_push(&state, request).await?;

    let options = IndexOptions {
        overwrite: state.feed.allow_overwrite,
        pending: state.feed.requires_approval,
        license_policy: state.feed.license_policy.clone(),
        // A push is self-describing: the manifest defines the identity.
        expect: None,
        reserved_elsewhere: state.feed.reserved_elsewhere.clone(),
    };
    let state = state.clone();
    detached(async move {
        let result = indexing::index_package(
            state.storage.as_ref(),
            state.db.as_ref(),
            state.feed(),
            temp.path().to_path_buf(),
            summary,
            &options,
        )
        .await?;
        // Pushing a deleted version back is how it is un-deleted for the
        // mirror.
        if let Err(e) = state
            .db
            .clear_tombstone(state.feed(), &result.id, &result.version)
            .await
        {
            tracing::warn!(id = %result.id, error = %e, "could not clear the version's tombstone");
        }

        // Optionally prune older versions of this id in this feed
        // (best-effort: never fail the push because of retention).
        if state.feed.retention.enabled && state.feed.retention.prune_on_push {
            let policy = RetentionPolicy::from(&state.feed.retention);
            if let Err(e) = retention::prune_package(
                state.storage.as_ref(),
                state.db.as_ref(),
                state.feed(),
                &result.id,
                &policy,
            )
            .await
            {
                tracing::error!(id = %result.id, error = %e, "prune-on-push failed");
            }
        }
        Ok(())
    })
    .await?;

    Ok(StatusCode::CREATED.into_response())
}

/// The part of a push that a package and a symbol package share: check the
/// push key, then stream the body (raw or multipart) to a fresh temp file,
/// synced to disk. The temp file is removed on every way out that does not
/// store it — an error here, a failed index later, or a dropped connection.
async fn receive_push(state: &AppState, request: Request) -> Result<(TempPath, StreamSummary)> {
    let headers = request.headers();
    if !state.feed.auth.check_headers(headers) {
        return Err(Error::Unauthorized);
    }
    let is_multipart = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.starts_with("multipart/"));

    state.ensure_disk_space(content_length(headers))?;
    let (temp, mut file) = state.create_temp().await?;
    let limit = state.config.max_package_size_bytes;

    let summary = write_upload(request, &mut file, is_multipart, limit, state).await?;
    // Flush the OS page cache to stable storage before the payload is renamed
    // into the store. The database row that follows says the package exists; if
    // a crash lands between the rename and the kernel's own writeback, that row
    // would point at a truncated or empty file.
    file.sync_all().await?;
    drop(file);
    Ok((temp, summary))
}

/// Write the upload (raw body or the first multipart file field) to `file`.
async fn write_upload(
    request: Request,
    file: &mut tokio::fs::File,
    is_multipart: bool,
    limit: Option<u64>,
    state: &AppState,
) -> Result<StreamSummary> {
    if is_multipart {
        let mut multipart = Multipart::from_request(request, state)
            .await
            .map_err(|e| Error::BadRequest(format!("invalid multipart body: {e}")))?;
        // The package is the first field; NuGet sends exactly one file part.
        let field = multipart
            .next_field()
            .await
            .map_err(|e| Error::BadRequest(format!("invalid multipart field: {e}")))?
            .ok_or_else(|| Error::BadRequest("multipart body contained no file".into()))?;
        let stream = Box::pin(field.map(|r| r.map_err(to_io_err)));
        streaming::stream_to_writer_limited(stream, file, limit, state.config.upload_idle_timeout())
            .await
            .map_err(map_upload_err)
    } else {
        let stream = Box::pin(
            request
                .into_body()
                .into_data_stream()
                .map(|r| r.map_err(to_io_err)),
        );
        streaming::stream_to_writer_limited(stream, file, limit, state.config.upload_idle_timeout())
            .await
            .map_err(map_upload_err)
    }
}

pub(super) async fn delete_package(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
) -> Result<StatusCode> {
    check_id(&id)?;
    if !state.feed.auth.check_headers(&headers) {
        return Err(Error::Unauthorized);
    }
    let version = parse_version(&version)?;

    if state.feed.hard_delete_enabled {
        // Hard delete removes this feed's membership (and, when it was the last
        // feed, the payload, sidecars and indexed symbols).
        let removed = retention::purge_version(
            state.storage.as_ref(),
            state.db.as_ref(),
            state.feed(),
            &id,
            &version,
        )
        .await?;
        if removed {
            Ok(StatusCode::NO_CONTENT)
        } else {
            Err(Error::PackageNotFound)
        }
    } else {
        // The NuGet client's "delete" means "unlist".
        let updated = state
            .db
            .set_listed(state.feed(), &id, &version, false)
            .await?;
        if updated {
            Ok(StatusCode::NO_CONTENT)
        } else {
            Err(Error::PackageNotFound)
        }
    }
}

pub(super) async fn relist_package(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
) -> Result<StatusCode> {
    check_id(&id)?;
    if !state.feed.auth.check_headers(&headers) {
        return Err(Error::Unauthorized);
    }
    let version = parse_version(&version)?;
    if state
        .db
        .set_listed(state.feed(), &id, &version, true)
        .await?
    {
        Ok(StatusCode::OK)
    } else {
        Err(Error::PackageNotFound)
    }
}

pub(super) async fn push_symbol_package(
    State(state): State<AppState>,
    request: Request,
) -> Result<Response> {
    // The size and hash are computed while the bytes stream past, so recording
    // them costs nothing. Dropping them left the one number in the log that says
    // how much a symbol push actually cost, and any later question about which
    // bytes were stored, unanswerable.
    let (temp, summary) = receive_push(&state, request).await?;

    let indexing = state.clone();
    let result = detached(async move {
        symbols::index_symbol_package(
            indexing.storage.as_ref(),
            indexing.db.as_ref(),
            indexing.feed(),
            temp.path().to_path_buf(),
        )
        .await
    })
    .await?;
    tracing::info!(
        id = %result.id,
        version = %result.version.normalized(),
        indexed = result.indexed,
        skipped = result.skipped,
        bytes = summary.size,
        sha512 = %summary.sha512_base64,
        "indexed symbol package",
    );
    Ok(StatusCode::CREATED.into_response())
}
