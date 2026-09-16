FROM rust:1.88-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libsqlite3-0 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/slop-proxy /usr/local/bin/slop-proxy
RUN useradd --system --home-dir /data --create-home slop-proxy
USER slop-proxy
VOLUME ["/data"]
EXPOSE 8484
ENTRYPOINT ["/usr/local/bin/slop-proxy"]
CMD ["--db", "/data/slop.db", "serve", "--bind", "0.0.0.0:8484"]
