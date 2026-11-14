# Rustis: MOTIS-compatible public transport router.
#
# Runtime layout (one volume at /data):
#   /data/feeds/       downloaded GTFS/NeTEx files (conditional GET cache)
#   /data/snapshots/   published snapshots (the last --keep are kept)
#   /data/current      symlink to the snapshot being served
# The MOTIS config.yml listing the datasets is expected at /config/motis-config.yml.
#
# Serve (default command) reloads /data/current every 60 s, so a daily
#   rustis update --root /data --motis-config /config/motis-config.yml
# (e.g. as a scheduled task in the same container) publishes without restart.

FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked && strip target/release/rustis

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends curl \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/rustis /usr/local/bin/rustis
VOLUME /data
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=120s --retries=3 \
  CMD curl -fsS http://127.0.0.1:8080/health >/dev/null || exit 1
ENTRYPOINT ["rustis"]
CMD ["serve", "--snapshot", "/data/current", "--listen", "0.0.0.0:8080", \
     "--max-concurrency", "2", "--reload-interval", "60"]
