# Build the broker, then ship a slim runtime image with the static frontend baked in.
FROM rust:1-bookworm AS build
ARG BUILD_SHA=dev
ENV BUILD_SHA=$BUILD_SHA
WORKDIR /src/broker
COPY broker/Cargo.toml broker/Cargo.lock ./
COPY broker/src ./src
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --user-group --no-create-home app \
    && mkdir -p /data /state && chown app:app /data /state
WORKDIR /app
COPY --from=build /src/broker/target/release/thingino-build-broker .
COPY web ./web
COPY defconfigs.json ./
ENV BIND_ADDR=0.0.0.0:8080 \
    STATIC_DIR=/app/web \
    DEFCONFIGS_PATH=/app/defconfigs.json \
    DB_PATH=/data/broker.db \
    LOCK_PATH=/data/broker.lock \
    PID_PATH=/data/broker.pid
VOLUME ["/data"]
EXPOSE 8080
# Drop privileges. Bind port is 8080 (unprivileged); /data + /state are mounted
# with :U so Podman chowns them to this uid. Caddy (host-net, ports 80/443) is the
# only piece that needs root.
USER app
ENTRYPOINT ["/app/thingino-build-broker"]
