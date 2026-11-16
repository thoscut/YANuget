# yanuget-pkg

NuGet package handling shared by the [YANuget](https://github.com/thoscut/yanuget)
server and its tools, with no web framework or database underneath:

- `version`: NuGet versions (`NuGetVersion`), parsing, normalization and ordering.
- `range`: NuGet version ranges (`VersionRange`) in interval notation.
- `nuspec`: a bounded, namespace-aware `.nuspec` parser.
- `nupkg`: reading `.nupkg` archives defensively (bounded entries, no
  traversing names, none that collide once case-folded or percent-decoded)
  and extracting single entries by seeking; `extracted_name` is the name a
  NuGet client writes an entry to disk under.
- `rewrite`: rewriting a `.nupkg` in constant memory. Unchanged entries are
  copied raw; entries and the manifest can be replaced, added or removed; the
  content types and core properties follow, and the signature is dropped.
- `address` (and, with the `net` feature, a guarded reqwest resolver): which
  hosts and addresses an upstream fetch must not reach.
- `v3`: the NuGet v3 documents an upstream serves (service index,
  registration, flat container, search, catalog), read leniently.
- `fetch`: helpers for fetching from an upstream: a URL as it appears in a
  log line, a base URL that relative paths extend, and a body that may be
  gzipped, read under a size cap.
- `files`: the rule for the names of files attached to a package version,
  which the server enforces and the internalizer names its downloads by.
- `models`: the manifest data a `.nuspec` declares (package types,
  dependency groups), independent of how it is stored.
- `text`: the characters that make untrusted text read differently from what
  it is, refused or made visible wherever a report shows it.
- `report`: the internalizer report (`internalizer/report.json`), schema
  version 1, shared by the tool that writes it and the server that shows it.

Errors are this crate's own `yanuget_pkg::Error` (module `error`, with
`error_chain` to print an error with each of its causes once); the server
converts them into its HTTP errors.
