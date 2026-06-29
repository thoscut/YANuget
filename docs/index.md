# YANuget

**Yet Another NuGet server** — a fast, streaming [NuGet v3](https://learn.microsoft.com/en-us/nuget/api/overview)
server written in Rust, built around one constraint: **handle very large packages
(25 GB and beyond) without ever loading them into memory.**

This documentation is served directly from the running server at `/docs` and is
fully self-contained — it loads no external fonts, scripts or styles, so it works
on an air-gapped network.

## Where to go next

- **[HTTP API](api.md)** — every endpoint YANuget exposes (service index, push,
  download, registration, search, autocomplete, symbols).
- **[Configuration](configuration.md)** — all settings and `YANUGET_*`
  environment variables, including TLS, rate limiting, multiple API keys,
  retention, and multi-feed / mirroring / license-policy options.
- **[Architecture](architecture.md)** — the module map and trait boundaries.
- **[Large packages](large-packages.md)** — the bounded-memory streaming design.

## Quick start

```bash
# Build (Rust 1.82+) and run with an API key
cargo build --release
YANUGET_API_KEY=change-me ./target/release/yanuget
```

```bash
# Add the feed and push a package
dotnet nuget add source http://localhost:5000/v3/index.json -n yanuget
dotnet nuget push MyPackage.1.0.0.nupkg --source yanuget --api-key change-me
```

By default the server listens on `https://0.0.0.0:5000` with an auto-generated
self-signed certificate. See [Configuration → TLS](configuration.md#tls) for
production certificates or running behind a TLS-terminating reverse proxy.

## Highlights

- NuGet v3 push / restore / search / registration (paginated) / autocomplete.
- Streaming, bounded-memory handling of multi-gigabyte packages with resumable
  (HTTP Range) downloads.
- Symbol server (`.snupkg`, Portable PDB → SSQP).
- A dependency-free web gallery, statistics and admin moderation pages.
- Multiple feeds with a deduplicated store, release-ring promotion / approval
  gates, upstream mirroring (with optional authentication) and an offline
  license policy.
- Per-IP rate limiting, multiple API keys, and configurable overwrite policy.
