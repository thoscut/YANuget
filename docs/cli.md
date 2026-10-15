# Command line

YANuget is a single binary, `yanuget`. Without a sub-command it runs the
server; two sub-commands cover a one-off import and container health checks.

```text
yanuget [--config <path>] [<command>]

Commands:
  migrate      Copy every package from another NuGet server into a feed
  healthcheck  Exit 0 if the local server is ready, 1 if not
  help         Print help for the server or a command

Options:
  -c, --config <path>  TOML configuration file [env: YANUGET_CONFIG]
  -h, --help           Print help
  -V, --version        Print the version
```

`--config` applies to every command, so `migrate` and `healthcheck` read the
same file, and the same `YANUGET_*` overrides, as the server.

## Running the server

```bash
yanuget --config /etc/yanuget/yanuget.toml
```

Settings come from the built-in defaults, then the file, then `YANUGET_*`
environment variables; see [Configuration](configuration.md). A value that does
not parse, or a contradictory combination, stops startup with an error that
names the setting.

Once the port is bound, the server prints where it can be reached:

```text
  YANuget 0.6.0

    Gallery        https://localhost:5000/
    Service index  https://localhost:5000/v3/index.json
    Documentation  https://localhost:5000/docs/
    Feeds          default
    Data           ./data
```

followed by a warning if any feed accepts pushes without an API key. Logging
goes to standard output and is controlled with `RUST_LOG`
([Logging](configuration.md#logging)).

On first start against an existing database, the schema is migrated to the
current version before the port opens. Back up the data directory before
upgrading; see [Upgrading](deployment.md#upgrading).

### Stopping

`Ctrl-C` or `SIGTERM` stops the server gracefully: it stops accepting
connections, lets requests in progress and background work finish for up to ten
seconds, then exits. A transfer still running after that is cut off; an upload
cut off this way is discarded, never stored half-written.

## `yanuget migrate`

Copies every package from another NuGet v3 server into a local feed, with live
progress. It opens the same data directory and database as the server; run it
against a stopped server, or one nobody is pushing to at the time.

```bash
yanuget migrate --source https://old-server/v3/index.json --feed default
```

All options, authentication and resumption are described in
[Bulk migration](migrate.md).

## `yanuget healthcheck`

Requests the readiness endpoint (`/health`) of the server on this machine, using
the port and scheme from the configuration, and exits `0` if it answers `200`
and `1` otherwise. It does not verify the certificate (it talks to its own
process over loopback) and ignores proxy settings. The container image uses it
as its `HEALTHCHECK`; elsewhere it fits any supervisor that runs a command:

```bash
yanuget --config /etc/yanuget/yanuget.toml healthcheck || echo "not ready"
```

For Kubernetes, probe the HTTP endpoints directly; see
[Kubernetes probes](deployment.md#kubernetes-probes).
