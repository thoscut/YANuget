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
| `api_keys` | `YANUGET_API_KEYS` | string[] | `[]` | Additional accepted push/delete keys (env: comma-separated). Any of these or `api_key` authenticates. |
| `admin_api_key` | `YANUGET_ADMIN_API_KEY` | string | *(none)* | Protects `/admin` (Basic auth). Unset ⇒ admin area off. |
| `gallery_page_size` | `YANUGET_GALLERY_PAGE_SIZE` | int | `20` | Packages per gallery page (`?take=` overrides). |
| `max_package_size_bytes` | `YANUGET_MAX_PACKAGE_SIZE_BYTES` | int | *(unlimited)* | Upload cap; streamed either way. |
| `allow_overwrite` | `YANUGET_ALLOW_OVERWRITE` | bool \| string | `false` | Re-push an existing version: `false`, `true`, or `"prerelease-only"` (overwrite pre-releases only). |
| `hard_delete_enabled` | `YANUGET_HARD_DELETE_ENABLED` | bool | `false` | DELETE removes vs. unlists. |
| `tls_enabled` | `YANUGET_TLS_ENABLED` | bool | `true` | Serve HTTPS (self-signed fallback). |
| `tls_cert_path` | `YANUGET_TLS_CERT_PATH` | path | *(self-signed)* | PEM certificate (chain). |
| `tls_key_path` | `YANUGET_TLS_KEY_PATH` | path | *(self-signed)* | PEM private key. |
| `enable_symbol_server` | `YANUGET_ENABLE_SYMBOL_SERVER` | bool | `true` | Accept `.snupkg` and serve PDBs. |
| `enable_web_ui` | `YANUGET_ENABLE_WEB_UI` | bool | `true` | Serve the HTML gallery. |
| `primary_client` | `YANUGET_PRIMARY_CLIENT` | string | `choco` | Install command shown first (`choco`/`dotnet`/`nuget`). |

Booleans accept `1/true/yes/on` (case-insensitive) via environment variables.

## Rate limiting

Per-client-IP request throttling under the `[rate_limit]` table. A fixed window
of `window_secs` allows at most `max_requests` requests per client IP; exceeding
it returns `429 Too Many Requests` with a `Retry-After` header. It is **on by
default** with a generous limit so ordinary restores are unaffected while online
API-key guessing is throttled. The client IP is taken from `X-Forwarded-For` /
`X-Real-IP` (behind a proxy) and otherwise the peer address; requests with no
determinable IP are not throttled. For very high read volume, raise the limit or
disable it and rely on a reverse proxy.

| TOML key | Env var | Type | Default | Description |
| --- | --- | --- | --- | --- |
| `rate_limit.enabled` | `YANUGET_RATELIMIT_ENABLED` | bool | `true` | Master switch. |
| `rate_limit.max_requests` | `YANUGET_RATELIMIT_MAX_REQUESTS` | int | `1000` | Max requests per IP per window (min 1). |
| `rate_limit.window_secs` | `YANUGET_RATELIMIT_WINDOW_SECS` | int | `60` | Window length in seconds. |

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

## Feeds

By default YANuget serves a single feed at the root (the implicit `default`
feed, using the global settings above). Add `[[feeds]]` blocks to host several
feeds; each is mounted under `/{name}` (e.g. `/stable/v3/index.json`) and the
root serves a feed index. **Feeds are configured in TOML only — there are no
`YANUGET_*` environment variables for them.**

A package version can belong to **many feeds at once**. The payload and metadata
are stored **once** (keyed by id/version); each feed holds only a *membership*
with its own mutable state (listed / enabled / pending / flagged / downloads).
Removing a version from a feed drops that membership; the shared payload is
deleted only when the **last** feed referencing it lets go.

| TOML key | Type | Default | Description |
| --- | --- | --- | --- |
| `feeds[].name` | string | *(required)* | URL slug + DB key; `[A-Za-z0-9._-]+`, unique. |
| `feeds[].api_key` | string | *(global `api_key`)* | Push (put) credential. |
| `feeds[].api_keys` | string[] | *(global)* | Additional push keys. A feed that sets any push key uses only its own. |
| `feeds[].read_api_key` | string | *(open)* | Download/restore (get) credential. See below. |
| `feeds[].admin_api_key` | string | *(global `admin_api_key`)* | Moderation/promotion (delete) credential. |
| `feeds[].allow_overwrite` | bool \| string | *(global)* | Re-push policy (`false`/`true`/`"prerelease-only"`). |
| `feeds[].hard_delete_enabled` | bool | *(global)* | DELETE removes vs. unlists. |
| `feeds[].requires_approval` | bool | `false` | Incoming versions are pending until approved. |
| `feeds[].promotes_to` | string | *(none)* | Next release ring (must name another feed). |
| `feeds[].mirror.enabled` | bool | `false` | Read-through cache of an upstream V3 feed. |
| `feeds[].mirror.upstream` | string | `https://api.nuget.org/v3/index.json` | Upstream service index. |
| `feeds[].mirror.timeout_secs` | int | `30` | Per-request upstream timeout. |
| `feeds[].mirror.auth.username` / `.password` | string | *(none)* | HTTP Basic credentials for the upstream. |
| `feeds[].mirror.auth.token` | string | *(none)* | Bearer token for the upstream (`Authorization: Bearer …`). |
| `feeds[].mirror.auth.headers` | table | `{}` | Arbitrary extra request headers (e.g. a private-feed API key). |
| `feeds[].license_policy.enabled` | bool | `false` | Evaluate the offline license policy. |
| `feeds[].license_policy.allowed` | string[] | `[]` | If non-empty, license must match one. |
| `feeds[].license_policy.blocked` | string[] | `[]` | Always rejected (even if also allowed). |
| `feeds[].license_policy.allow_unlicensed` | bool | `true` | Allow packages with no declared license. |
| `feeds[].license_policy.action` | string | `warn` | `warn` (accept + flag) or `block` (reject). |
| `feeds[].retention` | table | *(global `[retention]`)* | Per-feed retention overrides. |

### Read authentication

When a feed sets `read_api_key`, downloads/restore **and** the HTML gallery
require a credential, supplied either as an `X-NuGet-ApiKey` header or as the
password of HTTP Basic credentials (what `dotnet`/`nuget` send). The
`/v3/index.json` service index stays open so clients can discover the feed.

### Release rings & approval

`requires_approval = true` makes every version entering a feed (by push,
promotion or mirror) **pending** — withheld from clients until an operator
approves it in `/admin`. Combined with `promotes_to`, feeds form an ordered
promotion chain (e.g. `dev → stable`): an admin promotes a version into the next
ring, where it waits for approval if that ring gates. Feeds without
`promotes_to` are simply independent sets a version can be added to.

## Logging

Logging uses `tracing`. Control verbosity with `RUST_LOG`, e.g.:

```bash
RUST_LOG=info,yanuget=debug,tower_http=debug ./yanuget
```

The default is `info`.

## TLS

YANuget serves **HTTPS by default**. Behaviour:

- If `tls_cert_path` **and** `tls_key_path` are set, those PEM files are used.
- Otherwise a self-signed certificate is generated once and cached under
  `{data_dir}/tls/` (`cert.pem` + `key.pem`, the key written `0600`) and reused
  across restarts. Self-signed certificates are fine for getting started and for
  internal networks, but **clients must be told to trust them** (`dotnet`/`choco`
  reject untrusted certificates). For a public feed, supply a real certificate.
- Set `tls_enabled = false` to serve plain HTTP — appropriate when a reverse
  proxy (nginx, Caddy, Traefik) terminates TLS in front of YANuget. In that
  case forward `X-Forwarded-Proto`/`X-Forwarded-Host` so generated URLs use the
  right scheme/host.

When TLS is on and no base URL is configured, generated URLs default to the
`https` scheme (still overridable by `X-Forwarded-Proto`). With TLS enabled,
responses also carry a `Strict-Transport-Security` header (one year).

## Security notes

- **Always set `api_key` in production.** Without it, push and delete are open
  to anyone who can reach the server; YANuget logs a loud warning at startup in
  that case. The key is compared in constant time.
- The `/admin` area (set `admin_api_key`) and the gallery should only be exposed
  over HTTPS — keep TLS on, or terminate it at a proxy.

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
