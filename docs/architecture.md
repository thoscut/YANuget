# Architecture

YANuget is a single Rust crate (library + binary) organized into focused modules
separated by trait boundaries. The design priorities are: **bounded memory**
(see [large-packages.md](large-packages.md)), **testability** (pure logic split
from I/O), and **swappable backends** (storage and database behind traits).

## Module map

```
src/
├── lib.rs            Crate root; re-exports
├── main.rs           Binary: config load, wiring, axum::serve, graceful shutdown
├── error.rs          Error enum + IntoResponse (HTTP status mapping)
├── version.rs        NuGetVersion: parse / normalize / order / SemVer2
├── models.rs         Domain types: Package, Dependency, DependencyGroup, ...
├── validation.rs     Package-id rules
├── nuspec.rs         Event-based .nuspec XML parser
├── nupkg.rs          Seek-based ZIP reader (nuspec + targeted entry extraction)
├── streaming.rs      Bounded copy-to-disk with incremental SHA-512
├── config.rs         Layered config (defaults < TOML < env)
├── auth.rs           Constant-time API-key check
├── indexing.rs       Upload→validate→store→record pipeline (with rollback)
├── storage/
│   ├── mod.rs        PackageStorage trait, PackageContent, AuxFile
│   └── filesystem.rs Streaming filesystem backend
├── database/
│   ├── mod.rs        PackageDatabase trait, SearchRequest/Page/Group
│   └── sqlite.rs     SQLite backend (JSON columns, grouped+ranked search)
├── nuget/
│   ├── mod.rs        JSON response builders (pure)
│   └── urls.rs       UrlBuilder (absolute resource URLs)
└── web/
    ├── mod.rs        AppState, router, handlers
    └── files.rs      Range-aware streaming file responses
```

## Request flow: push

```
PUT /api/v2/package
  └─ web::push_package
       ├─ auth.check_headers              (X-NuGet-ApiKey, constant-time)
       ├─ create temp file under {storage}/.uploads
       ├─ web::write_upload  ─▶ streaming::stream_to_writer_limited
       │     (multipart or raw body → temp file, + SHA-512, + size cap)
       └─ indexing::index_package
            ├─ nupkg::read_archive        (seek to nuspec; never reads payload)
            ├─ nuspec::parse_nuspec
            ├─ validation::validate_package_id
            ├─ build Package (size/hash from the stream summary)
            ├─ duplicate / overwrite policy
            ├─ storage.store_package       (atomic rename into place)
            ├─ storage.store_aux           (nuspec / readme / icon sidecars)
            └─ db.add                       (rollback storage on failure)
```

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
