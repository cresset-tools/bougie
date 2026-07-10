//! `bougie db` — manage the project's dev database.
//!
//! Two verbs: `pull`, which downloads the latest production-shaped snapshot for
//! a repo from the team's sconce registry into the local cache, and `seed`,
//! which loads a jibs `.jibsdump` snapshot (a pulled one, a local file, or a
//! URL) into the project's mariadb tenant so the local database is shaped like
//! production.

pub mod pull;
pub mod seed;
