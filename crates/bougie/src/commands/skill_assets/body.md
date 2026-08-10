# bougie

The PHP environment here is **not** the machine's PHP. bougie owns the
interpreter, its extensions, the `vendor/` tree, and the services the site
talks to. A bare `php`, `composer`, or `vendor/bin/phpunit` is at best a
different PHP than the project resolves to, and is usually missing outright.

## Is this a bougie project?

bougie claims any directory whose ancestry holds a `composer.json`, a
`bougie.toml`, or a `vendor/bougie/`. When none is present every command
exits with `no bougie project found`, and none of this applies.

Explicit configuration lives in `bougie.toml` (`[php]`, `[extensions]`,
`[services]`, `[server]`, `[scripts]`, `[patches]`) or, equivalently, under
`extra.bougie` in `composer.json`. Both are first class; a project may use
either.

## First: is the site already up?

Check before you run anything that needs a live environment, and prefer the
checks that change nothing:

| command | what it tells you | side effects |
| --- | --- | --- |
| `bougie doctor` | whether the config loads, the installed PHP matches the pin, declared services are healthy, the team login is fresh, and the db snapshot is current | none — it talks to `bougied` only if it is already running |
| `bougie service list` | which services the project declares | none, reads config |
| `bougie server status` | registered `*.bougie.run` hosts, and whether the shared dev server is running | none |
| `bougie service status` | per-service state, pid, uptime | starts the `bougied` supervisor if it isn't running (the supervisor only — no services) |

Every `bougie doctor` line that isn't `[ ok ]` names the command that fixes
it; trust those hints over anything you remember. Read the lines, not the
exit code — only `[fail]` exits non-zero, and a project that has never been
synced, with every service down, reports `[warn]` and exits 0.

## If it isn't set up: ask, don't start

`bougie start` is not a lookup. It syncs dependencies, downloads a PHP
toolchain and extensions when they're missing, brings up the declared
services (MariaDB, Redis, OpenSearch, RabbitMQ, …), may run the framework's
own installer, and serves the site. That is minutes of work, potentially
gigabytes of downloads, and it touches services shared with every other
bougie project on the machine.

So when the checks say the site isn't up, **stop and ask the user**, naming
what you found and what you would run:

> This project isn't up — `bougie doctor` reports PHP not synced and mariadb
> and redis not running. Want me to run `bougie start`? It installs the
> toolchain, syncs dependencies, and brings the services up; that usually
> takes a few minutes.

Wait for the answer. If it's no, keep working — reading and editing code
needs nothing running. Only the commands below do.

## Working in a running project

Everything runs *through* bougie, which puts the project's PHP, its
extensions, the `vendor/bougie/bin/` shims, and the `BOUGIE_SERVICE_*`
tenant environment in scope:

- any PHP or project binary — `bougie run php -v`, `bougie run bin/magento cache:flush`
- tests — `bougie run vendor/bin/phpunit`
- Composer verbs — `bougie composer install|update|show|why|audit|…` (native, no phar)
- add or remove a package — `bougie add vendor/pkg`, `bougie remove vendor/pkg`
- a one-off global tool, uvx-style — `bgx laravel/pint`
- format PHP — `bougie format`
- a database or cache client — `bougie service exec mariadb`, `bougie service exec redis-cli`
- connection details for an external client — `bougie service credentials`
- logs — `bougie service logs [name]`, `bougie server logs`
- the site's URL — `bougie server status`, or `bougie server open`

Every command accepts `--format json-v1` when you want to parse the result
instead of reading it.

## Don't

- Don't run the system `php`, `composer`, or `vendor/bin/*` directly. Go
  through `bougie run`.
- Don't hand-edit `vendor/` or `composer.lock`. `bougie sync` owns the
  vendor tree; `bougie add` / `bougie lock` rewrite the lock.
- Don't install PHP or extensions with the system package manager — that's
  `bougie php install` and `bougie ext add`.
- Don't rely on Composer plugin install hooks. bougie never runs them;
  what a plugin used to do is either reimplemented natively or absent.
- Don't run the destructive verbs unasked: `bougie stop --purge`,
  `bougie service remove --purge`, `bougie projects purge`. They destroy
  tenant data, databases included.
- Don't stop or restart a service casually. Services are shared across every
  project on the machine, so `bougie service restart mariadb` interrupts all
  of them.
