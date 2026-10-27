# Used by the release workflow: packages the prebuilt release binaries.
FROM debian:trixie-slim
ARG TARGETARCH
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 1000 --create-home --home-dir /data miner
COPY --chmod=755 bin/${TARGETARCH}/twitch-miner /usr/local/bin/twitch-miner
USER miner
WORKDIR /data
VOLUME /data
ENTRYPOINT ["twitch-miner"]
