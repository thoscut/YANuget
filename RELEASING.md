# Releasing YANuget

Releases are cut by pushing a `vX.Y.Z` tag. Everything after that is automated by
[`.github/workflows/release.yml`](.github/workflows/release.yml).

## Versioning

Semantic versioning. While the major version is `0`:

- **minor** (`0.1.0` → `0.2.0`) for breaking changes to the configuration file,
  the CLI, or the on-disk layout — anything that makes an existing deployment
  need attention before it starts again;
- **patch** (`0.1.0` → `0.1.1`) for everything else.

The NuGet v3 endpoints are a published protocol. They do not change
incompatibly at any version.

## Before tagging

1. **CI is green on `main`** — including the `Packaging` and
   `NuGet client compatibility` jobs. The first proves the crate still packages
   and the container image still builds and comes up healthy; the second proves
   the real `dotnet` client can still push and restore.

2. **Run the client check locally** if the release touches anything a NuGet
   client observes:

   ```bash
   cargo build --release && scripts/verify-with-dotnet.sh
   ```

3. **Update the changelog.** Rename `## [Unreleased]` to `## [X.Y.Z] — YYYY-MM-DD`,
   add a fresh empty `## [Unreleased]`, and update the two link definitions at
   the bottom of the file.

4. **Bump `version` in `Cargo.toml`**, then `cargo build` so `Cargo.lock`
   records the new version too. Commit both.

   The release workflow refuses to build if the tag does not equal
   `v` + the `Cargo.toml` version, or if the changelog has no section for it.

5. **Check the dependency audit.** `cargo audit` runs in CI; a release is a
   reasonable moment to also run `cargo update` on a branch and see whether
   anything wants upgrading.

## Tagging

```bash
git checkout main && git pull
git tag -a v0.1.0 -m "YANuget 0.1.0"
git push origin v0.1.0
```

## What the workflow does

| Job | Result |
| --- | --- |
| `guard` | Refuses the release unless the tag, `Cargo.toml` and `CHANGELOG.md` agree. |
| `build` | Compiles for five targets (Linux x86-64/aarch64, macOS x86-64/aarch64, Windows x86-64) with the rendered docs embedded, and packages each with the README, licence, changelog and example config. |
| `image` | Publishes `ghcr.io/thoscut/yanuget` tagged `X.Y.Z`, `X.Y` and `latest`. |
| `release` | Attaches every archive plus `SHA256SUMS` to a GitHub Release whose notes are the changelog section for this version. |
| `crates-io` | Runs `cargo publish` — **only** if the repository has a `CARGO_REGISTRY_TOKEN` secret; otherwise it logs a notice and skips. |

Re-running a failed release is safe for everything except `crates-io`: a version
published to crates.io can be yanked but never replaced. If that job is the one
that failed, fix the cause and release a new patch version rather than retrying
the same one.

## After the release

- Check that the container image runs:
  `docker run --rm -e YANUGET_API_KEY=test -p 5000:5000 ghcr.io/thoscut/yanuget:X.Y.Z`
- Download one binary and confirm `/docs` serves the real documentation rather
  than the "documentation not bundled" placeholder — that placeholder appearing
  means the docs step did not run.

## A note on `cargo install`

The rendered documentation site is generated, so it is not part of the crates.io
package. A binary from `cargo install yanuget` therefore serves a placeholder at
`/docs` that links to the online documentation; everything else is identical. The
release binaries and the container image both ship the full offline site.
