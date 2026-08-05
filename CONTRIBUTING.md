# Contributing to YANuget

Thanks for taking an interest. Bug reports, protocol-compatibility findings and
pull requests are all welcome.

Security problems are the exception: please do **not** open a public issue for
one — see [SECURITY.md](SECURITY.md).

## Getting set up

```bash
# Rust 1.88 or newer (rust-toolchain.toml pins stable).
cargo build

# The docs site is optional for building — a placeholder page is embedded when
# MkDocs has not run — but you need it to work on the /docs endpoint.
pip install -r requirements-docs.txt
mkdocs build
```

Run the server:

```bash
YANUGET_API_KEY=change-me cargo run
```

It listens on `https://0.0.0.0:5000` with a self-signed certificate, so pass
`-k` / `--insecure` to `curl` and clients while testing.

## Before you open a pull request

Everything CI enforces, in the order it fails fastest:

```bash
cargo fmt --all
cargo clippy --all-targets --all-features   # must be warning-free
cargo test --all-features
mkdocs build --strict                       # if you touched docs/
```

If you changed anything that a NuGet client can observe — the service index, the
flat container, registration, search, download URLs, or symbol keys — also run
the real client:

```bash
cargo build --release && scripts/verify-with-dotnet.sh
```

This is not redundant with `cargo test`. The Rust tests drive the server with
`reqwest`, which is a faithful HTTP client but not a *NuGet* client: it does not
care whether the service index advertises the resources NuGet probes for, whether
the flat container lists a version NuGet is about to restore, or whether a symbol
key matches what a debugger computes. Real defects have hidden behind a green
test suite for exactly that reason.

## Screenshots on the README

The images under `.github/media/` are generated, never hand-edited. If you
change the gallery, regenerate them:

```bash
scripts/capture-media.sh
```

It builds the server, starts two throwaway instances (one seeded with a
realistic feed, one empty for the first-run panel), drives a real browser
against them and assembles the GIFs. A push to `main` that touches the UI and
leaves the committed media stale fails CI, so this is not something you can
forget quietly; the `Refresh product media` workflow also regenerates them on
demand and opens a pull request.

## What good changes look like

- **Tests come with the change.** A bug fix should include a test that fails
  without it. The suite covers version semantics, manifest parsing, the streaming
  hash, storage, the database and search, the protocol builders, the indexing
  pipeline, and full end-to-end HTTP flows — there is a place for most things.
- **Nothing buffers a package.** The whole point of this project is that package
  size is bounded by disk, not RAM. Any code path that reads a payload into a
  `Vec<u8>`, or reads an archive end to end to get at a small part of it, will be
  rejected. Stream it, or seek to it.
- **New backends go behind the existing traits.** Storage and database are
  `PackageStorage` and `PackageDatabase`; S3, Azure Blob, PostgreSQL and MySQL
  are all meant to be addable without touching the core.
- **Comments explain why, not what.** Match the density and voice of the code
  around you.
- **Configuration additions are documented** in `yanuget.example.toml` and
  `docs/configuration.md`, and rejected when misspelled — every config struct
  uses `deny_unknown_fields` so a typo is an error rather than a silent no-op.
- **Keep the web UI dependency-free.** The gallery loads no external CSS, fonts,
  scripts or images, and CI enforces the same for the docs site. Inline assets
  must be added to the CSP hash list in `src/web/ui.rs`.

## Commit messages and branches

Write the subject line as what the change does, in the imperative, without a
prefix: *"Keep unlisted versions restorable"*, not *"fix: unlisted versions"*.
Use the body to explain why the change is needed and anything non-obvious about
how it works.

Branch off `main` and open the pull request against `main`.

## Project layout

| Path | What lives there |
| --- | --- |
| `src/version.rs` | NuGet version parsing, normalization, ordering |
| `src/nuspec.rs`, `src/nupkg.rs` | Manifest parsing; seek-based archive reading |
| `src/streaming.rs` | Bounded-memory copy-to-disk with incremental SHA-512 |
| `src/storage/` | `PackageStorage` trait and the filesystem backend |
| `src/database/` | `PackageDatabase` trait and the SQLite backend |
| `src/nuget/` | Protocol: URL generation and JSON response builders |
| `src/indexing.rs` | Upload → validate → store → record, with rollback |
| `src/pdb.rs`, `src/symbols.rs` | Portable PDB parsing and `.snupkg` ingest |
| `src/web/` | axum router, handlers, Range-aware file serving, HTML gallery |
| `docs/` | MkDocs sources for the site embedded at `/docs` |
| `tests/` | Integration tests |
| `scripts/verify-with-dotnet.sh` | End-to-end check against the real .NET SDK |
| `scripts/capture-media.sh`, `scripts/media/` | Regenerates the README screenshots and GIFs |

Releases are described in [RELEASING.md](RELEASING.md).

## License

Contributions are accepted under the [MIT license](LICENSE).
