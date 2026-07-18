//! `bougie db` — manage the project's dev database.
//!
//! The verbs: `pull` downloads the latest production-shaped snapshot for a
//! repo from the team's sconce registry into the local cache; `seed` loads a
//! jibs `.jibsdump` snapshot (a pulled one, a local file, or a URL) into the
//! project's mariadb tenant so the local database is shaped like production
//! (one-shot — a no-op once seeded, unless `--force`); `refresh` = `pull` +
//! `seed --force`, the explicit "give me fresh prod data now" action (it
//! confirms before clobbering an already-seeded database); `get` pulls a
//! specific prod row-graph into the existing DB without a reseed; and `status`
//! reports it all, including whether the registry has a newer snapshot —
//! informational only, never reseeding on its own.

pub mod get;
pub mod pull;
pub mod refresh;
pub mod seed;
pub mod status;
