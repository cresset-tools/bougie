//! Recipe engine for `bougie start`. See RECIPES.md.
//!
//! Tasks are declared in `[task.<name>]` tables in `bougie.toml` (or
//! in a builtin recipe shipped with the binary). Each task is phony
//! unless it declares `creates`, in which case the recipe is freshness
//! gated on mtime against its file-path deps and the `creates` of its
//! named-task deps.

pub mod builtin;
pub mod dag;
pub mod freshness;
pub mod parser;
pub mod run;

pub use builtin::{detect_from_text, merge_with_builtin, BUILTINS};
pub use dag::{Dag, DagError};
pub use parser::{parse, Recipe, TaskDef};
pub use run::{run_task, RunOptions, TaskOutcome, TaskStatus};
