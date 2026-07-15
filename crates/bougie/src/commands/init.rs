use bougie_cli::OutputFormat;
use bougie_config::write_bougie_toml_skeleton;
use bougie_output::output::{emit, Render};
use eyre::{Result, WrapErr, eyre};
use serde::Serialize;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Phase 4 will refresh the cached index to learn the actual latest
/// minor. Until then `bougie init` writes a sensible default that can
/// be tightened later by the user (or by a future `bougie sync`).
const LATEST_PHP_MINOR: &str = "8.4";

#[derive(Debug, Serialize)]
pub struct InitResult {
    pub schema_version: u32,
    pub created: Vec<PathBuf>,
    pub already_present: Vec<PathBuf>,
}

impl Render for InitResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for p in &self.created {
            writeln!(w, "created  {}", p.display())?;
        }
        for p in &self.already_present {
            writeln!(w, "kept     {}", p.display())?;
        }
        Ok(())
    }
}

pub fn run(
    format: OutputFormat,
    with_toml: bool,
    name: Option<String>,
    starter: Option<String>,
    start: bool,
) -> Result<ExitCode> {
    let cwd = std::env::current_dir().wrap_err("getting current directory")?;
    scaffold(&cwd, None, format, with_toml, name, starter, start)
}

/// `bougie new <directory>`: create `<directory>` under the cwd and
/// scaffold a fresh project inside it. Refuses to scaffold into a
/// directory that already exists and is non-empty.
pub fn run_new(
    format: OutputFormat,
    directory: &str,
    with_toml: bool,
    name: Option<String>,
    starter: Option<String>,
    start: bool,
) -> Result<ExitCode> {
    let cwd = std::env::current_dir().wrap_err("getting current directory")?;
    let root = cwd.join(directory);
    if root.exists() {
        let non_empty = root
            .read_dir()
            .wrap_err_with(|| format!("reading {}", root.display()))?
            .next()
            .is_some();
        if non_empty {
            return Err(eyre!("`{directory}` already exists and is not empty"));
        }
    } else {
        fs::create_dir_all(&root)
            .wrap_err_with(|| format!("creating {}", root.display()))?;
    }
    scaffold(
        &root,
        Some(Path::new(directory)),
        format,
        with_toml,
        name,
        starter,
        start,
    )
}

/// Scaffold a project at `root`. `prefix`, when set (the `new` case),
/// is prepended to every reported path so output reads relative to the
/// directory the user invoked from rather than the new project root.
#[allow(clippy::too_many_arguments)]
fn scaffold(
    root: &Path,
    prefix: Option<&Path>,
    format: OutputFormat,
    with_toml: bool,
    name: Option<String>,
    starter: Option<String>,
    start: bool,
) -> Result<ExitCode> {
    let rel = |p: PathBuf| -> PathBuf {
        match prefix {
            Some(pre) => pre.join(p),
            None => p,
        }
    };
    let mut created = Vec::new();
    let mut already = Vec::new();

    if let Some(name) = &name
        && !super::composer_validate::is_valid_package_name(name)
    {
        return Err(eyre!(
            "invalid package name `{name}` — expected `vendor/package` \
             (lowercase letters, digits, and `-._`)"
        ));
    }

    // composer.json comes from the starter manifest when `--starter` is
    // given, else the empty default. `--starter` is for scaffolding a new
    // project, so refuse to clobber an existing composer.json.
    let composer = root.join("composer.json");
    let mut notes: Vec<String> = Vec::new();
    if composer.exists() && starter.is_some() {
        return Err(eyre!(
            "composer.json already exists — `--starter` scaffolds a new project; \
             run it in an empty directory"
        ));
    }

    // Bind the installer-package lookup to a local so the borrow of
    // `starter` ends here — the manifest arm below still moves `starter`.
    let installer_package = starter.as_deref().and_then(installer_starter_package);
    if let Some(package) = installer_package {
        // An *installer-based* starter (e.g. `--starter laravel`): rather
        // than fetch a manifest and author composer.json ourselves, run the
        // framework's own CLI installer — fetched + executed through the
        // `bougie tool run` engine — which scaffolds the whole project tree
        // (composer.json + skeleton + a composer install) in place.
        run_installer_starter(package, root, format)?;
        // `--name` isn't a flag the installer understands, so honour it by
        // patching the package name into the composer.json it generated.
        if let Some(name) = name {
            patch_composer_name(&composer, &name)?;
        }
        if !composer.is_file() {
            return Err(eyre!(
                "`{package}` finished but left no composer.json in {} — \
                 the installer may have failed",
                root.display()
            ));
        }
        created.push(rel(PathBuf::from("composer.json")));
    } else if composer.exists() {
        already.push(rel(PathBuf::from("composer.json")));
    } else {
        let contents = match starter {
            Some(s) => {
                let mut manifest = super::starter::fetch(&s)?;
                if let Some(name) = name {
                    set_name(&mut manifest.composer_json, &name)?;
                }
                // Make the manifest's recipe/services hints load-bearing by
                // persisting them into extra.bougie, so `--start` selects the
                // declared recipe and brings up the declared services.
                super::starter::apply_project_hints(
                    &mut manifest.composer_json,
                    manifest.recipe.as_deref(),
                    &manifest.services,
                );
                // Fill in any per-user placeholder tokens (e.g. a Hyvä repo
                // slug the producer can't bake into a shared manifest) before
                // rendering. Prompts read stdin, so only when interactive.
                let interactive =
                    matches!(format, OutputFormat::Text) && io::stdin().is_terminal();
                super::starter::resolve_placeholders(
                    &mut manifest.composer_json,
                    &manifest.placeholders,
                    interactive,
                )?;
                // Prompt for private-repo secrets (e.g. a Hyvä license key)
                // and stash them in bougie's credential store before the
                // resolve `--start` triggers — keeps them out of the
                // committed composer.json the placeholders just wrote.
                super::starter::resolve_auth(&manifest.auth, interactive)?;
                let rendered = super::starter::render_composer_json(&manifest);
                notes = manifest.notes;
                rendered
            }
            None => default_composer_json(name.as_deref()),
        };
        fs::write(&composer, contents).wrap_err("writing composer.json")?;
        created.push(rel(PathBuf::from("composer.json")));
    }

    // The project-local toolchain dir lives under `vendor/` now (it's
    // disposable — `rm -rf vendor` + `bougie sync` rebuilds it), so its
    // path relative to the project root is `vendor/bougie`.
    let bougie_dir = bougie_paths::project::dir(root);
    let bougie_rel = PathBuf::from("vendor").join("bougie");
    for sub in ["conf.d", "bin", "state"] {
        let p = bougie_dir.join(sub);
        if !p.exists() {
            fs::create_dir_all(&p)
                .wrap_err_with(|| format!("creating {}", p.display()))?;
            created.push(rel(bougie_rel.join(sub)));
        }
    }

    // Self-contained ignore (mirrors what Cargo writes into `target/`):
    // a single `*` keeps the whole disposable tree out of git even if
    // the project doesn't already ignore `vendor/`.
    let gitignore = bougie_dir.join(".gitignore");
    if !gitignore.exists() {
        fs::write(&gitignore, "*\n").wrap_err("writing vendor/bougie/.gitignore")?;
        created.push(rel(bougie_rel.join(".gitignore")));
    }

    if with_toml {
        let toml_path = root.join("bougie.toml");
        if toml_path.exists() {
            already.push(rel(PathBuf::from("bougie.toml")));
        } else {
            fs::write(&toml_path, write_bougie_toml_skeleton())
                .wrap_err("writing bougie.toml")?;
            created.push(rel(PathBuf::from("bougie.toml")));
        }
    }

    let result = InitResult {
        schema_version: 1,
        created,
        already_present: already,
    };
    emit(format, &result)?;

    // Starter notes (auth hints etc.) → stderr so `--format json-v1`
    // stdout stays a single clean document.
    for note in &notes {
        eprintln!("note: {note}");
    }

    if start {
        return start_project(root, format);
    }
    Ok(ExitCode::SUCCESS)
}

/// `--start`: bring the freshly-scaffolded project up, exactly like
/// `bougie start` — `bougie make start` syncs the toolchain + vendor and
/// then walks the project recipe (services → setup → server). Unix-only,
/// since the recipe/services stack is.
#[cfg(unix)]
fn start_project(root: &Path, format: OutputFormat) -> Result<ExitCode> {
    // Single shared entry point with `bougie start`, so `init --start`
    // and the standalone verb never drift.
    crate::commands::start::run_in(format, root)
}

#[cfg(not(unix))]
fn start_project(_root: &Path, _format: OutputFormat) -> Result<ExitCode> {
    Err(eyre!(
        "`--start` brings up the services stack, which is Unix-only"
    ))
}

/// Map an installer-based starter alias to the Composer package whose CLI
/// scaffolds the project. Unlike manifest starters (which only yield a
/// `composer.json`), these run an external installer via `bougie tool run`.
/// `None` for anything else — those fall through to the manifest path.
fn installer_starter_package(starter: &str) -> Option<&'static str> {
    match starter {
        "laravel" => Some("laravel/installer"),
        _ => None,
    }
}

/// Compute how to invoke an installer that creates the project directory
/// itself (`laravel new <name>`): run it from `root`'s parent with `root`'s
/// basename as the project name. Works for both `bougie init` (root = cwd)
/// and `bougie new <dir>` (root = cwd/<dir>); `--force` lets the installer
/// populate the directory bougie already created.
fn installer_invocation(root: &Path) -> Result<(PathBuf, std::ffi::OsString)> {
    let parent = root
        .parent()
        .ok_or_else(|| {
            eyre!(
                "cannot scaffold an installer starter at the filesystem root {}",
                root.display()
            )
        })?
        .to_path_buf();
    let name = root
        .file_name()
        .ok_or_else(|| eyre!("cannot derive a project name from {}", root.display()))?
        .to_os_string();
    Ok((parent, name))
}

/// Fetch + run an installer-based starter (e.g. `laravel/installer`)
/// through the `bougie tool run` engine. The tool is cached/materialised
/// exactly as `bougie tool run` would, then spawned as a child (not
/// execve'd — we still have toolchain scaffolding and `--start` to do
/// afterwards) with `laravel new <name> --force` in the project's parent.
/// A tool-engine `composer/composer` is materialised alongside and
/// placed first on the child's PATH, since the installer shells out to
/// the real Composer.
fn run_installer_starter(package: &str, root: &Path, format: OutputFormat) -> Result<()> {
    use bougie_paths::Paths;
    use bougie_tool::install::InstallContext;
    use bougie_tool::{exec, receipt, request, run};
    use std::ffi::OsString;

    let (parent, project_name) = installer_invocation(root)?;

    let paths = Paths::from_env()?;
    let req = request::parse(package)?;
    // Same callback wiring as `commands::tool_run::run`.
    let resolve_lock: &bougie_tool::install::LockResolver = &|paths, project_root| {
        super::composer_update::resolve_and_write_lock(
            paths,
            project_root,
            bougie_composer_resolver::ResolutionStrategy::Highest,
        )
        .map(|_| ())
    };
    let php_installer = super::tool_callbacks::php_installer();
    let classifier = super::tool_callbacks::extension_classifier();
    let ext_installer = super::tool_callbacks::extension_installer();
    let tool_requires = super::tool_callbacks::tool_requires_fetcher();
    let php_baseline = super::tool_callbacks::baseline_ensurer();
    let native_fetcher = super::tool_callbacks::native_prefetcher();
    let ctx = InstallContext {
        paths: &paths,
        resolve_lock,
        php_installer: php_installer.as_ref(),
        classifier: classifier.as_ref(),
        ext_installer: ext_installer.as_ref(),
        tool_requires: tool_requires.as_ref(),
        php_baseline: php_baseline.as_ref(),
        native_fetcher: native_fetcher.as_ref(),
    };

    eprintln!("note: scaffolding with `{package}` via bougie tool run…");

    // No project context: we're scaffolding the project right now, so
    // there's nothing meaningful to derive from the (half-written) cwd.
    let plan = run::prepare(&ctx, &req, None, &[], None)?;
    let receipt = receipt::read(&plan.tool_dir.join("receipt.toml"))?;
    let declared = bougie_tool::install::read_default_bin(&plan.tool_dir, &plan.package)?;
    let entry = run::pick_bin(&receipt.entrypoints, &plan.package, None, declared.as_deref())?;
    let wrapper = plan.tool_dir.join("bin").join(&entry.name);
    if !wrapper.is_file() {
        return Err(eyre!(
            "tool dir {} is missing the wrapper for bin `{}`",
            plan.tool_dir.display(),
            entry.name
        ));
    }

    // The installer shells out to `composer` (`create-project` for the
    // skeleton, `require` for Pest/Boost extras) — verbs bougie's native
    // composer shim deliberately doesn't implement, and a pristine
    // bougie machine has no Composer at all. Materialize the real
    // Composer through the same tool engine and put its wrapper first
    // on the installer's PATH; any ambient `composer` further down is
    // shadowed, so the scaffold doesn't depend on host state.
    let composer_plan = run::prepare(&ctx, &request::parse("composer/composer")?, None, &[], None)?;
    let composer_bin = composer_plan.tool_dir.join("bin");
    if !composer_bin.join("composer").is_file() {
        return Err(eyre!(
            "tool dir {} is missing the `composer` wrapper",
            composer_plan.tool_dir.display()
        ));
    }

    let args: Vec<OsString> = vec![
        OsString::from("new"),
        project_name,
        OsString::from("--force"),
    ];
    let prep = exec::prepare(&paths, &wrapper, args)?;

    let mut cmd = std::process::Command::new(&prep.php_path);
    cmd.args(&prep.argv);
    // `prep.env` layers PHP_INI_SCAN_DIR (so the installer's PHP has the
    // baseline extensions it needs) and the `unzip` shim; the Composer
    // wrapper dir goes in front of whichever PATH the prep produced.
    let mut path_seen = false;
    for (k, v) in &prep.env {
        if k == "PATH" {
            cmd.env(k, prepend_path(&composer_bin, v)?);
            path_seen = true;
        } else {
            cmd.env(k, v);
        }
    }
    if !path_seen {
        let ambient = std::env::var_os("PATH").unwrap_or_default();
        cmd.env("PATH", prepend_path(&composer_bin, &ambient)?);
    }
    cmd.current_dir(&parent);
    cmd.stdout(installer_stdout(format));

    let status = cmd
        .status()
        .wrap_err_with(|| format!("running `{package}` installer"))?;
    if !status.success() {
        return Err(eyre!("`{package}` installer exited with {status}"));
    }
    Ok(())
}

/// `dir` in front of an existing PATH value, via the platform's PATH
/// joining rules.
fn prepend_path(dir: &Path, rest: &std::ffi::OsStr) -> Result<std::ffi::OsString> {
    let parts = std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(rest));
    std::env::join_paths(parts).map_err(|e| eyre!("building installer PATH: {e}"))
}

/// Route the installer's stdout. In text mode it inherits ours so the user
/// sees (and can answer) its prompts; in `--format json-v1` mode its chatty
/// output is redirected to stderr so our stdout stays a single clean JSON
/// document.
fn installer_stdout(format: OutputFormat) -> std::process::Stdio {
    match format {
        OutputFormat::Text => std::process::Stdio::inherit(),
        OutputFormat::JsonV1 => {
            #[cfg(unix)]
            {
                use std::os::fd::AsFd;
                std::io::stderr().as_fd().try_clone_to_owned().map_or_else(
                    |_| std::process::Stdio::inherit(),
                    std::process::Stdio::from,
                )
            }
            #[cfg(not(unix))]
            {
                std::process::Stdio::inherit()
            }
        }
    }
}

/// Overwrite the `name` field in an already-written composer.json. Used to
/// honour `--name` for installer starters, which author the file themselves.
fn patch_composer_name(composer: &Path, name: &str) -> Result<()> {
    let text = fs::read_to_string(composer).wrap_err("reading generated composer.json")?;
    let mut value: serde_json::Value =
        serde_json::from_str(&text).wrap_err("parsing generated composer.json")?;
    set_name(&mut value, name)?;
    let mut s = serde_json::to_string_pretty(&value).wrap_err("serializing composer.json")?;
    s.push('\n');
    fs::write(composer, s).wrap_err("writing composer.json")?;
    Ok(())
}

fn default_composer_json(name: Option<&str>) -> String {
    let mut value = serde_json::json!({
        "require": {
            "php": format!("^{LATEST_PHP_MINOR}")
        }
    });
    if let Some(name) = name {
        set_name(&mut value, name).expect("default composer.json is an object");
    }
    let mut s = serde_json::to_string_pretty(&value).expect("infallible serialize");
    s.push('\n');
    s
}

/// Set the `name` field on a composer.json `Value`, inserting or
/// overwriting it. Errors if the document isn't a JSON object.
fn set_name(composer_json: &mut serde_json::Value, name: &str) -> Result<()> {
    let obj = composer_json
        .as_object_mut()
        .ok_or_else(|| eyre!("starter composer-json is not a JSON object"))?;
    obj.insert("name".to_string(), serde_json::Value::String(name.to_string()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn laravel_is_an_installer_starter() {
        assert_eq!(installer_starter_package("laravel"), Some("laravel/installer"));
        // Manifest aliases / URLs are not installer starters.
        assert_eq!(installer_starter_package("mageos"), None);
        assert_eq!(installer_starter_package("https://example.com/starter.json"), None);
    }

    #[test]
    fn installer_invocation_splits_parent_and_name() {
        let (parent, name) = installer_invocation(Path::new("/home/u/proj/blog")).unwrap();
        assert_eq!(parent, Path::new("/home/u/proj"));
        assert_eq!(name, std::ffi::OsString::from("blog"));
    }

    #[test]
    fn installer_invocation_rejects_filesystem_root() {
        assert!(installer_invocation(Path::new("/")).is_err());
    }

    #[test]
    fn patch_composer_name_overwrites_existing() {
        let dir = tempfile::TempDir::new().unwrap();
        let composer = dir.path().join("composer.json");
        // Mimic laravel/installer's default name + preserve key order.
        fs::write(
            &composer,
            "{\n    \"name\": \"laravel/laravel\",\n    \"type\": \"project\"\n}\n",
        )
        .unwrap();

        patch_composer_name(&composer, "acme/blog").unwrap();

        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&composer).unwrap()).unwrap();
        assert_eq!(value["name"], "acme/blog");
        // Other fields survive and `name` stays first (preserve_order).
        assert_eq!(value["type"], "project");
        let keys: Vec<&str> = value.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["name", "type"]);
    }
}
