# Feeds and release rings

One YANuget server can host several feeds: a feed per team, a gated `stable`
feed that only approved builds reach, a cache of nuget.org. This page walks
through setting that up. Every key it uses is listed in
[Configuration → Feeds](configuration.md#feeds).

## One feed or several

Without `[[feeds]]` in the configuration, the server has one feed, called
`default`, served at the root (`/v3/index.json`). That is all most setups need.

As soon as the configuration declares `[[feeds]]`, each one is mounted under its
name (`/dev/v3/index.json`, `/stable/v3/index.json`, likewise its gallery at
`/dev/` and its admin area at `/dev/admin`), and `/` lists the feeds. Feeds are
configured in the TOML file only; there are no environment variables for them.
A feed name is made of `A-Z a-z 0-9 . _ -` and may not be one of the server's
own path segments (`admin`, `api`, `docs`, `files`, `health`, `packages`,
`tags`, `v3` and a few more).

## Versions are shared, memberships are not

A package version is stored **once**, however many feeds hold it. Each feed
holds a *membership* of it with its own state: listed or unlisted, enabled or
disabled, pending or approved, flagged, its own download count, pinned or not.
Promoting, copying or moving a version between feeds adds or removes a
membership; the `.nupkg` is never copied. Its payload is deleted only when the
last feed lets go of it.

The flip side is that an id and version mean the same bytes everywhere. A push
of *different* content under a version any feed already holds is refused with
`409`. To keep a low-trust feed from claiming the names of your own packages,
reserve their prefix for the feed that publishes them:

```toml
[[feeds]]
name = "internal"
reserved_id_prefixes = ["Contoso."]
```

Now only `internal` can bring in `Contoso` or `Contoso.*` by push, mirror or
migration; every other feed answers `403`. Promoting or copying an existing
version is an admin action and is not affected.

## Keys per feed

Each feed can have its own credentials, falling back to the global ones:

| Purpose | Key | Without it |
| --- | --- | --- |
| Push and delete | `api_key` / `api_keys` | The global `api_key` / `api_keys` |
| Restore, search, gallery | `read_api_key` | Open to everyone who can reach the server |
| Admin area, approval, promotion | `admin_api_key` | The global `admin_api_key`; no admin area if neither is set |

A feed that sets any push key of its own accepts only its own. See
[Connecting clients](clients.md#read-gated-feeds) for passing a read key.

## Release rings

A release ring is a chain of feeds that a build moves through, for example
`dev → test → stable`. Two settings make one:

* **`promotes_to`** names the next feed in the chain. It must name another
  configured feed, or the server does not start.
* **`requires_approval = true`** gates a feed: every version that enters it, by
  push, promotion, copy or mirror, arrives **pending** and is withheld from
  clients until an admin approves it.

```toml
admin_api_key = "change-me-admin"

[[feeds]]
name = "dev"
api_key = "ci-push-key"        # CI pushes every build here
promotes_to = "test"

[[feeds]]
name = "test"
promotes_to = "stable"

[[feeds]]
name = "stable"
requires_approval = true       # nothing reaches clients unreviewed
```

### Promoting a version

In the admin area of the source feed (`/dev/admin`), open the package and use
**Promote → test** on the version. Promotion **adds** the version to the next
feed; it stays in the source feed as well. Over HTTP it is

```
POST /{feed}/admin/packages/{id}/{version}/promote
```

with the admin credentials and the CSRF token, as described under
[Admin area](api.md#admin-area-http-basic-auth).

* The version must be a member of the feed it is promoted from.
* The target's gate and license policy apply as for a push: in a gated target
  the version waits as pending; a target whose `license_policy` blocks the
  package refuses the promotion.
* Promoting into the feed named by `promotes_to` needs only the source feed's
  admin credentials, because the configuration already chains the two.
* The state the version had in the source feed carries over; see
  [Release rings & approval](configuration.md#release-rings-approval).

### Approving

A pending version shows up in the target feed's admin area. **Approve** clears
the gate, and from then on clients see it. Several versions can be approved at
once by ticking them. A pending version is not touched by retention and is not
counted by it.

### Copy and move

Promotion follows the chain; **copy** and **move** go to any other feed. Tick
the versions on the package's admin page, choose the operation and the target
feed. A copy adds the versions to the target as a promotion does; a move also
removes them from the source and keeps each one's listed, enabled and pinned
state. Copying or moving into a feed other than the `promotes_to` target needs
credentials valid for that feed too, so feeds that share the global
`admin_api_key` can hand versions to each other, and feeds with keys of their
own cannot without both.

Feeds without `promotes_to` are simply independent sets of packages between
which versions can be copied or moved.

## Mirroring an upstream feed

A feed with a mirror is a read-through cache: a request for a package it does
not have fetches that package from the upstream, stores it and serves it from
then on.

```toml
[[feeds]]
name = "nuget"

  [feeds.mirror]
  enabled = true                # upstream defaults to nuget.org
```

Point clients at `/nuget/v3/index.json` instead of nuget.org and every package
your builds use is kept on your server. Combine a mirror with
`requires_approval` to review third-party packages before builds may use them,
and with a license policy to flag or block licenses you do not accept. A
version deleted from a mirror feed is not fetched again. Limits, credentials for
private upstreams and refresh behaviour are under
[Configuration → Feeds](configuration.md#feeds).

## License policy

A feed's `[feeds.license_policy]` checks each incoming package's SPDX license
expression offline, against allow and block lists:

```toml
  [feeds.license_policy]
  enabled = true
  allowed = ["MIT", "Apache-2.0", "BSD-3-Clause"]
  action = "warn"               # accept and flag; "block" refuses
```

With `warn`, a violating package is accepted and marked **flagged** in the
admin area; with `block`, the push, mirror fetch, promotion or copy is refused
with `403`. A block list alone is advisory, since a package can declare its
license as a file; set `allowed` to control what comes in.

## Moving packages in from another server

To fill a feed with everything another NuGet server holds, use
[`yanuget migrate`](migrate.md). It honours the target feed's gate, license
policy and reserved prefixes like a push.
