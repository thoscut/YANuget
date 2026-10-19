# yanuget-internalize

Internalizes Chocolatey packages into a [YANuget](../../README.md) feed: runs
each package's scripts in an offline Windows sandbox with Chocolatey's
download helpers replaced by recorders, downloads what they asked for,
attaches the files to the feed, and pushes a copy whose install script points
at them. The plan is the "Chocolatey internalizer" section of
[ROADMAP.md](../../ROADMAP.md).

**Not released, and not usable yet.** The command line, the configuration and
its check (`INT-10`), the upstream client (`INT-11`), the dependency closure
(`INT-12`), the capture parser (`INT-16`), the downloader (`INT-17`) and file
naming (`INT-18`) exist; the sandbox runners and publishing follow. `plan`
shows the closure of the packages it is given and then stops with an error;
`run` stops before it fetches anything.

## Command line

```text
yanuget-internalize [-c PATH] plan PACKAGE...
yanuget-internalize [-c PATH] run [--replace-pending] PACKAGE...
yanuget-internalize [-c PATH] config check [--offline]
```

A package is `id` (the newest version) or `id@version`. `--config` falls back
to `YANUGET_INTERNALIZE_CONFIG`.

| Exit code | Meaning |
| --- | --- |
| 0 | Done: every package was internalized or needed nothing. |
| 1 | Failed: the run, or at least one package, failed. |
| 2 | The command line was wrong. |
| 3 | Needs attention: nothing failed, but a package needs an override. |

## Configuration

Defaults, then a TOML file, then `YANUGET_INTERNALIZE_*` environment
variables, as the server does it. The file refuses unknown keys; every
variable that is set has to parse. A variable's name is the setting's TOML
path in capitals, joined by `_`. The push key can come from a file:
`YANUGET_INTERNALIZE_TARGET_API_KEY_FILE`. Every setting, with its default,
is in [`yanuget-internalize.example.toml`](yanuget-internalize.example.toml).

`config check` loads the configuration as a run would and checks what it
points at. With the KVM backend: a Linux host, `/dev/kvm` openable for
reading and writing, QEMU found, the golden images present and in qcow2
format. With the Windows container backend: a Windows host and an image
pinned by digest. It asks the target feed for its service index, which must
list a push resource, and the upstream for its service document or index
(`--offline` skips both), and warns about a
`YANUGET_INTERNALIZE_*` variable that is not a setting. Errors exit `1`.

## Upstream

`upstream.url` is a NuGet v2 feed, the Chocolatey Community Repository by
default, or a v3 service index (a URL ending in `.json`). Versions come from
`FindPackagesById()` on v2, and from the registration (or, without one, the
flat container) on v3; without a version, a package resolves to its newest
listed stable version, as `choco install` picks it.

Every download is checked against the hash the feed states (`PackageHash`)
and its manifest against the id and version asked for, then kept in
`cache_dir/packages` under that hash; a cached package is hashed again before
it is used. A feed that states no hash, as most v3 feeds do, gets its
packages cached under their identity and marked as unverified.

URLs, redirects included, that lead to private, loopback or link-local
addresses are refused unless `upstream.allow_private` is set; names are
checked as they resolve. Redirects are followed one hop at a time, never from
https to http, and credentials go only to the upstream's own origin. A `429`
or `503` is waited out and tried again; when the upstream stops answering,
and when it answers again, the tool logs it once.

## Dependency closure

`plan` resolves each package's dependencies before anything is downloaded
and prints the closure, one package a line, root first:

```text
git 2.51.0
  git.install 2.51.0
  chocolatey-core.extension 1.4.0 (the target holds it)
  chocolatey-compatibility.extension 1.0.0
4 packages, 3 to internalize
```

The versions are the ones Chocolatey CLI v2 installs for the same command:
NuGet's resolver as Chocolatey runs it, with `DependencyBehavior.Highest`, so
each dependency takes the highest version every range on it allows, not the
lowest as `dotnet restore` does. Pre-release versions are candidates only when
the root is one or a range pins exactly that version, and an unlisted version
is taken only when no listed one fits. Extension packages are ordinary dependencies and part of the closure,
since scripts call their helpers. A dependency on `chocolatey` itself is left
to the Chocolatey CLI the image has, and listed as such.

With `target.url` set, each package the target feed serves already is marked.
A read-gated feed cannot say, since the tool holds a push key only; pending
versions are not listed either. Pushing such a version again answers `409`.

## Capture and downloads

What the recording helpers in the sandbox write is parsed as hostile input,
since the package's own code runs next to them: schema version 1, one JSON
object a line, unknown fields refused, caps on lines, calls and size, no
control or bidirectional characters, `http` and `https` URLs only, a hex
checksum of its type's length, and every call attributed to a package of the
closure. A capture that breaks a rule is rejected whole, with every reason,
and nothing in it is repaired.

Each captured URL is then downloaded into `cache_dir/files`, under the file's
SHA-256, while it is hashed with SHA-256 and with the algorithm the script
declares. A declared checksum that does not match fails the package. A match
on SHA-256 or SHA-512 verifies the file; MD5, SHA-1 or no checksum leaves it
trust-on-first-use, which the report says. URLs and redirects that lead to
loopback, private or link-local addresses are refused unless
`downloads.allow_private_hosts` names the host, and no credentials are sent.
A transfer that breaks off is resumed with a `Range` where the host offers
ranges and a validator, and restarted where it does not.

A file is attached under the URL's own name when that is valid for the feed
(1 to 128 characters of `A-Z a-z 0-9 . _ -`) and its extension fits the
content. Otherwise the name comes from `Content-Disposition`, the URL after
redirects or the package, and the extension from the name, the captured file
type or the content itself (`MZ`, the OLE header of an `.msi`, `PK`). Two
different files on one name are told apart by a prefix of their SHA-256. A
file whose extension the feed does not accept is reported, never renamed to
pass.

## Sandboxes

Everything around the sandbox only sees the `Sandbox` trait, so it builds and
is tested on any host against `sandbox::fake::FakeSandbox`. The backends are
KVM (`INT-15`) and Windows containers (`INT-29`).
