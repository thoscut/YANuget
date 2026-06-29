# YANuget

**Yet Another NuGet server** — a fast, streaming [NuGet v3](https://learn.microsoft.com/en-us/nuget/api/overview)
server written in Rust. YANuget is a from-scratch reimplementation of
[BaGetter](https://github.com/bagetter/BaGetter) (itself a fork of BaGet), built
around one guiding constraint: **handle very large packages (25 GB and beyond)
without ever loading them into memory.**

> Status: feature-complete core. Push/restore/search/registration work with the
> official `dotnet`/`nuget` clients. See [Roadmap](#roadmap) for what is not yet
> implemented.

---

## Why another NuGet server?

BaGet/BaGetter are excellent, but their request pipeline buffers package
payloads in memory in several places, which makes hosting multi-gigabyte
packages (large ML models, game assets, monorepo artifacts) painful. YANuget is
designed so that **package size is bounded by your disk, not your RAM**:

| Concern | YANuget approach |
| --- | --- |
| **Upload** | The request body (multipart *or* raw) is streamed chunk-by-chunk straight to a temp file. Peak memory ≈ one network buffer. |
| **Hashing** | SHA-512 is computed incrementally *during* the upload stream — the file is never re-read to hash it. |
| **Manifest read** | The `.nuspec` is pulled from the ZIP central directory by **seeking**, so a 25 GB `.nupkg` is never read end-to-end just to get a few KB of XML. |
| **Store** | The finished temp file is `rename`d into place — an atomic, zero-copy move (when temp and store share a filesystem). |
| **Download** | Files are streamed from disk and support HTTP **Range** requests, so 25 GB+ downloads are resumable and never buffered. |
| **Sizes** | All sizes are `u64`/`i64` end-to-end; nothing truncates at the 4 GB `u32` boundary. |

See [docs/large-packages.md](docs/large-packages.md) for the full design.

---

## Quick start

### From source

```bash
# Build (Rust 1.88+)
cargo build --release

# Run with an API key and a data directory
YANUGET_API_KEY=change-me ./target/release/yanuget
```

The server prints its listen address and service index URL on startup.
**TLS is on by default**, so it listens on `https://0.0.0.0:5000` with an
auto-generated self-signed certificate (cached under `{data_dir}/tls/`). For
local testing, trust that certificate or pass `--insecure`/`-k` to your client;
for production, provide a real cert via `tls_cert_path`/`tls_key_path`, or set
`tls_enabled = false` to run plain HTTP behind a TLS-terminating reverse proxy.
See [docs/configuration.md](docs/configuration.md#tls).

### Add the feed and push a package

```bash
dotnet nuget add source http://localhost:5000/v3/index.json -n yanuget

dotnet nuget push MyPackage.1.0.0.nupkg \
  --source yanuget \
  --api-key change-me
```

### Restore from it

```bash
dotnet restore --source http://localhost:5000/v3/index.json
```

---

## Configuration

Configuration is layered: **built-in defaults → TOML file → `YANUGET_*`
environment variables** (env wins). See
[`yanuget.example.toml`](yanuget.example.toml) for every option, and
[docs/configuration.md](docs/configuration.md) for details.

The essentials:

| Setting | Env var | Default | Notes |
| --- | --- | --- | --- |
| API key | `YANUGET_API_KEY` | *(none)* | Required to push/delete. **Set this.** |
| Listen port | `YANUGET_PORT` | `5000` | |
| Data directory | `YANUGET_DATA_DIR` | `./data` | Holds the package store and SQLite DB. |
| Base URL | `YANUGET_BASE_URL` | *(per-request)* | Derived from `Host`/`X-Forwarded-*` if unset. |
| Max upload size | `YANUGET_MAX_PACKAGE_SIZE_BYTES` | *(unlimited)* | Streams to disk regardless. |
| Overwrite | `YANUGET_ALLOW_OVERWRITE` | `false` | `false`/`true`/`prerelease-only`. |
| Hard delete | `YANUGET_HARD_DELETE_ENABLED` | `false` | Otherwise DELETE unlists. |
| Rate limit | `YANUGET_RATELIMIT_*` | on, 1000/60s | Per-IP throttle; returns `429`. |

---

## API

YANuget implements the NuGet v3 protocol. Full reference in
[docs/api.md](docs/api.md). Summary:

| Resource | Method & path |
| --- | --- |
| Service index | `GET /v3/index.json` |
| Push package | `PUT /api/v2/package` |
| Delete / unlist | `DELETE /api/v2/package/{id}/{version}` |
| Relist | `POST /api/v2/package/{id}/{version}` |
| Versions (flat container) | `GET /v3/package/{id}/index.json` |
| Download | `GET /v3/package/{id}/{version}/{id}.{version}.nupkg` |
| Registration index | `GET /v3/registration/{id}/index.json` |
| Registration page | `GET /v3/registration/{id}/page/{lower}/{upper}` |
| Registration leaf | `GET /v3/registration/{id}/{version}.json` |
| Search | `GET /v3/search?q=&skip=&take=&prerelease=&semVerLevel=&packageType=` |
| Autocomplete / versions | `GET /v3/autocomplete?q=` / `?id=` |
| Push symbols | `PUT /api/v2/symbol` |
| Download symbol (SSQP) | `GET /download/symbols/{file}/{key}/{file}` |
| Web gallery | `GET /` and `GET /packages/{id}[/{version}]` |
| Documentation | `GET /docs` (embedded, offline) |
| Admin (Basic auth) | `GET /admin`, `POST /admin/packages/{id}/{version}/{disable\|enable\|delete}` |
| Health | `GET /health` |

---

## Architecture

YANuget is a single crate with clearly separated modules behind trait
boundaries, which keeps each piece small and unit-testable. See
[docs/architecture.md](docs/architecture.md).

```
HTTP (axum)  ──▶  indexing pipeline  ──▶  PackageStorage (trait)  ──▶  filesystem
     │                  │                  PackageDatabase (trait)  ──▶  SQLite
     │                  └── streaming hash + seek-based nuspec read
     └── NuGet v3 protocol (URL + JSON builders)
```

| Module | Responsibility |
| --- | --- |
| `version` | NuGet version parsing, normalization & ordering |
| `nuspec` / `nupkg` | Manifest parsing; seek-based archive reading |
| `streaming` | Bounded-memory copy-to-disk with incremental SHA-512 |
| `storage` | `PackageStorage` trait + streaming filesystem backend |
| `database` | `PackageDatabase` trait + SQLite backend |
| `nuget` | Protocol: URL generation + JSON response builders |
| `indexing` | Upload → validate → store → record (with rollback) |
| `pdb` | Portable PDB parsing → SSQP symbol key |
| `symbols` | `.snupkg` ingest: extract PDBs, index by symbol key |
| `retention` | Pure prune policy + version pruning |
| `web` | axum router, handlers, Range-aware file serving, HTML gallery |

---

## Development

```bash
cargo test            # unit + integration tests
cargo clippy --all-targets   # lints (CI requires zero warnings)
cargo fmt --check     # formatting
```

The test suite covers version semantics, nuspec parsing, the streaming hash,
storage, the database/search, the protocol builders, the indexing pipeline, and
full end-to-end HTTP flows (push, download, Range, search, unlist/relist).

---

## Using YANuget with Chocolatey

YANuget is primarily intended as a private [Chocolatey](https://chocolatey.org)
feed. Chocolatey CLI v2+ speaks the NuGet v3 protocol, so point it at the
service index:

```powershell
choco source add -n=yanuget -s="http://localhost:5000/v3/index.json"

# Push (the API key is your YANUGET_API_KEY)
choco push my-package.1.0.0.nupkg -s="http://localhost:5000/v3/index.json" -k="change-me"

# Install
choco install my-package --version 1.0.0 --source="http://localhost:5000/v3/index.json"
```

The web gallery's package page shows the exact `choco install` command for each
version (configurable via `primary_client`).

## Symbol server

When `enable_symbol_server` is on (the default), YANuget accepts `.snupkg`
symbol packages at `PUT /api/v2/symbol` (e.g. `dotnet nuget push … --source`
with a `.snupkg`). It extracts every **Portable PDB**, computes its SSQP key
(GUID + `FFFFFFFF`), and serves it at
`GET /download/symbols/{file}/{key}/{file}` — the path the .NET debugger and
Visual Studio use. Point your debugger's symbol settings at the server's
`/download/symbols/` base. The owning package must be pushed before its
symbols. Native (Windows) PDBs are stored but cannot be indexed.

## Web gallery

A self-contained, dependency-free HTML gallery (no external assets, works
offline) lives at `/`:

* a searchable package list (search box in the header),
* a per-package detail page with versions, dependencies, links, readme and the
  install command for Chocolatey / `dotnet` / `nuget.exe`,
* a statistics page (`/stats`) with feed totals and the most-downloaded /
  recently-published lists,
* a read-only settings overview (`/settings`) that never exposes secrets,
* a configurable page size (`gallery_page_size`, default 20) with pagination.

The gallery loads **no external resources** — all CSS and JavaScript are inlined
and the favicon is an inline data URI, so it works on an air-gapped network.

Disable the gallery (and the docs below) with `enable_web_ui = false`.

## Documentation

The full documentation is built with [MkDocs](https://www.mkdocs.org/) (Material
theme) and **embedded into the binary**, served at `/docs` — completely offline,
with no fonts, scripts or styles fetched from outside the network. Build it
locally with:

```bash
pip install -r requirements-docs.txt
mkdocs build      # outputs site/, embedded at compile time
```

Release binaries ship the rendered docs; a plain `cargo build` without MkDocs
still compiles (a small placeholder page is embedded instead). The Markdown
sources live in [`docs/`](docs/).

## Admin moderation

When `admin_api_key` is set, an `/admin` area (HTTP Basic auth) lets an operator
**disable**, **re-enable** or **delete** individual package versions from the
browser. A *disabled* version is withheld from clients entirely — hidden from
search/registration/versions **and** not downloadable — which is stronger than
NuGet's *unlist* (an unlisted version stays downloadable for restore). Delete is
a hard delete (payload, sidecars and symbols). The area is only mounted when an
admin key is configured.

## Package retention

Opt-in automatic pruning of old versions, configured under `[retention]`. A
version is hard-deleted (payload, sidecars and symbols) when it is beyond the
newest *N* of its release channel **or** older than `max_age_days`; the newest
stable version (or newest pre-release, if none is stable) is always kept.
Retention runs on a schedule (`interval_hours`) and/or after each push
(`prune_on_push`). See [`yanuget.example.toml`](yanuget.example.toml).

## Feeds, mirroring & release rings

By default YANuget serves a single feed at the root. Define `[[feeds]]` in the
config to host several feeds at once — each mounted under `/{name}` (e.g.
`/stable/v3/index.json`), with a feed index at `/`. A package version can belong
to **many feeds simultaneously**; the payload and metadata are stored **once**
and referenced by each feed's membership, so nothing is duplicated on disk.

Each feed has its own put/get/delete configuration:

* `api_key` (push), optional `read_api_key` (download/restore), `admin_api_key`
  (moderation/promotion) — each falling back to the global key where sensible.
* `requires_approval`: incoming versions (push, mirror or promotion) land
  **pending** and are withheld from clients until an admin approves them in
  `/admin` — turning a feed into a release-ring gate.
* `promotes_to`: names the next ring; an admin can **promote** a version from
  one feed into the next (pending if that ring also gates). Feeds without
  `promotes_to` are simply independent sets.

```toml
[[feeds]]
name = "dev"
requires_approval = false
promotes_to = "stable"

  [feeds.dev.mirror]            # read-through cache of nuget.org
  enabled = true

[[feeds]]
name = "stable"
requires_approval = true        # versions are pending until approved

  [feeds.stable.license_policy]
  enabled = true
  allowed = ["MIT", "Apache-2.0"]
  action = "warn"               # or "block" to reject the push
```

### Upstream mirroring

A feed with `[feeds.<name>.mirror] enabled = true` becomes a read-through cache:
on a request for a package it does not have, YANuget fetches that package's
versions from the upstream V3 feed (default `https://api.nuget.org/v3/index.json`),
streams each `.nupkg` to disk and indexes it locally. Mirrored versions honour
the feed's `requires_approval` gate and `license_policy`. Mirroring is
best-effort: an upstream outage degrades to a normal cache miss.

### Bulk migration

Moving to YANuget from another NuGet server? Where mirroring caches packages
*on demand*, the `migrate` sub-command copies **everything** at once. It
discovers every package on the source (paging its `SearchQueryService`, falling
back to the `Catalog/3.0.0` resource), then streams each `.nupkg` through the
same indexing pipeline a push uses — honouring the target feed's
`requires_approval` gate and `license_policy`. A live display shows progress,
ETA and transfer rate:

```bash
# Import every package from a source server into the "default" feed.
yanuget migrate --source https://old-server/v3/index.json --feed default

# Authenticated source, more parallelism, stable versions only.
yanuget migrate --source https://old-server/v3/index.json \
  --source-username ci --source-password "$TOKEN" \
  --concurrency 8 --skip-prerelease

# See what would be copied without downloading anything.
yanuget migrate --source https://old-server/v3/index.json --dry-run
```

Versions already present in the target feed are skipped, so a migration is
**idempotent and resumable** — re-run it to pick up only what is missing.
Source credentials accept `--source-username`/`--source-password` (Basic),
`--source-token` (Bearer) or repeated `--source-header "Name: Value"`.

### Offline license policy

A feed's `[feeds.<name>.license_policy]` evaluates each pushed/mirrored
package's SPDX `licenseExpression` (or legacy `licenseUrl`) against `allowed` /
`blocked` lists — no network access. With `action = "warn"` (default) a
violation is accepted but **flagged** (visible in `/admin`); with
`action = "block"` the push is rejected.

## Roadmap

Implemented: NuGet v3 push/restore/search/registration (with paginated
registration for packages with many versions) / autocomplete, streaming
large-package support, API-key auth (**multiple keys**), per-IP **rate
limiting**, filesystem storage, SQLite index, unlist / relist / hard-delete,
configurable overwrite (incl. **pre-release-only**), Range downloads,
**symbol/PDB server**, a **web gallery**, **package retention policies**,
**multiple feeds** (a deduplicated store with per-feed membership), **upstream
mirroring** (read-through caching of a public feed, with optional
Basic/Bearer/custom-header **upstream auth**), **bulk migration** (`migrate`
command — copy every package from another server, with progress/ETA/transfer
rate), **release-ring promotion & approval gates**, and an **offline license
policy**.

Not yet implemented (contributions welcome): additional storage backends
(S3/Azure Blob) and database backends (PostgreSQL/MySQL), online vulnerability
scanning, and native (Windows) PDB indexing. These are deliberately behind trait
boundaries so they can be added without touching the core.

---

## License

[MIT](LICENSE).
