# Configuration

YANuget reads configuration from three layers, each overriding the previous:

1. **Built-in defaults** (see below).
2. **A TOML file**, passed via `--config <path>` (or `YANUGET_CONFIG=<path>`).
3. **Environment variables** (`YANUGET_*`) — these win.

A fully commented template lives in
[`yanuget.example.toml`](../yanuget.example.toml).

## Options

| TOML key | Env var | Type | Default | Description |
| --- | --- | --- | --- | --- |
| `host` | `YANUGET_HOST` | IP | `0.0.0.0` | Interface to bind. |
| `port` | `YANUGET_PORT` | int | `5000` | TCP port. |
| `base_url` | `YANUGET_BASE_URL` | string | *(per-request)* | External base URL. If unset, derived from `Host`/`X-Forwarded-*`. |
| `data_dir` | `YANUGET_DATA_DIR` | path | `./data` | Root for all data. |
| `storage_path` | `YANUGET_STORAGE_PATH` | path | `{data_dir}/packages` | Package store. |
| `database_path` | `YANUGET_DATABASE_PATH` | path | `{data_dir}/yanuget.db` | SQLite file. |
| `api_key` | `YANUGET_API_KEY` | string | *(none)* | Required to push/delete. |
| `max_package_size_bytes` | `YANUGET_MAX_PACKAGE_SIZE_BYTES` | int | *(unlimited)* | Upload cap; streamed either way. |
| `allow_overwrite` | `YANUGET_ALLOW_OVERWRITE` | bool | `false` | Re-push an existing version. |
| `hard_delete_enabled` | `YANUGET_HARD_DELETE_ENABLED` | bool | `false` | DELETE removes vs. unlists. |
| `enable_symbol_server` | `YANUGET_ENABLE_SYMBOL_SERVER` | bool | `true` | Accept `.snupkg` and serve PDBs. |
| `enable_web_ui` | `YANUGET_ENABLE_WEB_UI` | bool | `true` | Serve the HTML gallery. |
| `primary_client` | `YANUGET_PRIMARY_CLIENT` | string | `choco` | Install command shown first (`choco`/`dotnet`/`nuget`). |

Booleans accept `1/true/yes/on` (case-insensitive) via environment variables.

## Retention

Automatic pruning of old versions, under the `[retention]` table. Retention
**hard-deletes** versions (payload, sidecars and symbols), so it is opt-in. A
version is pruned when it is beyond the newest *N* of its release channel
(stable / pre-release) **or** older than `max_age_days`; the newest stable
version — or newest pre-release when no stable exists — is always kept, so a
package can never be pruned out of existence.

| TOML key | Env var | Type | Default | Description |
| --- | --- | --- | --- | --- |
| `retention.enabled` | `YANUGET_RETENTION_ENABLED` | bool | `false` | Master switch. |
| `retention.prune_on_push` | `YANUGET_RETENTION_PRUNE_ON_PUSH` | bool | `false` | Prune a package right after each push. |
| `retention.interval_hours` | `YANUGET_RETENTION_INTERVAL_HOURS` | int | `24` | Background sweep interval; `0` disables the sweep. |
| `retention.keep_latest_stable` | `YANUGET_RETENTION_KEEP_LATEST_STABLE` | int | *(unset)* | Keep newest N stable versions per id. |
| `retention.keep_latest_prerelease` | `YANUGET_RETENTION_KEEP_LATEST_PRERELEASE` | int | *(unset)* | Keep newest N pre-release versions per id. |
| `retention.max_age_days` | `YANUGET_RETENTION_MAX_AGE_DAYS` | int | *(unset)* | Prune versions published more than N days ago. |

With no limit set, the sweep does nothing even when `enabled`.

## Logging

Logging uses `tracing`. Control verbosity with `RUST_LOG`, e.g.:

```bash
RUST_LOG=info,yanuget=debug,tower_http=debug ./yanuget
```

The default is `info`.

## Security notes

- **Always set `api_key` in production.** Without it, push and delete are open
  to anyone who can reach the server; YANuget logs a loud warning at startup in
  that case. The key is compared in constant time.
- YANuget does not terminate TLS. Run it behind a reverse proxy (nginx, Caddy,
  Traefik) that handles HTTPS, and forward `X-Forwarded-Proto`/`X-Forwarded-Host`
  so generated URLs use the right scheme/host.

## Reverse proxy

When hosting large packages, configure the proxy not to buffer and not to impose
a small body limit. Example nginx snippet:

```nginx
location / {
    proxy_pass http://127.0.0.1:5000;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-Proto $scheme;
    proxy_set_header X-Forwarded-Host $host;

    client_max_body_size 0;          # no upload size cap at the proxy
    proxy_request_buffering off;     # stream uploads through
    proxy_buffering off;             # stream downloads through
    proxy_read_timeout 3600s;        # allow slow, large transfers
}
```
