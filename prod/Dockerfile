FROM rust:1-slim AS builder
WORKDIR /app
COPY Cargo.toml ./
COPY db ./db
RUN cargo build --release --manifest-path db/Cargo.toml

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --home-dir /data --shell /usr/sbin/nologin nyxdb
COPY --from=builder /app/target/release/nyxdb /usr/local/bin/nyxdb
ENV NYXDB_ADDR=0.0.0.0:5984
ENV NYXDB_DATA=/data
VOLUME ["/data"]
EXPOSE 5984
USER nyxdb
ENTRYPOINT ["/usr/local/bin/nyxdb"]
