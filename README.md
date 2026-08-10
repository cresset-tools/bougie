# bougie

_PHP toolchain management, the luxury way._

The aim of bougie is to get big PHP projects running with ease.
Originally it was built for Magento development but we want to support other frameworks and projects too.

Using bougie means no worries about:
- Different PHP versions
- PHP native extensions
- Waiting for vendor downloads
- Getting your global PHP tools installed (PHPStan, Pint, etc)
- Getting installs of MariaDB running
- or OpenSearch
- or Redis
- or Rabbitmq
- or an HTTP server

Bougie will set it up for you automatically.


## Installation

We provide an installer script that works on macOS and Linux:

```bash
curl -LsSf https://bougie.tools/install.sh | sh
```

If you prefer, you can also install bougie by using cargo:

```bash
cargo install bougie
```


## Bougie as a composer replacement

Just run `bougie sync`.
Bougie resolves and downloads packages way faster than composer.


## Bougie as a services manager

For native speed, bougie doesn't run services in Docker.
It does try to sandbox the services using native tooling.

On MacOS you don't need Docker installed and you don't need Linux running in the background.


## Bougie as a task runner

Bougie has a builtin task runner that works a bit like `make`.
Not everyone has make installed so this allows for a portable task runner that can depend on other tasks for your PHP projects.

There are some differences from the standard UNIX make.
The default recipe for example is always the `start` recipe.
This is what runs when you run `bougie make`, or when you run `bougie start`.

Bougie comes builtin with a recipe for Magento/Mage-OS.
Try `bougie start` in your Magento project to start everything up and begin developing immediately.

<!-- Link to the docs here on how to write recipes in bougie.toml -->


## Bougie as a tool runner

Bougie installs a `bgx` helper binary that works like `npx`.

```bash
bgx laravel/pint
```

You can also run these tools with a specific PHP version using `--php 8.4`

```bash
bgx --php 8.4 laravel/pint
```

To add this tool to your path, run `bougie tool install`

```bash
bougie tool install --php 8.4 laravel/pint
```

## Bougie and coding agents

Coding agents reach for the machine's `php` and `composer` by reflex, which
in a bougie project is either the wrong interpreter or none at all. Bougie
ships a skill that sets them straight: the environment is bougie's, check
whether the site is already up before running anything that needs it, and
ask before running `bougie start` — it downloads a toolchain and brings up
services shared with every other project on the machine.

```bash
bougie skill install
```

With no flags it asks which agents — leading with any already in use in the
project, and taking `1,3` for more than one — and then where the files go:

| `--agent` | file |
| --- | --- |
| `claude` | `.claude/skills/bougie/SKILL.md` (also `~/.claude/…`) |
| `agents` | `AGENTS.md` — Codex, opencode, Jules, Amp, Zed, … |
| `cursor` | `.cursor/rules/bougie.mdc` |
| `copilot` | `.github/instructions/bougie.instructions.md` |
| `gemini` | `GEMINI.md` (also `~/.gemini/GEMINI.md`) |
| `plain` | bare markdown, wherever `--path` says |

Same prose in each; only the frontmatter that agent keys off differs. Teams
mix editors, so a run can cover several at once:

```bash
bougie skill install --project --agent claude,agents,cursor
```

Add `--project`, `--user`, or `--path <DIR>` to skip the questions, which
is what scripts and CI must do. `bougie skill print --agent <name>` writes
a single document to stdout.

`AGENTS.md` and `GEMINI.md` are yours, not bougie's, so bougie confines
itself to a marked block and leaves the rest of the file alone. A file it
owns outright is never overwritten without asking (or `--force`).

The document ships inside the binary, so `bougie self update` keeps it in
step with the CLI it describes — re-run `bougie skill install` after an
update and it reports `unchanged` if there was nothing to do.


## Telemetry

Bougie can collect anonymous usage statistics and crash reports —
strictly **opt-in**, asked once at install time or on first
interactive run, and never enabled in CI or scripts without an
explicit `BOUGIE_TELEMETRY=on`. It never collects project names,
package names, paths, or IP addresses, and it honors `DO_NOT_TRACK`.
Inspect exactly what would be sent with `bougie telemetry log`;
switch with `bougie telemetry {on,off,local}`. The complete field
list and policy live in [TELEMETRY.md](TELEMETRY.md).

## License

Bougie is freely licensed under the EUPL.
See the LICENSE file for the full license text.

If you need a commercial license, please [get in touch](mailto:jelle@pingiun.com).
