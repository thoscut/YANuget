# Architecture

The YANuget server is a single Rust crate (library + binary) organized into
focused modules separated by trait boundaries; the package handling it shares
with its tools lives in a second crate, `yanuget-pkg`. The design priorities are: **bounded memory**
(see [large-packages.md](large-packages.md)), **testability** (pure logic split
from I/O), and **swappable backends** (storage and database behind traits).

## Module map

The workspace holds three crates. The server is the root crate; what it shares
with tools that build or fetch packages is `yanuget-pkg`, which depends on no
web framework and no database.

```
crates/
├── yanuget-pkg/         Package handling shared by the server and its tools
│   └── src/
│       ├── version.rs   NuGetVersion: parse / normalize / order / SemVer2
│       ├── range.rs     Version ranges, as a .nuspec dependency writes them
│       ├── nuspec.rs    Event-based .nuspec XML parser
│       ├── nupkg.rs     Seek-based ZIP reader (nuspec + targeted entry extraction)
│       ├── rewrite.rs   Rewriting a .nupkg: replace, add or remove entries
│       ├── models.rs    What a .nuspec declares: Dependency, DependencyGroup, ...
│       ├── v3.rs        The v3 documents an upstream serves, read leniently
│       ├── address.rs   Which hosts and addresses an upstream fetch must not
│       │                reach (the DNS resolver behind feature `net`)
│       ├── fetch.rs     Upstream fetch helpers: redacted URLs, capped decoding
│       ├── files.rs     The rule for names of attached files
│       ├── report.rs    The internalizer report a package carries
│       ├── text.rs      Characters that make untrusted text read differently
│       └── error.rs     The crate's own Error
└── yanuget-internalize/ The Chocolatey internalizer, a separate binary that
                         reaches the server only over HTTP (see its README)
```

The server re-exports `version`, `range`, `nuspec`, `nupkg` and `report` under
its own name (`yanuget::version`, …), which is how the flows below refer to
them.

```
src/
├── lib.rs            Crate root; re-exports
├── main.rs           Binary: config load, wiring, background tasks, graceful
│                     shutdown; the backup, restore, migrate and healthcheck
│                     commands
├── cli.rs            The `admin`, `config` and `feeds` sub-commands
├── error.rs          Error enum + IntoResponse (HTTP status mapping)
├── models.rs         The stored Package; re-exports the manifest types
├── validation.rs     Package-id rules
├── streaming.rs      Bounded copy-to-disk with incremental SHA-512
├── space.rs          SpaceGuard: free space and the bytes promised to writes in
│                     progress, one guard for uploads, mirror downloads and
│                     inbox imports
├── config/           Layered config (defaults < TOML < env)
│   ├── mod.rs        Config, loading, resolved paths
│   ├── env.rs        The `YANUGET_*` overlay, and the names nothing reads
│   ├── feeds.rs      Feeds, their mirror and license policy, and how they
│   │                 resolve against the server-wide settings
│   ├── sections.rs   Overwrite policy, retention, rate limit, monitoring, files
│   └── validate.rs   Checks on a loaded configuration, feed names and prefixes
├── config_check.rs   `yanuget config check`: what the server would stop on, or
│                     start with and then fail at
├── auth.rs           Constant-time API-key / read / admin checks
├── indexing.rs       Upload→validate→policy→store→membership pipeline (with rollback)
├── policy.rs         Offline license allow/deny evaluation (pure)
├── mirror/           Upstream read-through mirroring (V3 feed → local feed)
│   ├── mod.rs        The mirror of one feed and its entry points
│   ├── client.rs     The HTTP client: vetting and sending requests, redaction
│   ├── upstream.rs   Reading the upstream's documents and downloads
│   ├── listing.rs    A package's upstream version list; following its listing
│   ├── target.rs     Filling the feed: fetching versions and indexing them
│   ├── tracker.rs    Per-package fetch state, back-off, running fetches
│   └── health.rs     Whether the upstream answers, and the probe behind it
├── migrate.rs        `yanuget migrate`: bulk import of a whole source server
├── inbox.rs          The SSH inbox: files dropped by scp/sftp/rsync, attached
│                     to versions
├── backup.rs         `yanuget backup` / `restore`: a consistent copy taken
│                     while the server runs, and the way back
├── data_lock.rs      Cross-process advisory lock on `<database_path>.lock`:
│                     one server on the data, and no offline command beside it
├── feed_data.rs      Versions left under a feed the configuration no longer
│                     has; what `yanuget feeds rename/remove` act on
├── admin_client.rs   HTTP client for a running server's /admin (`yanuget admin`)
├── pdb.rs            Portable PDB parsing → SSQP symbol key, PDB checksum
├── pe.rs             PE debug-directory reader (ties a PDB to its assembly)
├── symbols.rs        `.snupkg` ingest: extract PDBs, index by symbol key
├── ratelimit.rs      Per-client-IP fixed-window throttle
├── proxy.rs          Trusted-proxy gate for `X-Forwarded-*` / `Forwarded`
├── retention.rs      Pure prune policy + feed-scoped version pruning / GC
├── locks.rs          Process-global per-version async lock (store/purge races)
├── server.rs         Serving over HTTP or TLS: header timeout, connection cap, shutdown
├── tls.rs            TLS cert loading + cached self-signed generation
├── storage/
│   ├── mod.rs        PackageStorage trait, PackageContent, AuxFile
│   └── filesystem.rs Streaming filesystem backend (global, deduplicated)
├── database/
│   ├── mod.rs        PackageDatabase trait, Membership, FeedVersion, Search*
│   └── sqlite/       SQLite backend: global `packages` + `feed_packages` membership
│       ├── mod.rs    Connection setup; the trait impl, delegating per concern
│       ├── schema.rs Numbered migrations (`PRAGMA user_version`)
│       ├── packages.rs, memberships.rs, feeds.rs
│       │             Global package data; per-feed state; feed-scoped reads
│       ├── search.rs Search, autocomplete, tag counts (FTS5 index, tag index)
│       └── files.rs, uploads.rs, symbols.rs, tombstones.rs
├── nuget/
│   ├── mod.rs        JSON response builders (pure)
│   └── urls.rs       UrlBuilder (absolute resource URLs, feed-prefix aware)
└── web/
    ├── mod.rs        The routers: build_app, per-feed routes, the admin route_layer
    ├── state.rs      AppState, FeedContext, FeedMeta
    ├── middleware.rs Global layers: security headers + CSP, host / cross-site guard,
    │                 forwarded-header filter, rate limit, CORS; HTML error pages
    ├── protocol.rs   NuGet V3 reads: service index, flat container, registration,
    │                 search, autocomplete, symbol download; health probes
    ├── publish.rs    Push, symbol push, delete/unlist, relist
    ├── gallery.rs    Gallery handlers: list, package page, icon, tags, stats, settings
    ├── admin.rs      Admin area: auth gate, CSRF check, version actions, bulk,
    │                 copy/move/promote, retention
    ├── hosted.rs     Files attached to versions; resumable (tus) uploads
    ├── monitoring.rs `/metrics` (Prometheus text) and `/health/status` (JSON)
    ├── forms.rs      Form-body and lenient query-value parsing
    ├── helpers.rs    check_id / parse_version, header helpers, detached()
    ├── files.rs      Range-aware streaming file responses
    ├── assets.rs     The embedded gallery font
    ├── docs.rs       The embedded documentation site (this page), served at /docs
    └── ui/           HTML rendering with format!, one module per page family
        ├── escape.rs escape_html / safe_href / enc_path, and when each applies
        ├── layout.rs Shared chrome, inline style + script, the CSP, error page
        ├── gallery.rs, package.rs, stats.rs, settings.rs, admin.rs
        └── format.rs Counts, sizes, truncation, key/value rows
```

## Feeds

Package metadata (`packages`) and payloads are stored **once** and shared by
every feed. A *feed* is a set of [`feed_packages`] membership rows that link the
feed to the versions it exposes, each carrying that feed's own mutable state
(listed / enabled / pending / flagged / downloads). The same version can belong
to many feeds — release rings or independent sets — without duplicating bytes.
Each feed is an [`AppState`] (sharing the process-wide storage + database) with
its own [`FeedContext`] (auth, mirror, policy, retention), mounted under its
path prefix. `main` builds one state per resolved feed and nests their routers.

## Request flow: push

```
PUT /api/v2/package
  └─ web::publish::push_package
       ├─ auth.check_headers              (X-NuGet-ApiKey, constant-time)
       ├─ create temp file under {storage}/.uploads
       ├─ web::publish::write_upload ─▶ streaming::stream_to_writer_limited
       │     (multipart or raw body → temp file, + SHA-512, + size cap)
       └─ indexing::index_package
            ├─ nupkg::read_archive        (seek to nuspec; never reads payload)
            ├─ nuspec::parse_nuspec
            ├─ validation::validate_package_id
            ├─ build Package (size/hash from the stream summary)
            ├─ reserved id prefixes, license policy
            ├─ lock_version                (per id/version, across feeds)
            ├─ orphan check                (finish a purge that failed part-way)
            ├─ duplicate / overwrite policy (different bytes under a stored
            │                                id/version → 409 in any feed)
            ├─ storage.store_package       (atomic rename into place)
            ├─ storage.store_aux           (nuspec / readme / icon sidecars)
            └─ db.add_version / db.replace_version
                                           (one transaction; a new version's
                                            payload is removed if it fails, an
                                            overwritten one is put back)
```

The database keeps a full-text index for search (FTS5, trigram tokenizer)
next to `packages`, maintained by triggers. Memberships, tags, attached files
and symbol mappings each have a foreign key to their version in `packages`,
so deleting the version takes them with it. Every schema change is a numbered
step in `database/sqlite/schema.rs`, recorded in `PRAGMA user_version`; each
runs once, in its own `BEGIN IMMEDIATE` transaction, so two processes opening
the same file never both apply it, and a new database runs every step, so it
ends up with exactly the schema an upgraded one has. A changed table, index
or trigger is a new step, never an edit to an old one.

## Request flow: restore / download

```
GET /v3/index.json                → nuget::service_index(UrlBuilder)
GET /v3/package/{id}/index.json   → db.find_versions → nuget::flat_container_index
GET /v3/registration/{id}/...     → db.find_versions → nuget::registration_index
GET /v3/search?q=...              → db.search        → nuget::search_response
GET /v3/package/{id}/{v}/{f}.nupkg→ storage.get_package → files::serve_local_file (Range)
```

## Trait boundaries

Two traits isolate I/O so the core is testable with in-memory fakes and so new
backends have a seam to go behind:

- **`PackageStorage`** — payload + sidecars. The `PackageContent::LocalPath`
  return lets the web layer serve files with zero-copy Range support. It is the
  only variant, and downloads, uploads (`TempPath` renamed into place) and the
  SSH inbox assume local files, so an object-store backend has to add a
  streaming variant and teach those paths to use it — not only implement the
  trait.
- **`PackageDatabase`** — metadata, listing, download counts, search,
  autocomplete. The SQLite backend stores nested metadata as JSON columns and
  finishes NuGet's pre-release ordering in Rust (SQL can't express it).

The protocol layer (`nuget`) is pure data transformation over domain types and a
`UrlBuilder`, so every response shape is unit-tested without a running server.

## Why a single crate

The whole server compiles as one crate with internal modules rather than a
workspace of many crates. This keeps build times and cognitive overhead low
while the trait boundaries still give clean seams. If a backend grows large
(e.g. an S3 store with its own deps), it can be promoted to its own crate behind
the existing trait with no API churn.

`yanuget-pkg` is the one part that was split off, because another program
needs it: the internalizer reads, rewrites and names packages by the same
rules as the server, and must not pull in the server to do so. The server
re-exports its modules under their old paths, so nothing else moved.
