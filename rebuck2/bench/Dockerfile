# Bench fleet node: builds rebuck2 from the LOCAL working tree so engine
# edits bench in seconds without a push. Multi-stage keeps the runtime
# image lean and the cargo layer cached.
FROM rust:1.92-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --bin rebuck2

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/rebuck2 /usr/local/bin/rebuck2
