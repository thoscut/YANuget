# Security policy

## Supported versions

YANuget is pre-1.0. Only the latest released version receives security fixes.

| Version | Supported |
| --- | --- |
| 0.1.x | ✅ |
| < 0.1 | ❌ |

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

## Running YANuget safely

Configuration choices that matter most:

- **Set `api_key`.** Without one, anybody who can reach the server can push and
  delete.
- **Set `trusted_proxies` when, and only when, you run behind a proxy.** It is
  empty by default, so forwarding headers are ignored — which is what keeps a
  client from steering the URLs handed to other clients, or rotating
  `X-Forwarded-For` to walk through the rate limiter. Behind a proxy, list that
  proxy's address.
- **Set `cors_allowed_origins` only if a browser really needs cross-origin
  access.** It is empty by default, so no CORS headers are sent. `*` makes the
  feed's whole inventory readable by any page a user with network reach
  visits.
- **Set `base_url`** when the server is behind a proxy, rather than relying on
  forwarded headers, if you can.
- **Use a real certificate** (`tls_cert_path`/`tls_key_path`), or terminate TLS
  at a proxy and set `tls_enabled = false`.
- **Set `max_package_size_bytes`** on any feed open to more than a few people.
- **Keep `admin_api_key` distinct** from the push key, and do not expose `/admin`
  to the internet.
- **Review `[feeds.mirror]` upstreams.** A mirror makes your server fetch
  from, and republish under your name, whatever that upstream serves.
