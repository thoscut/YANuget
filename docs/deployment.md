# Deployment

This page covers running YANuget as a service: how to install it, what lives in
the data directory, how to back it up, and how to upgrade. For individual
settings see [Configuration](configuration.md); for hosting very large packages
see [Large packages](large-packages.md).

## Choosing a build

| | Ships offline `/docs` | Best for |
| --- | --- | --- |
| Container image (`ghcr.io/thoscut/yanuget`) | ✅ | Most deployments |
| Release archive from GitHub | ✅ | Bare metal, systemd |
| `cargo install yanuget` | ❌ (placeholder page) | Trying it out, custom builds |
| `cargo build --release` | Only if you ran `mkdocs build` first | Development |

The binary has no runtime dependencies beyond a C runtime. For mirroring and
migration it trusts the Mozilla root certificates built into it *and* the
system's CA store when there is one, so an upstream behind an internal CA or a
TLS-inspecting proxy works once that CA is in the system store — or named in
the feed's `mirror.ca_cert_path` (`--source-ca-cert` for `migrate`).

## Minimum viable production setup

```bash
YANUGET_API_KEY=<a long random string> \
YANUGET_DATA_DIR=/var/lib/yanuget \
YANUGET_BASE_URL=https://nuget.example.com \
yanuget
```

Three settings do most of the work:

- **`api_key`** — without it, anybody who can reach the port can push and delete.
  The server logs a loud warning at startup when it is unset.
- **`base_url`** — the absolute URLs a restoring client is handed are built from
  this. Set it explicitly when the server is behind a proxy; the alternative is
  deriving it per-request from headers, which is correct only when
  `trusted_proxies` is also right.
- **`trusted_proxies`** — see [Trusted proxies](configuration.md#trusted-proxies).
  The default is empty: nobody is trusted and forwarding headers are ignored,
  which is right when clients connect to YANuget directly. Behind a reverse
  proxy, list that proxy's address — and only that.

Also set **`max_package_size_bytes`** on any feed open to more than a handful of
people, and keep **`admin_api_key`** distinct from the push key.

## The data directory

Everything mutable lives under one directory (`data_dir`, default `./data`):

```
data/
├── yanuget.db          SQLite index (plus -wal and -shm while running)
├── packages/
│   ├── <id>/<version>/<id>.<version>.nupkg
│   ├── <id>/<version>/<id>.<version>.snupkg
│   ├── <id>/<version>/<sidecars: nuspec, readme, icon>
│   ├── .symbols/<ssqp-key>/<file.pdb>
│   ├── .blobs/sha256/<ab>/<sha256>   attached files, stored once by content hash
│   ├── .uploads/        in-flight uploads (and resumable ones), renamed into
│   │                    place when complete
│   └── .migrate/        scratch space of a running `yanuget migrate`
└── tls/                 cert.pem and key.pem — only the self-signed pair
```

Package ids and versions are lower-cased and normalized, so the layout is
predictable and case-insensitively unique. The one other directory written to
at runtime is the SSH inbox, when `files.inbox_dir` is set: it lives wherever
you point it (never inside the package store), and files leave it as they are
imported.

Size it for the packages you expect plus room for one in-flight upload per
concurrent push. An upload is streamed into `packages/.uploads/` and then
`rename`d into its final directory — deliberately on the same filesystem, which
is what makes that move atomic and free — so a 25 GB push briefly needs 25 GB of
headroom beyond the stored copy. Do not mount `.uploads` elsewhere; a
cross-filesystem rename would turn every push into a full second copy.

## Backup and restore

The database and the package store must be backed up **together and
consistently** — a database referencing a payload that is not in the backup is
worse than either one missing.

The database is in WAL mode, so copying `yanuget.db` while the server runs can
capture a torn state. Either stop the server, or use SQLite's online backup:

```bash
# Consistent database snapshot without stopping the server.
sqlite3 /var/lib/yanuget/yanuget.db ".backup '/backup/yanuget.db'"

# Then the payloads, and the certificate pair.
rsync -a /var/lib/yanuget/packages/ /backup/packages/
rsync -a /var/lib/yanuget/tls/ /backup/tls/
```

Taking the database first and the files second is the right order, but it is
not airtight while the server runs. New pushes only add files, so they can only
leave extra files in the backup. Anything that *removes* a file between the two
steps — a delete, an overwrite of an existing version (`allow_overwrite`), a
retention run, a file detached from a version — takes away a payload the
snapshot still references, and the backup has a database row whose file is
missing. For a backup that is consistent by construction, either stop the
server for the duration, or copy from a filesystem snapshot (LVM, ZFS, btrfs)
of the whole data directory taken at one instant — with the database inside it
copied by `.backup` from that snapshot, or the server stopped when it is taken.
`.uploads/` and `.migrate/` hold only work in progress and can be left out
(a resumable upload that was unfinished at backup time then starts over).

Restore by putting everything back and starting the server; no import step is
needed.

Back up `tls/` too if the server uses its self-signed pair. A missing pair is
regenerated on start, but as a *new* certificate: every client that was told to
trust the old one stops connecting until it is told again. (With
`tls_cert_path`/`tls_key_path` the certificate lives wherever those point —
back that up instead.)

## systemd

The Linux release archives ship a hardened unit, `yanuget.service`, and the
system user it runs as, `yanuget.sysusers` (both also in the repository under
`contrib/systemd/`). From the unpacked archive:

```bash
install -m 0755 yanuget /usr/local/bin/yanuget
install -m 0644 yanuget.sysusers /usr/lib/sysusers.d/yanuget.conf
systemd-sysusers                       # creates the yanuget user and group

install -d -m 0750 -g yanuget /etc/yanuget
install -m 0640 -g yanuget yanuget.example.toml /etc/yanuget/yanuget.toml
install -m 0644 yanuget.service /etc/systemd/system/yanuget.service

systemctl daemon-reload
systemctl enable --now yanuget
```

What the unit sets up:

- **Unprivileged user.** The server runs as `yanuget`, never as root.
- **Data in `/var/lib/yanuget`.** systemd creates it (`StateDirectory=`) owned
  by the service user, and the unit sets `YANUGET_DATA_DIR` to it, so the
  `data_dir` in the TOML file is ignored.
- **Configuration in `/etc/yanuget/yanuget.toml`**, read-only to the service.
  Keys can stay out of that file: put `YANUGET_API_KEY=…` and
  `YANUGET_ADMIN_API_KEY=…` in `/etc/yanuget/secrets.env`, owned by root and
  mode `0600`. systemd reads it before dropping privileges, and the keys are not
  visible in `systemctl show`.
- **Read-only system.** `ProtectSystem=strict` mounts the whole file system
  read-only for the service, with `ReadWritePaths=/var/lib/yanuget` the one
  exception. Home directories, devices, kernel settings and most system calls
  are out of reach, and `/tmp` is private to the service; `systemd-analyze
  security yanuget` rates the unit "OK".
- **Port 443 without root**, see below.

Change it with `systemctl edit yanuget` (a drop-in under
`/etc/systemd/system/yanuget.service.d/`) rather than editing the file, so an
upgrade can replace the unit. The usual changes:

```ini
[Service]
# Data on another volume: both lines together.
Environment=YANUGET_DATA_DIR=/srv/yanuget
ReadWritePaths=/srv/yanuget
# files.inbox_dir must be writable too, since imported files leave it.
ReadWritePaths=/srv/yanuget-inbox
```

A directory outside `/var/lib/yanuget` is not created for you: create it owned
by `yanuget` first. The binary itself is upgraded by replacing
`/usr/local/bin/yanuget` and restarting; see [Upgrading](#upgrading).

`yanuget healthcheck` works as a check under systemd too, run as the service
user so it reads the same configuration:

```bash
sudo -u yanuget yanuget --config /etc/yanuget/yanuget.toml healthcheck
```

### Port 443 as a non-root user

Ports below 1024 normally need root. The unit instead grants the process the
single capability for binding them:

```ini
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
```

so serving clients on the standard HTTPS port is only a configuration change:

```toml
# /etc/yanuget/yanuget.toml
port = 443
base_url = "https://nuget.example.com"
tls_cert_path = "/etc/yanuget/tls/fullchain.pem"
tls_key_path = "/etc/yanuget/tls/key.pem"
```

The capability is all the process can ever hold: `CapabilityBoundingSet`
removes every other one, and `NoNewPrivileges` stops it gaining more. When
YANuget listens on its default port 5000, or behind a reverse proxy, delete both
lines in a drop-in (`AmbientCapabilities=` and `CapabilityBoundingSet=` set
empty).

Outside systemd, the same capability can be put on the binary instead:
`setcap cap_net_bind_service=+ep /usr/local/bin/yanuget`. It is lost whenever
the file is replaced, so it has to be repeated on every upgrade, which is why
the unit is the better place for it.

The certificate and key must be readable by the service user: owned by root,
group `yanuget`, mode `0640`. Clients check the name strictly, so the
certificate's subject alternative names must cover every name they use to reach
the server: the fully qualified name, a short host name if clients use one, and
the IP address if any client connects by address.

## Docker Compose

```yaml
services:
  yanuget:
    image: ghcr.io/thoscut/yanuget:latest
    restart: unless-stopped
    ports:
      - "5000:5000"
    environment:
      YANUGET_BASE_URL: https://nuget.example.com
    env_file:
      - secrets.env          # YANUGET_API_KEY, YANUGET_ADMIN_API_KEY
    volumes:
      - yanuget-data:/data

volumes:
  yanuget-data:
```

Clients reach the container directly here, so `trusted_proxies` stays at its
empty default. Setting it to `private` in this setup would let any machine on
the network send its own `X-Forwarded-For` and `X-Forwarded-Host`. When a
reverse proxy runs in front — another service in the same Compose file, say —
set `YANUGET_TRUSTED_PROXIES` to that proxy's address, or to the Compose
network's subnet if nothing else can reach the container.

The image runs as an unprivileged user (uid 10001) and declares a `HEALTHCHECK`
(`yanuget healthcheck`, which probes `/health/ready`), so `docker ps` reports
whether the server can actually reach its database rather than merely that the
process is alive. The probe reads the same configuration as the server, so
point a mounted TOML file at it with `YANUGET_CONFIG=/path/in/container`
rather than with `--config`: the health check does not see the container's
arguments.

To mount a host directory instead of a named volume, make sure it is writable by
uid 10001.

## Kubernetes probes

Two endpoints, and they are not interchangeable:

```yaml
livenessProbe:
  httpGet: { path: /health/live, port: 5000, scheme: HTTPS }
  periodSeconds: 10
readinessProbe:
  httpGet: { path: /health, port: 5000, scheme: HTTPS }
  periodSeconds: 5
```

`/health/live` answers as long as the process is serving. `/health` also probes
the database and returns `503` when it cannot be reached — use it for readiness
so a pod whose storage has gone away is taken out of rotation instead of being
restarted in a loop.

Use `scheme: HTTP` instead if you set `tls_enabled = false` to terminate TLS at
an ingress.

## Behind a reverse proxy

See [Reverse proxy](configuration.md#reverse-proxy) for the nginx configuration.
Two settings there are not optional for a package server: disable request and
response buffering, and remove the body size limit. A proxy that buffers a 25 GB
upload in memory or on its own disk undoes the whole point of streaming, and a
default `client_max_body_size` will reject the push outright.

## Upgrading

1. Read the [changelog](https://github.com/thoscut/yanuget/blob/main/CHANGELOG.md)
   for the versions you are skipping.
2. Back up as above. This is the step people skip.
3. Replace the binary or pull the new image and restart.

Schema changes are applied automatically at startup and are additive, so an
upgrade needs no migration command and re-running it is a no-op. **Downgrading
is not supported**: an older binary may not understand columns a newer one
added. Roll back by restoring the backup.

Package payloads are never rewritten by an upgrade, so a restore only ever needs
to roll back the database.

## Operational notes

- **Logging** — the default filter is `info,yanuget=info,tower_http=info`; set
  `RUST_LOG=yanuget=debug` for request-level detail. `5xx` responses are
  deliberately generic, and the underlying I/O, SQL or upstream error appears
  only in the log.
- **Rate limiting** is per-IP and on by default. Behind a proxy it is only
  correct if `trusted_proxies` covers that proxy — otherwise every request looks
  like it comes from the proxy's address.
- **Retention** is off unless configured. Enabling it hard-deletes payloads; try
  it against a copy first.
- **Mirroring** makes your server fetch from an upstream and republish under
  your own name. Review the upstream, and prefer a feed dedicated to it over
  mirroring into the feed your own packages live in. To pull a mirrored version
  (one found to be malicious, say), delete or disable it in `/admin`: a
  deleted version is recorded, and the mirror never fetches it back (pushing
  it again undoes that). See [Feeds](configuration.md#feeds).
