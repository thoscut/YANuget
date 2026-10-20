# Roadmap

What is planned, and what a review found that needs fixing. The README's
[Roadmap](README.md#roadmap) section lists what is already implemented; this file
is the work list.

Items are tagged with a stable id (`SEC-…`, `COR-…`, …) so commits and pull
requests can reference them, and ticked off rather than deleted when they land.

## Open items

Everything still open across this file and the project's working threads, as
of 2026-10-20 (0.8.0 released). The entries further down hold the detail; this
list is the index.

**From the review of 2026-10-19**

- Correctness: `COR-27` to `COR-31`. Operations: `DEP-08`, `DEP-09`.
  Hardening: `SEC-28` to `SEC-30`. Performance and UI: `PERF-04` to
  `PERF-08`, `UI-01`.

**Server, after 0.8.0** ([Target releases](#target-releases))

- `INT-31` Attached files on read-gated feeds: the premise holds (Chocolatey's
  helpers never send source credentials); a security review of the token
  design comes next, before it is built. 0.8.0 shipped without it.

**Internalizer 0.1.0**

- `INT-13` to `INT-15`, `INT-19` and `INT-22`: golden images, capture
  helpers, the KVM runner, the prelude, and the regression corpus. `INT-13`
  waits on the licensing question below. Capture validation, downloader, file
  names, publish and the report (`INT-16` to `INT-18`, `INT-20`, `INT-21`)
  have landed.

**Dependencies and toolchain**

- [x] **`DEP-06` `axum-server` 0.8.** It no longer depends on `rustls-pemfile`
  (`RUSTSEC-2025-0134`, see `DEP-01`): 0.8.0 reads PEM through
  `rustls-pki-types` and needs Rust 1.82. Upgrade, then drop the comment on
  that advisory in `ci.yml`.
  *Done:* `rustls-pemfile` is gone from `Cargo.lock`.
- [x] **`DEP-07` MSRV policy.** The floor was 1.88 (`Cargo.toml`, the MSRV
  job, the Dockerfile builder stage, README, CONTRIBUTING). `uuid` 1.27 needs
  1.89, which is why Dependabot's #47 was closed unmerged; the next `uuid`
  bump fails the same way, because Dependabot does not ignore it. `sqlx` 0.9
  needs 1.94 and is ignored in `dependabot.yml` on purpose. Decide: raise the
  floor to 1.89 now (cheap; 1.89 is over a year old), or hold `uuid` in
  Dependabot too. `sqlx` 0.9 waits for a release that raises the floor to
  1.94.
  *Done:* raised to 1.89 everywhere the floor is stated, `uuid` is at 1.28.

**Documentation**

- [ ] **`DOC-09` cosign 3.** The 0.7.0 image signature is stored as a
  Sigstore bundle, which `cosign verify` finds from cosign 3 on; cosign 2.5
  reports "no signatures found". SECURITY.md and RELEASING.md should say which
  cosign version the command needs.

**Features under discussion**

- Markdown readmes on the package page; see [Features](#features).

**For the repository owner**

- The release settings under [For the repository owner](#for-the-repository-owner):
  0.7.0 ran attestation and cosign signing for real, but whether the `release`
  environment, the tag ruleset and crates.io trusted publishing exist has not
  been checked from here.
- An open question from the review of 2026-09-30: move its `SEC-…` findings
  from this public file into a private security advisory? Every one of them
  is fixed and released since 0.6.0.
- The internalizer's [open questions](#open-questions), above all the Windows
  license for the golden images (needed before `INT-13` ships) and the
  release scheme for the tool.

## Review of 2026-10-19

A multi-expert review of `main` after 0.7.0 (auth and web, package parsing,
database and protocol, operations, UI and performance). PR #63 fixes what
it found that fits one pull request; the entries below are what it leaves
open. File and line references are from `main` at `a193027`.

### Correctness and data integrity

- [x] **`COR-26` `pending-only` overwrite skips other feeds' license policy.**
  `src/database/sqlite/packages.rs:147-192`, `src/indexing.rs:286-330`. With
  `allow_overwrite = "pending-only"`, a push to feed A replaces the global
  build for feed B too, where the same version is pending; only A's policy is
  checked and only A's `flagged` is recomputed. *Fix:* in the same
  transaction, check the policy of every feed that holds the version, or
  re-flag it there. *Done:* both, in the swap's transaction: a feed whose
  policy blocks the new build refuses the push, the others re-flag it.
- [ ] **`COR-31` Attached files of an approved version can still change.**
  `src/web/hosted.rs` (`delete` → `detach`, then `attach_locked`) checks only
  that the version is in the push key's feed. After a promotion the source
  feed keeps its membership, so its push key can replace `setup.exe` of a
  version approved in production; files hang off the version, not the feed,
  and the package hash does not cover them. Under `pending-only` this breaks
  "approved anywhere stays immutable". *Fix:* refuse attach and detach while
  `approved_in` is not empty, except with the admin key, at least under
  `pending-only`.
- [ ] **`COR-27` Memberships of removed or renamed feeds stay behind.** Then
  `purge_global_data` never runs for those versions, an overwrite answers
  `409`, and the renamed feed looks empty. *Fix:* warn at startup and in
  `yanuget config check`, and add a command that migrates or removes them.
- [ ] **`COR-28` `max_age_days` counts from the global `published`**
  (`src/retention.rs:178`), not from `feed_packages.added`, so an old version
  promoted into prod is deleted there by the next sweep. Count from `added`,
  or document the behaviour.
- [ ] **`COR-29` A mirror takes the upstream's listing state only once**
  (`src/mirror.rs:1581, 1649, 1675`), on the first mirror. A version unlisted
  upstream later stays listed here. *Fix:* reconcile on refresh while no
  admin has set the state.
- [ ] **`COR-30` Backup verification compares sizes only** (`verify` in
  `src/backup.rs`). Under `allow_overwrite`, new bytes of the same size leave
  a copy whose hash no longer matches its row. *Fix:* compare `package_hash`
  for the files copied in the last pass.

### Operations and documentation

- [ ] **`DEP-08` `sudo -u yanuget yanuget …` does not see the unit's
  environment** (`contrib/systemd/yanuget.service`, `docs/backup.md`,
  `docs/deployment.md:153`). `data_dir` then falls back to `./data` in the
  current directory, and `restore` writes there and reports success. *Fix:*
  the install steps put an absolute `data_dir` into the TOML file, and
  `restore` prints its target and refuses a relative `data_dir`.
- [ ] **`DEP-09` No warning for unknown `YANUGET_*` variables** in the
  server, as the internalizer has (`check_env_names`). `YANUGET_ADMIN_KEY`
  instead of `YANUGET_ADMIN_API_KEY` silently turns `/admin` off.
- [ ] **`PERF-04` `/metrics` and `/health/status` without a token** run three
  aggregate scans per feed on every request. *Fix:* cache `gather()` for a
  few seconds.

### Hardening

- [ ] **`SEC-28` A wrong preview key answers `404`, not `401`**
  (`src/web/protocol.rs:122-138`), so it does not count toward
  `max_failed_auth`.
- [ ] **`SEC-29` Free-space checks do not reserve.** Without a running
  reservation, N concurrent uploads each pass the check alone, and the number
  of open tus sessions per feed is not capped. PR #63 re-checks
  `min_free_disk_bytes` while an upload streams; the reservation and the cap
  remain.
- [ ] **`SEC-30` `rewrite` and `[Content_Types].xml`.** New types are inserted
  at `rfind("</Types>")` (`crates/yanuget-pkg/src/rewrite.rs:421`), which a
  comment can contain, and control characters in part names are not refused.
  Matters once the internalizer rewrites packages (`INT-20`).

### Performance and UI

- [ ] **`PERF-05` No response compression** (tower-http's `compression-*`
  features are off): large registration and search JSON goes out
  uncompressed when no proxy compresses it.
- [ ] **`PERF-06` Mirror JSON is decompressed on the async thread and parsed
  into `serde_json::Value`** (`src/mirror.rs:510-570`; `:745` clones the
  items). *Fix:* `spawn_blocking` and typed structs.
- [ ] **`PERF-07` The bundled docs have no `Cache-Control` or `ETag`**, and
  each request copies the file (`into_owned()`, `src/web/docs.rs:57-75`).
- [ ] **`PERF-08` Gallery header queries.** The header needs the headline
  and a `COUNT(*)` per id, not more. (The cap on `take` is in PR #63.)
- [ ] **`UI-01` The admin dashboard is a flat list** without paging and
  without marking pending or flagged versions (`src/web/admin.rs:103-110`).
  *Fix:* badges with counts, pending first, filters.

---

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

### Status

Every finding below has been fixed on the review branch, with tests, and is
described in the *Unreleased* section of the [changelog](CHANGELOG.md). That
includes the release blockers (`SEC-01`–`SEC-03`, `COR-06`, `COR-07`) and the
data migration for pre-0.5.0 databases (`COR-02`). The file and line
references below are from the reviewed commit; many files have since been
split or moved (`src/web/`, `src/database/`).

After the fixes: 522 tests (from 321), `clippy -D warnings` clean,
`mkdocs build --strict` clean, and `cargo audit` / `cargo deny` in CI.

#### Left open on purpose

What a fix deliberately did not do, and why:

- `SEC-03`: before reusing an existing blob, the store compares its size, not a
  full re-hash. Re-hashing would read gigabytes on every attach, and the
  inbox now copies through a handle only the server holds, so a blob cannot
  change after it was verified.
- `SEC-07`: credentials go only to the configured upstream's own origin; there
  is no allow-list for further credentialed hosts. Private feeds serve every
  resource from one origin; a CDN redirect is followed without credentials.
- `SEC-20`: logs redact userinfo and query strings of upstream URLs, not a
  token embedded in the *path* (MyGet-style `/auth/<token>/`). `/settings`
  shows only scheme, host and port.
- `SEC-23`: request bodies are bounded by the per-chunk idle timeout, the
  header-read timeout and the connection cap, not by total duration: a slow
  multi-gigabyte upload is legitimate.
- `COR-12`: while overwrite is off, downloads are `immutable`, so a
  hard-deleted version can stay in a shared cache until it expires.
- `COR-17`: admin bulk *delete* and *copy/move* are checked up front and then
  applied one version at a time, because they move files and take a lock per
  version; bulk flag changes are one transaction.
- `CI-04`: `cargo auditable` is not used.
- `MNT-03`: schema migration 7 logs, but does not delete, the blobs and PDBs
  of rows it drops as orphans; deleting stored bytes belongs in a storage
  sweep, not a schema step. The migrations stay on `user_version` rather than
  `sqlx::migrate!` (the reasons are at the top of
  `src/database/sqlite/schema.rs`).
- `MNT-01`: no template engine and no `serde_urlencoded`. Both would change
  behaviour, and the inline style and script must stay byte-identical to their
  CSP hashes. The escaping rule is documented in `src/web/ui/escape.rs`.

#### For the repository owner

These need settings no commit can change; the release workflow expects them
(see [RELEASING.md](RELEASING.md)):

1. Create the `release` environment (Settings → Environments), restricted to
   the tag pattern `v*` and no branches; required reviewers are optional.
2. Add a tag ruleset protecting `v*`, so only maintainers can create, move or
   delete release tags.
3. On crates.io, set up trusted publishing for `yanuget` (owner `thoscut`,
   repository `yanuget`, workflow `release.yml`, environment `release`), then
   delete the `CARGO_REGISTRY_TOKEN` secret and revoke the token. A crate that
   was never published needs its first publish by hand.
4. Keep `PUBLISH_TO_CRATES_IO` unset until crates.io publishing should run.
5. Give this repository *Write* access to the `thoscut/yanuget` package on
   ghcr (package settings → Manage Actions access).

Exercised by the 0.7.0 release: build provenance for the archives and the
image, and the image's cosign signature (`cosign verify` from SECURITY.md
passes with cosign 3; see `DOC-09`). Not yet exercised: crates.io trusted
publishing (`PUBLISH_TO_CRATES_IO` is unset), and
`scripts/Send-YanugetFile.ps1`, which was reviewed but not run.

---

## Security

### High

- [x] **`SEC-01` The inbox writes its `.error` report through symlinks.**
  *Unreleased.* `src/inbox.rs:126-140`. `try_exists` follows links and the
  report is written with `tokio::fs::write`, so a `{name}.error` symlink placed
  by the upload account makes the server create or truncate a file of its
  choosing, as the server's user. Absolute link targets resolve in the server's
  namespace, not the SFTP chroot, which defeats the documented "can reach
  nothing but the inbox" boundary.
  *Fix:* keep failure reports out of the uploader-writable tree (a DB row shown
  in `/admin`), or open with `O_NOFOLLOW | O_CREAT | O_EXCL` after removing any
  existing non-regular entry.

- [x] **`SEC-02` The inbox stages, hashes and attaches whatever a path points
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

- [x] **`SEC-03` An inbox file can change after it was verified.**
  *Unreleased, (unverified).* Same code. `rename` keeps the inode, so an upload
  account that still holds an open SFTP handle can write to it after hashing.
  The content-addressed blob then no longer matches its name, and every later
  attach of that hash deduplicates onto it. Fixed by the copy in `SEC-02`;
  `store_blob` could also re-verify before deduplicating onto an existing blob.

### Medium

- [x] **`SEC-04` Read-gated content is served as `Cache-Control: public,
  immutable`.** `src/web/mod.rs:54`, `src/web/files.rs:179`. Downloads,
  nuspecs, icons, symbols and attached files of a feed with `read_api_key` are
  marked `public`, and `Vary` names neither `Authorization` nor
  `X-NuGet-ApiKey`, so a shared cache or CDN in front of the server may serve
  them to anyone. `immutable` is also wrong when overwrite is enabled (see
  `COR-12`). *Fix:* `private` when read auth is on, and `immutable` only when
  overwrite is `Disabled`.

- [x] **`SEC-05` Behind a trusted proxy, the rate limiter keys on the
  client-chosen end of `X-Forwarded-For`.** `src/ratelimit.rs:103-116`. Headers
  from untrusted peers are correctly stripped, but for a trusted peer the
  *leftmost* entry wins. nginx's usual `$proxy_add_x_forwarded_for` keeps what
  the client sent, so each request can pick its own bucket; the bucket map also
  grows with every one. *Fix:* hand the limiter the `TrustedProxies` set, walk
  the list right to left, skipping trusted hops, and unmap IPv4-mapped
  addresses. Also only accept `http`/`https` from `X-Forwarded-Proto`
  (`src/web/mod.rs`, `forwarded()`).

- [x] **`SEC-06` The mirror's private-address guard misses DNS names on
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

- [x] **`SEC-07` Upstream credentials go to every host the upstream's JSON
  names.** `src/mirror.rs:91, 305-310, 500-544`. Auth is set as default headers,
  so the `PackageBaseAddress`, `SearchQueryService` and catalog `@id`s taken
  from upstream JSON receive it wherever they point. Catalog page URLs are never
  passed through `check_url`. The same-host redirect rule compares host only, so
  an https→http or cross-port redirect still carries custom headers.
  *Fix:* attach credentials per request only when the origin matches the
  configured upstream (or an explicit allow-list); `check_url` every catalog
  page; compare scheme, host and port, and refuse downgrades.

- [x] **`SEC-08` Symbol ownership is per package id, not per version or feed,
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

- [x] **`SEC-09` The nuspec parser can read a different identity than NuGet
  does.** `src/nuspec.rs:193-204, 339-371`. The last `<id>`/`<version>` wins
  (NuGet's `NuspecReader` takes the first), any descendant of `<metadata>`
  counts, namespaces are ignored, a child element inside `<id>` resets the
  text, and `<dependency>` is collected anywhere. The same applies to a
  duplicate `<license>`, which the license policy evaluates. The feed can then
  index an id, dependencies or license that differ from what a client reads
  from the same bytes. *Fix:* accept only `package/metadata/<field>` as direct
  children, in the nuspec namespace; reject duplicate scalar elements and child
  elements inside scalar fields.

- [x] **`SEC-10` A crafted manifest costs CPU out of proportion to its size.**
  `src/nuspec.rs:124, 168, 244-266, 381-394`. quick-xml's duplicate-attribute
  check falls back to a linear scan, so an element with tens of thousands of
  attributes is quadratic per `attr()` pass (four passes per `<dependency>`),
  and `check_limits` re-sums every group on every element. Parsing runs on an
  async worker, so a few pushes stall the runtime. This is reachable by any
  pusher, and through mirrored upstreams. *(Timing unverified.)* *Fix:* run
  `parse_nuspec` in `spawn_blocking`, cap attributes per element and the nuspec
  size (1 MiB is ample), collect attributes in one pass, and keep running
  counters.

- [x] **`SEC-11` A `.snupkg` push holds up to 512 MiB of decompressed PDBs in
  memory and stores truncated ones silently.** `src/symbols.rs:22-29, 97-141`,
  `src/nupkg.rs:137-170`. Highly compressible input makes this a small upload,
  and a few concurrent pushes exhaust memory. `extract_entries` cuts entries at
  the cap and `symbols.rs` never notices, so corrupt PDBs are indexed.
  *Fix:* stream each entry to a temp file, parse the key from its first KiB,
  and reject oversize entries explicitly.

- [x] **`SEC-12` The license deny list is bypassable.** `src/policy.rs:138-179`.
  `GPL-2.0+` does not match a `GPL-2.0` rule (the SPDX `+` is not stripped);
  deprecated ids and `-only`/`-or-later` variants are not normalised; on an allow
  list, `MIT WITH <anything>` passes because the exception is never checked;
  `licenseUrl`-only and file licenses cannot be denied at all. *Fix:* normalise
  `+`, `-only`, `-or-later` and deprecated ids; accept only known (or
  configured) exception ids after `WITH`; document that a deny list is advisory
  unless an allow list is also set.

- [x] **`SEC-13` No `Host` validation, and permissive CORS permits writes.**
  `src/web/mod.rs:202-219, 386-388, 612-615`. Any `Host` is accepted even with
  `base_url` set, so DNS rebinding can reach an intranet-only feed from a
  browser (and push or delete, when no API key is set, which is the default).
  `cors_allowed_origins = ["*"]` uses `CorsLayer::permissive()`, which allows
  every method and header; the relist `POST` is a CORS-simple request even
  without it. *Fix:* reject a `Host` that does not match `base_url` (or a new
  `allowed_hosts`) with 421; limit permissive CORS to GET/HEAD/OPTIONS; refuse
  writes carrying `Sec-Fetch-Site: cross-site`.

- [x] **`SEC-14` Invalid environment overrides fail open, silently.**
  `src/config.rs:589-760`. `YANUGET_HOST=localhost` is ignored and the server
  binds `0.0.0.0`; `YANUGET_TLS_ENABLED=enabled` (any value outside the truthy
  list) turns TLS **off**; `YANUGET_MAX_PACKAGE_SIZE_BYTES=10G` means
  *unlimited*; a typo in `YANUGET_RATELIMIT_ENABLED` disables the limiter. The
  TOML file is strict (`deny_unknown_fields`), and the docs promise that "a
  mistyped security setting fails loudly". Relatedly, a `tls_cert_path` without
  `tls_key_path` (or the reverse) silently falls back to self-signed.
  *Fix:* refuse to start on any environment value that does not parse, and on a
  half-configured certificate pair.

- [x] **`SEC-15` An id+version is one namespace across every feed.**
  `src/indexing.rs:240-254`, `src/mirror.rs:711`. Different bytes under a known
  id+version are correctly refused, which makes whoever stores it first own it
  everywhere: a push key on a low-trust feed, or an anonymous read of a mirror
  feed, can pre-claim a version that another feed then cannot receive (409 on
  push; the other mirror's fetch fails, logged as a benign race). Global
  metadata (readme, icon, attached files) is shared the same way, so detaching a
  file in one feed removes it from all. *Fix:* at least document it and log a
  hash mismatch as a failure; better, an id-prefix reservation per feed, or key
  global rows by content hash.

- [x] **`SEC-16` Deleting a mirrored version is undone by the next anonymous
  read.** `src/mirror.rs:664`, `src/retention.rs:207`. Admin delete, hard
  `DELETE` and retention all remove the membership row, which is what the mirror
  checks, so a version removed as malicious comes straight back on a feed
  without `requires_approval`; retention and the mirror also fight over old
  versions. *Fix:* a per-feed tombstone written on delete and prune in mirror
  feeds; until then, document "disable, don't delete".

- [x] **`SEC-17` Mirror downloads bypass `min_free_disk_bytes`.**
  `src/mirror.rs:376`, `src/web/mod.rs:273-296`. Pushes check free space;
  read-through fetches, triggered anonymously, do not, and can fill the volume
  the database lives on. *Fix:* run the same check against `Content-Length`
  before each download; optionally a global mirror byte budget.

### Low

- [x] **`SEC-18` Key handling.** `src/auth.rs:152-195`. The CSRF token is a
  static `SHA-256(prefix ‖ admin_key)`, not session-bound and never rotated;
  admin pages that embed it lack `Cache-Control: no-store`. `constant_time_eq`
  returns early on a length mismatch. Push keys are trimmed, read and admin keys
  are not. *Fix:* HMAC with a random per-process secret and a timestamp,
  `no-store` on `/admin*`, compare fixed-length digests (`subtle`), and trim
  consistently.
- [x] **`SEC-19` The rate limiter is weak against key guessing.**
  `src/ratelimit.rs:67-100`. Every IPv6 /128 gets its own bucket; `window_secs =
  0` disables limiting; there is no separate throttle for failed
  authentication, and no warning for short keys. *Fix:* bucket IPv6 by /64, add
  a strict limiter for 401s, warn on keys under 32 characters, and reject a zero
  window.
- [x] **`SEC-20` Secrets can reach logs and pages.** Upstream URLs are printed
  with their userinfo (`src/migrate.rs:152`, `src/mirror.rs:286, 481, 490`), and
  reqwest errors embed the URL (logged at `src/web/mod.rs:292`); `/settings`
  redacts only `user:pass@`, not tokens in the path or query
  (`src/web/ui.rs:1992-2005`), and it is readable by anyone on a feed without a
  read key. `Config`, the auth structs and `MigrateArgs` derive `Debug` with raw
  secrets. *Fix:* keep a redacted display form, use `e.without_url()`, show only
  scheme and host on `/settings`, and give secret-bearing types a redacting
  `Debug`.
- [x] **`SEC-21` Migration credentials only on the command line.**
  `src/main.rs:46-57`. `--source-password`, `--source-token` and
  `--source-header` show up in `ps`, `/proc/*/cmdline` and shell history; the
  docs' `"$TOKEN"` still expands into argv. *Fix:* `env =` with
  `hide_env_values`, or `--source-token-file`, and update `docs/migrate.md`.
- [x] **`SEC-22` Unicode case folding differs between layers.** The database
  uses `to_lowercase()`, storage paths and the version lock use
  `to_ascii_lowercase()`, and ids taken from URLs are never validated. A
  `DELETE` with a Kelvin sign `K` matches the database row but neither the lock
  nor the directory: the rows go, the payload is orphaned, and a concurrent
  push is not serialised. Needs the push key. *Fix:* `validate_package_id` on
  every URL id and one canonicalisation function.
- [x] **`SEC-23` Graceful shutdown and slow clients.** `src/main.rs:169-220`.
  The plain-HTTP path has no shutdown deadline (the TLS one has 10 s); the
  retention sweep is dropped at whatever await point it reached; neither server
  sets a header-read timeout or a connection cap, and the upload idle timeout
  is per chunk, so slow clients can hold sockets. *(Partly unverified.)*
- [x] **`SEC-24` `index_page` writes the raw `Host`-derived URL into HTML.**
  `src/web/mod.rs:831-844` (only with `enable_web_ui = false`). Not exploitable
  cross-origin (browsers cannot set `Host`, and the CSP blocks inline script),
  but it is the one sink that skips `escape_html`.
- [x] **`SEC-25` `security_headers` replaces `Vary`.** `src/web/mod.rs:575-578`.
  `insert` drops the `Vary: origin, access-control-request-*` that the CORS
  layer adds, so a shared cache can replay one origin's
  `Access-Control-Allow-Origin` to another. *Fix:* append.
- [x] **`SEC-26` The ZIP duplicate-entry check can be sidestepped.**
  `src/nupkg.rs:230-271`. It reads the EOCD's total-entries field while the zip
  crate uses entries-on-this-disk, skips ZIP64, and takes the last EOCD
  signature where the crate skips invalid candidates. The impact is small (the
  NuGet client itself refuses several nuspecs). *Fix:* walk the central
  directory and count records.
- [x] **`SEC-27` The multi-feed root lists every feed name without auth**,
  read-gated ones included (`src/web/mod.rs:308-319`). Probably fine; worth a
  decision and a sentence in the docs.

---

## Correctness and data integrity

### High

- [x] **`COR-01` Retention ranks and protects versions that clients cannot
  see.** `src/retention.rs:95-121, 355-372`. `prune_plan` picks the "always
  keep" newest version and the "newest N" among every membership, including
  pending, disabled and unlisted ones. On a gated feed with `prune_on_push`,
  pushing N pending builds deletes every approved version, and rejecting the
  pending ones then leaves nothing. Disabling a broken newest version has the
  same effect by accident. *Fix:* rank and protect only servable memberships;
  treat pending and disabled versions like pins (outside the rules).

- [x] **`COR-02` Pre-release keys stored before 0.5.0 were never migrated.**
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

- [x] **`COR-03` A purge that fails part-way strands the version for good.**
  `src/retention.rs:207-240`, `src/database/sqlite.rs:512-539`. Membership goes
  first, then files, then four DELETEs without a transaction. A failure after
  the files are gone leaves a `packages` row with no payload and no membership;
  nothing retries it. A later push of the same bytes adopts the missing payload
  (every download 404s), and different bytes get 409 in every feed. *Fix:* one
  transaction for `delete_package_data`; in `index_inner` treat "data exists,
  no memberships" (or a missing payload) as an orphan to purge and replace; a
  periodic orphan sweep.

- [x] **`COR-04` An overwriting push is not atomic.** `src/indexing.rs:176-232,
  263-281`. The membership and package rows are deleted (errors discarded)
  before the new payload and sidecars are stored, so a failed store (disk full)
  loses the version from the database while bytes are orphaned or already
  replaced. *Fix:* store first, then swap the rows in one transaction, or
  restore `previous` on failure.

- [x] **`COR-05` An overwrite resets admin state and drops attached files.**
  Same code. The new membership is always listed and enabled with zero
  downloads (only `pinned` is carried over), so a push key can undo an admin's
  *disable* on an ungated feed. `delete_package_data` removes the
  `package_files` rows but not their blobs: the version loses its attachments
  and the blobs leak. *Fix:* carry `enabled`, `listed` and `downloads` over, and
  keep (or explicitly purge) attached files.

- [x] **`COR-06` Blob garbage collection races attaches to other versions.**
  *Unreleased.* `src/web/hosted.rs:227-278`, `src/retention.rs:247-264`. Blobs
  are shared by content, but locks are per version: an attach can find the blob
  present and drop its copy while a detach or purge of another version counts
  zero references and deletes it, leaving a row that points at nothing behind an
  immutable cache header. *Fix:* a lock keyed by `blob/{sha256}` held across
  store+insert and count+delete, or a transactional reference count.

- [x] **`COR-07` tus upload state is read outside its lock.**
  *Unreleased, (unverified).* `src/web/hosted.rs:648-716`. `session()` reads
  `received` before `slot.try_lock()`, and `forget_slot` runs before the attach
  finishes. A client retrying a PATCH, which tus clients do, can write through a
  stale offset into the inode being renamed into the blob store. *Fix:* take
  the slot first, re-read the session under it, keep it until the attach
  completes, and rename the part file to a unique staging name first.

- [x] **`COR-08` The read-through mirror never refreshes and ignores the
  requested version.** `src/web/mod.rs:1055-1061, 1202-1205`,
  `src/mirror.rs:648-652`. The flat index and registration call the mirror only
  when the feed holds no version of the id; after the first partial fill, new
  upstream releases never appear, contrary to `docs/configuration.md:242-245`.
  `ensure_package` keeps the newest N and never prioritises the version a
  client asked for, so pinning an older one 404s forever. *Fix:* fetch the
  requested version first, outside the cap; re-list after a per-(feed, id) TTL.

- [x] **`COR-09` Mirror fetch limits and failure handling.**
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

- [x] **`COR-10` A credentialed upstream that redirects downloads fails
  confusingly.** `src/mirror.rs:121-126, 332, 360-365`. `attempt.stop()` returns
  the 3xx itself, which `error_for_status` does not reject, so the redirect body
  is stored and fails as an invalid archive. That breaks any source serving
  packages from a blob store or CDN *(which vendors do this is unverified)*.
  *Fix:* follow cross-host redirects without credentials (per-request auth from
  `SEC-07`), or report the refusal explicitly.

- [x] **`COR-11` `migrate` gaps.** `src/mirror.rs:408-544`, `src/main.rs:372-377`,
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

- [x] **`COR-12` Overwrite modes clash with caching.**
  `src/web/mod.rs:1094-1170`, `src/web/files.rs:62-78`. With overwrite
  allowed, a year of `immutable` keeps stale bytes in clients and proxies, and
  the nuspec and icon have no ETag. The ETag comes from `db.find` before the
  file is opened, so an overwrite in between pairs new bytes with the old tag
  and lets `If-Range` splice old and new. Hard-deleted packages also stay live in
  caches. *(Unverified.)*

### Low

- [x] **`COR-13` Version identity edge cases.** `src/version.rs`.
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
- [x] **`COR-14` Protocol deviations.**
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
- [x] **`COR-15` Downloads are counted on a `304`**, and the count is awaited
  before the file is served, so every restore waits on a SQLite write (up to the
  30 s busy timeout) despite the "never block" comment
  (`src/web/mod.rs:1141-1148`). *Fix:* count after the precondition check, off
  the request path.
- [x] **`COR-16` Copy and promote re-enable what the source withheld**
  (`src/web/mod.rs:2079-2089`): the target membership is listed and enabled even
  if the source version was disabled or unlisted.
- [x] **`COR-17` Multi-step writes without transactions.** Admin bulk actions
  (`src/web/mod.rs:2160-2235`) validate up front but can half-apply on a
  database error. Package and tag inserts are separate statements
  (`src/database/sqlite.rs:418-489`); a failed tag insert is never retried.
  Pins set during a long cleanup are not re-checked under the lock
  (`src/retention.rs:197-212, 502-513`).
- [x] **`COR-18` Non-atomic writes into the store.**
  `src/storage/filesystem.rs:206-338`. `move_into_place` falls back to copying
  over the live file on *any* rename error, not just `EXDEV`, without fsync, and
  leaves a partial file on failure (duplicated in `store_symbol_package`).
  `store_aux` and `store_symbol` use plain `fs::write`. Directories are not
  fsynced after rename. *Fix:* copy to a temp file, fsync, rename, fsync the
  directory; fall back only on `CrossesDevices`.
- [x] **`COR-19` Silent truncation of readmes and icons over 1 MiB**
  (`src/indexing.rs:130-141`, `src/nupkg.rs:196-200`), possibly mid-UTF-8, still
  flagged `has_readme`. Reject, or drop the flag.
- [x] **`COR-20` Symbol pushes take no version lock** (`src/symbols.rs:57-146`),
  so they race purge and overwrite, and a rejected push is partially applied
  (PDBs before the offending one stay).
- [x] **`COR-21` Cancellation leaves partial state.** Indexing and mirror fetches
  run inside the request future; a dropped connection between steps can leave a
  payload without a row, an overwrite with its rows already gone, or a leaked
  temp file. The startup sweep removes `*.tmp` but not `inbox-*.part`.
  *(Unverified.)* *Fix:* run post-upload indexing and mirror fetches in a
  spawned task and await its handle; RAII temp paths.
- [x] **`COR-22` Ids that are Windows device names** (`Aux.Core`, `Con.Utils`,
  `COM1.Sdk`) pass `validate_package_id` but are refused at store time with a
  late 400, on every OS (`src/storage/filesystem.rs:368-394`). Check early with
  a clear message, or escape them on disk.
- [x] **`COR-23` Two processes migrating the schema at once.**
  `src/database/sqlite.rs:261-296` uses a deferred transaction, so the loser can
  get an immediate `SQLITE_BUSY` (server plus `yanuget migrate` started
  together). *(Unverified.)* *Fix:* `BEGIN IMMEDIATE`.
- [x] **`COR-24` Durability.** `synchronous = NORMAL` combined with
  files-first deletes means a power loss can roll back a committed DELETE while
  the payload stays unlinked. *(Unverified.)* Consider `FULL`.
- [x] **`COR-25` Small things.**
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

- [x] **`PERF-01` Every search scans every version, twice.**
  `src/database/sqlite.rs:798-877`. The page query and the count query each run
  `lower()` and `LIKE '%q%'` across every version's description, tags and
  title; neither `q` nor `<description>` has a length cap. A few packages with
  multi-megabyte descriptions make every anonymous search expensive and can
  occupy the connection pool that downloads also need. `LIKE` on the raw JSON
  tags column matches `"` and `,`, and non-ASCII case folding differs between
  Rust and SQLite. *(Load impact unverified.)* *Fix:* cap `q` (256) and the
  description (4000, as nuget.org does), and index the latest version with FTS5.
- [x] **`PERF-02` N+1 queries in the admin area.** `retention::preview`
  (`src/retention.rs:464-499`) runs several queries per id and per planned
  version on every GET of `/admin/retention`, and again on run; `admin_package`
  queries files per version (`src/web/mod.rs:1968`).
- [x] **`PERF-03` `read_archive` opens every entry** (`src/nupkg.rs:57-71`):
  `by_index` seeks and reads each local header and builds a decompressor, and
  one encrypted or unsupported entry anywhere rejects the package. Use
  `name_for_index` and open only the nuspec; consider an entry-count cap, since
  the central directory costs about 5× its size in memory.

---

## Supply chain, CI and releases

- [x] **`CI-01` (high) Release jobs hold a write token while running untrusted
  build code.** `.github/workflows/release.yml:15-16`. `contents: write` applies
  to every job, including the matrix builds that run crate build scripts and an
  unpinned `pip install`; `actions/checkout` persists the token. *Fix:*
  `permissions: {}` at the top, `contents: write` only on the `release` job,
  `persist-credentials: false` on every checkout. Add
  `permissions: contents: read` to `ci.yml`, which has none.
- [x] **`CI-02` (high) No action is pinned to a commit SHA**, including those
  that run with write tokens (`softprops/action-gh-release`,
  `peter-evans/create-pull-request`, `docker/login-action`,
  `docker/build-push-action`) and mutable refs such as
  `taiki-e/install-action@cargo-audit` and `dtolnay/rust-toolchain@stable`.
  Pin to SHAs with version comments and let Dependabot update them.
- [x] **`CI-03` Release builds restore caches.** `release.yml:93-101, 197-198`.
  Tag builds read the `~/.cargo`/`target/` cache and the Docker `type=gha` cache
  that jobs on `main` can write (including the media workflow, which runs npm,
  pip and Playwright). Build releases from a clean state.
- [x] **`CI-04` Nothing is signed or attested.** Only an unsigned `SHA256SUMS`
  is published. Add `actions/attest-build-provenance` for archives and the
  image, sign the image with cosign keyless, consider `cargo auditable`, and
  document `gh attestation verify`.
- [x] **`CI-05` No release gate.** `workflow_dispatch` runs the workflow file
  from any branch; `CARGO_REGISTRY_TOKEN` is a repository secret. Put `image`,
  `release` and `crates-io` in a protected `release` environment restricted to
  tags, and move to crates.io trusted publishing. On a manual dispatch a missing
  tag may be created at the dispatching branch head rather than the built
  commit *(unverified)*.
- [x] **`CI-06` The media workflow runs third-party code with write access.**
  `media.yml:28-30`, `scripts/capture-media.sh:37, 42`. `contents` and
  `pull-requests: write` cover the whole job (`npm install`, an unpinned
  `pip install Pillow`, Playwright, mkdocs, cargo). Capture with read-only
  permissions and upload an artifact; push and open the PR from a separate job
  that runs no third-party code. Use `npm ci`, and pin Pillow.
- [x] **`CI-07` Expressions interpolated into shell.** `release.yml:39, 132, 230`
  put `github.event.inputs.tag`, `github.ref_name` and job outputs straight into
  `run:`. Pass them via `env:` and quote. (Needs write access to exploit.)
- [x] **`CI-08` Unpinned, unmonitored docs and media toolchains.**
  `requirements-docs.txt` is a floating range executed in the release and image
  builds; Dependabot has no `pip` or `npm` entries. Lock with hashes
  (`pip-compile --generate-hashes`, `--require-hashes`) and add both ecosystems.
- [x] **`CI-09` `cargo audit` runs only on push and PR.** Add a scheduled run;
  consider `cargo deny` for license and source policy. Add `concurrency:` and
  `timeout-minutes` to `ci.yml`.
- [x] **`CI-10` The MSRV job only runs `cargo check`**, while the Docker image
  is built with 1.88; run the tests there too.

## Deployment

- [x] **`DEP-01` Dependency hygiene.** `RUSTSEC-2023-0071` (`rsa`, lockfile-only
  via `sqlx-mysql`) is correctly ignored in CI and should be re-checked when
  sqlx releases a fix. `rustls-pemfile` is unmaintained (`RUSTSEC-2025-0134`,
  via `axum-server` 0.7). `chacha20 0.10.1` in `Cargo.lock` is yanked (via
  `rand 0.10`): `cargo update -p chacha20`.
- [x] **`DEP-02` Docker image.** Base images are pinned by tag, not digest; only
  `linux/amd64` is built although aarch64 binaries ship; `curl` is in the
  runtime image only for the health check, and the health check reads
  `YANUGET_PORT` only, so a port set in the TOML file reports unhealthy.
  Consider a `yanuget healthcheck` subcommand, a distroless base, digest pins
  and a multi-arch build. `debian:bookworm-slim` is in LTS; plan the move to
  trixie.
- [x] **`DEP-03` No custom CA for outbound TLS.** reqwest is built with
  `rustls-tls` (bundled webpki roots), so the system store is never consulted:
  upstreams behind an internal CA or a TLS-inspecting proxy fail, and
  `docs/deployment.md:17-18` is wrong to say system CA certificates are needed.
  Enable native roots or add a `ca_cert_path` option for mirror and migrate.
- [x] **`DEP-04` Secret files are not ignored.** `yanuget.toml`, `secrets.env`,
  `*.pem` and `*.key`, all of which the docs tell people to create, are
  missing from `.gitignore` and `.dockerignore`.
- [x] **`DEP-05` Script hygiene.** `scripts/verify-with-dotnet.sh:45-46` pipes
  an unverified `dotnet-install.sh` into bash. `scripts/Send-YanugetFile.ps1`
  takes the key as a plain string, does not require https, and sends it to
  whatever `Location` (or `-Resume` URL) it is given; check the host against
  `-Feed` and accept the key from the environment.

## Documentation

- [x] **`DOC-01` The trusted-proxy default is described backwards.**
  `docs/deployment.md:38-39` and `docs/configuration.md:348-351` still say the
  default trusts private ranges; it has been empty since 0.5.0.
- [x] **`DOC-02` The Compose example trusts every LAN client.**
  `docs/deployment.md:150` sets `YANUGET_TRUSTED_PROXIES: "private"` with no
  proxy in front, which lets any client on the network spoof forwarding
  headers.
- [x] **`DOC-03` The nginx snippet does not set the client address.**
  `docs/configuration.md:334-346`. Add `proxy_set_header X-Real-IP
  $remote_addr;` and `proxy_set_header X-Forwarded-For $remote_addr;` (and see
  `SEC-05`).
- [x] **`DOC-04` Contradictory `base_url` advice.** `yanuget.example.toml:10-12`
  says to leave it unset behind a proxy; SECURITY.md and deployment.md say to
  set it; `docs/api.md` does not mention the trusted-proxy requirement.
- [x] **`DOC-05` Backups.** `docs/deployment.md:48-92`: "can never miss a file
  the snapshot references" is false if a delete, overwrite or retention run
  lands between the SQLite snapshot and the file copy; "`tls/` need not be
  backed up" breaks every client that trusted the old self-signed certificate;
  the layout omits `.blobs/`, `.migrate/` and the inbox.
- [x] **`DOC-06` Stale or broken passages.**
  - `docs/large-packages.md:110` says there is no upload timeout
    (`upload_idle_timeout_secs` defaults to 300).
  - The feeds table in `docs/configuration.md:229-252` is interrupted by a list,
    so its last rows render as literal pipes.
  - The README says the dotnet CI job covers the multi-feed features (it uses
    the root feed only).
  - CONTRIBUTING suggests `-k` for clients that have no such flag.
  - `RestrictAddressFamilies` in the systemd unit omits `AF_UNIX`.
- [x] **`DOC-07` The README roadmap is incomplete.** The implemented list omits
  attached files, tus, the SSH inbox, pins, retention preview and copy/move.
  "Backends can be added without touching the core" is optimistic:
  `PackageContent` has only `LocalPath`, and the web layer, `main` and the inbox
  rely on local paths and `rename`.
- [x] **`DOC-08` Document the global id+version namespace** (`SEC-15`),
  "disable, don't delete" on mirror feeds (`SEC-16`), and that single-feed mode
  cannot gate reads (`config.rs:799` sets `read_api_key: None`).

## Tests

Security claims made in the README and SECURITY.md that have no test yet:

- [x] **`TEST-01` Per-feed key isolation.** No test sets `feed.api_key` or
  `feed.api_keys`: feed A's push key rejected on feed B, a feed with its own key
  rejecting the global one, and feed A's admin key refused on `/B/admin`.
- [x] **`TEST-02` `read_api_key` coverage.** It is tested on the flat container,
  symbols and `/settings` only. Missing: `.nupkg` download, registration,
  search, autocomplete, gallery pages, `/files`, icons. The HTTP Basic form,
  which is what `dotnet` sends, is never tested.
- [x] **`TEST-03` Mirror identity pinning and the mirror happy path.** Nothing
  tests that a manifest not matching the requested id or version is refused
  (`src/indexing.rs:114-121`), and there is no read-through integration test
  apart from the give-up case, so approval and license policy on mirrored
  versions are untested too.
- [x] **`TEST-04` Mirror SSRF guard.** Unit tests cover literal IPs only; add a
  redirect to a private address and a DNS name that resolves to one.
- [x] **`TEST-05` Forwarding chains.** No test covers an appended
  `X-Forwarded-For` from a trusted proxy (`SEC-05`).
- [x] **`TEST-06` TLS.** Every integration test disables TLS; HSTS and the
  `0600` key mode are untested in Rust.
- [x] **`TEST-07` End-to-end edges.**
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

- [x] **`MNT-01` Split the web layer.** `src/web/mod.rs` (2.5k lines) mixes
  routing, the protocol, the gallery, admin and hand-rolled form parsing;
  `src/web/ui.rs` (4.1k lines) builds HTML with `format!`, where every sink
  relies on someone remembering `escape_html`, `safe_href` or `enc_path`
  (`SEC-24` is the one that was missed). Split into `protocol.rs`, `gallery.rs`
  and `admin.rs`; consider an auto-escaping template engine (maud, askama);
  parse forms with `serde_urlencoded`.
- [x] **`MNT-02` Enforce admin auth with a `route_layer`**, not as the first line
  of each handler, so a new handler cannot forget it. Deduplicate
  `push_package` and `push_symbol_package`.
- [x] **`MNT-03` Split the database layer.** `src/database/sqlite.rs` is about
  1.5k lines plus tests behind a 45-method trait covering packages,
  memberships, files, uploads and symbols. Split by concern; add foreign keys
  (`feed_packages` → `packages`); drop the dead legacy
  `packages.listed/enabled/downloads` columns; move schema changes to numbered
  `sqlx::migrate!` files, since a changed `CREATE INDEX IF NOT EXISTS` silently
  keeps the old definition on existing databases.
- [x] **`MNT-04` One `get_json` helper in `src/mirror.rs`** for the four
  copy-pasted fetch blocks, centralising `check_url`, credential scoping, the
  size cap and the timeout. Carry the parsed `NuGetVersion` in migrate's work
  items instead of re-parsing (today a parse failure silently drops the
  identity pin). Tidy migrate reporting: mixed id- and version-level failures,
  hash-mismatch refusals counted as skipped, a byte-rate bar that only moves per
  package, and a `remove_dir_all(.migrate)` that clobbers a concurrent run.
- [x] **`MNT-05` `build.rs` re-runs every build** when `site/` does not exist
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
- A Chocolatey internalizer, planned in detail under
  [Chocolatey internalizer](#chocolatey-internalizer) (`INT-…`).
- Markdown readmes on the package page. NuGet readmes are Markdown, and
  nuget.org renders them; descriptions are plain text and stay so, in the list
  and on the package page. Sketch: `pulldown-cmark` with a filter over its
  events that emits raw HTML as text and sends every link and image URL
  through `safe_href`; external images as links with their alt text (the CSP
  blocks them anyway); a cap on the HTML produced. This would be the first
  place package data becomes markup, so it needs a documented exception to the
  escaping rule of `MNT-01`, a `SEC-…` entry of its own and an XSS corpus in
  `tests/http_security.rs`. Assessed on 2026-10-02 at one to two days; not
  started.

---

## Chocolatey internalizer

Partly built: the ticked items have landed. A tool that takes a package from the
Chocolatey Community Repository (or any v2 or v3 feed), fetches every file its
scripts download at install time, and publishes a copy into a YANuget feed that
installs without internet access. Drafted on 2026-10-15 against 0.7.0, whose
code every reference below was checked against, after reviewing the plan from
five angles: Chocolatey packaging, security, the YANuget architecture, Windows
operations, and testing. Items are tagged `INT-…`;
[Target releases](#target-releases) proposes where they land.

### Decisions

- **A separate binary, `yanuget-internalize`, in a workspace with the server.**
  The server never runs package content: [SECURITY.md](SECURITY.md) has it
  store and serve what it is given. The tool runs on a worker of its own (its
  own machine, or at least its own user) and reaches the server only through
  the push and attached-file APIs, with an ordinary push key.
- **Dynamic capture, not static analysis.** Each package runs in a disposable,
  offline Windows guest with Chocolatey's download and install helpers
  replaced by recorders. Rust has no usable PowerShell parser, and real scripts
  build their URLs from splatted hashtables, string interpolation and
  `$env:ChocolateyPackageVersion`. A static scan stays as a cross-check only
  (`INT-23`).
- **KVM first, Windows containers second.** The default sandbox is a Windows
  Server Core virtual machine under KVM/QEMU on a Linux host: no network
  device, a throwaway overlay on a golden image, and control through the QEMU
  guest agent over virtio-serial. It needs no Windows host, runs on the kind of
  machine YANuget already runs on, isolates as well as Hyper-V, and reduces the
  network of the validation install (`INT-26`) to nftables rules on a bridge.
  Windows containers under Hyper-V isolation are the backend for operators who
  have a Windows host (`INT-29`). Both sit behind the `Sandbox` trait
  (`INT-10`) and must produce the same capture.
- **The offline sandbox is the test.** A script that needs the network for
  anything but a recorded helper call fails there exactly as it would on an
  internal client, and is reported rather than guessed at.
- **Rewrite by prelude, not by editing the script.** The internalized
  `chocolateyInstall.ps1` is a generated prelude (a map from original to
  internal URL, and wrappers with the helpers' names) followed by the original
  script, byte for byte. How a script builds its URLs no longer matters, and
  the diff a reviewer reads is the prelude.
- **Installers become attached files**, stored once under their SHA-256 and
  served from `/files/{id}/{version}/{name}`. They are the upstream bytes, so
  the original checksum still holds. Embedding into the `.nupkg` is a later
  option for other feeds (`INT-28`).
- **Same id, same version.** Meta packages pin their `.install` package exactly
  (`[1.2.3]`), so a copy keeps the upstream id and version. Re-internalizing may
  replace a version that is still pending (`INT-04`); an approved version is
  never changed.
- **Nothing reaches clients unreviewed.** The tool pushes into an intake feed
  with `requires_approval = true` that `promotes_to` the production feed. Its
  report travels inside the package (`INT-21`, `INT-30`).

Not goals: parity with Chocolatey for Business, a PowerShell evaluator,
running package scripts under PowerShell on Linux (they take other branches
than under Windows PowerShell 5.1 and yield plausible but wrong URLs), a silent
fall-back to the original URL at install time, and any judgement on whether a
vendor's license allows redistribution inside an organisation. That stays with
the operator, and the docs say so.

### Server: 0.8.0

Every server change the tool relies on, in one release (see
[Target releases](#target-releases)), each useful on its own.

- [x] **`INT-01` Extract a `yanuget-pkg` crate.** Move `version.rs`,
  `nuspec.rs`, `nupkg.rs` and the models they use into a workspace crate with
  no axum or sqlx dependency and its own `thiserror` error type, converted into
  `crate::error::Error` by the server. Today `nupkg.rs` and `nuspec.rs` return
  the server's error, which carries `IntoResponse`. `pub use` re-exports keep
  the server's call sites unchanged. Add a `VersionRange` (parse, normalize,
  `contains`, best match), which does not exist yet: `Dependency::version_range`
  is a plain string. Behind a feature, since it brings reqwest: the upstream
  address policy (`proxy::is_private_ip_addr` and the filtering resolver in
  `src/mirror.rs`), for `INT-17`. It also holds the report schema of
  `INT-21`, so the tool that writes `internalizer/report.json` and the server
  that reads it (`INT-30`) share one definition.
  *Done when:* the server builds against the crate with no change in
  behaviour, `cargo deny` and `cargo audit` cover the workspace, and
  `VersionRange` has property tests over NuGet's interval notation.
  *Landed:* the crate is `crates/yanuget-pkg` (address policy in `address.rs`,
  the resolver behind the `net` feature). The report schema (version 1) landed
  there with `INT-30`, as `report.rs`.
- [x] **`INT-02` A streaming `.nupkg` rewriter** in `yanuget-pkg`. Copy
  unchanged entries raw, without recompressing; replace or add named entries;
  rewrite the `.nuspec`; keep the OPC parts (`[Content_Types].xml`,
  `_rels/.rels`, the `.psmdcp` core properties) consistent, with a content type
  for any new extension. Drop `.signature.p7s`, which any change invalidates.
  Peak memory must not depend on package size.
  *Done when:* an unchanged rewrite keeps every entry byte-identical, Chocolatey
  CLI installs a rewritten package, and a 5 GB entry passes in constant memory.
  *Landed:* `yanuget_pkg::rewrite`. An empty rewrite keeps every entry's
  compressed bytes and CRC; a 5 GiB entry passes with under 64 MiB of growth
  (an ignored test, `cargo test -p yanuget-pkg -- --ignored`), and Python's
  `zipfile` reads it back. Every copy is checked against the source after
  writing: `zip` 2.x left the ZIP64 sizes of a raw copy at zero, which the
  rewriter patched until `zip` 9 (taken for RUSTSEC-2026-0367/0368/0369)
  wrote them itself. Not yet checked: an install with Chocolatey CLI, which
  needs a Windows host; that belongs with the regression corpus (`INT-22`).
- [x] **`INT-03` Per-feed `allowed_extensions` for attached files.** `[files]`
  is global, and its defaults rightly exclude `exe` and `msi`. Let a feed
  override the list (`[feeds.files]`), so executables are accepted by the
  internal Chocolatey feed only. Typical additions: `exe`, `msi`, `msix`,
  `msu`, `appx`, `vsix`, `ps1`.
  *Done when:* `yanuget config check`, `yanuget.example.toml` and
  `docs/configuration.md` cover it, and a feed without the override behaves as
  today.
- [x] **`INT-04` Replace a pending version.** With overwrite off, a second push
  of the same version is `409` even while the first is pending and has never
  been served. Add `allow_overwrite = "pending-only"`: a push may replace a
  version that is pending in every feed holding it, together with its attached
  files, under the version lock (`src/locks.rs`). A version approved anywhere
  stays immutable.
  *Done when:* `yanuget config check` accepts the new mode, the old files are
  gone after a replace, a version approved in any feed is refused, and the race
  with a concurrent approve is tested.
- [x] **`INT-05` Pin the intake flow with a test.** According to
  `src/web/hosted.rs`, a file can be attached to a pending version
  (`version_in_feed` checks membership only) and is withheld until approval
  (`download`). Cover it end to end: push to a gated feed, attach, download is
  refused, approve, download succeeds; then the same after `promote`.
- [x] **`INT-30` Show the report in the admin area.** The package page of a
  pending version, which since 0.7.0 says where the version is approved, reads
  `internalizer/report.json` with the seek-based `nupkg::extract_file` and
  shows the upstream source, each original URL with
  its file, the checksum status, the warnings and the prelude. It flags a
  version whose report lists files that are not attached, so an interrupted run
  is not approved by accident. The entry is size-capped and escaped like every
  other sink (`SEC-24`); a package without it renders as today. Tested
  against fixture reports, because the tool that writes real ones ships after
  0.8.0.
  *Landed:* schema version 1 is `yanuget_pkg::report` (`Report::from_slice`,
  1 MiB cap, unknown fields and enum values tolerated, another schema version
  refused by name). The page reads the ten newest pending versions, checks
  each listed file's presence, size and SHA-256, and asks before approving one
  that does not match; the selection's Approve does not check.
- [ ] **`INT-31` Attached files on read-gated feeds.** Chocolatey's helpers do
  not send source credentials to download URLs *(to verify)*, so a feed with a
  `read_api_key` cannot serve its files to `choco install`. Proposal: a token in
  the file URL, an HMAC over feed, id, version and name under a server secret,
  that the prelude carries. Whoever can read the package can read its files:
  the same permission, no wider. Rotation needs a list of secrets, because
  changing the only one breaks every internalized package. The secrets are set
  like every key since 0.7.0: in TOML, from the environment, or from a file
  through `_FILE`. On a read-gated feed the answer to an attach
  (`{name, size, sha256, url}`) carries the URL with its token, so the tool
  never computes one. Verify the premise first: if the helpers do send source
  credentials, the item shrinks to a test. Either way, a security review
  before it is built.
  *Premise checked* on 2026-10-19 against Chocolatey CLI's helpers
  (`chocolatey/choco` at `c8659d4`): `Get-WebFile` and `Get-WebHeaders` set
  only `CredentialCache::DefaultCredentials` (the Windows logon, for
  integrated authentication) and proxy credentials; the source's user name
  and password never reach PowerShell, and none of the variables the CLI sets
  for a script carries them. So a read-gated feed cannot serve its files to
  `choco install` today, and the item stands. The one alternative the helpers
  allow: `-Options @{Headers = @{…}}` adds any request header, so a prelude
  could send `Authorization` from a variable set on each client. That puts a
  read key on every client and needs per-client setup, where the token needs
  none; it stays a fallback.
- [x] **`INT-32` Read access to pending versions.** A per-feed
  `preview_api_key` that can download pending packages and their files and do
  nothing else: no approve, no push. For `INT-26`, and for anyone who wants to
  test a version before approving it. Set like every other key:
  `YANUGET_FEED_{NAME}_PREVIEW_API_KEY`, or the same with `_FILE`.

### Internalizer: first version

Goal: internalize the dependency closure of a package into a gated feed, for
packages whose downloads go through Chocolatey's helpers.

- [x] **`INT-10` Crate, command line, configuration.** `yanuget-internalize`
  with `plan` (capture and report, publish nothing) and `run` (capture,
  download, rewrite, publish). Configuration is layered like the server's
  (defaults, TOML, `YANUGET_INTERNALIZE_*`, `_FILE` for secrets) with
  `deny_unknown_fields`, and `config check` checks it the way
  `yanuget config check` does since 0.7.0 (every value, the target feed,
  `/dev/kvm`, the golden images), exiting `1` on an error. The sandbox sits
  behind a `Sandbox` trait with a KVM backend (`INT-15`) and a Windows
  container backend (`INT-29`), so planning, capture parsing, downloading,
  rewriting and publishing build and are tested on Linux CI against a fake
  sandbox. Exit codes tell "done", "needs attention" and "failed" apart for
  pipelines.
  *Landed:* `crates/yanuget-internalize`, `publish = false` until its first
  release. Exit codes: `0` done, `1` failed, `3` needs attention (`2` stays
  clap's usage error). A package is `id` or `id@version`. Every setting is
  also `YANUGET_INTERNALIZE_` plus its TOML path in capitals; the push key
  takes `_FILE`, and `config check` warns about a variable that is not a
  setting. The check asks the target feed for its service index, which must
  list `PackagePublish` (`--offline` skips it), and checks `/dev/kvm`, QEMU
  and the qcow2 images, or a Windows host and a digest-pinned image. Both
  backends check their host and refuse every capture until their runners
  land, so `plan` and `run` stop before they fetch anything.
- [x] **`INT-11` Upstream client.** The Chocolatey Community Repository over v2
  OData (`FindPackagesById()`, package download), any v3 feed through its
  service index, and the auth modes of `[feeds.mirror]`. Each download is
  verified against the feed's `PackageHash`. A local cache keyed by hash,
  backoff on `429`, and a low default concurrency: the community repository
  rate-limits heavy users. Like a mirror since 0.7.0, it logs once when an
  upstream stops answering and once when it answers again.
  *Landed:* `yanuget_internalize::upstream`. v2 pages through
  `FindPackagesById()` (checked against the community repository's real
  answers: paging, `PackageHash`, `Dependencies`); v3 reads the registration,
  or the flat container without one. A download must match the feed's hash
  and carry a manifest with the id and version asked for before it enters
  the cache (`cache_dir/packages`, keyed by that hash and re-hashed on use).
  Most v3 feeds state no hash: their packages are cached by identity and
  marked unverified, for the report to say so. The mirror's address policy
  and its hop-by-hop redirects apply, credentials go to the upstream's
  origin only, `429`/`503` are retried after `Retry-After` or a doubling
  backoff. `config check` probes the upstream too; `plan` resolves its
  packages before it stops.
- [x] **`INT-12` Dependency closure.** Resolve dependencies with
  `VersionRange` (`INT-01`), extension packages (`*.extension`) included, since
  scripts call their helpers. Pick the versions Chocolatey CLI v2 would install
  for the same command *(to verify against the client)*, and skip what the
  target already holds. `plan` shows the whole closure before anything is
  downloaded.
  *Landed:* `yanuget_internalize::closure`. Chocolatey CLI v2's source
  (`NugetService`, `NugetCommon`) runs NuGet's resolver with
  `DependencyBehavior.Highest` and `AnyFramework`, and lets pre-release
  dependencies in only for a pre-release root; the closure does the same,
  with backtracking when ranges clash. As NuGet's `ResolverComparer` orders
  candidates, an unlisted version is taken only when no listed one fits. A
  dependency on `chocolatey` is left to
  the CLI in the image. A v3 feed with only a flat container gives its
  dependencies through the manifests. `plan` marks what the target serves
  already, read from its flat container; a read-gated target cannot say, and
  pending versions are not listed, so `INT-20` still takes a `409` as "already
  internalized". Checked against the community repository: `git`,
  `vscode`, `googlechrome`, `vlc` and `dotnet-8.0-desktopruntime` (eleven
  packages through `vcredist140` and the `KB` updates) resolve.
- [ ] **`INT-13` Golden images.** `contrib/internalizer/` builds, unattended
  and without network access, two qcow2 images from a Windows Server Core ISO
  (Windows PowerShell 5.1, which package scripts target). The *stock* image has
  the virtio-win drivers, the QEMU guest agent and a pinned Chocolatey CLI
  installed from a local `.nupkg`, with no sources configured and the guest's
  background traffic turned off (Windows Update, time sync, telemetry, LLMNR,
  NetBIOS, WPAD). The *capture* image is an overlay on it, with the stock image
  as backing file, that adds the capture helpers of `INT-14`. Updates come
  from pinned offline packages, not Windows Update. Each image ships with a
  saved memory state of the booted, idle guest, so a run restores instead of
  booting. Inputs are pinned by hash (ISO, virtio-win, Chocolatey, updates);
  the report records the image hash and the Chocolatey version. The guests
  need a regular Windows license; evaluation media are for development and CI
  (`INT-34`).
  *Done when:* a rebuild from the same inputs gives the same results on the
  regression corpus, and a restored guest answers its first guest-agent
  command within seconds *(target to verify)*.
- [ ] **`INT-14` Capture helpers.** Replace, inside the `chocolateyInstaller`
  module so that calls from other helpers and from extensions are caught too,
  at least `Get-ChocolateyWebFile`, `Get-WebFile`, `Get-FtpFile`,
  `Install-ChocolateyPackage`, `Install-ChocolateyZipPackage`,
  `Install-ChocolateyVsixPackage`, `Install-ChocolateyPowershellCommand`,
  `Install-ChocolateyInstallPackage` and `Get-ChocolateyUnzip`. Each appends
  one JSON line (package, URL, 64-bit URL, checksums and their types, file
  type, target path, and the caller from `Get-PSCallStack`, so a call made from
  inside an extension module is told apart) and returns a plausible result: a
  placeholder file where the script expects one, exit code 0. The script then
  goes on to its next download. Installers never run.
- [ ] **`INT-15` KVM sandbox runner.** One guest per root package, restored
  from the capture image's saved state onto a fresh overlay, with no network
  device at all. The upstream closure arrives as a read-only ISO 9660 image,
  found in the guest by its volume label; nothing is shared from the host (no
  virtio-fs, no 9p). The runner sets the guest clock (`guest-set-time`, since
  the saved state is stale), starts a plain `choco install {id} --version {v}
  --source {iso} -y` through `guest-exec`, polls `guest-exec-status` under a
  wall-clock timeout, reads the capture back with the guest agent's file calls
  under a size cap, then powers off and deletes the overlay whatever happened.
  Chocolatey sets the environment, installs dependencies and extensions, and
  runs each script as a client would; captures are attributed through
  `$env:ChocolateyPackageName`. QEMU runs with `-sandbox on`, CPU and memory
  limits and a bounded number of guests in parallel. The runner refuses to
  start without `/dev/kvm`: software emulation is too slow to be useful. QEMU
  is driven over QMP and the guest-agent socket directly; libvirt only if that
  proves necessary. Two systemd units in `contrib/systemd/`, modelled on the
  `yanuget.service` of 0.7.0: `yanuget-internalize.service` holds the push key
  and the download cache and cannot open `/dev/kvm`; `yanuget-sandbox.service`
  runs the guests as a user of its own, with `DeviceAllow=/dev/kvm` and no key,
  and takes jobs over a Unix socket. It cannot keep `PrivateDevices=yes`
  (`DevicePolicy=closed` instead); whether QEMU under KVM runs with
  `MemoryDenyWriteExecute=yes` is *(to verify)*.
- [x] **`INT-16` Treat the capture as untrusted input.** Untrusted code wrote
  it: a versioned schema, strict types, caps on line length, entry count and
  total size, no control characters, `http`/`https` URLs only, and attribution
  to a package of the closure. Anything else marks the package
  `needs-override`; nothing is repaired.
  *Landed:* `yanuget_internalize::capture`, schema version 1, which the
  helpers of `INT-14` write: one JSON object a line with `schema`, `package`,
  `version`, `helper`, `url`, `url64`, `checksum`, `checksumType`,
  `checksum64`, `checksumType64`, `fileType`, `fileFullPath` and `caller`,
  unknown fields refused. Caps: 16 KiB a line, 1,000 calls, 16 MiB in all,
  4,096 characters a URL, 1,024 a path. Control characters and the
  bidirectional controls are refused in every string. A checksum must be hex
  of its type's length; one without a type is MD5, as Chocolatey takes it.
  The helper must be one of the nine that record. A rejection lists every
  reason, up to twenty. The caller's path tells the package's own scripts
  (`lib\{id}\`) from an extension's module (`extensions\{name}\`) for
  `INT-21`.
- [x] **`INT-17` Downloader.** Stream each captured URL to disk while hashing
  with SHA-256 and with the algorithm the script declares (MD5, SHA-1, SHA-256,
  SHA-512); a declared checksum that does not match fails the package. A call
  with no checksum, or only MD5 or SHA-1, is marked trust-on-first-use in the
  report; the prelude pins SHA-256 either way (`INT-19`). The mirror's address
  policy applies (`INT-01`): private, loopback and link-local targets are
  refused unless allow-listed, after every redirect too. The URLs come from
  untrusted scripts, and a package pointing at an internal host would
  otherwise get that host's answer published into the feed. Size cap, retries,
  resume.
  *Landed:* `yanuget_internalize::download`, configured in `[downloads]`
  (`concurrency`, `timeout_secs`, `max_file_bytes`, default 8 GiB,
  `resume_attempts`, `allow_private_hosts`, `proxy`). Files land in
  `cache_dir/files` under their SHA-256; a declared SHA-256 finds a file
  there without a download, after hashing it again. The upstream's HTTP client
  is shared, without credentials: the address policy on every hop, no
  https-to-http redirect, `429`/`503` waited out. An allow-listed host is
  resolved without the filter. A transfer that breaks off is resumed with
  `Range` and `If-Range` (a strong `ETag`, else `Last-Modified`) where the
  host offers byte ranges, and restarted otherwise. A failed download leaves
  nothing in the cache.
- [x] **`INT-18` File names.** Attached file names are 1–128 characters of
  `A-Z a-z 0-9 . _ -` with an allowed extension; upstream names carry spaces
  and `+`, or there is no name at all (`?product=…&os=win64`). Keep the
  original name where it is valid; otherwise derive it from
  `Content-Disposition`, the URL and the captured file type, check the
  extension against the content (`MZ`, the OLE header of `.msi`, `PK`), and
  break collisions with a hash prefix. A file the feed's `allowed_extensions`
  refuses is reported, not renamed to pass.
  *Landed:* `yanuget_internalize::naming`. The server's rule moved to
  `yanuget_pkg::files`, so the server's attach check and the tool's names are
  one definition. The URL's own name is kept when it is valid and its
  extension fits the content. Otherwise the name comes from
  `Content-Disposition` (`filename*` first), the URL after redirects, the URL
  as captured, and last the package (`git.install-2.51.0`). Disallowed runs
  become `_` and long stems are cut. The extension is the name's own if it
  fits the content, else the captured `fileType` if that fits, else the
  content's (`MZ`, OLE, `PK`, 7z, `MSCF`, gzip). The same file keeps the
  name it was given, and a different file on a taken name gets the first
  eight hex digits of its SHA-256 in front.
- [ ] **`INT-19` Prelude.** The wrappers are proxy commands
  (`[System.Management.Automation.ProxyCommand]::Create`) generated from the
  pinned Chocolatey version's helper metadata, so positional calls and
  parameter aliases bind exactly as they do against the real helpers. A
  wrapper looks the URL up and calls the original
  (`chocolateyInstaller\Install-ChocolateyPackage`) with the internal URL and
  the stored file's SHA-256 as checksum. A URL not in the map stops the install
  with a message naming the package, never a fall-back to the internet. The map
  is keyed by the SHA-256 of the original URL, so no text a package controls is
  ever emitted as PowerShell; the report carries the originals. The server's
  base URL sits in one variable at the top, overridable by an environment
  variable for a host move.
- [x] **`INT-20` Publish.** Rewrite the package (`INT-02`) with the new
  `chocolateyInstall.ps1` and the report, push it to the intake feed, attach
  the files, and check that the feed withholds the version; warn when it is
  served at once. Attach is idempotent (same content, `200`), so a re-run
  completes a version whose files are missing. A `409` on push means "already
  internalized" unless `--replace-pending` (`INT-04`). The tool takes a push key
  and nothing more. `--replace-pending` needs YANuget 0.8.0: when
  `/health/status` is reachable the tool checks the server version up front,
  otherwise its error for a refused replace names the version it needs.
  *Landed:* `yanuget_internalize::publish`. The package is rewritten with
  `internalizer/report.json` and the entries `INT-19` will supply, pushed to
  `api/v2/package`, and its files attached with `x-checksum-sha256`. Whether
  the feed serves the version comes from its flat container, which lists
  approved versions only. After a `409` the files are attached only while the
  version is withheld: an attach is not reviewed, so a version the feed
  serves, or one a read-gated feed cannot tell about, gets nothing. On a
  `pending-only` feed every push replaces a pending version, flag or not; the
  push key cannot see it to ask first. An attach is tried three times; a
  request may take `target.timeout_secs` plus a second per MiB. The feed's
  file types are `target.allowed_extensions`. Tested against the server
  itself: held back until approved, report inside, a re-run completing a
  pending version, a refused type and a changed file failing. `run` calls it
  once a sandbox backend captures.
- [x] **`INT-21` Report.** Per package: `internalized`, `unchanged` (no
  downloads: embedded or meta packages), `needs-override` (network use outside
  the helpers, a script that failed, an unmapped call, a download from inside
  an extension module) or `failed`, each with reasons. It lists the upstream
  source and hash, the image hash and Chocolatey version, the captured calls,
  the files with sizes and hashes, the checksum status and the warnings. As
  JSON for pipelines, as text for people, and inside the package as
  `internalizer/report.json`, covered by the package hash and carried along by
  promotion, copy, `yanuget migrate` and `yanuget backup`. The schema is
  versioned and defined in `yanuget-pkg` (`INT-01`); YANuget 0.8.0 reads
  version 1, so fixing it may add fields but not change the ones 0.8.0 reads,
  and a reader ignores fields it does not know.
  *Landed:* `yanuget_internalize::report` and `::assess`, schema version 1
  unchanged. A failure outranks an override, and every reason is listed,
  failures first. A failed sandbox fails every package of the capture; an
  install that failed (exit codes other than 0, 1641 and 3010), ran out of
  time or left a rejected capture marks every package of it
  `needs-override`; a helper called from an extension's module or from
  outside the package's scripts, or a file of a type the feed does not take,
  marks its own package. `url64` that repeats `url` is one download. Text for
  people, and `{"outcome", "packages"}` as JSON. Network use outside the
  helpers needs the sandbox's own record (`INT-15`).
- [ ] **`INT-22` Regression corpus and CI.** A pinned list of the most
  installed community packages (id, version, upstream hash), cached rather than
  committed, run through `plan` whenever the capture helpers or the prelude
  change. Each result is compared with the last run, and the share per status
  is tracked. Golden files for generated preludes; fuzzing for the capture
  parser and the rewriter. A nightly job runs the KVM backend for real on a
  KVM-capable Linux runner, with images built from evaluation media. Whether
  GitHub's hosted Linux runners expose `/dev/kvm` and can cache a Windows image
  of that size is *(to verify)*; otherwise a self-hosted runner. The container
  backend (`INT-29`) gets a job on a hosted Windows runner with process
  isolation, and the release checklist adds one Hyper-V run.

### Internalizer: production

- [x] **`INT-23` Static cross-check.** Scan every package script for URL-like
  literals, including `chocolateyUninstall.ps1` and
  `chocolateyBeforeModify.ps1`, which capture does not run, and warn about each
  one capture did not see. A regex is enough, since it only ever raises a
  warning.
  *Landed:* `yanuget_internalize::crosscheck`. Every `.ps1`, `.psm1` and
  `.psd1` entry up to 4 MiB (UTF-8, or UTF-16 by its byte-order mark) is
  scanned for `http`/`https` URLs outside `#` and `<# #>` comments; a larger
  one is named in a warning instead. A URL built from variables is seen when
  a captured URL starts with its literal part, and one whose host is a
  variable cannot be compared. At most twenty warnings a package, then a
  count. `run` calls it once a sandbox backend captures.
- [ ] **`INT-24` Environment variants.** Capture once per architecture
  (`$env:ChocolateyForceX86`), configured locale and set of package
  parameters, and map the union of URLs. A package that picks its installer by
  system language is internalized for the configured locales only, and the
  report says so. Locales are variants of the golden images, because changing
  the system locale needs a reboot, which would throw away the saved state;
  whether `Set-Culture` before the run is enough for most scripts is
  *(to verify)*.
- [x] **`INT-25` Overrides.** One TOML file per package id
  (`deny_unknown_fields`): extra URLs, URL rewrites, package parameters for
  capture, a replacement script, or `skip` with a reason. Every override used
  is in the report; one that no longer matches a capture is a warning, so stale
  overrides surface.
  *Landed:* `yanuget_internalize::overrides`, in `overrides_dir`, the file
  named for the id. `versions` narrows it to a range; `skip` stands alone and
  `plan` marks the package. `package_parameters` go to the capture job as
  `--params` for the root. The script (a path inside the directory, 1 MiB at
  most) replaces the package's `chocolateyInstall.ps1` under its own name
  before the capture, so capture and feed see the same script. Downloads may
  carry a SHA-256; a rewrite fetches a captured URL from elsewhere while the
  package keeps asking for the original. The report gained `overrides`, a
  list of lines (a field added to schema version 1, which 0.8.0 ignores).
  One bad file fails the set; `config check` loads it.
- [ ] **`INT-26` Validation install.** After publishing, restore a guest from
  the *stock* image, without the capture helpers, and install each new pending
  version with its real installers. The guest's only network device sits on an
  isolated bridge whose nftables rules accept the YANuget server's address and
  port and log and drop everything else; the server's name goes into the
  guest's `hosts` file, so no DNS is needed. A drop that an idle baseline run
  of the same image also shows is ignored; any other fails the package, as
  does a failing exit code from Chocolatey. This catches what capture cannot
  see: web installers that pass capture and fetch their payload when they run.
  Needs read access to pending versions (`INT-32`). On the container backend
  the same restriction needs its own design (`INT-29`).
- [x] **`INT-27` Keep feeds current.** A watch list of ids, optionally with
  ranges; `sync` finds new upstream versions, internalizes them into the intake
  feed and reports what waits for approval. Meant as a systemd timer on the
  worker (a scheduled task with the container backend); idempotent and safe to
  interrupt. Each run writes its counts per status and the age of the oldest
  pending version in the Prometheus text format of `[monitoring]`, as a file
  for node_exporter's textfile collector, so a stuck sync shows up next to the
  server's metrics.
  *Landed:* `yanuget_internalize::sync` and `[sync]` in the configuration.
  Each watch entry (`[[package]]`, an id and an optional `versions` range)
  follows its newest listed version, a pre-release only under a pre-release
  bound. A state file records each version as `pending`, `approved`,
  `needs-override` or `failed`; it is saved after every package and guarded
  by a lock file. A version is never pushed twice; a failure is tried again,
  an override need once any override changes. Approval is read from the
  target's flat container; a pending version stops counting once a newer one
  in its range is approved or an override skips it. The metrics file adds the
  time and outcome of the last sync. `sync --dry-run` changes nothing;
  `config check` loads the watch list and the state. Fixing an approved
  version stays a manual `run`. The pipeline is behind a trait, so `sync`
  records each version as failed until a sandbox backend captures.
- [ ] **`INT-28` Embedding for other feeds.** For targets that are not YANuget:
  store the files under `tools/` in the package, streamed (`INT-02`), with the
  prelude mapping to paths under the package folder. Whether the helpers take a
  local path or `file:` URL for `-Url` is *(to verify)*; if not, the wrappers
  call `Install-ChocolateyInstallPackage` or `Get-ChocolateyUnzip` with the
  local file. A package over the target's upload limit fails with a clear
  message.
- [ ] **`INT-29` Windows container backend.** For operators with a Windows
  host instead of KVM: a Server Core container image pinned by digest, with the
  same Chocolatey CLI and capture helpers as the golden images (`INT-13`). One
  container per root package under Hyper-V isolation (process isolation only
  in CI), `--network none`, the closure mounted read-only as a folder source,
  one writable output directory, CPU and memory limits, a wall-clock timeout,
  and removal whatever happened; Docker Engine API or `docker` CLI is decided
  in the PR. The regression corpus (`INT-22`) runs against both backends and
  must give the same capture. A validation install (`INT-26`) on this backend
  needs a way to restrict a container's network *(to design)*.

### Server: later

- [ ] **`INT-33` Internalization requests.** Admins request a package in the
  admin area; the worker polls a small JSON API with a dedicated worker key
  that can read the queue and report status, and pushes with its push key as
  before. The server still runs nothing; the queue only saves a trip to the
  worker. Its errors are JSON for a request that asks for `application/json`,
  as admin errors are since 0.7.0. The queue needs a numbered schema migration
  (`src/database/sqlite/schema.rs`) and reports its length and oldest request
  in `/metrics` and `/health/status`. Whether it is built at all is open (see
  *Open questions*).

### Documentation

- [ ] **`INT-34` Documentation and threat model.** `docs/internalizer.md` in
  the MkDocs nav: the intake feed and approving with `yanuget admin approve`,
  the worker host with KVM or Windows containers and the two systemd units of
  `INT-15`, building the golden images, the Windows license the guests need,
  overrides, and the operator's responsibility for the licenses of packaged
  software. A section in SECURITY.md: the worker is its own trust zone, holds a
  push key for the intake feed only, runs guests as a user that holds no key,
  and what it produces is reviewed before it reaches clients.

### Open questions

- **Fixing an approved version.** It cannot be replaced, and the package fix
  version (`1.2.3.20261015`) breaks the exact pin of a meta package on its
  `.install` package. Re-internalize the dependents with the pin rewritten, or
  allow a documented delete-and-repush? Decide before `INT-27`.
- **Downloads from extension modules.** The prelude only redirects calls made
  by the package's own script. A helper inside an extension that downloads
  (the Visual Studio extension's bootstrapper, for example) is captured but not
  redirected, and ends as `needs-override`. Is a supported way to intercept
  those worth its complexity?
- **Supported Chocolatey CLI versions.** The wrappers are generated from one
  version's helpers. Which client versions must an internalized package
  support, and how is that tested?
- **Releases.** [Target releases](#target-releases) assumes the tool is
  versioned on its own. Open: a tag prefix for it (`internalize-v*` next to
  `v*`), crates.io trusted publishing per crate, whether `yanuget-pkg` is
  published at all, and a changelog of its own or a section in `CHANGELOG.md`.
  RELEASING.md and the release workflow follow from the answers.
- **Licensing the guests.** Which Windows license covers a golden image that
  is restored thousands of times a month, and may evaluation media run in CI?
  The answer decides whether KVM can be the default for every operator, and is
  needed before `INT-13` ships in a release.
- **Requests on the server or in the watch list?** `INT-33` needs a schema
  migration (step 8 in `src/database/sqlite/schema.rs`), after which older
  builds refuse the database (`check_known`), and it widens `PackageDatabase`
  for every future backend. The watch list of `INT-27`, kept in a reviewed
  repository, covers the same need without touching the server; what is lost
  is a request button in the admin area.
- **Names.** `yanuget-internalize` and `yanuget-pkg` are placeholders.

### Target releases

A proposal on top of 0.7.0, in the order the items depend on each other.

| Release | Items | Notes |
| --- | --- | --- |
| YANuget 0.8.0 | `INT-01`–`INT-05`, `INT-30`, `INT-32` | Released 2026-10-20, together with the fixes from the review of 2026-10-19, some of which need attention when upgrading (see CHANGELOG.md). |
| YANuget, after 0.8.0 | `INT-31`, the server side of `INT-21` | The rest of what the tool needs. `INT-31` waits on the security review of its token design. |
| internalizer 0.1.0 | `INT-10`–`INT-22` | KVM backend; needs YANuget 0.8.0, and the release with `INT-31` for read-gated feeds. |
| internalizer 0.2.0 | `INT-23`–`INT-28` | Needs nothing beyond YANuget 0.8.0. |
| internalizer 0.3.0 | `INT-29` | Optional. |
| YANuget, later | `INT-33` | Only if the open question is answered for it; needs a schema migration. |

`INT-34` is not a release of its own: each release ships the part of
`docs/internalizer.md` and SECURITY.md that covers it.

### Sizing

Rough, for one developer who knows the codebase and Chocolatey packaging:

| Part | Person-weeks |
| --- | --- |
| Server, 0.8.0 (`INT-01`–`INT-05`, `INT-30`–`INT-32`) | 5–7 |
| First version (`INT-10`–`INT-22`) | 10–12 |
| Production (`INT-23`–`INT-28`) | 6–8 |
| Windows container backend (`INT-29`), optional | 2–3 |
| Server, later (`INT-33`) | 1–2 |

The KVM backend costs about two weeks more in the first version than
containers would have, mostly for the unattended image build and the guest
agent; the validation network of `INT-26` gets cheaper in return. 0.7.0
changes none of this: the systemd split in `INT-15` and the `config check`
of `INT-10` fit inside the ranges. Documentation (`INT-34`) is inside each
row. Bringing `INT-30`–`INT-32` forward into 0.8.0 changes when that work
happens, not how much of it there is.
