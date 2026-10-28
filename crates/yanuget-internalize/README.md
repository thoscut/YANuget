# yanuget-internalize

Internalizes Chocolatey packages into a [YANuget](../../README.md) feed: runs
each package's scripts in an offline Windows sandbox with Chocolatey's
download helpers replaced by recorders, downloads what they asked for,
attaches the files to the feed, and pushes a copy whose install script points
at them. The plan is the "Chocolatey internalizer" section of
[ROADMAP.md](../../ROADMAP.md).

**Not released, and not usable yet.** The command line, the configuration and
its check (`INT-10`), the upstream client (`INT-11`), the dependency closure
(`INT-12`), the capture parser (`INT-16`), the downloader (`INT-17`), file
naming (`INT-18`), publishing (`INT-20`), the report (`INT-21`), the
static cross-check (`INT-23`), overrides (`INT-25`) and the prelude (`INT-19`)
exist; the sandbox runners follow, and `run` is wired up with them.
`plan` shows the closure of the packages it is given and then stops with an
error; `run` stops before it fetches anything.

## Command line

```text
yanuget-internalize [-c PATH] plan PACKAGE...
yanuget-internalize [-c PATH] run [--replace-pending] PACKAGE...
yanuget-internalize [-c PATH] sync [--dry-run]
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
checked as they resolve, and behind `upstream.proxy` resolved here first, as
for downloads below. Redirects are followed one hop at a time, never from
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
Behind `downloads.proxy` the proxy resolves the names, so each host is also
resolved here first and refused when it is private or does not resolve; the
proxy resolves again on its own, so let it refuse private destinations too. A
file that came over plain HTTP with nothing to verify it is a warning. The
name a download got (its URL after redirects, `Content-Disposition`,
`Content-Type`) is kept next to it, so a file taken from the cache is
attached under the same name. A transfer that breaks off is resumed with a `Range` where the host offers
ranges and a validator, and restarted where it does not. `downloads.timeout_secs`
bounds any silence; `downloads.file_timeout_secs` (four hours by default)
bounds a whole file, resumes included, so a host that trickles bytes cannot
hold a run.

A file is attached under the URL's own name when that is valid for the feed
(1 to 128 characters of `A-Z a-z 0-9 . _ -`) and its extension fits the
content. Otherwise the name comes from `Content-Disposition`, the URL after
redirects or the package, and the extension from the name, the captured file
type or the content itself (`MZ`, the OLE header of an `.msi`, `PK`). Two
different files on one name are told apart by a prefix of their SHA-256. A
file whose extension the feed does not accept is reported, never renamed to
pass. The extensions the feed accepts are `target.allowed_extensions`, which
has to match the feed's own `allowed_extensions`. A report larger than
YANuget reads (1 MiB) fails the package before it is pushed.

## Report

Every package of a run gets a report, in the schema the server reads
(`yanuget_pkg::report`, version 1):

| Status | When |
| --- | --- |
| `internalized` | Every download was fetched and can be attached. |
| `unchanged` | Nothing to download: an embedded or a meta package. |
| `needs-override` | The capture cannot be trusted for it: the install failed or ran out of time, the capture was rejected, a helper was called from an extension's module or from outside the package's scripts, or a file has a type the feed does not take. |
| `failed` | The sandbox failed, or a download did (an error answer, a checksum that does not match, the size cap). |

A failure outranks an override, and the report lists every reason, failures
first. It names the upstream source and hash, the image and the Chocolatey
version the scripts ran with, the tool's version, each download with its
helper, file name, size, SHA-256 and checksum status, and the warnings. One
capture covers a whole closure, so what makes the capture untrustworthy marks
every package in it; a single call marks only its own package.

Capture runs the install script only, down the branches the sandbox takes.
So every PowerShell file in the package (`*.ps1`, `*.psm1`, `*.psd1`,
`chocolateyUninstall.ps1` and `chocolateyBeforeModify.ps1` included) is
also scanned for `http` and `https` URLs outside comments, and each one the
capture did not ask for becomes a warning: the internalized package would
fail where it reaches for it. A URL built from variables
(`https://example.com/$version/setup.exe`) counts as seen when a captured
URL starts with its literal part. The scan cannot tell a download from a
link to a project page, so it only warns, up to twenty URLs a package.

A run prints the reports as text, or as one JSON document for pipelines:
`{"outcome": "needs-attention", "packages": [...]}`, the outcome being what
the exit code says. The report also goes inside the package as
`internalizer/report.json`, covered by the package hash, so a promotion, a
copy, `yanuget migrate` and `yanuget backup` keep it with the package.

## Publishing

A package that is `internalized` or `unchanged` is rewritten with its report
(and, when files were attached, the prelude), pushed to `target.url` with the push key,
and its files are attached under `api/v2/files/…` with their SHA-256 for the
feed to check. The tool then reads the feed's flat container, which lists
approved versions only, and warns when the version is served at once: the
feed does not require approval.

The push key cannot see pending versions, so a push is how the tool learns
what the feed holds:

- `201`: new. The files are attached.
- `409`: the feed has the version and kept it, which a run takes as already
  internalized. While the version is pending, the files are attached again:
  the same file is a no-op, so a run that stopped half-way is completed by
  the next one. A version the feed serves already, or one a read-gated feed
  cannot tell about, gets nothing attached, since an attach is not reviewed.
- With `--replace-pending`, a `409` is an error: the version is approved, or
  the feed does not set `allow_overwrite = "pending-only"`. That mode needs
  YANuget 0.8.0, which the tool checks up front where `/health/status` says
  the server's version. On such a feed every push replaces a pending
  version, with or without the flag.

An attach that breaks off or meets a server error is tried again. A file the
feed refuses (its type, a different file under the name) fails the package.
Each request may take `target.timeout_secs`, plus a second for every MiB of
its file.

## Prelude

A package with attached files gets `internalizer/prelude.ps1`, the prelude.
The package's own `chocolateyInstall.ps1` moves to
`chocolateyInstall.upstream.ps1` in the same folder, so `$toolsDir` and
`$MyInvocation` still lead there. In its place goes a loader that
dot-sources the prelude and then the package's own script.
`chocolateyBeforeModify.ps1` and `chocolateyUninstall.ps1` get the same
treatment (`chocolateyUninstall.upstream.ps1`, ...): Chocolatey runs them with
the same helpers, so they can download too. The capture ran neither, so a
download there that the install did not make stops the upgrade or uninstall
rather than reaching the internet; the static cross-check warns about such
URLs beforehand, and `choco uninstall --skip-powershell` gets past one. Every
copy of these scripts in the package is wrapped, in whatever folder, since
which one Chocolatey runs depends on how it walks the extracted package. A
package that already has entries under the new names is refused.

The prelude wraps `Get-ChocolateyWebFile`, `Get-WebFile`, `Get-FtpFile`,
`Install-ChocolateyPackage`, `Install-ChocolateyZipPackage`,
`Install-ChocolateyVsixPackage` and `Install-ChocolateyPowershellCommand` in
proxy commands. The client makes them at install time from the helpers it
has (`ProxyCommand::Create`), so positional parameters and aliases bind as
they do against those helpers, whatever Chocolatey version is installed. A
wrapper:

- replaces each `http(s)` or `ftp(s)` URL with the attached file's URL;
- replaces the checksum with the file's SHA-256, whatever the package
  declared;
- for `Get-WebFile`, which takes no checksum, checks the file it wrote.

A URL that is not in the map stops the install with a message naming the
package; it never falls back to the internet. Whatever starts with `http` or
`ftp` once the blanks around it are trimmed counts as a URL, as it does for
the helpers, so ` https://...` or `https:\\host\file` cannot slip past. A
file on this machine (a drive path, a relative path, a `file:` URI without
a host) is left alone. A UNC path (`\\host\share`), a `file:` URI with a
host or any other scheme stops the install as well: the helpers would copy
it over the network. Normalizing such a value and copying it anyway was
the alternative; refusing is the one that cannot reach another machine by
a spelling the prelude did not foresee. Calls the helpers make among
themselves are not wrapped, so they keep the URL they were given.

The map is keyed by the SHA-256 of the URL as the report records it. The
wrapper looks up the URL as the package spelled it, then in the canonical
form .NET gives it (`[Uri]::AbsoluteUri`), so a URL that the capture
recorded in canonical form still matches. No text a package wrote is
written into the prelude, only hashes, the file names `INT-18` checked and
the feed's URL. That URL is `target.url`. A client sets
`YANUGET_INTERNALIZED_FEED_URL` to fetch from another address, after a host
move.

A package with no attached files is published as it is. A feed with a
`read_api_key` cannot serve the files to `choco install` yet (`INT-31`).
`tests/prelude.rs` installs a package from a real server through the
prelude, with a stand-in for Chocolatey's helpers. It runs where `pwsh` is
installed, or where `YANUGET_TEST_PWSH` names it.

## Overrides

What the capture cannot find out, a person writes down: one TOML file per
package id in `overrides_dir`, named for the id (`git.install.toml`).
Unknown keys are refused, and one file that does not check out stops the
run, as do two files for one id (`Git.toml`, `git.toml`), so no override is
silently left out. `config check` loads them.

```toml
versions = "[2.51,)"                       # the versions it applies to; all without
package_parameters = "/NoShellIntegration" # --params for the install, as the root
script = "git.install.ps1"                 # replaces chocolateyInstall.ps1

[[download]]                               # a download the capture misses
url = "https://example.com/extra.cab"
sha256 = "…"                               # verifies it; without, trust-on-first-use
file_type = "cab"

[[rewrite]]                                # fetch a captured URL from elsewhere
from = "https://example.com/gone/setup.exe"
to = "https://mirror.example.com/setup.exe"
```

Or, alone in the file, `skip = "why"`: the package is left out of runs, and
`plan` says so. The script is a path inside `overrides_dir`; it goes into the
package before the capture, under the name the package's own has, so the
capture runs it and the feed gets it. A rewritten URL is still the one the
package asks for, and the prelude maps it to the attached file. Every
override used is listed in the report under `overrides`; a rewrite that
matches no captured URL, or a download the capture saw anyway, is a warning,
so stale overrides surface.

## Keeping feeds current

`sync` internalizes, for each entry of the watch list (`[sync] watch_list`),
the newest listed upstream version in its range that no earlier sync
handled, and lists what waits for approval. A pre-release counts only when a
bound of the range is one. A version released and superseded between two
syncs is not fetched.

```toml
# watch.toml
[[package]]
id = "git"

[[package]]                 # an id may be listed once per range
id = "dotnet-8.0-sdk"
versions = "[8.0,9.0)"
```

It is meant for a timer on the worker. What it did lives in
`[sync] state_file`, saved after every package, so a sync is idempotent and
safe to interrupt; a lock file next to it keeps a second sync from starting.
Each version a sync handled is:

| Status | Means | Next sync |
| --- | --- | --- |
| `pending` | Pushed; the target feed does not serve it yet. | Waits. |
| `approved` | The target feed serves it. | Nothing. |
| `needs-override` | A package of its closure needs an override. | Tries again once any override changes. |
| `failed` | The run failed. | Tries again. |

A sync never pushes a version twice, so a version an admin deletes or leaves
pending stays out. It stops counting as pending once a newer version in the
same range is approved, or at once when an override skips it
(`versions = "[2.51.0]"`, `skip = "why"`). Ids taken off the watch list are
forgotten. To fix a version that is already approved, delete it for good in
the feed (from the admin area, or with `hard_delete_enabled`) and internalize
it again with `run`; a package fix version would break the exact pin of a
meta package on its `.install` package.
Whether a version is approved comes from the target's flat container; on a
read-gated target the tool cannot tell, and says so.

With `[sync] metrics_file` set, each sync writes its metrics in the
Prometheus text format, atomically, for node_exporter's textfile collector:

| Metric | Means |
| --- | --- |
| `yanuget_internalize_sync_last_run_timestamp_seconds` | When the last sync ended. Alert when it is old: a sync that cannot start writes nothing. |
| `yanuget_internalize_sync_outcome{outcome}` | 1 for how the last sync ended (`done`, `needs-attention`, `failed`). |
| `yanuget_internalize_sync_versions{status}` | Versions in the state, per status above. |
| `yanuget_internalize_sync_oldest_pending_seconds` | How long the oldest pending version has waited; 0 when none. |

`sync --dry-run` says what a sync would internalize and changes nothing. Until
a sandbox backend captures, `sync` records each new version as failed.

## Sandboxes

Everything around the sandbox only sees the `Sandbox` trait, so it builds and
is tested on any host against `sandbox::fake::FakeSandbox`. The backends are
KVM (`INT-15`) and Windows containers (`INT-29`).
