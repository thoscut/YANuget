# HTTP API reference

YANuget implements the [NuGet v3 protocol](https://learn.microsoft.com/en-us/nuget/api/overview).
All resource URLs are advertised by the **service index** so clients discover
them automatically; the paths below are the defaults YANuget serves.

Base URLs in responses are derived per-request from the `Host` /
`X-Forwarded-Proto` / `X-Forwarded-Host` headers, or taken from
`YANUGET_BASE_URL` when set.

## Feeds and path prefixes

With no `[[feeds]]` configured, a single feed is served at the root and every
path below is exactly as shown. When multiple feeds are configured, **each path
is prefixed with the feed name** — e.g. `/stable/v3/index.json`,
`/stable/api/v2/package`, `/stable/admin` — and the root `/` serves an HTML feed
index. The service index advertises feed-prefixed resource URLs, so a client
pointed at `/{feed}/v3/index.json` discovers the right paths automatically.

If a feed sets `read_api_key`, the read endpoints (flat container, registration,
download, search, autocomplete) and the HTML gallery require a credential —
sent as an `X-NuGet-ApiKey` header or HTTP Basic password — and return `401`
without it. The service index itself stays open for discovery.

## Service index

```
GET /v3/index.json
```

Returns `{ "version": "3.0.0", "resources": [...] }`. Advertised resource
`@type`s include `PackageBaseAddress/3.0.0`, `RegistrationsBaseUrl` (and the
SemVer2 aliases), `SearchQueryService`, `SearchAutocompleteService`,
`PackagePublish/2.0.0`, `SymbolPackagePublish/4.9.0`, and `SymbolServer/4.9.0`.

## Push a package

```
PUT /api/v2/package
X-NuGet-ApiKey: <key>
Content-Type: multipart/form-data        (raw body also accepted)
```

The `.nupkg` is streamed to disk, its `.nuspec` is read, and metadata is
indexed. Responses:

| Status | Meaning |
| --- | --- |
| `201 Created` | Package indexed. |
| `400 Bad Request` | Malformed package / nuspec / id / version. |
| `401 Unauthorized` | Missing or wrong API key. |
| `403 Forbidden` | Rejected by the feed's `license_policy` (`action = "block"`). |
| `409 Conflict` | Version already exists in this feed (unless `allow_overwrite`). |
| `413 Payload Too Large` | Exceeds `max_package_size_bytes`. |

In a feed with `requires_approval = true`, a pushed version is still accepted
(`201`) but lands **pending** — withheld from clients until an admin approves it.
Under `license_policy` with `action = "warn"`, a violating package is accepted
and **flagged** (shown in `/admin`) rather than rejected.

For a mirror-enabled feed, a read miss on the flat container, registration or
download triggers a best-effort read-through fetch from the upstream feed, which
is streamed to disk and indexed locally (honouring the feed's approval gate and
license policy); an upstream failure simply degrades to a normal `404`.

## Delete / unlist / relist

```
DELETE /api/v2/package/{id}/{version}     # unlist (default) or hard-delete
POST   /api/v2/package/{id}/{version}     # relist
```

`DELETE` returns `204 No Content`. By default it **unlists** (hides from search,
still restorable by exact version) — the same semantics as the NuGet client's
`delete`. With `hard_delete_enabled = true` it removes the package and files.
`POST` relists, returning `200 OK`. Both require the API key.

## Package content (flat container)

```
GET /v3/package/{id}/index.json
```

`{ "versions": ["1.0.0", "1.1.0", ...] }` — lower-cased, normalized versions,
ascending. `404` if the id is unknown.

**Unlisted versions are included.** This endpoint is how a client resolves a
version it is about to restore, so omitting them would make a project pinned to
an unlisted version fail with `NU1101` — which is precisely the difference
between unlisting and deleting. Unlisted versions are still hidden from
`/v3/search`, and registration reports them with `"listed": false`.

Admin-**disabled** and still-**pending** versions are excluded here, as they are
everywhere else: those are withheld from clients outright.

```
GET /v3/package/{id}/{version}/{id}.{version}.nupkg
```

Streams the `.nupkg`. Supports `Range: bytes=...` (responds `206 Partial
Content` with `Content-Range`); always sends `Accept-Ranges: bytes`. Each
successful fetch increments the download counter.

A published id/version is immutable, so the response carries a strong `ETag`
(the package's SHA-512 — a content hash of exactly the bytes served) and
`Cache-Control: public, max-age=31536000, immutable`. Repeating the request with
`If-None-Match` returns `304 Not Modified` with an empty body, so a client that
already holds a multi-gigabyte package pays for a header exchange rather than
the payload.

```
GET /v3/package/{id}/{version}/{id}.nuspec
```

Returns the package's `.nuspec` manifest as `application/xml`.

## Registration

NuGet exposes **two registration hives** and a client picks one from the service
index:

| Hive | Path | `@type`s | Contents |
| --- | --- | --- | --- |
| SemVer1 | `/v3/registration/` | `RegistrationsBaseUrl`, `…/3.0.0-beta`, `…/3.0.0-rc`, `…/3.4.0` | Only versions a pre-SemVer2 client can parse |
| SemVer2 | `/v3/registration-semver2/` | `…/3.6.0`, `…/Versioned` | Every version |

A version is SemVer2 when it carries build metadata or more than one
dot-separated pre-release identifier (`2.0.0-alpha.1`, `3.0.0+build`). Those are
withheld from the SemVer1 hive — advertising a single hive under both sets of
`@type`s would hand an older client versions it chokes on. Each hive's documents
keep their self-references inside that hive, and a SemVer2-only version returns
`404` from a SemVer1 leaf. `/v3/search` links results into the hive matching the
request's `semVerLevel`.


```
GET /v3/registration/{id}/index.json
```

A registration index containing every version (listed and unlisted, with a
`listed` flag) and full `catalogEntry` metadata, including `dependencyGroups`.
Unlisted versions additionally report `published` in the year 1900, per NuGet
convention. `dependencyGroups` is omitted when a version has no dependencies.
`404` if unknown.

Packages with **fewer than 128 versions** get a single inlined page — the
whole registration in one response. At 128 versions or more the index instead
lists external pages of 64 versions each, without inline `items`, and the
client follows each page's `@id`:

```
GET /v3/registration/{id}/page/{lower}/{upper}.json
```

This matches how nuget.org pages large registrations, and is the reason a
client may issue page requests you did not expect for a heavily versioned
package. It has nothing to do with package *size*.

```
GET /v3/registration/{id}/{version}.json
```

A single registration leaf for one version.

## Search

```
GET /v3/search?q=&skip=&take=&prerelease=&semVerLevel=&packageType=
```

| Param | Default | Notes |
| --- | --- | --- |
| `q` | *(empty = all)* | Matches id, description, tags, title. |
| `skip` | `0` | Pagination offset (over package ids). |
| `take` | `20` | Clamped to `1000`. |
| `prerelease` | `false` | Include pre-release versions. |
| `semVerLevel` | `1` | `2.0.0` includes SemVer2 packages. |
| `packageType` | *(any)* | Filters the returned page by package type. |

Returns `{ "@context": {...}, "totalHits": N, "data": [...] }`. Each hit groups
all matching versions of one package id and is ranked by total downloads.

## Autocomplete & version enumeration

```
GET /v3/autocomplete?q=&take=&skip=          # id autocomplete
GET /v3/autocomplete?id={id}&prerelease=     # versions of one package
```

Both return `{ "@context": {...}, "totalHits": N, "data": [...] }` — a list of
package ids, or of versions when `id` is supplied.

## Symbol server

```
PUT /api/v2/symbol
X-NuGet-ApiKey: <key>
Content-Type: multipart/form-data        (raw body also accepted)
```

Streams a `.snupkg` to disk, reads its `.nuspec` to identify the owning package
(which **must already exist** — `404` otherwise), then extracts every **Portable
PDB** and indexes it by its SSQP key. Responses mirror package push (`201`,
`400`, `401`, `404`, `413`). Requires `enable_symbol_server` (on by default).
Native (Windows) PDBs are stored within the `.snupkg` but cannot be indexed.

```
GET /download/symbols/{file}/{key}/{file}
```

Serves an indexed PDB to a debugger (the Simple Symbol Query Protocol path).
`{key}` is the upper-case `{GUID}{age}` signature; for Portable PDBs the age is
`FFFFFFFF`. Streams with `Range` support. `404` for an unknown file/key so the
debugger falls through to the next symbol source.

## Web gallery (HTML)

```
GET /                                  # searchable package list
GET /packages?q=&skip=&take=           # same, as a search page
GET /packages/{id}                     # detail for the newest version
GET /packages/{id}/{version}           # detail for a specific version
GET /packages/{id}/{version}/icon      # the package's embedded icon
GET /stats                             # feed-wide statistics
GET /settings                          # read-only policy overview
```

Human-facing HTML (not part of the NuGet protocol). The header has a search box
(submitting to `/packages?q=`). The detail page shows versions, dependencies,
links, readme, symbol availability, and the install command for Chocolatey /
`dotnet` / `nuget.exe` (ordered by `primary_client`). `/stats` shows feed totals
(packages, versions, downloads, storage, symbol files) plus the most-downloaded
and most-recently-published lists. `/settings` summarises the feed's policy
(auth mode incl. download auth, size/overwrite/delete behaviour, approval &
promotion ring, upstream mirror, license policy, symbol server, retention) and
never exposes the API key or storage paths. All of these honour the feed's
`read_api_key` when one is set.

`/packages/{id}/{version}/icon` serves the icon embedded in the package. Those
bytes come from an uploaded `.nupkg`, so the content type is decided by
**sniffing the bytes** rather than by trusting any declared name, and only
raster formats (PNG, JPEG, GIF, WebP, BMP, ICO) are recognised — an "icon" that
is really an SVG, which is a script-bearing document, returns `404` instead. The
response also carries its own `Content-Security-Policy: default-src 'none'`.

Requires `enable_web_ui` (on by default); when disabled, `/` serves a minimal
info page and `/packages/*` return `404`.

## Admin area (HTTP Basic auth)

Mounted only when `admin_api_key` is set; protected by HTTP Basic auth (any
username, the admin key as the password). Lets an operator moderate versions
from the browser.

```
GET  /admin                                       # dashboard: all package ids
GET  /admin/packages/{id}                          # versions + actions
POST /admin/packages/{id}/{version}/disable        # withhold a version
POST /admin/packages/{id}/{version}/enable         # restore a disabled version
POST /admin/packages/{id}/{version}/delete         # remove from this feed
POST /admin/packages/{id}/{version}/approve        # clear the pending gate
POST /admin/packages/{id}/{version}/promote        # add to the next ring
```

A **disabled** version is withheld from clients entirely — hidden from search,
registration and the flat container, **and** not downloadable (`404`) — until an
admin re-enables it. This is stronger than the NuGet client's *unlist* (which
keeps a version downloadable for restore). The admin page also surfaces
**pending** versions (awaiting approval) and **flagged** versions (license-policy
violations under `warn`).

`delete` removes the version from **this feed**; when no other feed references
it, the shared payload, sidecars and indexed symbols are hard-deleted too.
`approve` clears the pending gate on a version in a `requires_approval` feed.
`promote` adds the version to the feed named by this feed's `promotes_to`
(landing pending if that ring also gates) — the release-ring step; it requires
the version to be a member of the current feed. The POST actions return
`303 See Other` back to the package page; without credentials they return `401`
with a `WWW-Authenticate: Basic` challenge.

### CSRF

The POST actions additionally require a CSRF token. Browsers replay HTTP Basic
credentials automatically on any request to this origin, so authentication alone
does not distinguish a click in `/admin` from a form auto-submitted by a page on
someone else's site — without this, a signed-in operator merely visiting a
hostile page would be enough to delete packages.

The token is derived from the admin key, so producing it requires already
knowing that key. The admin page embeds it in every form as a hidden `_csrf`
field; a scripted caller may instead send it as an `X-CSRF-Token` header:

```
curl -u admin:$ADMIN_KEY -X POST \
     -H "X-CSRF-Token: $TOKEN" \
     https://host/admin/packages/foo/1.0.0/disable
```

A request without the token, or one a browser labels `Sec-Fetch-Site:
cross-site`, returns `400`. Admin POST bodies are capped at 64 KiB.

## Health

```
GET /health         → 200 "OK"   (readiness: also probes the database)
GET /health/ready   → same as /health
GET /health/live    → 200 "OK"   (liveness: this process only)
```

`/health` returns `503` with `database unavailable` when the store cannot be
reached — the case an orchestrator has to act on, and one a static `OK` would
hide. `/health/live` touches nothing else, so a slow dependency cannot trigger
a restart loop through it.

## Response headers

Every response carries `X-Content-Type-Options: nosniff`,
`X-Frame-Options: DENY`, `Referrer-Policy: no-referrer` and
`Vary: Host, X-Forwarded-Host, X-Forwarded-Proto` — protocol documents embed
absolute URLs derived from those inputs, so a shared cache must key on them.
With TLS terminated by YANuget itself, responses also carry
`Strict-Transport-Security`.

Gallery pages carry a `Content-Security-Policy` of `default-src 'none'` whose
only permitted inline style and script are the two the server itself emits,
pinned by SHA-256. The embedded documentation site sets its own, looser policy
(mkdocs emits inline bootstrap code this crate does not control).

## Error bodies

Errors return the appropriate status with a JSON body `{ "error": "<message>" }`.

`4xx` messages describe what the caller did wrong and are safe to act on. `5xx`
responses return a generic `internal server error`: the underlying I/O,
SQL or upstream detail would otherwise disclose filesystem paths, queries and
upstream URLs, so it goes to the server log only.
