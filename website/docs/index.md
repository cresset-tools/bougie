# What is bougie?

bougie gets big PHP projects running with ease. It is a package manager,
a PHP toolchain manager, a services manager, a dev server, and a task
runner — a single fast binary written in Rust, in the spirit of what
[uv](https://docs.astral.sh/uv/) did for Python.

It was originally built for Magento development, but it works for any
Composer-based PHP project.

Using bougie means no worries about:

- Different PHP versions
- PHP native extensions
- Waiting for vendor downloads
- Getting your global PHP tools installed (PHPStan, Pint, …)
- Getting installs of MariaDB running — or OpenSearch, Redis, RabbitMQ
- Running an HTTP server for local development

## How it fits together

**As a Composer replacement.** `bougie sync` resolves and installs your
project's dependencies with a native resolver and parallel downloads —
dramatically faster than Composer, while staying compatible with
`composer.json` and `composer.lock`. The generated autoloader is
byte-equivalent to Composer's. The `bougie composer` subcommands
(`install`, `update`, `require`, `show`, `why`, …) are native
reimplementations, so existing muscle memory and scripts keep working.

**As a PHP manager.** bougie downloads self-contained PHP builds and
extensions per project. `bougie php pin 8.4` and you're done — every
`bougie run` and every service uses the right interpreter.

**As a services manager.** For native speed, bougie doesn't run services
in Docker. `bougie up` starts MariaDB, Redis, OpenSearch, RabbitMQ or
Mailpit directly on your machine, sandboxed with native OS tooling, each
project isolated in its own tenant. On macOS you don't need Docker
installed and no Linux VM runs in the background.

**As a dev server.** `bougie server` registers your project with a
shared local server and gives it a stable `https://` URL on a
`bougie.run` subdomain, TLS included.

**As a task runner.** `bougie make` is a portable, built-in take on
`make`. `bougie start` runs the default recipe — for Magento and Mage-OS
projects a recipe ships built in, so one command takes you from clone to
a running shop.

## Next steps

- [Install bougie](/docs/installation)
- [Quickstart](/docs/quickstart)
