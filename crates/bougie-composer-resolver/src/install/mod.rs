//! Apply a resolved Composer dependency set to a project: download
//! each package's dist archive in parallel, extract into the project
//! `vendor/` tree. The eventual end-to-end `bougie composer install`
//! command will read `composer.lock`, build [`DistRequest`]s from the
//! locked packages, call [`fetch_and_extract_dists`], then hand off to
//! `bougie-autoloader` for `vendor/autoload.php` generation.

mod downloader;

pub use downloader::{fetch_and_extract_dists, DistRequest};
