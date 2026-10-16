# yanuget-internalize

Internalizes Chocolatey packages into a [YANuget](../../README.md) feed: runs
each package's scripts in an offline Windows sandbox with Chocolatey's
download helpers replaced by recorders, downloads what they asked for,
attaches the files to the feed, and pushes a copy whose install script points
at them. The plan is the "Chocolatey internalizer" section of
[ROADMAP.md](../../ROADMAP.md).

**Not released, and not usable yet.** The command line, the configuration and
its check exist (`INT-10`); fetching packages, the sandbox runners and
publishing follow. `plan` and `run` stop with an error before they fetch
anything.

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
list a push resource (`--offline` skips that), and warns about a
`YANUGET_INTERNALIZE_*` variable that is not a setting. Errors exit `1`.

## Sandboxes

Everything around the sandbox only sees the `Sandbox` trait, so it builds and
is tested on any host against `sandbox::fake::FakeSandbox`. The backends are
KVM (`INT-15`) and Windows containers (`INT-29`).
