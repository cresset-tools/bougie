# bougie container images

Published to GitHub Container Registry on every release:

| Image | Base | Use |
|-------|------|-----|
| `ghcr.io/cresset-tools/bougie:<version>` | `scratch` | Binary-only. Lift `/bougie` + `/bgx` into your own image. |
| `ghcr.io/cresset-tools/bougie:latest` | `scratch` | Same, tracking the latest stable release. |
| `ghcr.io/cresset-tools/bougie:<version>-debian-slim` | `debian:trixie-slim` | Runnable, glibc (fully functional on amd64 — see below). |
| `ghcr.io/cresset-tools/bougie:<version>-alpine` | `alpine:3.22` | Runnable, **musl/Alpine** (fully functional on amd64 — see below). |

Stable releases also publish rolling `<major>.<minor>` and bare-variant tags
(`debian-slim`, `alpine`); prereleases publish only the exact `<version>` /
`<version>-<variant>` tags.

Both `linux/amd64` and `linux/arm64` are published as a single multi-arch
manifest, so `docker pull` / `FROM` / `COPY --from` resolve the right arch
automatically.

## Platform support for `php install`

The `bougie` binary runs on every published image/arch. Fetching a **PHP
runtime** depends on what bougie's distribution index (`index.bougie.tools`)
ships: **`x86_64-unknown-linux-gnu` and `x86_64-unknown-linux-musl`** (plus
`aarch64-apple-darwin`, irrelevant to Linux containers). So `php install`
works on **both glibc and musl on amd64** — Alpine included. Only arm64 Linux
PHP isn't in the index yet.

| Image / arch | `bougie` runs | `bougie php install` |
|--------------|:--:|:--:|
| `debian-slim` · amd64 | ✓ | ✓ |
| `alpine` · amd64 | ✓ | ✓ |
| `debian-slim` · arm64 | ✓ | ✗ — no `aarch64-unknown-linux-gnu` in the index yet |
| `alpine` · arm64 | ✓ | ✗ — no `aarch64-unknown-linux-musl` in the index yet |

Both amd64 runnable images — **`debian-slim` and `alpine`** — are fully
functional today (the index gained `x86_64-unknown-linux-musl` in
php-build-standalone 0.2.5). The arm64 variants are published ahead of the
index: once php-build-standalone ships the `aarch64-*-linux-*` builds they
become functional with no image changes. On Apple Silicon, Docker Desktop runs
the amd64 image under Rosetta, so both variants work there too.

## Copy the binary into your own image

The `scratch` image exists to be a source for `COPY --from` — the binaries
are static (no libc), so they run in whatever stage you copy them into:

```dockerfile
FROM debian:trixie-slim
COPY --from=ghcr.io/cresset-tools/bougie:latest /bougie /bgx /usr/local/bin/
# bougie's multi-call argv[0] roles (php, composer, bougied, bougie-babysit)
# all live on the single /bougie binary; `bougie sync` creates the shims.
```

## Run bougie directly

Use a runnable variant — the bare `scratch` image has **no CA certificates**,
so it can't fetch PHP runtimes over TLS. Both `debian-slim` (glibc) and
`alpine` (musl) are fully functional on amd64:

```sh
# Debian (glibc)
docker run --rm -v "$PWD:/app" -w /app ghcr.io/cresset-tools/bougie:debian-slim sync
# Alpine (musl) — installs the musl PHP runtime from the index
docker run --rm -v "$PWD:/app" -w /app ghcr.io/cresset-tools/bougie:alpine sync
```

## How the images are built

`docker/Dockerfile` cross-compiles both arches from a single amd64 builder via
[`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild): the `build`
stage is pinned to `$BUILDPLATFORM` (always native amd64) and targets the
requested `$TARGETPLATFORM`'s `*-unknown-linux-musl` triple, with Zig as the
cross-linker. The final `scratch` stage runs no code, so buildx never emulates
arm64 — no QEMU, no arm runner.

`docker/Dockerfile.extra` builds the runnable variants by `COPY --from`-ing the
just-built base binaries onto a real OS base and installing CA certificates
(its only `RUN`). That cert install runs under QEMU on the arm64 leg — cheap
for a package install; the expensive Rust compile stays in the base image and
never needs emulation.

CI: `.github/workflows/build-docker.yml`, wired into the release as a
post-announce job via `post-announce-jobs` in `dist-workspace.toml`. It also
runs a push-free amd64 smoke build on PRs that touch the Dockerfiles or the
cargo sources.
