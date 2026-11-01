<?php
/**
 * Plugin Name: bougie share
 * Description: Serves this site on its public `bougie share` URL. Installed by bougie; inert on every other host.
 */

// WordPress stores its URLs absolutely (the `home`/`siteurl` options, or the
// WP_HOME/WP_SITEURL constants a Bedrock .env sets, and every link saved in
// post content), so a share would redirect visitors to the local dev URL. On a
// share request this swaps that local origin for the public one: in the URLs
// WordPress generates, and in the finished response for links stored verbatim.
//
// Only requests that arrived through the bougie relay are touched — it sets
// X-Forwarded-Host to the `*.bougie.show` share host — so the plugin does
// nothing locally and is safe to leave installed.

if (!defined('ABSPATH')) {
    exit;
}

(static function (): void {
    $host = (string) ($_SERVER['HTTP_X_FORWARDED_HOST'] ?? '');
    // A proxy chain may send a list; the client-facing host comes first.
    $host = strtolower(trim(explode(',', $host)[0]));
    if (!preg_match('/^[a-z0-9-]+\.bougie\.show$/', $host)) {
        return;
    }

    // The relay terminates TLS, so PHP sees plain HTTP on the dev server's port.
    $_SERVER['HTTPS'] = 'on';
    $_SERVER['SERVER_PORT'] = '443';
    $_SERVER['HTTP_HOST'] = $host;

    $public = 'https://' . $host;
    $origin = static function ($url): string {
        $p = is_string($url) ? parse_url($url) : false;
        if (!is_array($p) || empty($p['host'])) {
            return '';
        }
        return ($p['scheme'] ?? 'http') . '://' . $p['host'] . (isset($p['port']) ? ':' . $p['port'] : '');
    };
    // Read before our filters exist: the configured (local) origins.
    $local = array_values(array_unique(array_filter(
        [$origin(get_option('home')), $origin(get_option('siteurl'))],
        static fn (string $o): bool => $o !== '' && $o !== $public
    )));
    if ($local === []) {
        return;
    }

    // Each local origin also appears JSON-escaped (`http:\/\/…`) in inline
    // scripts and REST responses.
    $from = [];
    $to = [];
    foreach ($local as $o) {
        array_push($from, $o, str_replace('/', '\/', $o));
        array_push($to, $public, str_replace('/', '\/', $public));
    }
    $swap = static fn ($v) => is_string($v) ? str_replace($from, $to, $v) : $v;

    foreach ([
        'option_home', 'option_siteurl',
        'content_url', 'plugins_url', 'theme_root_uri',
        'stylesheet_directory_uri', 'template_directory_uri',
        'script_loader_src', 'style_loader_src', 'wp_get_attachment_url',
    ] as $hook) {
        add_filter($hook, $swap, PHP_INT_MAX);
    }
    add_filter('upload_dir', static function ($dir) use ($swap) {
        $dir['url'] = $swap($dir['url']);
        $dir['baseurl'] = $swap($dir['baseurl']);
        return $dir;
    }, PHP_INT_MAX);

    // Whatever still slips through (links saved in post content, hardcoded
    // theme URLs) is rewritten in the response body.
    ob_start($swap);
})();
