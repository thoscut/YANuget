# Bulk migration

`yanuget migrate` copies **every** package from another NuGet server into a
local feed. Where [mirroring](configuration.md#feeds) caches packages on demand,
this is the one-shot import you run when moving off an existing server.

It discovers every package on the source — paging its `SearchQueryService`, and
falling back to the `Catalog/3.0.0` resource for feeds whose search is capped or
absent — then streams each `.nupkg` through the same indexing pipeline a push
uses. The target feed's `requires_approval` gate and `license_policy` apply, so
an import cannot put anything into a feed that a push could not.

Nothing is buffered: each package is streamed to a temp file, hashed as it
arrives, and renamed into place, exactly as described in
[Large packages](large-packages.md).

## Usage

```bash
# Import everything from a source server into the "default" feed.
yanuget migrate --source https://old-server/v3/index.json --feed default

# Authenticated source, more parallelism, stable versions only.
yanuget migrate --source https://old-server/v3/index.json \
  --source-username ci --source-password "$TOKEN" \
  --concurrency 8 --skip-prerelease

# See what would be copied, without downloading anything.
yanuget migrate --source https://old-server/v3/index.json --dry-run
```

A live display shows progress, ETA and transfer rate while it runs.

## Options

| Flag | Default | Description |
| --- | --- | --- |
| `--source <url>` | *(required)* | Source V3 service index, e.g. `https://old-server/v3/index.json`. |
| `--feed <name>` | `default` | Local feed to import into. |
| `--source-username` / `--source-password` | *(none)* | HTTP Basic credentials for the source. |
| `--source-token <token>` | *(none)* | Bearer token for the source. |
| `--source-header "Name: Value"` | *(none)* | Extra request header; repeatable. |
| `--timeout-secs <n>` | `60` | Per-request timeout against the source. |
| `--concurrency <n>` | `4` | Packages downloaded and indexed at once. |
| `--skip-prerelease` | off | Import only stable versions. |
| `--overwrite` | off | Replace versions the target feed already has. |
| `--dry-run` | off | Discover and report only; download nothing. |
| `--max-package-size-bytes <n>` | *(no limit)* | Skip any source package larger than this. |

`--config` works here too, so the target server's TOML — including the feed
definitions — is read the same way the server reads it.

## Notes

- **It is idempotent and resumable.** Versions the target feed already holds are
  skipped, so re-running after an interruption picks up where it stopped. Use
  `--overwrite` only when you deliberately want to replace what is there.
- **Run it against a stopped server, or a quiet one.** The command opens the
  same data directory and database as the server. That works, but the two
  processes do not share the server's in-process version locks, so a migration
  running against a feed that is simultaneously being pushed to is best avoided.
- **Start with `--dry-run`.** It reports how many packages and versions would be
  copied, which is the cheapest way to find out that the source's search
  resource is paginating in a way you did not expect.
- **`--concurrency` trades throughput for load on the source.** The default of 4
  is polite; public feeds may throttle higher values.
