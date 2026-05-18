<?php

/*
 * Port composer/semver's PHPUnit data providers into a JSON fixture
 * file consumed by bougie-semver's Layer 1 conformance tests.
 *
 * Usage:
 *   php scripts/port-composer-semver-tests.php /path/to/composer/semver/clone
 *
 * The argument must point at a checkout of github.com/composer/semver
 * with `composer install` already run inside it. Output is written to
 * crates/bougie-semver/tests/data/conformance.json relative to the
 * bougie repo root (the script's parent of `scripts/`).
 *
 * This is a vendor step. Re-run when bumping the pinned upstream
 * commit; commit the regenerated JSON. The script is documentation;
 * the JSON is the test data. See RESOLVER_TEST_PLAN.md Layer 1.
 *
 * What gets ported (string-valued providers only; Constraint-object
 * providers are deferred until bougie-semver's Constraint parser
 * exists and we can re-stringify them faithfully):
 *
 *  - VersionParser:
 *      numericAliasVersions, isValidVersions,
 *      successfulNormalizedVersions, failingNormalizedVersions,
 *      successfulNormalizedBranches, stabilityProvider
 *  - Comparator:
 *      greaterThan, greaterThanOrEqualTo, lessThan,
 *      lessThanOrEqualTo, equalTo, notEqualTo, compare
 *  - Semver:
 *      sortProvider, satisfiesProviderPositive, satisfiesProviderNegative
 */

declare(strict_types=1);

if ($argc < 2) {
    fwrite(STDERR, "usage: php {$argv[0]} /path/to/composer-semver-checkout\n");
    exit(1);
}

$semverRoot = rtrim($argv[1], '/');
$autoload = $semverRoot . '/vendor/autoload.php';
if (!is_file($autoload)) {
    fwrite(STDERR, "missing $autoload — run `composer install` in the semver checkout first\n");
    exit(1);
}
require $autoload;

// composer/semver's test classes extend PHPUnit\Framework\TestCase,
// but its composer.json pulls phpunit in lazily via symfony/phpunit-bridge,
// which means PHPUnit isn't on disk at vendor time. We only need the
// static data-provider methods, not the test runner, so a stub TestCase
// is enough to let the test classes load.
if (!class_exists('PHPUnit\\Framework\\TestCase')) {
    eval('namespace PHPUnit\\Framework; abstract class TestCase {}');
}

foreach (glob($semverRoot . '/tests/*.php') as $f) {
    require_once $f;
}

$out = [
    'source' => [
        'repo' => 'composer/semver',
        'commit' => trim((string) @shell_exec(
            'git -C ' . escapeshellarg($semverRoot) . ' rev-parse HEAD 2>/dev/null'
        )),
    ],
    'suites' => [],
];

$collect = function (string $class, string $method) use (&$out): void {
    $cases = call_user_func([$class, $method]);
    $portable = [];
    foreach ($cases as $row) {
        $coerced = [];
        $skip = false;
        foreach ($row as $cell) {
            if (is_scalar($cell) || is_null($cell)) {
                $coerced[] = $cell;
            } elseif (is_array($cell)) {
                $allScalar = true;
                foreach ($cell as $c) {
                    if (!is_scalar($c) && !is_null($c)) {
                        $allScalar = false;
                        break;
                    }
                }
                if ($allScalar) {
                    $coerced[] = $cell;
                } else {
                    $skip = true;
                    break;
                }
            } else {
                $skip = true;
                break;
            }
        }
        if (!$skip) {
            $portable[] = $coerced;
        }
    }
    $out['suites'][] = [
        'class' => $class,
        'method' => $method,
        'cases' => $portable,
    ];
};

$collect('Composer\\Semver\\VersionParserTest', 'numericAliasVersions');
$collect('Composer\\Semver\\VersionParserTest', 'isValidVersions');
$collect('Composer\\Semver\\VersionParserTest', 'successfulNormalizedVersions');
$collect('Composer\\Semver\\VersionParserTest', 'failingNormalizedVersions');
$collect('Composer\\Semver\\VersionParserTest', 'successfulNormalizedBranches');
$collect('Composer\\Semver\\VersionParserTest', 'stabilityProvider');

$collect('Composer\\Semver\\ComparatorTest', 'greaterThanProvider');
$collect('Composer\\Semver\\ComparatorTest', 'greaterThanOrEqualToProvider');
$collect('Composer\\Semver\\ComparatorTest', 'lessThanProvider');
$collect('Composer\\Semver\\ComparatorTest', 'lessThanOrEqualToProvider');
$collect('Composer\\Semver\\ComparatorTest', 'equalToProvider');
$collect('Composer\\Semver\\ComparatorTest', 'notEqualToProvider');
$collect('Composer\\Semver\\ComparatorTest', 'compareProvider');

$collect('Composer\\Semver\\SemverTest', 'sortProvider');
$collect('Composer\\Semver\\SemverTest', 'satisfiesProviderPositive');
$collect('Composer\\Semver\\SemverTest', 'satisfiesProviderNegative');

$bougieRoot = dirname(__DIR__);
$outPath = $bougieRoot . '/crates/bougie-semver/tests/data/conformance.json';
file_put_contents(
    $outPath,
    json_encode($out, JSON_PRETTY_PRINT | JSON_UNESCAPED_SLASHES) . "\n"
);

$total = array_sum(array_map(fn($s) => count($s['cases']), $out['suites']));
fwrite(STDERR, "wrote $outPath (" . count($out['suites']) . " suites, $total cases)\n");
