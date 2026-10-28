# Admin command line

`yanuget admin` acts on one version through a running server's admin area —
the same actions as the buttons in `/admin`, from a shell, a CI job or a
deployment script. `yanuget config check` checks a configuration before the
server is started with it.

## Acting on a version

```bash
yanuget admin --feed stage promote Contoso.Utils 1.4.0
yanuget admin --feed production approve Contoso.Utils 1.4.0
```

| Action | What it does |
| --- | --- |
| `promote ID VERSION` | Copy the version into the feed's `promotes_to` feed. |
| `approve ID VERSION` | Release a version that is pending in this feed. |
| `disable` / `enable ID VERSION` | Hide a version from clients, or show it again. |
| `pin` / `unpin ID VERSION` | Keep a version from retention, or let it go. |
| `delete ID VERSION --yes` | Remove the version from this feed. Cannot be undone. |

Each prints one line on success and exits `0`; on failure it prints the
server's reason (`this feed has no promotion target`, the target's license
policy, …) and exits `1`. A promotion into a feed with `requires_approval`
says that the version is pending there.

### Which server, which key

On the server's own machine, nothing else is needed: with no `--url`, the
command reads the same configuration the server does (`--config` or
`YANUGET_CONFIG`), connects to the configured port on this machine, and takes
the feed's admin key from it. It asks for the server by its `base_url` host,
so `allowed_hosts` still lets it in, and does not check the certificate,
since the connection never leaves the machine.

From anywhere else, name the server and supply the key:

```bash
export YANUGET_ADMIN_KEY="$(cat ~/.config/yanuget/admin.key)"
yanuget admin --url https://nuget.example.com --ca-cert /etc/ssl/internal-ca.pem \
    --feed stage promote Contoso.Utils 1.4.0
```

- `--url` is the address clients use, without the feed; `--feed` adds
  `/{feed}`. Without `--feed` the command acts on the root feed, which is right
  only for a server with no `[[feeds]]`.
- The key comes from `--admin-key-file`, else `YANUGET_ADMIN_KEY`, else the
  configuration. There is no flag for the key itself: a value on the command
  line is visible to every local user in `ps`.
- `--ca-cert` adds a PEM bundle of CA certificates to the system's, for a
  server with a certificate from an internal CA.

The command reads the CSRF token from the package's admin page and sends it
with the action, as [the HTTP API](api.md#csrf) requires.

## Checking a configuration

```bash
yanuget --config /etc/yanuget/yanuget.toml config check
```

loads the configuration exactly as the server would (the file, then the
`YANUGET_*` environment, [key files](configuration.md#keys-from-the-environment-or-a-file)
included) and then opens what the server would only open later:

- every value parses, feed names are valid and unique, and every
  `promotes_to` names a configured feed;
- the TLS certificate and key files parse, and the key belongs to the
  certificate;
- the certificate names the `base_url` host and each of `allowed_hosts`
  (a warning for each it does not — clients connecting by that name refuse it);
- each mirror's `ca_cert_path` holds at least one certificate;
- feeds without a push key, and keys shorter than 32 characters, are
  reported as warnings.
- when the database can be read, versions it holds for a feed the
  configuration does not have (one renamed or taken out) are reported as a
  warning, with the [`yanuget feeds`](cli.md#yanuget-feeds) command that deals
  with them. The database is only read; when there is none yet, or it cannot be
  opened, a note says so.

It prints `error:`, `warning:` and `note:` lines, then `configuration ok`, and
exits `1` if there was any error — so it can run as `ExecStartPre=` in a
systemd unit, or in the pipeline that deploys the file.
