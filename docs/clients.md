# Connecting clients

YANuget speaks the NuGet v3 protocol, so every client that can use nuget.org can
use it: point the client at the feed's **service index** and the client
discovers everything else from it. This page shows the setup for the common
clients; the [HTTP API](api.md) describes what happens underneath.

## The feed URL

| Setup | Service index |
| --- | --- |
| Single feed (no `[[feeds]]` configured) | `https://host:5000/v3/index.json` |
| Several feeds | `https://host:5000/{feed}/v3/index.json`, e.g. `/stable/v3/index.json` |

The server prints the URL when it starts, and with several feeds the page at
`/` lists them. Use the public address the clients reach — the one in
`base_url` when it is set — rather than the address the server binds to.

Two keys matter to a client:

* the **push key** (`api_key`, or a feed's own `api_key`/`api_keys`) to push and
  delete, sent by every client as `X-NuGet-ApiKey`;
* the **read key** (a feed's `read_api_key`), only if the feed sets one, to
  restore, search and browse. Clients send it as the password of HTTP Basic
  credentials; the user name is ignored. See [Read-gated feeds](#read-gated-feeds).

## Trusting the default certificate

Out of the box YANuget serves HTTPS with a self-signed certificate it generates
on first start and keeps under `{data_dir}/tls/cert.pem`. NuGet clients refuse a
certificate they do not trust, so either give the server a real certificate
([TLS](configuration.md#tls)), put it behind a reverse proxy that has one, or
make the clients trust `cert.pem`:

On **Windows**, as administrator, trust it machine-wide:

```powershell
certutil -addstore -f Root cert.pem
```

On **Debian or Ubuntu**:

```bash
sudo cp cert.pem /usr/local/share/ca-certificates/yanuget.crt
sudo update-ca-certificates
```

For **one shell only** (.NET on Linux and macOS reads the OpenSSL bundle this
names; the repository's end-to-end test does the same):

```bash
cat /etc/ssl/certs/ca-certificates.crt cert.pem > bundle.pem
export SSL_CERT_FILE="$PWD/bundle.pem"
```

The generated certificate is valid for `localhost` and for the host name in
`base_url` *at the time it was generated*. If you set or change `base_url`
later, delete `{data_dir}/tls/` and restart so a certificate with the new name
is generated, then trust that one.

### Names on the certificate

NuGet clients check that the name they connect to is on the certificate, and
they check it strictly. Every name a client uses must be listed as a subject
alternative name (SAN): the fully qualified name (`nuget.example.com`), a short
name some machines use (`nuget`), and the IP address if anything connects by
address. A name that is missing fails the TLS handshake even when the
certificate is otherwise trusted.

The generated certificate only carries `localhost` and the `base_url` host.
When clients reach the server under other names, give it a certificate of your
own (`tls_cert_path`, `tls_key_path`) that lists them all, for example from your
internal CA, and check it before rolling it out:

```bash
openssl x509 -in cert.pem -noout -ext subjectAltName
# X509v3 Subject Alternative Name:
#     DNS:nuget.example.com, DNS:nuget, IP Address:10.0.0.15
```

Better still, give every client the same name, the one in `base_url`: the
URLs in the service index are built from it, so clients end up there anyway.

## dotnet CLI

```bash
# Register the feed once per machine (or commit a nuget.config, below).
dotnet nuget add source https://host:5000/v3/index.json -n yanuget

# Push a package. dotnet also pushes the matching .snupkg next to it.
dotnet nuget push MyLib.1.0.0.nupkg --source yanuget --api-key "$YANUGET_API_KEY"

# Unlist a version (or delete it, with hard_delete_enabled).
dotnet nuget delete MyLib 1.0.0 --source yanuget --api-key "$YANUGET_API_KEY" --non-interactive
```

A `nuget.config` next to the solution makes restore use the feed for everyone
who builds it:

```xml
<?xml version="1.0" encoding="utf-8"?>
<configuration>
  <packageSources>
    <clear />
    <add key="yanuget" value="https://host:5000/v3/index.json" />
  </packageSources>
</configuration>
```

`<clear />` makes YANuget the only source. Leave it out to keep nuget.org as
well, or, better, configure YANuget as a [mirror](feeds.md#mirroring-an-upstream-feed)
of nuget.org and keep `<clear />`, so every package a build uses comes through
one server.

## nuget.exe

```powershell
nuget sources add -Name yanuget -Source https://host:5000/v3/index.json
nuget push MyLib.1.0.0.nupkg -Source yanuget -ApiKey $env:YANUGET_API_KEY
nuget install MyLib -Version 1.0.0 -Source yanuget
```

## Chocolatey

YANuget is built first of all as a private Chocolatey feed. Chocolatey CLI v2
and later use the v3 protocol:

```powershell
choco source add -n=yanuget -s="https://host:5000/v3/index.json"
choco push my-package.1.0.0.nupkg -s="https://host:5000/v3/index.json" -k="$env:YANUGET_API_KEY"
choco install my-package --source=yanuget
```

For a read-gated feed, add `-u=any -p="<read key>"` to `choco source add`.

The gallery's package page shows the install command for each version, with
Chocolatey first by default (`primary_client`). A package that needs a large
installer image can carry it as an [attached file](api.md#attached-files); the
package page then shows the `chocolateyInstall.ps1` lines that download and
verify it.

To replace a broken build without overwriting, push it under Chocolatey's
package fix version notation, e.g. `1.2.3.20261015`.

## Visual Studio

Under **Tools → Options → NuGet Package Manager → Package Sources**, add a
source with the service index URL. For a read-gated feed, Visual Studio asks
for credentials on first use: any user name, the read key as the password.

## Read-gated feeds

A feed with `read_api_key` answers `401` to restore, search and the gallery
without a credential; only the service index stays open. Store the credential
with the source:

```bash
dotnet nuget add source https://host:5000/internal/v3/index.json -n internal \
  --username any --password "$READ_KEY" --store-password-in-clear-text
```

or in `nuget.config`, with the password taken from an environment variable so
it never lands in the repository:

```xml
<packageSourceCredentials>
  <internal>
    <add key="Username" value="any" />
    <add key="ClearTextPassword" value="%YANUGET_READ_KEY%" />
  </internal>
</packageSourceCredentials>
```

The push key is separate: a feed's read key does not let anyone push, and the
push key does not let anyone read a gated feed.

## Debugging with symbols

When the symbol server is on (the default), pushing a package with
`dotnet nuget push` also uploads its `.snupkg`. Build packages with portable
symbols to produce one:

```xml
<PropertyGroup>
  <IncludeSymbols>true</IncludeSymbols>
  <SymbolPackageFormat>snupkg</SymbolPackageFormat>
</PropertyGroup>
```

Then add the server as a symbol location in the debugger:

* **Visual Studio**: **Tools → Options → Debugging → Symbols**, add
  `https://host:5000/download/symbols` (with several feeds,
  `https://host:5000/{feed}/download/symbols`), and turn off **Just My Code**
  to step into package code.
* **VS Code / C# Dev Kit**: add the same URL to `symbolOptions.searchPaths` in
  the launch configuration.

The package must be pushed before its symbols, and the PDBs must belong to the
assemblies in that package; see [Symbol server](api.md#symbol-server).

## Browsers and scripts

Everything a client does is plain HTTP, so `curl` and `Invoke-WebRequest` work
too. Downloads support ranges, so an interrupted transfer of a large package can
continue:

```bash
curl -fC - -O https://host:5000/v3/package/mylib/1.0.0/mylib.1.0.0.nupkg
```

Ids and versions in these URLs are lower-case. For large files attached to a
package, `scripts/Send-YanugetFile.ps1` uploads resumably from PowerShell; see
[Attached files](api.md#attached-files).
