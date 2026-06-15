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
# Build (Rust 1.82+)
cargo build --release

# Run with an API key and a data directory
YANUGET_API_KEY=change-me ./target/release/yanuget
```

The server prints its listen address and service index URL on startup
(default `http://0.0.0.0:5000`, index at `/v3/index.json`).

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
| Overwrite | `YANUGET_ALLOW_OVERWRITE` | `false` | Re-push an existing version. |
| Hard delete | `YANUGET_HARD_DELETE_ENABLED` | `false` | Otherwise DELETE unlists. |

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
| Registration leaf | `GET /v3/registration/{id}/{version}.json` |
| Search | `GET /v3/search?q=&skip=&take=&prerelease=&semVerLevel=&packageType=` |
| Autocomplete / versions | `GET /v3/autocomplete?q=` / `?id=` |
| Push symbols | `PUT /api/v2/symbol` |
| Download symbol (SSQP) | `GET /download/symbols/{file}/{key}/{file}` |
| Web gallery | `GET /` and `GET /packages/{id}[/{version}]` |
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

* searchable package list,
* a per-package detail page with versions, dependencies, links, readme and the
  install command for Chocolatey / `dotnet` / `nuget.exe`.

Disable it with `enable_web_ui = false`.

## Package retention

Opt-in automatic pruning of old versions, configured under `[retention]`. A
version is hard-deleted (payload, sidecars and symbols) when it is beyond the
newest *N* of its release channel **or** older than `max_age_days`; the newest
stable version (or newest pre-release, if none is stable) is always kept.
Retention runs on a schedule (`interval_hours`) and/or after each push
(`prune_on_push`). See [`yanuget.example.toml`](yanuget.example.toml).

## Roadmap

Implemented: NuGet v3 push/restore/search/registration/autocomplete, streaming
large-package support, API-key auth, filesystem storage, SQLite index, unlist /
relist / hard-delete, Range downloads, **symbol/PDB server**, a **web gallery**,
and **package retention policies**.

Not yet implemented (contributions welcome): upstream mirroring/caching of
nuget.org, additional storage backends (S3/Azure Blob) and database backends
(PostgreSQL/MySQL), and native (Windows) PDB indexing. These are deliberately
behind trait boundaries so they can be added without touching the core.

---

## License

[MIT](LICENSE).
