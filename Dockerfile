# Build stage
FROM rust:1.82-slim AS builder
WORKDIR /app

# Cache dependencies first.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo "fn main() {}" > src/main.rs \
    && echo "" > src/lib.rs \
    && cargo build --release 2>/dev/null || true
RUN rm -rf src

# Build the real sources.
COPY src ./src
RUN touch src/main.rs src/lib.rs && cargo build --release

# Runtime stage
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/yanuget /usr/local/bin/yanuget

# Persist packages and the database here.
VOLUME ["/data"]
ENV YANUGET_DATA_DIR=/data \
    YANUGET_HOST=0.0.0.0 \
    YANUGET_PORT=5000
EXPOSE 5000

# Set YANUGET_API_KEY at runtime to require authentication for push/delete.
ENTRYPOINT ["yanuget"]
