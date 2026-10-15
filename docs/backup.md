# Backup, restore and moving hosts

`yanuget backup` writes a consistent copy of everything the server keeps (the
database, the package store and the self-signed TLS pair) into a directory,
**while the server keeps running**. `yanuget restore` puts it back. Moving a
server to a new host is a backup on the old one and a restore on the new one.

## Taking a backup

```bash
# As the user the service runs as, with the service's configuration.
sudo -u yanuget yanuget --config /etc/yanuget/yanuget.toml backup /srv/backup/yanuget
```

The directory is created if needed, and must not be inside the data directory
(a backup on the same disk goes with it). It ends up holding:

```
/srv/backup/yanuget/
├── backup.json     manifest: when, what, how much; written last
├── yanuget.db      database snapshot, a single self-contained file
├── packages/       the package store, file for file
└── tls/            the self-signed certificate pair, if the server uses one
```

**Re-running into the same directory is incremental.** Stored files are never
rewritten in place, so a file that is already in the backup with the same size
and modification time is skipped; only new and replaced files are copied, and
files the server no longer has are removed. The first backup of a 50 GB store
copies 50 GB; the next one copies what was pushed since.

### Why it is consistent while the server runs

The database and the files must match: a database row whose `.nupkg` is not in
the backup is worse than either one missing. Copying `yanuget.db` with `cp` or
`rsync` while the server runs can capture a torn state (the database is in WAL
mode, and the latest commits live in `yanuget.db-wal`), and copying the files
at a different moment from the database can miss a package either way.

`yanuget backup` works in this order:

1. **Snapshot the database** with SQLite's `VACUUM INTO`, which reads one
   committed state and does not block the server's writes.
2. **Copy the package store.** Each file goes to a temp name, is synced, and is
   renamed into place, so the backup never holds a partial file. In-flight
   uploads (`.uploads/`) and a running migration's downloads (`.migrate/`) are
   skipped.
3. **Check every file the snapshot references** is in the backup with the size
   the database recorded. A push between steps 1 and 2 only leaves an extra
   file, which is harmless. A delete, an overwrite or a retention run in that
   window can take away a file the snapshot still lists, and then the pass
   runs again: a fresh snapshot and an incremental copy, which takes seconds.
4. **Put the snapshot in place**, remove files the server no longer has, copy
   the TLS pair, and write `backup.json`.

A file still missing after three passes is missing from the live store too.
The backup completes, lists it under `missing` in `backup.json`, and prints a
warning: it is a faithful copy of a server that has a problem, not a backup
that hides one.

Resumable uploads that were unfinished at backup time are not included; a
client resuming one after a restore starts it over.

### What it does not include

- The configuration file (`yanuget.toml`). Back it up with the rest of `/etc`;
  it holds the API keys.
- A certificate and key configured with `tls_cert_path` / `tls_key_path`. Only
  the self-signed pair under `data_dir/tls` is included; the command reminds
  you when the server uses its own pair.
- The SSH inbox (`files.inbox_dir`): files only pass through it.

### An interrupted backup

`backup.json` is removed when a run starts and written when it ends, so a
directory without it is not a complete backup, and `yanuget restore` refuses
it. Run `yanuget backup` again: it picks up where the last run stopped. The
previous snapshot stays in place until the new one is complete, so even an
interrupted run leaves a database whose files are all there (plus some
newer ones).

Two runs into the same directory at once are refused; the second one exits
with an error.

### Scheduling and keeping several

A nightly systemd timer, running as the service user:

```ini
# /etc/systemd/system/yanuget-backup.service
[Unit]
Description=YANuget backup

[Service]
Type=oneshot
User=yanuget
Group=yanuget
ExecStart=/usr/local/bin/yanuget --config /etc/yanuget/yanuget.toml backup /srv/backup/yanuget
```

```ini
# /etc/systemd/system/yanuget-backup.timer
[Unit]
Description=Nightly YANuget backup

[Timer]
OnCalendar=*-*-* 02:30
Persistent=true

[Install]
WantedBy=timers.target
```

```bash
systemctl daemon-reload
systemctl enable --now yanuget-backup.timer
```

The command exits non-zero when the backup fails, so the unit's failure shows
up in `systemctl --failed` and whatever watches it.

One directory holds one backup. To keep several, take a hard-link copy of it
after each run: unchanged files then cost no space, and the next run cannot
change the copy, because it replaces files by renaming new ones over them
rather than writing into them.

```bash
cp -al /srv/backup/yanuget /srv/backup/yanuget.$(date +%F)
```

A filesystem snapshot (ZFS, btrfs, LVM) of the backup directory works the same
way. To get the backup off the machine, `rsync -a --delete` the directory
somewhere else once the command has finished.

## Restoring

The server must be stopped: the database file is replaced underneath it, and
`yanuget restore` refuses to run while a server answers on the configured
port.

```bash
systemctl stop yanuget
sudo -u yanuget yanuget --config /etc/yanuget/yanuget.toml restore /srv/backup/yanuget
systemctl start yanuget
```

It restores into the locations the configuration names (`data_dir`, or
`storage_path` and `database_path` if set), so a backup taken from one layout
restores into another. The package store is copied first and the database last,
and any `-wal`/`-shm` files left by the previous database are removed so they
cannot be replayed into the restored one. The database is then opened the way
the server opens it, which brings a backup taken by an older version up to the
current schema.

By default the target must be empty. To replace existing data, pass `--force`:
the database is replaced, and the package store is made to match the backup
exactly, which **deletes files the backup does not have**.

Run it as the service user, so the restored files belong to the server. If you
restore as root, `chown -R yanuget:yanuget` the data directory afterwards.

## Moving to a new host

With `yanuget backup` the move is short and safe at any size. The old server
keeps serving until the last minute:

```bash
# 1. Old host, server running: take a full backup. This is the long part.
yanuget --config /etc/yanuget/yanuget.toml backup /srv/backup/move

# 2. Copy it to the new host. Repeat 1 and 2 as often as you like; each
#    round only moves what changed.
rsync -a --delete /srv/backup/move/ newhost:/srv/backup/move/

# 3. Old host: stop the server, take a final backup (seconds, since only the
#    last changes are copied) and copy it over.
systemctl stop yanuget
yanuget --config /etc/yanuget/yanuget.toml backup /srv/backup/move
rsync -a --delete /srv/backup/move/ newhost:/srv/backup/move/

# 4. New host: install the binary and configuration, restore and start.
yanuget --config /etc/yanuget/yanuget.toml restore /srv/backup/move
systemctl start yanuget
```

Stopping the server for step 3 is what makes the move lossless: no push can
land on the old host after the final backup. Then point DNS (or the clients)
at the new host. Remember what the backup does not include: copy
`yanuget.toml`, and the certificate and key if they are configured, and make
sure the new host's certificate covers the names and addresses clients use
(see [TLS](configuration.md#tls)). **Mirror feeds on other hosts** that
read through this server need their `mirror.upstream` (and `mirror.ca_cert_path`, for an internal
CA) updated if the URL or the
certificate changes.

### Without `yanuget backup`

Older versions have no backup command. Copying the data directory with
`rsync` works, but only for a final copy taken with the server stopped:

```bash
# While running: moves the bulk of the packages, not consistent on its own.
rsync -a --delete /var/lib/yanuget/ newhost:/var/lib/yanuget/

# Stopped: the final pass, which also brings over yanuget.db-wal.
systemctl stop yanuget
rsync -a --delete /var/lib/yanuget/ newhost:/var/lib/yanuget/
```

Copy `yanuget.db` and its `-wal` and `-shm` files together, from a stopped
server. A database copied from a running server can be torn, and a database
without its `-wal` file is missing its latest commits.

## Moving from another NuGet server

To bring packages over from BaGet, BaGetter or any other NuGet V3 server, use
[`yanuget migrate`](migrate.md): it copies every package over HTTP into a
running YANuget. Once that is done, back the new server up as above.
