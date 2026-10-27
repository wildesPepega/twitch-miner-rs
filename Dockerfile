# Builds twitch-miner from source.
# Data (config.toml, token file) lives in /data; mount a volume there.
FROM rust:1-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 1000 --create-home --home-dir /data miner
COPY --from=build /src/target/release/twitch-miner /usr/local/bin/twitch-miner
USER miner
WORKDIR /data
VOLUME /data
ENTRYPOINT ["twitch-miner"]
