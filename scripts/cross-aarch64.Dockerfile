# Cross-compile the aarch64 binary on an x86_64 host, with a glibc that matches
# production (Raspberry Pi / Debian 11 = glibc 2.31). The whole Rust + C cross
# toolchain lives inside the image, so the host needs only Docker (no rustup —
# which can't run on Guix anyway).
#
# Build the image once:
#   docker build -t daly-bms-exporter-cross-aarch64 -f scripts/cross-aarch64.Dockerfile scripts
# Or via the Makefile: `make cross-image` / `make cross-build`.
FROM rust:1-bullseye

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      gcc-aarch64-linux-gnu libc6-dev-arm64-cross \
 && rm -rf /var/lib/apt/lists/* \
 && rustup target add aarch64-unknown-linux-gnu

# Point cargo's linker (and the cc crate, should a C dependency appear later) at
# the aarch64 cross toolchain. This is the config that would otherwise live in
# .cargo/config.toml; here it is env-only. No OpenSSL / pkg-config: the exporter
# only receives HTTP and serves /metrics, so it has no C library dependencies.
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar

WORKDIR /src
