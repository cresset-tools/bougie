# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.0.0](https://github.com/cresset-tools/bougie/compare/bougie-v0.8.1...bougie-v1.0.0) (2026-05-31)


### ⚠ BREAKING CHANGES

* **composer-install:** `bougie composer install` no longer exits non-zero when the lockfile declares a Composer plugin or `composer.json` has a non-empty `scripts` section. CI pipelines that relied on that failure must inspect the new `warnings` field or the stderr `warning:` lines.
* **cli:** `bougie composer install <version>` no longer manages Composer phars — that verb is now `bougie composer fetch <version>`. Bare `bougie composer install` (no positional) is the new Composer-convention project install: reads `composer.lock` from CWD (or `-d <dir>`), content-hash-verifies it, downloads dists into `vendor/`, emits `vendor/autoload.php` + `installed.{json,php}`.
* --no-baseline and --baseline-only are gone. Users who scripted them must switch to --bare / --without. The baseline extension set itself changed wholesale — Composer projects that previously got mbstring/curl/intl/zip free now need to declare require.ext-* in composer.json so `bougie sync` materializes them.
* **cli:** promote `services up`/`services down` to top-level `up`/`down`
* **server:** drop `server add/remove`, require --config for `server run`
* bougie's source and binary distributions are now governed by the EUPL-1.2 instead of Apache-2.0 OR MIT.

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
* **autoloader:** exclude-from-classmap + classmap-exclude/mixed fixtures ([63e4bbe](https://github.com/cresset-tools/bougie/commit/63e4bbeafd63781bdf9c53233ba60579442cae2f))
* **autoloader:** exclude-from-classmap + classmap-exclude/mixed fixtures ([ffdb937](https://github.com/cresset-tools/bougie/commit/ffdb93729f2f153945a5d00b960a6dda89676869))
* **autoloader:** full Composer normalize() port ([d23c3a5](https://github.com/cresset-tools/bougie/commit/d23c3a537c43cd86e1a7fd46bb4cfe3b7f9dedb7))
* **autoloader:** full Composer normalize() port ([03c8747](https://github.com/cresset-tools/bougie/commit/03c874784110e18070e1f39261cceedd8fbf2baf))
* **autoloader:** Phase 1 — PSR-4 / PSR-0 / files emitters ([5040d2a](https://github.com/cresset-tools/bougie/commit/5040d2aceb96102bd2f50783de4fd2fbc8bbfe25))
* **autoloader:** Phase 1 — PSR-4 / PSR-0 / files emitters (re-land) ([5477f0f](https://github.com/cresset-tools/bougie/commit/5477f0ff9a6e2bcd653665716034056d21c991dc))
* **autoloader:** surface PSR warnings and Composer-style footer ([#113](https://github.com/cresset-tools/bougie/issues/113)) ([4ab31f1](https://github.com/cresset-tools/bougie/commit/4ab31f1015ab81b4f5be149f85ac8c2522c79251))
* **autoloader:** vendored runtime files (Phase 3 part 1) ([34af086](https://github.com/cresset-tools/bougie/commit/34af086895242fa43614b5962143190a0f672faf))
* **autoloader:** vendored runtime files (Phase 3 part 1) ([fee6e59](https://github.com/cresset-tools/bougie/commit/fee6e592de0327feb826aa0da26bf7447b5712de))
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
* bougied service supervisor (Phases 1-3, redis end-to-end) ([36f79a4](https://github.com/cresset-tools/bougie/commit/36f79a4c6f0df2c7f556a9ab35875e0c12051acd))
* **cli:** bougie composer dump-autoloader ([6b92845](https://github.com/cresset-tools/bougie/commit/6b92845cf30ced30f9ae1325d7035075424b9d2d))
* **cli:** bougie composer dump-autoloader ([c0fa870](https://github.com/cresset-tools/bougie/commit/c0fa8705c626757a847b3ee2209a427699a809b6))
* **cli:** bougie composer install (project install) + Composer fetch rename ([54225e1](https://github.com/cresset-tools/bougie/commit/54225e196827f724421a2505176b99044ecc35d4))
* **cli:** bougie composer install / fetch rename ([1caa56a](https://github.com/cresset-tools/bougie/commit/1caa56aed41aa23a83ecc100848ed39f4198a634))
* **cli:** bougie composer update --dry-run ([#117](https://github.com/cresset-tools/bougie/issues/117)) ([1568377](https://github.com/cresset-tools/bougie/commit/15683776067c9a0f0a2b5e4e8850e0b0cd006df1))
* **cli:** composer install falls back to resolve when composer.lock is missing ([#132](https://github.com/cresset-tools/bougie/issues/132)) ([e39e195](https://github.com/cresset-tools/bougie/commit/e39e195b567407f72a7f13d55ac50a63f9c0a4e5))
* **cli:** implement bougie composer validate ([#189](https://github.com/cresset-tools/bougie/issues/189)) ([08fad82](https://github.com/cresset-tools/bougie/commit/08fad823fb80bb474791cfd91443f322faffc84a))
* **cli:** promote `services up`/`services down` to top-level `up`/`down` ([78316a2](https://github.com/cresset-tools/bougie/commit/78316a228631381f1f024f761ae2ab004aed0868))
* **cli:** unify list commands with shared coloured renderer ([a918aa2](https://github.com/cresset-tools/bougie/commit/a918aa266eeff5e0e43c1eed12d1d4de7ecce788))
* **cli:** unify list commands with shared coloured renderer ([6fa9c2d](https://github.com/cresset-tools/bougie/commit/6fa9c2d54fbaa38c04d684b10411d69ae8dd9d27))
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
* **composer-resolver:** support Composer v1 repositories ([#133](https://github.com/cresset-tools/bougie/issues/133)) ([79b2829](https://github.com/cresset-tools/bougie/commit/79b2829526cf101a4a4656a370f409769e261741))
* **composer-resolver:** support composer.json `repositories` field (composer-type) ([#130](https://github.com/cresset-tools/bougie/issues/130)) ([04c996a](https://github.com/cresset-tools/bougie/commit/04c996a8cab6e0cc13e6164b175c692f1760a854))
* **composer-resolver:** virtual packages via provide/replace pre-fetch ([#124](https://github.com/cresset-tools/bougie/issues/124)) ([eb8ee13](https://github.com/cresset-tools/bougie/commit/eb8ee1356e54706257c63201614c1ee57a6b753d))
* **composer-resolver:** wildcard replace/provide via on-demand synthesis ([#127](https://github.com/cresset-tools/bougie/issues/127)) ([2b2c0b1](https://github.com/cresset-tools/bougie/commit/2b2c0b1bd0c0da370cb2805de9cd683bb86f992e))
* **composer-resolver:** write composer.lock from bougie composer update ([#123](https://github.com/cresset-tools/bougie/issues/123)) ([2a355cb](https://github.com/cresset-tools/bougie/commit/2a355cbc75f544c1fbfe4041e881cd7062e9fc6e))
* **composer:** add lts channel as a version request ([8f28290](https://github.com/cresset-tools/bougie/commit/8f282904d64f3b19e7d6db18e0cf6be110291636))
* **composer:** typed composer.lock reader ([54e3921](https://github.com/cresset-tools/bougie/commit/54e3921f1f1abff4544e61092b11303b977fd5fe))
* **daemon:** bougied entry point + JSON IPC dispatcher ([2c38a72](https://github.com/cresset-tools/bougie/commit/2c38a7209fd70e466dcfa13de043ed4a2a186982))
* **daemon:** export BOUGIE_SERVICE_*_HOST and _PORT for TCP services ([5d318b9](https://github.com/cresset-tools/bougie/commit/5d318b9b6675772c9694ec66d9ef1220fedfaceb))
* **daemon:** recursively install requires_tools[] inner tools ([06ddd69](https://github.com/cresset-tools/bougie/commit/06ddd6971fda38ab3fa1c396221c03c8ddade065))
* **daemon:** stream tarball download progress to the CLI ([#169](https://github.com/cresset-tools/bougie/issues/169)) ([77ead93](https://github.com/cresset-tools/bougie/commit/77ead93ee57b3bcdcc9f603100b3e9bdbae71a73))
* **daemon:** supervisor state machine, sandbox compilation, tenants ledger ([e41c1d5](https://github.com/cresset-tools/bougie/commit/e41c1d5c289bec671a5ddd4cce5d4595c3940b56))
* **daemon:** vendor sandbox-run + wire bougied shim role and paths ([8fc3495](https://github.com/cresset-tools/bougie/commit/8fc34954d08f99f7aa0e17da8b6f240b3025e4ed))
* **daemon:** walk closure[] when auto-fetching tool tarballs ([9f896ff](https://github.com/cresset-tools/bougie/commit/9f896ffd021e6374717f3d9d3dcb653baa072a67))
* **daemon:** warn on catalog vs requires_tools drift ([44f279a](https://github.com/cresset-tools/bougie/commit/44f279a6d12c701febcf0548f6c52c781748bb39))
* **extensions:** Add curl to the baseline ([70a28d0](https://github.com/cresset-tools/bougie/commit/70a28d0c5dcf9b09ea7bb78e9e08e3492f6b51ef))
* **ext:** Mach-O parser for macOS PHP extensions ([5ca89c8](https://github.com/cresset-tools/bougie/commit/5ca89c89112373eb9e256078a5bd2d7b7334b2b2))
* **ext:** support local .so installs via `bougie ext add <path>` ([08428cf](https://github.com/cresset-tools/bougie/commit/08428cf61f61d075fcd4a6ca9edf35237d7c6da8))
* **ext:** support local .so installs via `bougie ext add <path>` ([58b7eb5](https://github.com/cresset-tools/bougie/commit/58b7eb55d7a00bd3d64f8e9b270d5b24af80e3c6))
* **fetch:** detect_zip_top_level + DistRequest auto-detect ([3c65f50](https://github.com/cresset-tools/bougie/commit/3c65f5041334dcd8ab3117b01be5376dbe030414))
* **fetch:** shared bougie/&lt;v&gt; User-Agent across all outbound HTTP ([#116](https://github.com/cresset-tools/bougie/issues/116)) ([5f01a22](https://github.com/cresset-tools/bougie/commit/5f01a22408f21433bf4b65e0a357a9d1dd86a542))
* **fetch:** switch on ArchiveKind { TarZst, Zip } in extract path ([2aa6891](https://github.com/cresset-tools/bougie/commit/2aa689177b32ce989803ac90ba94a1d3a614a45f))
* **fetch:** unify downloads under a single DownloadBar ([5220846](https://github.com/cresset-tools/bougie/commit/5220846a2ffbeed562644b1d6f6cc2d709cd1fb3))
* **fetch:** unify downloads under a single DownloadBar ([c3a4b3b](https://github.com/cresset-tools/bougie/commit/c3a4b3b1ced85e468caa68245f86ddc076acedb3))
* implement UNBUNDLE_PLAN.md (phases 0-4) ([b930571](https://github.com/cresset-tools/bougie/commit/b930571eb50491102166024425135121928f3982))
* **index:** add requires_tools to manifest schema ([ffcf05a](https://github.com/cresset-tools/bougie/commit/ffcf05a14981af3604554f6e699e4b02839aab90))
* **installer:** baseline mbstring, pdo_sqlite, sqlite3 for Composer parity ([cdb5c51](https://github.com/cresset-tools/bougie/commit/cdb5c514a94e03f0122bae1c5a1c61572cc783f7))
* **installers:** native Composer install-plugin support (Magento, composer/installers, Laravel) ([#248](https://github.com/cresset-tools/bougie/issues/248)) ([ebdf9c3](https://github.com/cresset-tools/bougie/commit/ebdf9c31be080a26ce00196c5b4ceefb27b5599e))
* **install:** use bundled DLLs for Windows baseline extensions ([3a3d1c3](https://github.com/cresset-tools/bougie/commit/3a3d1c3b129fadfc801292c2ba424a9b836a4a8e))
* **install:** use bundled DLLs for Windows baseline extensions ([d92d590](https://github.com/cresset-tools/bougie/commit/d92d590c1f5780836490a539b7cef69f649dc701))
* **paths:** anchor windows home + cache under %LOCALAPPDATA% ([2b1827a](https://github.com/cresset-tools/bougie/commit/2b1827a7d4565fc522522a58dab3c1da28b9bfea))
* **paths:** split Windows home into roaming state + local downloads ([5899ade](https://github.com/cresset-tools/bougie/commit/5899ade19ca50aecf6c7fa4bd10a58d66c6186b6))
* **paths:** split Windows home into roaming state + local downloads ([750eaf4](https://github.com/cresset-tools/bougie/commit/750eaf4b575ef148d94f7ce2fb748ca9a293ae7f))
* **recipe/magento:** wire Redis into setup:install ([#174](https://github.com/cresset-tools/bougie/issues/174)) ([47a2614](https://github.com/cresset-tools/bougie/commit/47a2614db93313706918059602e1bd87ba3e6dd2))
* **recipe:** bougie start with DAG-based recipe engine ([bc3ce6e](https://github.com/cresset-tools/bougie/commit/bc3ce6ea81613471364da1bd9beb3a74e8752075))
* **recipe:** print URL and admin creds after `bougie start` ([#171](https://github.com/cresset-tools/bougie/issues/171)) ([8f3f2c8](https://github.com/cresset-tools/bougie/commit/8f3f2c851f85a4e77a73ddc6858a969d2f03679c))
* **release:** ship dist binary pipeline + bougie.tools mirror ([#206](https://github.com/cresset-tools/bougie/issues/206)) ([560c259](https://github.com/cresset-tools/bougie/commit/560c259eb09e3c07be52aca8eee25693226eb4b7))
* **run:** fall back to default PHP when no project constraint ([d85b6c7](https://github.com/cresset-tools/bougie/commit/d85b6c7c4bf41c3f08ff2399b7ab9a90e758918b))
* **run:** fall back to default PHP when no project constraint ([7d847e6](https://github.com/cresset-tools/bougie/commit/7d847e6afa2c5c237b4d520a738d996af80ab3c0))
* **run:** look up composer.json scripts before falling through to exec ([36e68cb](https://github.com/cresset-tools/bougie/commit/36e68cbade5606e1497f1322edfb85bc4dc43e29))
* **self:** implement bougie self update ([#244](https://github.com/cresset-tools/bougie/issues/244)) ([3f0f200](https://github.com/cresset-tools/bougie/commit/3f0f200b5faee9a801f71e3183eb7148239cc889))
* **semver:** bougie-semver crate + Layer 1 conformance fixture ([6c2bb10](https://github.com/cresset-tools/bougie/commit/6c2bb10621285d909f352c6fab7d722edad82d60))
* **semver:** bougie-semver crate skeleton + Layer 1 conformance fixture ([44b96cb](https://github.com/cresset-tools/bougie/commit/44b96cb346e40d4bd37fa65bfc3725becb703c91))
* **semver:** Composer-conformant Version + Constraint impl ([#104](https://github.com/cresset-tools/bougie/issues/104)) ([544b991](https://github.com/cresset-tools/bougie/commit/544b991407800f14f14723967a58c7fdfe93cb64))
* **semver:** Constraint::parse handles dev-&lt;name&gt; branch references ([#129](https://github.com/cresset-tools/bougie/issues/129)) ([098a69d](https://github.com/cresset-tools/bougie/commit/098a69d59a6f3bb1bc0fe6edd474a07cbadd07d4))
* **semver:** Constraint::parse handles Nx-dev + commit-ref + [@stability](https://github.com/stability) suffixes ([#125](https://github.com/cresset-tools/bougie/issues/125)) ([5d83100](https://github.com/cresset-tools/bougie/commit/5d831002f2fcca5d0e0266470715cbcba0372ceb))
* server-resident live autoloader ([#108](https://github.com/cresset-tools/bougie/issues/108)) ([#111](https://github.com/cresset-tools/bougie/issues/111)) ([df5e03d](https://github.com/cresset-tools/bougie/commit/df5e03d3f12c44fdd35c4d06f67d53ea7401fc80))
* **server:** [[host.rewrite]] with framework auto-detection ([ee1ebe4](https://github.com/cresset-tools/bougie/commit/ee1ebe4bcbb88e8dbb4ca9e447c4deeb5e99bf3c))
* **server:** colourise text-mode request log on TTY stderr ([4a0186f](https://github.com/cresset-tools/bougie/commit/4a0186f3d900e2833cd32e17d6d3b80aba61393e))
* **server:** port bougie-server to Windows via php-cgi.exe ([dd4ea23](https://github.com/cresset-tools/bougie/commit/dd4ea239315a64bfc66e59157950d4572ef23f71))
* **server:** port bougie-server to Windows via php-cgi.exe ([cba3e00](https://github.com/cresset-tools/bougie/commit/cba3e00616e2fb03203a0781268f378964b44aac))
* **server:** require --config on all server subcommands; drop XDG default ([2a6dfb1](https://github.com/cresset-tools/bougie/commit/2a6dfb193cc1c0b801ac6aedd67c34c651cfc22a))
* **services:** auto-detect supervised server docroot ([6ee2389](https://github.com/cresset-tools/bougie/commit/6ee238970ebf41026e16e0db3809b35403b1c332))
* **services:** auto-detect supervised server docroot ([3892cff](https://github.com/cresset-tools/bougie/commit/3892cff92c125583d4fa2d00557cad66cd8d13c7))
* **services:** auto-fetch service tarballs on first `services up` ([512c60f](https://github.com/cresset-tools/bougie/commit/512c60f17e1a60db20079e014c49c9100a229ed7))
* **services:** auto-fetch service tarballs on first `services up` ([047e93f](https://github.com/cresset-tools/bougie/commit/047e93f5c484e296c7d993dfc9f7265e7c56abe5))
* **services:** auto-restart on failure with exponential backoff ([2f2b443](https://github.com/cresset-tools/bougie/commit/2f2b44316e44244d868b3547469bd9e663f7b153))
* **services:** auto-restart on failure with exponential backoff ([e545281](https://github.com/cresset-tools/bougie/commit/e545281dafb3ab4496ebc104b026396a9d5acde2))
* **services:** babysit shim for crash-safe process-group supervision ([3b45ccd](https://github.com/cresset-tools/bougie/commit/3b45ccd431a1c693f251b33e08d580430f440944))
* **services:** babysit shim for crash-safe process-group supervision ([efa9709](https://github.com/cresset-tools/bougie/commit/efa9709542a009cb68b7f6d07c1cd9a7d37fec34))
* **services:** bougie server as a managed service ([bc9b0f0](https://github.com/cresset-tools/bougie/commit/bc9b0f0f5abeadc4145822bcfba351d22acd420c))
* **services:** bougie server as managed service (Phase 8) ([38baba7](https://github.com/cresset-tools/bougie/commit/38baba74ce502ee9b744d4c7ed79aa1b9a6c767f))
* **services:** bougie services daemon {status,stop,version} ([6de4f3f](https://github.com/cresset-tools/bougie/commit/6de4f3f1877d7c40612497903a78b32925854286))
* **services:** bougied self-restart on version mismatch (Phase 9) ([71a2be6](https://github.com/cresset-tools/bougie/commit/71a2be637a06aaa383013f968de14d5f0e486d54))
* **services:** bougied self-restart on version mismatch (Phase 9) ([afaf0b7](https://github.com/cresset-tools/bougie/commit/afaf0b702471a27870b91ef18ab7f40061591122))
* **services:** built-in catalog + [services] config schema ([f07ea07](https://github.com/cresset-tools/bougie/commit/f07ea075587f1dbd4c241d41a57066176629a5dd))
* **services:** inject BOUGIE_SERVICE_* env into `bougie run` ([cfb7135](https://github.com/cresset-tools/bougie/commit/cfb7135a65080cf0c4e845b695c32b39d5d4f53a))
* **services:** log rotation + `bougie services logs [-f] [-n N]` ([9191b05](https://github.com/cresset-tools/bougie/commit/9191b056560310232c6c9e26077415240276e333))
* **services:** mariadb provisioner + integration tests against real binary ([53e1d75](https://github.com/cresset-tools/bougie/commit/53e1d75981252790949737bd03a4a9d9fbe95e8b))
* **services:** offline subcommands — add/remove/list/catalog ([878b507](https://github.com/cresset-tools/bougie/commit/878b5071cf907c0c6576566047e51d448926a6c2))
* **services:** opensearch provisioner against the real binary (Phase 7) ([1df6beb](https://github.com/cresset-tools/bougie/commit/1df6beb828a471110b50d22c971eeaeaabaa59bc))
* **services:** opensearch provisioner with per-tenant index templates ([4e802ea](https://github.com/cresset-tools/bougie/commit/4e802ea5b86aaeada015179678bd029c6997f269))
* **services:** rabbitmq provisioner (Phase 10) ([b6b5103](https://github.com/cresset-tools/bougie/commit/b6b5103f45b7797b8f989d668450e91f364f2f7b))
* **services:** rabbitmq provisioner (Phase 10) ([a0d8ce3](https://github.com/cresset-tools/bougie/commit/a0d8ce3613a3ff59575f3be37893214606661d57))
* **services:** redis provisioner + service.{up,down,status} IPC + CLI ([8b00869](https://github.com/cresset-tools/bougie/commit/8b008695e3220aa88abd592ecfefc23a14b7bc33))
* **services:** service.restart + catalog IPC ([19a0701](https://github.com/cresset-tools/bougie/commit/19a07011e58487428ead7478e1a103d753af0a6b))
* **services:** service.restart IPC + catalog IPC method ([1ec0312](https://github.com/cresset-tools/bougie/commit/1ec031227334d0adf1a1722f34bddc762cf11172))
* **sync:** infer PHP version and required extensions ([#178](https://github.com/cresset-tools/bougie/issues/178)) ([3ea9db5](https://github.com/cresset-tools/bougie/commit/3ea9db5204094d33e79172dc76cf2b5539bda627))
* **sync:** one-command install — create lock + vendor, learn PHP/exts from the lock ([#241](https://github.com/cresset-tools/bougie/issues/241)) ([7bd6a21](https://github.com/cresset-tools/bougie/commit/7bd6a21781fe26e422f709a87cc5bafe71458306))
* **target:** detect x86_64-pc-windows-msvc on Windows hosts ([f04a491](https://github.com/cresset-tools/bougie/commit/f04a49126506cf8298858e2de5236fea98e88702))
* **tool:** ship `bougie tool` (Phases 1–3) + incremental composer install ([#204](https://github.com/cresset-tools/bougie/issues/204)) ([27bd073](https://github.com/cresset-tools/bougie/commit/27bd073615f03c8ba9f29eef3394e407898e5753))
* **up:** surface resolved tool dependencies in json-v1 ([5eb0b9b](https://github.com/cresset-tools/bougie/commit/5eb0b9b911af089bbca3245cd60e4d55a612517c))
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
* **autoloader:** port reverse-sortPackageMap order for PSR-* + classmap ([5e1597b](https://github.com/cresset-tools/bougie/commit/5e1597b0f9c6cc4ab60902541cd16efaef4876b3))
* **autoloader:** port reverse-sortPackageMap order for PSR-* + classmap ([e9252ac](https://github.com/cresset-tools/bougie/commit/e9252acf89584c8cabb75015b5abb4d2ac3b62ac))
* **autoloader:** route empty PSR-0/PSR-4 prefixes to fallback dirs ([c0caa01](https://github.com/cresset-tools/bougie/commit/c0caa01e323076bfb040c80bd0ce46c29d13ba3f))
* **autoloader:** route empty PSR-0/PSR-4 prefixes to fallback dirs ([08ed25e](https://github.com/cresset-tools/bougie/commit/08ed25e6f79b2e6a707bece3e2a7b2168bd490b7))
* **autoloader:** vendor-dir auto-exclude on PSR-* scans that span vendor ([d5bfbea](https://github.com/cresset-tools/bougie/commit/d5bfbea851bcc0baf86a81aa0cf8c02190caab0a))
* **autoloader:** vendor-dir auto-exclude on PSR-* scans that span vendor ([aa7b434](https://github.com/cresset-tools/bougie/commit/aa7b434374175fb2c9aedae20a729f3e9b1da3bd))
* **babysit:** install SIGTERM handler before spawning service ([0958daf](https://github.com/cresset-tools/bougie/commit/0958daf15738f9c1c18adb914fb7bcd67e3264ea))
* **babysit:** install SIGTERM handler before spawning the service ([990c281](https://github.com/cresset-tools/bougie/commit/990c2812bf714b0d936297e35980e067842662fe))
* **baseline:** load openssl + sodium on Windows via conf.d fragments ([037861d](https://github.com/cresset-tools/bougie/commit/037861d161bd021a8610b7ff11772214590b8972))
* **ci:** enable Git LFS checkout in CI and release-plz workflows ([#195](https://github.com/cresset-tools/bougie/issues/195)) ([83a1ad8](https://github.com/cresset-tools/bougie/commit/83a1ad875a02dcee9177da7bc8540f52b562164e))
* **ci:** give intra-workspace path deps a version ([b48be7a](https://github.com/cresset-tools/bougie/commit/b48be7a539bdb69a4aab93ba967264a9e8569e5f))
* **ci:** give intra-workspace path deps a version ([484fbd1](https://github.com/cresset-tools/bougie/commit/484fbd188b0a1680a13bd8246fecd3bafb7fc8e2))
* **ci:** switch release-plz to git_only mode ([f35b68c](https://github.com/cresset-tools/bougie/commit/f35b68c6e7c14beaefee2d4570bb73b8e6a7d430))
* **composer-install:** accept empty dist.shasum (match Composer) ([#161](https://github.com/cresset-tools/bougie/issues/161)) ([3d2aae5](https://github.com/cresset-tools/bougie/commit/3d2aae5ee0f81b789d90883f419de98e95309ca9))
* **composer-install:** claim Composer/2 UA and reuse shared HTTP client for dist downloads ([#163](https://github.com/cresset-tools/bougie/issues/163)) ([a374d83](https://github.com/cresset-tools/bougie/commit/a374d8352d11e7ecd97c5830e5795b969246ca0a))
* **composer-install:** skip metapackages instead of rejecting them ([#192](https://github.com/cresset-tools/bougie/issues/192)) ([a469aa9](https://github.com/cresset-tools/bougie/commit/a469aa92f22a9d9422690ec2be44496ea2840041))
* **composer-resolver:** accept `<name>-dev` constraint as synonym for `dev-<name>` ([#190](https://github.com/cresset-tools/bougie/issues/190)) ([700546a](https://github.com/cresset-tools/bougie/commit/700546a112735b6cd6c4b96958cf1e03c003c18b))
* **composer-resolver:** report all resolution problems, not just the first ([#191](https://github.com/cresset-tools/bougie/issues/191)) ([0a2d499](https://github.com/cresset-tools/bougie/commit/0a2d499d3fbf105736d3524b41d33bc5be58e0ef))
* **composer-resolver:** resolve all cross-check divergences ([#184](https://github.com/cresset-tools/bougie/issues/184)) ([acc637b](https://github.com/cresset-tools/bougie/commit/acc637bb61d0adee20f834c08fc56eb20e920509))
* **composer-resolver:** union repo candidates and multi-provider virtuals ([#135](https://github.com/cresset-tools/bougie/issues/135)) ([126d0c6](https://github.com/cresset-tools/bougie/commit/126d0c6adb17fa2566f76a6aa34fbd763e8ab3fa))
* **composer:** Mage-OS resolve fixes — caret ^0, self-replace, fetch retry ([#232](https://github.com/cresset-tools/bougie/issues/232)) ([96cef9e](https://github.com/cresset-tools/bougie/commit/96cef9ec36cb0d15d13f97a47e773f50244532e6))
* **composer:** tolerate PHP empty-array form for empty maps ([#128](https://github.com/cresset-tools/bougie/issues/128)) ([937d691](https://github.com/cresset-tools/bougie/commit/937d691db3bd2a3038db165a789643e29a7d325f))
* **conf_d:** quote extension= paths on Windows to survive `~` in 8.3 names ([08b8b02](https://github.com/cresset-tools/bougie/commit/08b8b02a07a09b82a636e274b14117421605f2f2))
* **daemon:** anchor rabbitmq CWD to its data dir ([#167](https://github.com/cresset-tools/bougie/issues/167)) ([584d156](https://github.com/cresset-tools/bougie/commit/584d1567efa13f69c1e2beac6c00daaef08b4024))
* **errors:** show root cause in network error diagnostics ([#166](https://github.com/cresset-tools/bougie/issues/166)) ([eafe7a2](https://github.com/cresset-tools/bougie/commit/eafe7a22e0ca502c624ae57aece124b27739cbbd))
* **ext:** canonicalise on-disk basename for local .so installs ([24d4164](https://github.com/cresset-tools/bougie/commit/24d4164e30db1b1f839f3f3e3ec13b320c4fe1f8))
* **ext:** skip duplicate conf.d fragment when ext is bundled ([3213fab](https://github.com/cresset-tools/bougie/commit/3213fab7854c27e5d889b485a2e1cf2bd2d8ea3a))
* **ext:** skip duplicate conf.d fragment when ext is bundled by install ([c13458c](https://github.com/cresset-tools/bougie/commit/c13458c0e104bc2c16c43330c849049ee040aa91))
* **fetch:** gate test-only DownloadBar::planned to non-Windows ([71d0ccc](https://github.com/cresset-tools/bougie/commit/71d0ccc186d9ec52129f1ddef70e9dfd81e7e63f))
* **fetch:** hide DownloadBar until first real progress event ([#173](https://github.com/cresset-tools/bougie/issues/173)) ([6aaebd0](https://github.com/cresset-tools/bougie/commit/6aaebd00ed3e146bf4ff840cae31c8dc43c10bd9))
* **index:** accept Nix-base32 closure hashes in wire validator ([3a47b15](https://github.com/cresset-tools/bougie/commit/3a47b15259c9a5823d4766b45037620bb14a79c9))
* **index:** accept Nix-base32 closure hashes in wire validator ([1748c7b](https://github.com/cresset-tools/bougie/commit/1748c7b1cf5baaa03dabfac6adf103bee4844900))
* **installer:** skip opcache baseline install on PHP 8.5+ ([#159](https://github.com/cresset-tools/bougie/issues/159)) ([4de7312](https://github.com/cresset-tools/bougie/commit/4de7312b687f9551530fa4b252672beb6c2757a2))
* **install:** re-import ArchiveKind for the unix-only closure path ([d26d8ba](https://github.com/cresset-tools/bougie/commit/d26d8baf298b2ad9cb933f91c355337d0a9d736a))
* **opensearch:** pin OPENSEARCH_JAVA_HOME + detect early child exit in health probe ([5706a64](https://github.com/cresset-tools/bougie/commit/5706a640aa562cd91aad4c1af18e66abfbad6beb))
* **recipe:** Mage-OS one-command bring-up — detect mage-os, redis-over-socket, lock re-stamp ([#251](https://github.com/cresset-tools/bougie/issues/251)) ([4d29004](https://github.com/cresset-tools/bougie/commit/4d2900418697defb4bc17ecfcac98c498b31b784))
* **recipe:** provision server tenant before Magento install ([#170](https://github.com/cresset-tools/bougie/issues/170)) ([38c5ff3](https://github.com/cresset-tools/bougie/commit/38c5ff33b056c253ea9eeb99b6e087185b7e103e))
* **release:** allow dirty working directory for LFS-tracked fixtures ([#196](https://github.com/cresset-tools/bougie/issues/196)) ([edd54bd](https://github.com/cresset-tools/bougie/commit/edd54bd9020642d5053ba481ba177069296874fa))
* **release:** decouple bougie version + centralize workspace dep pins ([#180](https://github.com/cresset-tools/bougie/issues/180)) ([24e13e3](https://github.com/cresset-tools/bougie/commit/24e13e33572a1d6169a4d6cd6c0600eae05861c8))
* **release:** inherit workspace.package.version across all bougie-* crates ([#143](https://github.com/cresset-tools/bougie/issues/143)) ([c63dc75](https://github.com/cresset-tools/bougie/commit/c63dc75b30d4caf19e4ac9fbe24dec730ae32892))
* **release:** install LFS system-wide with --skip-smudge for release-plz ([#200](https://github.com/cresset-tools/bougie/issues/200)) ([f812055](https://github.com/cresset-tools/bougie/commit/f812055857ae660fe56b46f7aec381e24fdd58f1))
* **release:** jq key-access syntax for release-please-manifest ([#242](https://github.com/cresset-tools/bougie/issues/242)) ([7c0a5f4](https://github.com/cresset-tools/bougie/commit/7c0a5f408980e3cbc3962ff8208476c393c6863e))
* **release:** let dist own the GitHub Release; release-please pushes tag only ([#238](https://github.com/cresset-tools/bougie/issues/238)) ([55ef8e5](https://github.com/cresset-tools/bougie/commit/55ef8e5d30a1d7e4bd2c5e79051a101c9973e135))
* **release:** let release-please own the draft GitHub Release ([#245](https://github.com/cresset-tools/bougie/issues/245)) ([6b8ce18](https://github.com/cresset-tools/bougie/commit/6b8ce18395186d66963f96a2bb7e3056d2a9b0fe))
* **release:** make release-please actually rewrite Cargo.toml ([#237](https://github.com/cresset-tools/bougie/issues/237)) ([ca40f63](https://github.com/cresset-tools/bougie/commit/ca40f63e432c7ddae1c491db0123fc8101ce1143))
* **release:** move release-tag push into its own isolated job ([#253](https://github.com/cresset-tools/bougie/issues/253)) ([1570fc1](https://github.com/cresset-tools/bougie/commit/1570fc1e8d041cf82f305ee2818ff177371b08c1))
* **release:** neutralize LFS smudge/process filter for release-plz ([#199](https://github.com/cresset-tools/bougie/issues/199)) ([b108d3f](https://github.com/cresset-tools/bougie/commit/b108d3f151dee4e93a21a3f333fa9bbbed3accc3))
* **release:** push the release tag (draft Releases don't auto-tag) ([#249](https://github.com/cresset-tools/bougie/issues/249)) ([469ee13](https://github.com/cresset-tools/bougie/commit/469ee1373c5b22b3b35e5336dc907b14138a57a9))
* **release:** re-pin version on intra-workspace path deps ([#148](https://github.com/cresset-tools/bougie/issues/148)) ([2bcc1e4](https://github.com/cresset-tools/bougie/commit/2bcc1e4d0d1f2086921273850e6fd2ea5071c1a0))
* **release:** skip LFS smudge in release-plz workflow ([#197](https://github.com/cresset-tools/bougie/issues/197)) ([6e1bcda](https://github.com/cresset-tools/bougie/commit/6e1bcda7f071425f54df889afb906734975b353c))
* **release:** sudo for system-wide LFS install ([#201](https://github.com/cresset-tools/bougie/issues/201)) ([925ad52](https://github.com/cresset-tools/bougie/commit/925ad52ba6f82acb43018ad69dce2bb010debfad))
* **release:** un-LFS the magento2 fixture, scope LFS to cross-check only ([#202](https://github.com/cresset-tools/bougie/issues/202)) ([89119e4](https://github.com/cresset-tools/bougie/commit/89119e4066ec206b8c8e9f92afbfb630de51be86))
* **release:** unblock musl + windows dist targets ([#233](https://github.com/cresset-tools/bougie/issues/233)) ([87705a9](https://github.com/cresset-tools/bougie/commit/87705a9ec70115f857bb84d9daa827dde5e58f15))
* **release:** uninstall LFS filter before running release-plz ([#198](https://github.com/cresset-tools/bougie/issues/198)) ([93b406f](https://github.com/cresset-tools/bougie/commit/93b406fc114a2425332cddb73fe9f900d3270bff))
* resolve whole-project review findings ([#207](https://github.com/cresset-tools/bougie/issues/207)–[#231](https://github.com/cresset-tools/bougie/issues/231)) ([#234](https://github.com/cresset-tools/bougie/issues/234)) ([4f873e9](https://github.com/cresset-tools/bougie/commit/4f873e95dd96e62f4423b8cd0fe0f1a369038aab))
* **run:** walk up to project root, not cwd ([#176](https://github.com/cresset-tools/bougie/issues/176)) ([7eec606](https://github.com/cresset-tools/bougie/commit/7eec60673320e28de3245db3e52d25f140c67fef))
* **sandbox-run:** narrow ProtectHome read-deny to file-read-data on macOS ([39ae59e](https://github.com/cresset-tools/bougie/commit/39ae59eaa521d922d46d2822a73033f9a8a2eec2))
* **server:** drop orphaned home_from_passwd tests ([9ba083c](https://github.com/cresset-tools/bougie/commit/9ba083c938be99b6d23f95a8b90761867efcd826))
* **services/mariadb:** pass --no-defaults to every mariadb invocation ([53e2bd3](https://github.com/cresset-tools/bougie/commit/53e2bd323e7e3554b03e8fe78d82520e5d3eea36))
* **services:** re-sync rabbitmq password to broker after `bougie down` ([#31](https://github.com/cresset-tools/bougie/issues/31)) ([5636c37](https://github.com/cresset-tools/bougie/commit/5636c37a3d9611eaefb64a51b0bc04150c148873))
* **services:** re-sync rabbitmq password to broker after `bougie down` ([#31](https://github.com/cresset-tools/bougie/issues/31)) ([97ad805](https://github.com/cresset-tools/bougie/commit/97ad8055c0aa42402725c88a3f331b2d8aa4087a))
* **shim:** strip .exe case-insensitively so unzip.EXE detects as Unzip role ([e26973c](https://github.com/cresset-tools/bougie/commit/e26973c0c486f7f6a72673e59a894b769c3e9c0e))
* **sync:** accept Composer wildcards in require.php ([#106](https://github.com/cresset-tools/bougie/issues/106)) ([#150](https://github.com/cresset-tools/bougie/issues/150)) ([70d1c35](https://github.com/cresset-tools/bougie/commit/70d1c35d07066a08e07c0aed075236296c166c89))
* **sync:** drop stale composer-write fragments when an ext joins the baseline ([f3905cc](https://github.com/cresset-tools/bougie/commit/f3905cc9220666bdc5335e77f95653f435acfe11))
* **sync:** drop stale composer-write fragments when an ext joins the baseline ([73e6c99](https://github.com/cresset-tools/bougie/commit/73e6c99967ea9a1835e0630b831979b8010f20bd))
* **sync:** refresh staged shim on mtime change, not size alone ([0c989e6](https://github.com/cresset-tools/bougie/commit/0c989e6a1082ce172b5ff473bf91c4c1847ad39d))
* **sync:** stage local copy of bougie.exe for cross-volume NTFS shims ([0a9e8ab](https://github.com/cresset-tools/bougie/commit/0a9e8ab2428bfcbe557948eb7e8772afbebd2b42))
* **sync:** update fragment_name test after mbstring joined baseline ([04917e1](https://github.com/cresset-tools/bougie/commit/04917e1145f33f6f29db14dda76a42235f6d6bb6))
* **target:** gate libc helpers on target_os=linux, not unix ([ace310c](https://github.com/cresset-tools/bougie/commit/ace310cbcc8071d748e3d894de7c9c4f466b4fc2))
* **tests:** pass --config to server list calls in integration tests ([b8f8833](https://github.com/cresset-tools/bougie/commit/b8f8833bf49c080cf030f325fdae5f4ece4c02ca))
* **tests:** retarget phase9 binary-install tests to `composer fetch` ([aa4d240](https://github.com/cresset-tools/bougie/commit/aa4d240f2edca37aaf8d448f71b6675f9121e2d7))
* **version:** parse `~8.3` as a tilde-range constraint, not a path ([#177](https://github.com/cresset-tools/bougie/issues/177)) ([f5770d7](https://github.com/cresset-tools/bougie/commit/f5770d7be97876d836ed9cbb63bb3b6a3413d3fe))
* **windows:** parse releases.json size string into bytes ([4cc5d19](https://github.com/cresset-tools/bougie/commit/4cc5d19133e40ec1066b43a9c5f753d911357578))
* **windows:** parse releases.json size string into bytes ([71e6dfa](https://github.com/cresset-tools/bougie/commit/71e6dfa78ae0b9afa1ce41492927323d303a3036))


### Performance Improvements

* **autoloader:** parallelize classmap scan + bench harness ([b6c7480](https://github.com/cresset-tools/bougie/commit/b6c7480ac444e4010bf6a8a1c59c9bf3622275ee))
* **autoloader:** parallelize classmap scan + bench harness ([60a9aa7](https://github.com/cresset-tools/bougie/commit/60a9aa764c60a97938cb83e4ae2e967294a87916))
* **composer-resolver:** hand pubgrub a Ref instead of cloning versions_for ([#140](https://github.com/cresset-tools/bougie/issues/140)) ([c602264](https://github.com/cresset-tools/bougie/commit/c602264733fd7766f37ad8a0192f0e5752055b40))
* **composer-resolver:** mem::forget provider + hoist virtual computation into workers ([#142](https://github.com/cresset-tools/bougie/issues/142)) ([836b450](https://github.com/cresset-tools/bougie/commit/836b4500991365861ce816e06595960a719a9744))
* **composer-resolver:** uv-style improvements (PRs 0-4) ([#157](https://github.com/cresset-tools/bougie/issues/157)) ([7600b0b](https://github.com/cresset-tools/bougie/commit/7600b0bcb8102389368f46fa500e69accb9c7b25))


### Miscellaneous Chores

* relicense from Apache-2.0 OR MIT to EUPL-1.2 ([bf4b42a](https://github.com/cresset-tools/bougie/commit/bf4b42a13122270aa7782df1082e3b4871009037))


### Code Refactoring

* Debian-faithful baseline + --bare / --without flags ([87e3718](https://github.com/cresset-tools/bougie/commit/87e3718a4ee064359215fc7d2b7589d366387963))
* **server:** drop `server add/remove`, require --config for `server run` ([242431a](https://github.com/cresset-tools/bougie/commit/242431a374de86c5273a388ca786fce4f46a3aa6))

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
