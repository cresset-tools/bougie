# syntax=docker/dockerfile:1

# Base "binary-only" image for bougie: a `scratch` image whose entire
# contents are the two static-musl binaries `/bougie` and `/bgx`.
#
# Consume it from your own Dockerfile with:
#
#     COPY --from=ghcr.io/cresset-tools/bougie:latest /bougie /bgx /usr/local/bin/
#
# Built natively for both linux/amd64 and linux/arm64 — each arch compiles
# on its own GitHub-hosted runner (amd64 on ubuntu-latest, arm64 on
# ubuntu-24.04-arm), no QEMU, no cross-compile. The `build` stage runs on
# Alpine, so musl is the native libc and a plain `cargo build --release`
# yields a fully static binary on each arch; the same image therefore drops
# into scratch, alpine, and debian alike. The final `scratch` stage runs no
# code — it just holds the native-arch binaries.
#
# aws-lc-sys (pulled in via sigstore) is the only C/asm dependency that needs
# more than a Rust toolchain — it wants cmake + clang at build time, plus the
# usual musl build deps.

# Bumps to this flow through PRs so the image stays reproducible. RUST_VERSION
# tracks rust-toolchain.toml (the toolchain file still wins at build time; this
# only picks the base image).
ARG RUST_VERSION=1.95

FROM rust:${RUST_VERSION}-alpine AS build

# aws-lc-sys build deps (cmake + clang) plus the native musl build toolchain.
RUN apk add --no-cache cmake clang musl-dev make perl linux-headers

WORKDIR /src
# Materialize the toolchain pinned by rust-toolchain.toml before the sources so
# this layer stays cached across source-only changes. rust:alpine's default
# host target is already the native musl triple, so no `rustup target add` is
# needed — the static build is the default.
COPY rust-toolchain.toml .
RUN rustup toolchain install

COPY . .

# Build both shipped binaries. The cargo registry/git caches and the target
# dir are mounted as build caches so reruns are fast; they live on the builder
# and are never baked into the image. Copy the artifacts out of the target
# cache mount in the same RUN so they survive into the next layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --bin bougie --bin bgx \
 && cp target/release/bougie /bougie \
 && cp target/release/bgx /bgx

FROM scratch
COPY --from=build /bougie /bgx /
WORKDIR /io
# Telemetry consent hygiene (TELEMETRY.md): default the published image
# to off so an interactive `docker run -it` can't first-run-prompt and
# bake a consent file into a layer someone else inherits. Plain ENV =
# default-but-overridable: `docker run -e BOUGIE_TELEMETRY=on` (or a
# GitHub Actions `env:` block) wins. Note the env outranks the mode
# file, so `bougie telemetry on` *inside* a container won't stick —
# `telemetry status` names the source; override the env instead.
ENV BOUGIE_TELEMETRY=off
ENTRYPOINT ["/bougie"]
