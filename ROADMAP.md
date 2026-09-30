# Roadmap

What is planned, and what a review found that needs fixing. The README's
[Roadmap](README.md#roadmap) section lists what is already implemented; this file
is the work list.

Items are tagged with a stable id (`SEC-…`, `COR-…`, …) so commits and pull
requests can reference them, and ticked off rather than deleted when they land.

## Review of 2026-09-30

A full review of `main` at `4e086c5` (0.5.1 plus everything under
*Unreleased*), covering every module, the CI and release workflows, the Docker
image, the scripts, the docs and the test suite.

Baseline at the time: `cargo clippy --all-targets --all-features` clean,
`cargo test --all-features` green (321 tests), `cargo audit` clean apart from
the deliberately ignored `RUSTSEC-2023-0071` (see `DEP-01`).

How to read the entries:

- **Severity** is the reviewer's estimate of impact given the threat model in
  [SECURITY.md](SECURITY.md): *high* means a trust boundary SECURITY.md promises
  is crossed or data is lost in normal operation; *medium* means the same under
  a specific configuration or with an extra precondition; *low* is hardening,
  robustness or a narrow bug.
- **Confirmed** findings were traced through the code end to end. Findings
  marked *(unverified)* hold on reading but need a test, or depend on timing or
  on how a third-party client behaves.
- Entries describe the defect and the fix, not an exploit. Anything that turns
  out worse than written here should go through [SECURITY.md](SECURITY.md).
- **Unreleased** marks code that has not shipped in a tagged release yet; those
  items block the next release.

### Before the next release

The *Unreleased* features (attached files, the SSH inbox, tus uploads) carry the
two high-severity findings. Fix these before tagging 0.6.0:

- `SEC-01`, `SEC-02`, `SEC-03`: inbox symlink handling.
- `COR-06`, `COR-07`: blob GC and tus races.
- `COR-01`: retention and pending versions (released, but it now interacts
  with pins).
- `COR-02`: a data migration for pre-0.5.0 databases.

---

## Security

### High

- [ ] **`SEC-01` The inbox writes its `.error` report through symlinks.**
  *Unreleased.* `src/inbox.rs:126-140`. `try_exists` follows links and the
  report is written with `tokio::fs::write`, so a `{name}.error` symlink placed
  by the upload account makes the server create or truncate a file of its
  choosing, as the server's user. Absolute link targets resolve in the server's
  namespace, not the SFTP chroot, which defeats the documented "can reach
  nothing but the inbox" boundary.
  *Fix:* keep failure reports out of the uploader-writable tree (a DB row shown
  in `/admin`), or open with `O_NOFOLLOW | O_CREAT | O_EXCL` after removing any
  existing non-regular entry.

- [ ] **`SEC-02` The inbox stages, hashes and attaches whatever a path points
  at.** *Unreleased.* `src/inbox.rs:171-213`, `src/storage/filesystem.rs`
  (`store_blob`). The regular-file check happens in `scan`. `import` awaits
  other I/O and then `rename`s the path into staging without re-checking it. A
  symlink swapped in during that window is renamed into staging and hashed
  through the link; the mismatch report then echoes the target's hash, and a
  matching checksum attaches the link, which downloads then follow. A link to
  `/dev/zero` or a FIFO stalls the sequential scanner for every feed. Hard links
  are the same class where `protected_hardlinks` is off.
  *Fix:* open the inbox file once with `O_NOFOLLOW`, `fstat` it (regular file,
  `nlink == 1`), and copy it through that descriptor into a fresh server-owned
  staging file while hashing. Never rename an uploader-owned inode into the
  store; `store_blob` should refuse anything that is not a regular file. Do not
  echo the computed hash in the report.

- [ ] **`SEC-03` An inbox file can change after it was verified.**
  *Unreleased, (unverified).* Same code. `rename` keeps the inode, so an upload
  account that still holds an open SFTP handle can write to it after hashing.
  The content-addressed blob then no longer matches its name, and every later
  attach of that hash deduplicates onto it. Fixed by the copy in `SEC-02`;
  `store_blob` could also re-verify before deduplicating onto an existing blob.

### Medium

- [ ] **`SEC-04` Read-gated content is served as `Cache-Control: public,
  immutable`.** `src/web/mod.rs:54`, `src/web/files.rs:179`. Downloads,
  nuspecs, icons, symbols and attached files of a feed with `read_api_key` are
  marked `public`, and `Vary` names neither `Authorization` nor
  `X-NuGet-ApiKey`, so a shared cache or CDN in front of the server may serve
  them to anyone. `immutable` is also wrong when overwrite is enabled (see
  `COR-12`). *Fix:* `private` when read auth is on, and `immutable` only when
  overwrite is `Disabled`.

- [ ] **`SEC-05` Behind a trusted proxy, the rate limiter keys on the
  client-chosen end of `X-Forwarded-For`.** `src/ratelimit.rs:103-116`. Headers
  from untrusted peers are correctly stripped, but for a trusted peer the
  *leftmost* entry wins. nginx's usual `$proxy_add_x_forwarded_for` keeps what
  the client sent, so each request can pick its own bucket; the bucket map also
  grows with every one. *Fix:* hand the limiter the `TrustedProxies` set, walk
  the list right to left, skipping trusted hops, and unmap IPv4-mapped
  addresses. Also only accept `http`/`https` from `X-Forwarded-Proto`
  (`src/web/mod.rs`, `forwarded()`).

- [ ] **`SEC-06` The mirror's private-address guard misses DNS names on
  redirects, and checks each resource host only once.** `src/mirror.rs:107-127,
  198-228`, `src/proxy.rs:170-219`. The redirect policy classifies literals only
  (`localhost.` with a trailing dot, `*.nip.io`, or any name with a private A
  record passes). `check_url_resolved` runs once per process inside the
  `OnceCell`, so later requests resolve afresh (rebinding), and a resolution
  failure passes. reqwest also honours `HTTP(S)_PROXY` from the environment.
  `is_private_ip` lacks `0.0.0.0/8`, `198.18.0.0/15`, `240.0.0.0/4`, multicast,
  NAT64 `64:ff9b::/96`, 6to4 `2002::/16` and IPv4-compatible `::a.b.c.d`.
  *Fix:* a custom `reqwest::dns::Resolve` that drops private addresses unless
  `allow_private_upstream` (it covers redirects, rebinding and every resource
  URL at once), `.no_proxy()` unless configured, and the missing ranges.

- [ ] **`SEC-07` Upstream credentials go to every host the upstream's JSON
  names.** `src/mirror.rs:91, 305-310, 500-544`. Auth is set as default headers,
  so the `PackageBaseAddress`, `SearchQueryService` and catalog `@id`s taken
  from upstream JSON receive it wherever they point. Catalog page URLs are never
  passed through `check_url`. The same-host redirect rule compares host only, so
  an https→http or cross-port redirect still carries custom headers.
  *Fix:* attach credentials per request only when the origin matches the
  configured upstream (or an explicit allow-list); `check_url` every catalog
  page; compare scheme, host and port, and refuse downgrades.

- [ ] **`SEC-08` Symbol ownership is per package id, not per version or feed,
  and the check races.** `src/symbols.rs:121-131`, `src/database/sqlite.rs:1217-1229`.
  The CHANGELOG's cross-package fix holds, but anyone who can push the
  *identical* nupkg to another feed (it is adopted by hash) can then replace the
  PDB bytes that the first feed serves. A PDB id is public, so a key can also be
  claimed before its real owner pushes symbols. `find_symbol` → `store_symbol`
  → `add_symbol` runs without a lock, so two concurrent pushes can leave the
  row with one owner and the bytes from the other. *Fix:* check each PDB's
  id/age against a CodeView entry of an assembly in the owning version's
  nupkg (as nuget.org does), claim the row atomically under a per-key lock
  before writing, write via temp file and rename, and refuse to replace bytes
  whose hash differs.

- [ ] **`SEC-09` The nuspec parser can read a different identity than NuGet
  does.** `src/nuspec.rs:193-204, 339-371`. The last `<id>`/`<version>` wins
  (NuGet's `NuspecReader` takes the first), any descendant of `<metadata>`
  counts, namespaces are ignored, a child element inside `<id>` resets the
  text, and `<dependency>` is collected anywhere. The same applies to a
  duplicate `<license>`, which the license policy evaluates. The feed can then
  index an id, dependencies or license that differ from what a client reads
  from the same bytes. *Fix:* accept only `package/metadata/<field>` as direct
  children, in the nuspec namespace; reject duplicate scalar elements and child
  elements inside scalar fields.

- [ ] **`SEC-10` A crafted manifest costs CPU out of proportion to its size.**
  `src/nuspec.rs:124, 168, 244-266, 381-394`. quick-xml's duplicate-attribute
  check falls back to a linear scan, so an element with tens of thousands of
  attributes is quadratic per `attr()` pass (four passes per `<dependency>`),
  and `check_limits` re-sums every group on every element. Parsing runs on an
  async worker, so a few pushes stall the runtime. This is reachable by any
  pusher, and through mirrored upstreams. *(Timing unverified.)* *Fix:* run
  `parse_nuspec` in `spawn_blocking`, cap attributes per element and the nuspec
  size (1 MiB is ample), collect attributes in one pass, and keep running
  counters.

- [ ] **`SEC-11` A `.snupkg` push holds up to 512 MiB of decompressed PDBs in
  memory and stores truncated ones silently.** `src/symbols.rs:22-29, 97-141`,
  `src/nupkg.rs:137-170`. Highly compressible input makes this a small upload,
  and a few concurrent pushes exhaust memory. `extract_entries` cuts entries at
  the cap and `symbols.rs` never notices, so corrupt PDBs are indexed.
  *Fix:* stream each entry to a temp file, parse the key from its first KiB,
  and reject oversize entries explicitly.

- [ ] **`SEC-12` The license deny list is bypassable.** `src/policy.rs:138-179`.
  `GPL-2.0+` does not match a `GPL-2.0` rule (the SPDX `+` is not stripped);
  deprecated ids and `-only`/`-or-later` variants are not normalised; on an allow
  list, `MIT WITH <anything>` passes because the exception is never checked;
  `licenseUrl`-only and file licenses cannot be denied at all. *Fix:* normalise
  `+`, `-only`, `-or-later` and deprecated ids; accept only known (or
  configured) exception ids after `WITH`; document that a deny list is advisory
  unless an allow list is also set.

- [ ] **`SEC-13` No `Host` validation, and permissive CORS permits writes.**
  `src/web/mod.rs:202-219, 386-388, 612-615`. Any `Host` is accepted even with
  `base_url` set, so DNS rebinding can reach an intranet-only feed from a
  browser (and push or delete, when no API key is set, which is the default).
  `cors_allowed_origins = ["*"]` uses `CorsLayer::permissive()`, which allows
  every method and header; the relist `POST` is a CORS-simple request even
  without it. *Fix:* reject a `Host` that does not match `base_url` (or a new
  `allowed_hosts`) with 421; limit permissive CORS to GET/HEAD/OPTIONS; refuse
  writes carrying `Sec-Fetch-Site: cross-site`.

- [ ] **`SEC-14` Invalid environment overrides fail open, silently.**
  `src/config.rs:589-760`. `YANUGET_HOST=localhost` is ignored and the server
  binds `0.0.0.0`; `YANUGET_TLS_ENABLED=enabled` (any value outside the truthy
  list) turns TLS **off**; `YANUGET_MAX_PACKAGE_SIZE_BYTES=10G` means
  *unlimited*; a typo in `YANUGET_RATELIMIT_ENABLED` disables the limiter. The
  TOML file is strict (`deny_unknown_fields`), and the docs promise that "a
  mistyped security setting fails loudly". Relatedly, a `tls_cert_path` without
  `tls_key_path` (or the reverse) silently falls back to self-signed.
  *Fix:* refuse to start on any environment value that does not parse, and on a
  half-configured certificate pair.

- [ ] **`SEC-15` An id+version is one namespace across every feed.**
  `src/indexing.rs:240-254`, `src/mirror.rs:711`. Different bytes under a known
  id+version are correctly refused, which makes whoever stores it first own it
  everywhere: a push key on a low-trust feed, or an anonymous read of a mirror
  feed, can pre-claim a version that another feed then cannot receive (409 on
  push; the other mirror's fetch fails, logged as a benign race). Global
  metadata (readme, icon, attached files) is shared the same way, so detaching a
  file in one feed removes it from all. *Fix:* at least document it and log a
  hash mismatch as a failure; better, an id-prefix reservation per feed, or key
  global rows by content hash.

- [ ] **`SEC-16` Deleting a mirrored version is undone by the next anonymous
  read.** `src/mirror.rs:664`, `src/retention.rs:207`. Admin delete, hard
  `DELETE` and retention all remove the membership row, which is what the mirror
  checks, so a version removed as malicious comes straight back on a feed
  without `requires_approval`; retention and the mirror also fight over old
  versions. *Fix:* a per-feed tombstone written on delete and prune in mirror
  feeds; until then, document "disable, don't delete".

- [ ] **`SEC-17` Mirror downloads bypass `min_free_disk_bytes`.**
  `src/mirror.rs:376`, `src/web/mod.rs:273-296`. Pushes check free space;
  read-through fetches, triggered anonymously, do not, and can fill the volume
  the database lives on. *Fix:* run the same check against `Content-Length`
  before each download; optionally a global mirror byte budget.

### Low

- [ ] **`SEC-18` Key handling.** `src/auth.rs:152-195`. The CSRF token is a
  static `SHA-256(prefix ‖ admin_key)`, not session-bound and never rotated;
  admin pages that embed it lack `Cache-Control: no-store`. `constant_time_eq`
  returns early on a length mismatch. Push keys are trimmed, read and admin keys
  are not. *Fix:* HMAC with a random per-process secret and a timestamp,
  `no-store` on `/admin*`, compare fixed-length digests (`subtle`), and trim
  consistently.
- [ ] **`SEC-19` The rate limiter is weak against key guessing.**
  `src/ratelimit.rs:67-100`. Every IPv6 /128 gets its own bucket; `window_secs =
  0` disables limiting; there is no separate throttle for failed
  authentication, and no warning for short keys. *Fix:* bucket IPv6 by /64, add
  a strict limiter for 401s, warn on keys under 32 characters, and reject a zero
  window.
- [ ] **`SEC-20` Secrets can reach logs and pages.** Upstream URLs are printed
  with their userinfo (`src/migrate.rs:152`, `src/mirror.rs:286, 481, 490`), and
  reqwest errors embed the URL (logged at `src/web/mod.rs:292`); `/settings`
  redacts only `user:pass@`, not tokens in the path or query
  (`src/web/ui.rs:1992-2005`), and it is readable by anyone on a feed without a
  read key. `Config`, the auth structs and `MigrateArgs` derive `Debug` with raw
  secrets. *Fix:* keep a redacted display form, use `e.without_url()`, show only
  scheme and host on `/settings`, and give secret-bearing types a redacting
  `Debug`.
- [ ] **`SEC-21` Migration credentials only on the command line.**
  `src/main.rs:46-57`. `--source-password`, `--source-token` and
  `--source-header` show up in `ps`, `/proc/*/cmdline` and shell history; the
  docs' `"$TOKEN"` still expands into argv. *Fix:* `env =` with
  `hide_env_values`, or `--source-token-file`, and update `docs/migrate.md`.
- [ ] **`SEC-22` Unicode case folding differs between layers.** The database
  uses `to_lowercase()`, storage paths and the version lock use
  `to_ascii_lowercase()`, and ids taken from URLs are never validated. A
  `DELETE` with a Kelvin sign `K` matches the database row but neither the lock
  nor the directory: the rows go, the payload is orphaned, and a concurrent
  push is not serialised. Needs the push key. *Fix:* `validate_package_id` on
  every URL id and one canonicalisation function.
- [ ] **`SEC-23` Graceful shutdown and slow clients.** `src/main.rs:169-220`.
  The plain-HTTP path has no shutdown deadline (the TLS one has 10 s); the
  retention sweep is dropped at whatever await point it reached; neither server
  sets a header-read timeout or a connection cap, and the upload idle timeout
  is per chunk, so slow clients can hold sockets. *(Partly unverified.)*
- [ ] **`SEC-24` `index_page` writes the raw `Host`-derived URL into HTML.**
  `src/web/mod.rs:831-844` (only with `enable_web_ui = false`). Not exploitable
  cross-origin (browsers cannot set `Host`, and the CSP blocks inline script),
  but it is the one sink that skips `escape_html`.
- [ ] **`SEC-25` `security_headers` replaces `Vary`.** `src/web/mod.rs:575-578`.
  `insert` drops the `Vary: origin, access-control-request-*` that the CORS
  layer adds, so a shared cache can replay one origin's
  `Access-Control-Allow-Origin` to another. *Fix:* append.
- [ ] **`SEC-26` The ZIP duplicate-entry check can be sidestepped.**
  `src/nupkg.rs:230-271`. It reads the EOCD's total-entries field while the zip
  crate uses entries-on-this-disk, skips ZIP64, and takes the last EOCD
  signature where the crate skips invalid candidates. The impact is small (the
  NuGet client itself refuses several nuspecs). *Fix:* walk the central
  directory and count records.
- [ ] **`SEC-27` The multi-feed root lists every feed name without auth**,
  read-gated ones included (`src/web/mod.rs:308-319`). Probably fine; worth a
  decision and a sentence in the docs.

---

## Correctness and data integrity

### High

- [ ] **`COR-01` Retention ranks and protects versions that clients cannot
  see.** `src/retention.rs:95-121, 355-372`. `prune_plan` picks the "always
  keep" newest version and the "newest N" among every membership, including
  pending, disabled and unlisted ones. On a gated feed with `prune_on_push`,
  pushing N pending builds deletes every approved version, and rejecting the
  pending ones then leaves nothing. Disabling a broken newest version has the
  same effect by accident. *Fix:* rank and protect only servable memberships;
  treat pending and disabled versions like pins (outside the rules).

- [ ] **`COR-02` Pre-release keys stored before 0.5.0 were never migrated.**
  `src/version.rs:136-150`, `src/database/sqlite.rs:218-298`. 0.5.0 began
  lower-casing the pre-release label in `normalized()`, but rows written earlier
  as `1.0.0-Beta` in `packages`, `feed_packages`, `symbols`, `package_tags` and
  `package_files` keep their case. Listings still find them, while every exact
  lookup (download, delete, unlist, download count, retention purge) now misses:
  they 404, cannot be removed, and a re-push creates a second row sharing the
  lower-cased file, which is the bug 0.5.0 meant to fix. *Fix:* a `user_version
  = 3` migration that rewrites the keys in all five tables and resolves
  collisions.

### Medium

- [ ] **`COR-03` A purge that fails part-way strands the version for good.**
  `src/retention.rs:207-240`, `src/database/sqlite.rs:512-539`. Membership goes
  first, then files, then four DELETEs without a transaction. A failure after
  the files are gone leaves a `packages` row with no payload and no membership;
  nothing retries it. A later push of the same bytes adopts the missing payload
  (every download 404s), and different bytes get 409 in every feed. *Fix:* one
  transaction for `delete_package_data`; in `index_inner` treat "data exists,
  no memberships" (or a missing payload) as an orphan to purge and replace; a
  periodic orphan sweep.

- [ ] **`COR-04` An overwriting push is not atomic.** `src/indexing.rs:176-232,
  263-281`. The membership and package rows are deleted (errors discarded)
  before the new payload and sidecars are stored, so a failed store (disk full)
  loses the version from the database while bytes are orphaned or already
  replaced. *Fix:* store first, then swap the rows in one transaction, or
  restore `previous` on failure.

- [ ] **`COR-05` An overwrite resets admin state and drops attached files.**
  Same code. The new membership is always listed and enabled with zero
  downloads (only `pinned` is carried over), so a push key can undo an admin's
  *disable* on an ungated feed. `delete_package_data` removes the
  `package_files` rows but not their blobs: the version loses its attachments
  and the blobs leak. *Fix:* carry `enabled`, `listed` and `downloads` over, and
  keep (or explicitly purge) attached files.

- [ ] **`COR-06` Blob garbage collection races attaches to other versions.**
  *Unreleased.* `src/web/hosted.rs:227-278`, `src/retention.rs:247-264`. Blobs
  are shared by content, but locks are per version: an attach can find the blob
  present and drop its copy while a detach or purge of another version counts
  zero references and deletes it, leaving a row that points at nothing behind an
  immutable cache header. *Fix:* a lock keyed by `blob/{sha256}` held across
  store+insert and count+delete, or a transactional reference count.

- [ ] **`COR-07` tus upload state is read outside its lock.**
  *Unreleased, (unverified).* `src/web/hosted.rs:648-716`. `session()` reads
  `received` before `slot.try_lock()`, and `forget_slot` runs before the attach
  finishes. A client retrying a PATCH, which tus clients do, can write through a
  stale offset into the inode being renamed into the blob store. *Fix:* take
  the slot first, re-read the session under it, keep it until the attach
  completes, and rename the part file to a unique staging name first.

- [ ] **`COR-08` The read-through mirror never refreshes and ignores the
  requested version.** `src/web/mod.rs:1055-1061, 1202-1205`,
  `src/mirror.rs:648-652`. The flat index and registration call the mirror only
  when the feed holds no version of the id; after the first partial fill, new
  upstream releases never appear, contrary to `docs/configuration.md:242-245`.
  `ensure_package` keeps the newest N and never prioritises the version a
  client asked for, so pinning an older one 404s forever. *Fix:* fetch the
  requested version first, outside the cap; re-list after a per-(feed, id) TTL.

- [ ] **`COR-09` Mirror fetch limits and failure handling.**
  `src/mirror.rs:137, 266, 637`. The whole-download deadline is `timeout_secs`
  (default 30 s), so larger packages are never mirrored, and each miss
  re-downloads 30 s worth and discards it. There is no negative cache: every
  request for an unknown id, or during an upstream outage, goes upstream and
  waits out the connect timeout. With `requires_approval`, mirrored versions
  stay pending, so every read re-lists. The lock is keyed by id only, so two
  mirror feeds block each other, and concurrent restores of a new package get
  an immediate 404 while the first one fetches. JSON bodies are read unbounded.
  *Fix:* a separate download deadline, a TTL negative cache with backoff, a
  (feed, id) lock with a bounded wait on a shared future, and a byte cap on
  JSON.

- [ ] **`COR-10` A credentialed upstream that redirects downloads fails
  confusingly.** `src/mirror.rs:121-126, 332, 360-365`. `attempt.stop()` returns
  the 3xx itself, which `error_for_status` does not reject, so the redirect body
  is stored and fails as an invalid archive. That breaks any source serving
  packages from a blob store or CDN *(which vendors do this is unverified)*.
  *Fix:* follow cross-host redirects without credentials (per-request auth from
  `SEC-07`), or report the refusal explicitly.

- [ ] **`COR-11` `migrate` gaps.** `src/mirror.rs:408-544`, `src/main.rs:372-377`,
  `src/indexing.rs:268`.
  - A search that stops at a cap yields a partial list and the catalog is never
    consulted; a search error past a skip limit aborts the run instead of
    falling back.
  - Catalog pages that fail are only warned about, so the run can exit 0 with
    whole pages missing, contrary to the docs.
  - Every imported version is listed: deliberately unlisted versions are
    republished into search and "latest" (the mirror does the same).
  - With a target feed that allows overwrite, a re-run re-downloads everything,
    and with `Enabled` each version goes through the overwrite path of
    `COR-05`. *Fix:* default `opts.overwrite` to `Disabled` unless `--overwrite`.
  - *Fix, overall:* union search and catalog, record page failures in
    `summary.failures`, and read the source's `listed` flag.

- [ ] **`COR-12` Overwrite modes clash with caching.**
  `src/web/mod.rs:1094-1170`, `src/web/files.rs:62-78`. With overwrite
  allowed, a year of `immutable` keeps stale bytes in clients and proxies, and
  the nuspec and icon have no ETag. The ETag comes from `db.find` before the
  file is opened, so an overwrite in between pairs new bytes with the old tag
  and lets `If-Range` splice old and new. Hard-deleted packages also stay live in
  caches. *(Unverified.)*

### Low

- [ ] **`COR-13` Version identity edge cases.** `src/version.rs`.
  - `1.0.0-01` and `1.0.0-1` compare equal but normalise and hash differently,
    which breaks the `Hash`/`Eq` contract and permits two rows for one NuGet
    version.
  - Numeric components are `u64`, where NuGet uses `Int32`.
  - Build metadata is not validated (spaces, empty identifiers, non-ASCII all
    pass, and the original is persisted).
  - A leading `v` is accepted.
  - There is no length cap, so a long label surfaces as a late `ENAMETOOLONG`
    500.

  *Fix:* reject leading zeros in numeric pre-release identifiers, cap
  components at `i32::MAX` and the normalised length at 64 characters, and
  validate metadata like pre-release labels.
- [ ] **`COR-14` Protocol deviations.**
  - Autocomplete's `prerelease` defaults to `true` (spec: `false`,
    `src/web/mod.rs:1376`).
  - The standalone registration leaf `/{version}.json` returns the page-item
    shape rather than the leaf shape (`catalogEntry` as a URL, top-level
    `listed`/`published`; `src/nuget/mod.rs:223-225`).
  - Displayed versions are normalised (lower-cased label, no metadata).
  - Embedded icons get no `iconUrl`.
  - No `licenseUrl` is synthesised for expressions.
  - Dependency-group `@id` fragments are not percent-encoded.
  - Search `totalDownloads` sums only the filtered versions.
- [ ] **`COR-15` Downloads are counted on a `304`**, and the count is awaited
  before the file is served, so every restore waits on a SQLite write (up to the
  30 s busy timeout) despite the "never block" comment
  (`src/web/mod.rs:1141-1148`). *Fix:* count after the precondition check, off
  the request path.
- [ ] **`COR-16` Copy and promote re-enable what the source withheld**
  (`src/web/mod.rs:2079-2089`): the target membership is listed and enabled even
  if the source version was disabled or unlisted.
- [ ] **`COR-17` Multi-step writes without transactions.** Admin bulk actions
  (`src/web/mod.rs:2160-2235`) validate up front but can half-apply on a
  database error. Package and tag inserts are separate statements
  (`src/database/sqlite.rs:418-489`); a failed tag insert is never retried.
  Pins set during a long cleanup are not re-checked under the lock
  (`src/retention.rs:197-212, 502-513`).
- [ ] **`COR-18` Non-atomic writes into the store.**
  `src/storage/filesystem.rs:206-338`. `move_into_place` falls back to copying
  over the live file on *any* rename error, not just `EXDEV`, without fsync, and
  leaves a partial file on failure (duplicated in `store_symbol_package`).
  `store_aux` and `store_symbol` use plain `fs::write`. Directories are not
  fsynced after rename. *Fix:* copy to a temp file, fsync, rename, fsync the
  directory; fall back only on `CrossesDevices`.
- [ ] **`COR-19` Silent truncation of readmes and icons over 1 MiB**
  (`src/indexing.rs:130-141`, `src/nupkg.rs:196-200`), possibly mid-UTF-8, still
  flagged `has_readme`. Reject, or drop the flag.
- [ ] **`COR-20` Symbol pushes take no version lock** (`src/symbols.rs:57-146`),
  so they race purge and overwrite, and a rejected push is partially applied
  (PDBs before the offending one stay).
- [ ] **`COR-21` Cancellation leaves partial state.** Indexing and mirror fetches
  run inside the request future; a dropped connection between steps can leave a
  payload without a row, an overwrite with its rows already gone, or a leaked
  temp file. The startup sweep removes `*.tmp` but not `inbox-*.part`.
  *(Unverified.)* *Fix:* run post-upload indexing and mirror fetches in a
  spawned task and await its handle; RAII temp paths.
- [ ] **`COR-22` Ids that are Windows device names** (`Aux.Core`, `Con.Utils`,
  `COM1.Sdk`) pass `validate_package_id` but are refused at store time with a
  late 400, on every OS (`src/storage/filesystem.rs:368-394`). Check early with
  a clear message, or escape them on disk.
- [ ] **`COR-23` Two processes migrating the schema at once.**
  `src/database/sqlite.rs:261-296` uses a deferred transaction, so the loser can
  get an immediate `SQLITE_BUSY` (server plus `yanuget migrate` started
  together). *(Unverified.)* *Fix:* `BEGIN IMMEDIATE`.
- [ ] **`COR-24` Durability.** `synchronous = NORMAL` combined with
  files-first deletes means a power loss can roll back a committed DELETE while
  the payload stays unlinked. *(Unverified.)* Consider `FULL`.
- [ ] **`COR-25` Small things.**
  - `bytes=5-3` answers 416 where RFC 9110 says to ignore the header.
  - `u64::parse` accepts `+5` in a range.
  - `.nuspec` downloads read up to 16 MiB into memory instead of streaming.
  - A mirror client that fails to build silently disables the mirror, with no
    log.
  - Basic and token upstream auth together drop the token silently.
  - The error pages contain runs of spaces from a broken line continuation, and
    the admin 401 page mentions the *read* key (`src/web/ui.rs:490-506`).

---

## Performance

- [ ] **`PERF-01` Every search scans every version, twice.**
  `src/database/sqlite.rs:798-877`. The page query and the count query each run
  `lower()` and `LIKE '%q%'` across every version's description, tags and
  title; neither `q` nor `<description>` has a length cap. A few packages with
  multi-megabyte descriptions make every anonymous search expensive and can
  occupy the connection pool that downloads also need. `LIKE` on the raw JSON
  tags column matches `"` and `,`, and non-ASCII case folding differs between
  Rust and SQLite. *(Load impact unverified.)* *Fix:* cap `q` (256) and the
  description (4000, as nuget.org does), and index the latest version with FTS5.
- [ ] **`PERF-02` N+1 queries in the admin area.** `retention::preview`
  (`src/retention.rs:464-499`) runs several queries per id and per planned
  version on every GET of `/admin/retention`, and again on run; `admin_package`
  queries files per version (`src/web/mod.rs:1968`).
- [ ] **`PERF-03` `read_archive` opens every entry** (`src/nupkg.rs:57-71`):
  `by_index` seeks and reads each local header and builds a decompressor, and
  one encrypted or unsupported entry anywhere rejects the package. Use
  `name_for_index` and open only the nuspec; consider an entry-count cap, since
  the central directory costs about 5× its size in memory.

---

## Supply chain, CI and releases

- [ ] **`CI-01` (high) Release jobs hold a write token while running untrusted
  build code.** `.github/workflows/release.yml:15-16`. `contents: write` applies
  to every job, including the matrix builds that run crate build scripts and an
  unpinned `pip install`; `actions/checkout` persists the token. *Fix:*
  `permissions: {}` at the top, `contents: write` only on the `release` job,
  `persist-credentials: false` on every checkout. Add
  `permissions: contents: read` to `ci.yml`, which has none.
- [ ] **`CI-02` (high) No action is pinned to a commit SHA**, including those
  that run with write tokens (`softprops/action-gh-release`,
  `peter-evans/create-pull-request`, `docker/login-action`,
  `docker/build-push-action`) and mutable refs such as
  `taiki-e/install-action@cargo-audit` and `dtolnay/rust-toolchain@stable`.
  Pin to SHAs with version comments and let Dependabot update them.
- [ ] **`CI-03` Release builds restore caches.** `release.yml:93-101, 197-198`.
  Tag builds read the `~/.cargo`/`target/` cache and the Docker `type=gha` cache
  that jobs on `main` can write (including the media workflow, which runs npm,
  pip and Playwright). Build releases from a clean state.
- [ ] **`CI-04` Nothing is signed or attested.** Only an unsigned `SHA256SUMS`
  is published. Add `actions/attest-build-provenance` for archives and the
  image, sign the image with cosign keyless, consider `cargo auditable`, and
  document `gh attestation verify`.
- [ ] **`CI-05` No release gate.** `workflow_dispatch` runs the workflow file
  from any branch; `CARGO_REGISTRY_TOKEN` is a repository secret. Put `image`,
  `release` and `crates-io` in a protected `release` environment restricted to
  tags, and move to crates.io trusted publishing. On a manual dispatch a missing
  tag may be created at the dispatching branch head rather than the built
  commit *(unverified)*.
- [ ] **`CI-06` The media workflow runs third-party code with write access.**
  `media.yml:28-30`, `scripts/capture-media.sh:37, 42`. `contents` and
  `pull-requests: write` cover the whole job (`npm install`, an unpinned
  `pip install Pillow`, Playwright, mkdocs, cargo). Capture with read-only
  permissions and upload an artifact; push and open the PR from a separate job
  that runs no third-party code. Use `npm ci`, and pin Pillow.
- [ ] **`CI-07` Expressions interpolated into shell.** `release.yml:39, 132, 230`
  put `github.event.inputs.tag`, `github.ref_name` and job outputs straight into
  `run:`. Pass them via `env:` and quote. (Needs write access to exploit.)
- [ ] **`CI-08` Unpinned, unmonitored docs and media toolchains.**
  `requirements-docs.txt` is a floating range executed in the release and image
  builds; Dependabot has no `pip` or `npm` entries. Lock with hashes
  (`pip-compile --generate-hashes`, `--require-hashes`) and add both ecosystems.
- [ ] **`CI-09` `cargo audit` runs only on push and PR.** Add a scheduled run;
  consider `cargo deny` for license and source policy. Add `concurrency:` and
  `timeout-minutes` to `ci.yml`.
- [ ] **`CI-10` The MSRV job only runs `cargo check`**, while the Docker image
  is built with 1.88; run the tests there too.

## Deployment

- [ ] **`DEP-01` Dependency hygiene.** `RUSTSEC-2023-0071` (`rsa`, lockfile-only
  via `sqlx-mysql`) is correctly ignored in CI and should be re-checked when
  sqlx releases a fix. `rustls-pemfile` is unmaintained (`RUSTSEC-2025-0134`,
  via `axum-server` 0.7). `chacha20 0.10.1` in `Cargo.lock` is yanked (via
  `rand 0.10`): `cargo update -p chacha20`.
- [ ] **`DEP-02` Docker image.** Base images are pinned by tag, not digest; only
  `linux/amd64` is built although aarch64 binaries ship; `curl` is in the
  runtime image only for the health check, and the health check reads
  `YANUGET_PORT` only, so a port set in the TOML file reports unhealthy.
  Consider a `yanuget healthcheck` subcommand, a distroless base, digest pins
  and a multi-arch build. `debian:bookworm-slim` is in LTS; plan the move to
  trixie.
- [ ] **`DEP-03` No custom CA for outbound TLS.** reqwest is built with
  `rustls-tls` (bundled webpki roots), so the system store is never consulted:
  upstreams behind an internal CA or a TLS-inspecting proxy fail, and
  `docs/deployment.md:17-18` is wrong to say system CA certificates are needed.
  Enable native roots or add a `ca_cert_path` option for mirror and migrate.
- [ ] **`DEP-04` Secret files are not ignored.** `yanuget.toml`, `secrets.env`,
  `*.pem` and `*.key`, all of which the docs tell people to create, are
  missing from `.gitignore` and `.dockerignore`.
- [ ] **`DEP-05` Script hygiene.** `scripts/verify-with-dotnet.sh:45-46` pipes
  an unverified `dotnet-install.sh` into bash. `scripts/Send-YanugetFile.ps1`
  takes the key as a plain string, does not require https, and sends it to
  whatever `Location` (or `-Resume` URL) it is given; check the host against
  `-Feed` and accept the key from the environment.

## Documentation

- [ ] **`DOC-01` The trusted-proxy default is described backwards.**
  `docs/deployment.md:38-39` and `docs/configuration.md:348-351` still say the
  default trusts private ranges; it has been empty since 0.5.0.
- [ ] **`DOC-02` The Compose example trusts every LAN client.**
  `docs/deployment.md:150` sets `YANUGET_TRUSTED_PROXIES: "private"` with no
  proxy in front, which lets any client on the network spoof forwarding
  headers.
- [ ] **`DOC-03` The nginx snippet does not set the client address.**
  `docs/configuration.md:334-346`. Add `proxy_set_header X-Real-IP
  $remote_addr;` and `proxy_set_header X-Forwarded-For $remote_addr;` (and see
  `SEC-05`).
- [ ] **`DOC-04` Contradictory `base_url` advice.** `yanuget.example.toml:10-12`
  says to leave it unset behind a proxy; SECURITY.md and deployment.md say to
  set it; `docs/api.md` does not mention the trusted-proxy requirement.
- [ ] **`DOC-05` Backups.** `docs/deployment.md:48-92`: "can never miss a file
  the snapshot references" is false if a delete, overwrite or retention run
  lands between the SQLite snapshot and the file copy; "`tls/` need not be
  backed up" breaks every client that trusted the old self-signed certificate;
  the layout omits `.blobs/`, `.migrate/` and the inbox.
- [ ] **`DOC-06` Stale or broken passages.**
  - `docs/large-packages.md:110` says there is no upload timeout
    (`upload_idle_timeout_secs` defaults to 300).
  - The feeds table in `docs/configuration.md:229-252` is interrupted by a list,
    so its last rows render as literal pipes.
  - The README says the dotnet CI job covers the multi-feed features (it uses
    the root feed only).
  - CONTRIBUTING suggests `-k` for clients that have no such flag.
  - `RestrictAddressFamilies` in the systemd unit omits `AF_UNIX`.
- [ ] **`DOC-07` The README roadmap is incomplete.** The implemented list omits
  attached files, tus, the SSH inbox, pins, retention preview and copy/move.
  "Backends can be added without touching the core" is optimistic:
  `PackageContent` has only `LocalPath`, and the web layer, `main` and the inbox
  rely on local paths and `rename`.
- [ ] **`DOC-08` Document the global id+version namespace** (`SEC-15`),
  "disable, don't delete" on mirror feeds (`SEC-16`), and that single-feed mode
  cannot gate reads (`config.rs:799` sets `read_api_key: None`).

## Tests

Security claims made in the README and SECURITY.md that have no test yet:

- [ ] **`TEST-01` Per-feed key isolation.** No test sets `feed.api_key` or
  `feed.api_keys`: feed A's push key rejected on feed B, a feed with its own key
  rejecting the global one, and feed A's admin key refused on `/B/admin`.
- [ ] **`TEST-02` `read_api_key` coverage.** It is tested on the flat container,
  symbols and `/settings` only. Missing: `.nupkg` download, registration,
  search, autocomplete, gallery pages, `/files`, icons. The HTTP Basic form,
  which is what `dotnet` sends, is never tested.
- [ ] **`TEST-03` Mirror identity pinning and the mirror happy path.** Nothing
  tests that a manifest not matching the requested id or version is refused
  (`src/indexing.rs:114-121`), and there is no read-through integration test
  apart from the give-up case, so approval and license policy on mirrored
  versions are untested too.
- [ ] **`TEST-04` Mirror SSRF guard.** Unit tests cover literal IPs only; add a
  redirect to a private address and a DNS name that resolves to one.
- [ ] **`TEST-05` Forwarding chains.** No test covers an appended
  `X-Forwarded-For` from a trusted proxy (`SEC-05`).
- [ ] **`TEST-06` TLS.** Every integration test disables TLS; HSTS and the
  `0600` key mode are untested in Rust.
- [ ] **`TEST-07` End-to-end edges.**
  - A 416 range.
  - A multi-range request.
  - A `.snupkg` with `../` entry names.
  - A nuspec with a DOCTYPE.
  - CORS with configured origins.
  - Generic 5xx bodies.
  - The inbox with a symlinked file or `.error` (`SEC-01`, `SEC-02`).
  - Retention with pending versions (`COR-01`).
  - A database created by 0.4 (`COR-02`).

## Maintainability

- [ ] **`MNT-01` Split the web layer.** `src/web/mod.rs` (2.5k lines) mixes
  routing, the protocol, the gallery, admin and hand-rolled form parsing;
  `src/web/ui.rs` (4.1k lines) builds HTML with `format!`, where every sink
  relies on someone remembering `escape_html`, `safe_href` or `enc_path`
  (`SEC-24` is the one that was missed). Split into `protocol.rs`, `gallery.rs`
  and `admin.rs`; consider an auto-escaping template engine (maud, askama);
  parse forms with `serde_urlencoded`.
- [ ] **`MNT-02` Enforce admin auth with a `route_layer`**, not as the first line
  of each handler, so a new handler cannot forget it. Deduplicate
  `push_package` and `push_symbol_package`.
- [ ] **`MNT-03` Split the database layer.** `src/database/sqlite.rs` is about
  1.5k lines plus tests behind a 45-method trait covering packages,
  memberships, files, uploads and symbols. Split by concern; add foreign keys
  (`feed_packages` → `packages`); drop the dead legacy
  `packages.listed/enabled/downloads` columns; move schema changes to numbered
  `sqlx::migrate!` files, since a changed `CREATE INDEX IF NOT EXISTS` silently
  keeps the old definition on existing databases.
- [ ] **`MNT-04` One `get_json` helper in `src/mirror.rs`** for the four
  copy-pasted fetch blocks, centralising `check_url`, credential scoping, the
  size cap and the timeout. Carry the parsed `NuGetVersion` in migrate's work
  items instead of re-parsing (today a parse failure silently drops the
  identity pin). Tidy migrate reporting: mixed id- and version-level failures,
  hash-mismatch refusals counted as skipped, a byte-rate bar that only moves per
  package, and a `remove_dir_all(.migrate)` that clobbers a concurrent run.
- [ ] **`MNT-05` `build.rs` re-runs every build** when `site/` does not exist
  (`rerun-if-changed` on a missing path). *(Unverified.)*

---

## Features

Not yet implemented (contributions welcome):

- Additional storage backends (S3, Azure Blob). This needs a streaming
  `PackageContent` variant first; see `DOC-07`.
- Additional database backends (PostgreSQL, MySQL).
- Online vulnerability scanning.
- Native (Windows) PDB indexing.
- Reproducible builds (pinned toolchain, `--remap-path-prefix`, embedded
  timestamps).
