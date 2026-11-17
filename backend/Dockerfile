# syntax=docker/dockerfile:1.7
# Multi-stage build with cargo-chef (Zero To Production, ch. 5): dependencies are compiled
# in their own cached layer, so code-only changes rebuild in seconds.

FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef
WORKDIR /app

FROM chef AS planner
COPY backend/ .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY backend/ .
RUN cargo build --release --bin vitals_monitor --bin vitalsctl

# ---- runtime: small, non-root, read-only friendly
FROM debian:bookworm-slim AS runtime
RUN apt-get update -y \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /app --shell /usr/sbin/nologin app
WORKDIR /app
COPY --from=builder /app/target/release/vitals_monitor /app/target/release/vitalsctl /usr/local/bin/
COPY config /app/config
ENV APP_ENVIRONMENT=prod APP_CONFIG_DIR=/app/config
USER app
EXPOSE 8000
HEALTHCHECK --interval=15s --timeout=3s --start-period=10s --retries=3 CMD ["vitalsctl", "health"]
ENTRYPOINT ["vitals_monitor"]
