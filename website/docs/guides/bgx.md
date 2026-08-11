# Run tools with bgx

`bgx` runs a PHP command line tool straight from its Composer package. Nothing
is added to your `composer.json`, and nothing lands on your `PATH`. It comes
with bougie and works the way `npx` does for Node.

```sh
bgx laravel/pint
```

The first run gives Pint its own vendor directory and its own PHP, somewhere out
of the way. Later runs reuse that and start right away.

`bgx` is shorthand for `bougie tool run`. They're the same command, so
everything below works with either.

## Arguments go straight to the tool

Everything after the package name is passed on as is, so you never need a `--`
separator:

```bash
bgx phpstan/phpstan analyse src --level=8
```

bougie's own options go before the package. `bgx --php 8.4 laravel/pint` runs
Pint on PHP 8.4, while `bgx laravel/pint --php 8.4` hands `--php 8.4` to Pint,
which won't know what to do with it.

Exit codes and signals pass through, so bgx works as the command in a CI step or
anywhere else you'd have run the tool yourself.

## Pinning a version

Add `@` and a constraint to ask for a specific version. It's an ordinary
Composer constraint, dev branches included:

```bash
bgx phpstan/phpstan@^1.10 analyse src
bgx friendsofphp/php-cs-fixer@dev-master fix
```

Without a constraint you get the newest stable release that fits the PHP the run
ends up on.

## Choosing a PHP

Usually you don't have to. bougie starts from the tool's own `require.php`,
narrows that with the project you're standing in, and otherwise takes the newest
PHP you have installed. To pick one yourself:

```sh
bgx --php 8.4 laravel/pint
```

That takes a version like `8.3` or `8.3.12`, or a constraint like `~8.3`. If the
PHP isn't installed yet, bougie installs it first. Tools always run on a managed
PHP, never your system one.

## Extra packages and extensions

`--with` adds something to the tool for this run. A name with a slash is a
Composer package, a name without one is a PHP extension:

```bash
bgx --with phpstan/phpstan-symfony --with intl phpstan/phpstan analyse src
```

You can pass it as often as you need. Most of the time you don't need it at all:
the tool's own `require.ext-*` entries are installed for you, and so are the
extensions of the project you're in. What you ask for by hand has to work out
though. If bougie can't install it, the run stops rather than starting the tool
without it.

## Inside a project

Tools like n98-magerun2 and Deployer boot your application, so they need the
same PHP and extensions your application does. They also don't belong in your
`composer.json`, which is why so many of them ship a `-dist` build.

Run bgx inside a project and it sorts this out for you. It looks for the nearest
`composer.json` or `bougie.toml` above you, takes the PHP version that project
resolved to, and adds the extensions it requires plus the ones bougie infers
from your framework and lock file. So instead of:

```bash
bgx --php 8.3 --with intl --with pdo_mysql --with zip n98/magerun2-dist sys:info
```

you write:

```sh
bgx n98/magerun2-dist sys:info
```

bougie prints one line saying what it picked up and where that came from. Pass
`--no-project` if you'd rather ignore the project entirely.

Your own `--php` always wins. When the project and the tool disagree about which
PHP they need, the tool wins and bougie warns you, because the tool has to be
able to run in the first place.

## Packages with more than one bin

bougie runs the bin named after the package, so `phpstan/phpstan` gives you
`phpstan`. If there's only one bin it runs that. When a package ships several
and none of them matches the package name, bougie stops and lists them. Pick one
with `--bin`, before the package like any other option:

```sh
bgx --bin bricklayer-mcp inchoo/magento-bricklayer
```

## The cache

The first run of a tool writes a complete install into
`$BOUGIE_CACHE/tool-run/<hash>`: a `composer.json`, a lock file, a `vendor/` and
a wrapper. The hash covers the package, the version constraint, the PHP and
anything you added with `--with`, so two projects that need the same tool in
different shapes each get their own copy.

If you already installed the tool with `bougie tool install` and the request
matches it exactly, bgx runs that install instead and writes nothing to the
cache.

Cached tools are dropped 14 days after you last used them, when you run `bougie
cache prune`. Running a tool resets that clock, so the ones you use stay put.

Tools also run with `memory_limit = -1`. bougie sets it through
`PHP_INI_SCAN_DIR`, so any PHP the tool starts itself inherits it.

## When to install instead

Use [`bougie tool install`](/docs/guides/global-tools) for the tools you type by
name every day, like `pint` or `phpstan`. Those go on your `PATH`. Use bgx for
everything else: one offs, CI steps, and the tools you need a few times a year.

bgx is also how you run the Composer commands bougie has no native version of:

```sh
bgx composer/composer create-project magento/project-community-edition
```

## Platform support

bgx runs on Linux and macOS. Windows support is on the way; for now the tool
commands tell you they're Unix only.
