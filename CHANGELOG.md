# Changelog

All notable changes to YANuget are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

While the major version is `0`, the minor version is bumped for breaking changes
to the configuration file, the CLI or the on-disk layout, and the patch version
for everything else. The NuGet v3 endpoints are a published protocol and are not
expected to change incompatibly at any version.

## [Unreleased]

Nothing yet.

## [0.1.0] — 2026-08-05

First public release.

### Added

**NuGet v3 server**

- Service index, flat container, registration (index, pages and leaves — served
  as separate SemVer1 and SemVer2 hives), search, and autocomplete for both
  package ids and versions.
- Push (`PUT /api/v2/package`), delete/unlist (`DELETE`) and relist (`POST`),
  accepting both `multipart/form-data` and raw request bodies.
- Configurable overwrite behaviour: `false`, `true` or `prerelease-only`; hard
  delete is opt-in, and `DELETE` unlists by default.
- API-key authentication with multiple accepted keys, an optional separate
  read key, and a per-IP rate limiter.
- Verified against the real `dotnet` CLI end to end — pack, push, restore,
  build and run — in CI, not only against an HTTP client library.

**Large packages without large memory**

- Uploads stream chunk-by-chunk to a temp file while SHA-512 is computed
  incrementally, so nothing is buffered and the payload is never re-read.
- The `.nuspec` is read by seeking to the ZIP central directory, so a 25 GB
  `.nupkg` is touched in two small reads rather than scanned end to end.
- The finished temp file is atomically renamed into the store.
- Downloads stream from disk with `Range` support, so multi-gigabyte restores
  are resumable and never buffered.
- All sizes are `u64`/`i64` end to end; nothing truncates at the 4 GB boundary.
- Measured on a 5 GiB package: ≈16 MB RSS at peak for push and download alike,
  and ≈15 MB with six concurrent 5 GiB downloads in flight.

**Feeds and distribution**

- Multiple feeds in one server, each mounted under `/{name}`, with a feed index
  at `/`. Payload and metadata are stored once and referenced by per-feed
  membership, so a version in five feeds costs one copy on disk.
- Approval gates (`requires_approval`) and release-ring promotion
  (`promotes_to`) for staged rollouts.
- Upstream mirroring: a read-through cache of any NuGet v3 feed, with optional
  Basic/Bearer/custom-header upstream authentication.
- `yanuget migrate`: bulk import of every package from another server, with
  live progress, ETA and transfer rate. Idempotent and resumable.
- Offline license policy per feed, evaluating SPDX `licenseExpression` (or the
  legacy `licenseUrl`) against allow/deny lists with `warn` or `block`.
- Opt-in retention policies that prune old versions by count and age while
  always keeping the newest stable version.

**Symbols**

- `.snupkg` ingest at `PUT /api/v2/symbol`, extracting every Portable PDB and
  indexing it by its SSQP key, served at the path the .NET debugger requests.
- Native (Windows) PDBs are stored but not indexed.

**Web and operations**

- A dependency-free HTML gallery at `/`: package list with search, per-package
  detail pages with readme and install commands for Chocolatey / `dotnet` /
  `nuget.exe`, embedded package icons, a statistics page and a read-only
  settings overview that never reveals secrets. No external resources are
  loaded, so it works on an air-gapped network.
- An `/admin` area (HTTP Basic) to disable, re-enable, delete, approve and
  promote individual versions.
- The full MkDocs documentation site, embedded in the binary and served offline
  at `/docs`.
- `GET /health` (readiness — probes the database) and `GET /health/live`.
- TLS on by default, with a self-signed certificate generated and cached on
  first start; supply `tls_cert_path`/`tls_key_path` for a real one, or set
  `tls_enabled = false` to run behind a terminating proxy.
- Layered configuration: built-in defaults → TOML file → `YANUGET_*`
  environment variables. Unknown keys are rejected rather than ignored, so a
  typo in a config file is reported instead of silently doing nothing.

### Security

The threat this release is most concerned with is not the server being
compromised but the server becoming a hazard to the clients that restore from
it. The following are properties of this release, not fixes to a shipped one:

- `X-Forwarded-*` and `Forwarded` headers are honoured only from peers listed
  in `trusted_proxies` (default: private ranges). Untrusted peers cannot steer
  the absolute URLs a restoring client is handed, nor evade the rate limiter by
  forging a client address.
- Mirrored packages are verified to be the package that was requested — id,
  version and hash — before being published locally under a trusted name. A
  version already held by another feed with a different hash is rejected.
- Mirror upstreams are checked for scheme and private-address targets (SSRF),
  cross-host redirects are refused when credentials are attached, and downloads
  are size-bounded.
- Archives with mismatched or duplicate central-directory entries (split-view
  ZIPs, where a validator and a consumer disagree about the contents) are
  refused.
- Package icons are content-sniffed and only raster formats are served; an
  "icon" that is really an SVG — a script-bearing document — is refused rather
  than handed to a browser.
- Gallery pages ship a Content-Security-Policy whose inline script and style
  are pinned by SHA-256 hash; admin actions require a CSRF token derived from
  the admin key; responses carry `nosniff`, `X-Frame-Options: DENY`, a
  `Referrer-Policy`, `Vary` on the headers that select the generated URLs, and
  HSTS when serving TLS.
- Downloads are conditional and immutably cacheable (`ETag` and `304`), and
  `Content-Disposition` filenames are sanitised.
- Manifest parsing is bounded in depth and element count, and error responses
  do not leak internal detail.
- Symbol downloads require read authorisation and resolve to a package the
  requester is allowed to see.

[Unreleased]: https://github.com/thoscut/yanuget/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/thoscut/yanuget/releases/tag/v0.1.0
