# syntax=docker/dockerfile:1
# Multi-stage build: compile with full Rust toolchain, ship only the binary.

FROM rust:1.77-alpine AS builder

RUN apk add --no-cache musl-dev

WORKDIR /app

# Copy manifests first so dependency layer is cached separately from src.
COPY Cargo.toml Cargo.lock ./
COPY crates/types/Cargo.toml       crates/types/Cargo.toml
COPY crates/matching/Cargo.toml    crates/matching/Cargo.toml
COPY crates/matcher/Cargo.toml     crates/matcher/Cargo.toml
COPY crates/api-server/Cargo.toml  crates/api-server/Cargo.toml

# Stub out all lib/main files so `cargo build` caches deps without real source.
RUN mkdir -p crates/types/src crates/matching/src crates/matcher/src crates/api-server/src \
 && echo "fn main(){}" > crates/matcher/src/main.rs \
 && echo "fn main(){}" > crates/api-server/src/main.rs \
 && echo "" > crates/types/src/lib.rs \
 && echo "" > crates/matching/src/lib.rs

ARG CRATE=api-server
RUN cargo build --release -p ${CRATE} 2>/dev/null || true

# Now copy real source and rebuild (only app code recompiles, deps are cached).
COPY crates/ crates/

ARG CRATE=api-server
RUN cargo build --release -p ${CRATE}

# ── Runtime image ─────────────────────────────────────────────────────────────
FROM alpine:3.19

RUN apk add --no-cache libgcc

ARG CRATE=api-server
COPY --from=builder /app/target/release/${CRATE} /usr/local/bin/app

ENV RUST_LOG=info
ENTRYPOINT ["/usr/local/bin/app"]
