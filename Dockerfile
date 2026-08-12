# Build stage
#
# Pinned to the crate's `rust-version` (see Cargo.toml), not to the newest
# stable. `rust-toolchain.toml` is deliberately not copied into this stage, so
# this tag really is the compiler that runs — which makes the image build a
# second, independent check that the crate still compiles at its declared MSRV.
# Raise it only together with `rust-version`, the `msrv` job in CI, and the
# floor documented in the README.
FROM rust:1.88-slim AS builder
WORKDIR /app

# Cache dependencies first. `build.rs` has to be present even for this dummy
# build: it is what stages the embeddable documentation site into `OUT_DIR`, and
# `src/web/docs.rs` does not compile without it.
COPY Cargo.toml Cargo.lock build.rs ./
RUN mkdir src \
    && echo "fn main() {}" > src/main.rs \
    && echo "" > src/lib.rs \
    && cargo build --release --locked 2>/dev/null || true
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
COPY mkdocs.yml requirements-docs.txt README.md ./
RUN set -eu; \
    if apt-get update \
       && apt-get install -y --no-install-recommends python3 python3-venv >/dev/null \
       && python3 -m venv /opt/docs-venv \
       && /opt/docs-venv/bin/pip install --no-cache-dir -r requirements-docs.txt \
       && /opt/docs-venv/bin/mkdocs build --strict; then \
      echo "documentation site built"; \
    elif [ "$REQUIRE_DOCS" = "1" ]; then \
      echo "REQUIRE_DOCS=1 and the documentation site failed to build" >&2; \
      exit 1; \
    else \
      echo "docs build skipped; the placeholder page will be embedded"; \
    fi; \
    rm -rf /var/lib/apt/lists/* /opt/docs-venv

# Build the real sources.
COPY src ./src
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

# Runtime stage
FROM debian:bookworm-slim AS runtime

LABEL org.opencontainers.image.title="YANuget" \
      org.opencontainers.image.description="Yet Another NuGet server — a fast, streaming NuGet v3 server in Rust" \
      org.opencontainers.image.source="https://github.com/thoscut/yanuget" \
      org.opencontainers.image.documentation="https://github.com/thoscut/yanuget#readme" \
      org.opencontainers.image.licenses="MIT"

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# Run unprivileged. The server needs nothing beyond its data directory, and a
# container escape from a root process is a far worse day than one from an
# unprivileged process.
RUN useradd --system --uid 10001 --home-dir /data --no-create-home yanuget \
    && mkdir -p /data \
    && chown -R yanuget:yanuget /data

WORKDIR /app
COPY --from=builder /app/target/release/yanuget /usr/local/bin/yanuget

# Persist packages and the database here.
VOLUME ["/data"]
ENV YANUGET_DATA_DIR=/data \
    YANUGET_HOST=0.0.0.0 \
    YANUGET_PORT=5000
EXPOSE 5000

USER yanuget

# `/health` probes the database, so a container whose storage has gone away is
# reported unhealthy rather than merely "process alive". TLS is on by default
# with a self-signed certificate, so the probe deliberately does not verify the
# chain — it is checking this process, not the certificate.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsSk "https://127.0.0.1:${YANUGET_PORT}/health" \
        || curl -fsS "http://127.0.0.1:${YANUGET_PORT}/health" \
        || exit 1

# Set YANUGET_API_KEY at runtime to require authentication for push/delete.
ENTRYPOINT ["yanuget"]
