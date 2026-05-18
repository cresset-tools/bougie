<?php

// Class name + path are contrived to deliberately satisfy the root's
// PSR-4 namespace+path rule (App\<...> rooted at project root) so
// that without Composer's vendor-dir auto-exclude this class would
// land in the user's autoload_classmap.php.
namespace App\vendor\acme\sneak;

class SneakClass
{
}
