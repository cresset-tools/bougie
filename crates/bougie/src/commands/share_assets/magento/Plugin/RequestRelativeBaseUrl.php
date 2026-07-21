<?php
/**
 * Bougie_Share — request-relative base URLs for bougie-served hosts.
 */

declare(strict_types=1);

namespace Bougie\Share\Plugin;

use Magento\Store\Model\Store;

/**
 * Rewrites base URLs to the *request* host + scheme for bougie-served hosts
 * (`*.bougie.run` dev and `*.bougie.show` shares), per request and at runtime.
 *
 * It runs *after* the value leaves the config cache, so the config cache stays
 * on (Magento stays fast) and no stored config is touched — meaning it neither
 * trips {@see \Magento\Deploy\Model\Plugin\ConfigChangeDetector} nor gets frozen
 * to the first host that warmed the cache. Any other host (a real production
 * domain) is left untouched, so the module is inert in production and safe to
 * leave installed.
 */
class RequestRelativeBaseUrl
{
    /** Hosts bougie serves: the dev domain and the share domain. */
    private const BOUGIE_HOST = '/\.(bougie\.run|bougie\.show)$/';

    /**
     * @param string $result The base URL Magento resolved from config.
     * @return string The base URL, re-hosted onto the current request.
     */
    public function afterGetBaseUrl(Store $subject, $result)
    {
        if (!is_string($result)) {
            return $result;
        }

        $host = $_SERVER['HTTP_X_FORWARDED_HOST'] ?? ($_SERVER['HTTP_HOST'] ?? '');
        // A proxy chain may send a comma-separated list; the client-facing host
        // is the first entry.
        if (strpos($host, ',') !== false) {
            $host = trim(explode(',', $host)[0]);
        }
        if (!preg_match(self::BOUGIE_HOST, $host)) {
            return $result;
        }

        $forwardedProto = $_SERVER['HTTP_X_FORWARDED_PROTO'] ?? '';
        $https = $forwardedProto === 'https'
            || (isset($_SERVER['HTTPS']) && $_SERVER['HTTPS'] !== 'off');
        $scheme = $https ? 'https' : 'http';

        // Preserve the path (base URLs for static/media carry sub-paths); swap
        // only scheme + host.
        $path = parse_url($result, PHP_URL_PATH);
        if (!is_string($path) || $path === '') {
            $path = '/';
        }

        return $scheme . '://' . $host . $path;
    }
}
