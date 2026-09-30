# Architecture

YANuget is a single Rust crate (library + binary) organized into focused modules
separated by trait boundaries. The design priorities are: **bounded memory**
(see [large-packages.md](large-packages.md)), **testability** (pure logic split
from I/O), and **swappable backends** (storage and database behind traits).

## Module map

```
src/
├── lib.rs            Crate root; re-exports
├── main.rs           Binary: config load, wiring, background tasks, graceful shutdown
├── error.rs          Error enum + IntoResponse (HTTP status mapping)
├── version.rs        NuGetVersion: parse / normalize / order / SemVer2
├── models.rs         Domain types: Package, Dependency, DependencyGroup, ...
├── validation.rs     Package-id rules
├── nuspec.rs         Event-based .nuspec XML parser
├── nupkg.rs          Seek-based ZIP reader (nuspec + targeted entry extraction)
├── streaming.rs      Bounded copy-to-disk with incremental SHA-512
├── config.rs         Layered config (defaults < TOML < env); feeds, mirror, policy
├── auth.rs           Constant-time API-key / read / admin checks
├── indexing.rs       Upload→validate→policy→store→membership pipeline (with rollback)
├── policy.rs         Offline license allow/deny evaluation (pure)
├── mirror.rs         Upstream read-through mirroring (V3 feed → local feed)
├── migrate.rs        `yanuget migrate`: bulk import of a whole source server
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
│   └── sqlite.rs     SQLite backend: global `packages` + `feed_packages` membership
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
next to `packages`, maintained by triggers. Schema changes are numbered by
`PRAGMA user_version` and run once, inside a `BEGIN IMMEDIATE` transaction, so
two processes opening the same file never both migrate it.

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
backends slot in without touching handlers:

- **`PackageStorage`** — payload + sidecars. The `PackageContent::LocalPath`
  return lets the web layer serve files with zero-copy Range support. A future
  object-store backend adds a streaming variant.
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
