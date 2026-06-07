//! `bougie services …` — the client-side subcommands.
//!
//! Most of this surface is a thin IPC client over `bougied`. The
//! offline subcommands (`catalog`, `add`, `remove`, `list`) need no
//! running daemon.

pub mod add;
pub mod catalog;
pub mod client;
pub mod config_mut;
pub mod daemon;
pub mod down;
pub mod ide;
pub mod list;
pub mod logs;
pub mod projects;
pub mod remove;
pub mod restart;
pub mod status;
pub mod up;

/// Bridge for `bougie-recipe`'s `set_service_env_provider` hook:
/// fetches the `BOUGIE_SERVICE_*` env for `project` from a running
/// `bougied`. Returns empty when bougied is down or the project has
/// no managed services — recipe steps run unenriched in that case.
pub fn recipe_env_for_project(project: &std::path::Path) -> Vec<(String, String)> {
    use serde::Deserialize;
    #[derive(Deserialize)]
    struct EnvReply {
        #[serde(default)]
        vars: std::collections::BTreeMap<String, serde_json::Value>,
    }
    let Ok(paths) = bougie_paths::Paths::from_env() else {
        return Vec::new();
    };
    if !paths.bougied_sock().exists() {
        return Vec::new();
    }
    let args = serde_json::json!({"project": project});
    match client::call::<EnvReply>(&paths, "service.env", args) {
        Ok(r) => r
            .vars
            .into_iter()
            .map(|(k, v)| {
                let s = match v {
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                };
                (k, s)
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}
