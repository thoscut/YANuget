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

## [0.5.0] — 2026-08-11

The first release with a changelog. Versions 0.1.0 to 0.4.0 predate it; what
follows describes the server as it stands, not only what changed since 0.4.0,
because there is no earlier entry to read it against.

**Upgrading from 0.4.0 or earlier needs attention.** Three defaults changed to
fail closed, and a deployment relying on the old ones will behave differently:

- `trusted_proxies` is now empty rather than trusting private ranges, so
  `X-Forwarded-*` is ignored unless you list your proxy. **Behind a reverse
  proxy, set `trusted_proxies` — or better, set `base_url`** — or the URLs
  handed to clients will be derived from the `Host` header alone.
- CORS headers are no longer sent unless `cors_allowed_origins` lists an
  origin. Browser tooling that read the feed cross-origin will need listing.
- The rate limit rose from 1000 to 10 000 requests/minute per IP.

The on-disk layout and the database are unchanged, and no migration is needed.

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
  loaded, so it works on an air-gapped network, and the palette follows the
  browser's light or dark preference.
- An empty feed answers with the three commands that fill it — add source,
  push, restore — already carrying this server's own service-index URL, rather
  than with "no packages".
- A startup summary listing the gallery, service index and documentation URLs,
  the configured feeds and the data directory, with a wildcard bind shown as a
  URL a client will actually accept.
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

**The defaults fail closed.** Each of these is the safe answer rather than the
convenient one, because the convenient one is what an unconfigured deployment
gets:

- `X-Forwarded-*` and `Forwarded` are honoured only from peers listed in
  `trusted_proxies`, which is **empty by default**. Trusting private ranges
  would cover reverse proxies, but the common deployment is an internal feed on
  a LAN with no proxy — where every client machine is in those ranges and could
  rotate `X-Forwarded-For` to evade the rate limiter, or steer the absolute
  URLs a restoring client is handed.
- **No CORS headers** unless `cors_allowed_origins` lists an origin. CORS
  constrains browsers and nothing else, so a permissive default would buy
  clients nothing while letting any page a user with network reach visits read
  a private feed's whole inventory.
- The rate limit is on at 10 000 requests/minute per IP — above what a large
  restore needs (a few hundred packages behind one NAT address), because NuGet
  treats `429` as terminal and neither retries nor honours `Retry-After`.

Beyond the defaults:

- Mirrored packages are verified to be the package that was requested — id,
  version and hash — before being published locally under a trusted name. A
  version already held by another feed with a different hash is rejected.
- Mirror upstreams are checked for scheme and private-address targets on
  **every redirect hop, not just the first**, and DNS names are resolved before
  being classified. Cross-host redirects are refused when credentials are
  attached. Downloads are size-bounded (2 GiB unless configured) and one
  read-through miss has a 60-second budget, so an anonymous read cannot hold a
  connection open fetching fifty packages.
- A symbol package cannot claim a symbol key another package already owns. Both
  halves of an SSQP key come from the upload, so without that check any push
  credential could repoint another feed's symbols at itself and serve its own
  PDB to someone debugging the victim.
- A version's identity is case-insensitive in its pre-release label, matching
  what NuGet clients assume — so `1.0.0-Beta` and `1.0.0-beta` cannot become two
  database rows sharing one file, where one advertises a hash the served bytes
  no longer match.
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
- Manifest parsing is bounded in depth and element count — every element,
  including the ones with their own handling — so a small, highly compressible
  manifest cannot cost minutes of CPU on an async worker. Symbol packages are
  bounded in entry count and total extracted bytes, and are read in one pass
  rather than one pass per entry.
- The stored `.nuspec` is served as an attachment with `default-src 'none'` and
  `nosniff`, so a manifest carrying an XSLT processing instruction cannot
  execute script in the feed's own origin.
- Error responses do not leak internal detail.
- Symbol downloads require read authorisation and resolve to a package the
  requester is allowed to see.

[Unreleased]: https://github.com/thoscut/yanuget/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/thoscut/yanuget/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/thoscut/yanuget/releases/tag/v0.4.0
