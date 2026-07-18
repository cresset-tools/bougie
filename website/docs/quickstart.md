# Quickstart

## Start a project

Lorem ipsum dolor sit amet, consectetur adipiscing elit. Sed do eiusmod
tempor incididunt ut labore et dolore magna aliqua.

```sh
bougie new my-app        # lorem ipsum dolor sit amet
# — or —
cd existing-project
bougie sync              # consectetur adipiscing elit
```

Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris nisi
ut aliquip ex ea commodo consequat.

## Manage dependencies

Lorem ipsum dolor sit amet, consectetur adipiscing elit:

```sh
bougie add monolog/monolog        # lorem ipsum dolor
bougie add phpunit/phpunit --dev  # sit amet consectetur
bougie remove monolog/monolog     # adipiscing elit sed
bougie lock                       # do eiusmod tempor
bougie tree                       # incididunt ut labore
bougie outdated                   # et dolore magna aliqua
```

Duis aute irure dolor in reprehenderit in voluptate velit esse cillum
dolore eu fugiat nulla pariatur.

## Run PHP

Lorem ipsum dolor sit amet, consectetur adipiscing elit:

```sh
bougie run -- php -v
bougie run -- php bin/magento cache:flush
bougie run --xdebug -- php test.php   # lorem ipsum dolor
```

## Services

Lorem ipsum dolor sit amet, consectetur adipiscing elit. Sed do eiusmod
tempor incididunt ut labore et dolore magna aliqua.

```sh
bougie up                  # lorem ipsum dolor sit amet
bougie service status      # consectetur adipiscing elit
bougie service exec mariadb   # sed do eiusmod tempor
bougie service credentials    # incididunt ut labore
bougie down                # et dolore magna aliqua
```

Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris.

## Dev server

Lorem ipsum dolor sit amet, consectetur adipiscing elit:

```sh
bougie server
```

Duis aute irure dolor in reprehenderit in voluptate velit esse cillum
dolore eu fugiat nulla pariatur.

## Tasks

Lorem ipsum dolor sit amet, consectetur adipiscing elit:

```sh
bougie start
bougie make lint
```

## Global tools

Lorem ipsum dolor sit amet, consectetur adipiscing elit:

```sh
bgx laravel/pint
```

Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris nisi
ut aliquip ex ea commodo consequat.
