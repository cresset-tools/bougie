//! `bougie skill {install,print}` — the agent skill for bougie projects.
//!
//! Coding agents reach for the machine's `php` / `composer` by reflex, which
//! in a bougie project is either the wrong interpreter or no interpreter at
//! all. The fix is a skill file the agent loads before it touches anything:
//! it says the environment is bougie's, tells the agent to check whether the
//! site is up before running anything that needs it, and — because `bougie
//! start` downloads a toolchain and brings up shared services — to ask the
//! user rather than start it unprompted.
//!
//! One body, many conventions. Every agent wants the same prose in a
//! different wrapper — its own filename, its own YAML frontmatter, or no
//! frontmatter at all — so [`Target`] holds the per-agent layout and the
//! body is shared. Both live in the binary rather than shipping separately,
//! so the document can't drift from the CLI surface it describes and `self
//! update` makes a re-install current.
//!
//! Two shapes of destination, and the difference decides who may clobber
//! what:
//!
//! - **A dedicated file** (`SKILL.md`, `bougie.mdc`, …) is bougie's whole
//!   file. Re-installing over a differing one asks first, since the
//!   difference might be the user's edits.
//! - **A shared file** (`AGENTS.md`, `GEMINI.md`) belongs to the project and
//!   holds the user's own prose. bougie writes only between its markers and
//!   rewrites that block without asking — the markers say it's managed.

use bougie_cli::{OutputFormat, SkillAgent, SkillInstallArgs};
use bougie_output::output::{Render, emit};
use eyre::{Result, WrapErr, eyre};
use serde::Serialize;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The prose every agent gets, minus any frontmatter.
const BODY: &str = include_str!("skill_assets/body.md");

/// The one-line "when does this apply" summary. Frontmatter-bearing formats
/// key their activation off it, so it lives apart from the body and is
/// shared across them rather than retyped per agent.
const DESCRIPTION: &str = include_str!("skill_assets/description.txt");

/// Skill identifier — the `name` in Claude Code's frontmatter, and the stem
/// of every filename bougie writes.
const SKILL_NAME: &str = "bougie";

/// Fences for a shared file. Deliberately HTML comments: invisible when the
/// file renders, and greppable when it doesn't.
const BLOCK_START: &str = "<!-- bougie:start -->";
const BLOCK_END: &str = "<!-- bougie:end -->";

/// Agents offered at the prompt, best-known first. `Plain` is missing on
/// purpose — it has no location to offer, and exists for `--path`.
const PROMPTED: &[SkillAgent] = &[
    SkillAgent::Claude,
    SkillAgent::Agents,
    SkillAgent::Cursor,
    SkillAgent::Copilot,
    SkillAgent::Gemini,
];

// ---------------------------------------------------------------------------
// The per-agent layout table
// ---------------------------------------------------------------------------

/// Everything that differs between agents. One row per [`SkillAgent`].
struct Target {
    /// Name for prompts and messages.
    label: &'static str,
    /// Who else reads this file, when it's a shared convention.
    also: &'static str,
    /// File to write, relative to the project root. `None` for `Plain`,
    /// which only ever writes where `--path` says.
    project_rel: Option<&'static str>,
    /// File to write, relative to the home directory. `None` when the agent
    /// has no user-level file (Cursor keeps user rules in its settings;
    /// Copilot instructions are repository-scoped).
    user_rel: Option<&'static str>,
    /// Filename to use under `--path`.
    file_name: &'static str,
    /// Renders this agent's YAML frontmatter, body excluded. `None` writes
    /// the body bare.
    frontmatter: Option<fn() -> String>,
    /// Does bougie share this file with the user's own content?
    shared: bool,
    /// Paths (project-relative) whose presence means this agent is already
    /// in use here, so the prompt can lead with it.
    detect: &'static [&'static str],
}

fn target(agent: SkillAgent) -> Target {
    match agent {
        SkillAgent::Claude => Target {
            label: "Claude Code",
            also: "",
            project_rel: Some(".claude/skills/bougie/SKILL.md"),
            user_rel: Some(".claude/skills/bougie/SKILL.md"),
            file_name: "SKILL.md",
            frontmatter: Some(|| format!("name: {SKILL_NAME}\ndescription: {}", description())),
            shared: false,
            detect: &[".claude"],
        },
        SkillAgent::Agents => Target {
            label: "AGENTS.md",
            also: "Codex, opencode, Jules, Amp, Zed, …",
            project_rel: Some("AGENTS.md"),
            user_rel: None,
            file_name: "AGENTS.md",
            frontmatter: None,
            shared: true,
            detect: &["AGENTS.md"],
        },
        SkillAgent::Cursor => Target {
            label: "Cursor",
            also: "",
            project_rel: Some(".cursor/rules/bougie.mdc"),
            // Cursor's user-level rules live in its settings UI, not a file.
            user_rel: None,
            file_name: "bougie.mdc",
            // `alwaysApply: false` + a description is Cursor's
            // agent-requested rule: pulled in when the description matches,
            // which is the same contract as a skill.
            frontmatter: Some(|| {
                format!("description: {}\nglobs:\nalwaysApply: false", description())
            }),
            shared: false,
            detect: &[".cursor"],
        },
        SkillAgent::Copilot => Target {
            label: "GitHub Copilot",
            also: "",
            project_rel: Some(".github/instructions/bougie.instructions.md"),
            // Repository-scoped by design; VS Code's user-profile
            // instructions aren't a path bougie can write portably.
            user_rel: None,
            file_name: "bougie.instructions.md",
            frontmatter: Some(|| "applyTo: '**'".to_string()),
            shared: false,
            // `.github` alone means nothing — nearly every repo has one.
            detect: &[".github/instructions", ".github/copilot-instructions.md"],
        },
        SkillAgent::Gemini => Target {
            label: "Gemini CLI",
            also: "",
            project_rel: Some("GEMINI.md"),
            user_rel: Some(".gemini/GEMINI.md"),
            file_name: "GEMINI.md",
            frontmatter: None,
            shared: true,
            detect: &["GEMINI.md", ".gemini"],
        },
        SkillAgent::Plain => Target {
            label: "plain markdown",
            also: "",
            project_rel: None,
            user_rel: None,
            file_name: "bougie.md",
            frontmatter: None,
            shared: false,
            detect: &[],
        },
    }
}

/// `--agent` spelling, for messages that suggest a flag.
fn agent_key(agent: SkillAgent) -> &'static str {
    match agent {
        SkillAgent::Claude => "claude",
        SkillAgent::Agents => "agents",
        SkillAgent::Cursor => "cursor",
        SkillAgent::Copilot => "copilot",
        SkillAgent::Gemini => "gemini",
        SkillAgent::Plain => "plain",
    }
}

/// The description as a single line — the asset carries a trailing newline
/// so it stays a well-formed text file, and YAML wants it gone.
fn description() -> &'static str {
    DESCRIPTION.trim_end_matches('\n')
}

/// The document as this agent wants it: frontmatter (if any) then the body.
fn render(agent: SkillAgent) -> String {
    match target(agent).frontmatter {
        Some(front) => format!("---\n{}\n---\n\n{BODY}", front()),
        None => BODY.to_string(),
    }
}

// ---------------------------------------------------------------------------
// `bougie skill print`
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct SkillResult {
    pub schema_version: u32,
    pub name: &'static str,
    pub agent: &'static str,
    pub content: String,
}

impl Render for SkillResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        // Verbatim, no trailing newline of our own: the body already ends
        // in one, and a redirect of this output has to be a valid file.
        write!(w, "{}", self.content)
    }
}

pub fn print(format: OutputFormat, agent: SkillAgent) -> Result<ExitCode> {
    let result = SkillResult {
        schema_version: 1,
        name: SKILL_NAME,
        agent: agent_key(agent),
        // What `install` would put in an empty file — so for a shared
        // convention this carries the markers, and redirecting it produces
        // a file a later `install` updates in place instead of appending to.
        content: splice("", agent, &render(agent)),
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------
// `bougie skill install`
// ---------------------------------------------------------------------------

/// What the install did, so a scripted caller can tell a no-op from a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallStatus {
    /// Nothing of bougie's was there before.
    Installed,
    /// Something was, saying something else, and we replaced it.
    Updated,
    /// Something was, byte-identical — nothing written.
    Unchanged,
    /// A differing file was there and the user declined to replace it. A
    /// choice, not a failure: nothing written, exit 0.
    Kept,
}

impl InstallStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Installed => "installed",
            Self::Updated => "updated",
            Self::Unchanged => "unchanged",
            Self::Kept => "kept",
        }
    }

    /// Did this status come with a write?
    fn wrote(self) -> bool {
        matches!(self, Self::Installed | Self::Updated)
    }
}

/// One agent's outcome. A run installs for as many agents as were asked
/// for, so the result is always a list — even when it holds one row.
#[derive(Debug, Serialize)]
pub struct SkillInstall {
    pub agent: &'static str,
    pub path: PathBuf,
    pub status: InstallStatus,
}

#[derive(Debug, Serialize)]
pub struct SkillInstallResult {
    pub schema_version: u32,
    pub name: &'static str,
    pub installs: Vec<SkillInstall>,
}

impl Render for SkillInstallResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for install in &self.installs {
            writeln!(
                w,
                "{:<10} {:<15} {}",
                install.status.label(),
                install.agent,
                install.path.display()
            )?;
        }
        Ok(())
    }
}

pub fn install(format: OutputFormat, args: &SkillInstallArgs) -> Result<ExitCode> {
    let interactive = matches!(format, OutputFormat::Text) && io::stdin().is_terminal();
    let agents = if args.agent.is_empty() {
        // Asked when there's someone to ask; otherwise the format bougie
        // authors natively. Unlike a location, a format guessed wrong is
        // visible in the file you named and costs a re-run, so it defaults
        // rather than erroring.
        if interactive {
            prompt_for_agents()?
        } else {
            vec![SkillAgent::Claude]
        }
    } else {
        // `--agent claude --agent claude` is one install, not two.
        let mut seen = Vec::new();
        for &agent in &args.agent {
            if !seen.contains(&agent) {
                seen.push(agent);
            }
        }
        seen
    };

    // One location question for the whole set — every agent puts its own
    // file under it.
    let location = resolve_location(&agents, args, interactive)?;

    // Resolve every destination before writing any of them. A selection
    // holding one agent the chosen scope can't serve fails the run outright
    // rather than half-installing the ones that came before it.
    let dests: Vec<(SkillAgent, PathBuf)> = agents
        .iter()
        .map(|&agent| location.dest_for(agent).map(|dest| (agent, dest)))
        .collect::<Result<_>>()?;

    let mut installs = Vec::with_capacity(dests.len());
    for (agent, dest) in dests {
        installs.push(install_one(agent, dest, args, interactive)?);
    }

    let result = SkillInstallResult {
        schema_version: 1,
        name: SKILL_NAME,
        installs,
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

/// Write (or decline to write) one agent's file.
fn install_one(
    agent: SkillAgent,
    dest: PathBuf,
    args: &SkillInstallArgs,
    interactive: bool,
) -> Result<SkillInstall> {
    let existing = match std::fs::read_to_string(&dest) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(eyre!("reading {}: {e}", dest.display())),
    };
    let updated = splice(existing.as_deref().unwrap_or(""), agent, &render(agent));

    // Was any of bougie's content there already? For a shared file that's
    // the block, not the file — adding a block to someone's existing
    // AGENTS.md is an install, not an update.
    let had_ours = existing
        .as_deref()
        .is_some_and(|current| !target(agent).shared || current.contains(BLOCK_START));

    let status = if existing.as_deref() == Some(updated.as_str()) {
        InstallStatus::Unchanged
    } else if !had_ours {
        InstallStatus::Installed
    } else if target(agent).shared || confirm_overwrite(&dest, args.force, interactive)? {
        // A shared file's block is bougie's by declaration, so rewriting it
        // needs no permission; a file bougie owns whole might hold edits.
        InstallStatus::Updated
    } else {
        InstallStatus::Kept
    };

    if status.wrote() {
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir).wrap_err_with(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(&dest, &updated).wrap_err_with(|| format!("writing {}", dest.display()))?;
    }

    Ok(SkillInstall {
        agent: agent_key(agent),
        path: dest,
        status,
    })
}

/// Put `wanted` into `existing` the way this agent's file wants it.
///
/// A dedicated file is simply replaced. A shared one keeps everything
/// outside bougie's markers: an existing block is rewritten in place, and a
/// file without one gets the block appended.
fn splice(existing: &str, agent: SkillAgent, wanted: &str) -> String {
    if !target(agent).shared {
        return wanted.to_string();
    }
    let block = format!(
        "{BLOCK_START}\n\
         <!-- Managed by `bougie skill install`. Edits inside this block are overwritten. -->\n\n\
         {wanted}{BLOCK_END}\n"
    );
    let Some((before, rest)) = existing.split_once(BLOCK_START) else {
        if existing.trim().is_empty() {
            return block;
        }
        // Append, with exactly one blank line between their prose and ours.
        return format!("{}\n\n{block}", existing.trim_end());
    };
    // An unterminated start marker (hand-mangled) takes everything after it
    // — better to absorb the mess than to leave two start markers behind.
    let after = rest.split_once(BLOCK_END).map_or("", |(_, tail)| tail);
    format!("{before}{block}{}", after.trim_start_matches('\n'))
}

/// Where a run installs, resolved once and applied to every agent. Holds
/// the root it resolved so a multi-agent run doesn't re-walk the tree (or,
/// worse, disagree with itself) per agent.
enum Location {
    /// Under a project root, at each agent's project-level path.
    Project(PathBuf),
    /// Under the home directory, at each agent's user-level path.
    User(PathBuf),
    /// All in one directory, each under the agent's filename.
    Dir(PathBuf),
}

impl Location {
    fn dest_for(&self, agent: SkillAgent) -> Result<PathBuf> {
        let t = target(agent);
        match self {
            Self::Project(root) => {
                let rel = t.project_rel.ok_or_else(|| {
                    eyre!(
                        "`--agent {}` has no default location — name a directory with \
                         `--path <DIR>`",
                        agent_key(agent)
                    )
                })?;
                Ok(root.join(rel))
            }
            Self::User(home) => {
                let rel = t.user_rel.ok_or_else(|| {
                    eyre!(
                        "{} has no user-level location — install it per project (`--project`) \
                         or name a directory (`--path <DIR>`)",
                        t.label
                    )
                })?;
                Ok(home.join(rel))
            }
            Self::Dir(dir) => Ok(dir.join(t.file_name)),
        }
    }
}

/// The location for this run, from the flags — asking for whatever they
/// left open. `agents` only narrows the *offered* options: a scope an agent
/// can't use is left off the menu rather than failing after the choice.
fn resolve_location(
    agents: &[SkillAgent],
    args: &SkillInstallArgs,
    interactive: bool,
) -> Result<Location> {
    if let Some(path) = &args.path {
        return Ok(Location::Dir(path.clone()));
    }
    if args.project {
        return Ok(Location::Project(super::server::locate_project_root()?));
    }
    if args.user {
        return Ok(Location::User(bougie_paths::home_dir()?));
    }
    prompt_for_location(agents, interactive)
}

/// Ask which agents, leading with any already in use in this project.
/// Several may be chosen at once — teams mix editors, and one project often
/// wants both a `SKILL.md` and an `AGENTS.md`.
fn prompt_for_agents() -> Result<Vec<SkillAgent>> {
    let root = super::server::locate_project_root().ok();
    let mut rows: Vec<(SkillAgent, bool)> = PROMPTED
        .iter()
        .map(|&a| (a, root.as_deref().is_some_and(|r| in_use(a, r))))
        .collect();
    // A stable sort keeps the curated order within each group, so the list
    // only ever reorders to float what's already here to the top.
    rows.sort_by_key(|(_, detected)| !*detected);

    eprintln!("Which agents should the {SKILL_NAME} skill be written for?");
    for (i, (agent, detected)) in rows.iter().enumerate() {
        let t = target(*agent);
        let also = if t.also.is_empty() {
            String::new()
        } else {
            format!(" — also {}", t.also)
        };
        let here = if *detected { " [in use here]" } else { "" };
        eprintln!(
            "  {}) {:<16} {}{also}{here}",
            i + 1,
            t.label,
            t.project_rel.unwrap_or("")
        );
    }
    let chosen = ask_indices(rows.len())?;
    Ok(chosen.into_iter().map(|i| rows[i].0).collect())
}

/// Has this agent left traces in the project? Purely a prompt nicety.
fn in_use(agent: SkillAgent, root: &Path) -> bool {
    target(agent).detect.iter().any(|p| root.join(p).exists())
}

/// Ask where the chosen agents' files go: this project, your account, or a
/// directory you name. A scope only one of them can't use is left off the
/// menu — choosing it would fail for that agent after the fact.
fn prompt_for_location(agents: &[SkillAgent], interactive: bool) -> Result<Location> {
    // Nobody to ask: name the flags rather than guessing a destination and
    // writing into it. Checked before resolving any candidate path, so a
    // scripted run gets this rather than "could not resolve the home
    // directory" from building a list it will never see.
    if !interactive {
        return Err(eyre!(
            "no install location given, and input isn't interactive — re-run with \
             `--project`, `--user`, or `--path <DIR>`"
        ));
    }

    let mut options: Vec<(&str, Option<Location>)> = Vec::new();
    if agents.iter().all(|&a| target(a).project_rel.is_some())
        && let Ok(root) = super::server::locate_project_root()
    {
        options.push(("this project", Some(Location::Project(root))));
    }
    if agents.iter().all(|&a| target(a).user_rel.is_some()) {
        options.push((
            "your account",
            Some(Location::User(bougie_paths::home_dir()?)),
        ));
    }
    options.push(("somewhere else", None));

    eprintln!("Where should {} go?", one_or_several(agents));
    for (i, (label, location)) in options.iter().enumerate() {
        match location {
            // With several agents the paths differ per agent, so show the
            // directory they share rather than a list of files.
            Some(loc) => eprintln!("  {}) {label:<14} {}", i + 1, describe(loc, agents)),
            None => eprintln!("  {}) {label:<14} (enter a directory)", i + 1),
        }
    }
    let choice = ask_index(options.len())?;
    match options.swap_remove(choice).1 {
        Some(location) => Ok(location),
        None => Ok(Location::Dir(prompt_for_path()?)),
    }
}

/// "it" for one agent, "they" for several — the location prompt reads as
/// prose either way.
fn one_or_several(agents: &[SkillAgent]) -> &'static str {
    if agents.len() == 1 { "it" } else { "they" }
}

/// What a location resolves to, for the prompt: the exact file when there's
/// one agent, and the root they share when there are several.
fn describe(location: &Location, agents: &[SkillAgent]) -> String {
    if let [agent] = agents
        && let Ok(dest) = location.dest_for(*agent)
    {
        return dest.display().to_string();
    }
    match location {
        Location::Project(root) | Location::User(root) | Location::Dir(root) => {
            root.display().to_string()
        }
    }
}

/// Read a 1-based menu choice, re-asking until it's in range. Returns a
/// 0-based index; an empty line takes the first option.
fn ask_index(len: usize) -> Result<usize> {
    loop {
        eprint!("Choice [1]: ");
        io::stderr().flush().ok();
        let mut line = String::new();
        let read = io::stdin()
            .read_line(&mut line)
            .map_err(|e| eyre!("reading choice: {e}"))?;
        let ans = line.trim();
        // Empty line takes the default; EOF means the terminal went away
        // mid-prompt, and defaulting a write on that would be presumptuous.
        if ans.is_empty() {
            if read == 0 {
                return Err(eyre!("no choice given"));
            }
            return Ok(0);
        }
        match ans.parse::<usize>() {
            Ok(n) if (1..=len).contains(&n) => return Ok(n - 1),
            _ => eprintln!("pick a number between 1 and {len}"),
        }
    }
}

/// Read one *or several* 1-based menu choices — `2`, `1,3`, `1 3 4` — and
/// return them as 0-based indices in the order given, without repeats. An
/// empty line takes the first option.
fn ask_indices(len: usize) -> Result<Vec<usize>> {
    loop {
        eprint!("Choice [1] (several: 1,3): ");
        io::stderr().flush().ok();
        let mut line = String::new();
        let read = io::stdin()
            .read_line(&mut line)
            .map_err(|e| eyre!("reading choice: {e}"))?;
        let ans = line.trim();
        if ans.is_empty() {
            if read == 0 {
                return Err(eyre!("no choice given"));
            }
            return Ok(vec![0]);
        }
        match parse_indices(ans, len) {
            Some(indices) => return Ok(indices),
            None => eprintln!("pick one or more numbers between 1 and {len}, e.g. `1,3`"),
        }
    }
}

/// Parse a menu answer into 0-based indices. `None` if any token isn't a
/// number in range, so the caller can re-ask rather than act on half of it.
fn parse_indices(answer: &str, len: usize) -> Option<Vec<usize>> {
    let mut out: Vec<usize> = Vec::new();
    for token in answer.split([',', ' ']).filter(|t| !t.is_empty()) {
        let n = token.parse::<usize>().ok()?;
        if !(1..=len).contains(&n) {
            return None;
        }
        if !out.contains(&(n - 1)) {
            out.push(n - 1);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Read a directory for the "somewhere else" branch. Expands a leading `~`
/// ourselves — it was typed at our prompt, so no shell got to do it.
fn prompt_for_path() -> Result<PathBuf> {
    eprint!("Directory: ");
    io::stderr().flush().ok();
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .map_err(|e| eyre!("reading directory: {e}"))?;
    let ans = line.trim();
    if ans.is_empty() {
        return Err(eyre!("no directory given"));
    }
    expand_tilde(ans)
}

/// `~` / `~/x` → the home directory. A bare `~user` is left alone: resolving
/// another user's home is a different (and platform-specific) question.
fn expand_tilde(input: &str) -> Result<PathBuf> {
    let rest = match input.strip_prefix('~') {
        Some("") => return bougie_paths::home_dir(),
        Some(rest) if rest.starts_with('/') || rest.starts_with('\\') => &rest[1..],
        _ => return Ok(PathBuf::from(input)),
    };
    Ok(bougie_paths::home_dir()?.join(rest))
}

/// A differing file bougie owns outright is already at the destination.
/// `--force` says go ahead; a terminal gets asked; a script gets an error
/// naming the flag. `Ok(false)` is a decline — the caller keeps the file and
/// still exits 0. Shared files never reach here; only their block changes.
fn confirm_overwrite(dest: &Path, force: bool, interactive: bool) -> Result<bool> {
    if force {
        return Ok(true);
    }
    if !interactive {
        return Err(eyre!(
            "{} already exists with different contents — re-run with `--force` to replace it",
            dest.display()
        ));
    }
    eprintln!(
        "bougie: {} already exists and differs from this bougie's skill \
         (an older version, or your own edits).",
        dest.display()
    );
    eprint!("Replace it? [y/N] ");
    io::stderr().flush().ok();
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .map_err(|e| eyre!("reading confirmation: {e}"))?;
    let ans = line.trim().to_ascii_lowercase();
    Ok(ans == "y" || ans == "yes")
}

#[cfg(test)]
mod tests {
    use super::{BODY, PROMPTED, SkillAgent, expand_tilde, parse_indices, render, splice, target};
    use std::path::Path;

    /// The frontmatter is what an agent matches on; without it a Claude
    /// skill is just markdown and never loads.
    #[test]
    fn claude_render_opens_with_yaml_frontmatter() {
        let text = render(SkillAgent::Claude);
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("---"));
        let front: Vec<&str> = lines.by_ref().take_while(|l| *l != "---").collect();
        assert!(
            front.iter().any(|l| l.starts_with("name: ")),
            "frontmatter needs a name"
        );
        assert!(
            front.iter().any(|l| l.starts_with("description: ")),
            "frontmatter needs a description"
        );
        // One line each — a raw newline inside a YAML scalar breaks it.
        assert_eq!(front.len(), 2, "frontmatter should be two lines: {front:?}");
    }

    /// Every agent gets the same prose, whatever the wrapper.
    #[test]
    fn every_agent_carries_the_body() {
        for &agent in PROMPTED.iter().chain(&[SkillAgent::Plain]) {
            let text = render(agent);
            assert!(text.ends_with(BODY), "{agent:?} should end with the body");
            assert!(text.ends_with('\n'), "{agent:?} should end with a newline");
            if target(agent).frontmatter.is_none() {
                assert_eq!(text, BODY, "{agent:?} wants no frontmatter");
            }
        }
    }

    /// The two instructions the skill exists to deliver. If a rewrite drops
    /// either, the skill has stopped doing its job.
    #[test]
    fn states_the_check_then_ask_protocol() {
        assert!(
            BODY.contains("bougie doctor"),
            "must name the read-only check"
        );
        assert!(
            BODY.contains("stop and ask the user"),
            "must tell the agent to ask before bringing the project up"
        );
    }

    /// A skill that names a command the CLI doesn't have sends agents into
    /// usage errors. Nothing here is fancy — it just pins the verbs.
    #[test]
    fn names_only_real_verbs() {
        for verb in [
            "bougie start",
            "bougie sync",
            "bougie run",
            "bougie service list",
            "bougie service status",
            "bougie server status",
            "bougie php install",
            "bougie ext add",
        ] {
            assert!(BODY.contains(verb), "{verb} should appear in the skill");
        }
        // `up` / `down` live under `service` now; a bare `bougie up` is a
        // usage error (see bougie-cli's `top_level_up_down_are_gone`).
        assert!(!BODY.contains("`bougie up`"), "no top-level `up` exists");
    }

    /// Every prompted agent needs somewhere to offer, and every agent needs
    /// a filename for `--path`.
    #[test]
    fn prompted_agents_have_a_project_location() {
        for &agent in PROMPTED {
            let t = target(agent);
            assert!(t.project_rel.is_some(), "{agent:?} needs a project path");
            assert!(!t.file_name.is_empty());
        }
        assert!(
            target(SkillAgent::Plain).project_rel.is_none(),
            "plain is --path only"
        );
    }

    #[test]
    fn dedicated_files_are_replaced_wholesale() {
        let wanted = render(SkillAgent::Claude);
        assert_eq!(
            splice("whatever was here", SkillAgent::Claude, &wanted),
            wanted
        );
        assert_eq!(splice("", SkillAgent::Claude, &wanted), wanted);
    }

    #[test]
    fn shared_files_keep_the_user_prose_around_the_block() {
        let wanted = render(SkillAgent::Agents);
        let first = splice(
            "# My project\n\nBuild with make.\n",
            SkillAgent::Agents,
            &wanted,
        );
        assert!(first.starts_with("# My project\n\nBuild with make.\n\n"));
        assert!(first.contains("<!-- bougie:start -->"));
        assert!(first.trim_end().ends_with("<!-- bougie:end -->"));
        assert!(first.contains("The PHP environment here"));

        // Re-splicing is idempotent, and never duplicates the block.
        let second = splice(&first, SkillAgent::Agents, &wanted);
        assert_eq!(first, second);
        assert_eq!(second.matches("<!-- bougie:start -->").count(), 1);
    }

    #[test]
    fn shared_files_update_only_the_block() {
        let wanted = render(SkillAgent::Agents);
        let existing = "# My project\n\n<!-- bougie:start -->\nstale guidance\n\
                        <!-- bougie:end -->\n\nRun tests with make.\n";
        let out = splice(existing, SkillAgent::Agents, &wanted);
        assert!(
            !out.contains("stale guidance"),
            "the block must be replaced"
        );
        assert!(out.starts_with("# My project\n\n"), "prose above survives");
        assert!(
            out.ends_with("Run tests with make.\n"),
            "prose below survives"
        );
        assert_eq!(out.matches("<!-- bougie:end -->").count(), 1);
    }

    /// An empty (or whitespace-only) shared file gets the block alone, with
    /// no leading blank lines.
    #[test]
    fn shared_files_start_clean_when_empty() {
        let wanted = render(SkillAgent::Gemini);
        let out = splice("\n \n", SkillAgent::Gemini, &wanted);
        assert!(out.starts_with("<!-- bougie:start -->"));
    }

    /// The multi-select answer: several spellings, no repeats, and a
    /// rejection rather than a partial read when any token is bad.
    #[test]
    fn menu_answers_parse_into_indices() {
        assert_eq!(parse_indices("2", 5), Some(vec![1]));
        assert_eq!(parse_indices("1,3", 5), Some(vec![0, 2]));
        assert_eq!(parse_indices("1 3 4", 5), Some(vec![0, 2, 3]));
        assert_eq!(parse_indices(" 3 , 1 ", 5), Some(vec![2, 0]));
        // Order is the user's; a repeat collapses.
        assert_eq!(parse_indices("3,1,3", 5), Some(vec![2, 0]));
        // One bad token rejects the whole answer — acting on the good half
        // would install somewhere they didn't confirm.
        assert_eq!(parse_indices("1,9", 5), None);
        assert_eq!(parse_indices("1,x", 5), None);
        assert_eq!(parse_indices("0", 5), None);
        assert_eq!(parse_indices(",", 5), None);
    }

    #[test]
    fn tilde_expands_only_for_the_current_user() {
        let home = bougie_paths::home_dir().unwrap();
        assert_eq!(expand_tilde("~").unwrap(), home);
        assert_eq!(
            expand_tilde("~/skills/bougie").unwrap(),
            home.join("skills/bougie")
        );
        // Another user's home isn't ours to resolve; and an ordinary path
        // passes through untouched.
        assert_eq!(
            expand_tilde("~root/skills").unwrap(),
            Path::new("~root/skills")
        );
        assert_eq!(
            expand_tilde("/tmp/skills").unwrap(),
            Path::new("/tmp/skills")
        );
    }
}
