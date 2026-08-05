#!/usr/bin/env bash
#
# End-to-end verification against the real .NET SDK.
#
# The Rust test suite drives YANuget with `reqwest`, which is a faithful HTTP
# client but not a NuGet client: it does not care whether the service index
# advertises the resources NuGet probes for, whether the flat container lists a
# version NuGet is about to restore, or whether an SSQP key matches what a
# debugger computes. Several defects lived happily behind a green test suite for
# exactly that reason.
#
# This script runs the real thing — `dotnet pack`, `dotnet nuget push`,
# `dotnet restore`, `dotnet run` — against a real server over TLS, and checks
# the properties that only a real client can confirm.
#
# Usage:   scripts/verify-with-dotnet.sh [work-dir]
# Requires: a release build (cargo build --release) and network access the first
#           time, to fetch the .NET SDK into the work directory.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="${1:-${TMPDIR:-/tmp}/yanuget-dotnet-verify}"
BIN="$REPO/target/release/yanuget"
PORT="${PORT:-5399}"
API_KEY="verify-key"

[ -x "$BIN" ] || { echo "build it first: cargo build --release" >&2; exit 1; }

mkdir -p "$WORK"
cd "$WORK"

# --- .NET SDK ---------------------------------------------------------------
# Prefer an SDK that is already present (CI runners ship one, and a previous run
# may have installed one here); only fetch as a last resort. `DOTNET_ROOT` is set
# only when we are pointing at our own copy — exporting it for a system SDK sends
# it looking for the shared runtime in the wrong place.
export DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1
LOCAL_DOTNET="$WORK/dotnet"
if [ -x "$LOCAL_DOTNET/dotnet" ]; then
    export DOTNET_ROOT="$LOCAL_DOTNET"
    export PATH="$LOCAL_DOTNET:$PATH"
elif ! command -v dotnet >/dev/null 2>&1; then
    echo "==> no .NET SDK found; installing one into $LOCAL_DOTNET"
    curl -sSL -o dotnet-install.sh https://dot.net/v1/dotnet-install.sh
    bash dotnet-install.sh --channel 8.0 --install-dir "$LOCAL_DOTNET" --no-path >/dev/null
    export DOTNET_ROOT="$LOCAL_DOTNET"
    export PATH="$LOCAL_DOTNET:$PATH"
fi
echo "==> dotnet $(dotnet --version) from $(command -v dotnet), NuGet $(dotnet nuget --version | tail -1)"

# --- server (TLS, self-signed, as shipped by default) -----------------------
rm -rf server && mkdir -p server
YANUGET_API_KEY="$API_KEY" YANUGET_DATA_DIR="$WORK/server" \
YANUGET_PORT="$PORT" YANUGET_HOST=127.0.0.1 \
YANUGET_BASE_URL="https://localhost:$PORT" \
    setsid "$BIN" > server/log 2>&1 < /dev/null &
SERVER_PID=$!
trap 'kill "$SERVER_PID" 2>/dev/null || true' EXIT

for _ in $(seq 1 80); do
    curl -sfk "https://127.0.0.1:$PORT/health" >/dev/null 2>&1 && break
    sleep 0.25
done
curl -sfk "https://127.0.0.1:$PORT/health" >/dev/null || { cat server/log; exit 1; }

# Trust the generated certificate for this run only.
cat /etc/ssl/certs/ca-certificates.crt server/tls/cert.pem > bundle.pem
export SSL_CERT_FILE="$WORK/bundle.pem"
FEED="https://localhost:$PORT/v3/index.json"
CA=(--cacert "$WORK/bundle.pem")
echo "==> server on $FEED (HTTP/$(curl -s "${CA[@]}" -o /dev/null -w '%{http_version}' "https://localhost:$PORT/health"))"

fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok: $*"; }

# --- pack two packages, one depending on the other --------------------------
rm -rf src out && mkdir -p src
dotnet new classlib -n Core.Lib -o src/core --force >/dev/null
cat > src/core/Core.Lib.csproj <<'XML'
<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup>
  <TargetFramework>net8.0</TargetFramework><PackageId>Core.Lib</PackageId><Version>1.0.0</Version>
  <Authors>verify</Authors><Description>Leaf package. Handles &amp; and &lt;angles&gt;.</Description>
  <PackageLicenseExpression>MIT</PackageLicenseExpression>
  <PackageRequireLicenseAcceptance>true</PackageRequireLicenseAcceptance>
  <IncludeSymbols>true</IncludeSymbols><SymbolPackageFormat>snupkg</SymbolPackageFormat>
  <DebugType>portable</DebugType>
</PropertyGroup></Project>
XML
dotnet new classlib -n Wrapper.Lib -o src/wrapper --force >/dev/null
cat > src/wrapper/Wrapper.Lib.csproj <<'XML'
<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup>
  <TargetFramework>net8.0</TargetFramework><PackageId>Wrapper.Lib</PackageId><Version>2.1.0</Version>
  <Authors>verify</Authors><Description>Depends on Core.Lib</Description>
  <PackageLicenseExpression>Apache-2.0</PackageLicenseExpression>
</PropertyGroup><ItemGroup>
  <PackageReference Include="Core.Lib" Version="1.0.0" />
</ItemGroup></Project>
XML
cat > nuget.config <<XML
<?xml version="1.0" encoding="utf-8"?>
<configuration><packageSources>
  <clear /><add key="yanuget" value="$FEED" />
</packageSources></configuration>
XML

echo "==> pack and push"
dotnet pack src/core -o out --nologo >/dev/null
dotnet nuget push out/Core.Lib.1.0.0.nupkg --source "$FEED" --api-key "$API_KEY" >/dev/null
ok "pushed Core.Lib (dotnet also pushes its .snupkg to /api/v2/symbol)"

dotnet pack src/wrapper -o out --nologo >/dev/null
ok "packed Wrapper.Lib — which required restoring Core.Lib from the server"
dotnet nuget push out/Wrapper.Lib.2.1.0.nupkg --source "$FEED" --api-key "$API_KEY" >/dev/null

# These pushes are expected to fail, so capture the output rather than pipe it:
# under `set -o pipefail` the pipeline would inherit dotnet's non-zero exit even
# when the grep matches.
dup_out="$(dotnet nuget push out/Core.Lib.1.0.0.nupkg --source "$FEED" --api-key "$API_KEY" 2>&1 || true)"
grep -q "409\|Conflict" <<<"$dup_out" || fail "a duplicate push should be a 409: $dup_out"
ok "duplicate push reported as a conflict"

bad_out="$(dotnet nuget push out/Core.Lib.1.0.0.nupkg --source "$FEED" --api-key wrong 2>&1 || true)"
grep -q "401\|Unauthorized" <<<"$bad_out" || fail "a bad key should be a 401: $bad_out"
ok "bad api key rejected"

# --- transitive restore, build and run --------------------------------------
echo "==> restore, build and run a consumer"
dotnet new console -n Consumer -o app --force >/dev/null
cat > app/Consumer.csproj <<'XML'
<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup>
  <OutputType>Exe</OutputType><TargetFramework>net8.0</TargetFramework>
  <ImplicitUsings>enable</ImplicitUsings>
</PropertyGroup><ItemGroup>
  <PackageReference Include="Wrapper.Lib" Version="2.1.0" />
</ItemGroup></Project>
XML
cat > app/Program.cs <<'CS'
Console.WriteLine("OK " + typeof(Wrapper.Lib.Class1).FullName + " " + typeof(Core.Lib.Class1).FullName);
CS
dotnet nuget locals http-cache --clear >/dev/null 2>&1
dotnet nuget locals global-packages --clear >/dev/null 2>&1
dotnet restore app --nologo --no-cache >/dev/null
dotnet run --project app --nologo 2>/dev/null | grep -q "^OK " \
    || fail "the consumer did not run against the restored packages"
ok "transitive restore over TLS, then build and run"

# --- unlisted versions must stay restorable ---------------------------------
echo "==> unlist semantics"
dotnet nuget delete Core.Lib 1.0.0 --source "$FEED" --api-key "$API_KEY" --non-interactive >/dev/null
search_out="$(curl -s "${CA[@]}" "https://localhost:$PORT/v3/search?q=Core.Lib")"
grep -q '"id":"Core.Lib"' <<<"$search_out" \
    && fail "an unlisted package must disappear from search"
ok "unlisted package hidden from search"

cat > app/Consumer.csproj <<'XML'
<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup>
  <OutputType>Exe</OutputType><TargetFramework>net8.0</TargetFramework>
  <ImplicitUsings>enable</ImplicitUsings>
</PropertyGroup><ItemGroup>
  <PackageReference Include="Core.Lib" Version="[1.0.0]" />
</ItemGroup></Project>
XML
cat > app/Program.cs <<'CS'
Console.WriteLine("OK " + typeof(Core.Lib.Class1).FullName);
CS
dotnet nuget locals http-cache --clear >/dev/null 2>&1
dotnet nuget locals global-packages --clear >/dev/null 2>&1
rm -rf app/obj app/bin
dotnet restore app --nologo --no-cache >/dev/null 2>&1 \
    || fail "a project pinned to an unlisted version must still restore (NU1101 regression)"
ok "project pinned to the unlisted version still restores"

# --- the symbol key must match what a debugger computes ---------------------
echo "==> symbol server"
python3 - "$WORK" <<'PY'
import struct, sys, zipfile, pathlib
work = pathlib.Path(sys.argv[1])

def codeview(dll: bytes):
    """The GUID a debugger derives from the assembly's CodeView debug entry."""
    pe = struct.unpack_from("<I", dll, 0x3C)[0]
    coff = pe + 4
    nsec, = struct.unpack_from("<H", dll, coff + 2)
    optsz, = struct.unpack_from("<H", dll, coff + 16)
    opt = coff + 20
    magic, = struct.unpack_from("<H", dll, opt)
    rva, size = struct.unpack_from("<II", dll, opt + (112 if magic == 0x20b else 96) + 6 * 8)
    sections = opt + optsz
    def off(r):
        for i in range(nsec):
            s = sections + i * 40
            vsz, va = struct.unpack_from("<II", dll, s + 8)
            rawsz, rawptr = struct.unpack_from("<II", dll, s + 16)
            if va <= r < va + max(vsz, rawsz):
                return rawptr + (r - va)
        raise SystemExit("rva not mapped")
    base = off(rva)
    for i in range(size // 28):
        e = base + i * 28
        if struct.unpack_from("<I", dll, e + 12)[0] == 2:
            ptr = struct.unpack_from("<II", dll, e + 20)[1]
            assert dll[ptr:ptr + 4] == b"RSDS"
            return dll[ptr + 4:ptr + 20]
    raise SystemExit("no codeview entry")

nupkg = zipfile.ZipFile(work / "out/Core.Lib.1.0.0.nupkg")
guid = codeview(nupkg.read(next(n for n in nupkg.namelist() if n.endswith(".dll"))))
order = [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15]
(work / "key.txt").write_text("".join(f"{guid[i]:02X}" for i in order) + "FFFFFFFF")

snup = zipfile.ZipFile(work / "out/Core.Lib.1.0.0.snupkg")
name = next(n for n in snup.namelist() if n.endswith(".pdb"))
(work / "expected.pdb").write_bytes(snup.read(name))
(work / "pdbname.txt").write_text(name.rsplit("/", 1)[-1].lower())
PY
KEY="$(cat key.txt)"; PDB="$(cat pdbname.txt)"
curl -sf "${CA[@]}" -o served.pdb "https://localhost:$PORT/download/symbols/$PDB/$KEY/$PDB" \
    || fail "the debugger's SSQP path did not resolve"
cmp -s served.pdb expected.pdb || fail "the served PDB differs from the one in the .snupkg"
ok "SSQP key matches the assembly's CodeView GUID, and the PDB is byte-identical"

bogus="$(curl -s "${CA[@]}" -o /dev/null -w '%{http_code}' \
    "https://localhost:$PORT/download/symbols/$PDB/00000000000000000000000000000000FFFFFFFF/$PDB")"
[ "$bogus" = "404" ] || fail "an unknown symbol key should 404, got $bogus"
ok "unknown symbol key rejected"

echo
echo "All checks passed against dotnet $(dotnet --version)."
