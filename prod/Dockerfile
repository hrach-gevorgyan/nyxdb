FROM rust:1-slim AS builder
WORKDIR /app
COPY Cargo.toml ./
COPY db ./db
RUN cargo build --release --manifest-path db/Cargo.toml

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/couchdb-clone /usr/local/bin/couchdb-clone
ENV COUCHDB_CLONE_ADDR=0.0.0.0:5984
ENV COUCHDB_CLONE_DATA=/data
VOLUME ["/data"]
EXPOSE 5984
ENTRYPOINT ["/usr/local/bin/couchdb-clone"]
