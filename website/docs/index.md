# What is bougie?

Bougie is a PHP toolchain manager like DDEV or Warden.
But unlike those tools it doesn't use Docker, so you get native speed and ease of debugging.
Bougie is designed to just work and be fast, making your development experience truly a luxury.

You use bougie to install the PHP versions you need, set up your dependencies and patches, install and run background services, and your development server. This means bougie replaces (on your dev machine):

- DDEV/Warden/Laravel Herd
- phpenv/phpbrew (PHP version management)
- composer
- nginx + php-fpm
- docker-compose
- ngrok
- nvm

Bougie is the one tool that does it all.

## How it fits together

You still define your project using a `composer.json`, or you can add bougie specific config in your `bougie.toml` if you prefer.
This makes sure that other people also get the right PHP version, extensions, dependencies and services.
Any settings for bougie can be placed in either `composer.json` or `bougie.toml`, it doesn't need to clutter up your project if you don't want to.

Any services you need are easy to start. Services are declared in your configuration files and started with `bougie service up`.
Bougie runs in the background as bougied, working as a service orchestrator like systemd or supervisord.
The services run globally but databases and tenants are automatically provisioned for your project.
Bougie already supports these services:
- MariaDB and MySQL
- Redis
- OpenSearch
- RabbitMQ
- Mailpit

Running composer scripts or PHP scripts is easy with `bougie run`.
Bougie doesn't clobber your PATH, so anything you have set up there will stay working.
That does mean that any command you want to run in the bougie setup, requires a `bougie run` prefix.
If you dislike this way of working, you could try adding shell aliases.
`bougie run` is also used to connect to your database if you need a quick `mariadb` check.

To view your PHP application you use `bougie server` to start up PHP-FPM like a real production setup; except instead of nginx it is running a custom HTTP server.
It always uses the correct PHP version to run your project, and it even supports automatic XDebug enabling.
Your project starts up at a `project.bougie.run` url, and you can choose to share your project which creates a globally routed `bougie.show` url you can share with others.

Remembering which commands to run can be quite a hassle so bougie also ships with a command runner/recipe system.
It works a bit like `make`, and the whole idea is that you just need `bougie start` to run your default recipe.
Now your project is installed, and services are running, allowing you to just get started.

## Next steps

Ready to dive in? Here's where to go next depending on what you need:

- [Installation](/docs/installation)
- [Your first project](/docs/tutorials/first-project)
- [How-to guides](/docs/guides/)
- [Reference](/docs/reference/)
- [Concepts](/docs/concepts/)
