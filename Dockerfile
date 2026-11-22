# ProximaDB server: one static binary on an empty image, for linux/amd64 and linux/arm64.
#
#   docker buildx build --platform linux/amd64,linux/arm64 -t proximadb .
#
# Each platform builds natively for its own architecture (fastest on native CI runners; under emulation locally).
# The CPU baseline is fixed per architecture instead of this machine's `target-cpu=native`:
#   amd64: x86-64-v3 (AVX2: Intel since 2013, AMD since 2015)
#   arm64: neoverse-n1 (Armv8.2: AWS Graviton 2 and later, Ampere, Apple Silicon)

FROM rust:1-bookworm AS build
ARG TARGETARCH
RUN apt-get update && apt-get install -y --no-install-recommends musl-tools && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY rust-toolchain.toml ./
RUN case "$TARGETARCH" in \
      amd64) echo x86_64-unknown-linux-musl > /target ;; \
      arm64) echo aarch64-unknown-linux-musl > /target ;; \
      *) echo "unsupported architecture $TARGETARCH" && exit 1 ;; \
    esac \
 && rustup show active-toolchain \
 && rustup target add "$(cat /target)"
COPY Cargo.toml Cargo.lock ./
COPY src src
RUN case "$TARGETARCH" in amd64) cpu=x86-64-v3 ;; arm64) cpu=neoverse-n1 ;; esac \
 && RUSTFLAGS="-C target-cpu=$cpu -C target-feature=+crt-static" \
    cargo build --release --locked --features server --bin server --target "$(cat /target)" \
 && cp "target/$(cat /target)/release/server" /proximadb \
 && strip /proximadb \
 && mkdir -p /out/run/proximadb \
 && chown 65532:65532 /out/run/proximadb

# The bare binary, for release downloads: docker buildx build --target binary --output type=local,dest=out .
FROM scratch AS binary
COPY --from=build /proximadb /proximadb

FROM scratch
COPY --from=build /proximadb /proximadb
COPY --from=build /out/run /run
# Mount the schema at /config/schema.json and the data files at /data. Quarry's pool connects to port 7600, or to
# the socket in /run/proximadb when that directory is a volume shared with Quarry's container.
USER 65532:65532
EXPOSE 7600
ENTRYPOINT ["/proximadb"]
CMD ["--schema", "/config/schema.json", "--data", "/data", "--tcp", "0.0.0.0:7600", "--unix", "/run/proximadb/proximadb.sock"]
