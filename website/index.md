---
layout: home

hero:
  name: bougie
  text: PHP toolchain management, the luxury way.
  tagline: A Composer-compatible package manager, PHP version manager, dev services and web server — one fast binary, no Docker.
  actions:
    - theme: brand
      text: Get started
      link: /docs/
    - theme: alt
      text: View on GitHub
      link: https://github.com/cresset-tools/bougie

features:
  - icon: 📦
    title: Composer, but fast
    details: A native dependency resolver with parallel downloads. Drop-in compatible with composer.json and composer.lock — byte-equivalent autoloader output included.
  - icon: 🐘
    title: Any PHP, any extension
    details: Install and pin PHP versions and native extensions per project. No system PHP juggling, no pecl.
  - icon: 🛠️
    title: Services without Docker
    details: MariaDB, MySQL, Redis, OpenSearch, RabbitMQ, Mailpit — started natively and sandboxed, per project, with one command.
  - icon: 🌐
    title: A real dev server
    details: bougie server gives every project its own local domain on bougie.run, with TLS and FastCGI handled for you.
  - icon: ⚡
    title: Global tools, npx-style
    details: bgx runs PHPStan, Pint, or any Composer CLI tool in an isolated install with its own pinned PHP.
  - icon: 🧑‍🍳
    title: Task runner built in
    details: bougie make runs your project recipes portably — bougie start brings a whole Magento up in one command.
---

## Install in seconds

```sh
curl -LsSf https://bougie.tools/install.sh | sh
```

Then, in your PHP project:

```sh
bougie sync   # PHP + extensions + vendor/, all of it
```

Read the [quickstart](/docs/quickstart) to see what else it can do.
