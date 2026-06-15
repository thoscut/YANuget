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
`PackagePublish/2.0.0`, and `SymbolPackagePublish/4.9.0`.

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

## Registration

```
GET /v3/registration/{id}/index.json
```

A registration index with a single inlined page containing every version
(listed and unlisted, with a `listed` flag) and full `catalogEntry` metadata,
including `dependencyGroups`. `404` if unknown.

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

## Health

```
GET /health      → 200 "OK"
```

## Error bodies

Errors return the appropriate status with a JSON body `{ "error": "<message>" }`.
