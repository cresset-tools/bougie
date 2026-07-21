<?php
/**
 * Bougie_Share — makes base URLs request-relative for bougie-served hosts so a
 * store served on *.bougie.run (dev) or shared on *.bougie.show generates
 * correct absolute URLs, without writing stored config. Managed by `bougie`.
 */

declare(strict_types=1);

use Magento\Framework\Component\ComponentRegistrar;

ComponentRegistrar::register(ComponentRegistrar::MODULE, 'Bougie_Share', __DIR__);
