#!/usr/bin/env python3
"""Build a small, realistic set of packages and push them into a running server.

The gallery screenshots are only worth looking at if the feed has something in
it, and an empty feed deliberately renders the onboarding panel instead. This
builds a handful of plausible private-feed packages — a couple with several
versions, one prerelease, readmes, icons and dependencies — and pushes them
through the real HTTP API, so what ends up in the screenshots came through the
same path a client uses.

Usage: seed.py <base-url> <api-key>
"""

import io
import sys
import urllib.request
import zipfile

from PIL import Image, ImageDraw

# (id, [versions], description, authors, tags, deps, icon colour)
PACKAGES = [
    (
        "Contoso.Build.Tools",
        ["4.2.1", "4.1.0", "4.3.0-beta.2"],
        "MSBuild targets and analyzers shared across Contoso services. "
        "Enforces the house style, wires up deterministic builds and emits "
        "SBOM metadata for every artifact.",
        ["Contoso Platform Team"],
        ["msbuild", "build", "analyzers", "internal"],
        [("Microsoft.Build.Utilities.Core", "17.8.3"), ("Acme.Logging", "2.7.0")],
        (88, 101, 242),
    ),
    (
        "Acme.Logging",
        ["2.8.0", "2.7.4"],
        "Structured logging with correlation-id propagation, redaction of "
        "known secret shapes and a sink that survives the collector being "
        "briefly unreachable.",
        ["Acme Corp"],
        ["logging", "observability", "serilog"],
        [("Serilog", "3.1.1")],
        (46, 160, 100),
    ),
    (
        "Internal.Deploy.Cli",
        ["1.14.0"],
        "Command-line deployment tool for internal environments. Wraps the "
        "rollout API, waits for health, and rolls back on a failed canary.",
        ["Release Engineering"],
        ["cli", "deployment", "tooling"],
        [],
        (210, 153, 34),
    ),
    (
        "Fabrikam.Data.Sqlite",
        ["3.0.2"],
        "SQLite persistence helpers: WAL-aware connection pooling, migration "
        "runner and a busy-timeout policy that does not lose writes under "
        "concurrent readers.",
        ["Fabrikam"],
        ["sqlite", "database", "migrations"],
        [("Microsoft.Data.Sqlite", "8.0.4")],
        (88, 166, 255),
    ),
    (
        "Northwind.Analyzers",
        ["0.9.1"],
        "Roslyn analyzers that catch the mistakes our code review kept "
        "catching: unawaited tasks, swallowed cancellation and logging that "
        "interpolates instead of templating.",
        ["Northwind Traders"],
        ["roslyn", "analyzers", "codequality"],
        [],
        (182, 35, 36),
    ),
]

NUSPEC = """<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd">
  <metadata>
    <id>{id}</id>
    <version>{version}</version>
    <authors>{authors}</authors>
    <owners>{authors}</owners>
    <description>{description}</description>
    <projectUrl>https://example.com/{lid}</projectUrl>
    <repository type="git" url="https://example.com/{lid}.git" />
    <license type="expression">MIT</license>
    <icon>icon.png</icon>
    <readme>README.md</readme>
    <tags>{tags}</tags>
    <requireLicenseAcceptance>false</requireLicenseAcceptance>
    <dependencies>
      <group targetFramework="net8.0">
{deps}
      </group>
    </dependencies>
  </metadata>
</package>
"""

README = """# {id}

{description}

## Install

```
dotnet add package {id} --version {version}
```

## What changed in {version}

- Correlation ids now survive an `await` across a `TaskScheduler` boundary.
- The migration runner reports the statement it failed on, not just the file.
- Dropped the last dependency that pulled in a native library on Linux.
"""


def icon(colour: tuple) -> bytes:
    """A flat rounded-square icon with the package's initial."""
    img = Image.new("RGBA", (128, 128), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    d.rounded_rectangle([0, 0, 127, 127], radius=26, fill=colour + (255,))
    return _png(img)


def _png(img: Image.Image) -> bytes:
    buf = io.BytesIO()
    img.save(buf, format="PNG")
    return buf.getvalue()


def build(pkg_id, version, description, authors, tags, deps, colour) -> bytes:
    dep_xml = "\n".join(
        f'        <dependency id="{d}" version="{v}" />' for d, v in deps
    )
    nuspec = NUSPEC.format(
        id=pkg_id,
        lid=pkg_id.lower(),
        version=version,
        authors=", ".join(authors),
        description=description,
        tags=" ".join(tags),
        deps=dep_xml,
    )
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr(f"{pkg_id}.nuspec", nuspec)
        z.writestr("README.md", README.format(id=pkg_id, version=version,
                                              description=description))
        z.writestr("icon.png", icon(colour))
        z.writestr(f"lib/net8.0/{pkg_id}.dll", b"MZ" + b"\0" * 4096)
        z.writestr("[Content_Types].xml",
                   '<?xml version="1.0" encoding="utf-8"?><Types '
                   'xmlns="http://schemas.openxmlformats.org/package/2006/'
                   'content-types"><Default Extension="dll" '
                   'ContentType="application/octet" /></Types>')
    return buf.getvalue()


# Roughly how often each package has been restored. Real feeds have a long
# tail, and a gallery where every row says "0 downloads" reads as broken.
DOWNLOADS = {
    "Contoso.Build.Tools": 1284,
    "Acme.Logging": 3907,
    "Internal.Deploy.Cli": 412,
    "Fabrikam.Data.Sqlite": 176,
    "Northwind.Analyzers": 58,
}


def push(base_url: str, api_key: str, payload: bytes) -> int:
    req = urllib.request.Request(
        f"{base_url.rstrip('/')}/api/v2/package",
        data=payload,
        method="PUT",
        headers={"X-NuGet-ApiKey": api_key,
                 "Content-Type": "application/octet-stream"},
    )
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status
    except urllib.error.HTTPError as e:
        return e.code


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__.strip().splitlines()[-1], file=sys.stderr)
        return 2
    base_url, api_key = sys.argv[1], sys.argv[2]

    pushed = 0
    for pkg_id, versions, *rest in PACKAGES:
        for version in versions:
            status = push(base_url, api_key, build(pkg_id, version, *rest))
            if status not in (201, 409):
                print(f"push {pkg_id} {version} -> HTTP {status}", file=sys.stderr)
                return 1
            pushed += 1
    print(f"seeded {pushed} package versions")
    return 0


if __name__ == "__main__":
    sys.exit(main())
