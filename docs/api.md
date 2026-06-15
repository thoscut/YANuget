# HTTP API reference

YANuget implements the [NuGet v3 protocol](https://learn.microsoft.com/en-us/nuget/api/overview).
All resource URLs are advertised by the **service index** so clients discover
them automatically; the paths below are the defaults YANuget serves.

Base URLs in responses are derived per-request from the `Host` /
`X-Forwarded-Proto` / `X-Forwarded-Host` headers, or taken from
`YANUGET_BASE_URL` when set.

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
| `409 Conflict` | Version already exists (unless `allow_overwrite`). |
| `413 Payload Too Large` | Exceeds `max_package_size_bytes`. |

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

`{ "versions": ["1.0.0", "1.1.0", ...] }` — lower-cased, normalized, listed
versions, ascending. `404` if the id is unknown.

```
GET /v3/package/{id}/{version}/{id}.{version}.nupkg
```

Streams the `.nupkg`. Supports `Range: bytes=...` (responds `206 Partial
Content` with `Content-Range`); always sends `Accept-Ranges: bytes`. Each
successful fetch increments the download counter.

```
GET /v3/package/{id}/{version}/{id}.nuspec
```

Returns the package's `.nuspec` manifest as `application/xml`.

## Registration

```
GET /v3/registration/{id}/index.json
```

A registration index with a single inlined page containing every version
(listed and unlisted, with a `listed` flag) and full `catalogEntry` metadata,
including `dependencyGroups`. Unlisted versions additionally report
`published` in the year 1900, per NuGet convention. `dependencyGroups` is
omitted when a version has no dependencies. `404` if unknown.

> All versions are currently inlined into one page. nuget.org pages packages
> with ≥128 versions into pages of 64; YANuget does not yet do this, which is
> only relevant for packages with an extreme number of versions (not large
> package *size*). See the roadmap.

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
GET /stats                             # feed-wide statistics
GET /settings                          # read-only policy overview
```

Human-facing HTML (not part of the NuGet protocol). The header has a search box
(submitting to `/packages?q=`). The detail page shows versions, dependencies,
links, readme, symbol availability, and the install command for Chocolatey /
`dotnet` / `nuget.exe` (ordered by `primary_client`). `/stats` shows feed totals
(packages, versions, downloads, storage, symbol files) plus the most-downloaded
and most-recently-published lists. `/settings` summarises the feed's policy
(auth mode, size/overwrite/delete behaviour, symbol server, retention) and never
exposes the API key or storage paths.
Requires `enable_web_ui` (on by default); when disabled, `/` serves a minimal
info page and `/packages/*` return `404`.

## Health

```
GET /health      → 200 "OK"
```

## Error bodies

Errors return the appropriate status with a JSON body `{ "error": "<message>" }`.
