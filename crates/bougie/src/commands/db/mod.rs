//! `bougie db` — manage the project's dev database.
//!
//! Three verbs: `pull` downloads the latest production-shaped snapshot for a
//! repo from the team's sconce registry into the local cache; `seed` loads a
//! jibs `.jibsdump` snapshot (a pulled one, a local file, or a URL) into the
//! project's mariadb tenant so the local database is shaped like production
//! (one-shot — a no-op once seeded, unless `--force`); and `refresh` = `pull` +
//! `seed --force`, the explicit "give me fresh prod data now" action.

pub mod get;
pub mod pull;
pub mod refresh;
pub mod seed;
