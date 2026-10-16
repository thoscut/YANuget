# yanuget-internalize

Internalizes Chocolatey packages into a [YANuget](../../README.md) feed: runs
each package's scripts in an offline Windows sandbox with Chocolatey's
download helpers replaced by recorders, downloads what they asked for,
attaches the files to the feed, and pushes a copy whose install script points
at them. The plan is the "Chocolatey internalizer" section of
[ROADMAP.md](../../ROADMAP.md).

**Not released, and not usable yet.** The command line, the configuration and
its check (`INT-10`) and the upstream client (`INT-11`) exist; the sandbox
runners and publishing follow. `plan` resolves the packages it is given and
then stops with an error, as does `run`.

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

## Sandboxes

Everything around the sandbox only sees the `Sandbox` trait, so it builds and
is tested on any host against `sandbox::fake::FakeSandbox`. The backends are
KVM (`INT-15`) and Windows containers (`INT-29`).
