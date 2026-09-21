# Install cargo-chef
FROM rust:1.88-slim AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

# Compute dependency recipe
FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

# Build dependencies only (cached when Cargo.toml/Cargo.lock unchanged)
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
# Build the app binary (only this layer reruns on src/ changes)
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

# Runtime stage — no libc needed (pure UDP/TCP, no TLS)
FROM gcr.io/distroless/cc-debian12
COPY --from=builder /app/target/release/dnsglobe /usr/local/bin/dnsglobe
ENTRYPOINT ["/usr/local/bin/dnsglobe"]
