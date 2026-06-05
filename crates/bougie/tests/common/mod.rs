//! Shared test harness: isolated `BOUGIE_HOME` / `BOUGIE_CACHE` per test
//! plus a pre-configured `assert_cmd` builder.
#![allow(dead_code)]

pub mod mariadb_fixture;
pub mod opensearch_fixture;
pub mod rabbitmq_fixture;

use assert_cmd::Command;
use std::path::{Path, PathBuf};
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

    /// Build a `bgx` command with isolated env (short alias for
    /// `bougie tool run`).
    pub fn bgx(&self) -> Command {
        let mut cmd = Command::cargo_bin("bgx").expect("bgx binary");
        cmd.env("BOUGIE_HOME", self.home.path())
            .env("BOUGIE_CACHE", self.cache.path())
            .env_remove("RUST_LOG");
        cmd
    }
}

/// A test project on disk. The project root is a child directory whose
/// basename is the sanitized composer name, so the daemon's
/// basename-derived tenant is predictable (`acme/blog` → tenant
/// `acme_blog`). The private parent `TempDir` keeps the project unique
/// and cleans up on drop.
pub struct TestProject {
    _tmp: TempDir,
    root: PathBuf,
}

impl TestProject {
    pub fn path(&self) -> &Path {
        &self.root
    }
}

/// Mirror of the production tenant sanitizer
/// (`commands::tenant::sanitize_tenant`): lowercase ASCII alphanumerics
/// kept, everything else → `_`.
fn tenant_slug(input: &str) -> String {
    let s: String = input
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect();
    let trimmed = s.trim_matches('_');
    if trimmed.is_empty() { "project".to_string() } else { trimmed.to_string() }
}

/// Create a project with the given composer `name` in a directory whose
/// basename sanitizes to the derived tenant. Use this instead of a bare
/// `TempDir` so tenant assertions stay deterministic.
pub fn project_with_composer(name: &str) -> TestProject {
    project_with_composer_inner(name, false)
}

/// Like [`project_with_composer`] but also seeds a `public/` web root
/// (the realistic Laravel/Symfony shape the dev server auto-detects).
pub fn project_with_composer_and_public(name: &str) -> TestProject {
    project_with_composer_inner(name, true)
}

fn project_with_composer_inner(name: &str, with_public: bool) -> TestProject {
    let tmp = TempDir::new().expect("project tempdir");
    let root = tmp.path().join(tenant_slug(name));
    std::fs::create_dir_all(&root).expect("create project dir");
    std::fs::write(root.join("composer.json"), format!(r#"{{"name":"{name}"}}"#))
        .expect("write composer.json");
    if with_public {
        std::fs::create_dir_all(root.join("public")).expect("create public/");
    }
    TestProject { _tmp: tmp, root }
}
