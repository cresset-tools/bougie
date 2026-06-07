<?php return array(
    'root' => array(
        'name' => 'acme/platform-check-provider',
        'pretty_version' => '1.0.0+no-version-set',
        'version' => '1.0.0.0',
        'reference' => null,
        'type' => 'library',
        'install_path' => __DIR__ . '/../../',
        'aliases' => array(),
        'dev' => true,
    ),
    'versions' => array(
        'acme/foo-polyfill' => array(
            'pretty_version' => '1.0.0',
            'version' => '1.0.0.0',
            'reference' => '3e6e52d9dab6f2a0c102e35191f84a3012ec5d27',
            'type' => 'library',
            'install_path' => __DIR__ . '/../acme/foo-polyfill',
            'aliases' => array(),
            'dev_requirement' => false,
        ),
        'acme/platform-check-provider' => array(
            'pretty_version' => '1.0.0+no-version-set',
            'version' => '1.0.0.0',
            'reference' => null,
            'type' => 'library',
            'install_path' => __DIR__ . '/../../',
            'aliases' => array(),
            'dev_requirement' => false,
        ),
    ),
);
