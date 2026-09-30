# Bulk migration

`yanuget migrate` copies **every** package from another NuGet server into a
local feed. Where [mirroring](configuration.md#feeds) caches packages on demand,
this is the one-shot import you run when moving off an existing server.

It discovers every package on the source — paging its `SearchQueryService` and
walking its `Catalog/3.0.0` resource when it has one, and taking the union, since
a search can be capped and a catalog is only as complete as its pages — then
streams each `.nupkg` through the same indexing pipeline a push uses. Versions
the source has unlisted are unlisted in the target too. The target feed's `requires_approval` gate and `license_policy` apply, so
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
| `--timeout-secs <n>` | `60` | How long the source may take to connect, or stay silent while answering. Listing requests must also finish within it; a package download may take longer, as long as data keeps arriving. |
| `--concurrency <n>` | `4` | Packages downloaded and indexed at once. |
| `--skip-prerelease` | off | Import only stable versions. |
| `--overwrite` | off | Replace versions the target feed already has. Without it, existing versions are skipped whatever the feed's `allow_overwrite` says. |
| `--dry-run` | off | Discover and report only; download nothing. |
| `--max-package-size-bytes <n>` | *(no limit)* | Skip any source package larger than this. |
| `--source-ca-cert <path>` | *(none)* | PEM file of extra CA certificates to trust for the source. The system store and the bundled Mozilla roots are trusted either way. |

`--config` works here too, so the target server's TOML — including the feed
definitions — is read the same way the server reads it.

## Notes

- **It is idempotent and resumable.** Versions the target feed already holds are
  skipped, so re-running after an interruption picks up where it stopped. Use
  `--overwrite` only when you deliberately want to replace what is there.
- **It exits non-zero if anything failed** — a version, a package whose
  versions could not be listed, or a catalog page that could not be read (the
  packages on it would be missing). Each is listed at the end, and the exit
  status is non-zero even when everything else got through, so a script can
  gate on it — for example before switching clients over or stopping the old
  server. Re-run to retry the failures; what already arrived is skipped.
- **A version refused as already present elsewhere is a failure.** When another
  feed on the target holds the same id and version with different content, the
  source's copy can never be served there, so it is reported rather than
  counted as skipped.
- **Deleted versions stay deleted.** A version deleted from (or retention-pruned
  in) the target feed is skipped on a re-run, as the mirror skips it. Push it
  to bring it back.
- **Run it against a stopped server, or a quiet one.** The command opens the
  same data directory and database as the server. That works, but the two
  processes do not share the server's in-process version locks, so a migration
  running against a feed that is simultaneously being pushed to is best avoided.
- **Start with `--dry-run`.** It reports how many packages and versions would be
  copied, which is the cheapest way to find out that the source's search
  resource is paginating in a way you did not expect.
- **`--concurrency` trades throughput for load on the source.** The default of 4
  is polite; public feeds may throttle higher values.
