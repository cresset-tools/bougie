//! `bougie ci init` — generate a CI workflow that reproduces the local bougie
//! toolchain, so CI and laptops run identically.
//!
//! The generated file is deliberately thin: it composes the first-party
//! `cresset-tools/setup-bougie` action (or the GitLab `bougie` component) with
//! **this project's own recipe tasks**. CI runs `bougie make <task>` — the same
//! command the dev runs — so parity is *structural*, not maintained: the file
//! encodes no PHP version, no extension list, no service images. `bougie sync`
//! reproduces the toolchain from the lock; `bougie make test` runs the merged
//! (built-in + team + local) recipe. For a team project it also emits a
//! zero-secret `bougie login --ci` step (OIDC JWT → org read token).

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bougie_cli::{CiInitArgs, OutputFormat};
use eyre::{eyre, Result, WrapErr};

use super::make::{self, MakeOptions};
use super::service::config_mut::locate_project_root;
use super::team;

/// The recipe tasks `ci init` wires up as CI steps, in run order, when the
/// project's merged recipe defines them. Everything else the dev can add.
const CI_TASKS: &[&str] = &["lint", "test", "deploy-check"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Provider {
    Github,
    Gitlab,
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "owned args from clap-parsed CLI"
)]
pub fn run(_format: OutputFormat, args: CiInitArgs) -> Result<ExitCode> {
    let project_root = locate_project_root()?;
    let telemetry = match args.telemetry.as_str() {
        "true" | "false" | "local" => args.telemetry.clone(),
        other => {
            return Err(eyre!(
                "--telemetry must be true, false, or local (got `{other}`)"
            ));
        }
    };
    let provider = resolve_provider(args.provider.as_deref(), &project_root)?;
    let plan = Plan {
        bougie_version: env!("CARGO_PKG_VERSION").to_string(),
        telemetry,
        services: declared_services(&project_root),
        tasks: ci_tasks(&project_root),
        team: team_info(&project_root, args.repository.as_deref()),
    };

    let (rel_path, content) = match provider {
        Provider::Github => (
            PathBuf::from(".github/workflows/bougie.yml"),
            render_github(&plan),
        ),
        Provider::Gitlab => (PathBuf::from(".gitlab-ci.yml"), render_gitlab(&plan)),
    };
    let out = project_root.join(&rel_path);
    if out.exists() && !args.force {
        return Err(eyre!(
            "{} already exists — pass --force to overwrite",
            rel_path.display()
        ));
    }
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent).wrap_err_with(|| format!("creating {}", parent.display()))?;
    }
    fs::write(&out, content).wrap_err_with(|| format!("writing {}", out.display()))?;

    println!("bougie ci init: wrote {}", rel_path.display());
    if plan
        .team
        .as_ref()
        .is_some_and(|t| t.repository_is_placeholder)
    {
        eprintln!(
            "note: set the login repository — re-run with `--repository <org/repo>` (the sconce \
             repo whose `read` CI policy authorizes the exchange), or edit the file."
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Everything derived from the project that shapes the workflow.
struct Plan {
    bougie_version: String,
    telemetry: String,
    services: Vec<String>,
    tasks: Vec<String>,
    team: Option<TeamInfo>,
}

struct TeamInfo {
    registry: String,
    repository: String,
    repository_is_placeholder: bool,
}

fn resolve_provider(flag: Option<&str>, root: &Path) -> Result<Provider> {
    if let Some(p) = flag {
        return match p {
            "github" => Ok(Provider::Github),
            "gitlab" => Ok(Provider::Gitlab),
            other => Err(eyre!("--provider must be github or gitlab (got `{other}`)")),
        };
    }
    if root.join(".gitlab-ci.yml").exists() {
        return Ok(Provider::Gitlab);
    }
    // `.github/` present, or nothing to go on → GitHub Actions (the default).
    Ok(Provider::Github)
}

/// The project's declared services (for the `bougie up` step). Empty when the
/// config can't load or declares none.
fn declared_services(root: &Path) -> Vec<String> {
    bougie_config::load_project(root)
        .map(|p| p.bougie.services.keys().cloned().collect())
        .unwrap_or_default()
}

/// The `CI_TASKS` the project's merged recipe actually defines, in run order.
/// A recipe-load failure degrades to no task steps (the file still syncs).
fn ci_tasks(root: &Path) -> Vec<String> {
    let Ok((_, recipe, _)) = make::load_merged_recipe(&root.to_path_buf(), &MakeOptions::default())
    else {
        return Vec::new();
    };
    CI_TASKS
        .iter()
        .filter(|t| recipe.tasks.contains_key(**t))
        .map(|t| (*t).to_string())
        .collect()
}

/// Team login info when the project is wired to a registry. `repository` is the
/// `--repository` value, else a `<org>/<repo>` placeholder (flagged).
fn team_info(root: &Path, repository: Option<&str>) -> Option<TeamInfo> {
    let record = team::read_record(root)?;
    let (repository, placeholder) = match repository {
        Some(r) => (r.to_string(), false),
        None => ("<org>/<repo>".to_string(), true),
    };
    Some(TeamInfo {
        registry: record.registry,
        repository,
        repository_is_placeholder: placeholder,
    })
}

const HEADER: &str = "\
# Generated by `bougie ci init` — reproduces the local bougie toolchain so CI and
# your laptop run identically. Safe to edit; re-run with --force to regenerate.
";

fn render_github(plan: &Plan) -> String {
    let mut w = String::new();
    w.push_str(HEADER);
    w.push_str(
        "name: ci\non: [push, pull_request]\njobs:\n  bougie:\n    runs-on: ubuntu-latest\n",
    );
    w.push_str("    permissions:\n      contents: read\n");
    if plan.team.is_some() {
        w.push_str("      id-token: write   # for the zero-secret `bougie login --ci`\n");
    }
    w.push_str("    steps:\n");
    w.push_str("      - uses: actions/checkout@v4\n");
    w.push_str("      - uses: cresset-tools/setup-bougie@v1\n        with:\n");
    let _ = writeln!(w, "          version: '{}'", plan.bougie_version);
    let _ = writeln!(w, "          telemetry: '{}'", plan.telemetry);
    if let Some(t) = &plan.team {
        let _ = writeln!(
            w,
            "      - run: bougie login --ci --repository {} {}",
            t.repository, t.registry
        );
    }
    w.push_str("      - run: bougie sync\n");
    if !plan.services.is_empty() {
        let _ = writeln!(
            w,
            "      - run: bougie up --detach {}",
            plan.services.join(" ")
        );
    }
    if plan.tasks.is_empty() {
        w.push_str(
            "      # Add task steps, e.g. `- run: bougie make test` (see `bougie make --list`).\n",
        );
    } else {
        for task in &plan.tasks {
            let _ = writeln!(w, "      - run: bougie make {task}");
        }
    }
    w
}

fn render_gitlab(plan: &Plan) -> String {
    let mut w = String::new();
    w.push_str(HEADER);
    w.push_str("include:\n");
    w.push_str(
        "  # Pin the component to a released version — see gitlab.com/cresset-tools/components\n",
    );
    w.push_str("  - component: gitlab.com/cresset-tools/components/bougie@main\n    inputs:\n");
    let _ = writeln!(w, "      version: '{}'", plan.bougie_version);
    let _ = writeln!(w, "      telemetry: '{}'", plan.telemetry);
    w.push_str("ci:\n  extends: .bougie\n");
    if plan.team.is_some() {
        w.push_str("  id_tokens:\n    SCONCE_ID_TOKEN:\n      aud: sconce\n");
    }
    w.push_str("  script:\n");
    if let Some(t) = &plan.team {
        let _ = writeln!(
            w,
            "    - bougie login --ci --repository {} {}",
            t.repository, t.registry
        );
    }
    w.push_str("    - bougie sync\n");
    if !plan.services.is_empty() {
        let _ = writeln!(w, "    - bougie up --detach {}", plan.services.join(" "));
    }
    if plan.tasks.is_empty() {
        w.push_str("    # Add task steps, e.g. `- bougie make test` (see `bougie make --list`).\n");
    } else {
        for task in &plan.tasks {
            let _ = writeln!(w, "    - bougie make {task}");
        }
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(team: bool, services: &[&str], tasks: &[&str]) -> Plan {
        Plan {
            bougie_version: "0.51.0".to_string(),
            telemetry: "true".to_string(),
            services: services.iter().map(|s| (*s).to_string()).collect(),
            tasks: tasks.iter().map(|s| (*s).to_string()).collect(),
            team: team.then(|| TeamInfo {
                registry: "https://packages.acme.com".to_string(),
                repository: "acme/app".to_string(),
                repository_is_placeholder: false,
            }),
        }
    }

    #[test]
    fn github_team_project_wires_login_services_and_tasks() {
        let y = render_github(&plan(true, &["mariadb", "redis"], &["lint", "test"]));
        assert!(y.contains("uses: cresset-tools/setup-bougie@v1"), "{y}");
        assert!(y.contains("version: '0.51.0'"), "{y}");
        assert!(y.contains("telemetry: 'true'"), "{y}");
        assert!(y.contains("id-token: write"), "{y}");
        assert!(
            y.contains("bougie login --ci --repository acme/app https://packages.acme.com"),
            "{y}"
        );
        assert!(y.contains("bougie up --detach mariadb redis"), "{y}");
        assert!(y.contains("bougie make lint"), "{y}");
        assert!(y.contains("bougie make test"), "{y}");
        // No toolchain details are ever emitted.
        assert!(!y.contains("php-version"), "{y}");
    }

    #[test]
    fn github_non_team_omits_login_and_id_token() {
        let y = render_github(&plan(false, &[], &["test"]));
        assert!(!y.contains("id-token"), "{y}");
        assert!(!y.contains("login --ci"), "{y}");
        // No services declared → no `up` line.
        assert!(!y.contains("bougie up"), "{y}");
        assert!(y.contains("bougie sync"), "{y}");
        assert!(y.contains("bougie make test"), "{y}");
    }

    #[test]
    fn no_tasks_leaves_a_helpful_placeholder() {
        let y = render_github(&plan(false, &[], &[]));
        assert!(y.contains("# Add task steps"), "{y}");
        assert!(!y.contains("bougie make lint"), "{y}");
    }

    #[test]
    fn gitlab_team_uses_component_and_id_tokens() {
        let y = render_gitlab(&plan(true, &["mariadb"], &["test"]));
        assert!(
            y.contains("component: gitlab.com/cresset-tools/components/bougie@"),
            "{y}"
        );
        assert!(y.contains("version: '0.51.0'"), "{y}");
        assert!(y.contains("SCONCE_ID_TOKEN:"), "{y}");
        assert!(y.contains("aud: sconce"), "{y}");
        assert!(
            y.contains("- bougie login --ci --repository acme/app"),
            "{y}"
        );
        assert!(y.contains("- bougie up --detach mariadb"), "{y}");
        assert!(y.contains("- bougie make test"), "{y}");
    }
}
