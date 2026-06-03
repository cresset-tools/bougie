# syntax=docker/dockerfile:1

# Base "binary-only" image for bougie: a `scratch` image whose entire
# contents are the two static-musl binaries `/bougie` and `/bgx`.
#
# Consume it from your own Dockerfile with:
#
#     COPY --from=ghcr.io/cresset-tools/bougie:latest /bougie /bgx /usr/local/bin/
#
# Built for both linux/amd64 and linux/arm64 from a single amd64 builder
# via cargo-zigbuild: the `build` stage is pinned to $BUILDPLATFORM (always
# native amd64) and cross-compiles to the requested $TARGETPLATFORM's musl
# target, so no QEMU and no arm runner is needed. The final `scratch` stage
# runs no code — it just holds the target-arch binary, so buildx never has
# to emulate arm64.
#
# The binaries are static (no libc), so the same image drops into scratch,
# alpine, and debian alike. aws-lc-sys (pulled in via sigstore) is the only
# C/asm dependency that needs more than zig — it wants cmake + clang/libclang
# at build time. Verified cross-building clean to aarch64-unknown-linux-musl
# with zig 0.16.0 + cargo-zigbuild 0.22.3.

# Bumps to these flow through PRs so the image stays reproducible. RUST_VERSION
# tracks rust-toolchain.toml (the toolchain file still wins at build time; this
# only picks the base image).
ARG RUST_VERSION=1.95
ARG ZIG_VERSION=0.16.0
ARG CARGO_ZIGBUILD_VERSION=0.22.3

FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-bookworm AS build
ARG ZIG_VERSION
ARG CARGO_ZIGBUILD_VERSION

# aws-lc-sys build deps (cmake + clang/libclang) plus the tools to fetch zig.
RUN apt-get update && apt-get install -y --no-install-recommends \
      cmake clang curl ca-certificates xz-utils \
 && rm -rf /var/lib/apt/lists/*

# Pinned zig toolchain — the cross-linker that cargo-zigbuild drives. The
# builder is always amd64 (see $BUILDPLATFORM above), so we always grab the
# x86_64-linux zig.
RUN curl --proto '=https' --tlsv1.2 -fsSL \
      "https://ziglang.org/download/${ZIG_VERSION}/zig-x86_64-linux-${ZIG_VERSION}.tar.xz" \
      -o /tmp/zig.tar.xz \
 && tar -xJf /tmp/zig.tar.xz -C /opt \
 && ln -s "/opt/zig-x86_64-linux-${ZIG_VERSION}/zig" /usr/local/bin/zig \
 && rm /tmp/zig.tar.xz \
 && zig version

RUN cargo install --locked cargo-zigbuild@${CARGO_ZIGBUILD_VERSION}

# Map the buildx target platform to its musl rust target triple.
ARG TARGETPLATFORM
RUN case "${TARGETPLATFORM}" in \
      "linux/amd64") echo "x86_64-unknown-linux-musl"  > /rust-target ;; \
      "linux/arm64") echo "aarch64-unknown-linux-musl" > /rust-target ;; \
      *) echo "unsupported TARGETPLATFORM: ${TARGETPLATFORM}" >&2; exit 1 ;; \
    esac

WORKDIR /src
# Materialize the toolchain pinned by rust-toolchain.toml, then add the musl
# target to *that* toolchain — `rustup target add` applies to the active
# toolchain, which is only the pinned one once the file is in WORKDIR. Copying
# the toolchain file on its own first keeps this layer cached across
# source-only changes.
COPY rust-toolchain.toml .
RUN rustup toolchain install \
 && rustup target add "$(cat /rust-target)"

COPY . .

# Build both shipped binaries. The cargo registry/git caches and the target
# dir are mounted as build caches so reruns are fast; they live on the builder
# and are never baked into the image. Copy the artifacts out of the target
# cache mount in the same RUN so they survive into the next layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo zigbuild --release --target "$(cat /rust-target)" --bin bougie --bin bgx \
 && cp "target/$(cat /rust-target)/release/bougie" /bougie \
 && cp "target/$(cat /rust-target)/release/bgx" /bgx

FROM scratch
COPY --from=build /bougie /bgx /
WORKDIR /io
ENTRYPOINT ["/bougie"]
