use bougie_cli::OutputFormat;
use bougie_config::write_bougie_toml_skeleton;
use bougie_output::output::{emit, Render};
use eyre::{Result, WrapErr, eyre};
use serde::Serialize;
use std::fs;
use std::io::{self, Write};
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
    if composer.exists() {
        if starter.is_some() {
            return Err(eyre!(
                "composer.json already exists — `--starter` scaffolds a new project; \
                 run it in an empty directory"
            ));
        }
        already.push(rel(PathBuf::from("composer.json")));
    } else {
        let contents = match starter {
            Some(s) => {
                let mut manifest = super::starter::fetch(&s)?;
                if let Some(name) = name {
                    set_name(&mut manifest.composer_json, &name)?;
                }
                let rendered = super::starter::render_composer_json(&manifest);
                notes = manifest.notes;
                rendered
            }
            None => default_composer_json(name.as_deref()),
        };
        fs::write(&composer, contents).wrap_err("writing composer.json")?;
        created.push(rel(PathBuf::from("composer.json")));
    }

    let bougie_dir = root.join(".bougie");
    for sub in ["conf.d", "bin", "state"] {
        let p = bougie_dir.join(sub);
        if !p.exists() {
            fs::create_dir_all(&p)
                .wrap_err_with(|| format!("creating {}", p.display()))?;
            created.push(rel(PathBuf::from(".bougie").join(sub)));
        }
    }

    let gitignore = bougie_dir.join(".gitignore");
    if !gitignore.exists() {
        fs::write(&gitignore, "bin/\nstate/\n").wrap_err("writing .bougie/.gitignore")?;
        created.push(rel(PathBuf::from(".bougie").join(".gitignore")));
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
    // `make::run` operates on the cwd, so enter the freshly-scaffolded
    // project root first (a no-op for `init`, where root == cwd).
    std::env::set_current_dir(root)
        .wrap_err_with(|| format!("entering {}", root.display()))?;
    crate::commands::make::run(
        format,
        crate::commands::make::MakeOptions {
            task: Some("start".to_string()),
            ..Default::default()
        },
    )
}

#[cfg(not(unix))]
fn start_project(_root: &Path, _format: OutputFormat) -> Result<ExitCode> {
    Err(eyre!(
        "`--start` brings up the services stack, which is Unix-only"
    ))
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
