//! Task DAG: resolve deps, detect cycles, walk in dependency order.

use super::parser::Recipe;
use std::collections::{BTreeSet, HashMap, HashSet};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DagError {
    #[error("recipe defines no task named `{0}`")]
    UnknownTask(String),
    #[error("recipe has a dependency cycle through `{0}`")]
    Cycle(String),
}

/// A resolved walk of named tasks in dependency order (leaves first).
#[derive(Debug, Clone)]
pub struct Dag<'r> {
    pub recipe: &'r Recipe,
    /// Named tasks, topologically sorted (deps before dependents).
    pub order: Vec<String>,
}

impl<'r> Dag<'r> {
    /// Build a topological order reachable from `target`. File-path
    /// deps (i.e. dep strings not present in `recipe.tasks`) are
    /// ignored here — they belong to the freshness check, not the
    /// DAG walk.
    pub fn build(recipe: &'r Recipe, target: &str) -> Result<Self, DagError> {
        if !recipe.tasks.contains_key(target) {
            return Err(DagError::UnknownTask(target.to_string()));
        }
        let mut order = Vec::new();
        let mut visited: HashSet<String> = HashSet::new();
        let mut stack: HashSet<String> = HashSet::new();
        visit(recipe, target, &mut visited, &mut stack, &mut order)?;
        Ok(Self { recipe, order })
    }

    /// All available task names, sorted.
    pub fn all_task_names(recipe: &Recipe) -> Vec<String> {
        let s: BTreeSet<_> = recipe.tasks.keys().cloned().collect();
        s.into_iter().collect()
    }
}

fn visit(
    recipe: &Recipe,
    name: &str,
    visited: &mut HashSet<String>,
    stack: &mut HashSet<String>,
    order: &mut Vec<String>,
) -> Result<(), DagError> {
    if visited.contains(name) {
        return Ok(());
    }
    if !stack.insert(name.to_string()) {
        return Err(DagError::Cycle(name.to_string()));
    }
    let task = recipe
        .tasks
        .get(name)
        .ok_or_else(|| DagError::UnknownTask(name.to_string()))?;
    for dep in &task.deps {
        if recipe.tasks.contains_key(dep) {
            visit(recipe, dep, visited, stack, order)?;
        }
    }
    stack.remove(name);
    visited.insert(name.to_string());
    order.push(name.to_string());
    Ok(())
}

/// Partition a task's deps into named-task deps (resolved by name) and
/// file-path deps (everything else). Named-task-first resolution per
/// RECIPES.md §3.
pub fn split_deps<'a>(
    recipe: &Recipe,
    deps: &'a [String],
) -> (Vec<&'a str>, Vec<&'a str>) {
    let mut named = Vec::new();
    let mut files = Vec::new();
    for d in deps {
        if recipe.tasks.contains_key(d) {
            named.push(d.as_str());
        } else {
            files.push(d.as_str());
        }
    }
    (named, files)
}

// Convenience for callers that don't want lifetime juggling.
pub fn task_names_by_creates(recipe: &Recipe) -> HashMap<&str, &str> {
    let mut m = HashMap::new();
    for (name, task) in &recipe.tasks {
        if let Some(c) = &task.creates {
            for p in c {
                m.insert(p.as_str(), name.as_str());
            }
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe::parser::parse;

    #[test]
    fn topo_orders_deps_first() {
        let r = parse(
            r#"
[task.a]
run = "true"

[task.b]
deps = ["a"]
run = "true"

[task.c]
deps = ["b", "a"]
run = "true"
"#,
        )
        .unwrap();
        let d = Dag::build(&r, "c").unwrap();
        assert_eq!(d.order, vec!["a", "b", "c"]);
    }

    #[test]
    fn ignores_file_deps_in_topo() {
        let r = parse(
            r#"
[task.vendor]
deps = ["composer.lock"]
run = "true"
"#,
        )
        .unwrap();
        let d = Dag::build(&r, "vendor").unwrap();
        assert_eq!(d.order, vec!["vendor"]);
    }

    #[test]
    fn detects_cycle() {
        let r = parse(
            r#"
[task.a]
deps = ["b"]
run = "true"

[task.b]
deps = ["a"]
run = "true"
"#,
        )
        .unwrap();
        let err = Dag::build(&r, "a").unwrap_err();
        assert!(matches!(err, DagError::Cycle(_)));
    }

    #[test]
    fn unknown_target_errors() {
        let r = parse(r#"[task.x]"#).unwrap();
        let err = Dag::build(&r, "y").unwrap_err();
        assert!(matches!(err, DagError::UnknownTask(_)));
    }
}
