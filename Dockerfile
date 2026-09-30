# Build stage
#
# Pinned to the crate's `rust-version` (see Cargo.toml), not to the newest
# stable. `rust-toolchain.toml` is deliberately not copied into this stage, so
# this tag really is the compiler that runs — which makes the image build a
# second, independent check that the crate still compiles at its declared MSRV.
# Raise it only together with `rust-version`, the `msrv` job in CI, and the
# floor documented in the README.
#
# Both base images are pinned by digest as well as by tag, so a rebuild of the
# same commit starts from the same bytes rather than from whatever the tag
# points at that day. Dependabot's docker ecosystem moves the digests (and,
# for the runtime base, the tag); the builder's tag stays where it is on
# purpose, see .github/dependabot.yml. Builder and runtime are both Debian
# trixie, so the binary never needs a newer glibc than the image ships.
#
# The builder runs on the build machine's own architecture and cross-compiles
# for the image's (`TARGETARCH`), because compiling the arm64 image under QEMU
# emulation works but takes an order of magnitude longer.
FROM --platform=$BUILDPLATFORM rust:1.88-slim-trixie@sha256:9a7159329166b45f453351a077367f501aa3e98378f7e327530e7966a139d05f AS builder
ARG TARGETARCH
WORKDIR /app

# The Rust target for the image's architecture, and a C cross toolchain when it
# is not the builder's own: `ring` and the bundled SQLite compile C code. The
# linker and compiler variables name the target triplet's gcc, which Debian's
# native `gcc` also provides, so they are right for a native build too.
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) triple=x86_64-unknown-linux-gnu; gnu=x86-64-linux-gnu ;; \
      arm64) triple=aarch64-unknown-linux-gnu; gnu=aarch64-linux-gnu ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    if [ "$(dpkg --print-architecture)" != "$TARGETARCH" ]; then \
      apt-get update; \
      apt-get install -y --no-install-recommends "gcc-$gnu" "libc6-dev-$TARGETARCH-cross"; \
      rm -rf /var/lib/apt/lists/*; \
    fi; \
    rustup target add "$triple"; \
    echo "$triple" > /rust-target

# Cache dependencies first. `build.rs` has to be present even for this dummy
# build: it is what stages the embeddable documentation site into `OUT_DIR`, and
# `src/web/docs.rs` does not compile without it.
COPY Cargo.toml Cargo.lock build.rs ./
RUN mkdir src \
    && echo "fn main() {}" > src/main.rs \
    && echo "" > src/lib.rs \
    && cargo build --release --locked --target "$(cat /rust-target)" 2>/dev/null || true
RUN rm -rf src

# Render the documentation so the real site is embedded rather than the
# placeholder. Best-effort by default: a build without network access still
# produces a working binary, just with the "docs not bundled" page.
#
# Pass `--build-arg REQUIRE_DOCS=1` to make a failure here fatal instead. The
# release workflow does, because an image that silently ships the placeholder
# looks exactly like a good one until someone opens /docs.
ARG REQUIRE_DOCS=0
COPY docs ./docs
# The theme's `custom_dir` (the logo icon). Without it `mkdocs build` fails.
COPY overrides ./overrides
COPY mkdocs.yml requirements-docs.txt README.md ./
# `--require-hashes`: the documentation toolchain runs inside the release
# build, so it installs exactly the reviewed, hashed set in the lock.
RUN set -eu; \
    if apt-get update \
       && apt-get install -y --no-install-recommends python3 python3-venv >/dev/null \
       && python3 -m venv /opt/docs-venv \
       && /opt/docs-venv/bin/pip install --no-cache-dir --require-hashes -r requirements-docs.txt \
       && /opt/docs-venv/bin/mkdocs build --strict; then \
      echo "documentation site built"; \
    elif [ "$REQUIRE_DOCS" = "1" ]; then \
      echo "REQUIRE_DOCS=1 and the documentation site failed to build" >&2; \
      exit 1; \
    else \
      echo "docs build skipped; the placeholder page will be embedded"; \
    fi; \
    rm -rf /var/lib/apt/lists/* /opt/docs-venv

# Build the real sources. `build.rs` is touched too: the site did not exist
# when the dependency build ran it, and it is what copies the site in.
COPY src ./src
RUN touch src/main.rs src/lib.rs build.rs \
    && cargo build --release --locked --target "$(cat /rust-target)" \
    && cp "target/$(cat /rust-target)/release/yanuget" /yanuget

# Runtime stage
FROM debian:trixie-slim@sha256:a99cfc517144bc59b1978475ec53b46ecabec7e43635402ee5b77cc54cd1b20a AS runtime

LABEL org.opencontainers.image.title="YANuget" \
      org.opencontainers.image.description="Yet Another NuGet server — a fast, streaming NuGet v3 server in Rust" \
      org.opencontainers.image.source="https://github.com/thoscut/yanuget" \
      org.opencontainers.image.documentation="https://github.com/thoscut/yanuget#readme" \
      org.opencontainers.image.licenses="MIT"

# The system trust store, for outbound TLS (mirroring, migration). Nothing
# else: the health check is the binary itself, so there is no `curl` here.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Run unprivileged. The server needs nothing beyond its data directory, and a
# container escape from a root process is a far worse day than one from an
# unprivileged process.
RUN useradd --system --uid 10001 --home-dir /data --no-create-home yanuget \
    && mkdir -p /data \
    && chown -R yanuget:yanuget /data

WORKDIR /app
COPY --from=builder /yanuget /usr/local/bin/yanuget

# Persist packages and the database here.
VOLUME ["/data"]
ENV YANUGET_DATA_DIR=/data \
    YANUGET_HOST=0.0.0.0 \
    YANUGET_PORT=5000
EXPOSE 5000

USER yanuget

# The readiness probe checks the database, so a container whose storage has
# gone away is reported unhealthy rather than merely "process alive".
# `yanuget healthcheck` loads the same configuration as the server, so it
# follows a port or `tls_enabled` set in the TOML file as well as in the
# environment; pass the file as `YANUGET_CONFIG` rather than `--config`, since
# the health check does not see the container's arguments. It does not verify
# the certificate: it is checking this process, and the default one is
# self-signed.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["yanuget", "healthcheck"]

# Set YANUGET_API_KEY at runtime to require authentication for push/delete.
ENTRYPOINT ["yanuget"]
