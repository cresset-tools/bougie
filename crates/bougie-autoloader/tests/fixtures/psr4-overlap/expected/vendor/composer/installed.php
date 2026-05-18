<?php return array(
    'root' => array(
        'name' => 'fixture/psr4-overlap',
        'pretty_version' => '1.0.0+no-version-set',
        'version' => '1.0.0.0',
        'reference' => null,
        'type' => 'project',
        'install_path' => __DIR__ . '/../../',
        'aliases' => array(),
        'dev' => true,
    ),
    'versions' => array(
        'acme/overlap' => array(
            'pretty_version' => '1.0.0',
            'version' => '1.0.0.0',
            'reference' => '682dd4f3cdf3f33f9b434b9fc0986bca421e5d0e',
            'type' => 'library',
            'install_path' => __DIR__ . '/../acme/overlap',
            'aliases' => array(),
            'dev_requirement' => false,
        ),
        'fixture/psr4-overlap' => array(
            'pretty_version' => '1.0.0+no-version-set',
            'version' => '1.0.0.0',
            'reference' => null,
            'type' => 'project',
            'install_path' => __DIR__ . '/../../',
            'aliases' => array(),
            'dev_requirement' => false,
        ),
    ),
);
