//! `bougie.toml` recipe parser. See RECIPES.md §2.

use eyre::{Result, WrapErr};
use serde::Deserialize;
use std::collections::BTreeMap;

/// One `[task.<name>]` table.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct TaskDef {
    #[serde(deserialize_with = "deserialize_string_or_vec", default)]
    pub deps: Vec<String>,
    #[serde(deserialize_with = "deserialize_string_or_vec_opt", default)]
    pub creates: Option<Vec<String>>,
    pub check: Option<String>,
    pub run: Option<String>,
}

/// A parsed recipe — a map of task name to definition.
#[derive(Debug, Clone, Default)]
pub struct Recipe {
    pub tasks: BTreeMap<String, TaskDef>,
}

#[derive(Debug, Deserialize)]
struct RawRecipe {
    #[serde(default)]
    task: BTreeMap<String, TaskDef>,
}

/// Parse a TOML document into a [`Recipe`]. Unknown top-level keys
/// other than `[task.*]` are tolerated (a project `bougie.toml` mixes
/// recipe tables with `[php]`, `[extensions]`, etc.).
pub fn parse(text: &str) -> Result<Recipe> {
    let raw: RawRecipe = toml_edit::de::from_str(text).wrap_err("parsing recipe TOML")?;
    Ok(Recipe { tasks: raw.task })
}

fn deserialize_string_or_vec<'de, D>(de: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum One {
        S(String),
        V(Vec<String>),
    }
    match One::deserialize(de)? {
        One::S(s) => Ok(vec![s]),
        One::V(v) => Ok(v),
    }
}

fn deserialize_string_or_vec_opt<'de, D>(
    de: D,
) -> std::result::Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(deserialize_string_or_vec(de)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_task() {
        let r = parse(
            r#"
[task.start]
run = "echo hi"
"#,
        )
        .unwrap();
        let t = &r.tasks["start"];
        assert_eq!(t.run.as_deref(), Some("echo hi"));
        assert!(t.deps.is_empty());
        assert!(t.creates.is_none());
    }

    #[test]
    fn parses_full_task() {
        let r = parse(
            r#"
[task.vendor]
creates = "vendor"
deps = ["composer.lock", "composer.json"]
run = "composer install"

[task.install]
creates = ["app/etc/env.php", "app/etc/config.php"]
deps = ["vendor", "services"]
check = "test -f app/etc/env.php"
run = """
echo step 1
echo step 2
"""
"#,
        )
        .unwrap();
        let v = &r.tasks["vendor"];
        assert_eq!(v.creates.as_deref(), Some(&["vendor".to_string()][..]));
        assert_eq!(v.deps, vec!["composer.lock", "composer.json"]);

        let i = &r.tasks["install"];
        assert_eq!(
            i.creates.as_deref(),
            Some(&["app/etc/env.php".to_string(), "app/etc/config.php".to_string()][..])
        );
        assert_eq!(i.check.as_deref(), Some("test -f app/etc/env.php"));
        assert!(i.run.as_ref().unwrap().contains("step 1"));
    }

    #[test]
    fn tolerates_unrelated_top_level_keys() {
        let r = parse(
            r#"
[php]
version = "8.3"

[task.start]
run = "true"
"#,
        )
        .unwrap();
        assert!(r.tasks.contains_key("start"));
    }

    #[test]
    fn rejects_unknown_task_keys() {
        let err = parse(
            r#"
[task.start]
phony = true
run = "true"
"#,
        );
        assert!(err.is_err());
    }
}
