# Security policy

## Supported versions

YANuget is pre-1.0. Only the latest released version receives security fixes.

| Version | Supported |
| --- | --- |
| 0.8.x | ✅ |
| < 0.8 | ❌ |

The table is for the server, `yanuget`, and the `yanuget-pkg` library it is
built with, which is fixed in the same releases. The internalizer has no
release yet; see [The internalizer](#the-internalizer).

## Reporting a vulnerability

**Please do not open a public issue for a security problem.**

Report it privately through GitHub's
[private vulnerability reporting](https://github.com/thoscut/yanuget/security/advisories/new)
form. If that is unavailable to you, open a normal issue that says only "security
report, requesting a private channel" — with no details — and you will be pointed
at one.

Useful things to include, as far as you have them:

- the version (or commit) and how the server is configured — TLS on or off, behind
  a proxy or not, which feeds/mirroring/admin features are enabled;
- what an attacker gains, and what access they need to start;
- a reproduction: a request, a crafted `.nupkg`, or a short script.

You can expect an acknowledgement within a few days and an assessment within two
weeks. Fixes are released as a new patch version with a GitHub Security Advisory,
crediting you unless you ask otherwise.

## Scope

YANuget hosts and serves packages, so the interesting failures are usually not
"the server crashed" but "the server handed a client something it should not
have". In scope, and taken seriously:

- **Anything that makes the feed a hazard to its clients.** Serving a payload
  whose content does not match the hash or the identity it is published under;
  making a client resolve a package from somewhere other than this server;
  content served to a browser that runs as script.
- **Authentication and authorisation.** Push, delete, promote, approve, or read
  from a feed without the key that should be required; one feed's key working on
  another feed; the admin area reachable without the admin key.
- **Reading outside the package store**, whether via a path in an archive entry,
  a URL, a symbol key or a configured path.
- **Requests the server is tricked into making** on an attacker's behalf, notably
  through mirror upstream URLs.
- **Denial of service from a single well-formed-looking request** — a crafted
  archive or manifest that consumes memory or CPU out of proportion to its size.
  Note that being able to *fill the disk* by pushing large packages is what the
  API key and `max_package_size_bytes` are for, not a vulnerability.
- **Secrets leaking** into responses, logs, the gallery or the settings page.

Out of scope:

- Findings that require an API key, admin key, or filesystem/database access
  that the attacker is not supposed to have — those are the trust boundary, not
  a way through it.
- The default self-signed TLS certificate being untrusted. That is deliberate,
  documented, and exists so the server is not plaintext out of the box; supply a
  real certificate for production.
- Volumetric denial of service (flooding). Use the built-in rate limiter and a
  reverse proxy.
- Vulnerabilities in the *packages you host*. YANuget stores and serves what it
  is given; it does not audit package contents.
- Missing hardening headers on endpoints that serve no HTML.

### The internalizer

The repository also holds `yanuget-internalize` (`crates/yanuget-internalize`),
a separate command-line tool that copies Chocolatey packages into a YANuget
feed: it runs each package's install script in an offline Windows sandbox to
record what the script downloads, fetches those files itself, attaches them to
the feed, and pushes a copy of the package whose script installs from the feed
instead of the internet. **It is not released and not usable yet** (version
0.0.0, not published, not in the release archives or the image), so it has no
supported version; it is listed here so that reports about it are welcome
anyway, and so the boundaries are clear before it ships.

It treats everything that comes out of a package as untrusted: the package
itself, what its scripts do inside the sandbox, the URLs they record, and the
files behind them. In scope:

- **Requests it is tricked into making**: a recorded URL, a redirect or an
  upstream feed that makes it reach a private, loopback or link-local address
  it was not told to allow, send credentials to another origin, or downgrade
  from https to http.
- **An internalized package that still reaches the internet**, or installs
  something other than the file that was downloaded and hashed, when the
  package's script goes through Chocolatey's download helpers.
- **Misleading the reviewer**: a report, file name or admin-area page that
  shows an approver something other than what the package will install.
- **Writing outside its working directories** through a file name, an archive
  entry or a path in the capture.

Out of scope for it, as for the server: findings that need the worker's push
key or access to the worker host, and escapes from the virtualisation itself
(QEMU, Hyper-V), which belong to those projects. The full threat model, with the
worker as its own trust zone holding a push key for the intake feed only, comes
with the tool's documentation (`INT-34` in [ROADMAP.md](ROADMAP.md)).

## Running YANuget safely

Configuration choices that matter most:

- **Set `api_key`.** Without one, anybody who can reach the server can push and
  delete.
- **Set `trusted_proxies` when, and only when, you run behind a proxy.** It is
  empty by default, so forwarding headers are ignored — which is what keeps a
  client from steering the URLs handed to other clients, or rotating
  `X-Forwarded-For` to walk through the rate limiter. Behind a proxy, list that
  proxy's address, and have it set `X-Forwarded-Host` itself rather than pass
  on the client's (see [Host validation](docs/configuration.md#host-validation)).
- **Set `cors_allowed_origins` only if a browser really needs cross-origin
  access.** It is empty by default, so no CORS headers are sent. `*` makes the
  feed's whole inventory readable by any page a user with network reach
  visits. Cross-origin access is read-only either way.
- **Set `base_url`** whenever you know the public address, and always behind a
  proxy, rather than relying on forwarded headers. It also restricts the server
  to that host name (plus any `allowed_hosts`), which is what defeats DNS
  rebinding against an intranet feed.
- **Use a real certificate** (`tls_cert_path`/`tls_key_path`), or terminate TLS
  at a proxy and set `tls_enabled = false`.
- **Set `max_package_size_bytes`** on any feed open to more than a few people.
- **Keep `admin_api_key` distinct** from the push key, and do not expose `/admin`
  to the internet.
- **Gate reads with a `[[feeds]]` entry.** `read_api_key` is a per-feed
  setting; the implicit single feed at the root has none, so without
  `[[feeds]]` anyone who can reach the server can restore from it.
- **Remember that an id and version are one package across all feeds.** The
  first feed to store a version fixes its content for every other feed, and a
  push to one feed can pre-empt another. Feeds separate who may push and read,
  not what a name means.
- **Review `[feeds.mirror]` upstreams.** A mirror makes your server fetch
  from, and republish under your name, whatever that upstream serves.
- **Pull a bad version from a mirror feed by deleting or disabling it.** A
  deleted version is recorded and never fetched from the upstream again; a
  disabled one stays in the feed, withheld.

## Verifying a release

Release archives, `SHA256SUMS` and the container image of every release from
0.6.0 on carry signed build provenance from the release workflow, and the image
of every release from 0.7.0 on is also signed with cosign (keyless). Check them
before you run them:

```bash
# An archive (or SHA256SUMS) downloaded from the GitHub Release.
gh attestation verify yanuget-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz \
  --repo thoscut/yanuget

# The container image: provenance, then the cosign signature.
gh attestation verify oci://ghcr.io/thoscut/yanuget:X.Y.Z --repo thoscut/yanuget
cosign verify ghcr.io/thoscut/yanuget:X.Y.Z \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity-regexp '^https://github\.com/thoscut/YANuget/\.github/workflows/release\.yml@refs/tags/v'
```

`cosign verify` needs **cosign 3 or newer**: the signature is stored as a
Sigstore bundle, which older versions do not look for (cosign 2.5 reports "no
signatures found" for an image that is signed).

Either check failing means the file or image did not come from this
repository's release workflow run for a tag; do not use it.
