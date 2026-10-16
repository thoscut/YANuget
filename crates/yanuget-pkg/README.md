# yanuget-pkg

NuGet package handling shared by the [YANuget](https://github.com/thoscut/yanuget)
server and its tools, with no web framework or database underneath:

- `version`: NuGet versions (`NuGetVersion`), parsing, normalization and ordering.
- `range`: NuGet version ranges (`VersionRange`) in interval notation.
- `nuspec`: a bounded, namespace-aware `.nuspec` parser.
- `nupkg`: reading `.nupkg` archives defensively (bounded entries, no
  duplicate or traversing names) and extracting single entries by seeking.
- `address` (and, with the `net` feature, a guarded reqwest resolver): which
  hosts and addresses an upstream fetch must not reach.
- `report`: the internalizer report (`internalizer/report.json`), schema
  version 1, shared by the tool that writes it and the server that shows it.

Errors are this crate's own `yanuget_pkg::Error`; the server converts them into
its HTTP errors.
