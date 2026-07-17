# Quickstart

The short version — from a fresh clone to a running project:

```shellbox
bougie sync              # install PHP, extensions and vendor/
bougie up                # start MariaDB, Redis, OpenSearch, …
bougie server            # a local https:// URL for the project
```

The rest of this page walks through each step.

## Start a project

Create a new project, or adopt an existing Composer project as-is:

```sh
bougie new my-app        # scaffold a fresh project in ./my-app
# — or —
cd existing-project
bougie sync              # installs PHP, extensions and vendor/
```

`bougie sync` reads `composer.json` / `composer.lock`, provisions the
right PHP version and extensions, and installs dependencies — no
system PHP required.

## Manage dependencies

```sh
bougie add monolog/monolog        # add a dependency
bougie add phpunit/phpunit --dev  # add a dev dependency
bougie remove monolog/monolog     # remove it again
bougie lock                       # refresh composer.lock minimally
bougie tree                       # inspect the dependency tree
bougie outdated                   # what has newer releases?
```

Prefer the Composer verbs you already know? They're all there, natively:

```sh
bougie composer install
bougie composer update
bougie composer why vendor/package
```

## Run PHP

`bougie run` executes any command inside the project environment — the
pinned PHP, the project's extensions, vendor binaries on `PATH`:

```sh
bougie run -- php -v
bougie run -- php bin/magento cache:flush
bougie run --xdebug -- php test.php   # xdebug overlay, one-off
```

## Services

Declare the services your project needs, then:

```sh
bougie up                  # start them (MariaDB, Redis, OpenSearch, …)
bougie service status      # what's running?
bougie service exec mariadb   # open a client wired to your project
bougie service credentials    # connection info for GUI tools
bougie down                # stop them
```

Services run natively (no Docker), sandboxed, and each project gets its
own isolated tenant — two projects can share one MariaDB instance
without seeing each other's databases.

## Dev server

```sh
bougie server
```

Registers the project with the local dev server and prints its
`https://` URL on a `bougie.run` subdomain — TLS and FastCGI are
handled for you.

## Tasks

```sh
bougie start     # run the project's default recipe
bougie make lint # run a specific task
```

For Magento / Mage-OS projects, a built-in recipe takes a fresh clone
to a running shop with just `bougie start`.

## Global tools

Run any Composer CLI tool without installing it into your project,
npx-style:

```sh
bgx laravel/pint                       # ephemeral run
bougie tool install phpstan/phpstan    # install onto your PATH
```

Each tool lives in its own isolated vendor tree with its own pinned PHP.
