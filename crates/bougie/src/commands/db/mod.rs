//! `bougie db` — manage the project's dev database.
//!
//! Currently one verb, `seed`, which loads a jibs `.jibsdump` snapshot into the
//! project's mariadb tenant so the local database is shaped like production.

pub mod seed;
