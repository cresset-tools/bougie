# Migrate from Composer

Bougie mirrors many composer commands under the `bougie composer` command.
For example:

```bash
bougie composer install # Install vendor/
bougie composer update # Bump a dependency
bougie composer require symfony/console # Add a dependency
bougie composer remove symfony/console
bougie composer dump-autoload
```

## The recommended path

You don't have to change anything to get moving. `bougie composer` tracks
Composer closely enough that your existing habits, scripts, and CI commands
keep working with a `bougie` in front of them.

Once you've settled in, there's a shorter set of native commands worth
learning. bougie offers them the way `uv` offers `uv add` next to `uv pip
install`: a tighter surface for the things you do every day.

| In Composer | The native bougie command |
| --- | --- |
| `composer install` | `bougie sync` |
| `composer require symfony/console` | `bougie add symfony/console` |
| `composer require --dev phpunit/phpunit` | `bougie add --dev phpunit/phpunit` |
| `composer remove symfony/console` | `bougie remove symfony/console` |
| `composer show --tree` | `bougie tree` |
| `composer outdated` | `bougie outdated` |

There's one difference to know before you switch. When you add a package,
`bougie add` writes a lower bound (`>=6.4`) where `composer require` would
write a caret (`^6.4`). Both let you move forward inside the major version;
the lower bound just records the minimum you've tested against and leaves the
rest to the resolver and your lock file. There's a
[widely-cited essay on this](https://iscinumpy.dev/post/bound-version-constraints/)
from the Python packaging world: an upper cap is a guess about the future
that tends to age badly, while a lower bound states what you have actually
tested. If you do want a specific constraint, supply it with `@`:

```bash
bougie add "symfony/console@^6.4"
```

To re-lock without pulling in newer releases, use `bougie lock`. It
reconciles `composer.lock` against `composer.json`, holds every package at
its current version where that's still valid, and re-resolves only what
changed.

## The escape hatch

You can always run composer using bougie with the `bougie tool run` or `bgx` command:

```bash
bgx composer/composer create-project
```
