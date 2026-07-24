# Installation

## Installer script (macOS and Linux)

The easiest and recommended way to install bougie is our shell installer:

```sh
curl -LsSf https://bougie.tools/install.sh | sh
```

The installer sets up bougie in your `$XDG_DATA_HOME` (~/.local/share) and drops
the binary in `$XDG_BIN_HOME` (~/.local/bin). There's nothing to compile, and if
that bin directory isn't on your `PATH` yet, the installer tells you the line to
add.

## Cargo

If you already have Rust, you can build bougie from source instead:

```sh
cargo install bougie
```

This works on platforms we don't ship a binary for, like Intel Macs or ARM
Linux. A cargo install updates through cargo, so `bougie self update` won't touch
it.

## Docker

We also publish multi-arch images (amd64 and arm64) to GHCR, so you can run
bougie in CI or on an ARM Linux host without building it:

- `ghcr.io/cresset-tools/bougie:<version>-debian-slim` runs on `debian:trixie-slim`
- `ghcr.io/cresset-tools/bougie:<version>-alpine` is a smaller `alpine:3.22` image
- `ghcr.io/cresset-tools/bougie:latest` is a bare `scratch` image with just the
  binaries, for `COPY --from`

## Platform support

We ship prebuilt binaries for:

| Platform | Architecture |
| --- | --- |
| Linux (glibc) | x86_64 |
| Linux (musl) | x86_64 |
| macOS | Apple Silicon (aarch64) |
| Windows | x86_64 |

The whole toolchain works the same on Linux, macOS, and Windows. The one
exception is the background services (MariaDB, Redis, OpenSearch, RabbitMQ,
Mailpit), which are Unix-only. On Windows `bougie server` still runs your app
through `php-cgi.exe`, you just bring your own services.

## Updating

Update in place with:

```sh
bougie self update
```

This only touches a binary bougie installed itself, so if you used cargo or a
package manager, let that tool handle updates instead.

## Telemetry

bougie can send anonymous usage stats and crash reports so we know what to fix
and build next. It never sends your code, package names, file paths, or IP
addresses, and nothing goes out without your consent.

It's opt-in, so the installer asks you once. You can change it whenever:

```sh
bougie telemetry on      # turn it on
bougie telemetry local   # record locally, never upload
bougie telemetry off     # off completely
bougie telemetry status  # see the current mode
```

To see exactly what would be sent, run `bougie telemetry log`. The full field
list is on [the telemetry page](https://bougie.tools/telemetry).
