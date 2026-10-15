# Troubleshooting

When a client fails, the status code it prints says most of what went wrong.
YANuget sends the reason for every `4xx` as the HTTP reason phrase, which is the
part `dotnet`, `nuget` and `choco` print, so read the whole line. A `5xx` shows a
generic message to the client and the cause in the server log
([Logging](configuration.md#logging)).

## By status code

| Status | Usual cause | What to do |
| --- | --- | --- |
| `400` | The package, its `.nuspec`, the id or the version is malformed; or a symbol package's PDBs do not match the assemblies in its package. | The message names the problem. Versions must be ones NuGet itself accepts; see [Push a package](api.md#push-a-package). |
| `401` | No API key or the wrong one; on a read-gated feed, no read credential. | Check which key the action needs: push key to push, read key to restore, admin key for `/admin`. See [Keys per feed](feeds.md#keys-per-feed). |
| `403` | A license policy with `action = "block"`; an id under another feed's `reserved_id_prefixes`; a browser request marked cross-site. | Push to the feed that reserves the prefix, or adjust the policy. |
| `404` on restore | The version is disabled or still pending in this feed, or not in this feed at all. | Look the package up in the feed's admin area. Approve or enable it, or promote it into this feed. |
| `404` on a symbol push | The package the symbols belong to has not been pushed. | Push the `.nupkg` first; `dotnet nuget push` does this in the right order. |
| `408` | An upload sent nothing for `upload_idle_timeout_secs` (300 s by default). | Look for a stalled network or proxy; raise the timeout if the client pauses legitimately. |
| `409` | The version already exists and overwriting is off, or another feed already stores different content under the same id and version. | Push a new version (for Chocolatey, a package fix version such as `1.2.3.20261015`), or delete the old one for good first. |
| `413` | The package is larger than `max_package_size_bytes`, or a reverse proxy's own body limit. | Raise the limit; behind nginx set `client_max_body_size 0;`. See [Reverse proxy](configuration.md#reverse-proxy). |
| `421` | `base_url` is set and the request named another host. | Use the host in `base_url`, or add the name to `allowed_hosts`. Behind a proxy, forward the original `Host`. See [Host validation](configuration.md#host-validation). |
| `429` | The per-IP rate limit was exceeded. NuGet does not retry, so the restore fails. | Behind a proxy, list it in `trusted_proxies`, otherwise every client shares the proxy's bucket. Raise `rate_limit.max_requests` for large CI farms behind one address. |
| `507` | The upload would leave less than `min_free_disk_bytes` (2 GiB by default) free. | Free space on the storage volume, or lower the threshold. |

## Common problems

### "The SSL connection could not be established"

The client does not trust the server's certificate. With the default
self-signed certificate, see [Trusting the default certificate](clients.md#trusting-the-default-certificate).
If the certificate is trusted but the name does not match, the certificate was
generated before `base_url` was set: delete `{data_dir}/tls/` and restart.

### Restore works, but URLs point at the wrong host or `http://`

The URLs the service index hands out come from `base_url`, or, without it, from
the request. Set `base_url` to the public address. Behind a TLS-terminating
proxy, also list the proxy in `trusted_proxies` so its `X-Forwarded-Proto` is
honoured; see [Reverse proxy](configuration.md#reverse-proxy).

### A pushed package does not show up

* In a feed with `requires_approval`, it is pending until approved in `/admin`.
* Search hides unlisted versions; restore by exact version still works.
* With several feeds, check that the client pushed to the feed it reads from:
  every feed has its own URL (`/{feed}/v3/index.json`).

### A promoted version does not appear in the next feed

Promotion adds the version to the feed named in `promotes_to`. If that feed has
`requires_approval`, the version waits there as pending; approve it in that
feed's admin area. A version that was unlisted or disabled in the source feed
keeps that state. See [Release rings](feeds.md#release-rings).

### The mirror does not return a package

* Mirroring is best-effort: an upstream outage, a timeout or a version over
  `max_package_size_bytes` is a cache miss, logged on the server.
* A version deleted from the mirror feed is never fetched again; push it to
  the feed to bring it back.
* Only the newest `max_versions_per_package` versions (50) are fetched on a
  miss, plus the one the client asked for.
* An upstream on a private address needs `allow_private_upstream = true`.

See [Configuration → Feeds](configuration.md#feeds) for the limits.

### The server refuses to start

The error names the setting. Typical causes: an environment variable that does
not parse (`YANUGET_PORT=5k`, `YANUGET_TLS_ENABLED=maybe`), only one of
`tls_cert_path` / `tls_key_path`, a `promotes_to` naming a feed that does not
exist, a feed named after one of the server's own paths, or a `[feeds.dev.mirror]`
table instead of `[feeds.mirror]`. A database written by a newer release is
refused by an older one; restore the backup taken before the upgrade.

### Admin actions answer `400`

Admin `POST`s need the CSRF token the admin page embeds. Scripts must send it as
`X-CSRF-Token`; see [CSRF](api.md#csrf). Copying or moving into a feed other
than this feed's `promotes_to` target also needs credentials valid for that
feed.

## Getting more detail

```bash
RUST_LOG=info,yanuget=debug,tower_http=debug yanuget --config yanuget.toml
```

logs each request with its status. The `/settings` page shows the policy each feed is actually running with.
