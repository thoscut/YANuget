# Roadmap

What is planned, and what a review found that needs fixing. The README's
[Roadmap](README.md#roadmap) section lists what is already implemented; this file
is the work list.

Items are tagged with a stable id (`SEC-…`, `COR-…`, …) so commits and pull
requests can reference them, and ticked off rather than deleted when they land.

## Open items

Everything still open across this file and the project's working threads, as
of 2026-11-03 (after the review of that day). The entries further down hold
the detail; this list is the index.

**Server, after 0.9.0** ([Target releases](#target-releases))

- `INT-31` Attached files on read-gated feeds: the premise holds (Chocolatey's
  helpers never send source credentials); a security review of the token
  design comes next, before it is built.

**Internalizer 0.1.0**

- `INT-13` to `INT-15` and `INT-22`: golden images, capture helpers, the KVM
  runner, and the regression corpus. `INT-13` waits on the licensing question
  below. Capture validation, downloader, file names, the prelude, publish and
  the report (`INT-16` to `INT-21`) have landed.

**Internalizer 0.2.0 and later**

- `INT-24` environment variants, `INT-26` validation install, `INT-28`
  embedding for other feeds; `INT-23`, `INT-25` and `INT-27` have landed.
- `INT-29` Windows container backend (0.3.0, optional).
- `INT-34` documentation and threat model, shipped in parts with each release.

**Server, later**

- `INT-33` internalization requests, if the open question about it is
  answered for it.

**Features under discussion**

- Markdown readmes on the package page; see [Features](#features).

**For the repository owner**

- The settings under [For the repository owner](#for-the-repository-owner),
  checked on 2026-11-03: the `release` environment exists; there is no tag
  ruleset for `v*` yet, and neither `yanuget` nor `yanuget-pkg` is on
  crates.io.
- An open question from the review of 2026-09-30: move its `SEC-…` findings
  from this public file (now `reviews/2026-09-30.md`) into a private security
  advisory? Every one of them is fixed and released since 0.6.0.
- The internalizer's [open questions](#open-questions), above all the Windows
  license for the golden images (needed before `INT-13` ships) and the
  release scheme for the tool.

## Review of 2026-11-03

A multi-expert review of `main` at 0.9.0 (`544e2f0`): architecture and
correctness, security, Rust code, tests and CI, operations and
documentation. Every finding was fixed with it, with tests. The changelog's *Unreleased* section describes them
for operators.

### Security and integrity

- [x] **`SEC-32` One resumable upload could hold all free space.** A tus
  session without a size cap reserved its whole `Upload-Length` until it
  expired (72 h), and a push reserved its `Content-Length` unchecked: either
  blocked every push on the volume with `507`. *Done:* a push over
  `max_package_size_bytes` is `413` before it reserves; a resumable upload
  holds its space for an hour after its last request (`uploads.held_since`,
  schema 10); the log says what holds the space.
- [x] **`SEC-33` Files of an approved version were protected only under
  `pending-only`,** and a source feed's admin key could always change them.
  *Done:* an approval in a feed that reviews what it serves
  (`requires_approval` or `pending-only`) holds the files against the push
  key of every feed and the inbox; only the admin key of each approving feed
  sets it aside.
- [x] **`SEC-34` `[monitoring] token = ""` in the file opened `/metrics`.**
  *Done:* an error, like the blank variable; `config check` warns about a
  short token.
- [x] **`SEC-35` The prelude took provider paths and network drives for
  local.** *Done:* it strips the FileSystem provider, then refuses UNC paths,
  other providers and network drives.
- [x] **`SEC-36` The server mirror did not check private addresses behind a
  proxy.** *Done:* each hop is resolved locally and checked, as the
  internalizer does (shared `yanuget_pkg::address::check_resolved`).
- [x] **`SEC-37` Any credential on an open feed with a preview key turned a
  miss into `401`** and counted against `max_failed_auth`. *Done:* only a key
  offered where the preview key goes does.
- [x] **`SEC-38` `feeds rename` carried approvals into a gated feed
  silently.** *Done:* it asks for `--keep-approval` or
  `--hold-for-approval`; license policy and prefixes are not re-applied,
  which the docs say.
- [x] **`SEC-39` The prelude relied on Chocolatey's checksum check,** which
  `--ignore-checksums` turns off. *Done:* it keeps the check on around its
  helpers and verifies downloaded files itself; the feed URL must be http(s).
- [x] **`SEC-40` Bidi characters from a report reached the approve page.**
  *Done:* hidden characters show as `[U+XXXX]` (`yanuget_pkg::text`).

### Correctness

- [x] **`COR-38` Transfers dropped `listing_set`,** so a mirror relisted a
  version unlisted by hand in the target. *Done:* promote, copy and move
  carry it; an overwrite keeps it. Every version of a mirror feed, pushed ones
  included, follows the upstream's listing unless it was set by hand there;
  `docs/configuration.md` says so.
- [x] **`COR-39` New mirrored versions were listed until the fetch loop
  ended.** *Done:* indexed with the upstream's listing from the start.
- [x] **`COR-42` Mirror downloads and inbox imports did not reserve.**
  *Done:* one `SpaceGuard` admits every write into the staging directory;
  the four copies of the check are one.
- [x] **`COR-43` Offline commands detected a running server only over
  loopback.** *Done:* the server holds `<database_path>.lock`; `restore` and
  `feeds` take it too.
- [x] **`MNT-07` Relative catalog page ids, and registration pages after a
  redirect, were resolved wrongly.** *Done:* `yanuget_pkg::v3::resolve_id`
  against the final URL.
- [x] **`DEP-11` `feeds … --dry-run` migrated the database.** *Done:*
  read-only.
- [x] **`DEP-12` `config check` passed relative data paths.** *Done:* a
  warning.

### Rust code and performance

- [x] **`PERF-09`, `PERF-10` Blocking syscalls on async workers** (inbox
  scan, `statvfs`). *Done:* on the blocking pool.
- [x] **`PERF-11` Every push copied every feed's license policy.** *Done:*
  only under `pending-only`.
- [x] **`PERF-12` Protocol documents built as `serde_json::Value` trees.**
  *Done:* Serialize structs, same bytes.
- [x] **`PERF-13` The listing reconciliation fetched the whole registration
  and full rows each time.** *Done:* one registration read per refresh,
  shared with read-through, and a narrow query.
- [x] **`PERF-14` The admin dashboard aggregated twice.** *Done:* once.
- [x] **`MNT-21` Mirror and internalizer still read v3 differently.**
  *Done:* leaf reading, gzip, redaction and the slash helper are shared in
  `yanuget-pkg`; the internalizer parses on the blocking pool.
- [x] **`MNT-22` The headline rule existed twice.** *Done:* `headline_beats`.
- [x] **`MNT-23` 28 panicking `Row::get`.** *Done:* `try_get`.
- [x] **`MNT-24` String errors in the internalizer.** *Done:* enums with
  sources.
- [x] **`MNT-25` Two searches for Chocolatey's scripts.** *Done:* one,
  deterministic, and an override replaces every install script.
- [x] **`MNT-26` Large files.** *Done:* `src/mirror.rs`, `src/config.rs` and
  `tests/integration.rs` are split into modules, with shared test helpers in
  `tests/common/`.
- [x] **`MNT-27` Hand-rolled temporary names.** *Done:* `tempfile`.
- [x] **`MNT-28` Small things** (`push_str(&format!)`, `temp_dir` copies, the
  monitor's status clone, `SearchGroup`'s field and method of one name).
  *Done.*

### Tests and CI

- [x] **`CI-14` rustdoc was not checked; seven dead links.** *Done:* fixed,
  and CI builds the docs with `-D warnings`.
- [x] **`CI-15` The hash group proposed sha2 0.11 for the server, which did
  not build.** *Done:* the server is on sha2 0.11 too (`hash_reader`
  replaces `io::copy` into a hasher).
- [x] **`CI-16` The crates.io step took any answer but 404 as published,**
  and nothing asked for a new `yanuget-pkg` version. *Done:* `200` skips,
  `404` publishes, anything else fails; `guard` refuses a changed crate
  whose version crates.io has.
- [x] **`CI-17` aarch64, x86-64 macOS and the arm64 image were never
  started.** *Done:* each binary and image platform prints its version
  before it is published.
- [x] **`CI-18` The media pull request gets no CI.** *Done:* its body and
  CONTRIBUTING say how to start it.
- [x] **`CI-19` `yanuget-pkg` never ran without its features.** *Done:* a
  CI step. The ignored 5 GiB test runs on the weekly schedule, and pushes to
  `claude/**` no longer run CI a second time next to their pull request.
- [x] **`TEST-09` to `TEST-16`** Port race in the CLI tests, COR-35's other
  cases, the open-upload count, hermetic reservation tests, the blob sweep's
  promises, `listing_set` across transfers, dashboard filter with paging,
  and the second backup run. *Done:* tests for each; the space measurement is
  injectable.

### Operations and documentation

- [x] **`DEP-13`** `yanuget feeds` in a container; **`DOC-10`** to
  **`DOC-19`**: the README's crates.io remnant, the 0.9.0 upgrade note and
  its missing PERF-08 entry, this file's index and archive, the release jobs
  in RELEASING.md, `config check`'s warnings, troubleshooting for the newer
  status codes, compression and the dashboard filters, and small fixes in
  the example config, SECURITY.md, the upgrade text and the CLI page.
  *Done.*

### Left open by the review of 2026-09-30, done now

- [x] **`CI-04` No SBOM and no `cargo auditable`.** *Done:* the release
  binaries and the image's are built with `cargo auditable`; each release
  attaches a CycloneDX SBOM (`yanuget-vX.Y.Z.cdx.json`, attested with the
  archives), and the image carries a BuildKit SBOM attestation. CI generates
  the SBOM on every pull request and checks the image's binary for its
  dependency section.
- [x] **`CI-20` No coverage measurement.** *Done:* a `cargo llvm-cov` job
  writes a summary to each CI run and uploads the lcov report. Nothing gates
  on it. Fuzzing stays a property test over the NuGet v3 reader
  (`crates/yanuget-pkg/tests/v3_robustness.rs`) rather than a `cargo fuzz`
  target, which would need a nightly toolchain.

## Review of 2026-10-27

A multi-expert review of `main` after 0.8.0 (architecture and correctness,
security, Rust code, tests and CI, operations and documentation), at
`2c35bca`. The fixed findings landed with it; the rest followed before
0.9.0.

### Fixed with the review

- [x] **`COR-32` An overwrite kept the old build's SemVer 2.0.0 flag.** The
  `ON CONFLICT` update of `replace_version` left `is_semver2` out, so since
  `24cf7ad` a new build with a SemVer 2.0.0 dependency range was served to
  SemVer 1.0.0 clients, or the reverse.
- [x] **`COR-33` A promotion judged the build before taking the version
  lock** (`transfer_version` in `src/web/admin.rs`): a pending-only push in
  between swapped in a build the target's policy blocks.
- [x] **`COR-34` The shutdown did not wait for detached request work.** A
  restart dropped an overwrite or a move part-way, the gap `COR-21` closed
  for disconnects. Detached work now runs on a tracker the shutdown waits for.
- [x] **`SEC-31` A blank `YANUGET_MONITORING_TOKEN` removed the token** and
  left `/metrics` and `/health/status` open. It is read like a key now (blank
  is an error, `_FILE` works), and `config check` warns about monitoring
  without a token.
- [x] **`DEP-10` Dependabot's md-5 and sha1 0.11 did not build** (#75, #76):
  they share `digest` with sha2. All three moved together in the internalizer
  (#77), and Dependabot groups their majors now. The server stays on sha2
  0.10: digest 0.11 drops the `io::Write` impl `src/symbols.rs` hashes with.

- [x] **`CI-11` A release did not wait for CI.** `guard` checked the tag,
  the version and the CHANGELOG; nothing ran the tests or the audit on the
  tagged commit. `release.yml` now runs `ci.yml` (made callable) on that
  commit, and the image, the GitHub Release and crates.io wait for it.
- [x] **`CI-12` The `cfg(not(unix))` code was compiled only at tag time.** A
  `cargo check` on Windows and macOS runs on every pull request.
- [x] **`CI-13` Loose ends in CI.** `--locked` on every build, test and clippy
  step; cargo-audit and cargo-deny pinned; a yanked crate fails the audit;
  cache keys per job and per compiler.
- [x] **`TEST-08` Sleeps and a test that could not fail.** The connection-cap
  test polls instead of sleeping, `a_stalled_upload_is_cut_off` checks the
  cut-off, and the unused `tokio-test` is gone.

### Fixed after the review

- [x] **`0.8.1`** 0.8.0 shipped `rustls-webpki` 0.103.15 (RUSTSEC-2026-0382,
  RUSTSEC-2026-0381); `main` has 0.103.16. A patch release carries it with the
  fixes above.
  *Done:* released as 0.9.0 instead, since schema 9 and the stricter
  `restore` make it a minor release.
- [x] **`COR-35` Approving checks the package hash only**
  (`check_shown_builds`): attached files can still change between the page
  and the approval, the pending side of `COR-31`.
  *Done:* the admin page sends a digest of the attached files with the
  build, and an approval answers `409` if they changed.
- [x] **`COR-36` A move removes the membership and writes the tombstone in
  two steps**; a failure between them lets the mirror fetch the version back.
  *Done:* one transaction (`remove_membership_for_good`), used by moves and
  by retention's purge.
- [x] **`COR-37` Blobs a detach leaves are never collected.**
  *Done:* detach runs in `detached()`, and the daily orphan sweep deletes
  blobs no file references.
- [x] **`MNT-06` Two NuGet v3 readers.** The mirror takes `@type` only as a
  string and does not resolve a relative `@id` (`src/mirror.rs`), the
  internalizer does both (`upstream/v3.rs`). Typed structs in `yanuget-pkg`
  would serve both and settle `PERF-06`.
  *Done:* `yanuget_pkg::v3` reads the service index, registration, version
  list, search and catalog leniently; the mirror and the internalizer both
  use it.
- [x] **`INT-35` Overrides apply only `skip`.** `Override::apply` and
  `with_script` have no caller outside tests and `package_parameters` is
  always `None`, while `INT-25` and the README describe all of them.
  *Done:* `sandbox::job` puts the script into a copy of the package and the
  root's parameters into the job, and `assess` applies the downloads and
  rewrites; a dependency's parameters are a warning. `run` and `sync` reach
  these stages once a runner captures (`INT-15`), which the README says.
- [x] **`INT-36` Prelude and downloader hardening.** The prelude passes on any
  value that does not match `^(https?|ftps?)://` exactly (a leading space,
  `https:\\`, a UNC path); it wraps only the first `chocolateyInstall.ps1`;
  the downloader has no overall timeout per file; `write_atomic` in `sync.rs`
  uses a fixed temporary name and follows links. None of it is reachable
  before `run` and `sync` are wired.
  *Done:* the prelude refuses what it cannot redirect (UNC paths,
  `file://host`, other schemes) and wraps every install, upgrade and
  uninstall script; `downloads.file_timeout_secs` bounds a file (4 h
  default); `write_atomic` creates an unguessable temporary file with
  `create_new`.

## Earlier reviews

The reviews of [2026-09-30](reviews/2026-09-30.md) and
[2026-10-19](reviews/2026-10-19.md) are fixed and released in full; their
entries moved to `reviews/` with their ids unchanged.

## For the repository owner


These need settings no commit can change; the release workflow expects them
(see [RELEASING.md](RELEASING.md)):

1. Create the `release` environment (Settings → Environments), restricted to
   the tag pattern `v*` and no branches; required reviewers are optional.
2. Add a tag ruleset protecting `v*`, so only maintainers can create, move or
   delete release tags.
3. On crates.io, set up trusted publishing for `yanuget` (owner `thoscut`,
   repository `yanuget`, workflow `release.yml`, environment `release`), then
   delete the `CARGO_REGISTRY_TOKEN` secret and revoke the token. A crate that
   was never published needs its first publish by hand.
4. Keep `PUBLISH_TO_CRATES_IO` unset until crates.io publishing should run.
5. Give this repository *Write* access to the `thoscut/yanuget` package on
   ghcr (package settings → Manage Actions access).

Exercised by the 0.7.0 release: build provenance for the archives and the
image, and the image's cosign signature (`cosign verify` from SECURITY.md
passes with cosign 3; see `DOC-09`). Not yet exercised: crates.io trusted
publishing (`PUBLISH_TO_CRATES_IO` is unset), and
`scripts/Send-YanugetFile.ps1`, which was reviewed but not run.

---

## Features

Not yet implemented (contributions welcome):

- Additional storage backends (S3, Azure Blob). This needs a streaming
  `PackageContent` variant first; see `DOC-07`.
- Additional database backends (PostgreSQL, MySQL).
- Online vulnerability scanning.
- Native (Windows) PDB indexing.
- Reproducible builds (pinned toolchain, `--remap-path-prefix`, embedded
  timestamps).
- A Chocolatey internalizer, planned in detail under
  [Chocolatey internalizer](#chocolatey-internalizer) (`INT-…`).
- Markdown readmes on the package page. NuGet readmes are Markdown, and
  nuget.org renders them; descriptions are plain text and stay so, in the list
  and on the package page. Sketch: `pulldown-cmark` with a filter over its
  events that emits raw HTML as text and sends every link and image URL
  through `safe_href`; external images as links with their alt text (the CSP
  blocks them anyway); a cap on the HTML produced. This would be the first
  place package data becomes markup, so it needs a documented exception to the
  escaping rule of `MNT-01`, a `SEC-…` entry of its own and an XSS corpus in
  `tests/http_security.rs`. Assessed on 2026-10-02 at one to two days; not
  started.

---

## Chocolatey internalizer

Partly built: the ticked items have landed. A tool that takes a package from the
Chocolatey Community Repository (or any v2 or v3 feed), fetches every file its
scripts download at install time, and publishes a copy into a YANuget feed that
installs without internet access. Drafted on 2026-10-15 against 0.7.0, whose
code every reference below was checked against, after reviewing the plan from
five angles: Chocolatey packaging, security, the YANuget architecture, Windows
operations, and testing. Items are tagged `INT-…`;
[Target releases](#target-releases) proposes where they land.

### Decisions

- **A separate binary, `yanuget-internalize`, in a workspace with the server.**
  The server never runs package content: [SECURITY.md](SECURITY.md) has it
  store and serve what it is given. The tool runs on a worker of its own (its
  own machine, or at least its own user) and reaches the server only through
  the push and attached-file APIs, with an ordinary push key.
- **Dynamic capture, not static analysis.** Each package runs in a disposable,
  offline Windows guest with Chocolatey's download and install helpers
  replaced by recorders. Rust has no usable PowerShell parser, and real scripts
  build their URLs from splatted hashtables, string interpolation and
  `$env:ChocolateyPackageVersion`. A static scan stays as a cross-check only
  (`INT-23`).
- **KVM first, Windows containers second.** The default sandbox is a Windows
  Server Core virtual machine under KVM/QEMU on a Linux host: no network
  device, a throwaway overlay on a golden image, and control through the QEMU
  guest agent over virtio-serial. It needs no Windows host, runs on the kind of
  machine YANuget already runs on, isolates as well as Hyper-V, and reduces the
  network of the validation install (`INT-26`) to nftables rules on a bridge.
  Windows containers under Hyper-V isolation are the backend for operators who
  have a Windows host (`INT-29`). Both sit behind the `Sandbox` trait
  (`INT-10`) and must produce the same capture.
- **The offline sandbox is the test.** A script that needs the network for
  anything but a recorded helper call fails there exactly as it would on an
  internal client, and is reported rather than guessed at.
- **Rewrite by prelude, not by editing the script.** The internalized
  `chocolateyInstall.ps1` is a generated prelude (a map from original to
  internal URL, and wrappers with the helpers' names) followed by the original
  script, byte for byte. How a script builds its URLs no longer matters, and
  the diff a reviewer reads is the prelude.
- **Installers become attached files**, stored once under their SHA-256 and
  served from `/files/{id}/{version}/{name}`. They are the upstream bytes, so
  the original checksum still holds. Embedding into the `.nupkg` is a later
  option for other feeds (`INT-28`).
- **Same id, same version.** Meta packages pin their `.install` package exactly
  (`[1.2.3]`), so a copy keeps the upstream id and version. Re-internalizing may
  replace a version that is still pending (`INT-04`); an approved version is
  never changed.
- **Nothing reaches clients unreviewed.** The tool pushes into an intake feed
  with `requires_approval = true` that `promotes_to` the production feed. Its
  report travels inside the package (`INT-21`, `INT-30`).

Not goals: parity with Chocolatey for Business, a PowerShell evaluator,
running package scripts under PowerShell on Linux (they take other branches
than under Windows PowerShell 5.1 and yield plausible but wrong URLs), a silent
fall-back to the original URL at install time, and any judgement on whether a
vendor's license allows redistribution inside an organisation. That stays with
the operator, and the docs say so.

### Server: 0.8.0

Every server change the tool relies on, in one release (see
[Target releases](#target-releases)), each useful on its own.

- [x] **`INT-01` Extract a `yanuget-pkg` crate.** Move `version.rs`,
  `nuspec.rs`, `nupkg.rs` and the models they use into a workspace crate with
  no axum or sqlx dependency and its own `thiserror` error type, converted into
  `crate::error::Error` by the server. Today `nupkg.rs` and `nuspec.rs` return
  the server's error, which carries `IntoResponse`. `pub use` re-exports keep
  the server's call sites unchanged. Add a `VersionRange` (parse, normalize,
  `contains`, best match), which does not exist yet: `Dependency::version_range`
  is a plain string. Behind a feature, since it brings reqwest: the upstream
  address policy (`proxy::is_private_ip_addr` and the filtering resolver in
  `src/mirror.rs`), for `INT-17`. It also holds the report schema of
  `INT-21`, so the tool that writes `internalizer/report.json` and the server
  that reads it (`INT-30`) share one definition.
  *Done when:* the server builds against the crate with no change in
  behaviour, `cargo deny` and `cargo audit` cover the workspace, and
  `VersionRange` has property tests over NuGet's interval notation.
  *Landed:* the crate is `crates/yanuget-pkg` (address policy in `address.rs`,
  the resolver behind the `net` feature). The report schema (version 1) landed
  there with `INT-30`, as `report.rs`.
- [x] **`INT-02` A streaming `.nupkg` rewriter** in `yanuget-pkg`. Copy
  unchanged entries raw, without recompressing; replace or add named entries;
  rewrite the `.nuspec`; keep the OPC parts (`[Content_Types].xml`,
  `_rels/.rels`, the `.psmdcp` core properties) consistent, with a content type
  for any new extension. Drop `.signature.p7s`, which any change invalidates.
  Peak memory must not depend on package size.
  *Done when:* an unchanged rewrite keeps every entry byte-identical, Chocolatey
  CLI installs a rewritten package, and a 5 GB entry passes in constant memory.
  *Landed:* `yanuget_pkg::rewrite`. An empty rewrite keeps every entry's
  compressed bytes and CRC; a 5 GiB entry passes with under 64 MiB of growth
  (an ignored test, `cargo test -p yanuget-pkg -- --ignored`), and Python's
  `zipfile` reads it back. Every copy is checked against the source after
  writing: `zip` 2.x left the ZIP64 sizes of a raw copy at zero, which the
  rewriter patched until `zip` 9 (taken for RUSTSEC-2026-0367/0368/0369)
  wrote them itself. Not yet checked: an install with Chocolatey CLI, which
  needs a Windows host; that belongs with the regression corpus (`INT-22`).
- [x] **`INT-03` Per-feed `allowed_extensions` for attached files.** `[files]`
  is global, and its defaults rightly exclude `exe` and `msi`. Let a feed
  override the list (`[feeds.files]`), so executables are accepted by the
  internal Chocolatey feed only. Typical additions: `exe`, `msi`, `msix`,
  `msu`, `appx`, `vsix`, `ps1`.
  *Done when:* `yanuget config check`, `yanuget.example.toml` and
  `docs/configuration.md` cover it, and a feed without the override behaves as
  today.
- [x] **`INT-04` Replace a pending version.** With overwrite off, a second push
  of the same version is `409` even while the first is pending and has never
  been served. Add `allow_overwrite = "pending-only"`: a push may replace a
  version that is pending in every feed holding it, together with its attached
  files, under the version lock (`src/locks.rs`). A version approved anywhere
  stays immutable.
  *Done when:* `yanuget config check` accepts the new mode, the old files are
  gone after a replace, a version approved in any feed is refused, and the race
  with a concurrent approve is tested.
- [x] **`INT-05` Pin the intake flow with a test.** According to
  `src/web/hosted.rs`, a file can be attached to a pending version
  (`version_in_feed` checks membership only) and is withheld until approval
  (`download`). Cover it end to end: push to a gated feed, attach, download is
  refused, approve, download succeeds; then the same after `promote`.
- [x] **`INT-30` Show the report in the admin area.** The package page of a
  pending version, which since 0.7.0 says where the version is approved, reads
  `internalizer/report.json` with the seek-based `nupkg::extract_file` and
  shows the upstream source, each original URL with
  its file, the checksum status, the warnings and the prelude. It flags a
  version whose report lists files that are not attached, so an interrupted run
  is not approved by accident. The entry is size-capped and escaped like every
  other sink (`SEC-24`); a package without it renders as today. Tested
  against fixture reports, because the tool that writes real ones ships after
  0.8.0.
  *Landed:* schema version 1 is `yanuget_pkg::report` (`Report::from_slice`,
  1 MiB cap, unknown fields and enum values tolerated, another schema version
  refused by name). The page reads the ten newest pending versions, checks
  each listed file's presence, size and SHA-256, and asks before approving one
  that does not match; the selection's Approve does not check.
- [ ] **`INT-31` Attached files on read-gated feeds.** Chocolatey's helpers do
  not send source credentials to download URLs *(to verify)*, so a feed with a
  `read_api_key` cannot serve its files to `choco install`. Proposal: a token in
  the file URL, an HMAC over feed, id, version and name under a server secret,
  that the prelude carries. Whoever can read the package can read its files:
  the same permission, no wider. Rotation needs a list of secrets, because
  changing the only one breaks every internalized package. The secrets are set
  like every key since 0.7.0: in TOML, from the environment, or from a file
  through `_FILE`. On a read-gated feed the answer to an attach
  (`{name, size, sha256, url}`) carries the URL with its token, so the tool
  never computes one. Verify the premise first: if the helpers do send source
  credentials, the item shrinks to a test. Either way, a security review
  before it is built.
  *Premise checked* on 2026-10-19 against Chocolatey CLI's helpers
  (`chocolatey/choco` at `c8659d4`): `Get-WebFile` and `Get-WebHeaders` set
  only `CredentialCache::DefaultCredentials` (the Windows logon, for
  integrated authentication) and proxy credentials; the source's user name
  and password never reach PowerShell, and none of the variables the CLI sets
  for a script carries them. So a read-gated feed cannot serve its files to
  `choco install` today, and the item stands. The one alternative the helpers
  allow: `-Options @{Headers = @{…}}` adds any request header, so a prelude
  could send `Authorization` from a variable set on each client. That puts a
  read key on every client and needs per-client setup, where the token needs
  none; it stays a fallback.
- [x] **`INT-32` Read access to pending versions.** A per-feed
  `preview_api_key` that can download pending packages and their files and do
  nothing else: no approve, no push. For `INT-26`, and for anyone who wants to
  test a version before approving it. Set like every other key:
  `YANUGET_FEED_{NAME}_PREVIEW_API_KEY`, or the same with `_FILE`.

### Internalizer: first version

Goal: internalize the dependency closure of a package into a gated feed, for
packages whose downloads go through Chocolatey's helpers.

- [x] **`INT-10` Crate, command line, configuration.** `yanuget-internalize`
  with `plan` (capture and report, publish nothing) and `run` (capture,
  download, rewrite, publish). Configuration is layered like the server's
  (defaults, TOML, `YANUGET_INTERNALIZE_*`, `_FILE` for secrets) with
  `deny_unknown_fields`, and `config check` checks it the way
  `yanuget config check` does since 0.7.0 (every value, the target feed,
  `/dev/kvm`, the golden images), exiting `1` on an error. The sandbox sits
  behind a `Sandbox` trait with a KVM backend (`INT-15`) and a Windows
  container backend (`INT-29`), so planning, capture parsing, downloading,
  rewriting and publishing build and are tested on Linux CI against a fake
  sandbox. Exit codes tell "done", "needs attention" and "failed" apart for
  pipelines.
  *Landed:* `crates/yanuget-internalize`, `publish = false` until its first
  release. Exit codes: `0` done, `1` failed, `3` needs attention (`2` stays
  clap's usage error). A package is `id` or `id@version`. Every setting is
  also `YANUGET_INTERNALIZE_` plus its TOML path in capitals; the push key
  takes `_FILE`, and `config check` warns about a variable that is not a
  setting. The check asks the target feed for its service index, which must
  list `PackagePublish` (`--offline` skips it), and checks `/dev/kvm`, QEMU
  and the qcow2 images, or a Windows host and a digest-pinned image. Both
  backends check their host and refuse every capture until their runners
  land, so `plan` and `run` stop before they fetch anything.
- [x] **`INT-11` Upstream client.** The Chocolatey Community Repository over v2
  OData (`FindPackagesById()`, package download), any v3 feed through its
  service index, and the auth modes of `[feeds.mirror]`. Each download is
  verified against the feed's `PackageHash`. A local cache keyed by hash,
  backoff on `429`, and a low default concurrency: the community repository
  rate-limits heavy users. Like a mirror since 0.7.0, it logs once when an
  upstream stops answering and once when it answers again.
  *Landed:* `yanuget_internalize::upstream`. v2 pages through
  `FindPackagesById()` (checked against the community repository's real
  answers: paging, `PackageHash`, `Dependencies`); v3 reads the registration,
  or the flat container without one. A download must match the feed's hash
  and carry a manifest with the id and version asked for before it enters
  the cache (`cache_dir/packages`, keyed by that hash and re-hashed on use).
  Most v3 feeds state no hash: their packages are cached by identity and
  marked unverified, for the report to say so. The mirror's address policy
  and its hop-by-hop redirects apply, credentials go to the upstream's
  origin only, `429`/`503` are retried after `Retry-After` or a doubling
  backoff. `config check` probes the upstream too; `plan` resolves its
  packages before it stops.
- [x] **`INT-12` Dependency closure.** Resolve dependencies with
  `VersionRange` (`INT-01`), extension packages (`*.extension`) included, since
  scripts call their helpers. Pick the versions Chocolatey CLI v2 would install
  for the same command *(to verify against the client)*, and skip what the
  target already holds. `plan` shows the whole closure before anything is
  downloaded.
  *Landed:* `yanuget_internalize::closure`. Chocolatey CLI v2's source
  (`NugetService`, `NugetCommon`) runs NuGet's resolver with
  `DependencyBehavior.Highest` and `AnyFramework`, and lets pre-release
  dependencies in only for a pre-release root; the closure does the same,
  with backtracking when ranges clash. As NuGet's `ResolverComparer` orders
  candidates, an unlisted version is taken only when no listed one fits. A
  dependency on `chocolatey` is left to
  the CLI in the image. A v3 feed with only a flat container gives its
  dependencies through the manifests. `plan` marks what the target serves
  already, read from its flat container; a read-gated target cannot say, and
  pending versions are not listed, so `INT-20` still takes a `409` as "already
  internalized". Checked against the community repository: `git`,
  `vscode`, `googlechrome`, `vlc` and `dotnet-8.0-desktopruntime` (eleven
  packages through `vcredist140` and the `KB` updates) resolve.
- [ ] **`INT-13` Golden images.** `contrib/internalizer/` builds, unattended
  and without network access, two qcow2 images from a Windows Server Core ISO
  (Windows PowerShell 5.1, which package scripts target). The *stock* image has
  the virtio-win drivers, the QEMU guest agent and a pinned Chocolatey CLI
  installed from a local `.nupkg`, with no sources configured and the guest's
  background traffic turned off (Windows Update, time sync, telemetry, LLMNR,
  NetBIOS, WPAD). The *capture* image is an overlay on it, with the stock image
  as backing file, that adds the capture helpers of `INT-14`. Updates come
  from pinned offline packages, not Windows Update. Each image ships with a
  saved memory state of the booted, idle guest, so a run restores instead of
  booting. Inputs are pinned by hash (ISO, virtio-win, Chocolatey, updates);
  the report records the image hash and the Chocolatey version. The guests
  need a regular Windows license; evaluation media are for development and CI
  (`INT-34`).
  *Done when:* a rebuild from the same inputs gives the same results on the
  regression corpus, and a restored guest answers its first guest-agent
  command within seconds *(target to verify)*.
- [ ] **`INT-14` Capture helpers.** Replace, inside the `chocolateyInstaller`
  module so that calls from other helpers and from extensions are caught too,
  at least `Get-ChocolateyWebFile`, `Get-WebFile`, `Get-FtpFile`,
  `Install-ChocolateyPackage`, `Install-ChocolateyZipPackage`,
  `Install-ChocolateyVsixPackage`, `Install-ChocolateyPowershellCommand`,
  `Install-ChocolateyInstallPackage` and `Get-ChocolateyUnzip`. Each appends
  one JSON line (package, URL, 64-bit URL, checksums and their types, file
  type, target path, and the caller from `Get-PSCallStack`, so a call made from
  inside an extension module is told apart) and returns a plausible result: a
  placeholder file where the script expects one, exit code 0. The script then
  goes on to its next download. Installers never run.
- [ ] **`INT-15` KVM sandbox runner.** One guest per root package, restored
  from the capture image's saved state onto a fresh overlay, with no network
  device at all. The upstream closure arrives as a read-only ISO 9660 image,
  found in the guest by its volume label; nothing is shared from the host (no
  virtio-fs, no 9p). The runner sets the guest clock (`guest-set-time`, since
  the saved state is stale), starts a plain `choco install {id} --version {v}
  --source {iso} -y` through `guest-exec`, polls `guest-exec-status` under a
  wall-clock timeout, reads the capture back with the guest agent's file calls
  under a size cap, then powers off and deletes the overlay whatever happened.
  Chocolatey sets the environment, installs dependencies and extensions, and
  runs each script as a client would; captures are attributed through
  `$env:ChocolateyPackageName`. QEMU runs with `-sandbox on`, CPU and memory
  limits and a bounded number of guests in parallel. The runner refuses to
  start without `/dev/kvm`: software emulation is too slow to be useful. QEMU
  is driven over QMP and the guest-agent socket directly; libvirt only if that
  proves necessary. Two systemd units in `contrib/systemd/`, modelled on the
  `yanuget.service` of 0.7.0: `yanuget-internalize.service` holds the push key
  and the download cache and cannot open `/dev/kvm`; `yanuget-sandbox.service`
  runs the guests as a user of its own, with `DeviceAllow=/dev/kvm` and no key,
  and takes jobs over a Unix socket. It cannot keep `PrivateDevices=yes`
  (`DevicePolicy=closed` instead); whether QEMU under KVM runs with
  `MemoryDenyWriteExecute=yes` is *(to verify)*.
- [x] **`INT-16` Treat the capture as untrusted input.** Untrusted code wrote
  it: a versioned schema, strict types, caps on line length, entry count and
  total size, no control characters, `http`/`https` URLs only, and attribution
  to a package of the closure. Anything else marks the package
  `needs-override`; nothing is repaired.
  *Landed:* `yanuget_internalize::capture`, schema version 1, which the
  helpers of `INT-14` write: one JSON object a line with `schema`, `package`,
  `version`, `helper`, `url`, `url64`, `checksum`, `checksumType`,
  `checksum64`, `checksumType64`, `fileType`, `fileFullPath` and `caller`,
  unknown fields refused. Caps: 16 KiB a line, 1,000 calls, 16 MiB in all,
  4,096 characters a URL, 1,024 a path. Control characters and the
  bidirectional controls are refused in every string. A checksum must be hex
  of its type's length; one without a type is MD5, as Chocolatey takes it.
  The helper must be one of the nine that record. A rejection lists every
  reason, up to twenty. The caller's path tells the package's own scripts
  (`lib\{id}\`) from an extension's module (`extensions\{name}\`) for
  `INT-21`.
- [x] **`INT-17` Downloader.** Stream each captured URL to disk while hashing
  with SHA-256 and with the algorithm the script declares (MD5, SHA-1, SHA-256,
  SHA-512); a declared checksum that does not match fails the package. A call
  with no checksum, or only MD5 or SHA-1, is marked trust-on-first-use in the
  report; the prelude pins SHA-256 either way (`INT-19`). The mirror's address
  policy applies (`INT-01`): private, loopback and link-local targets are
  refused unless allow-listed, after every redirect too. The URLs come from
  untrusted scripts, and a package pointing at an internal host would
  otherwise get that host's answer published into the feed. Size cap, retries,
  resume.
  *Landed:* `yanuget_internalize::download`, configured in `[downloads]`
  (`concurrency`, `timeout_secs`, `max_file_bytes`, default 8 GiB,
  `resume_attempts`, `allow_private_hosts`, `proxy`). Files land in
  `cache_dir/files` under their SHA-256; a declared SHA-256 finds a file
  there without a download, after hashing it again. The upstream's HTTP client
  is shared, without credentials: the address policy on every hop, no
  https-to-http redirect, `429`/`503` waited out. An allow-listed host is
  resolved without the filter. A transfer that breaks off is resumed with
  `Range` and `If-Range` (a strong `ETag`, else `Last-Modified`) where the
  host offers byte ranges, and restarted otherwise. A failed download leaves
  nothing in the cache.
- [x] **`INT-18` File names.** Attached file names are 1–128 characters of
  `A-Z a-z 0-9 . _ -` with an allowed extension; upstream names carry spaces
  and `+`, or there is no name at all (`?product=…&os=win64`). Keep the
  original name where it is valid; otherwise derive it from
  `Content-Disposition`, the URL and the captured file type, check the
  extension against the content (`MZ`, the OLE header of `.msi`, `PK`), and
  break collisions with a hash prefix. A file the feed's `allowed_extensions`
  refuses is reported, not renamed to pass.
  *Landed:* `yanuget_internalize::naming`. The server's rule moved to
  `yanuget_pkg::files`, so the server's attach check and the tool's names are
  one definition. The URL's own name is kept when it is valid and its
  extension fits the content. Otherwise the name comes from
  `Content-Disposition` (`filename*` first), the URL after redirects, the URL
  as captured, and last the package (`git.install-2.51.0`). Disallowed runs
  become `_` and long stems are cut. The extension is the name's own if it
  fits the content, else the captured `fileType` if that fits, else the
  content's (`MZ`, OLE, `PK`, 7z, `MSCF`, gzip). The same file keeps the
  name it was given, and a different file on a taken name gets the first
  eight hex digits of its SHA-256 in front.
- [x] **`INT-19` Prelude.** The wrappers are proxy commands
  (`[System.Management.Automation.ProxyCommand]::Create`) generated from the
  pinned Chocolatey version's helper metadata, so positional calls and
  parameter aliases bind exactly as they do against the real helpers. A
  wrapper looks the URL up and calls the original
  (`chocolateyInstaller\Install-ChocolateyPackage`) with the internal URL and
  the stored file's SHA-256 as checksum. A URL not in the map stops the install
  with a message naming the package, never a fall-back to the internet. The map
  is keyed by the SHA-256 of the original URL, so no text a package controls is
  ever emitted as PowerShell; the report carries the originals. The server's
  base URL sits in one variable at the top, overridable by an environment
  variable for a host move.
  *Landed:* `yanuget_internalize::prelude`. The package's
  `chocolateyInstall.ps1` becomes a loader that dot-sources
  `internalizer/prelude.ps1` and then the package's own script, which moves
  to `chocolateyInstall.upstream.ps1` beside it. The proxy commands are made
  on the client at install time from the helpers it has, not from pinned
  metadata. That binds against the Chocolatey that actually runs the
  script, and needs no PowerShell on the worker. The wrapper calls
  `chocolateyInstaller\{helper}` by its module-qualified name, because
  `ProxyCommand::Create` names a module's function without its module and
  would call the wrapper itself. `Get-WebFile` takes no checksum, so its file
  is checked once it is written. The map's keys are the SHA-256 of the URL
  as the report records it. The wrapper tries the URL as written, then
  `[Uri]::AbsoluteUri`. The variable is `YANUGET_INTERNALIZED_FEED_URL`.
  Tested with PowerShell 7 against a real server and a stand-in for the
  helpers. Windows PowerShell 5.1 and the real helpers come with the
  regression corpus (`INT-22`).
- [x] **`INT-20` Publish.** Rewrite the package (`INT-02`) with the new
  `chocolateyInstall.ps1` and the report, push it to the intake feed, attach
  the files, and check that the feed withholds the version; warn when it is
  served at once. Attach is idempotent (same content, `200`), so a re-run
  completes a version whose files are missing. A `409` on push means "already
  internalized" unless `--replace-pending` (`INT-04`). The tool takes a push key
  and nothing more. `--replace-pending` needs YANuget 0.8.0: when
  `/health/status` is reachable the tool checks the server version up front,
  otherwise its error for a refused replace names the version it needs.
  *Landed:* `yanuget_internalize::publish`. The package is rewritten with
  `internalizer/report.json` and the entries `INT-19` will supply, pushed to
  `api/v2/package`, and its files attached with `x-checksum-sha256`. Whether
  the feed serves the version comes from its flat container, which lists
  approved versions only. After a `409` the files are attached only while the
  version is withheld: an attach is not reviewed, so a version the feed
  serves, or one a read-gated feed cannot tell about, gets nothing. On a
  `pending-only` feed every push replaces a pending version, flag or not; the
  push key cannot see it to ask first. An attach is tried three times; a
  request may take `target.timeout_secs` plus a second per MiB. The feed's
  file types are `target.allowed_extensions`. Tested against the server
  itself: held back until approved, report inside, a re-run completing a
  pending version, a refused type and a changed file failing. `run` calls it
  once a sandbox backend captures.
- [x] **`INT-21` Report.** Per package: `internalized`, `unchanged` (no
  downloads: embedded or meta packages), `needs-override` (network use outside
  the helpers, a script that failed, an unmapped call, a download from inside
  an extension module) or `failed`, each with reasons. It lists the upstream
  source and hash, the image hash and Chocolatey version, the captured calls,
  the files with sizes and hashes, the checksum status and the warnings. As
  JSON for pipelines, as text for people, and inside the package as
  `internalizer/report.json`, covered by the package hash and carried along by
  promotion, copy, `yanuget migrate` and `yanuget backup`. The schema is
  versioned and defined in `yanuget-pkg` (`INT-01`); YANuget 0.8.0 reads
  version 1, so fixing it may add fields but not change the ones 0.8.0 reads,
  and a reader ignores fields it does not know.
  *Landed:* `yanuget_internalize::report` and `::assess`, schema version 1
  unchanged. A failure outranks an override, and every reason is listed,
  failures first. A failed sandbox fails every package of the capture; an
  install that failed (exit codes other than 0, 1641 and 3010), ran out of
  time or left a rejected capture marks every package of it
  `needs-override`; a helper called from an extension's module or from
  outside the package's scripts, or a file of a type the feed does not take,
  marks its own package. `url64` that repeats `url` is one download. Text for
  people, and `{"outcome", "packages"}` as JSON. Network use outside the
  helpers needs the sandbox's own record (`INT-15`).
- [ ] **`INT-22` Regression corpus and CI.** A pinned list of the most
  installed community packages (id, version, upstream hash), cached rather than
  committed, run through `plan` whenever the capture helpers or the prelude
  change. Each result is compared with the last run, and the share per status
  is tracked. Golden files for generated preludes; fuzzing for the capture
  parser and the rewriter. A nightly job runs the KVM backend for real on a
  KVM-capable Linux runner, with images built from evaluation media. Whether
  GitHub's hosted Linux runners expose `/dev/kvm` and can cache a Windows image
  of that size is *(to verify)*; otherwise a self-hosted runner. The container
  backend (`INT-29`) gets a job on a hosted Windows runner with process
  isolation, and the release checklist adds one Hyper-V run.

### Internalizer: production

- [x] **`INT-23` Static cross-check.** Scan every package script for URL-like
  literals, including `chocolateyUninstall.ps1` and
  `chocolateyBeforeModify.ps1`, which capture does not run, and warn about each
  one capture did not see. A regex is enough, since it only ever raises a
  warning.
  *Landed:* `yanuget_internalize::crosscheck`. Every `.ps1`, `.psm1` and
  `.psd1` entry up to 4 MiB (UTF-8, or UTF-16 by its byte-order mark) is
  scanned for `http`/`https` URLs outside `#` and `<# #>` comments; a larger
  one is named in a warning instead. A URL built from variables is seen when
  a captured URL starts with its literal part, and one whose host is a
  variable cannot be compared. At most twenty warnings a package, then a
  count. `run` calls it once a sandbox backend captures.
- [ ] **`INT-24` Environment variants.** Capture once per architecture
  (`$env:ChocolateyForceX86`), configured locale and set of package
  parameters, and map the union of URLs. A package that picks its installer by
  system language is internalized for the configured locales only, and the
  report says so. Locales are variants of the golden images, because changing
  the system locale needs a reboot, which would throw away the saved state;
  whether `Set-Culture` before the run is enough for most scripts is
  *(to verify)*.
- [x] **`INT-25` Overrides.** One TOML file per package id
  (`deny_unknown_fields`): extra URLs, URL rewrites, package parameters for
  capture, a replacement script, or `skip` with a reason. Every override used
  is in the report; one that no longer matches a capture is a warning, so stale
  overrides surface.
  *Landed:* `yanuget_internalize::overrides`, in `overrides_dir`, the file
  named for the id. `versions` narrows it to a range; `skip` stands alone and
  `plan` marks the package. `package_parameters` go to the capture job as
  `--params` for the root. The script (a path inside the directory, 1 MiB at
  most) replaces the package's `chocolateyInstall.ps1` under its own name
  before the capture, so capture and feed see the same script. Downloads may
  carry a SHA-256; a rewrite fetches a captured URL from elsewhere while the
  package keeps asking for the original. The report gained `overrides`, a
  list of lines (a field added to schema version 1, which 0.8.0 ignores).
  One bad file fails the set; `config check` loads it.
- [ ] **`INT-26` Validation install.** After publishing, restore a guest from
  the *stock* image, without the capture helpers, and install each new pending
  version with its real installers. The guest's only network device sits on an
  isolated bridge whose nftables rules accept the YANuget server's address and
  port and log and drop everything else; the server's name goes into the
  guest's `hosts` file, so no DNS is needed. A drop that an idle baseline run
  of the same image also shows is ignored; any other fails the package, as
  does a failing exit code from Chocolatey. This catches what capture cannot
  see: web installers that pass capture and fetch their payload when they run.
  Needs read access to pending versions (`INT-32`). On the container backend
  the same restriction needs its own design (`INT-29`).
- [x] **`INT-27` Keep feeds current.** A watch list of ids, optionally with
  ranges; `sync` finds new upstream versions, internalizes them into the intake
  feed and reports what waits for approval. Meant as a systemd timer on the
  worker (a scheduled task with the container backend); idempotent and safe to
  interrupt. Each run writes its counts per status and the age of the oldest
  pending version in the Prometheus text format of `[monitoring]`, as a file
  for node_exporter's textfile collector, so a stuck sync shows up next to the
  server's metrics.
  *Landed:* `yanuget_internalize::sync` and `[sync]` in the configuration.
  Each watch entry (`[[package]]`, an id and an optional `versions` range)
  follows its newest listed version, a pre-release only under a pre-release
  bound. A state file records each version as `pending`, `approved`,
  `needs-override` or `failed`; it is saved after every package and guarded
  by a lock file. A version is never pushed twice; a failure is tried again,
  an override need once any override changes. Approval is read from the
  target's flat container; a pending version stops counting once a newer one
  in its range is approved or an override skips it. The metrics file adds the
  time and outcome of the last sync. `sync --dry-run` changes nothing;
  `config check` loads the watch list and the state. Fixing an approved
  version stays a manual `run`. The pipeline is behind a trait, so `sync`
  records each version as failed until a sandbox backend captures.
- [ ] **`INT-28` Embedding for other feeds.** For targets that are not YANuget:
  store the files under `tools/` in the package, streamed (`INT-02`), with the
  prelude mapping to paths under the package folder. Whether the helpers take a
  local path or `file:` URL for `-Url` is *(to verify)*; if not, the wrappers
  call `Install-ChocolateyInstallPackage` or `Get-ChocolateyUnzip` with the
  local file. A package over the target's upload limit fails with a clear
  message.
- [ ] **`INT-29` Windows container backend.** For operators with a Windows
  host instead of KVM: a Server Core container image pinned by digest, with the
  same Chocolatey CLI and capture helpers as the golden images (`INT-13`). One
  container per root package under Hyper-V isolation (process isolation only
  in CI), `--network none`, the closure mounted read-only as a folder source,
  one writable output directory, CPU and memory limits, a wall-clock timeout,
  and removal whatever happened; Docker Engine API or `docker` CLI is decided
  in the PR. The regression corpus (`INT-22`) runs against both backends and
  must give the same capture. A validation install (`INT-26`) on this backend
  needs a way to restrict a container's network *(to design)*.

### Server: later

- [ ] **`INT-33` Internalization requests.** Admins request a package in the
  admin area; the worker polls a small JSON API with a dedicated worker key
  that can read the queue and report status, and pushes with its push key as
  before. The server still runs nothing; the queue only saves a trip to the
  worker. Its errors are JSON for a request that asks for `application/json`,
  as admin errors are since 0.7.0. The queue needs a numbered schema migration
  (`src/database/sqlite/schema.rs`) and reports its length and oldest request
  in `/metrics` and `/health/status`. Whether it is built at all is open (see
  *Open questions*).

### Documentation

- [ ] **`INT-34` Documentation and threat model.** `docs/internalizer.md` in
  the MkDocs nav: the intake feed and approving with `yanuget admin approve`,
  the worker host with KVM or Windows containers and the two systemd units of
  `INT-15`, building the golden images, the Windows license the guests need,
  overrides, and the operator's responsibility for the licenses of packaged
  software. A section in SECURITY.md: the worker is its own trust zone, holds a
  push key for the intake feed only, runs guests as a user that holds no key,
  and what it produces is reviewed before it reaches clients.

### Open questions

- ~~**Fixing an approved version.**~~ *Decided 2026-10-20:* delete and
  re-push. The package fix version (`1.2.3.20261015`) would break the exact pin
  of a meta package on its `.install` package, so an approved version is
  removed for good (from the admin area, or with `hard_delete_enabled`) and
  internalized again with `run`. `sync` never re-pushes a version it handled.
- **Downloads from extension modules.** The prelude only redirects calls made
  by the package's own script. A helper inside an extension that downloads
  (the Visual Studio extension's bootstrapper, for example) is captured but not
  redirected, and ends as `needs-override`. Is a supported way to intercept
  those worth its complexity?
- **Supported Chocolatey CLI versions.** The wrappers are generated from one
  version's helpers. Which client versions must an internalized package
  support, and how is that tested?
- **Releases.** [Target releases](#target-releases) assumes the tool is
  versioned on its own. Open: a tag prefix for it (`internalize-v*` next to
  `v*`), crates.io trusted publishing per crate, whether `yanuget-pkg` is
  published at all, and a changelog of its own or a section in `CHANGELOG.md`.
  RELEASING.md and the release workflow follow from the answers.
- **Licensing the guests.** Which Windows license covers a golden image that
  is restored thousands of times a month, and may evaluation media run in CI?
  The answer decides whether KVM can be the default for every operator, and is
  needed before `INT-13` ships in a release.
- **Requests on the server or in the watch list?** `INT-33` needs a schema
  migration (step 11 in `src/database/sqlite/schema.rs`; the next release
  takes step 10), after which older
  builds refuse the database (`check_known`), and it widens `PackageDatabase`
  for every future backend. The watch list of `INT-27`, kept in a reviewed
  repository, covers the same need without touching the server; what is lost
  is a request button in the admin area.
- **Names.** `yanuget-internalize` and `yanuget-pkg` are placeholders.

### Target releases

A proposal on top of 0.7.0, in the order the items depend on each other.

| Release | Items | Notes |
| --- | --- | --- |
| YANuget 0.8.0 | `INT-01`–`INT-05`, `INT-30`, `INT-32` | Released 2026-10-20, together with the fixes from the review of 2026-10-19, some of which need attention when upgrading (see CHANGELOG.md). |
| YANuget 0.9.0 | review fixes of 2026-10-19 and 2026-10-27 | Released 2026-10-28, schema 9. |
| YANuget, after 0.9.0 | `INT-31`, the server side of `INT-21`, the fixes of 2026-11-03 (schema 10) | The rest of what the tool needs. `INT-31` waits on the security review of its token design. |
| internalizer 0.1.0 | `INT-10`–`INT-22` | KVM backend; needs YANuget 0.8.0, and the release with `INT-31` for read-gated feeds. |
| internalizer 0.2.0 | `INT-23`–`INT-28` | Needs nothing beyond YANuget 0.8.0. |
| internalizer 0.3.0 | `INT-29` | Optional. |
| YANuget, later | `INT-33` | Only if the open question is answered for it; needs a schema migration. |

`INT-34` is not a release of its own: each release ships the part of
`docs/internalizer.md` and SECURITY.md that covers it.

### Sizing

Rough, for one developer who knows the codebase and Chocolatey packaging:

| Part | Person-weeks |
| --- | --- |
| Server, 0.8.0 (`INT-01`–`INT-05`, `INT-30`, `INT-32`; `INT-31` still open) | 5–7 |
| First version (`INT-10`–`INT-22`) | 10–12 |
| Production (`INT-23`–`INT-28`) | 6–8 |
| Windows container backend (`INT-29`), optional | 2–3 |
| Server, later (`INT-33`) | 1–2 |

The KVM backend costs about two weeks more in the first version than
containers would have, mostly for the unattended image build and the guest
agent; the validation network of `INT-26` gets cheaper in return. 0.7.0
changes none of this: the systemd split in `INT-15` and the `config check`
of `INT-10` fit inside the ranges. Documentation (`INT-34`) is inside each
row. Bringing `INT-30`–`INT-32` forward into 0.8.0 changes when that work
happens, not how much of it there is.
