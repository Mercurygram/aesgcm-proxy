# SPDX-FileCopyrightText: 2026 Timothy Redaelli <timothy.redaelli@gmail.com>
#
# SPDX-License-Identifier: MIT OR Apache-2.0

# aesgcm-proxy — multi-stage OCI build.
# Built with the rustls feature so the binary links no OpenSSL and stays a
# static musl executable; the runtime image carries only ca-certificates.

FROM docker.io/library/rust:1-alpine AS builder

# aws-lc-rs (the rustls crypto backend) builds native C via cmake; nasm is used
# for the x86_64 assembly fast path (ignored on other arches).
RUN apk add --no-cache musl-dev gcc g++ make cmake perl nasm

WORKDIR /build

# Cache the dependency build separately from the source: compile a stub main
# against the locked manifest first, then drop it and build the real source.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked --no-default-features --features rustls \
    && rm -rf src

COPY src ./src
RUN touch src/main.rs \
    && cargo build --release --locked --no-default-features --features rustls

FROM docker.io/library/alpine:3

LABEL org.opencontainers.image.description="Legacy WebPush (aesgcm Draft-04) to UnifiedPush rewrite proxy for Mercurygram"
LABEL org.opencontainers.image.licenses="MIT OR Apache-2.0"

# rustls-native-certs reads the system trust store at runtime.
RUN apk add --no-cache ca-certificates \
    && addgroup -S proxy \
    && adduser -S -G proxy proxy

COPY --from=builder /build/target/release/aesgcm-proxy /usr/local/bin/aesgcm-proxy

USER proxy

# Bind all interfaces inside the container (the systemd deployment instead uses
# socket activation and leaves LISTEN_ADDR unset).
ENV LISTEN_ADDR=0.0.0.0:8001
EXPOSE 8001

ENTRYPOINT ["/usr/local/bin/aesgcm-proxy"]
