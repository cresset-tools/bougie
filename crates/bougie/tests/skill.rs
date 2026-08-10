//! `bougie skill {print,install}`: each agent's convention renders around
//! the one body, install resolves its destination from the flags — reporting
//! installed / unchanged / updated — refuses to clobber a differing file it
//! owns without `--force`, and edits only its own block in a shared file.

use assert_cmd::Command;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

/// A bougie invocation with the ambient environment pinned: `HOME` /
/// `USERPROFILE` decide where `--user` lands, so they must not be the real
/// ones. Everything else is the usual isolation.
fn bougie(home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("bougie").expect("bougie binary");
    cmd.env("HOME", home)
        .env("USERPROFILE", home)
        .env("BOUGIE_HOME", home.join("bougie-home"))
        .env("BOUGIE_CACHE", home.join("bougie-cache"))
        .env_remove("BOUGIE_TELEMETRY")
        .env_remove("DO_NOT_TRACK")
        .env_remove("RUST_LOG");
    cmd
}

fn printed_skill(home: &Path, agent: &str) -> String {
    let out = bougie(home)
        .args(["skill", "print", "--agent", agent])
        .output()
        .unwrap();
    assert!(out.status.success(), "skill print --agent {agent} failed");
    String::from_utf8(out.stdout).expect("skill is utf-8")
}

#[test]
fn print_emits_a_well_formed_skill_md() {
    let home = TempDir::new().unwrap();
    let text = printed_skill(home.path(), "claude");

    assert!(text.starts_with("---\n"), "needs YAML frontmatter");
    assert!(text.contains("\nname: bougie\n"));
    assert!(text.contains("\ndescription: "));
    assert!(
        text.ends_with('\n'),
        "a redirect must yield a newline-terminated file"
    );
    // `claude` is the default, so a bare `print` is the same document.
    let bare = bougie(home.path())
        .args(["skill", "print"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(bare.stdout).unwrap(), text);
}

/// Each agent gets its own wrapper around one body.
#[test]
fn print_renders_each_agents_convention() {
    let home = TempDir::new().unwrap();
    let body_marker = "The PHP environment here is **not** the machine's PHP.";

    for agent in ["claude", "agents", "cursor", "copilot", "gemini", "plain"] {
        let text = printed_skill(home.path(), agent);
        assert!(text.contains(body_marker), "{agent} must carry the body");
    }

    // Cursor's agent-requested rule keys off the description.
    let cursor = printed_skill(home.path(), "cursor");
    assert!(cursor.starts_with("---\ndescription: "));
    assert!(cursor.contains("\nalwaysApply: false\n"));

    // Copilot instructions apply repo-wide.
    assert!(printed_skill(home.path(), "copilot").starts_with("---\napplyTo: '**'\n---\n"));

    // Nothing wraps a plain file.
    assert!(printed_skill(home.path(), "plain").starts_with("# bougie\n"));

    // A shared convention prints the managed block, so redirecting it
    // yields a file a later install updates in place.
    let agents = printed_skill(home.path(), "agents");
    assert!(agents.starts_with("<!-- bougie:start -->\n"));
    assert!(agents.trim_end().ends_with("<!-- bougie:end -->"));
}

#[test]
fn install_reports_installed_then_unchanged() {
    let home = TempDir::new().unwrap();
    let dest = home.path().join("skills").join("bougie");

    let out = bougie(home.path())
        .args(["skill", "install", "--path"])
        .arg(&dest)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("installed"));

    // The written file is exactly what `print` emits.
    let written = fs::read_to_string(dest.join("SKILL.md")).unwrap();
    assert_eq!(written, printed_skill(home.path(), "claude"));

    // A second install is a no-op, not a rewrite.
    bougie(home.path())
        .args(["skill", "install", "--path"])
        .arg(&dest)
        .assert()
        .success()
        .stdout(predicates::str::starts_with("unchanged"));
}

#[test]
fn install_wont_clobber_a_differing_file_without_force() {
    let home = TempDir::new().unwrap();
    let dest = home.path().join("skills").join("bougie");
    fs::create_dir_all(&dest).unwrap();
    fs::write(dest.join("SKILL.md"), "my own notes\n").unwrap();

    // Non-interactive (no tty under the test harness) — the flag is named
    // rather than guessed at.
    bougie(home.path())
        .args(["skill", "install", "--path"])
        .arg(&dest)
        .assert()
        .failure()
        .stderr(predicates::str::contains("--force"));
    assert_eq!(
        fs::read_to_string(dest.join("SKILL.md")).unwrap(),
        "my own notes\n",
        "the refusal must leave the file alone"
    );

    bougie(home.path())
        .args(["skill", "install", "--force", "--path"])
        .arg(&dest)
        .assert()
        .success()
        .stdout(predicates::str::starts_with("updated"));
    assert_eq!(
        fs::read_to_string(dest.join("SKILL.md")).unwrap(),
        printed_skill(home.path(), "claude")
    );
}

#[test]
fn install_resolves_project_and_user_locations() {
    let home = TempDir::new().unwrap();
    let project = home.path().join("app");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("composer.json"), r#"{"name":"acme/app"}"#).unwrap();

    bougie(home.path())
        .current_dir(&project)
        .args(["skill", "install", "--project"])
        .assert()
        .success();
    assert!(
        project.join(".claude/skills/bougie/SKILL.md").is_file(),
        "--project writes under the project root"
    );

    // From a subdirectory, the project root is still the one with the
    // composer.json — not the cwd.
    let sub = project.join("app/code");
    fs::create_dir_all(&sub).unwrap();
    bougie(home.path())
        .current_dir(&sub)
        .args(["skill", "install", "--project"])
        .assert()
        .success()
        .stdout(predicates::str::starts_with("unchanged"));

    bougie(home.path())
        .current_dir(&project)
        .args(["skill", "install", "--user"])
        .assert()
        .success();
    assert!(
        home.path().join(".claude/skills/bougie/SKILL.md").is_file(),
        "--user writes under the home directory"
    );
}

#[test]
fn install_without_a_location_names_the_flags() {
    let home = TempDir::new().unwrap();
    // No tty under the harness, so there is nobody to ask.
    bougie(home.path())
        .args(["skill", "install"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--project"))
        .stderr(predicates::str::contains("--user"))
        .stderr(predicates::str::contains("--path"));
}

#[test]
fn location_flags_are_mutually_exclusive() {
    let home = TempDir::new().unwrap();
    for pair in [
        ["--project", "--user"],
        ["--project", "--path=/tmp/x"],
        ["--user", "--path=/tmp/x"],
    ] {
        bougie(home.path())
            .args(["skill", "install"])
            .args(pair)
            .assert()
            .failure()
            .stderr(predicates::str::contains("cannot be used with"));
    }
}

/// Always a list, so a caller parses one shape whether the run wrote one
/// file or five.
#[test]
fn json_reports_every_install() {
    let home = TempDir::new().unwrap();
    let dest = home.path().join("skills").join("bougie");

    let out = bougie(home.path())
        .args(["skill", "install", "--format", "json-v1", "--path"])
        .arg(&dest)
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["name"], "bougie");
    let installs = v["installs"].as_array().expect("installs is a list");
    assert_eq!(installs.len(), 1);
    assert_eq!(installs[0]["agent"], "claude");
    assert_eq!(installs[0]["status"], "installed");
    assert_eq!(
        installs[0]["path"],
        dest.join("SKILL.md").to_string_lossy().as_ref()
    );

    // Several agents, one directory: each lands under its own filename.
    let out = bougie(home.path())
        .args([
            "skill",
            "install",
            "--format",
            "json-v1",
            "--agent",
            "cursor,plain",
            "--path",
        ])
        .arg(&dest)
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let installs = v["installs"].as_array().unwrap();
    assert_eq!(installs.len(), 2);
    assert_eq!(installs[0]["agent"], "cursor");
    assert_eq!(installs[1]["agent"], "plain");
    assert!(dest.join("bougie.mdc").is_file());
    assert!(dest.join("bougie.md").is_file());
}

/// One run, several agents — repeated or comma-separated, deduplicated,
/// and reported one line each.
#[test]
fn installs_for_several_agents_at_once() {
    let home = TempDir::new().unwrap();
    let project = home.path().join("app");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("composer.json"), r#"{"name":"acme/app"}"#).unwrap();

    let out = bougie(home.path())
        .current_dir(&project)
        .args([
            "skill",
            "install",
            "--project",
            "--agent",
            "claude",
            "--agent",
            "cursor",
            "--agent",
            "agents",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 3, "one line per agent: {stdout}");
    for rel in [
        ".claude/skills/bougie/SKILL.md",
        ".cursor/rules/bougie.mdc",
        "AGENTS.md",
    ] {
        assert!(project.join(rel).is_file(), "{rel} should exist");
    }

    // The comma spelling is the same thing, and a repeat is not two installs.
    let out = bougie(home.path())
        .current_dir(&project)
        .args(["skill", "install", "--project", "--agent", "claude,claude"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.starts_with("unchanged"));
    assert_eq!(stdout.lines().count(), 1, "deduplicated: {stdout}");
}

/// `--user` with an agent that has no user-level file fails before writing
/// anything — a mixed selection isn't half-installed.
#[test]
fn a_scope_one_agent_cant_use_fails_the_run() {
    let home = TempDir::new().unwrap();

    bougie(home.path())
        .args(["skill", "install", "--user", "--agent", "claude,cursor"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no user-level location"));
    assert!(
        !home.path().join(".claude/skills/bougie/SKILL.md").exists(),
        "claude is first in the list, but must not be written when a later \
         agent can't be"
    );
}

/// Every agent lands on its own file, under the project root.
#[test]
fn each_agent_installs_to_its_own_location() {
    let home = TempDir::new().unwrap();
    let project = home.path().join("app");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("composer.json"), r#"{"name":"acme/app"}"#).unwrap();

    for (agent, rel) in [
        ("claude", ".claude/skills/bougie/SKILL.md"),
        ("agents", "AGENTS.md"),
        ("cursor", ".cursor/rules/bougie.mdc"),
        ("copilot", ".github/instructions/bougie.instructions.md"),
        ("gemini", "GEMINI.md"),
    ] {
        bougie(home.path())
            .current_dir(&project)
            .args(["skill", "install", "--project", "--agent", agent])
            .assert()
            .success()
            .stdout(predicates::str::contains(agent));
        assert!(project.join(rel).is_file(), "{agent} should write {rel}");
    }
}

/// A shared file belongs to the project: bougie owns a marked block in it
/// and leaves everything else, on the first install and on every re-install.
#[test]
fn shared_files_keep_surrounding_prose() {
    let home = TempDir::new().unwrap();
    let project = home.path().join("app");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("composer.json"), r#"{"name":"acme/app"}"#).unwrap();
    let agents_md = project.join("AGENTS.md");
    fs::write(&agents_md, "# acme/app\n\nRun the linter before pushing.\n").unwrap();

    let install = |expect: &'static str| {
        bougie(home.path())
            .current_dir(&project)
            .args(["skill", "install", "--project", "--agent", "agents"])
            .assert()
            .success()
            .stdout(predicates::str::starts_with(expect));
    };

    // Adding a block to a file that already had none is an install, even
    // though the file itself was already there.
    install("installed");
    let after = fs::read_to_string(&agents_md).unwrap();
    assert!(
        after.starts_with("# acme/app\n\nRun the linter before pushing.\n"),
        "their prose must survive: {after}"
    );
    assert!(after.contains("<!-- bougie:start -->"));
    assert!(after.contains("bougie doctor"));

    // Idempotent — no second block, no drift.
    install("unchanged");
    assert_eq!(fs::read_to_string(&agents_md).unwrap(), after);

    // A stale block is rewritten in place without asking, since the markers
    // declare it managed — and the prose on both sides is untouched.
    let stale = after.replace("bougie doctor", "bougie doktor");
    fs::write(&agents_md, &stale).unwrap();
    install("updated");
    assert_eq!(fs::read_to_string(&agents_md).unwrap(), after);
}

/// Not every agent has a user-level file, and `plain` has no default
/// location at all. Both say so, and name the flag that works.
#[test]
fn missing_locations_are_explained() {
    let home = TempDir::new().unwrap();
    let project = home.path().join("app");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("composer.json"), r#"{"name":"acme/app"}"#).unwrap();

    bougie(home.path())
        .current_dir(&project)
        .args(["skill", "install", "--user", "--agent", "cursor"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no user-level location"))
        .stderr(predicates::str::contains("--project"));

    bougie(home.path())
        .current_dir(&project)
        .args(["skill", "install", "--project", "--agent", "plain"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--path"));

    // With a path, `plain` writes a bare markdown file.
    let dir = project.join(".windsurf/rules");
    bougie(home.path())
        .current_dir(&project)
        .args(["skill", "install", "--agent", "plain", "--path"])
        .arg(&dir)
        .assert()
        .success();
    let written = fs::read_to_string(dir.join("bougie.md")).unwrap();
    assert!(
        written.starts_with("# bougie\n"),
        "no frontmatter for plain"
    );
}
