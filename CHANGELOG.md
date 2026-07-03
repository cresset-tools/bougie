# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.39.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.38.1...bougie-v0.39.0) (2026-07-03)


### Features

* **services:** tenant-wired client tools (mysqldump, redis-cli, rabbitmqctl, …) ([#444](https://github.com/cresset-tools/bougie/issues/444)) ([9a7f260](https://github.com/cresset-tools/bougie/commit/9a7f260813a61d0504e5cc340e75847e05df0554))


### Bug Fixes

* **server:** tolerate slow opcache.preload at pool spawn ([#437](https://github.com/cresset-tools/bougie/issues/437)) ([61cc936](https://github.com/cresset-tools/bougie/commit/61cc936fd825a722cc0a528c8be26b4a8475010d))

## [0.38.1](https://github.com/cresset-tools/bougie/compare/bougie-v0.38.0...bougie-v0.38.1) (2026-07-02)


### Bug Fixes

* four tier-1 correctness/safety bugs (extraction, autoloader, cache key, license) ([#432](https://github.com/cresset-tools/bougie/issues/432)) ([9a7c619](https://github.com/cresset-tools/bougie/commit/9a7c619209b1a200f3738e4d9c75c0ab15a8fffa))
* **server:** surface php-fpm startup errors and stop orphaning workers ([#438](https://github.com/cresset-tools/bougie/issues/438)) ([6bd9701](https://github.com/cresset-tools/bougie/commit/6bd9701a991cd3dea301bf97faeaf87c8a43c8ba))
* tier-2 service-supervision correctness (grace window, restart, rotation, sandbox, flock) ([#436](https://github.com/cresset-tools/bougie/issues/436)) ([d1b2abe](https://github.com/cresset-tools/bougie/commit/d1b2abecb43f33cc48ffda01530b69f2c77d4bff))

## [0.38.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.37.0...bougie-v0.38.0) (2026-07-02)


### Features

* **composer:** support repository dist mirrors (Private Packagist) ([#439](https://github.com/cresset-tools/bougie/issues/439)) ([7b0f2ff](https://github.com/cresset-tools/bougie/commit/7b0f2ff4c18562152dbb3144ae6f4e376d8394e3))

## [0.37.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.36.0...bougie-v0.37.0) (2026-07-02)


### Features

* **patches:** apply multi-package top-level patches at the project root ([#430](https://github.com/cresset-tools/bougie/issues/430)) ([33e1124](https://github.com/cresset-tools/bougie/commit/33e1124135afd172f83db4a093ac607d3e899a0c))
* **php-discovery:** only use system PHP for one-off runs by default ([#433](https://github.com/cresset-tools/bougie/issues/433)) ([a934e49](https://github.com/cresset-tools/bougie/commit/a934e49c893cc2281fac7df0ab2c2123df1adfcd))


### Bug Fixes

* **release:** auto-retry crates-publish past crates.io index lag ([#435](https://github.com/cresset-tools/bougie/issues/435)) ([be788bc](https://github.com/cresset-tools/bougie/commit/be788bc5059e146f639e96ca387cb25b535aed02)), closes [#424](https://github.com/cresset-tools/bougie/issues/424)

## [0.36.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.35.1...bougie-v0.36.0) (2026-06-28)


### Features

* **composer-resolver:** honor --ignore-platform-req(s) at resolve time ([#427](https://github.com/cresset-tools/bougie/issues/427)) ([c304b13](https://github.com/cresset-tools/bougie/commit/c304b13746abd69ea7796266f369f14617d02f2d))

## [0.35.1](https://github.com/cresset-tools/bougie/compare/bougie-v0.35.0...bougie-v0.35.1) (2026-06-28)


### Bug Fixes

* **composer-resolver:** stop reporting satisfied `php` as missing from repos ([#425](https://github.com/cresset-tools/bougie/issues/425)) ([3efac2c](https://github.com/cresset-tools/bougie/commit/3efac2ce46869a0c27f3d2d586531af628661277))

## [0.35.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.34.0...bougie-v0.35.0) (2026-06-28)


### Features

* **patches:** add `patches create` to capture vendor edits as clean patches ([#417](https://github.com/cresset-tools/bougie/issues/417)) ([d087da6](https://github.com/cresset-tools/bougie/commit/d087da611ee725096d1998a98907837aeb57b6a5))


### Bug Fixes

* **sync:** sync the discovered project root, not the cwd ([#418](https://github.com/cresset-tools/bougie/issues/418)) ([faab201](https://github.com/cresset-tools/bougie/commit/faab201286b23df6e812c8630a7fbf9ecc6770af))

## [0.34.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.33.0...bougie-v0.34.0) (2026-06-23)


### Features

* **services:** show service binding in `services status` text output ([#412](https://github.com/cresset-tools/bougie/issues/412)) ([d68ddec](https://github.com/cresset-tools/bougie/commit/d68ddecc6ded532733d0ba3d75ca5810dbe293c1))
* **services:** warn on `up` when a service's TCP port is already in use ([#413](https://github.com/cresset-tools/bougie/issues/413)) ([1c520f6](https://github.com/cresset-tools/bougie/commit/1c520f6161aba99f4afdff792ef67a27fa7c7ebc))

## [0.33.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.32.2...bougie-v0.33.0) (2026-06-21)


### Features

* **recipe:** add localdev task to disable 2FA and set indexers realtime ([#409](https://github.com/cresset-tools/bougie/issues/409)) ([796e187](https://github.com/cresset-tools/bougie/commit/796e18765123ee04f6c5cf2f73768313ae7b28dd))
* **services:** integrate mailpit SMTP test server ([#408](https://github.com/cresset-tools/bougie/issues/408)) ([54f5682](https://github.com/cresset-tools/bougie/commit/54f5682bdb68a52d2c03f2ec0864647d164d2be6))
* **services:** warn on `up` when env.php DB user != provisioned tenant ([#411](https://github.com/cresset-tools/bougie/issues/411)) ([85f3c6c](https://github.com/cresset-tools/bougie/commit/85f3c6c7f28102d0e0c7e8e3a1f017eb4196f441))


### Bug Fixes

* **services:** persist tenant name so it can't drift across down/purge ([#407](https://github.com/cresset-tools/bougie/issues/407)) ([046521c](https://github.com/cresset-tools/bougie/commit/046521c2e6e276f42a5246ff1b916eb04b1f5667))

## [0.32.2](https://github.com/cresset-tools/bougie/compare/bougie-v0.32.1...bougie-v0.32.2) (2026-06-20)


### Bug Fixes

* **composer-resolver:** accept `{"packagist": false}` BC alias to disable Packagist ([#402](https://github.com/cresset-tools/bougie/issues/402)) ([5940298](https://github.com/cresset-tools/bougie/commit/5940298c5eacba3568a141a6371cef572f365882))
* **composer-resolver:** key repo auth by origin incl. port ([#404](https://github.com/cresset-tools/bougie/issues/404)) ([01e8e9b](https://github.com/cresset-tools/bougie/commit/01e8e9b46710cd248131ccc12dc0c508b98634cf))
* **daemon:** run service health probe off the Supervisor mutex ([#405](https://github.com/cresset-tools/bougie/issues/405)) ([9db5d61](https://github.com/cresset-tools/bougie/commit/9db5d614d5e816c587fb4f26eb7076ced696c730)), closes [#219](https://github.com/cresset-tools/bougie/issues/219)

## [0.32.1](https://github.com/cresset-tools/bougie/compare/bougie-v0.32.0...bougie-v0.32.1) (2026-06-19)


### Bug Fixes

* **tree:** dedupe shared subtrees so `bougie tree` can't hang ([#399](https://github.com/cresset-tools/bougie/issues/399)) ([d2a2bff](https://github.com/cresset-tools/bougie/commit/d2a2bff2091e52bfc1437eefe3f10241d628fb7e))

## [0.32.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.31.1...bougie-v0.32.0) (2026-06-19)


### Features

* **cli:** luxury-themed help headline ([#396](https://github.com/cresset-tools/bougie/issues/396)) ([13af36c](https://github.com/cresset-tools/bougie/commit/13af36cc6f4502f7dfab0e8c5e29f7f9c6124b7e))

## [0.31.1](https://github.com/cresset-tools/bougie/compare/bougie-v0.31.0...bougie-v0.31.1) (2026-06-18)


### Bug Fixes

* **php:** auto-sync on `php pin` and reconcile conf.d for system PHP ([#391](https://github.com/cresset-tools/bougie/issues/391)) ([ef2681b](https://github.com/cresset-tools/bougie/commit/ef2681b12dd8e949915181b1ec9a2ee37835302d))

## [0.31.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.30.0...bougie-v0.31.0) (2026-06-18)


### Features

* **starter:** prompt for private-repo auth secrets (e.g. Hyvä license key) ([#388](https://github.com/cresset-tools/bougie/issues/388)) ([bcb17d4](https://github.com/cresset-tools/bougie/commit/bcb17d48e1e482bf005d8a88f1481937aebde212))

## [0.30.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.29.0...bougie-v0.30.0) (2026-06-17)


### Features

* **composer-resolver:** support Composer `type: path` repositories ([#382](https://github.com/cresset-tools/bougie/issues/382)) ([c4fbb7f](https://github.com/cresset-tools/bougie/commit/c4fbb7fa63c19b4e4cdd19140ab55deaa0a2fbae))
* **init:** scaffold `--starter laravel` via the laravel installer ([#383](https://github.com/cresset-tools/bougie/issues/383)) ([7fa7c08](https://github.com/cresset-tools/bougie/commit/7fa7c083ac92f867e16879547351ee46eab027df))
* **patches:** native cweagans/composer-patches reimplementation ([#384](https://github.com/cresset-tools/bougie/issues/384)) ([85a3ec9](https://github.com/cresset-tools/bougie/commit/85a3ec995c7360bfc9d6116bab0ba56e94ecce88))
* **starter:** prompt for per-user placeholder tokens ([#385](https://github.com/cresset-tools/bougie/issues/385)) ([e236910](https://github.com/cresset-tools/bougie/commit/e23691063e54620a0cbb01014f2c6108906e6591))
* top-level `bougie projects` + uv-style `--resolution` ([#381](https://github.com/cresset-tools/bougie/issues/381)) ([09b0d5c](https://github.com/cresset-tools/bougie/commit/09b0d5cb08d0b7da37f2510584cf1e1cb964382b))


### Bug Fixes

* **init:** pass ResolutionStrategy to resolve_and_write_lock ([#387](https://github.com/cresset-tools/bougie/issues/387)) ([1d60930](https://github.com/cresset-tools/bougie/commit/1d60930d7f990bbc279f08295085e930e5729802))

## [0.29.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.28.0...bougie-v0.29.0) (2026-06-13)


### ⚠ BREAKING CHANGES

* **cli:** `bougie make` with no task argument now lists the available recipe tasks instead of running the `start` task. Use `bougie start` (or `bougie make start`) to bring the project up.

### Features

* **cli:** reorganize the CLI around start/stop, group --help ([#378](https://github.com/cresset-tools/bougie/issues/378)) ([a0c629b](https://github.com/cresset-tools/bougie/commit/a0c629bae9e68a801a5e8a57f607d6108374e40b))
* **tool:** forward bgx/tool-run args after the package without `--` ([#376](https://github.com/cresset-tools/bougie/issues/376)) ([41a99b4](https://github.com/cresset-tools/bougie/commit/41a99b46af45e5a2768e5e4d521ec99e20cf2331))


### Bug Fixes

* **autoloader:** emit root package autoload-dev when dev deps are included ([#380](https://github.com/cresset-tools/bougie/issues/380)) ([07eb2a1](https://github.com/cresset-tools/bougie/commit/07eb2a1db9cb3436f1455a482f75860cc6f460db))
* **sync:** re-resolve stale extensions after the active interpreter changes ([#379](https://github.com/cresset-tools/bougie/issues/379)) ([4b24785](https://github.com/cresset-tools/bougie/commit/4b24785311d2517e7a673e79d7e0ed6ac529838b))

## [0.28.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.27.0...bougie-v0.28.0) (2026-06-13)


### Features

* **node:** Node.js toolchain via nodejs.org + run PATH overlay ([#371](https://github.com/cresset-tools/bougie/issues/371)) ([72b0e4d](https://github.com/cresset-tools/bougie/commit/72b0e4d846568c208f32f47b1962e7a5e4638e4e))
* **paths:** move project toolchain into vendor/bougie; durable state under $BOUGIE_HOME ([#372](https://github.com/cresset-tools/bougie/issues/372)) ([2c92332](https://github.com/cresset-tools/bougie/commit/2c923323be730b8d9d7217e158917570dea04234))


### Bug Fixes

* **dist:** make installers prefer the origin mirror (hosting=[simple,github]) ([#374](https://github.com/cresset-tools/bougie/issues/374)) ([e627774](https://github.com/cresset-tools/bougie/commit/e6277743e7dd82b7996d975f74d5a30b5ee8a2a3))
* **format:** pin wick 0.2.3 ([#375](https://github.com/cresset-tools/bougie/issues/375)) ([55c73dc](https://github.com/cresset-tools/bougie/commit/55c73dc4a7306f3d03a226c28f6479fe94478a08))

## [0.27.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.26.0...bougie-v0.27.0) (2026-06-13)


### Features

* **format:** add `bougie format`, the `uv format` model for PHP ([#368](https://github.com/cresset-tools/bougie/issues/368)) ([2b0e567](https://github.com/cresset-tools/bougie/commit/2b0e567c4da6c6e20f4b07308bc902d2b3a1eca3))
* **run:** add `--php` to select the interpreter for one run ([#366](https://github.com/cresset-tools/bougie/issues/366)) ([bf47d59](https://github.com/cresset-tools/bougie/commit/bf47d59b47388efc1c35baedbfce545c0965f0d8))


### Bug Fixes

* **format:** pin wick 0.2.1 (0.2.0 shipped no binaries) ([#370](https://github.com/cresset-tools/bougie/issues/370)) ([f8808c5](https://github.com/cresset-tools/bougie/commit/f8808c525a1b31b0ebe2ee316d7339729b9bafea))

## [0.26.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.25.2...bougie-v0.26.0) (2026-06-13)


### Features

* **cli:** promote `services projects` to top-level `bougie projects` ([#365](https://github.com/cresset-tools/bougie/issues/365)) ([da52e1d](https://github.com/cresset-tools/bougie/commit/da52e1df7d61b178a67e53413e94024db6fa6af9))
* **server:** run the dev server against a system PHP ([#363](https://github.com/cresset-tools/bougie/issues/363)) ([9890626](https://github.com/cresset-tools/bougie/commit/98906264bdfccd27f59753ea935aa56d408e8dac))

## [0.25.2](https://github.com/cresset-tools/bougie/compare/bougie-v0.25.1...bougie-v0.25.2) (2026-06-12)


### Bug Fixes

* **daemon:** allow /tmp + /var/tmp RW so macOS bash heredocs work in sandbox ([#361](https://github.com/cresset-tools/bougie/issues/361)) ([fc8f421](https://github.com/cresset-tools/bougie/commit/fc8f421cd08377d46499e92bbb6f554daa5ce1e6))

## [0.25.1](https://github.com/cresset-tools/bougie/compare/bougie-v0.25.0...bougie-v0.25.1) (2026-06-12)


### Bug Fixes

* **daemon:** set TMPDIR for opensearch so bash heredocs work under macOS sandbox ([#358](https://github.com/cresset-tools/bougie/issues/358)) ([40664e5](https://github.com/cresset-tools/bougie/commit/40664e54881d9048c6bcd1579b38d47f7e38fd75))
* **dist:** build linux-gnu at glibc 2.17 via custom in-container job ([#357](https://github.com/cresset-tools/bougie/issues/357)) ([6188e00](https://github.com/cresset-tools/bougie/commit/6188e00c4f540510d96d5b459600b63d2abbe63b))
* **self-update:** fall back to newest release with assets for the target ([#356](https://github.com/cresset-tools/bougie/issues/356)) ([47a1df2](https://github.com/cresset-tools/bougie/commit/47a1df2fc07da138605d8c7d40a987b25c40efec))

## [0.25.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.24.0...bougie-v0.25.0) (2026-06-12)


### ⚠ BREAKING CHANGES

* **composer:** bougie no longer bundles or runs the Composer phar. The `composer` argv[0] shim now dispatches to bougie's native subcommands, so `composer install` (recipes, PATH) runs the native installer. Unrecognized subcommands (create-project, archive, bump, …) error with a pointer to `bougie tool install composer/composer` — the deliberate escape hatch. Removed: bougie-composer fetch/request/resolve modules + install_composer; paths.composer_phar; the resolved-composer state file; the `[composer] version` config field; sync's phar fetch and SyncResult.composer_{version,path}.

### Features

* **cli:** add `bougie lock` — minimal lockfile refresh ([#351](https://github.com/cresset-tools/bougie/issues/351)) ([ca6056a](https://github.com/cresset-tools/bougie/commit/ca6056a18884b4e6b87e17b71b00dfb76c26f16f))
* **cli:** native uv-style verbs — add, remove, tree, outdated ([#350](https://github.com/cresset-tools/bougie/issues/350)) ([b74126f](https://github.com/cresset-tools/bougie/commit/b74126fb27b960ef8b92b57d54a7ae87501f9c4c))
* **composer:** native uv-pip Composer surface; drop the phar ([#348](https://github.com/cresset-tools/bougie/issues/348)) ([0d491f6](https://github.com/cresset-tools/bougie/commit/0d491f60853c3b0b1d32ec95afe51ecc4d6214c9))
* **dist:** build linux-gnu against glibc 2.17 in manylinux2014 ([#355](https://github.com/cresset-tools/bougie/issues/355)) ([4ce19eb](https://github.com/cresset-tools/bougie/commit/4ce19eb809a60c8e1c2c5bb6484d02b841fad933))
* **php:** system PHP support (uv's system-Python model) ([#354](https://github.com/cresset-tools/bougie/issues/354)) ([32eef2e](https://github.com/cresset-tools/bougie/commit/32eef2e612b3e6a3c8ee3c62d2ec17e626fa9b8e))


### Bug Fixes

* **composer:** `update` installs vendor/ + `upgrade`/`u` aliases ([#352](https://github.com/cresset-tools/bougie/issues/352)) ([a4ac00e](https://github.com/cresset-tools/bougie/commit/a4ac00e43f8cf4d1e62caa3f9002431d16d23351))

## [0.24.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.23.1...bougie-v0.24.0) (2026-06-10)


### Features

* **sync:** uv-style concise summary + skip redundant autoloader dump ([#347](https://github.com/cresset-tools/bougie/issues/347)) ([a845daa](https://github.com/cresset-tools/bougie/commit/a845daaf211dc194a13d65447c771e94df5e865a))


### Performance Improvements

* **installer:** fetch the index root once per sync, not per extension ([#345](https://github.com/cresset-tools/bougie/issues/345)) ([68c6b7b](https://github.com/cresset-tools/bougie/commit/68c6b7bf8df2fcedc72df4ec8ccc017a9de84e71))

## [0.23.1](https://github.com/cresset-tools/bougie/compare/bougie-v0.23.0...bougie-v0.23.1) (2026-06-08)


### Bug Fixes

* **server:** respawn dead FPM pools instead of dispatching to a vanished socket ([#340](https://github.com/cresset-tools/bougie/issues/340)) ([24878fb](https://github.com/cresset-tools/bougie/commit/24878fb4e4cf895a8f3914cad6cb0bc3ffe75ece))

## [0.23.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.22.0...bougie-v0.23.0) (2026-06-07)


### Features

* **autoloader:** emit platform_check.php (Composer config.platform-check) ([#337](https://github.com/cresset-tools/bougie/issues/337)) ([d9bbb12](https://github.com/cresset-tools/bougie/commit/d9bbb1269b530efb4a5d10a5b04b876c6a0812c8))


### Bug Fixes

* **daemon:** setsid bougied so a terminal Ctrl-C can't kill it ([#339](https://github.com/cresset-tools/bougie/issues/339)) ([6021991](https://github.com/cresset-tools/bougie/commit/6021991736fa4818eddf8c741f8e0d25cccf75ea))

## [0.22.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.21.0...bougie-v0.22.0) (2026-06-07)


### Features

* **services:** stream stopping/starting progress on restart ([#335](https://github.com/cresset-tools/bougie/issues/335)) ([9bebd13](https://github.com/cresset-tools/bougie/commit/9bebd136a6e797da374e403b687749d807c281cd))
* **services:** write PhpStorm data source on `bougie up` ([#336](https://github.com/cresset-tools/bougie/issues/336)) ([126fec6](https://github.com/cresset-tools/bougie/commit/126fec67856d0e8e4ad39733e2d58d7e14a83642))


### Bug Fixes

* **autoloader:** exclude PSR-fallback volatile roots from the classmap ([#332](https://github.com/cresset-tools/bougie/issues/332)) ([346463b](https://github.com/cresset-tools/bougie/commit/346463b33009dc5c29b520f82a499eee934ad79b))
* **installer:** don't flash a progress bar when baseline extensions are all installed ([#334](https://github.com/cresset-tools/bougie/issues/334)) ([630a568](https://github.com/cresset-tools/bougie/commit/630a568f47c81aa44e99f9bb3f8b0b77a5209650))
* **self-update:** don't update to a release with no assets yet ([#331](https://github.com/cresset-tools/bougie/issues/331)) ([9664f49](https://github.com/cresset-tools/bougie/commit/9664f49a5b4960207d6f818536d79762a0026053))

## [0.21.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.20.0...bougie-v0.21.0) (2026-06-07)


### Features

* **composer:** distinguish -w and -W in partial update ([#330](https://github.com/cresset-tools/bougie/issues/330)) ([515a562](https://github.com/cresset-tools/bougie/commit/515a56278563f4a73111a57e6ac543b910920bcc))
* **composer:** partial update (composer update &lt;packages&gt;) ([#328](https://github.com/cresset-tools/bougie/issues/328)) ([bcf4ecd](https://github.com/cresset-tools/bougie/commit/bcf4ecde2d844ef0f25e2632315c4eee7d2b91c3))

## [0.20.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.19.0...bougie-v0.20.0) (2026-06-07)


### Features

* **starter:** make the manifest recipe load-bearing (+ detect modulargento) ([#326](https://github.com/cresset-tools/bougie/issues/326)) ([28453bc](https://github.com/cresset-tools/bougie/commit/28453bcd338703f19dcfb07541fa222d5a0865a3))

## [0.19.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.18.1...bougie-v0.19.0) (2026-06-06)


### Features

* **scripts:** opt-in root composer.json script execution ([#324](https://github.com/cresset-tools/bougie/issues/324)) ([b7637f3](https://github.com/cresset-tools/bougie/commit/b7637f31f8858f43d676d6957b6cf208b8f082a1))

## [0.18.1](https://github.com/cresset-tools/bougie/compare/bougie-v0.18.0...bougie-v0.18.1) (2026-06-05)


### Bug Fixes

* **resolver:** validate php platform requires against the pinned PHP ([#322](https://github.com/cresset-tools/bougie/issues/322)) ([007d1db](https://github.com/cresset-tools/bougie/commit/007d1dbab36fcdbf8863b837833a0a47c68a41fc)), closes [#118](https://github.com/cresset-tools/bougie/issues/118)

## [0.18.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.17.0...bougie-v0.18.0) (2026-06-05)


### Features

* **server:** redesign `bougie server` as a project verb over the shared daemon ([#318](https://github.com/cresset-tools/bougie/issues/318)) ([131a4d5](https://github.com/cresset-tools/bougie/commit/131a4d512a6bb65ba12478438fe542aa756eeaf2))
* **services:** add `bougie services projects` (list provisioned tenants) + `purge` ([#320](https://github.com/cresset-tools/bougie/issues/320)) ([c66de18](https://github.com/cresset-tools/bougie/commit/c66de18e9f02e8ba212821b14046d8078a57f04c))


### Bug Fixes

* **autoloader:** widen PackageSorter weight to i64 to match Composer ([#319](https://github.com/cresset-tools/bougie/issues/319)) ([75fcb2f](https://github.com/cresset-tools/bougie/commit/75fcb2f375aa2934bf0ac4aebd4584620bc415c2))
* **cli:** make `bgx --version` work ([#311](https://github.com/cresset-tools/bougie/issues/311)) ([747a54d](https://github.com/cresset-tools/bougie/commit/747a54deb62860b6eb2dfba12b0977cd2f7724b8))
* **composer-resolver:** don't let a replaced original's back-edge break the solve ([#317](https://github.com/cresset-tools/bougie/issues/317)) ([abade97](https://github.com/cresset-tools/bougie/commit/abade97fa671249d7be347dc52309c9728871962))
* **sandbox:** enforce ProtectHome/inaccessible/read-only paths on Linux ([#208](https://github.com/cresset-tools/bougie/issues/208)) ([#316](https://github.com/cresset-tools/bougie/issues/316)) ([ff885a5](https://github.com/cresset-tools/bougie/commit/ff885a5aee6d2d773f06140e210d110ad404b30d))
* **services:** derive tenant from project dir, not composer name, to stop collisions ([#321](https://github.com/cresset-tools/bougie/issues/321)) ([32959c3](https://github.com/cresset-tools/bougie/commit/32959c3e2ca53e87e2a21ff747b79ec303080ce8))

## [0.17.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.16.0...bougie-v0.17.0) (2026-06-03)


### Features

* **docker:** publish container images via cargo-zigbuild ([#305](https://github.com/cresset-tools/bougie/issues/305)) ([3ea8f05](https://github.com/cresset-tools/bougie/commit/3ea8f05c6c2241aa93181561cfe9ef882b652a40))

## [0.16.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.15.0...bougie-v0.16.0) (2026-06-03)


### Features

* **services:** attach to combined log stream on `bougie up` ([#300](https://github.com/cresset-tools/bougie/issues/300)) ([8b90051](https://github.com/cresset-tools/bougie/commit/8b90051146a05ff099c8c84dcaa91c6229dd2723))


### Bug Fixes

* **composer:** warn instead of erroring on stale composer.lock ([#304](https://github.com/cresset-tools/bougie/issues/304)) ([0a9bfcf](https://github.com/cresset-tools/bougie/commit/0a9bfcff4a1efa72a5111001d18d8f81c43fbf87))
* **fetch:** build the step bar with its draw target to stop a stranded frame ([#303](https://github.com/cresset-tools/bougie/issues/303)) ([bc97eb8](https://github.com/cresset-tools/bougie/commit/bc97eb8e9de5ec818e4cc13e93927fec35330ed0))
* **recipe:** pin bougie on PATH for check scripts ([#301](https://github.com/cresset-tools/bougie/issues/301)) ([addbd4e](https://github.com/cresset-tools/bougie/commit/addbd4e75c2ad17c9a2b4a62207b3404d2bad3bd))

## [0.15.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.14.0...bougie-v0.15.0) (2026-06-03)


### Features

* **daemon:** graceful shutdown + opensearch jdk runtime dep ([#297](https://github.com/cresset-tools/bougie/issues/297)) ([a21a051](https://github.com/cresset-tools/bougie/commit/a21a051384a8a63d8291ae3e951bfd27cfb03cb7))
* **installer:** count progress for baseline extension install ([#296](https://github.com/cresset-tools/bougie/issues/296)) ([045fee6](https://github.com/cresset-tools/bougie/commit/045fee64207b98ed5bc8d441e3b892c6adfa6d42))
* SIGQUIT activity dump + shared resolver metadata cache ([#295](https://github.com/cresset-tools/bougie/issues/295)) ([fc7c3cb](https://github.com/cresset-tools/bougie/commit/fc7c3cb211b935afc0fb57f790d839f4cd4a51ae))


### Bug Fixes

* don't orphan rabbitmq when bougied gets a foreground Ctrl-C ([#299](https://github.com/cresset-tools/bougie/issues/299)) ([385f4e5](https://github.com/cresset-tools/bougie/commit/385f4e5db63dce9afdb8c1adb9a35dd9c180d5bf))

## [0.14.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.13.0...bougie-v0.14.0) (2026-06-02)


### Features

* **server:** default web (php-fpm) memory_limit to 1G ([#294](https://github.com/cresset-tools/bougie/issues/294)) ([cf61811](https://github.com/cresset-tools/bougie/commit/cf61811981ee3c10e68f4ec98e529837c4ba37ca))
* **shim:** default CLI php to memory_limit=-1 (FPM unchanged) ([#292](https://github.com/cresset-tools/bougie/issues/292)) ([94a04b5](https://github.com/cresset-tools/bougie/commit/94a04b55183a16e52e03c970ece95cebe822f69b))


### Bug Fixes

* **babysit:** don't tear down a healthy service when the sidecar exits benignly ([#291](https://github.com/cresset-tools/bougie/issues/291)) ([cd5bbf9](https://github.com/cresset-tools/bougie/commit/cd5bbf9d4ff80878d08f7b86043ecf2857da0d63))

## [0.13.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.12.0...bougie-v0.13.0) (2026-06-02)


### Features

* **babysit:** co-locate a service's helper daemon via --sidecar (epmd, macOS-correct) ([#285](https://github.com/cresset-tools/bougie/issues/285)) ([30486ad](https://github.com/cresset-tools/bougie/commit/30486ad063e0bdc6e80109a3fe0b5a8f271ca7b0))


### Bug Fixes

* **daemon:** anchor bougied cwd so provisioner probes survive a deleted launch dir ([#289](https://github.com/cresset-tools/bougie/issues/289)) ([a30a83c](https://github.com/cresset-tools/bougie/commit/a30a83c318cf6a0e6dc2ef560f50897abea699c5))
* **daemon:** derive mariadb passwords so they survive down/purge/re-provision ([#287](https://github.com/cresset-tools/bougie/issues/287)) ([4eee91f](https://github.com/cresset-tools/bougie/commit/4eee91fec043795eb58121f479ee9991c50b002d))
* **daemon:** derive rabbitmq passwords too (stable across re-provision) ([#290](https://github.com/cresset-tools/bougie/issues/290)) ([98d1025](https://github.com/cresset-tools/bougie/commit/98d10250c82994b8d9a7d61caf86b9aa359f12a8))
* **server:** keep generated/ classmap entries fresh instead of dangling ([#288](https://github.com/cresset-tools/bougie/issues/288)) ([dcbc3be](https://github.com/cresset-tools/bougie/commit/dcbc3be5cd9aa69316a07eb4b81ad3958365a188))

## [0.12.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.11.0...bougie-v0.12.0) (2026-06-02)


### ⚠ BREAKING CHANGES

* **composer:** `bougie composer {fetch,uninstall,list,find,pin,dir,upgrade}` are removed. Pin the composer version via bougie.toml instead.

### Features

* **composer:** trim surface to native ops; make Composer a default project-aware tool ([#277](https://github.com/cresset-tools/bougie/issues/277)) ([970c751](https://github.com/cresset-tools/bougie/commit/970c7512956d94ffd51aacd988df10e2ae5406e6))
* **daemon:** cgroup-v2 supervision backend — reap daemonized escapees (e.g. epmd) ([#283](https://github.com/cresset-tools/bougie/issues/283)) ([535e198](https://github.com/cresset-tools/bougie/commit/535e198a7e708a39a1db207963e3ae3c313fc226))
* **init:** add --name flag and a new &lt;directory&gt; command ([#284](https://github.com/cresset-tools/bougie/issues/284)) ([e184eb9](https://github.com/cresset-tools/bougie/commit/e184eb9360ee0a580226ae53beaf1d36b6a21861))
* **self-update:** only update a binary bougie's installer placed ([#279](https://github.com/cresset-tools/bougie/issues/279)) ([f929f26](https://github.com/cresset-tools/bougie/commit/f929f2676d2fcd6cabf13e45ef57baa0bb490cbe))


### Bug Fixes

* **babysit:** SIGKILL the service via PR_SET_PDEATHSIG if the babysit dies abnormally ([#282](https://github.com/cresset-tools/bougie/issues/282)) ([df48680](https://github.com/cresset-tools/bougie/commit/df486804303a1ae8e852ae56aa76852e020ead75))
* **backend:** clearer error for an unsupported host target (musl/Alpine) ([#274](https://github.com/cresset-tools/bougie/issues/274)) ([9c789cb](https://github.com/cresset-tools/bougie/commit/9c789cb66ce42bb513dd91a26356100f87e3db46))
* **release:** bump-minor-pre-major so pre-1.0 breaking changes stay pre-major ([#280](https://github.com/cresset-tools/bougie/issues/280)) ([fa36828](https://github.com/cresset-tools/bougie/commit/fa36828b127a1d6c7be418841cd152a25797cd6a))
* **server:** serve on-disk static assets before the front-controller rewrite ([#281](https://github.com/cresset-tools/bougie/issues/281)) ([43e4cd5](https://github.com/cresset-tools/bougie/commit/43e4cd585977003d3250a4008b5410e30572db8a))

## [0.11.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.10.1...bougie-v0.11.0) (2026-06-01)


### Features

* **daemon:** forward the extracting phase to the CLI's mirrored download bar ([#272](https://github.com/cresset-tools/bougie/issues/272)) ([ecbb147](https://github.com/cresset-tools/bougie/commit/ecbb147c076dc6435da6e1f573a356d947f9756e))

## [0.10.1](https://github.com/cresset-tools/bougie/compare/bougie-v0.10.0...bougie-v0.10.1) (2026-06-01)


### Bug Fixes

* **daemon,recipe:** restore services on daemon restart; pin recipe bougie to current exe ([#267](https://github.com/cresset-tools/bougie/issues/267)) ([0988460](https://github.com/cresset-tools/bougie/commit/098846081dcb76b8c59b90b963e14a41df3b6d69))
* **daemon:** plan the whole tool tree up front so the download bar total is accurate ([#271](https://github.com/cresset-tools/bougie/issues/271)) ([819c1bc](https://github.com/cresset-tools/bougie/commit/819c1bcca456fed1b01d03d992b6e7f5004ad9e4))
* **fetch:** add stall timeout, retries with backoff, and extraction progress ([#270](https://github.com/cresset-tools/bougie/issues/270)) ([8245965](https://github.com/cresset-tools/bougie/commit/824596539b43551d0d3659a2d503af75b623c442))
* **resolver:** honor root composer.json wildcard `replace` ([#269](https://github.com/cresset-tools/bougie/issues/269)) ([e14720d](https://github.com/cresset-tools/bougie/commit/e14720d6fa17113b1849d600209d5652c8f900f3))

## [0.10.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.9.0...bougie-v0.10.0) (2026-05-31)


### Features

* **init:** treat --starter as a base URL, append /starter.json ([#265](https://github.com/cresset-tools/bougie/issues/265)) ([6a7b958](https://github.com/cresset-tools/bougie/commit/6a7b958e3686c38d136d2fc9a2c6ea80e5f6d005))

## [0.9.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.8.3...bougie-v0.9.0) (2026-05-31)


### Features

* **init:** bougie init --starter &lt;url|alias&gt; + --start ([#263](https://github.com/cresset-tools/bougie/issues/263)) ([bfb5bcd](https://github.com/cresset-tools/bougie/commit/bfb5bcdce03ca77f461d14a5afd2c636404fb94f))

## [0.8.3](https://github.com/cresset-tools/bougie/compare/bougie-v0.8.2...bougie-v0.8.3) (2026-05-31)


### Bug Fixes

* **release:** allow-dirty = ["ci"] so dist accepts the hand-edited trigger ([#261](https://github.com/cresset-tools/bougie/issues/261)) ([88be819](https://github.com/cresset-tools/bougie/commit/88be81911d47b4ef3a7f86b75a0ca08264ec1850))

## [0.8.2](https://github.com/cresset-tools/bougie/compare/bougie-v0.8.1...bougie-v0.8.2) (2026-05-31)


### Bug Fixes

* **release:** let release-please own the whole release; dist only uploads ([#260](https://github.com/cresset-tools/bougie/issues/260)) ([d404a11](https://github.com/cresset-tools/bougie/commit/d404a116e113e574a7d137f0e171d8057d6665b0))
* **release:** suppress the candidate PR on release-merge runs ([#258](https://github.com/cresset-tools/bougie/issues/258)) ([ed025ce](https://github.com/cresset-tools/bougie/commit/ed025ce8be971a25fba71b361f8f03af5d3fe8d9))

## [0.8.1](https://github.com/cresset-tools/bougie/compare/bougie-v0.8.0...bougie-v0.8.1) (2026-05-31)


### Bug Fixes

* **release:** move release-tag push into its own isolated job ([#253](https://github.com/cresset-tools/bougie/issues/253)) ([1570fc1](https://github.com/cresset-tools/bougie/commit/1570fc1e8d041cf82f305ee2818ff177371b08c1))

## [0.8.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.7.0...bougie-v0.8.0) (2026-05-30)


### Features

* **installers:** native Composer install-plugin support (Magento, composer/installers, Laravel) ([#248](https://github.com/cresset-tools/bougie/issues/248)) ([ebdf9c3](https://github.com/cresset-tools/bougie/commit/ebdf9c31be080a26ce00196c5b4ceefb27b5599e))


### Bug Fixes

* **recipe:** Mage-OS one-command bring-up — detect mage-os, redis-over-socket, lock re-stamp ([#251](https://github.com/cresset-tools/bougie/issues/251)) ([4d29004](https://github.com/cresset-tools/bougie/commit/4d2900418697defb4bc17ecfcac98c498b31b784))
* **release:** push the release tag (draft Releases don't auto-tag) ([#249](https://github.com/cresset-tools/bougie/issues/249)) ([469ee13](https://github.com/cresset-tools/bougie/commit/469ee1373c5b22b3b35e5336dc907b14138a57a9))

## [0.7.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.6.4...bougie-v0.7.0) (2026-05-30)


### Features

* **cli:** uv-style --version with git sha, date, and target triple ([#243](https://github.com/cresset-tools/bougie/issues/243)) ([06293c3](https://github.com/cresset-tools/bougie/commit/06293c31da20d3b332a11de1687f7355eb771ed9))
* **self:** implement bougie self update ([#244](https://github.com/cresset-tools/bougie/issues/244)) ([3f0f200](https://github.com/cresset-tools/bougie/commit/3f0f200b5faee9a801f71e3183eb7148239cc889))
* **sync:** one-command install — create lock + vendor, learn PHP/exts from the lock ([#241](https://github.com/cresset-tools/bougie/issues/241)) ([7bd6a21](https://github.com/cresset-tools/bougie/commit/7bd6a21781fe26e422f709a87cc5bafe71458306))


### Bug Fixes

* **release:** jq key-access syntax for release-please-manifest ([#242](https://github.com/cresset-tools/bougie/issues/242)) ([7c0a5f4](https://github.com/cresset-tools/bougie/commit/7c0a5f408980e3cbc3962ff8208476c393c6863e))
* **release:** let release-please own the draft GitHub Release ([#245](https://github.com/cresset-tools/bougie/issues/245)) ([6b8ce18](https://github.com/cresset-tools/bougie/commit/6b8ce18395186d66963f96a2bb7e3056d2a9b0fe))

## [0.6.4](https://github.com/cresset-tools/bougie/compare/bougie-v0.6.3...bougie-v0.6.4) (2026-05-30)


### Bug Fixes

* **release:** let dist own the GitHub Release; release-please pushes tag only ([#238](https://github.com/cresset-tools/bougie/issues/238)) ([55ef8e5](https://github.com/cresset-tools/bougie/commit/55ef8e5d30a1d7e4bd2c5e79051a101c9973e135))
* resolve whole-project review findings ([#207](https://github.com/cresset-tools/bougie/issues/207)–[#231](https://github.com/cresset-tools/bougie/issues/231)) ([#234](https://github.com/cresset-tools/bougie/issues/234)) ([4f873e9](https://github.com/cresset-tools/bougie/commit/4f873e95dd96e62f4423b8cd0fe0f1a369038aab))

## [0.6.3](https://github.com/cresset-tools/bougie/compare/bougie-v0.6.2...bougie-v0.6.3) (2026-05-30)


### Bug Fixes

* **composer:** Mage-OS resolve fixes — caret ^0, self-replace, fetch retry ([#232](https://github.com/cresset-tools/bougie/issues/232)) ([96cef9e](https://github.com/cresset-tools/bougie/commit/96cef9ec36cb0d15d13f97a47e773f50244532e6))
* **release:** make release-please actually rewrite Cargo.toml ([#237](https://github.com/cresset-tools/bougie/issues/237)) ([ca40f63](https://github.com/cresset-tools/bougie/commit/ca40f63e432c7ddae1c491db0123fc8101ce1143))
* **release:** unblock musl + windows dist targets ([#233](https://github.com/cresset-tools/bougie/issues/233)) ([87705a9](https://github.com/cresset-tools/bougie/commit/87705a9ec70115f857bb84d9daa827dde5e58f15))

## [Unreleased]

## [0.4.0](https://github.com/cresset-tools/bougie/compare/v0.3.0...v0.4.0) - 2026-05-16

### Added

- *(up)* surface resolved tool dependencies in json-v1
- *(daemon)* warn on catalog vs requires_tools drift
- *(daemon)* recursively install requires_tools[] inner tools
- *(daemon)* walk closure[] when auto-fetching tool tarballs
- *(index)* add requires_tools to manifest schema
- *(services)* auto-detect supervised server docroot
- *(cli)* [**breaking**] promote `services up`/`services down` to top-level `up`/`down`
- *(services)* auto-fetch service tarballs on first `services up`
- *(services)* babysit shim for crash-safe process-group supervision
- *(services)* rabbitmq provisioner (Phase 10)
- *(services)* bougied self-restart on version mismatch (Phase 9)
- *(services)* bougie server as a managed service
- *(services)* opensearch provisioner with per-tenant index templates
- *(services)* mariadb provisioner + integration tests against real binary
- *(services)* log rotation + `bougie services logs [-f] [-n N]`
- *(services)* inject BOUGIE_SERVICE_* env into `bougie run`
- *(services)* redis provisioner + service.{up,down,status} IPC + CLI
- *(daemon)* supervisor state machine, sandbox compilation, tenants ledger
- *(services)* offline subcommands — add/remove/list/catalog
- *(services)* built-in catalog + [services] config schema
- *(services)* bougie services daemon {status,stop,version}
- *(daemon)* bougied entry point + JSON IPC dispatcher
- *(daemon)* vendor sandbox-run + wire bougied shim role and paths

### Fixed

- *(services)* re-sync rabbitmq password to broker after `bougie down` ([#31](https://github.com/cresset-tools/bougie/pull/31))
- *(babysit)* install SIGTERM handler before spawning the service
- *(opensearch)* pin OPENSEARCH_JAVA_HOME + detect early child exit in health probe
- *(services/mariadb)* pass --no-defaults to every mariadb invocation

### Other

- *(index)* drop RequiresTool.manifest_sha256
- [**breaking**] Debian-faithful baseline + --bare / --without flags
- *(services)* convert opensearch pre_start file I/O to tokio::fs
- *(services)* make opensearch provisioner async
- Merge remote-tracking branch 'origin/main' into feat/services-babysit
- Set default binary
- Merge pull request #14 from cresset-tools/feat/services-opensearch
- *(opensearch)* dump opensearch.log on services-up failure
- *(services/mariadb)* pick per-target tarball for the test fixture
- fix macOS-specific failures surfaced in PR #8 validation
- *(services)* end-to-end redis up/down/status integration tests
- *(services)* integration tests for bougied auto-spawn + IPC roundtrip
- [**breaking**] relicense from Apache-2.0 OR MIT to EUPL-1.2

## [0.3.0](https://github.com/cresset-tools/bougie/compare/v0.2.0...v0.3.0) - 2026-05-14

### Added

- *(composer)* add lts channel as a version request
- *(cli)* unify list commands with shared coloured renderer

### Other

- Merge pull request #7 from cresset-tools/worktree-unified-list

## [0.2.0](https://github.com/cresset-tools/bougie/compare/v0.1.0...v0.2.0) - 2026-05-14

### Added

- *(server)* colourise text-mode request log on TTY stderr

### Fixed

- *(ci)* switch release-plz to git_only mode

### Other

- *(release-plz)* authenticate via GitHub App instead of PR_BOT PAT
- refresh lockfile and bump sha2, md-5, anstream to latest majors
- prune stale per-project runtime dirs at startup + shutdown
- make `ext add`, `run --xdebug`, and server routing all work
- pre-download xdebug into the store without enabling it
- split conf.d into conf.d-debug; auto-activate xdebug on first request
- make project arg optional, auto-detect from composer.json
- warn on missing web root / missing index at add + run
- filter notify Access events in watcher to fix reload loop
- sudo-aware server.toml resolution
- canonicalize project path on `server add`
- phase 6 — control socket + live `server list`
- phase 5 — /etc/hosts auto-sync via manage_etc_hosts flag
- phase 4 — pool lifecycle (idle-out, LRU cap, watch reload)
- phase 3 — per-request xdebug pool routing
- phase 2 — FastCGI dispatch to per-project php-fpm pools
- phase 1 — foreground HTTP server with static-file dispatch
- phase 0 — config schema + add/remove/list helpers
- phased build order for bougie server
- one aggregate progress bar per orchestrator call
- strip storeName prefix on closure tarballs, link store/ peer
- walk manifest closure + fix conf.d prefix ordering
- Improve wording
- auto-install composer.json's require.ext-* (CLI.md §3.3 step 4(c))
- install and auto-enable a default extension set per CLI.md §3.5.1.1
- ext list: --only-available keeps the `installed` marker visible
- honor config.sort-packages when editing require maps
- ext add/remove: drop composer subprocess; do the work ourselves
- manifest LoadDirective + install_extension + conf.d fragment writer
- lockfile + composer.json IO and editing primitives
- byte-exact PHP json_encode + Locker::getContentHash port
- add unzip role so composer's ZipDownloader prefers our extractor
- php list: colorize output uv-style, honoring NO_COLOR and pagers
- release v0.1.0
- use PR_BOT PAT for release-plz so it can open PRs and fan out

## [0.1.0](https://github.com/cresset-tools/bougie/releases/tag/v0.1.0) - 2026-05-10

### Other

- use PR_BOT PAT for release-plz so it can open PRs and fan out
- add release-plz for tag + GitHub release automation
- drop the release-mode matrix axis
- run cargo test across {ubuntu, macos} × {debug, release}
- gate BOUGIE_TRUST_ROOT_PATH on a Cargo feature, not debug_assertions
- phase4 — pass `fetch_root` a verifier factory
- ext list: implement filter flags and fix status taxonomy
- php list: implement filter flags
- php install/uninstall: accept multiple targets
- document every flag and render errors uv-style
- pin PHP_BINARY so composer @php scripts find the right php
- composer list: show all channel versions, paged on a tty
- bougie-managed phars with project-shim integration
- build verifier lazily in fetch_root
- ext list: show installed (.so on disk) and available (index) extensions
- strip leading `install/` prefix when extracting tarballs
- locate project root from env, argv[0] path, or cwd ancestors
- make `--` optional, auto-sync, export BOUGIE_PROJECT_ROOT
- follow versioned section URLs from the snapshot root
- switch to lean section + fat manifest wire schema
- anchor relative manifest URLs at the actual section file path
- Sigstore Bundle verification against pinned signer identity
- structured variants with operation + url + hint context
- point repository at cresset-tools/bougie
- point default index URL at index.bougie.tools
- phase 8: remaining commands
- phase 7: bougie sync end-to-end
- phase 6: blob fetch + bougie php install/uninstall/list/find
- phase 5: resolver + locks
- phase 4: index protocol + signature verification
- phase 3: config + request grammar + bougie init
- phase 2: trivial commands + shim dispatch
- phase 1: split into lib + foundation modules
- rework help text + magenta clap color theme
- tighten subcommand help text
- initial scaffolding
