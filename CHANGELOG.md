# Changelog

All notable changes to YANuget are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

While the major version is `0`, the minor version is bumped for breaking changes
to the configuration file, the CLI or the on-disk layout, and the patch version
for everything else. The NuGet v3 endpoints are a published protocol and are not
expected to change incompatibly at any version.

## [Unreleased]

**Upgrading from 0.5.x needs attention.** This release follows a full review
(see [ROADMAP.md](ROADMAP.md)); several changes fail closed where 0.5 was
lenient:

- An unparseable `YANUGET_*` environment value, a zero rate-limit window, a
  half-configured TLS certificate/key pair, or mirror Basic and token auth set
  together now **stop startup** instead of being ignored. `YANUGET_HOST`
  accepts an IP address or `localhost`.
- With `base_url` set, a request for any other host is answered `421`; list
  further names in `allowed_hosts`.
- Listed `cors_allowed_origins` allow `GET`/`HEAD`/`OPTIONS` only, and a
  browser request marked `Sec-Fetch-Site: cross-site` cannot push, delete or
  change anything.
- Manifests are read as NuGet reads them, capped at 1 MiB; versions follow
  NuGet's rules (no leading `v`, no leading zeros in numeric pre-release
  parts, Int32 components, at most 64 characters). Symbol packages must match
  an assembly in the package they belong to, as on nuget.org.
- `yanuget migrate` overwrites only with `--overwrite`, no longer following the
  target feed's `allow_overwrite`.
- `scripts/Send-YanugetFile.ps1` takes the key as a `SecureString` or from
  `YANUGET_API_KEY`, and requires https unless `-AllowHttp` is given.
- The database is migrated on first start (schema versions 3 and 4: pre-release
  keys stored before 0.5.0 are lower-cased, and a search index is built).
  Back it up first; older releases cannot open it afterwards.
- The container image runs `yanuget healthcheck` and no longer contains
  `curl`.

### Added

- The gallery list can be sorted by downloads (still the default), name, or
  most recently updated, from links above the list. Paging, the page-size form
  and a new search keep the order; the default stays out of the URL, so existing
  bookmarks mean what they meant.
- The admin page acts on several versions at once: tick them (or all of them)
  and enable, disable, approve, delete — or, with more than one feed, **copy**
  or **move** them to another feed. A move keeps each version's listed and
  enabled state and never touches its files, since the other feed then holds
  them. Every version is checked before anything changes, and the target feed's
  approval gate and license policy apply as for a push. Copying or moving into
  a feed takes credentials valid for that feed too, except into this feed's
  `promotes_to` target.
- A package's gallery page links to its admin page ("Manage versions"), and the
  header links to the admin area, whenever one is configured. Without one, the
  settings page says which setting turns it on.
- The server's version is shown in every page's footer and on the settings
  page. The stats page's lists show each package's version.
- The gallery filters by tag: every tag shown is a link to `/packages?tag=…`,
  which narrows the list — and a search — to packages carrying it,
  case-insensitively, and keeps the tag across paging, the page-size form and
  a new search. `/tags` lists every tag of the feed's visible packages
  alphabetically, set larger the more packages use it, with the count written
  out; the landing page offers the twelve most used. Tags are indexed in a new
  `package_tags` table, filled for existing databases once on startup.
- A package page links its `.nupkg` ("Download .nupkg"), and the admin page
  links each servable version's, both from the flat-container endpoint clients
  restore from.
- A feed can no longer be named `tags`.
- Versions can be **pinned** in the admin area, one at a time or as a
  selection. Retention never deletes a pinned version, and a pin does not use up
  one of the "newest N" the rules keep. A pin survives an overwriting push and a
  move to another feed; it does not stop an explicit delete, whose confirmation
  says the version is pinned. Stored in a new `feed_packages.pinned` column,
  added to existing databases on startup.
- `/admin/retention` shows the feed's retention rules as configured, what the
  last cleanup did, and every version the next one would delete, with the
  reason ("beyond the newest 5 stable versions", "older than 90 days") and the
  space it frees. Its button deletes exactly that list: it sends a fingerprint
  of the plan it showed, and the server recomputes the plan and deletes only if
  it still matches, so a push between looking and clicking cannot widen what is
  deleted. A cleanup and the scheduled sweep never run at once. The admin
  package page marks the versions the next cleanup would delete.
- **Attached files**: large files (`.wim` and other disk images, archives)
  attached to a package version, for its install script to fetch.
  - Served at `/files/{id}/{version}/{name}` for resumable clients: `HEAD`,
    single ranges, `If-Range`, a strong `ETag` (the SHA-256), `Last-Modified`,
    `Repr-Digest`, and always as a download (`octet-stream`, `attachment`,
    `default-src 'none'`). Verified with BITS (including a suspended and
    resumed job), `Invoke-WebRequest -Resume` and `curl -C -`.
  - Uploaded with the push key in one `PUT` (optionally checked against
    `X-Checksum-SHA256`), or resumably over tus 1.0.0 at `/api/v2/uploads`
    (creation, expiration, termination; stock clients work, and a resume also
    works across a server restart). `scripts/Send-YanugetFile.ps1` uploads
    resumably from PowerShell 7. A feed without a push key refuses files.
  - Or dropped over SSH into `[files].inbox_dir` with a `sha256sum` checksum
    file; the importer opens each file once, without following links, copies
    and verifies it through that handle into server-owned staging, attaches
    it, and explains a failure in a `.error` file next to it. Symbolic links,
    hard-linked files and anything but a regular file are refused wherever the
    upload account could plant them — the file, its checksum file, its
    directories and the `.error` report — so the account cannot make the
    server read or write outside the inbox. The copy needs as much free space
    again on the store's volume while it runs. YANuget runs no SSH server of
    its own.
  - Stored once per content under `.blobs/sha256/`, so versions that attach
    the same image share its bytes; a file goes with its version when it is
    deleted, pruned or moved, and its blob when nothing references it any
    more. File names are held to `A-Z a-z 0-9 . _ -` with an allowed
    extension, and never become part of a server path.
  - The package page lists a version's files with their SHA-256 and the
    `chocolateyInstall.ps1` lines (BITS plus `Get-ChecksumValid`) that fetch
    and check them; the admin page lists, downloads and deletes them; the
    stats page counts them; the settings page shows the file limits.
  - Configured under `[files]`. A feed can no longer be named `files`.
- `reserved_id_prefixes` on a feed: ids under a reserved prefix (`Contoso.`
  covers `Contoso` and `Contoso.*`) can be pushed, mirrored or migrated only
  into that feed; every other feed answers `403`. Reservations may not
  overlap. An id and version are one namespace across all feeds, so this is
  how a feed keeps a low-trust feed or a public mirror from claiming its names.
- `allowed_hosts` (`YANUGET_ALLOWED_HOSTS`), `max_connections` (default 4096)
  and `rate_limit.max_failed_auth` (default 30 failed authentications per
  window and client).
- Mirror settings `proxy`, `ca_cert_path`, `download_timeout_secs` (default
  3600) and `refresh_secs` (default 600); `yanuget migrate` gained
  `--source-ca-cert`, `--source-password-file`, `--source-token-file` and the
  `YANUGET_SOURCE_USERNAME`/`_PASSWORD`/`_TOKEN`/`_HEADERS` environment
  variables, so credentials no longer have to appear on the command line.
- `yanuget healthcheck`, which the container's `HEALTHCHECK` now runs: it reads
  the same configuration as the server, so a port or `tls_enabled` set in the
  TOML file is honoured.
- The container image is published for `linux/amd64` and `linux/arm64`.
  Release archives, `SHA256SUMS` and the image carry signed build provenance,
  and the image is signed with cosign; SECURITY.md explains how to verify
  them.
- An orphan sweep, at startup and daily, removes package data no feed
  references any more.

### Changed

- The gallery has a design of its own instead of a copy of GitHub's colours: a
  package page is laid out as a shipping label, lists as ruled manifests, and
  its one accent colour is kept for the primary action. It follows the
  browser's light or dark preference as before, and still loads nothing from
  outside the server.
- The gallery's typeface is Atkinson Hyperlegible Next (SIL Open Font License
  1.1), embedded in the binary (34 KB) and served from
  `/_assets/fonts/…woff2` with an immutable cache. The gallery's
  Content-Security-Policy gained `font-src 'self'`.
- The embedded documentation at `/docs` wears the same look: Material's
  palette remapped onto the gallery's colours in `docs/stylesheets/extra.css`,
  the gallery's font (loaded from the server, with the system fonts as the
  fallback when the site is opened any other way), and the taped-box mark as
  its logo and favicon. It follows the browser's light or dark preference, with
  Material's switch to override it. The theme's `custom_dir` is the new
  top-level `overrides/`, which the Dockerfile now copies into the docs build.
- The README's screenshots and walkthrough GIFs are recaptured from the new
  design, and `scripts/media/capture.mjs` clicks the gallery's new list markup
  (`.pkg h2 a`) — with the old selector the capture, and so the media workflow,
  would have failed.
- A feed can no longer be named `_assets`: that is where the font is served.
- `web::FeedMeta` carries the feed's admin key and has a
  `FeedMeta::from_resolved` constructor.
- Downloads now carry `Last-Modified` (the publish time) and honour `If-Range`,
  so a resuming client — BITS compares both between the requests of one
  transfer — never splices bytes of two different builds together: a resume
  whose validator no longer matches gets the whole current file. A download is
  counted once per transfer, for a `GET` of the whole file or of a range from
  its first byte, rather than once per ranged request and `HEAD`.
- An upload is aborted (`408`) once no byte has arrived for
  `upload_idle_timeout_secs` (default 300). Only silence counts; a slow but
  moving transfer is never cut off. Nothing timed a request body out before, so
  a stalled client held its connection and temp file open indefinitely.
- An upload is refused (`507`) when it would leave less than
  `min_free_disk_bytes` (default 2 GiB) free on the storage volume, checked
  against the declared size when the client sends one.
- A package keeps at most 64 tags of at most 64 characters, de-duplicated
  case-insensitively, and a gallery row shows at most 32. Nothing bounded the
  field but the 16 MiB manifest cap, so one push could put millions of tags on
  every page and search result.
- Retention ranks and protects only versions clients can restore: pending and
  disabled versions are neither counted towards `keep_latest_*` nor pruned.
  Unlisted versions still count, since they restore by exact version.
- An overwriting push keeps the version's listed, enabled and pinned state, its
  download count and its attached files, and sends new content back to
  approval in a gated feed. Copy and promote carry the source's listed,
  enabled and pending state.
- Search uses an SQLite FTS5 trigram index (still a case-insensitive substring
  match). `q` is cut to 256 characters, and only the first 4000 characters of a
  description are searched. The database runs with `synchronous = FULL`.
- Registration and search show versions as published (original casing and
  build metadata); embedded icons get an `iconUrl` served by the feed when the
  web UI is on, and license expressions a `licenses.nuget.org` `licenseUrl`.
  The standalone registration leaf has the shape the spec gives it.
- Downloads are counted only for `200`/`206` responses, off the request path.
- The read-through mirror fetches a requested version on a download miss even
  outside the newest-N cap, re-lists a package after `refresh_secs`, bounds a
  whole download by `download_timeout_secs` instead of the 30-second request
  timeout, caches unknown ids and a failing upstream negatively with backoff,
  and lets concurrent requests wait for a fetch in progress instead of
  answering `404`. A version deleted, pruned or moved out of a feed is recorded
  and never fetched back; pushing it again clears the record.
- `yanuget migrate` unions search and catalog, reports failed catalog pages and
  content mismatches as failures, keeps versions the source had unlisted
  unlisted, and stages each run in its own directory.
- Outbound TLS (mirror, migrate) trusts the system CA store as well as the
  bundled roots.
- The root feed index lists only feeds without a read key.
- The release workflow runs with least-privilege tokens, SHA-pinned actions, no
  caches and a protected `release` environment, and publishes to crates.io by
  trusted publishing. CI adds `cargo deny`, a weekly audit and tests at the
  MSRV. The docs and media toolchains install from hashed lock files. The
  image is built on Debian trixie with base images pinned by digest.
- Symbol packages for assemblies built without a PDB checksum (compilers
  older than Visual Studio 15.9) are refused, as on nuget.org.
- Only the manifest (and a declared readme or icon) is opened in a pushed
  archive, so an unreadable entry elsewhere no longer rejects the package.

### Fixed

- An overwrite that had to be refused — another feed holds the version, with
  different bytes — no longer takes the version out of the feed it was pushed
  to. The feed's membership was removed before the check that refused the push.
- Storage path segments are refused when Windows would read them as something
  other than a file name: a `:` (a drive-relative path, which escaped the store
  when joined, or an NTFS alternate data stream), a device name such as `NUL`
  or `COM1.pdb`, a trailing dot or space, or a control character. A symbol file
  named `c:x.pdb` inside a `.snupkg` could otherwise be written outside the
  store on a Windows host.
- Retention no longer deletes approved versions in favour of pending ones: in
  a gated feed with `prune_on_push`, pushing new builds could delete every
  approved version before any was approved.
- Versions stored before 0.5.0 with a mixed-case pre-release label could not be
  downloaded, deleted, unlisted or pruned by exact version; they are migrated.
- An overwriting push is atomic: it stores the new payload first, swaps the
  rows in one transaction, and puts the previous build back if anything fails.
  A failed store no longer loses the version.
- A purge that failed part-way no longer strands a version whose payload is
  gone; the leftovers are replaced on the next push or removed by the sweep.
  Pins set while a cleanup runs are honoured.
- A client disconnecting no longer interrupts indexing, symbol indexing,
  attaches or mirror fetches half-way, and no longer leaks upload temp files.
  Store writes are atomic and synced, and a failed rename no longer falls back
  to copying over the live file.
- Deleting a shared attached-file blob can no longer race an attach of the same
  bytes to another version, and a retried tus `PATCH` can no longer write into
  an upload that is being finished.
- Package ids that start with a Windows device name (`Aux.Core`, `Con.Utils`)
  are refused at push with a clear message, not at store time.
- Two processes opening the same database no longer race the schema
  migration.
- A 304 no longer counts as a download; `Range: bytes=5-3` is ignored as RFC
  9110 says; the `.nuspec` is streamed and, like the icon, has an `ETag`; an
  overwrite can no longer pair new bytes with an old `ETag`; the CORS layer's
  `Vary` is kept.
- Admin bulk enable, disable, approve, pin and unpin apply in one transaction.
- Graceful shutdown has a deadline on plain HTTP as it had on TLS; a retention
  sweep stops between versions, and background tasks are awaited.
- A credentialed mirror whose upstream redirects downloads to a CDN works.
- `build.rs` no longer recompiles the crate on every build when `site/` is
  absent. `chacha20` moves off the yanked 0.10.1.
- Documentation: the trusted-proxy default, the Compose example, the nginx
  client-address headers, consistent `base_url` advice, backups (ordering,
  `tls/`, the full layout), the upload idle timeout, the feeds table, and the
  systemd unit's `AF_UNIX`.
- Versions follow NuGet's rules for new input: no leading `v`, no leading
  zeros in numeric pre-release parts, components up to Int32, validated build
  metadata, at most 64 characters. `1.0.0-01` and `1.0.0-1` can no longer
  become two versions. Versions already stored stay readable under the rules
  they were stored with.
- An embedded readme or icon over 1 MiB is refused with a clear error instead
  of being stored cut off (possibly mid-UTF-8) and still marked present.
- Symbol pushes take the version lock and are all-or-nothing.
- Autocomplete's `prerelease` defaults to `false`; search `totalDownloads`
  counts every version; dependency-group `@id`s are percent-encoded.
- An inbox import is refused, like a push, when it would leave less than
  `min_free_disk_bytes` free.

### Security

- Read-gated content is served `private` with `Vary: Authorization,
  X-NuGet-ApiKey` instead of `public`, so a shared cache cannot hand it to
  anyone; `immutable` is sent only while overwrite is off.
- Behind a trusted proxy the rate limiter keys on the rightmost untrusted
  `X-Forwarded-For` hop rather than the client-chosen leftmost; IPv6 clients
  are counted per /64; failed authentications have their own budget; keys
  under 32 characters draw a warning. `X-Forwarded-Proto` is honoured only for
  `http`/`https`.
- Requests for a host other than `base_url`'s (or `allowed_hosts`) are refused,
  which stops DNS rebinding from reaching an intranet feed through a browser.
- Admin CSRF tokens are HMAC-signed with a per-process secret and expire after
  12 hours; admin pages are `no-store`; key comparison no longer depends on the
  key's length; read and admin keys are trimmed like push keys.
- Package ids in URLs are validated before any lookup, and ids are case-folded
  as ASCII in the database, matching storage and the version lock.
- Header-read timeout and a connection cap; the plain index page escapes its
  URL.
- `/settings` shows only the scheme, host and port of an upstream, and `Debug`
  output of configuration and credentials is redacted, as are upstream URLs in
  logs and errors.
- The mirror checks the addresses every host name resolves to, on every
  connection and redirect (not only literal IPs), refuses more reserved and
  embedded-IPv4 ranges and `localhost.`, and ignores `HTTP(S)_PROXY` unless
  `mirror.proxy` is set. Upstream credentials go only to the upstream's own
  scheme, host and port; redirects are checked hop by hop and `https` → `http`
  is refused. Mirror and migrate downloads respect `min_free_disk_bytes`.
- The license policy normalises SPDX ids (`+`, `-only`/`-or-later`, deprecated
  ids) on both sides, so `GPL-2.0+` no longer passes a `GPL-2.0` deny rule, and
  an allow list accepts only known SPDX exceptions after `WITH`.
- Different content under an id and version the server already stores is
  reported as a failure (`409`) rather than skipped as a race.
- Symbol packages are verified against the version they belong to, as on
  nuget.org: each Portable PDB must match the CodeView entry and PDB checksum
  of the `.dll`/`.exe` beside it in the stored package. A symbol key is claimed
  once, under a lock, and stored bytes are never replaced by different ones, so
  an identical copy of a package pushed to another feed can no longer replace
  the PDBs the first feed serves, and nobody can claim a key before its owner.
- The manifest is read the way NuGet's reader reads it: fields only as direct
  children of `<metadata>`, in its namespace, case-sensitively. Manifests NuGet
  could read differently are refused — repeated `id`, `version`, `license` and
  similar fields, elements inside a text field, dependencies outside
  `metadata/dependencies`, a DOCTYPE or an undeclared prefix — so the feed can
  no longer index an identity, dependencies or a license other than the one a
  client sees.
- A crafted manifest can no longer cost CPU out of proportion to its size: at
  most 64 attributes per element, manifests capped at 1 MiB (was 16 MiB), and
  parsing moved off the async runtime.
- Symbol pushes stream PDBs to disk instead of holding up to 512 MiB in memory,
  and an oversized PDB is refused rather than stored cut off.
- A ZIP whose central directory is inconsistent — disagreeing entry counts,
  ZIP64 records, extra or duplicate records — or that has two root manifests is
  refused; an archive may hold at most 100 000 entries.

## [0.5.1] — 2026-09-24

### Added

- The gallery pager can go to a page by number and change how many packages a
  page shows: 20, 50, 100, or the configured `gallery_page_size`. Both are plain
  GET forms, so they work without JavaScript. After a size change the page shown
  is the one that holds the package that was first on screen, because `page`
  wins over `skip` and any offset now snaps to the start of its page. Later
  pages carry their number in the page title, the size choice stays on offer
  when everything fits on one page, and paging keeps `prerelease` and
  `packageType` filters as well as the search.

### Changed

- `base64` 0.22 → 0.23, `toml` 0.8 → 1.1 and `tower-http` 0.6 → 0.7. All three
  are majors; none needed a source change. `toml` 1.x replaces `toml_edit` with
  the smaller `toml_parser`/`toml_writer` pair.
- The `fs` and `limit` features of `tower-http` are no longer requested. Nothing
  in the crate used `ServeDir` or `RequestBodyLimitLayer` — the documentation is
  served from `rust-embed` and the upload limit is enforced while streaming — so
  they only added compile time.
- The SQLite queries are assembled at compile time rather than with `format!` on
  every request. Ten statements on the search, autocomplete and lookup paths were
  building the same string on each call; they are now `concat!`ed constants. The
  three places that genuinely must interpolate a name — `PRAGMA table_info` and
  `ALTER TABLE` in the migration, and the `IN (?…)` placeholder list — take
  `&'static str` or generated placeholders only, never caller data.
- `yanuget migrate` exits non-zero when any version failed to migrate, not only
  when nothing got through. A partial copy used to exit 0, so a script gating on
  the exit code read it as finished. Re-running retries only the failures.

### Fixed

- The Copy buttons showed on feeds served over plain HTTP, and without
  JavaScript, where they could not copy anything (browsers withhold the
  clipboard API outside a secure context). They now appear only where they
  work, each names its command for screen readers, and a copy is announced
  through a status region.
- The stats page set its six tiles in as many columns as fit (five and an
  orphan on a desktop) and its two lists in the package page's `1fr 340px`
  split. Tiles are now in rows of three (two on a phone) and the lists share
  the width evenly.
- Install commands wrapped after any hyphen, so `--version` could end one line
  as `--` and start the next as `version`. Each flag now stays on one line with
  its value; the URL still wraps, and the copied text is unchanged.
- The stats, settings, package and first-run pages went from `<h1>` straight to
  `<h3>`; every section heading is now an `<h2>`, and the package page's client
  labels sit under a new "Install" heading. Counts of one read "1 download" and
  "1 version". Links in running text and the footer are underlined, not told
  apart by colour alone. The install and first-run labels are no longer set in
  capitals, the first-run steps are an ordered list, the settings labels get a
  wider column, and a phone gets a little more width for content. Gallery
  cards now show how many versions a package has.
- The package page kept its version list out of reach. The sticky install card
  (over 500 px tall) covered Info and Versions while scrolling, and on a phone
  the list came only after the whole readme. The card no longer sticks, the
  sidebar reads Install, Versions, Info, and the readme is a grid item of its
  own that follows the sidebar on a narrow screen.
- Form controls rendered in the browser's own font (Arial on Windows) and
  with borders of 1.4–1.6:1 against the page. They now use the page font and
  a `--ctl` border token (3:1 or more in both themes); the dark theme's button
  hover colour was darkened to keep white text readable.
- A search paged past its last page said "No packages match", although it had
  matches. It now says there is nothing on that page and links to the last page
  and the first, keeping the search and the page size.
- The gallery answered an empty or mistyped `?skip=`, `?take=`, `?page=` or
  `?prerelease=` with a 400 page. Its query string is now read leniently: a value
  that does not parse means the default. The 400 page's advice now reads "Check
  the address for a typo."
- `yanuget migrate` found only the first 100 packages on a BaGetter source.
  BaGetter reports the number of results on the current page as `totalHits`,
  and discovery stopped once `skip` passed it. Discovery now pages until the
  source returns an empty page, advances by the results actually received (a
  source may return fewer than `take` asked for), and stops early only when a
  page repeats ids it has already seen.
- `yanuget migrate` failed every package the source could not send within
  `--timeout-secs`, because the timeout covered the whole download: at ~2 MiB/s
  and the default 60 s, anything past ~120 MB. The failure read "error decoding
  response body". A migration download is now bounded only by how long the
  source goes silent (connect and read timeouts). Listing requests keep a total
  deadline, and so do read-through mirror downloads, which an anonymous request
  can start.

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

[Unreleased]: https://github.com/thoscut/yanuget/compare/v0.5.1...HEAD
[0.5.1]: https://github.com/thoscut/yanuget/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/thoscut/yanuget/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/thoscut/yanuget/releases/tag/v0.4.0
