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

The binary has no runtime dependencies beyond a C runtime and CA certificates
(the latter only if you enable mirroring or migration).

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
  The default trusts private ranges. If YANuget is directly reachable from the
  internet, set it to `[]`.

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
│   └── .uploads/        in-flight uploads, renamed into place when complete
└── tls/                 cert.pem and key.pem — only the self-signed pair
```

Package ids and versions are lower-cased and normalized, so the layout is
predictable and case-insensitively unique. Nothing else on the filesystem is
written to at runtime.

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

# Then the payloads. Package files are immutable once written, so copying them
# after the database snapshot can only ever include extra files, never miss one
# the snapshot references.
rsync -a /var/lib/yanuget/packages/ /backup/packages/
```

Restore by putting both back and starting the server; no import step is needed.
`tls/` does not need backing up — a self-signed pair is regenerated on first
start if it is missing.

## systemd

```ini
# /etc/systemd/system/yanuget.service
[Unit]
Description=YANuget package server
After=network-online.target
Wants=network-online.target

[Service]
Type=exec
ExecStart=/usr/local/bin/yanuget
Restart=on-failure
RestartSec=5s

User=yanuget
Group=yanuget
StateDirectory=yanuget
Environment=YANUGET_DATA_DIR=/var/lib/yanuget
Environment=YANUGET_BASE_URL=https://nuget.example.com
EnvironmentFile=/etc/yanuget/secrets.env

# The server needs its state directory and nothing else.
NoNewPrivileges=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectSystem=strict
ProtectHome=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictAddressFamilies=AF_INET AF_INET6
LockPersonality=yes

# Large uploads and downloads keep many file descriptors open.
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

Put `YANUGET_API_KEY=…` and `YANUGET_ADMIN_API_KEY=…` in
`/etc/yanuget/secrets.env`, owned by root and mode `0600`, so the keys are not
visible in `systemctl show` or the process environment of other users.

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
      YANUGET_TRUSTED_PROXIES: "private"
    env_file:
      - secrets.env          # YANUGET_API_KEY, YANUGET_ADMIN_API_KEY
    volumes:
      - yanuget-data:/data

volumes:
  yanuget-data:
```

The image runs as an unprivileged user (uid 10001) and declares a `HEALTHCHECK`
against `/health`, so `docker ps` reports whether the server can actually reach
its database rather than merely that the process is alive.

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
  mirroring into the feed your own packages live in.
