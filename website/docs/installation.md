# Installation

## Installer script (macOS and Linux)

```sh
curl -LsSf https://bougie.tools/install.sh | sh
```

The installer downloads the right binary for your platform and puts it
on your `PATH`.

## Cargo

If you have a Rust toolchain, you can build from source instead:

```sh
cargo install bougie
```

## Platform support

bougie runs on Linux, macOS and Windows. The services stack
(`bougie up`, background service supervision) is Unix-only; on Windows,
package management, PHP toolchain management and `bougie server` are
supported.

## Updating

bougie can update itself in place:

```sh
bougie self update
```

## Telemetry

bougie can collect anonymous usage statistics and crash reports —
strictly **opt-in**, asked once at install time or on first interactive
run. It never collects project names, package names, paths, or IP
addresses, and it honors `DO_NOT_TRACK`. Inspect exactly what would be
sent with `bougie telemetry log`; switch with
`bougie telemetry on`, `off`, or `local`. The complete field list and
policy live in
[TELEMETRY.md](https://github.com/cresset-tools/bougie/blob/main/TELEMETRY.md).
