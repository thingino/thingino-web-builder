# Build the broker, then ship a slim runtime image with the static frontend baked in.
FROM rust:1-bookworm AS build
WORKDIR /src/broker
COPY broker/Cargo.toml broker/Cargo.lock ./
COPY broker/src ./src
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
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
ENTRYPOINT ["/app/thingino-build-broker"]
