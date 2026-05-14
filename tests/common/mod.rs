//! Shared test harness: isolated `BOUGIE_HOME` / `BOUGIE_CACHE` per test
//! plus a pre-configured `assert_cmd` builder.
#![allow(dead_code)]

pub mod mariadb_fixture;

use assert_cmd::Command;
use std::path::Path;
use tempfile::TempDir;

pub struct TestEnv {
    pub home: TempDir,
    pub cache: TempDir,
}

impl TestEnv {
    pub fn new() -> Self {
        Self {
            home: TempDir::new().expect("tempdir for BOUGIE_HOME"),
            cache: TempDir::new().expect("tempdir for BOUGIE_CACHE"),
        }
    }

    pub fn home_path(&self) -> &Path {
        self.home.path()
    }

    pub fn cache_path(&self) -> &Path {
        self.cache.path()
    }

    /// Build a `bougie` command with isolated env.
    pub fn bougie(&self) -> Command {
        let mut cmd = Command::cargo_bin("bougie").expect("bougie binary");
        cmd.env("BOUGIE_HOME", self.home.path())
            .env("BOUGIE_CACHE", self.cache.path())
            .env_remove("RUST_LOG");
        cmd
    }
}
