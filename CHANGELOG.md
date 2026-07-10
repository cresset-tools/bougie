# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.48.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.47.0...bougie-v0.48.0) (2026-07-10)


### ⚠ BREAKING CHANGES

* **cli:** `bougie make` with no task argument now lists the available recipe tasks instead of running the `start` task. Use `bougie start` (or `bougie make start`) to bring the project up.
* **composer:** bougie no longer bundles or runs the Composer phar. The `composer` argv[0] shim now dispatches to bougie's native subcommands, so `composer install` (recipes, PATH) runs the native installer. Unrecognized subcommands (create-project, archive, bump, …) error with a pointer to `bougie tool install composer/composer` — the deliberate escape hatch. Removed: bougie-composer fetch/request/resolve modules + install_composer; paths.composer_phar; the resolved-composer state file; the `[composer] version` config field; sync's phar fetch and SyncResult.composer_{version,path}.
* **composer:** `bougie composer {fetch,uninstall,list,find,pin,dir,upgrade}` are removed. Pin the composer version via bougie.toml instead.
* **composer-install:** `bougie composer install` no longer exits non-zero when the lockfile declares a Composer plugin or `composer.json` has a non-empty `scripts` section. CI pipelines that relied on that failure must inspect the new `warnings` field or the stderr `warning:` lines.
* **cli:** `bougie composer install <version>` no longer manages Composer phars — that verb is now `bougie composer fetch <version>`. Bare `bougie composer install` (no positional) is the new Composer-convention project install: reads `composer.lock` from CWD (or `-d <dir>`), content-hash-verifies it, downloads dists into `vendor/`, emits `vendor/autoload.php` + `installed.{json,php}`.

### Features

* **auth:** support github-oauth, gitlab-token, gitlab-oauth ([#188](https://github.com/cresset-tools/bougie/issues/188)) ([267ef99](https://github.com/cresset-tools/bougie/commit/267ef99c470f041cee1d2df4d4e67506473c205f))
* **autoloader:** --apcu-autoloader + config.autoloader-suffix ([578c85d](https://github.com/cresset-tools/bougie/commit/578c85d178b0f8ec13aefd98823fdf0520c1cd51))
* **autoloader:** --apcu-autoloader + config.autoloader-suffix ([bf8f2c5](https://github.com/cresset-tools/bougie/commit/bf8f2c566bb59ddc9bb48afcfb0ea9c7c1b7bac1))
* **autoloader:** --optimize + --classmap-authoritative flags ([e10aca9](https://github.com/cresset-tools/bougie/commit/e10aca925e49c938a2079e7fac7d5755b7e8c862))
* **autoloader:** --optimize + --classmap-authoritative flags ([161464b](https://github.com/cresset-tools/bougie/commit/161464b4c39b92b15c97bd819588b3dd5493da87))
* **autoloader:** autoload_real.php emit (Phase 3 part 2) ([6785283](https://github.com/cresset-tools/bougie/commit/6785283c02990d5525ae3cea64e6ef94b4cff200))
* **autoloader:** autoload_real.php emit (Phase 3 part 2) ([a050c2b](https://github.com/cresset-tools/bougie/commit/a050c2b2736f4329db3bbe606f11f29c168df557))
* **autoloader:** autoload_static.php emit (Phase 3 part 3) ([ef47352](https://github.com/cresset-tools/bougie/commit/ef47352a05e67b8feecba4f2b0b7f34a1f1a7488))
* **autoloader:** autoload_static.php emit (Phase 3 part 3) ([ae3bcc4](https://github.com/cresset-tools/bougie/commit/ae3bcc4a60024a696cb0d7950fa08221fbe06c59))
* **autoloader:** bougie-autoloader skeleton + byte-equivalence fixtures ([c7c0af9](https://github.com/cresset-tools/bougie/commit/c7c0af955747989aba1331b7cddd2bfe2e4c380d))
* **autoloader:** bougie-autoloader skeleton + byte-equivalence fixtures ([983e2eb](https://github.com/cresset-tools/bougie/commit/983e2eb39ce2f8e632684b10a27caf23e8161bfb))
* **autoloader:** classmap scanner + emitter (Phase 2 part 1) ([2df7d9c](https://github.com/cresset-tools/bougie/commit/2df7d9c8f271dbdd42207bd7689bcf5bec39aaeb))
* **autoloader:** classmap scanner + emitter (Phase 2 part 1) ([265d618](https://github.com/cresset-tools/bougie/commit/265d61855751e1353bab602f704013ae9d7607db))
* **autoloader:** emit installed.json + installed.php ([2851fe0](https://github.com/cresset-tools/bougie/commit/2851fe0b6abe052638631c47ca7cfcb0c09421ff))
* **autoloader:** emit installed.json + installed.php ([c96333f](https://github.com/cresset-tools/bougie/commit/c96333f6bd6f32abe3b05ef54a8f912b358dfc76))
* **autoloader:** emit platform_check.php (Composer config.platform-check) ([#337](https://github.com/cresset-tools/bougie/issues/337)) ([d9bbb12](https://github.com/cresset-tools/bougie/commit/d9bbb1269b530efb4a5d10a5b04b876c6a0812c8))
* **autoloader:** exclude-from-classmap + classmap-exclude/mixed fixtures ([63e4bbe](https://github.com/cresset-tools/bougie/commit/63e4bbeafd63781bdf9c53233ba60579442cae2f))
* **autoloader:** exclude-from-classmap + classmap-exclude/mixed fixtures ([ffdb937](https://github.com/cresset-tools/bougie/commit/ffdb93729f2f153945a5d00b960a6dda89676869))
* **autoloader:** full Composer normalize() port ([d23c3a5](https://github.com/cresset-tools/bougie/commit/d23c3a537c43cd86e1a7fd46bb4cfe3b7f9dedb7))
* **autoloader:** full Composer normalize() port ([03c8747](https://github.com/cresset-tools/bougie/commit/03c874784110e18070e1f39261cceedd8fbf2baf))
* **autoloader:** Phase 1 — PSR-4 / PSR-0 / files emitters ([5040d2a](https://github.com/cresset-tools/bougie/commit/5040d2aceb96102bd2f50783de4fd2fbc8bbfe25))
* **autoloader:** Phase 1 — PSR-4 / PSR-0 / files emitters (re-land) ([5477f0f](https://github.com/cresset-tools/bougie/commit/5477f0ff9a6e2bcd653665716034056d21c991dc))
* **autoloader:** surface PSR warnings and Composer-style footer ([#113](https://github.com/cresset-tools/bougie/issues/113)) ([4ab31f1](https://github.com/cresset-tools/bougie/commit/4ab31f1015ab81b4f5be149f85ac8c2522c79251))
* **autoloader:** vendored runtime files (Phase 3 part 1) ([34af086](https://github.com/cresset-tools/bougie/commit/34af086895242fa43614b5962143190a0f672faf))
* **autoloader:** vendored runtime files (Phase 3 part 1) ([fee6e59](https://github.com/cresset-tools/bougie/commit/fee6e592de0327feb826aa0da26bf7447b5712de))
* **babysit:** co-locate a service's helper daemon via --sidecar (epmd, macOS-correct) ([#285](https://github.com/cresset-tools/bougie/issues/285)) ([30486ad](https://github.com/cresset-tools/bougie/commit/30486ad063e0bdc6e80109a3fe0b5a8f271ca7b0))
* **backend:** extract Backend trait + BougieIndexBackend (phase 2) ([e9286c6](https://github.com/cresset-tools/bougie/commit/e9286c62caafe378bb22760fa938a8c2b85a28f1))
* **backend:** imagick on Windows via per-ext PATH extras (phase 5) ([5fe954e](https://github.com/cresset-tools/bougie/commit/5fe954e2ab2726e3590b4a4a545e925dacaae393))
* **backend:** imagick on Windows via per-ext PATH extras (phase 5) ([151c9c6](https://github.com/cresset-tools/bougie/commit/151c9c62755297c46bb5141bdefe93aab854482e))
* **backend:** PECL extensions via windows.php.net (phase 4b) ([814d019](https://github.com/cresset-tools/bougie/commit/814d0192061213dc72456b510c3d4a5492e76e15))
* **backend:** PECL extensions via windows.php.net (phase 4b) ([6932d71](https://github.com/cresset-tools/bougie/commit/6932d71b2d992c6522172934e8acb09b6efef5ed))
* **backend:** WindowsPhpNetBackend interpreter path (phase 3) ([f3bd3f4](https://github.com/cresset-tools/bougie/commit/f3bd3f4fc1d4b5427a18e110ecaf3d385f66b372))
* **backend:** WindowsPhpNetBackend interpreter path (phase 3) ([e60b145](https://github.com/cresset-tools/bougie/commit/e60b145f5494a2c5759991a946f4b01afdac2f32))
* **baseline:** add xml family and mysqlnd to BASELINE_EXTENSIONS ([7a06dfa](https://github.com/cresset-tools/bougie/commit/7a06dfa540c2f751fb4b695781f952f2080ae787))
* **baseline:** add xml family and mysqlnd to BASELINE_EXTENSIONS ([14a22fa](https://github.com/cresset-tools/bougie/commit/14a22fae5e7114bae6bc0f9a809cf19a937176f9))
* bougie make + bougie run scripts ([b2d7a28](https://github.com/cresset-tools/bougie/commit/b2d7a280d7a68b4ecedbb3b076f09f87a56ab9a8))
* **cli:** add `bougie lock` — minimal lockfile refresh ([#351](https://github.com/cresset-tools/bougie/issues/351)) ([ca6056a](https://github.com/cresset-tools/bougie/commit/ca6056a18884b4e6b87e17b71b00dfb76c26f16f))
* **cli:** bougie composer dump-autoloader ([6b92845](https://github.com/cresset-tools/bougie/commit/6b92845cf30ced30f9ae1325d7035075424b9d2d))
* **cli:** bougie composer dump-autoloader ([c0fa870](https://github.com/cresset-tools/bougie/commit/c0fa8705c626757a847b3ee2209a427699a809b6))
* **cli:** bougie composer install (project install) + Composer fetch rename ([54225e1](https://github.com/cresset-tools/bougie/commit/54225e196827f724421a2505176b99044ecc35d4))
* **cli:** bougie composer install / fetch rename ([1caa56a](https://github.com/cresset-tools/bougie/commit/1caa56aed41aa23a83ecc100848ed39f4198a634))
* **cli:** bougie composer update --dry-run ([#117](https://github.com/cresset-tools/bougie/issues/117)) ([1568377](https://github.com/cresset-tools/bougie/commit/15683776067c9a0f0a2b5e4e8850e0b0cd006df1))
* **cli:** composer install falls back to resolve when composer.lock is missing ([#132](https://github.com/cresset-tools/bougie/issues/132)) ([e39e195](https://github.com/cresset-tools/bougie/commit/e39e195b567407f72a7f13d55ac50a63f9c0a4e5))
* **cli:** implement bougie composer validate ([#189](https://github.com/cresset-tools/bougie/issues/189)) ([08fad82](https://github.com/cresset-tools/bougie/commit/08fad823fb80bb474791cfd91443f322faffc84a))
* **cli:** luxury-themed help headline ([#396](https://github.com/cresset-tools/bougie/issues/396)) ([13af36c](https://github.com/cresset-tools/bougie/commit/13af36cc6f4502f7dfab0e8c5e29f7f9c6124b7e))
* **cli:** native uv-style verbs — add, remove, tree, outdated ([#350](https://github.com/cresset-tools/bougie/issues/350)) ([b74126f](https://github.com/cresset-tools/bougie/commit/b74126fb27b960ef8b92b57d54a7ae87501f9c4c))
* **cli:** promote `services projects` to top-level `bougie projects` ([#365](https://github.com/cresset-tools/bougie/issues/365)) ([da52e1d](https://github.com/cresset-tools/bougie/commit/da52e1df7d61b178a67e53413e94024db6fa6af9))
* **cli:** rename 'bougie services' to 'bougie service' ([#453](https://github.com/cresset-tools/bougie/issues/453)) ([0d7e5d6](https://github.com/cresset-tools/bougie/commit/0d7e5d62928c51a0022f6f7d4e2474b1e199da3b))
* **cli:** reorganize the CLI around start/stop, group --help ([#378](https://github.com/cresset-tools/bougie/issues/378)) ([a0c629b](https://github.com/cresset-tools/bougie/commit/a0c629bae9e68a801a5e8a57f607d6108374e40b))
* **cli:** uv-style --version with git sha, date, and target triple ([#243](https://github.com/cresset-tools/bougie/issues/243)) ([06293c3](https://github.com/cresset-tools/bougie/commit/06293c31da20d3b332a11de1687f7355eb771ed9))
* **composer-auth:** read global auth.json + COMPOSER_AUTH env ([#162](https://github.com/cresset-tools/bougie/issues/162)) ([1d2bf36](https://github.com/cresset-tools/bougie/commit/1d2bf3619c7997fc2f93140cea4db37e9eb089d1))
* **composer-install:** aggregate plugin warnings into one line ([#165](https://github.com/cresset-tools/bougie/issues/165)) ([7e31f53](https://github.com/cresset-tools/bougie/commit/7e31f5360523e72af803bf670844bc5d763d5ec3))
* **composer-install:** generate vendor/bin proxy scripts ([#186](https://github.com/cresset-tools/bougie/issues/186)) ([92cb3fa](https://github.com/cresset-tools/bougie/commit/92cb3fabc48ad35b7896db82fc387777b7b0b8c0))
* **composer-install:** install dists from `type: artifact` repos ([#193](https://github.com/cresset-tools/bougie/issues/193)) ([d1f0aca](https://github.com/cresset-tools/bougie/commit/d1f0acaf62514b631f79e273dcf6287e27644caf))
* **composer-install:** per-package progress bar ([#164](https://github.com/cresset-tools/bougie/issues/164)) ([7573682](https://github.com/cresset-tools/bougie/commit/757368284611237d7ff698ff4fbaf3b31ef32933))
* **composer-install:** platform requirement checks + --ignore-platform-reqs ([#187](https://github.com/cresset-tools/bougie/issues/187)) ([d266b56](https://github.com/cresset-tools/bougie/commit/d266b56fe431f1e3343596efa0e4cc6f102da399))
* **composer-install:** skip up-to-date packages by diffing against installed.json ([#194](https://github.com/cresset-tools/bougie/issues/194)) ([be7d3f2](https://github.com/cresset-tools/bougie/commit/be7d3f2021fae08e16035f24c76cb6113d07dccb))
* **composer-install:** warn instead of error on Composer plugins/scripts ([#160](https://github.com/cresset-tools/bougie/issues/160)) ([7bf2a01](https://github.com/cresset-tools/bougie/commit/7bf2a01a2fa060f75daf69ec9e6acb2752208708))
* **composer-resolver:** add parallel dist downloader ([e20113d](https://github.com/cresset-tools/bougie/commit/e20113d09164046c8fcae2ad21b6a120fb3ecdd1))
* **composer-resolver:** consult /p2/&lt;name&gt;~dev.json when dev versions allowed ([#121](https://github.com/cresset-tools/bougie/issues/121)) ([12abafa](https://github.com/cresset-tools/bougie/commit/12abafadbb5d4c0a3e69f455a8ba9d705dbd4942))
* **composer-resolver:** encode replace/provide as additional requires ([#119](https://github.com/cresset-tools/bougie/issues/119)) ([5c55701](https://github.com/cresset-tools/bougie/commit/5c55701d1c9b3edf147ef09ac5950dc5a13d0295))
* **composer-resolver:** honor --ignore-platform-req(s) at resolve time ([#427](https://github.com/cresset-tools/bougie/issues/427)) ([c304b13](https://github.com/cresset-tools/bougie/commit/c304b13746abd69ea7796266f369f14617d02f2d))
* **composer-resolver:** http-basic + bearer auth for composer-type repos ([#131](https://github.com/cresset-tools/bougie/issues/131)) ([5ad131d](https://github.com/cresset-tools/bougie/commit/5ad131dc5c1792702b4e84c7ad8960098bf0d4d4))
* **composer-resolver:** install_from_lock orchestrator ([d7e3c77](https://github.com/cresset-tools/bougie/commit/d7e3c77e7bff86b5660cbea8890bc263e5d5caf4))
* **composer-resolver:** minimum-stability + per-package [@stability](https://github.com/stability) flags ([#120](https://github.com/cresset-tools/bougie/issues/120)) ([5899107](https://github.com/cresset-tools/bougie/commit/5899107ed407ef4e072218f40640f9728f8b8142))
* **composer-resolver:** Packagist v2 metadata fetcher ([#114](https://github.com/cresset-tools/bougie/issues/114)) ([061ec52](https://github.com/cresset-tools/bougie/commit/061ec52b38dea1e49fd93c69bcf1922d664ccd77))
* **composer-resolver:** parallel metadata pre-fetch closure ([#136](https://github.com/cresset-tools/bougie/issues/136)) ([032230f](https://github.com/cresset-tools/bougie/commit/032230fc926090b872a968b74ff1f36e0ce0b576))
* **composer-resolver:** Phase A foundation — parallel downloader + lock reader ([fb24f18](https://github.com/cresset-tools/bougie/commit/fb24f18d919b178e0ba00ea46a81907cc5833341))
* **composer-resolver:** prefer-stable candidate ordering ([#126](https://github.com/cresset-tools/bougie/issues/126)) ([3af3e3f](https://github.com/cresset-tools/bougie/commit/3af3e3f1e07f4152d22befd21e04e113d3254eb3))
* **composer-resolver:** pubgrub --lock-verify ([#110](https://github.com/cresset-tools/bougie/issues/110)) ([57b3605](https://github.com/cresset-tools/bougie/commit/57b36056b3ce40226e01c22f33849a582e860af8))
* **composer-resolver:** pubgrub DependencyProvider over Packagist ([#115](https://github.com/cresset-tools/bougie/issues/115)) ([f9041df](https://github.com/cresset-tools/bougie/commit/f9041df604c632334e545d711422843a41509655))
* **composer-resolver:** rewrite GitHub dist URLs to bypass API rate limit ([#168](https://github.com/cresset-tools/bougie/issues/168)) ([dfd4999](https://github.com/cresset-tools/bougie/commit/dfd499943cfa7b08b9a2c7af02f7bbc35ca7e4d3))
* **composer-resolver:** send per-host auth on dist downloads ([#134](https://github.com/cresset-tools/bougie/issues/134)) ([0f32f08](https://github.com/cresset-tools/bougie/commit/0f32f08c1b9a9697f0828ff54d5729795242300c))
* **composer-resolver:** solve-phase progress spinner + tracing logs ([#137](https://github.com/cresset-tools/bougie/issues/137)) ([8d2dd09](https://github.com/cresset-tools/bougie/commit/8d2dd0935e47dc627c3b8bdc6fb2bcfebf2c1897))
* **composer-resolver:** support Composer `type: path` repositories ([#382](https://github.com/cresset-tools/bougie/issues/382)) ([c4fbb7f](https://github.com/cresset-tools/bougie/commit/c4fbb7fa63c19b4e4cdd19140ab55deaa0a2fbae))
* **composer-resolver:** support Composer v1 repositories ([#133](https://github.com/cresset-tools/bougie/issues/133)) ([79b2829](https://github.com/cresset-tools/bougie/commit/79b2829526cf101a4a4656a370f409769e261741))
* **composer-resolver:** support composer.json `repositories` field (composer-type) ([#130](https://github.com/cresset-tools/bougie/issues/130)) ([04c996a](https://github.com/cresset-tools/bougie/commit/04c996a8cab6e0cc13e6164b175c692f1760a854))
* **composer-resolver:** virtual packages via provide/replace pre-fetch ([#124](https://github.com/cresset-tools/bougie/issues/124)) ([eb8ee13](https://github.com/cresset-tools/bougie/commit/eb8ee1356e54706257c63201614c1ee57a6b753d))
* **composer-resolver:** wildcard replace/provide via on-demand synthesis ([#127](https://github.com/cresset-tools/bougie/issues/127)) ([2b2c0b1](https://github.com/cresset-tools/bougie/commit/2b2c0b1bd0c0da370cb2805de9cd683bb86f992e))
* **composer-resolver:** write composer.lock from bougie composer update ([#123](https://github.com/cresset-tools/bougie/issues/123)) ([2a355cb](https://github.com/cresset-tools/bougie/commit/2a355cbc75f544c1fbfe4041e881cd7062e9fc6e))
* **composer:** distinguish -w and -W in partial update ([#330](https://github.com/cresset-tools/bougie/issues/330)) ([515a562](https://github.com/cresset-tools/bougie/commit/515a56278563f4a73111a57e6ac543b910920bcc))
* **composer:** install composer-plugin package files (hooks stay skipped) ([#491](https://github.com/cresset-tools/bougie/issues/491)) ([6b0f6b9](https://github.com/cresset-tools/bougie/commit/6b0f6b9f6b26ce681f1b0da7ffd40bea464895dc))
* **composer:** native uv-pip Composer surface; drop the phar ([#348](https://github.com/cresset-tools/bougie/issues/348)) ([0d491f6](https://github.com/cresset-tools/bougie/commit/0d491f60853c3b0b1d32ec95afe51ecc4d6214c9))
* **composer:** partial update (composer update &lt;packages&gt;) ([#328](https://github.com/cresset-tools/bougie/issues/328)) ([bcf4ecd](https://github.com/cresset-tools/bougie/commit/bcf4ecde2d844ef0f25e2632315c4eee7d2b91c3))
* **composer:** support repository dist mirrors (Private Packagist) ([#439](https://github.com/cresset-tools/bougie/issues/439)) ([7b0f2ff](https://github.com/cresset-tools/bougie/commit/7b0f2ff4c18562152dbb3144ae6f4e376d8394e3))
* **composer:** trim surface to native ops; make Composer a default project-aware tool ([#277](https://github.com/cresset-tools/bougie/issues/277)) ([970c751](https://github.com/cresset-tools/bougie/commit/970c7512956d94ffd51aacd988df10e2ae5406e6))
* **composer:** typed composer.lock reader ([54e3921](https://github.com/cresset-tools/bougie/commit/54e3921f1f1abff4544e61092b11303b977fd5fe))
* **daemon:** cgroup-v2 supervision backend — reap daemonized escapees (e.g. epmd) ([#283](https://github.com/cresset-tools/bougie/issues/283)) ([535e198](https://github.com/cresset-tools/bougie/commit/535e198a7e708a39a1db207963e3ae3c313fc226))
* **daemon:** export BOUGIE_SERVICE_*_HOST and _PORT for TCP services ([5d318b9](https://github.com/cresset-tools/bougie/commit/5d318b9b6675772c9694ec66d9ef1220fedfaceb))
* **daemon:** forward the extracting phase to the CLI's mirrored download bar ([#272](https://github.com/cresset-tools/bougie/issues/272)) ([ecbb147](https://github.com/cresset-tools/bougie/commit/ecbb147c076dc6435da6e1f573a356d947f9756e))
* **daemon:** graceful shutdown + opensearch jdk runtime dep ([#297](https://github.com/cresset-tools/bougie/issues/297)) ([a21a051](https://github.com/cresset-tools/bougie/commit/a21a051384a8a63d8291ae3e951bfd27cfb03cb7))
* **daemon:** stream tarball download progress to the CLI ([#169](https://github.com/cresset-tools/bougie/issues/169)) ([77ead93](https://github.com/cresset-tools/bougie/commit/77ead93ee57b3bcdcc9f603100b3e9bdbae71a73))
* **db:** add `bougie db seed` to load a jibs snapshot into the mariadb tenant ([#476](https://github.com/cresset-tools/bougie/issues/476)) ([2746b0b](https://github.com/cresset-tools/bougie/commit/2746b0b37be3f9dd48301e899792563d4e9935c7))
* **diagnose:** failure ring with repeat-collapsing replaces the single slot ([#490](https://github.com/cresset-tools/bougie/issues/490)) ([a42e49f](https://github.com/cresset-tools/bougie/commit/a42e49fca509a4ec03f858237d9c2fdfb507e962))
* **diagnose:** service-aware reports reviewed in $EDITOR, markdown wire v2 ([#472](https://github.com/cresset-tools/bougie/issues/472)) ([68323cc](https://github.com/cresset-tools/bougie/commit/68323ccd1af802fd350610e4099d2a8839e3c5d2))
* **dist:** build linux-gnu against glibc 2.17 in manylinux2014 ([#355](https://github.com/cresset-tools/bougie/issues/355)) ([4ce19eb](https://github.com/cresset-tools/bougie/commit/4ce19eb809a60c8e1c2c5bb6484d02b841fad933))
* **docker:** publish container images via cargo-zigbuild ([#305](https://github.com/cresset-tools/bougie/issues/305)) ([3ea8f05](https://github.com/cresset-tools/bougie/commit/3ea8f05c6c2241aa93181561cfe9ef882b652a40))
* **errors:** categorize command failures — chain-walk + typed no-project/config/service ([#485](https://github.com/cresset-tools/bougie/issues/485)) ([0f79c15](https://github.com/cresset-tools/bougie/commit/0f79c151cf60ed6116e1952921c118e8244f9191))
* **extensions:** Add curl to the baseline ([70a28d0](https://github.com/cresset-tools/bougie/commit/70a28d0c5dcf9b09ea7bb78e9e08e3492f6b51ef))
* **ext:** Mach-O parser for macOS PHP extensions ([5ca89c8](https://github.com/cresset-tools/bougie/commit/5ca89c89112373eb9e256078a5bd2d7b7334b2b2))
* **ext:** support local .so installs via `bougie ext add <path>` ([08428cf](https://github.com/cresset-tools/bougie/commit/08428cf61f61d075fcd4a6ca9edf35237d7c6da8))
* **ext:** support local .so installs via `bougie ext add <path>` ([58b7eb5](https://github.com/cresset-tools/bougie/commit/58b7eb55d7a00bd3d64f8e9b270d5b24af80e3c6))
* **fetch:** detect_zip_top_level + DistRequest auto-detect ([3c65f50](https://github.com/cresset-tools/bougie/commit/3c65f5041334dcd8ab3117b01be5376dbe030414))
* **fetch:** shared bougie/&lt;v&gt; User-Agent across all outbound HTTP ([#116](https://github.com/cresset-tools/bougie/issues/116)) ([5f01a22](https://github.com/cresset-tools/bougie/commit/5f01a22408f21433bf4b65e0a357a9d1dd86a542))
* **fetch:** switch on ArchiveKind { TarZst, Zip } in extract path ([2aa6891](https://github.com/cresset-tools/bougie/commit/2aa689177b32ce989803ac90ba94a1d3a614a45f))
* **format:** add `bougie format`, the `uv format` model for PHP ([#368](https://github.com/cresset-tools/bougie/issues/368)) ([2b0e567](https://github.com/cresset-tools/bougie/commit/2b0e567c4da6c6e20f4b07308bc902d2b3a1eca3))
* **init:** add --name flag and a new &lt;directory&gt; command ([#284](https://github.com/cresset-tools/bougie/issues/284)) ([e184eb9](https://github.com/cresset-tools/bougie/commit/e184eb9360ee0a580226ae53beaf1d36b6a21861))
* **init:** bougie init --starter &lt;url|alias&gt; + --start ([#263](https://github.com/cresset-tools/bougie/issues/263)) ([bfb5bcd](https://github.com/cresset-tools/bougie/commit/bfb5bcdce03ca77f461d14a5afd2c636404fb94f))
* **init:** scaffold `--starter laravel` via the laravel installer ([#383](https://github.com/cresset-tools/bougie/issues/383)) ([7fa7c08](https://github.com/cresset-tools/bougie/commit/7fa7c083ac92f867e16879547351ee46eab027df))
* **init:** treat --starter as a base URL, append /starter.json ([#265](https://github.com/cresset-tools/bougie/issues/265)) ([6a7b958](https://github.com/cresset-tools/bougie/commit/6a7b958e3686c38d136d2fc9a2c6ea80e5f6d005))
* **installer:** baseline mbstring, pdo_sqlite, sqlite3 for Composer parity ([cdb5c51](https://github.com/cresset-tools/bougie/commit/cdb5c514a94e03f0122bae1c5a1c61572cc783f7))
* **installer:** count progress for baseline extension install ([#296](https://github.com/cresset-tools/bougie/issues/296)) ([045fee6](https://github.com/cresset-tools/bougie/commit/045fee64207b98ed5bc8d441e3b892c6adfa6d42))
* **installers:** native Composer install-plugin support (Magento, composer/installers, Laravel) ([#248](https://github.com/cresset-tools/bougie/issues/248)) ([ebdf9c3](https://github.com/cresset-tools/bougie/commit/ebdf9c31be080a26ce00196c5b4ceefb27b5599e))
* **install:** use bundled DLLs for Windows baseline extensions ([3a3d1c3](https://github.com/cresset-tools/bougie/commit/3a3d1c3b129fadfc801292c2ba424a9b836a4a8e))
* **install:** use bundled DLLs for Windows baseline extensions ([d92d590](https://github.com/cresset-tools/bougie/commit/d92d590c1f5780836490a539b7cef69f649dc701))
* **login:** `bougie login` for a Composer registry via device grant ([#479](https://github.com/cresset-tools/bougie/issues/479)) ([212ba7f](https://github.com/cresset-tools/bougie/commit/212ba7f4e88551a6faef4d29fd64fafe40f50297))
* **login:** auto-provision project repositories after login ([#484](https://github.com/cresset-tools/bougie/issues/484)) ([ddb7c38](https://github.com/cresset-tools/bougie/commit/ddb7c382ce51c5c43c9c770e1edf687334c4ab63))
* **node:** Node.js toolchain via nodejs.org + run PATH overlay ([#371](https://github.com/cresset-tools/bougie/issues/371)) ([72b0e4d](https://github.com/cresset-tools/bougie/commit/72b0e4d846568c208f32f47b1962e7a5e4638e4e))
* **patches:** add `patches create` to capture vendor edits as clean patches ([#417](https://github.com/cresset-tools/bougie/issues/417)) ([d087da6](https://github.com/cresset-tools/bougie/commit/d087da611ee725096d1998a98907837aeb57b6a5))
* **patches:** apply multi-package top-level patches at the project root ([#430](https://github.com/cresset-tools/bougie/issues/430)) ([33e1124](https://github.com/cresset-tools/bougie/commit/33e1124135afd172f83db4a093ac607d3e899a0c))
* **patches:** native cweagans/composer-patches reimplementation ([#384](https://github.com/cresset-tools/bougie/issues/384)) ([85a3ec9](https://github.com/cresset-tools/bougie/commit/85a3ec995c7360bfc9d6116bab0ba56e94ecce88))
* **paths:** anchor windows home + cache under %LOCALAPPDATA% ([2b1827a](https://github.com/cresset-tools/bougie/commit/2b1827a7d4565fc522522a58dab3c1da28b9bfea))
* **paths:** move project toolchain into vendor/bougie; durable state under $BOUGIE_HOME ([#372](https://github.com/cresset-tools/bougie/issues/372)) ([2c92332](https://github.com/cresset-tools/bougie/commit/2c923323be730b8d9d7217e158917570dea04234))
* **paths:** split Windows home into roaming state + local downloads ([5899ade](https://github.com/cresset-tools/bougie/commit/5899ade19ca50aecf6c7fa4bd10a58d66c6186b6))
* **paths:** split Windows home into roaming state + local downloads ([750eaf4](https://github.com/cresset-tools/bougie/commit/750eaf4b575ef148d94f7ce2fb748ca9a293ae7f))
* **php-discovery:** only use system PHP for one-off runs by default ([#433](https://github.com/cresset-tools/bougie/issues/433)) ([a934e49](https://github.com/cresset-tools/bougie/commit/a934e49c893cc2281fac7df0ab2c2123df1adfcd))
* **php:** system PHP support (uv's system-Python model) ([#354](https://github.com/cresset-tools/bougie/issues/354)) ([32eef2e](https://github.com/cresset-tools/bougie/commit/32eef2e612b3e6a3c8ee3c62d2ec17e626fa9b8e))
* **recipe/magento:** wire Redis into setup:install ([#174](https://github.com/cresset-tools/bougie/issues/174)) ([47a2614](https://github.com/cresset-tools/bougie/commit/47a2614db93313706918059602e1bd87ba3e6dd2))
* **recipe:** add localdev task to disable 2FA and set indexers realtime ([#409](https://github.com/cresset-tools/bougie/issues/409)) ([796e187](https://github.com/cresset-tools/bougie/commit/796e18765123ee04f6c5cf2f73768313ae7b28dd))
* **recipe:** bougie start with DAG-based recipe engine ([bc3ce6e](https://github.com/cresset-tools/bougie/commit/bc3ce6ea81613471364da1bd9beb3a74e8752075))
* **recipe:** print URL and admin creds after `bougie start` ([#171](https://github.com/cresset-tools/bougie/issues/171)) ([8f3f2c8](https://github.com/cresset-tools/bougie/commit/8f3f2c851f85a4e77a73ddc6858a969d2f03679c))
* **release:** ship dist binary pipeline + bougie.tools mirror ([#206](https://github.com/cresset-tools/bougie/issues/206)) ([560c259](https://github.com/cresset-tools/bougie/commit/560c259eb09e3c07be52aca8eee25693226eb4b7))
* **run:** add `--php` to select the interpreter for one run ([#366](https://github.com/cresset-tools/bougie/issues/366)) ([bf47d59](https://github.com/cresset-tools/bougie/commit/bf47d59b47388efc1c35baedbfce545c0965f0d8))
* **run:** fall back to default PHP when no project constraint ([d85b6c7](https://github.com/cresset-tools/bougie/commit/d85b6c7c4bf41c3f08ff2399b7ab9a90e758918b))
* **run:** fall back to default PHP when no project constraint ([7d847e6](https://github.com/cresset-tools/bougie/commit/7d847e6afa2c5c237b4d520a738d996af80ab3c0))
* **run:** lift CLI memory_limit to -1 for project PHP runs ([#471](https://github.com/cresset-tools/bougie/issues/471)) ([c813ab0](https://github.com/cresset-tools/bougie/commit/c813ab0ccc34156327ae612d2fb2bfeb6abf9acc))
* **run:** look up composer.json scripts before falling through to exec ([36e68cb](https://github.com/cresset-tools/bougie/commit/36e68cbade5606e1497f1322edfb85bc4dc43e29))
* **scripts:** opt-in root composer.json script execution ([#324](https://github.com/cresset-tools/bougie/issues/324)) ([b7637f3](https://github.com/cresset-tools/bougie/commit/b7637f31f8858f43d676d6957b6cf208b8f082a1))
* **script:** uv-style single-file PHP scripts with inline dependencies ([#429](https://github.com/cresset-tools/bougie/issues/429)) ([b8ea30a](https://github.com/cresset-tools/bougie/commit/b8ea30a289f897d50812279e988ac4fb9e94b71b))
* **self-update:** only update a binary bougie's installer placed ([#279](https://github.com/cresset-tools/bougie/issues/279)) ([f929f26](https://github.com/cresset-tools/bougie/commit/f929f2676d2fcd6cabf13e45ef57baa0bb490cbe))
* **self:** implement bougie self update ([#244](https://github.com/cresset-tools/bougie/issues/244)) ([3f0f200](https://github.com/cresset-tools/bougie/commit/3f0f200b5faee9a801f71e3183eb7148239cc889))
* **semver:** bougie-semver crate + Layer 1 conformance fixture ([6c2bb10](https://github.com/cresset-tools/bougie/commit/6c2bb10621285d909f352c6fab7d722edad82d60))
* **semver:** bougie-semver crate skeleton + Layer 1 conformance fixture ([44b96cb](https://github.com/cresset-tools/bougie/commit/44b96cb346e40d4bd37fa65bfc3725becb703c91))
* **semver:** Composer-conformant Version + Constraint impl ([#104](https://github.com/cresset-tools/bougie/issues/104)) ([544b991](https://github.com/cresset-tools/bougie/commit/544b991407800f14f14723967a58c7fdfe93cb64))
* **semver:** Constraint::parse handles dev-&lt;name&gt; branch references ([#129](https://github.com/cresset-tools/bougie/issues/129)) ([098a69d](https://github.com/cresset-tools/bougie/commit/098a69d59a6f3bb1bc0fe6edd474a07cbadd07d4))
* **semver:** Constraint::parse handles Nx-dev + commit-ref + [@stability](https://github.com/stability) suffixes ([#125](https://github.com/cresset-tools/bougie/issues/125)) ([5d83100](https://github.com/cresset-tools/bougie/commit/5d831002f2fcca5d0e0266470715cbcba0372ceb))
* server-resident live autoloader ([#108](https://github.com/cresset-tools/bougie/issues/108)) ([#111](https://github.com/cresset-tools/bougie/issues/111)) ([df5e03d](https://github.com/cresset-tools/bougie/commit/df5e03d3f12c44fdd35c4d06f67d53ea7401fc80))
* **server:** [[host.rewrite]] with framework auto-detection ([ee1ebe4](https://github.com/cresset-tools/bougie/commit/ee1ebe4bcbb88e8dbb4ca9e447c4deeb5e99bf3c))
* **server:** default web (php-fpm) memory_limit to 1G ([#294](https://github.com/cresset-tools/bougie/issues/294)) ([cf61811](https://github.com/cresset-tools/bougie/commit/cf61811981ee3c10e68f4ec98e529837c4ba37ca))
* **server:** port bougie-server to Windows via php-cgi.exe ([dd4ea23](https://github.com/cresset-tools/bougie/commit/dd4ea239315a64bfc66e59157950d4572ef23f71))
* **server:** port bougie-server to Windows via php-cgi.exe ([cba3e00](https://github.com/cresset-tools/bougie/commit/cba3e00616e2fb03203a0781268f378964b44aac))
* **server:** redesign `bougie server` as a project verb over the shared daemon ([#318](https://github.com/cresset-tools/bougie/issues/318)) ([131a4d5](https://github.com/cresset-tools/bougie/commit/131a4d512a6bb65ba12478438fe542aa756eeaf2))
* **server:** require --config on all server subcommands; drop XDG default ([2a6dfb1](https://github.com/cresset-tools/bougie/commit/2a6dfb193cc1c0b801ac6aedd67c34c651cfc22a))
* **server:** run the dev server against a system PHP ([#363](https://github.com/cresset-tools/bougie/issues/363)) ([9890626](https://github.com/cresset-tools/bougie/commit/98906264bdfccd27f59753ea935aa56d408e8dac))
* **server:** warn when DNS blocks *.bougie.run loopback answers ([#464](https://github.com/cresset-tools/bougie/issues/464)) ([5de12e2](https://github.com/cresset-tools/bougie/commit/5de12e2408d3180be13cc4ca39f13fc336728c71))
* **service:** credentials subcommand for tenant connection info ([#462](https://github.com/cresset-tools/bougie/issues/462)) ([b3aca75](https://github.com/cresset-tools/bougie/commit/b3aca7518764c9d3bcb4351a28b87c3104f6eb1a))
* **services:** add `bougie services projects` (list provisioned tenants) + `purge` ([#320](https://github.com/cresset-tools/bougie/issues/320)) ([c66de18](https://github.com/cresset-tools/bougie/commit/c66de18e9f02e8ba212821b14046d8078a57f04c))
* **services:** attach to combined log stream on `bougie up` ([#300](https://github.com/cresset-tools/bougie/issues/300)) ([8b90051](https://github.com/cresset-tools/bougie/commit/8b90051146a05ff099c8c84dcaa91c6229dd2723))
* **services:** integrate mailpit SMTP test server ([#408](https://github.com/cresset-tools/bougie/issues/408)) ([54f5682](https://github.com/cresset-tools/bougie/commit/54f5682bdb68a52d2c03f2ec0864647d164d2be6))
* **services:** protocol-aware health checks, at startup and continuously ([#415](https://github.com/cresset-tools/bougie/issues/415)) ([23e26fd](https://github.com/cresset-tools/bougie/commit/23e26fd8858a99af1de1ab080e48661a6fa43074))
* **services:** show service binding in `services status` text output ([#412](https://github.com/cresset-tools/bougie/issues/412)) ([d68ddec](https://github.com/cresset-tools/bougie/commit/d68ddecc6ded532733d0ba3d75ca5810dbe293c1))
* **services:** stream stopping/starting progress on restart ([#335](https://github.com/cresset-tools/bougie/issues/335)) ([9bebd13](https://github.com/cresset-tools/bougie/commit/9bebd136a6e797da374e403b687749d807c281cd))
* **services:** tenant-wired client tools (mysqldump, redis-cli, rabbitmqctl, …) ([#444](https://github.com/cresset-tools/bougie/issues/444)) ([9a7f260](https://github.com/cresset-tools/bougie/commit/9a7f260813a61d0504e5cc340e75847e05df0554))
* **services:** warn on `up` when a service's TCP port is already in use ([#413](https://github.com/cresset-tools/bougie/issues/413)) ([1c520f6](https://github.com/cresset-tools/bougie/commit/1c520f6161aba99f4afdff792ef67a27fa7c7ebc))
* **services:** warn on `up` when env.php DB user != provisioned tenant ([#411](https://github.com/cresset-tools/bougie/issues/411)) ([85f3c6c](https://github.com/cresset-tools/bougie/commit/85f3c6c7f28102d0e0c7e8e3a1f017eb4196f441))
* **services:** write PhpStorm data source on `bougie up` ([#336](https://github.com/cresset-tools/bougie/issues/336)) ([126fec6](https://github.com/cresset-tools/bougie/commit/126fec67856d0e8e4ad39733e2d58d7e14a83642))
* **shim:** default CLI php to memory_limit=-1 (FPM unchanged) ([#292](https://github.com/cresset-tools/bougie/issues/292)) ([94a04b5](https://github.com/cresset-tools/bougie/commit/94a04b55183a16e52e03c970ece95cebe822f69b))
* SIGQUIT activity dump + shared resolver metadata cache ([#295](https://github.com/cresset-tools/bougie/issues/295)) ([fc7c3cb](https://github.com/cresset-tools/bougie/commit/fc7c3cb211b935afc0fb57f790d839f4cd4a51ae))
* **starter:** make the manifest recipe load-bearing (+ detect modulargento) ([#326](https://github.com/cresset-tools/bougie/issues/326)) ([28453bc](https://github.com/cresset-tools/bougie/commit/28453bcd338703f19dcfb07541fa222d5a0865a3))
* **starter:** prompt for per-user placeholder tokens ([#385](https://github.com/cresset-tools/bougie/issues/385)) ([e236910](https://github.com/cresset-tools/bougie/commit/e23691063e54620a0cbb01014f2c6108906e6591))
* **starter:** prompt for private-repo auth secrets (e.g. Hyvä license key) ([#388](https://github.com/cresset-tools/bougie/issues/388)) ([bcb17d4](https://github.com/cresset-tools/bougie/commit/bcb17d48e1e482bf005d8a88f1481937aebde212))
* **sync:** infer PHP version and required extensions ([#178](https://github.com/cresset-tools/bougie/issues/178)) ([3ea9db5](https://github.com/cresset-tools/bougie/commit/3ea9db5204094d33e79172dc76cf2b5539bda627))
* **sync:** one-command install — create lock + vendor, learn PHP/exts from the lock ([#241](https://github.com/cresset-tools/bougie/issues/241)) ([7bd6a21](https://github.com/cresset-tools/bougie/commit/7bd6a21781fe26e422f709a87cc5bafe71458306))
* **sync:** self-heal the Composer repo-config overlay after a vendor wipe ([#487](https://github.com/cresset-tools/bougie/issues/487)) ([f7131ec](https://github.com/cresset-tools/bougie/commit/f7131ec368b0409dc6053737135598bd8cb1f5e8))
* **sync:** uv-style concise summary + skip redundant autoloader dump ([#347](https://github.com/cresset-tools/bougie/issues/347)) ([a845daa](https://github.com/cresset-tools/bougie/commit/a845daaf211dc194a13d65447c771e94df5e865a))
* **target:** detect x86_64-pc-windows-msvc on Windows hosts ([f04a491](https://github.com/cresset-tools/bougie/commit/f04a49126506cf8298858e2de5236fea98e88702))
* **telemetry:** close the plan's schema gaps; retire TELEMETRY_PLAN.md ([#449](https://github.com/cresset-tools/bougie/issues/449)) ([bb0ead3](https://github.com/cresset-tools/bougie/commit/bb0ead38841df03d94d0919ed8ed07242fe508ad))
* **telemetry:** docker images default BOUGIE_TELEMETRY=off (overridable) + Windows flush smoke ([#450](https://github.com/cresset-tools/bougie/issues/450)) ([6e2665b](https://github.com/cresset-tools/bougie/commit/6e2665bcb1a51c66e869ed6e9f98e7fb0cc50d4b))
* **telemetry:** opt-in anonymous telemetry, crash reports, and bougie diagnose ([#447](https://github.com/cresset-tools/bougie/issues/447)) ([8d094f1](https://github.com/cresset-tools/bougie/commit/8d094f19d4a23d2479708099df0081a4ccb84d00))
* **telemetry:** wire the perf fields — download_bytes, cache_hit_pct, autoload_ms ([#451](https://github.com/cresset-tools/bougie/issues/451)) ([acf2d23](https://github.com/cresset-tools/bougie/commit/acf2d23d0d1b59c9bb6ae5e4e22e21a37ddbe7be))
* **tool:** forward bgx/tool-run args after the package without `--` ([#376](https://github.com/cresset-tools/bougie/issues/376)) ([41a99b4](https://github.com/cresset-tools/bougie/commit/41a99b46af45e5a2768e5e4d521ec99e20cf2331))
* **tool:** prefetch + Sigstore-verify native binaries via composer extra ([#467](https://github.com/cresset-tools/bougie/issues/467)) ([5a04bfb](https://github.com/cresset-tools/bougie/commit/5a04bfb4f9a01f64fa29b422636fda5fbc2e45a2))
* **tool:** project-aware bgx runs, derived extensions, CLI memory-limit lift ([#446](https://github.com/cresset-tools/bougie/issues/446)) ([a8fd293](https://github.com/cresset-tools/bougie/commit/a8fd2935c92f100aa230c5949f6c895fea0da90f))
* **tool:** ship `bougie tool` (Phases 1–3) + incremental composer install ([#204](https://github.com/cresset-tools/bougie/issues/204)) ([27bd073](https://github.com/cresset-tools/bougie/commit/27bd073615f03c8ba9f29eef3394e407898e5753))
* top-level `bougie projects` + uv-style `--resolution` ([#381](https://github.com/cresset-tools/bougie/issues/381)) ([09b0d5c](https://github.com/cresset-tools/bougie/commit/09b0d5cb08d0b7da37f2510584cf1e1cb964382b))
* **windows:** native build, phase 1 — CLI surface only ([285d8ca](https://github.com/cresset-tools/bougie/commit/285d8caafb03ce576384ae0070479f79d8a85947))
* **windows:** native build, phase 1 — CLI surface only ([5fa52a6](https://github.com/cresset-tools/bougie/commit/5fa52a647302d2dd72a95d0161d67c4b67303551))
* **windows:** native Windows support via windows.php.net ([d127917](https://github.com/cresset-tools/bougie/commit/d1279177b3f1a6d81597d4711fc7147916114838))
* **windows:** PHP_INI_SCAN_DIR separator + windows-latest CI (phase 6) ([57495ad](https://github.com/cresset-tools/bougie/commit/57495ad66905b7abe80c3778a1c115fb6ff93527))
* **windows:** PHP_INI_SCAN_DIR separator + windows-latest CI (phase 6) ([e7c5d65](https://github.com/cresset-tools/bougie/commit/e7c5d65287e17e7fe378696a9e4beab73a6be3de))


### Bug Fixes

* **autoloader:** apply krsort to PSR-* emit + classmap scan ([61e7c8c](https://github.com/cresset-tools/bougie/commit/61e7c8ccbd84fadcd6b26912d138f42db37570c5))
* **autoloader:** apply krsort to PSR-* emit + classmap scan ([58ee3b6](https://github.com/cresset-tools/bougie/commit/58ee3b620883658571ec6ee758edb4478fa9b9c2))
* **autoloader:** canonicalize install paths so macOS /var/folders works ([343d4fd](https://github.com/cresset-tools/bougie/commit/343d4fdb3cd93457d082cab54662178507c6b4ab))
* **autoloader:** dump_bench copy tolerates dangling symlinks ([86ddf86](https://github.com/cresset-tools/bougie/commit/86ddf866660c95392b10c67c951e9f3a52fc95e9))
* **autoloader:** dump_bench example must not mutate the target tree ([7d61d15](https://github.com/cresset-tools/bougie/commit/7d61d1515b953c453d233f4c5cefeb51ffa0c1f5))
* **autoloader:** emit files autoload in topological order ([9b7750f](https://github.com/cresset-tools/bougie/commit/9b7750fdcf3f59377238bf700b5c109210c5cf44))
* **autoloader:** emit files autoload in topological order ([f42d3e0](https://github.com/cresset-tools/bougie/commit/f42d3e090162e7ecb5d6f526627004105d6913d2))
* **autoloader:** emit root package autoload-dev when dev deps are included ([#380](https://github.com/cresset-tools/bougie/issues/380)) ([07eb2a1](https://github.com/cresset-tools/bougie/commit/07eb2a1db9cb3436f1455a482f75860cc6f460db))
* **autoloader:** exclude PSR-fallback volatile roots from the classmap ([#332](https://github.com/cresset-tools/bougie/issues/332)) ([346463b](https://github.com/cresset-tools/bougie/commit/346463b33009dc5c29b520f82a499eee934ad79b))
* **autoloader:** port reverse-sortPackageMap order for PSR-* + classmap ([5e1597b](https://github.com/cresset-tools/bougie/commit/5e1597b0f9c6cc4ab60902541cd16efaef4876b3))
* **autoloader:** port reverse-sortPackageMap order for PSR-* + classmap ([e9252ac](https://github.com/cresset-tools/bougie/commit/e9252acf89584c8cabb75015b5abb4d2ac3b62ac))
* **autoloader:** route empty PSR-0/PSR-4 prefixes to fallback dirs ([c0caa01](https://github.com/cresset-tools/bougie/commit/c0caa01e323076bfb040c80bd0ce46c29d13ba3f))
* **autoloader:** route empty PSR-0/PSR-4 prefixes to fallback dirs ([08ed25e](https://github.com/cresset-tools/bougie/commit/08ed25e6f79b2e6a707bece3e2a7b2168bd490b7))
* **autoloader:** vendor-dir auto-exclude on PSR-* scans that span vendor ([d5bfbea](https://github.com/cresset-tools/bougie/commit/d5bfbea851bcc0baf86a81aa0cf8c02190caab0a))
* **autoloader:** vendor-dir auto-exclude on PSR-* scans that span vendor ([aa7b434](https://github.com/cresset-tools/bougie/commit/aa7b434374175fb2c9aedae20a729f3e9b1da3bd))
* **autoloader:** widen PackageSorter weight to i64 to match Composer ([#319](https://github.com/cresset-tools/bougie/issues/319)) ([75fcb2f](https://github.com/cresset-tools/bougie/commit/75fcb2f375aa2934bf0ac4aebd4584620bc415c2))
* **babysit:** don't tear down a healthy service when the sidecar exits benignly ([#291](https://github.com/cresset-tools/bougie/issues/291)) ([cd5bbf9](https://github.com/cresset-tools/bougie/commit/cd5bbf9d4ff80878d08f7b86043ecf2857da0d63))
* **babysit:** SIGKILL the service via PR_SET_PDEATHSIG if the babysit dies abnormally ([#282](https://github.com/cresset-tools/bougie/issues/282)) ([df48680](https://github.com/cresset-tools/bougie/commit/df486804303a1ae8e852ae56aa76852e020ead75))
* **backend:** clearer error for an unsupported host target (musl/Alpine) ([#274](https://github.com/cresset-tools/bougie/issues/274)) ([9c789cb](https://github.com/cresset-tools/bougie/commit/9c789cb66ce42bb513dd91a26356100f87e3db46))
* **baseline:** load openssl + sodium on Windows via conf.d fragments ([037861d](https://github.com/cresset-tools/bougie/commit/037861d161bd021a8610b7ff11772214590b8972))
* **ci:** enable Git LFS checkout in CI and release-plz workflows ([#195](https://github.com/cresset-tools/bougie/issues/195)) ([83a1ad8](https://github.com/cresset-tools/bougie/commit/83a1ad875a02dcee9177da7bc8540f52b562164e))
* **ci:** give intra-workspace path deps a version ([b48be7a](https://github.com/cresset-tools/bougie/commit/b48be7a539bdb69a4aab93ba967264a9e8569e5f))
* **ci:** give intra-workspace path deps a version ([484fbd1](https://github.com/cresset-tools/bougie/commit/484fbd188b0a1680a13bd8246fecd3bafb7fc8e2))
* **cli:** make `bgx --version` work ([#311](https://github.com/cresset-tools/bougie/issues/311)) ([747a54d](https://github.com/cresset-tools/bougie/commit/747a54deb62860b6eb2dfba12b0977cd2f7724b8))
* **composer-install:** accept empty dist.shasum (match Composer) ([#161](https://github.com/cresset-tools/bougie/issues/161)) ([3d2aae5](https://github.com/cresset-tools/bougie/commit/3d2aae5ee0f81b789d90883f419de98e95309ca9))
* **composer-install:** claim Composer/2 UA and reuse shared HTTP client for dist downloads ([#163](https://github.com/cresset-tools/bougie/issues/163)) ([a374d83](https://github.com/cresset-tools/bougie/commit/a374d8352d11e7ecd97c5830e5795b969246ca0a))
* **composer-install:** skip metapackages instead of rejecting them ([#192](https://github.com/cresset-tools/bougie/issues/192)) ([a469aa9](https://github.com/cresset-tools/bougie/commit/a469aa92f22a9d9422690ec2be44496ea2840041))
* **composer-resolver:** accept `{"packagist": false}` BC alias to disable Packagist ([#402](https://github.com/cresset-tools/bougie/issues/402)) ([5940298](https://github.com/cresset-tools/bougie/commit/5940298c5eacba3568a141a6371cef572f365882))
* **composer-resolver:** accept `<name>-dev` constraint as synonym for `dev-<name>` ([#190](https://github.com/cresset-tools/bougie/issues/190)) ([700546a](https://github.com/cresset-tools/bougie/commit/700546a112735b6cd6c4b96958cf1e03c003c18b))
* **composer-resolver:** apply patches before the magento2-base deploy ([#468](https://github.com/cresset-tools/bougie/issues/468)) ([7f3bdec](https://github.com/cresset-tools/bougie/commit/7f3bdecdc3300a4e89da6909b0ee1f5633b57fe2))
* **composer-resolver:** don't let a replaced original's back-edge break the solve ([#317](https://github.com/cresset-tools/bougie/issues/317)) ([abade97](https://github.com/cresset-tools/bougie/commit/abade97fa671249d7be347dc52309c9728871962))
* **composer-resolver:** key repo auth by origin incl. port ([#404](https://github.com/cresset-tools/bougie/issues/404)) ([01e8e9b](https://github.com/cresset-tools/bougie/commit/01e8e9b46710cd248131ccc12dc0c508b98634cf))
* **composer-resolver:** report all resolution problems, not just the first ([#191](https://github.com/cresset-tools/bougie/issues/191)) ([0a2d499](https://github.com/cresset-tools/bougie/commit/0a2d499d3fbf105736d3524b41d33bc5be58e0ef))
* **composer-resolver:** resolve all cross-check divergences ([#184](https://github.com/cresset-tools/bougie/issues/184)) ([acc637b](https://github.com/cresset-tools/bougie/commit/acc637bb61d0adee20f834c08fc56eb20e920509))
* **composer-resolver:** stop reporting satisfied `php` as missing from repos ([#425](https://github.com/cresset-tools/bougie/issues/425)) ([3efac2c](https://github.com/cresset-tools/bougie/commit/3efac2ce46869a0c27f3d2d586531af628661277))
* **composer-resolver:** union repo candidates and multi-provider virtuals ([#135](https://github.com/cresset-tools/bougie/issues/135)) ([126d0c6](https://github.com/cresset-tools/bougie/commit/126d0c6adb17fa2566f76a6aa34fbd763e8ab3fa))
* **composer:** `update` installs vendor/ + `upgrade`/`u` aliases ([#352](https://github.com/cresset-tools/bougie/issues/352)) ([a4ac00e](https://github.com/cresset-tools/bougie/commit/a4ac00e43f8cf4d1e62caa3f9002431d16d23351))
* **composer:** accept the PHP empty-array form in PSR namespace maps ([#489](https://github.com/cresset-tools/bougie/issues/489)) ([59d9269](https://github.com/cresset-tools/bougie/commit/59d926972e75afcae6adcbef65cbf38abaf24004))
* **composer:** Mage-OS resolve fixes — caret ^0, self-replace, fetch retry ([#232](https://github.com/cresset-tools/bougie/issues/232)) ([96cef9e](https://github.com/cresset-tools/bougie/commit/96cef9ec36cb0d15d13f97a47e773f50244532e6))
* **composer:** tolerate PHP empty-array form for empty maps ([#128](https://github.com/cresset-tools/bougie/issues/128)) ([937d691](https://github.com/cresset-tools/bougie/commit/937d691db3bd2a3038db165a789643e29a7d325f))
* **composer:** validate finds require-dev deps hoisted into prod lock section ([#474](https://github.com/cresset-tools/bougie/issues/474)) ([6983608](https://github.com/cresset-tools/bougie/commit/69836082da3ffad938305dcc68355947c5f63bfe))
* **composer:** warn instead of erroring on stale composer.lock ([#304](https://github.com/cresset-tools/bougie/issues/304)) ([0a9bfcf](https://github.com/cresset-tools/bougie/commit/0a9bfcff4a1efa72a5111001d18d8f81c43fbf87))
* **conf_d:** quote extension= paths on Windows to survive `~` in 8.3 names ([08b8b02](https://github.com/cresset-tools/bougie/commit/08b8b02a07a09b82a636e274b14117421605f2f2))
* **daemon,recipe:** restore services on daemon restart; pin recipe bougie to current exe ([#267](https://github.com/cresset-tools/bougie/issues/267)) ([0988460](https://github.com/cresset-tools/bougie/commit/098846081dcb76b8c59b90b963e14a41df3b6d69))
* **daemon:** allow /tmp + /var/tmp RW so macOS bash heredocs work in sandbox ([#361](https://github.com/cresset-tools/bougie/issues/361)) ([fc8f421](https://github.com/cresset-tools/bougie/commit/fc8f421cd08377d46499e92bbb6f554daa5ce1e6))
* **daemon:** anchor bougied cwd so provisioner probes survive a deleted launch dir ([#289](https://github.com/cresset-tools/bougie/issues/289)) ([a30a83c](https://github.com/cresset-tools/bougie/commit/a30a83c318cf6a0e6dc2ef560f50897abea699c5))
* **daemon:** anchor rabbitmq CWD to its data dir ([#167](https://github.com/cresset-tools/bougie/issues/167)) ([584d156](https://github.com/cresset-tools/bougie/commit/584d1567efa13f69c1e2beac6c00daaef08b4024))
* **daemon:** derive mariadb passwords so they survive down/purge/re-provision ([#287](https://github.com/cresset-tools/bougie/issues/287)) ([4eee91f](https://github.com/cresset-tools/bougie/commit/4eee91fec043795eb58121f479ee9991c50b002d))
* **daemon:** derive rabbitmq passwords too (stable across re-provision) ([#290](https://github.com/cresset-tools/bougie/issues/290)) ([98d1025](https://github.com/cresset-tools/bougie/commit/98d10250c82994b8d9a7d61caf86b9aa359f12a8))
* **daemon:** namespace service cgroups by home so concurrent daemons don't reap each other's services ([#457](https://github.com/cresset-tools/bougie/issues/457)) ([a3d18c3](https://github.com/cresset-tools/bougie/commit/a3d18c3df9ce9e8e1be0b2024bdc297caa7a1de3)), closes [#456](https://github.com/cresset-tools/bougie/issues/456)
* **daemon:** plan the whole tool tree up front so the download bar total is accurate ([#271](https://github.com/cresset-tools/bougie/issues/271)) ([819c1bc](https://github.com/cresset-tools/bougie/commit/819c1bcca456fed1b01d03d992b6e7f5004ad9e4))
* **daemon:** run service health probe off the Supervisor mutex ([#405](https://github.com/cresset-tools/bougie/issues/405)) ([9db5d61](https://github.com/cresset-tools/bougie/commit/9db5d614d5e816c587fb4f26eb7076ced696c730)), closes [#219](https://github.com/cresset-tools/bougie/issues/219)
* **daemon:** set TMPDIR for opensearch so bash heredocs work under macOS sandbox ([#358](https://github.com/cresset-tools/bougie/issues/358)) ([40664e5](https://github.com/cresset-tools/bougie/commit/40664e54881d9048c6bcd1579b38d47f7e38fd75))
* **daemon:** setsid bougied so a terminal Ctrl-C can't kill it ([#339](https://github.com/cresset-tools/bougie/issues/339)) ([6021991](https://github.com/cresset-tools/bougie/commit/6021991736fa4818eddf8c741f8e0d25cccf75ea))
* **dist:** build linux-gnu at glibc 2.17 via custom in-container job ([#357](https://github.com/cresset-tools/bougie/issues/357)) ([6188e00](https://github.com/cresset-tools/bougie/commit/6188e00c4f540510d96d5b459600b63d2abbe63b))
* **dist:** make installers prefer the origin mirror (hosting=[simple,github]) ([#374](https://github.com/cresset-tools/bougie/issues/374)) ([e627774](https://github.com/cresset-tools/bougie/commit/e6277743e7dd82b7996d975f74d5a30b5ee8a2a3))
* don't orphan rabbitmq when bougied gets a foreground Ctrl-C ([#299](https://github.com/cresset-tools/bougie/issues/299)) ([385f4e5](https://github.com/cresset-tools/bougie/commit/385f4e5db63dce9afdb8c1adb9a35dd9c180d5bf))
* **errors:** show root cause in network error diagnostics ([#166](https://github.com/cresset-tools/bougie/issues/166)) ([eafe7a2](https://github.com/cresset-tools/bougie/commit/eafe7a22e0ca502c624ae57aece124b27739cbbd))
* **ext:** canonicalise on-disk basename for local .so installs ([24d4164](https://github.com/cresset-tools/bougie/commit/24d4164e30db1b1f839f3f3e3ec13b320c4fe1f8))
* **fetch:** add stall timeout, retries with backoff, and extraction progress ([#270](https://github.com/cresset-tools/bougie/issues/270)) ([8245965](https://github.com/cresset-tools/bougie/commit/824596539b43551d0d3659a2d503af75b623c442))
* **fetch:** build the step bar with its draw target to stop a stranded frame ([#303](https://github.com/cresset-tools/bougie/issues/303)) ([bc97eb8](https://github.com/cresset-tools/bougie/commit/bc97eb8e9de5ec818e4cc13e93927fec35330ed0))
* **fetch:** gate test-only DownloadBar::planned to non-Windows ([71d0ccc](https://github.com/cresset-tools/bougie/commit/71d0ccc186d9ec52129f1ddef70e9dfd81e7e63f))
* **fetch:** hide DownloadBar until first real progress event ([#173](https://github.com/cresset-tools/bougie/issues/173)) ([6aaebd0](https://github.com/cresset-tools/bougie/commit/6aaebd00ed3e146bf4ff840cae31c8dc43c10bd9))
* **format:** pin wick 0.2.1 (0.2.0 shipped no binaries) ([#370](https://github.com/cresset-tools/bougie/issues/370)) ([f8808c5](https://github.com/cresset-tools/bougie/commit/f8808c525a1b31b0ebe2ee316d7339729b9bafea))
* **format:** pin wick 0.2.3 ([#375](https://github.com/cresset-tools/bougie/issues/375)) ([55c73dc](https://github.com/cresset-tools/bougie/commit/55c73dc4a7306f3d03a226c28f6479fe94478a08))
* four tier-1 correctness/safety bugs (extraction, autoloader, cache key, license) ([#432](https://github.com/cresset-tools/bougie/issues/432)) ([9a7c619](https://github.com/cresset-tools/bougie/commit/9a7c619209b1a200f3738e4d9c75c0ab15a8fffa))
* **index:** accept Nix-base32 closure hashes in wire validator ([3a47b15](https://github.com/cresset-tools/bougie/commit/3a47b15259c9a5823d4766b45037620bb14a79c9))
* **index:** accept Nix-base32 closure hashes in wire validator ([1748c7b](https://github.com/cresset-tools/bougie/commit/1748c7b1cf5baaa03dabfac6adf103bee4844900))
* **init:** pass ResolutionStrategy to resolve_and_write_lock ([#387](https://github.com/cresset-tools/bougie/issues/387)) ([1d60930](https://github.com/cresset-tools/bougie/commit/1d60930d7f990bbc279f08295085e930e5729802))
* **installer:** don't flash a progress bar when baseline extensions are all installed ([#334](https://github.com/cresset-tools/bougie/issues/334)) ([630a568](https://github.com/cresset-tools/bougie/commit/630a568f47c81aa44e99f9bb3f8b0b77a5209650))
* **installer:** skip opcache baseline install on PHP 8.5+ ([#159](https://github.com/cresset-tools/bougie/issues/159)) ([4de7312](https://github.com/cresset-tools/bougie/commit/4de7312b687f9551530fa4b252672beb6c2757a2))
* **install:** re-import ArchiveKind for the unix-only closure path ([d26d8ba](https://github.com/cresset-tools/bougie/commit/d26d8baf298b2ad9cb933f91c355337d0a9d736a))
* **patches:** declared patch files are skipped by the patches/ scan ([#470](https://github.com/cresset-tools/bougie/issues/470)) ([5e91f4f](https://github.com/cresset-tools/bougie/commit/5e91f4f2cd2b5efdca79a057a765477eb8dcc383))
* **php:** auto-sync on `php pin` and reconcile conf.d for system PHP ([#391](https://github.com/cresset-tools/bougie/issues/391)) ([ef2681b](https://github.com/cresset-tools/bougie/commit/ef2681b12dd8e949915181b1ec9a2ee37835302d))
* **recipe:** Mage-OS one-command bring-up — detect mage-os, redis-over-socket, lock re-stamp ([#251](https://github.com/cresset-tools/bougie/issues/251)) ([4d29004](https://github.com/cresset-tools/bougie/commit/4d2900418697defb4bc17ecfcac98c498b31b784))
* **recipe:** pin bougie on PATH for check scripts ([#301](https://github.com/cresset-tools/bougie/issues/301)) ([addbd4e](https://github.com/cresset-tools/bougie/commit/addbd4e75c2ad17c9a2b4a62207b3404d2bad3bd))
* **recipe:** provision server tenant before Magento install ([#170](https://github.com/cresset-tools/bougie/issues/170)) ([38c5ff3](https://github.com/cresset-tools/bougie/commit/38c5ff33b056c253ea9eeb99b6e087185b7e103e))
* **recipe:** skip rabbitmq + amqp wiring when the Amqp module is absent ([#459](https://github.com/cresset-tools/bougie/issues/459)) ([dc6ac52](https://github.com/cresset-tools/bougie/commit/dc6ac5227e2f529c194aa3b18cd413fe44d03e57))
* **release:** allow dirty working directory for LFS-tracked fixtures ([#196](https://github.com/cresset-tools/bougie/issues/196)) ([edd54bd](https://github.com/cresset-tools/bougie/commit/edd54bd9020642d5053ba481ba177069296874fa))
* **release:** allow-dirty = ["ci"] so dist accepts the hand-edited trigger ([#261](https://github.com/cresset-tools/bougie/issues/261)) ([88be819](https://github.com/cresset-tools/bougie/commit/88be81911d47b4ef3a7f86b75a0ca08264ec1850))
* **release:** auto-retry crates-publish past crates.io index lag ([#435](https://github.com/cresset-tools/bougie/issues/435)) ([be788bc](https://github.com/cresset-tools/bougie/commit/be788bc5059e146f639e96ca387cb25b535aed02)), closes [#424](https://github.com/cresset-tools/bougie/issues/424)
* **release:** bump-minor-pre-major so pre-1.0 breaking changes stay pre-major ([#280](https://github.com/cresset-tools/bougie/issues/280)) ([fa36828](https://github.com/cresset-tools/bougie/commit/fa36828b127a1d6c7be418841cd152a25797cd6a))
* **release:** decouple bougie version + centralize workspace dep pins ([#180](https://github.com/cresset-tools/bougie/issues/180)) ([24e13e3](https://github.com/cresset-tools/bougie/commit/24e13e33572a1d6169a4d6cd6c0600eae05861c8))
* **release:** inherit workspace.package.version across all bougie-* crates ([#143](https://github.com/cresset-tools/bougie/issues/143)) ([c63dc75](https://github.com/cresset-tools/bougie/commit/c63dc75b30d4caf19e4ac9fbe24dec730ae32892))
* **release:** install LFS system-wide with --skip-smudge for release-plz ([#200](https://github.com/cresset-tools/bougie/issues/200)) ([f812055](https://github.com/cresset-tools/bougie/commit/f812055857ae660fe56b46f7aec381e24fdd58f1))
* **release:** jq key-access syntax for release-please-manifest ([#242](https://github.com/cresset-tools/bougie/issues/242)) ([7c0a5f4](https://github.com/cresset-tools/bougie/commit/7c0a5f408980e3cbc3962ff8208476c393c6863e))
* **release:** let dist own the GitHub Release; release-please pushes tag only ([#238](https://github.com/cresset-tools/bougie/issues/238)) ([55ef8e5](https://github.com/cresset-tools/bougie/commit/55ef8e5d30a1d7e4bd2c5e79051a101c9973e135))
* **release:** let release-please own the draft GitHub Release ([#245](https://github.com/cresset-tools/bougie/issues/245)) ([6b8ce18](https://github.com/cresset-tools/bougie/commit/6b8ce18395186d66963f96a2bb7e3056d2a9b0fe))
* **release:** let release-please own the whole release; dist only uploads ([#260](https://github.com/cresset-tools/bougie/issues/260)) ([d404a11](https://github.com/cresset-tools/bougie/commit/d404a116e113e574a7d137f0e171d8057d6665b0))
* **release:** make release-please actually rewrite Cargo.toml ([#237](https://github.com/cresset-tools/bougie/issues/237)) ([ca40f63](https://github.com/cresset-tools/bougie/commit/ca40f63e432c7ddae1c491db0123fc8101ce1143))
* **release:** move release-tag push into its own isolated job ([#253](https://github.com/cresset-tools/bougie/issues/253)) ([1570fc1](https://github.com/cresset-tools/bougie/commit/1570fc1e8d041cf82f305ee2818ff177371b08c1))
* **release:** neutralize LFS smudge/process filter for release-plz ([#199](https://github.com/cresset-tools/bougie/issues/199)) ([b108d3f](https://github.com/cresset-tools/bougie/commit/b108d3f151dee4e93a21a3f333fa9bbbed3accc3))
* **release:** push the release tag (draft Releases don't auto-tag) ([#249](https://github.com/cresset-tools/bougie/issues/249)) ([469ee13](https://github.com/cresset-tools/bougie/commit/469ee1373c5b22b3b35e5336dc907b14138a57a9))
* **release:** re-pin version on intra-workspace path deps ([#148](https://github.com/cresset-tools/bougie/issues/148)) ([2bcc1e4](https://github.com/cresset-tools/bougie/commit/2bcc1e4d0d1f2086921273850e6fd2ea5071c1a0))
* **release:** skip LFS smudge in release-plz workflow ([#197](https://github.com/cresset-tools/bougie/issues/197)) ([6e1bcda](https://github.com/cresset-tools/bougie/commit/6e1bcda7f071425f54df889afb906734975b353c))
* **release:** sudo for system-wide LFS install ([#201](https://github.com/cresset-tools/bougie/issues/201)) ([925ad52](https://github.com/cresset-tools/bougie/commit/925ad52ba6f82acb43018ad69dce2bb010debfad))
* **release:** suppress the candidate PR on release-merge runs ([#258](https://github.com/cresset-tools/bougie/issues/258)) ([ed025ce](https://github.com/cresset-tools/bougie/commit/ed025ce8be971a25fba71b361f8f03af5d3fe8d9))
* **release:** un-LFS the magento2 fixture, scope LFS to cross-check only ([#202](https://github.com/cresset-tools/bougie/issues/202)) ([89119e4](https://github.com/cresset-tools/bougie/commit/89119e4066ec206b8c8e9f92afbfb630de51be86))
* **release:** unblock musl + windows dist targets ([#233](https://github.com/cresset-tools/bougie/issues/233)) ([87705a9](https://github.com/cresset-tools/bougie/commit/87705a9ec70115f857bb84d9daa827dde5e58f15))
* **release:** uninstall LFS filter before running release-plz ([#198](https://github.com/cresset-tools/bougie/issues/198)) ([93b406f](https://github.com/cresset-tools/bougie/commit/93b406fc114a2425332cddb73fe9f900d3270bff))
* resolve whole-project review findings ([#207](https://github.com/cresset-tools/bougie/issues/207)–[#231](https://github.com/cresset-tools/bougie/issues/231)) ([#234](https://github.com/cresset-tools/bougie/issues/234)) ([4f873e9](https://github.com/cresset-tools/bougie/commit/4f873e95dd96e62f4423b8cd0fe0f1a369038aab))
* **resolver:** honor root composer.json wildcard `replace` ([#269](https://github.com/cresset-tools/bougie/issues/269)) ([e14720d](https://github.com/cresset-tools/bougie/commit/e14720d6fa17113b1849d600209d5652c8f900f3))
* **resolver:** validate php platform requires against the pinned PHP ([#322](https://github.com/cresset-tools/bougie/issues/322)) ([007d1db](https://github.com/cresset-tools/bougie/commit/007d1dbab36fcdbf8863b837833a0a47c68a41fc)), closes [#118](https://github.com/cresset-tools/bougie/issues/118)
* **run:** walk up to project root, not cwd ([#176](https://github.com/cresset-tools/bougie/issues/176)) ([7eec606](https://github.com/cresset-tools/bougie/commit/7eec60673320e28de3245db3e52d25f140c67fef))
* **sandbox-run:** narrow ProtectHome read-deny to file-read-data on macOS ([39ae59e](https://github.com/cresset-tools/bougie/commit/39ae59eaa521d922d46d2822a73033f9a8a2eec2))
* **sandbox:** enforce ProtectHome/inaccessible/read-only paths on Linux ([#208](https://github.com/cresset-tools/bougie/issues/208)) ([#316](https://github.com/cresset-tools/bougie/issues/316)) ([ff885a5](https://github.com/cresset-tools/bougie/commit/ff885a5aee6d2d773f06140e210d110ad404b30d))
* **self-update:** don't update to a release with no assets yet ([#331](https://github.com/cresset-tools/bougie/issues/331)) ([9664f49](https://github.com/cresset-tools/bougie/commit/9664f49a5b4960207d6f818536d79762a0026053))
* **self-update:** fall back to newest release with assets for the target ([#356](https://github.com/cresset-tools/bougie/issues/356)) ([47a1df2](https://github.com/cresset-tools/bougie/commit/47a1df2fc07da138605d8c7d40a987b25c40efec))
* **server:** drop orphaned home_from_passwd tests ([9ba083c](https://github.com/cresset-tools/bougie/commit/9ba083c938be99b6d23f95a8b90761867efcd826))
* **server:** keep generated/ classmap entries fresh instead of dangling ([#288](https://github.com/cresset-tools/bougie/issues/288)) ([dcbc3be](https://github.com/cresset-tools/bougie/commit/dcbc3be5cd9aa69316a07eb4b81ad3958365a188))
* **server:** respawn dead FPM pools instead of dispatching to a vanished socket ([#340](https://github.com/cresset-tools/bougie/issues/340)) ([24878fb](https://github.com/cresset-tools/bougie/commit/24878fb4e4cf895a8f3914cad6cb0bc3ffe75ece))
* **server:** self-escalate hosts apply instead of hinting sudo bougie ([#466](https://github.com/cresset-tools/bougie/issues/466)) ([b6ef2ac](https://github.com/cresset-tools/bougie/commit/b6ef2acf1bba96acdbd10de5870193946afb5999))
* **server:** serve on-disk static assets before the front-controller rewrite ([#281](https://github.com/cresset-tools/bougie/issues/281)) ([43e4cd5](https://github.com/cresset-tools/bougie/commit/43e4cd585977003d3250a4008b5410e30572db8a))
* **server:** surface php-fpm startup errors and stop orphaning workers ([#438](https://github.com/cresset-tools/bougie/issues/438)) ([6bd9701](https://github.com/cresset-tools/bougie/commit/6bd9701a991cd3dea301bf97faeaf87c8a43c8ba))
* **server:** tolerate slow opcache.preload at pool spawn ([#437](https://github.com/cresset-tools/bougie/issues/437)) ([61cc936](https://github.com/cresset-tools/bougie/commit/61cc936fd825a722cc0a528c8be26b4a8475010d))
* **services:** capture bougied stderr to state/bougied.log ([#455](https://github.com/cresset-tools/bougie/issues/455)) ([2bef601](https://github.com/cresset-tools/bougie/commit/2bef60130672ffe2b94e153b04c45d9c700d2f91))
* **services:** derive tenant from project dir, not composer name, to stop collisions ([#321](https://github.com/cresset-tools/bougie/issues/321)) ([32959c3](https://github.com/cresset-tools/bougie/commit/32959c3e2ca53e87e2a21ff747b79ec303080ce8))
* **services:** persist tenant name so it can't drift across down/purge ([#407](https://github.com/cresset-tools/bougie/issues/407)) ([046521c](https://github.com/cresset-tools/bougie/commit/046521c2e6e276f42a5246ff1b916eb04b1f5667))
* **shim:** strip .exe case-insensitively so unzip.EXE detects as Unzip role ([e26973c](https://github.com/cresset-tools/bougie/commit/e26973c0c486f7f6a72673e59a894b769c3e9c0e))
* **starter:** make `--starter laravel` work end-to-end ([#460](https://github.com/cresset-tools/bougie/issues/460)) ([e1442e7](https://github.com/cresset-tools/bougie/commit/e1442e7847482b7a4b002dee60ce64ec30c016b6))
* **sync:** accept Composer wildcards in require.php ([#106](https://github.com/cresset-tools/bougie/issues/106)) ([#150](https://github.com/cresset-tools/bougie/issues/150)) ([70d1c35](https://github.com/cresset-tools/bougie/commit/70d1c35d07066a08e07c0aed075236296c166c89))
* **sync:** drop stale composer-write fragments when an ext joins the baseline ([f3905cc](https://github.com/cresset-tools/bougie/commit/f3905cc9220666bdc5335e77f95653f435acfe11))
* **sync:** drop stale composer-write fragments when an ext joins the baseline ([73e6c99](https://github.com/cresset-tools/bougie/commit/73e6c99967ea9a1835e0630b831979b8010f20bd))
* **sync:** re-resolve stale extensions after the active interpreter changes ([#379](https://github.com/cresset-tools/bougie/issues/379)) ([4b24785](https://github.com/cresset-tools/bougie/commit/4b24785311d2517e7a673e79d7e0ed6ac529838b))
* **sync:** refresh staged shim on mtime change, not size alone ([0c989e6](https://github.com/cresset-tools/bougie/commit/0c989e6a1082ce172b5ff473bf91c4c1847ad39d))
* **sync:** stage local copy of bougie.exe for cross-volume NTFS shims ([0a9e8ab](https://github.com/cresset-tools/bougie/commit/0a9e8ab2428bfcbe557948eb7e8772afbebd2b42))
* **sync:** sync the discovered project root, not the cwd ([#418](https://github.com/cresset-tools/bougie/issues/418)) ([faab201](https://github.com/cresset-tools/bougie/commit/faab201286b23df6e812c8630a7fbf9ecc6770af))
* **sync:** update fragment_name test after mbstring joined baseline ([04917e1](https://github.com/cresset-tools/bougie/commit/04917e1145f33f6f29db14dda76a42235f6d6bb6))
* **target:** gate libc helpers on target_os=linux, not unix ([ace310c](https://github.com/cresset-tools/bougie/commit/ace310cbcc8071d748e3d894de7c9c4f466b4fc2))
* **tests:** pass --config to server list calls in integration tests ([b8f8833](https://github.com/cresset-tools/bougie/commit/b8f8833bf49c080cf030f325fdae5f4ece4c02ca))
* **tests:** retarget phase9 binary-install tests to `composer fetch` ([aa4d240](https://github.com/cresset-tools/bougie/commit/aa4d240f2edca37aaf8d448f71b6675f9121e2d7))
* tier-2 service-supervision correctness (grace window, restart, rotation, sandbox, flock) ([#436](https://github.com/cresset-tools/bougie/issues/436)) ([d1b2abe](https://github.com/cresset-tools/bougie/commit/d1b2abecb43f33cc48ffda01530b69f2c77d4bff))
* **tool:** forward leading tool args past clap in tool-exec ([#465](https://github.com/cresset-tools/bougie/issues/465)) ([a92fca7](https://github.com/cresset-tools/bougie/commit/a92fca7fd27485564b1019015e5fcadd4c5a28be))
* **tree:** dedupe shared subtrees so `bougie tree` can't hang ([#399](https://github.com/cresset-tools/bougie/issues/399)) ([d2a2bff](https://github.com/cresset-tools/bougie/commit/d2a2bff2091e52bfc1437eefe3f10241d628fb7e))
* **version:** parse `~8.3` as a tilde-range constraint, not a path ([#177](https://github.com/cresset-tools/bougie/issues/177)) ([f5770d7](https://github.com/cresset-tools/bougie/commit/f5770d7be97876d836ed9cbb63bb3b6a3413d3fe))
* **windows:** parse releases.json size string into bytes ([4cc5d19](https://github.com/cresset-tools/bougie/commit/4cc5d19133e40ec1066b43a9c5f753d911357578))
* **windows:** parse releases.json size string into bytes ([71e6dfa](https://github.com/cresset-tools/bougie/commit/71e6dfa78ae0b9afa1ce41492927323d303a3036))


### Performance Improvements

* **autoloader:** parallelize classmap scan + bench harness ([b6c7480](https://github.com/cresset-tools/bougie/commit/b6c7480ac444e4010bf6a8a1c59c9bf3622275ee))
* **autoloader:** parallelize classmap scan + bench harness ([60a9aa7](https://github.com/cresset-tools/bougie/commit/60a9aa764c60a97938cb83e4ae2e967294a87916))
* **composer-resolver:** hand pubgrub a Ref instead of cloning versions_for ([#140](https://github.com/cresset-tools/bougie/issues/140)) ([c602264](https://github.com/cresset-tools/bougie/commit/c602264733fd7766f37ad8a0192f0e5752055b40))
* **composer-resolver:** mem::forget provider + hoist virtual computation into workers ([#142](https://github.com/cresset-tools/bougie/issues/142)) ([836b450](https://github.com/cresset-tools/bougie/commit/836b4500991365861ce816e06595960a719a9744))
* **composer-resolver:** uv-style improvements (PRs 0-4) ([#157](https://github.com/cresset-tools/bougie/issues/157)) ([7600b0b](https://github.com/cresset-tools/bougie/commit/7600b0bcb8102389368f46fa500e69accb9c7b25))
* **installer:** fetch the index root once per sync, not per extension ([#345](https://github.com/cresset-tools/bougie/issues/345)) ([68c6b7b](https://github.com/cresset-tools/bougie/commit/68c6b7bf8df2fcedc72df4ec8ccc017a9de84e71))

## [0.47.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.46.1...bougie-v0.47.0) (2026-07-09)


### Features

* **errors:** categorize command failures — chain-walk + typed no-project/config/service ([#485](https://github.com/cresset-tools/bougie/issues/485)) ([0f79c15](https://github.com/cresset-tools/bougie/commit/0f79c151))

## [0.46.1](https://github.com/cresset-tools/bougie/compare/bougie-v0.46.0...bougie-v0.46.1) (2026-07-07)


### Bug Fixes

* **composer:** validate finds require-dev deps hoisted into prod lock section ([#474](https://github.com/cresset-tools/bougie/issues/474)) ([6983608](https://github.com/cresset-tools/bougie/commit/69836082da3ffad938305dcc68355947c5f63bfe))

## [0.46.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.45.0...bougie-v0.46.0) (2026-07-07)


### Features

* **diagnose:** service-aware reports reviewed in $EDITOR, markdown wire v2 ([#472](https://github.com/cresset-tools/bougie/issues/472)) ([68323cc](https://github.com/cresset-tools/bougie/commit/68323ccd1af802fd350610e4099d2a8839e3c5d2))

## [0.45.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.44.0...bougie-v0.45.0) (2026-07-06)


### Features

* **run:** lift CLI memory_limit to -1 for project PHP runs ([#471](https://github.com/cresset-tools/bougie/issues/471)) ([c813ab0](https://github.com/cresset-tools/bougie/commit/c813ab0ccc34156327ae612d2fb2bfeb6abf9acc))


### Bug Fixes

* **composer-resolver:** apply patches before the magento2-base deploy ([#468](https://github.com/cresset-tools/bougie/issues/468)) ([7f3bdec](https://github.com/cresset-tools/bougie/commit/7f3bdecdc3300a4e89da6909b0ee1f5633b57fe2))
* **patches:** declared patch files are skipped by the patches/ scan ([#470](https://github.com/cresset-tools/bougie/issues/470)) ([5e91f4f](https://github.com/cresset-tools/bougie/commit/5e91f4f2cd2b5efdca79a057a765477eb8dcc383))

## [0.44.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.43.2...bougie-v0.44.0) (2026-07-06)


### Features

* **server:** warn when DNS blocks *.bougie.run loopback answers ([#464](https://github.com/cresset-tools/bougie/issues/464)) ([5de12e2](https://github.com/cresset-tools/bougie/commit/5de12e2408d3180be13cc4ca39f13fc336728c71))
* **service:** credentials subcommand for tenant connection info ([#462](https://github.com/cresset-tools/bougie/issues/462)) ([b3aca75](https://github.com/cresset-tools/bougie/commit/b3aca7518764c9d3bcb4351a28b87c3104f6eb1a))
* **tool:** prefetch + Sigstore-verify native binaries via composer extra ([#467](https://github.com/cresset-tools/bougie/issues/467)) ([5a04bfb](https://github.com/cresset-tools/bougie/commit/5a04bfb4f9a01f64fa29b422636fda5fbc2e45a2))


### Bug Fixes

* **server:** self-escalate hosts apply instead of hinting sudo bougie ([#466](https://github.com/cresset-tools/bougie/issues/466)) ([b6ef2ac](https://github.com/cresset-tools/bougie/commit/b6ef2acf1bba96acdbd10de5870193946afb5999))
* **tool:** forward leading tool args past clap in tool-exec ([#465](https://github.com/cresset-tools/bougie/issues/465)) ([a92fca7](https://github.com/cresset-tools/bougie/commit/a92fca7fd27485564b1019015e5fcadd4c5a28be))

## [0.43.2](https://github.com/cresset-tools/bougie/compare/bougie-v0.43.1...bougie-v0.43.2) (2026-07-05)


### Bug Fixes

* **starter:** make `--starter laravel` work end-to-end ([#460](https://github.com/cresset-tools/bougie/issues/460)) ([e1442e7](https://github.com/cresset-tools/bougie/commit/e1442e7847482b7a4b002dee60ce64ec30c016b6))

## [0.43.1](https://github.com/cresset-tools/bougie/compare/bougie-v0.43.0...bougie-v0.43.1) (2026-07-05)


### Bug Fixes

* **daemon:** namespace service cgroups by home so concurrent daemons don't reap each other's services ([#457](https://github.com/cresset-tools/bougie/issues/457)) ([a3d18c3](https://github.com/cresset-tools/bougie/commit/a3d18c3df9ce9e8e1be0b2024bdc297caa7a1de3)), closes [#456](https://github.com/cresset-tools/bougie/issues/456)
* **recipe:** skip rabbitmq + amqp wiring when the Amqp module is absent ([#459](https://github.com/cresset-tools/bougie/issues/459)) ([dc6ac52](https://github.com/cresset-tools/bougie/commit/dc6ac5227e2f529c194aa3b18cd413fe44d03e57))
* **services:** capture bougied stderr to state/bougied.log ([#455](https://github.com/cresset-tools/bougie/issues/455)) ([2bef601](https://github.com/cresset-tools/bougie/commit/2bef60130672ffe2b94e153b04c45d9c700d2f91))

## [0.43.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.42.0...bougie-v0.43.0) (2026-07-04)


### Features

* **cli:** rename 'bougie services' to 'bougie service' ([#453](https://github.com/cresset-tools/bougie/issues/453)) ([0d7e5d6](https://github.com/cresset-tools/bougie/commit/0d7e5d62928c51a0022f6f7d4e2474b1e199da3b))

## [0.42.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.41.0...bougie-v0.42.0) (2026-07-04)


### Features

* **script:** uv-style single-file PHP scripts with inline dependencies ([#429](https://github.com/cresset-tools/bougie/issues/429)) ([b8ea30a](https://github.com/cresset-tools/bougie/commit/b8ea30a289f897d50812279e988ac4fb9e94b71b))
* **tool:** project-aware bgx runs, derived extensions, CLI memory-limit lift ([#446](https://github.com/cresset-tools/bougie/issues/446)) ([a8fd293](https://github.com/cresset-tools/bougie/commit/a8fd2935c92f100aa230c5949f6c895fea0da90f))

## [0.41.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.40.0...bougie-v0.41.0) (2026-07-04)


### Features

* **telemetry:** close the plan's schema gaps; retire TELEMETRY_PLAN.md ([#449](https://github.com/cresset-tools/bougie/issues/449)) ([bb0ead3](https://github.com/cresset-tools/bougie/commit/bb0ead38841df03d94d0919ed8ed07242fe508ad))
* **telemetry:** docker images default BOUGIE_TELEMETRY=off (overridable) + Windows flush smoke ([#450](https://github.com/cresset-tools/bougie/issues/450)) ([6e2665b](https://github.com/cresset-tools/bougie/commit/6e2665bcb1a51c66e869ed6e9f98e7fb0cc50d4b))
* **telemetry:** opt-in anonymous telemetry, crash reports, and bougie diagnose ([#447](https://github.com/cresset-tools/bougie/issues/447)) ([8d094f1](https://github.com/cresset-tools/bougie/commit/8d094f19d4a23d2479708099df0081a4ccb84d00))
* **telemetry:** wire the perf fields — download_bytes, cache_hit_pct, autoload_ms ([#451](https://github.com/cresset-tools/bougie/issues/451)) ([acf2d23](https://github.com/cresset-tools/bougie/commit/acf2d23d0d1b59c9bb6ae5e4e22e21a37ddbe7be))

## [0.40.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.39.0...bougie-v0.40.0) (2026-07-03)


### Features

* **services:** protocol-aware health checks, at startup and continuously ([#415](https://github.com/cresset-tools/bougie/issues/415)) ([23e26fd](https://github.com/cresset-tools/bougie/commit/23e26fd8858a99af1de1ab080e48661a6fa43074))

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
