//! The event runner: walk a `scripts.<event>` list in order, executing each
//! entry, aborting on the first non-zero exit (matching Composer).

use std::collections::HashMap;
use std::collections::HashSet;
use std::process::Command;
use std::time::Duration;

use eyre::{bail, eyre, Result, WrapErr};
use wait_timeout::ChildExt;

use crate::{Entry, ScriptContext, Scripts};

/// The Composer callback that disables the per-process timeout for the rest
/// of the dispatch (`Composer\Config::disableProcessTimeout`). Recognised
/// specially because it has to mutate dispatch-local timeout state — a
/// registry callback (which only gets `&ScriptContext`) couldn't.
const DISABLE_TIMEOUT_CALLBACK: &str = "Composer\\Config::disableProcessTimeout";

/// What happened to one entry during a dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryOutcome {
    /// A process entry ran to a zero exit, or `@putenv` mutated the env.
    Ran,
    /// A PHP callback with no native handler — warned and skipped.
    SkippedCallback(String),
    /// A PHP callback served by a registered native handler.
    NativeCallback,
    /// An `@composer` subcommand bougie doesn't map — warned and skipped.
    SkippedComposer(String),
}

/// Dispatch a named event, running its entries in order. Returns one outcome
/// per entry (recursing into aliases inline). A non-zero process exit or an
/// alias cycle is an `Err` that aborts the event and the surrounding command.
///
/// `@putenv` mutations are scoped to this dispatch; the inherited process env
/// is untouched. Output streams straight to the user's stdout/stderr — scripts
/// are chatty and the user opted into running them.
pub fn dispatch(scripts: &Scripts, event: &str, ctx: &ScriptContext) -> Result<Vec<EntryOutcome>> {
    let mut env = seed_env(ctx);
    let mut seen = HashSet::new();
    let mut outcomes = Vec::new();
    // Per-process timeout, mutable across the dispatch: the
    // `disableProcessTimeout` callback flips it off for every subsequent
    // entry (matching Composer's process-wide ProcessExecutor::$timeout).
    let mut timeout = ctx.timeout;
    dispatch_inner(scripts, event, ctx, &mut env, &mut timeout, &mut seen, &mut outcomes)?;
    Ok(outcomes)
}

fn dispatch_inner(
    scripts: &Scripts,
    event: &str,
    ctx: &ScriptContext,
    env: &mut HashMap<String, String>,
    timeout: &mut Option<Duration>,
    seen: &mut HashSet<String>,
    outcomes: &mut Vec<EntryOutcome>,
) -> Result<()> {
    if !seen.insert(event.to_string()) {
        bail!("script alias cycle detected at `{event}`");
    }
    let Some(entries) = scripts.get(event) else {
        // An undefined event is a no-op, exactly as Composer treats a missing
        // listener list. (Aliases to undefined scripts also no-op.)
        seen.remove(event);
        return Ok(());
    };
    for entry in entries {
        match entry {
            Entry::PutEnv { key, val } => {
                env.insert(key.clone(), expand(val, env));
                outcomes.push(EntryOutcome::Ran);
            }
            Entry::Alias(name) => {
                dispatch_inner(scripts, name, ctx, env, timeout, seen, outcomes)?;
            }
            Entry::Php(args) => {
                let line = format!("{} {}", shell_quote(&ctx.php_bin.display().to_string()), args);
                run_command_line(line.trim(), ctx, env, *timeout)
                    .wrap_err_with(|| format!("`{event}`: @php {args}"))?;
                outcomes.push(EntryOutcome::Ran);
            }
            Entry::Composer(args) => match map_composer(args) {
                ComposerMap::Noop => outcomes.push(EntryOutcome::Ran),
                ComposerMap::Unmapped => {
                    eprintln!(
                        "warning: `{event}` runs `@composer {args}`, which bougie does not map; \
                         skipping. Run it via `bougie run -- composer {args}` if required."
                    );
                    outcomes.push(EntryOutcome::SkippedComposer(args.clone()));
                }
            },
            Entry::Shell(cmd) => {
                run_command_line(cmd, ctx, env, *timeout)
                    .wrap_err_with(|| format!("`{event}`: {cmd}"))?;
                outcomes.push(EntryOutcome::Ran);
            }
            Entry::Callback { class, method } => {
                // `disableProcessTimeout` is recognised here (not via the
                // registry) because it mutates the dispatch-local timeout.
                if normalize_callback(class, method) == DISABLE_TIMEOUT_CALLBACK {
                    *timeout = None;
                    outcomes.push(EntryOutcome::NativeCallback);
                    continue;
                }
                if let Some(handler) = ctx.callbacks.get(class, method) {
                    handler(ctx)
                        .wrap_err_with(|| format!("`{event}`: native callback {class}::{method}"))?;
                    outcomes.push(EntryOutcome::NativeCallback);
                } else {
                    eprintln!(
                        "warning: `{event}` lists the PHP callback `{class}::{method}`, which \
                         reaches into Composer internals; bougie does not run it. Express it as a \
                         shell/`@php` entry if the behavior is required."
                    );
                    outcomes.push(EntryOutcome::SkippedCallback(format!("{class}::{method}")));
                }
            }
        }
    }
    seen.remove(event);
    Ok(())
}

/// Seed the per-dispatch env from the host's `base_env`, ensuring `bin_dir`
/// leads `PATH`. The prepend is idempotent so it composes with a host that
/// already folded `bin_dir` into `base_env`'s `PATH`.
fn seed_env(ctx: &ScriptContext) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = ctx.base_env.iter().cloned().collect();
    let bin = ctx.bin_dir.display().to_string();
    let path = env.get("PATH").cloned().unwrap_or_default();
    let leads = path.split(PATH_SEP).next().is_some_and(|first| first == bin);
    if !bin.is_empty() && !leads {
        let joined = if path.is_empty() { bin } else { format!("{bin}{PATH_SEP}{path}") };
        env.insert("PATH".into(), joined);
    }
    env
}

#[cfg(unix)]
const PATH_SEP: &str = ":";
#[cfg(not(unix))]
const PATH_SEP: &str = ";";

/// `Class::method` with a single leading namespace `\` stripped, so
/// `\Composer\Config::disableProcessTimeout` and the slash-less form match.
fn normalize_callback(class: &str, method: &str) -> String {
    format!("{}::{method}", class.strip_prefix('\\').unwrap_or(class))
}

/// Run a command line through the platform shell with the dispatch env,
/// rooted at the project. Non-zero exit is an `Err` (aborts the event).
///
/// `timeout` caps the wall-clock per process (Composer's
/// `config.process-timeout`, default 300s). On expiry the child is killed
/// and an error aborts the event; `None` waits indefinitely.
fn run_command_line(
    line: &str,
    ctx: &ScriptContext,
    env: &HashMap<String, String>,
    timeout: Option<Duration>,
) -> Result<()> {
    let mut cmd = shell_command(line);
    cmd.current_dir(ctx.project_root);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let Some(limit) = timeout else {
        let status = cmd.status().wrap_err_with(|| format!("spawning shell for `{line}`"))?;
        return exit_to_result(status, line);
    };
    // Put the script in its own process group so a timeout tears down the
    // whole tree (the shell *and* anything it forked), not just the shell —
    // matching what Symfony Process does for Composer. Only on the timeout
    // path: the unlimited path keeps the shell in bougie's group so Ctrl-C
    // reaches it normally.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().wrap_err_with(|| format!("spawning shell for `{line}`"))?;
    if let Some(status) = child.wait_timeout(limit).wrap_err_with(|| format!("waiting for `{line}`"))? {
        return exit_to_result(status, line);
    }
    kill_tree(&mut child);
    Err(eyre!(
        "command `{line}` exceeded the {}s process timeout; raise it with \
         `config.process-timeout` in composer.json (0 = unlimited) or call \
         `Composer\\Config::disableProcessTimeout` earlier in the script",
        limit.as_secs(),
    ))
}

/// Kill a timed-out child and reap it. On Unix the child leads its own
/// process group (set via `process_group(0)` above), so `killpg` takes down
/// any grandchildren it forked too.
#[cfg(unix)]
fn kill_tree(child: &mut std::process::Child) {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;
    if let Ok(pid) = i32::try_from(child.id()) {
        let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
    }
    let _ = child.wait();
}

#[cfg(not(unix))]
fn kill_tree(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn exit_to_result(status: std::process::ExitStatus, line: &str) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        Err(eyre!("command `{line}` exited with {status}"))
    }
}

#[cfg(unix)]
fn shell_command(line: &str) -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-e").arg("-c").arg(line);
    cmd
}

#[cfg(not(unix))]
fn shell_command(line: &str) -> Command {
    let mut cmd = Command::new("cmd");
    cmd.arg("/C").arg(line);
    cmd
}

/// Quote a path for the platform shell so a binary path with spaces survives.
#[cfg(unix)]
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(not(unix))]
fn shell_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

enum ComposerMap {
    /// We're already mid-install / autoload-dump; the subcommand's effect is
    /// either done natively or would re-enter. Treat as a no-op.
    Noop,
    /// Not a subcommand bougie maps.
    Unmapped,
}

/// Map the common `@composer <sub>` calls to bougie equivalents. Only the
/// ones that occur inside install lifecycle scripts are mapped; the rest are
/// warn-skipped (`bougie tool composer` is the future escape hatch).
fn map_composer(args: &str) -> ComposerMap {
    match args.split_whitespace().next() {
        // `install` / `update` would re-enter the operation we're already
        // running; the native autoload dump already ran before
        // `post-autoload-dump`. All no-ops inside the install lifecycle.
        Some("install" | "update" | "dump-autoload" | "dumpautoload" | "dump") => ComposerMap::Noop,
        _ => ComposerMap::Unmapped,
    }
}

/// Expand `$VAR` / `${VAR}` against the current dispatch env (used by
/// `@putenv` values). Unknown vars expand to empty, matching `getenv`.
fn expand(val: &str, env: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(val.len());
    let mut chars = val.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let braced = chars.peek() == Some(&'{');
        if braced {
            chars.next();
        }
        let mut name = String::new();
        while let Some(&nc) = chars.peek() {
            let ok = if braced { nc != '}' } else { nc.is_ascii_alphanumeric() || nc == '_' };
            if !ok {
                break;
            }
            name.push(nc);
            chars.next();
        }
        if braced && chars.peek() == Some(&'}') {
            chars.next();
        }
        if name.is_empty() {
            out.push('$');
        } else if let Some(v) = env.get(&name) {
            out.push_str(v);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CallbackRegistry;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn ctx<'a>(root: &'a Path, reg: &'a CallbackRegistry, env: Vec<(String, String)>) -> ScriptContext<'a> {
        ScriptContext {
            project_root: root,
            php_bin: Path::new("/usr/bin/php"),
            bin_dir: Path::new("/nonexistent/bin"),
            base_env: env,
            dev_mode: true,
            timeout: None,
            callbacks: reg,
        }
    }

    #[test]
    fn shell_entry_runs_and_aborts_on_nonzero() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = CallbackRegistry::new();
        let sentinel = tmp.path().join("ran");
        // Use the `:` builtin + redirection so the test doesn't depend on a
        // populated PATH (dispatch sets PATH from base_env, which is empty here).
        let scripts = Scripts::parse(&serde_json::json!({
            "scripts": { "post-install-cmd": [format!(": > {}", sentinel.display())] }
        }));
        let c = ctx(tmp.path(), &reg, vec![]);
        dispatch(&scripts, "post-install-cmd", &c).unwrap();
        assert!(sentinel.exists());

        // A failing step returns Err and stops.
        let failing = Scripts::parse(&serde_json::json!({
            "scripts": { "x": ["false", format!(": > {}", tmp.path().join("after").display())] }
        }));
        assert!(dispatch(&failing, "x", &c).is_err());
        assert!(!tmp.path().join("after").exists());
    }

    #[test]
    fn putenv_is_scoped_and_expands() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = CallbackRegistry::new();
        let out = tmp.path().join("env.txt");
        let scripts = Scripts::parse(&serde_json::json!({
            "scripts": { "x": [
                "@putenv GREETING=hello",
                "@putenv MESSAGE=${GREETING}-world",
                format!("printf '%s' \"$MESSAGE\" > {}", out.display()),
            ] }
        }));
        let c = ctx(tmp.path(), &reg, vec![]);
        dispatch(&scripts, "x", &c).unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "hello-world");
    }

    #[test]
    fn alias_recurses_and_detects_cycles() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = CallbackRegistry::new();
        let scripts = Scripts::parse(&serde_json::json!({
            "scripts": { "a": ["@b"], "b": ["@a"] }
        }));
        let c = ctx(tmp.path(), &reg, vec![]);
        assert!(dispatch(&scripts, "a", &c).is_err());
    }

    #[test]
    fn callback_hits_registry_else_warn_skips() {
        let tmp = tempfile::tempdir().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let mut reg = CallbackRegistry::new();
        reg.register(
            "Acme\\Scripts::run",
            Box::new(move |_| {
                h.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
        );
        let scripts = Scripts::parse(&serde_json::json!({
            "scripts": { "x": ["Acme\\Scripts::run", "Other\\Thing::go"] }
        }));
        let c = ctx(tmp.path(), &reg, vec![]);
        let out = dispatch(&scripts, "x", &c).unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(
            out,
            vec![EntryOutcome::NativeCallback, EntryOutcome::SkippedCallback("Other\\Thing::go".into())]
        );
    }

    #[test]
    fn composer_subcommands_map_or_skip() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = CallbackRegistry::new();
        let scripts = Scripts::parse(&serde_json::json!({
            "scripts": { "x": ["@composer dump-autoload", "@composer require foo/bar"] }
        }));
        let c = ctx(tmp.path(), &reg, vec![]);
        let out = dispatch(&scripts, "x", &c).unwrap();
        assert_eq!(
            out,
            vec![EntryOutcome::Ran, EntryOutcome::SkippedComposer("require foo/bar".into())]
        );
    }

    #[test]
    fn undefined_event_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = CallbackRegistry::new();
        let scripts = Scripts::parse(&serde_json::json!({ "scripts": {} }));
        let c = ctx(tmp.path(), &reg, vec![]);
        assert!(dispatch(&scripts, "post-install-cmd", &c).unwrap().is_empty());
    }

    #[test]
    fn bin_dir_prepended_to_path() {
        let reg = CallbackRegistry::new();
        let bin = PathBuf::from("/opt/proj/vendor/bin");
        let c = ScriptContext {
            project_root: Path::new("/tmp"),
            php_bin: Path::new("/usr/bin/php"),
            bin_dir: &bin,
            base_env: vec![("PATH".into(), "/usr/bin".into())],
            dev_mode: true,
            timeout: None,
            callbacks: &reg,
        };
        let env = seed_env(&c);
        assert_eq!(env.get("PATH").unwrap(), "/opt/proj/vendor/bin:/usr/bin");
        // Idempotent: already-leading bin_dir isn't doubled.
        let c2 = ScriptContext { base_env: vec![("PATH".into(), env["PATH"].clone())], ..c };
        assert_eq!(seed_env(&c2).get("PATH").unwrap(), "/opt/proj/vendor/bin:/usr/bin");
    }

    /// A `ScriptContext` with the inherited `PATH` (so `sleep` resolves) and
    /// a per-process `timeout`.
    fn ctx_with_timeout<'a>(
        root: &'a Path,
        reg: &'a CallbackRegistry,
        timeout: Option<std::time::Duration>,
    ) -> ScriptContext<'a> {
        ScriptContext {
            project_root: root,
            php_bin: Path::new("/usr/bin/php"),
            bin_dir: Path::new("/nonexistent/bin"),
            base_env: vec![("PATH".into(), std::env::var("PATH").unwrap_or_default())],
            dev_mode: true,
            timeout,
            callbacks: reg,
        }
    }

    #[test]
    fn process_timeout_kills_a_slow_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = CallbackRegistry::new();
        let c = ctx_with_timeout(tmp.path(), &reg, Some(std::time::Duration::from_millis(300)));
        let scripts = Scripts::parse(&serde_json::json!({ "scripts": { "x": ["sleep 5"] } }));
        let start = std::time::Instant::now();
        let err = dispatch(&scripts, "x", &c).unwrap_err();
        // Killed promptly, nowhere near the 5s sleep.
        assert!(start.elapsed() < std::time::Duration::from_secs(2), "should kill promptly");
        assert!(format!("{err:#}").contains("timeout"), "{err:#}");
    }

    #[test]
    fn disable_process_timeout_callback_lifts_the_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = CallbackRegistry::new();
        let done = tmp.path().join("done");
        // A 200ms budget would kill `sleep 0.5`, but the callback lifts it
        // for the rest of the dispatch, so the entry completes.
        let c = ctx_with_timeout(tmp.path(), &reg, Some(std::time::Duration::from_millis(200)));
        let scripts = Scripts::parse(&serde_json::json!({ "scripts": { "x": [
            "Composer\\Config::disableProcessTimeout",
            format!("sleep 0.5 && : > {}", done.display()),
        ] } }));
        dispatch(&scripts, "x", &c).expect("disabled timeout must let the slow entry finish");
        assert!(done.exists());
    }
}
