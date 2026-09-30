# HTTP API reference

YANuget implements the [NuGet v3 protocol](https://learn.microsoft.com/en-us/nuget/api/overview).
All resource URLs are advertised by the **service index** so clients discover
them automatically; the paths below are the defaults YANuget serves.

Base URLs in responses are taken from `base_url` (`YANUGET_BASE_URL`) when it
is set, which is the recommended setup whenever the public address is known.
Otherwise they are derived per request from the `Host` header, and from
`X-Forwarded-Proto` / `X-Forwarded-Host` only when the connection comes from a
peer listed in [`trusted_proxies`](configuration.md#trusted-proxies); from
anyone else those headers are ignored.

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

## Attached files

Large files — disk images (`.wim`, `.swm`, `.esd`, `.iso`, `.vhd(x)`) and
archives — can be attached to a package version, for its install script to
fetch at install time. Not NuGet protocol. They are enabled by default and
configured under [`[files]`](configuration.md#attached-files).

A file belongs to one id/version and goes wherever the version goes: it is
visible in every feed that holds the version, and deleted with it — by an
admin, by a hard `DELETE`, or by retention. The bytes are stored once, under
their SHA-256, however many versions attach the same content.

### Download

```
GET|HEAD /files/{id}/{version}/{name}
GET      /files/{id}/{version}/index.json   # [{name, size, sha256, uploaded, url}]
```

Needs read access like any download, and a version the feed serves (not
disabled, not pending). The response is built for resumable clients:

- `Content-Length` on `HEAD` and `GET`, a fixed length rather than chunked
  encoding, and no compression.
- `Accept-Ranges: bytes`, single ranges answered with `206`, and `If-Range`
  honoured — a resume against changed content gets the whole file.
- A strong `ETag` (the SHA-256), `Last-Modified` (the upload time), and
  `Repr-Digest: sha-256=:…:` (RFC 9530). `Cache-Control: immutable`: a file's
  URL never serves different bytes, because a file of the same name cannot be
  replaced, only deleted.
- Always `Content-Type: application/octet-stream`, `Content-Disposition:
  attachment` and `Content-Security-Policy: default-src 'none'`, so no hosted
  file can render in a browser as this origin.

A download is counted once per transfer: a `GET` of the whole file, or of a
range from byte 0. BITS' `HEAD` and ranged continuations are not.

In a `chocolateyInstall.ps1` — the package page shows this with the real URL
and checksum, ready to copy:

```powershell
$file = Join-Path $env:TEMP 'base.wim'
Start-BitsTransfer -Source 'https://nuget.example.com/files/contoso.images/1.2.0/base.wim' -Destination $file
Get-ChecksumValid -File $file -Checksum '<sha256>' -ChecksumType sha256
```

`Invoke-WebRequest -Resume` (PowerShell 7) and `curl -C -` resume a partial file
the same way. On a feed with a `read_api_key`, BITS takes it as the password of
`-Credential` with `-Authentication Basic` (over HTTPS), or as
`-CustomHeaders 'X-NuGet-ApiKey: <key>'`.

### Upload in one request

```
PUT    /api/v2/files/{id}/{version}/{name}   # the body is the file
DELETE /api/v2/files/{id}/{version}/{name}
```

Both need the feed's push key in `X-NuGet-ApiKey`, **and** a push key must be
configured: a feed left open for package pushes answers `403` rather than take
multi-gigabyte files from anyone. The version must be in the feed (`404`).

The name is 1–128 characters of `A-Z a-z 0-9 . _ -`, starts with a letter or
digit, and has one of `[files].allowed_extensions`; anything else is `400`.
The body is streamed to disk and hashed on the way, and is limited by
`max_file_size_bytes` (`413`), `min_free_disk_bytes` (`507`) and
`upload_idle_timeout_secs` (`408`). An optional `X-Checksum-SHA256: <hex>` makes
a body with a different hash fail (`400`) and leave nothing behind.

The answer is `201` with `{name, size, sha256, url}`. The same file again is
`200` and changes nothing; different content under a name already attached is
`409` — delete it first.

### Resumable upload (tus)

```
OPTIONS /api/v2/uploads          # Tus-Version, Tus-Extension, Tus-Max-Size
POST    /api/v2/uploads          # start: 201 + Location
HEAD    /api/v2/uploads/{upload} # Upload-Offset: how much arrived
PATCH   /api/v2/uploads/{upload} # append at Upload-Offset
DELETE  /api/v2/uploads/{upload} # abandon
```

The [tus 1.0.0](https://tus.io/protocols/resumable-upload) protocol with the
creation, expiration and termination extensions, so stock clients work
(`tuspy`, `TusDotNetClient`, `tusc`). Every request carries `Tus-Resumable:
1.0.0` (`412` otherwise) and the push key, under the same rules as `PUT`.

`POST` takes `Upload-Length` and `Upload-Metadata` with `id`, `version`,
`filename` and optionally `sha256` (base64 values, as tus specifies). `PATCH`
sends `Content-Type: application/offset+octet-stream` and must start exactly at
the current offset (`409` with the right `Upload-Offset` otherwise); one request
at a time writes to an upload. Bytes that arrived before a connection dropped
count, so the client sends `HEAD`, then continues from there — also after a
server restart, when the first request re-reads what arrived to go on hashing.
The request that brings the last byte verifies the SHA-256 (discarding the
upload on a mismatch, `400`) and attaches the file.

Unfinished uploads expire after `[files].upload_expiry_hours` (`Upload-Expires`
says when) and are swept. [`scripts/Send-YanugetFile.ps1`](https://github.com/thoscut/yanuget/blob/main/scripts/Send-YanugetFile.ps1)
is a PowerShell 7 function that uploads resumably:

```powershell
. ./scripts/Send-YanugetFile.ps1
# The key comes from $env:YANUGET_API_KEY, from -ApiKey as a SecureString,
# or from a prompt.
Send-YanugetFile -Feed https://nuget.example.com `
    -Id Contoso.Images -Version 1.2.0 -Path .\base.wim
# after an interruption: the same call with -Resume <the URL it printed>
```

It sends the key only over https (`-AllowHttp` for a local test server) and
only to the scheme, host and port of `-Feed`: an upload URL anywhere else,
whether handed back by the server or passed as `-Resume`, is refused, and
redirects are not followed.

### Over SSH

`scp`, `sftp` and `rsync` reach the server through its own SSH daemon and an
inbox directory YANuget scans; see
[the inbox](configuration.md#the-ssh-inbox).

## Web gallery (HTML)

```
GET /                                  # searchable package list
GET /packages?q=&skip=&take=&sort=&tag= # same, as a search page
GET /tags                              # every tag, sized by how many packages use it
GET /packages/{id}                     # detail for the newest version
GET /packages/{id}/{version}           # detail for a specific version
GET /packages/{id}/{version}/icon      # the package's embedded icon
GET /stats                             # feed-wide statistics
GET /settings                          # read-only policy overview
```

Human-facing HTML (not part of the NuGet protocol). The header has a search box
(submitting to `/packages?q=`). The list is sorted by `sort`: `downloads` (the
default, the same ranking `/v3/search` gives clients), `name` (A to Z) or
`updated` (the package whose newest version was published last comes first); an
unknown value falls back to the default. `tag` narrows the list (and a search)
to packages with that tag, case-insensitively; a value no tag could be — empty,
over 64 characters, or containing whitespace — is ignored. Paging, the page-size
form and a new search keep the chosen order and tag. Every tag shown links to
its filtered list, and the landing page offers the most used ones. The detail
page links the `.nupkg` itself (from the flat-container endpoint, so read auth,
ranges and caching apply as for a client) and shows versions, dependencies,
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

```
GET /_assets/fonts/atkinson-hyperlegible-next-2.001-latin-wght.woff2
```

The gallery's one font, embedded in the binary and served from the root (not
per feed), with a one-year immutable cache. Its name carries the font's version,
so a different font arrives under a different URL. The gallery's policy allows
fonts from this origin only (`font-src 'self'`).

## Admin area (HTTP Basic auth)

Mounted only when `admin_api_key` is set; protected by HTTP Basic auth (any
username, the admin key as the password). Lets an operator moderate versions
from the browser.

```
GET  /admin                                       # dashboard: all package ids
GET  /admin/packages/{id}                          # versions + actions
POST /admin/packages/{id}                          # one action on several versions
POST /admin/packages/{id}/{version}/disable        # withhold a version
POST /admin/packages/{id}/{version}/enable         # restore a disabled version
POST /admin/packages/{id}/{version}/delete         # remove from this feed
POST /admin/packages/{id}/{version}/approve        # clear the pending gate
POST /admin/packages/{id}/{version}/promote        # add to the next ring
POST /admin/packages/{id}/{version}/pin            # keep it from retention
POST /admin/packages/{id}/{version}/unpin          # let retention decide again
POST /admin/packages/{id}/{version}/files/{name}/delete  # detach a file
GET  /admin/retention                              # rules, last run, next cleanup's plan
POST /admin/retention/run                          # delete exactly the plan shown
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

`POST /admin/packages/{id}` applies one action to every version it names — how a
whole package is disabled, deleted or moved. The form fields are `op` (`enable`,
`disable`, `approve`, `pin`, `unpin`, `delete`, `copy` or `move`), one `v` per
version, and for `copy`/`move` a `target` feed:

```
curl -u admin:$ADMIN_KEY -X POST -H "X-CSRF-Token: $TOKEN" \
     -d "op=move&target=stable&v=1.0.0&v=1.1.0" \
     https://host/dev/admin/packages/foo
```

Every named version is checked before anything changes, so one this feed does
not hold (`404`) or a target whose license policy refuses one (`403`) leaves the
feed as it was. `copy` adds the versions to the target, as `promote` does;
`move` also removes them from this feed and keeps each one's listed and enabled
state. The files are never touched by either, since the target holds them
afterwards. The target applies its own approval gate and license policy, as for
a push. With no version named, the request changes nothing and redirects back.

Copying or moving **into** a feed takes that feed's admin key: the request must
carry credentials that are valid for the target as well, unless the target is
this feed's `promotes_to`, which the configuration already trusts this feed's
admin to fill. Otherwise it returns `400`. Feeds that share one admin key (the
global `admin_api_key`) can therefore hand versions to each other; feeds with
keys of their own cannot, without both.

`pin` keeps a version from retention: it is never pruned, and it does not use
up one of the "newest *N*" the rules keep. It survives an overwriting push and
a move, and does not stop an explicit delete.

`/admin/retention` shows the feed's rules, the last cleanup since the server
started, and every version the next cleanup would delete, with the reason and
the space it frees. Its button posts `plan`, a fingerprint of that list, to
`/admin/retention/run`. The plan is recomputed and applied only if the
fingerprint still matches: otherwise nothing is deleted and the page shows the
new list (`?changed=1`), so a push between looking and clicking cannot widen
what the click deletes. A cleanup already running — the scheduled sweep, or
another admin — is not queued behind (`?busy=1`). The run is refused (`400`)
unless `retention.enabled` is on and a limit is set.

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
