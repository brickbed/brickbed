FROM rust:1.88-slim AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && groupadd --system brickbed \
    && useradd --system --gid brickbed --home-dir /data brickbed \
    && mkdir -p /data \
    && chown brickbed:brickbed /data \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/brickbed-server /usr/local/bin/brickbed-server
ENV HOST=0.0.0.0
ENV PORT=3001
ENV STORAGE_PATH=/data
WORKDIR /data
USER brickbed
EXPOSE 3001
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD curl --fail --silent http://127.0.0.1:3001/ready >/dev/null || exit 1
CMD ["brickbed-server"]
