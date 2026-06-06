//! Opt-in execution of **root** `composer.json` scripts.
//!
//! Composer only ever runs `scripts` from the root package (never from
//! dependencies), so they're the project author's own commands — not a
//! supply-chain hazard. bougie keeps execution opt-in / off by default; this
//! crate is the engine that runs them when the user turns it on.
//!
//! The crate is intentionally FS/PHP-agnostic: it [`parse`](Scripts::parse)s
//! and classifies the `scripts` table into [`Entry`] values and
//! [`dispatch`]es a named event, given a host-injected [`ScriptContext`]
//! (resolved PHP binary, env, callback registry). Everything that needs to
//! know about bougie's path/PHP/service-env machinery lives in the caller —
//! mirroring how `bougie-installers` isolates declarative-plugin logic.
//!
//! **Scope:** the non-internal entry forms (`@php`, `@composer`, `@putenv`,
//! `@<alias>`, plain shell). PHP-callback entries (`Class::method`) reach into
//! Composer internals in-process; bougie does not host them. They are
//! warn-and-skipped, except for a small allowlist the host registers natively
//! (e.g. Laravel's `clearCompiled`).

mod context;
mod dispatch;

pub use context::{CallbackHandler, CallbackRegistry, ScriptContext};
pub use dispatch::{dispatch, EntryOutcome};

/// A single listener entry within a `scripts.<event>` list, classified by
/// Composer's entry grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// A plain shell command, run via `/bin/sh -e -c` (Unix) / `cmd /C`.
    Shell(String),
    /// `@php <args>` — run with the project's resolved PHP binary.
    Php(String),
    /// `@composer <args>` — re-invoke Composer. bougie isn't Composer; the
    /// common subcommands are mapped, the rest warn-skipped.
    Composer(String),
    /// `@putenv KEY=VAL` — set an env var for *subsequent* entries in this
    /// dispatch (scoped to the dispatch, not the process).
    PutEnv { key: String, val: String },
    /// `@<name>` — a script alias; dispatch recurses into `scripts.<name>`.
    Alias(String),
    /// `Vendor\Class::method` — a PHP callback invoked in-process by Composer.
    /// Not hosted by bougie except via the native callback registry.
    Callback { class: String, method: String },
}

/// A parsed root `scripts` table: ordered event/alias name → ordered entries.
///
/// Order is preserved (Composer runs entries in declaration order, and alias
/// recursion depends on it), so the backing store is an ordered `Vec` rather
/// than a map.
#[derive(Debug, Clone, Default)]
pub struct Scripts(Vec<(String, Vec<Entry>)>);

impl Scripts {
    /// Parse the root `composer.json`'s `scripts` object into classified,
    /// order-preserving entries.
    ///
    /// Reads the raw value directly rather than going through
    /// `bougie-config`'s normaliser, which drops callback/mixed arrays — we
    /// need every entry classified in order, callbacks included. A single
    /// string `scripts.<event>` is treated as a one-element list, matching
    /// Composer.
    #[must_use]
    pub fn parse(root_composer_json: &serde_json::Value) -> Self {
        let mut out: Vec<(String, Vec<Entry>)> = Vec::new();
        let Some(obj) = root_composer_json.get("scripts").and_then(serde_json::Value::as_object)
        else {
            return Self(out);
        };
        for (event, value) in obj {
            let entries = match value {
                serde_json::Value::String(s) => vec![classify(s)],
                serde_json::Value::Array(a) => a
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(classify)
                    .collect(),
                // A non-string/array entry (object, number, …) isn't a valid
                // listener; skip the whole event rather than guess.
                _ => continue,
            };
            out.push((event.clone(), entries));
        }
        Self(out)
    }

    /// Look up the entries for an event or alias name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&[Entry]> {
        self.0.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_slice())
    }

    /// Whether the table has no events.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Classify one raw `scripts` string into an [`Entry`].
fn classify(raw: &str) -> Entry {
    let trimmed = raw.trim_start();
    // `@`-prefixed forms. Match the longest specific prefix first so `@php` /
    // `@composer` / `@putenv` don't fall through to the generic `@alias` arm.
    if let Some(rest) = trimmed.strip_prefix("@php")
        && (rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
        return Entry::Php(rest.trim_start().to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("@composer")
        && (rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
        return Entry::Composer(rest.trim_start().to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("@putenv")
        && rest.starts_with(char::is_whitespace)
    {
        let assignment = rest.trim_start();
        let (key, val) = assignment.split_once('=').unwrap_or((assignment, ""));
        return Entry::PutEnv { key: key.trim().to_string(), val: val.to_string() };
    }
    if let Some(name) = trimmed.strip_prefix('@')
        && !name.is_empty()
        && !name.contains(char::is_whitespace)
    {
        // A bare `@name` alias: a single token, no embedded whitespace.
        return Entry::Alias(name.to_string());
    }
    if let Some((class, method)) = as_callback(trimmed) {
        return Entry::Callback { class, method };
    }
    // Preserve the original (untrimmed) string for shell fidelity.
    Entry::Shell(raw.to_string())
}

/// Recognise a `Vendor\Class::method` PHP-callback entry: a single token
/// (no whitespace) of the shape `<class>::<method>`, where the class may
/// contain namespace separators. Anything with whitespace is a shell command.
fn as_callback(s: &str) -> Option<(String, String)> {
    if s.contains(char::is_whitespace) {
        return None;
    }
    let (class, method) = s.split_once("::")?;
    if class.is_empty() || method.is_empty() || method.contains("::") {
        return None;
    }
    let class_ok = class.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '\\');
    let method_ok = method.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    (class_ok && method_ok).then(|| (class.to_string(), method.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_each_entry_form() {
        assert_eq!(classify("@php artisan migrate"), Entry::Php("artisan migrate".into()));
        assert_eq!(classify("@php"), Entry::Php(String::new()));
        assert_eq!(classify("@composer dump-autoload"), Entry::Composer("dump-autoload".into()));
        assert_eq!(
            classify("@putenv APP_ENV=testing"),
            Entry::PutEnv { key: "APP_ENV".into(), val: "testing".into() }
        );
        assert_eq!(classify("@build"), Entry::Alias("build".into()));
        assert_eq!(
            classify("Illuminate\\Foundation\\ComposerScripts::postAutoloadDump"),
            Entry::Callback {
                class: "Illuminate\\Foundation\\ComposerScripts".into(),
                method: "postAutoloadDump".into(),
            }
        );
        assert_eq!(classify("phpunit --colors"), Entry::Shell("phpunit --colors".into()));
    }

    #[test]
    fn php_prefix_does_not_swallow_longer_token() {
        // `@phpstan` is an alias, not a `@php` invocation of `stan`.
        assert_eq!(classify("@phpstan"), Entry::Alias("phpstan".into()));
    }

    #[test]
    fn callback_requires_no_whitespace() {
        // A shell command that merely contains `::` is not a callback.
        assert_eq!(classify("echo a::b c"), Entry::Shell("echo a::b c".into()));
    }

    #[test]
    fn parse_preserves_order_and_single_string_form() {
        let scripts = Scripts::parse(&json!({
            "scripts": {
                "post-install-cmd": ["@php artisan migrate", "phpunit"],
                "test": "phpunit"
            }
        }));
        assert_eq!(
            scripts.get("post-install-cmd"),
            Some(&[Entry::Php("artisan migrate".into()), Entry::Shell("phpunit".into())][..])
        );
        assert_eq!(scripts.get("test"), Some(&[Entry::Shell("phpunit".into())][..]));
        assert!(scripts.get("missing").is_none());
    }

    #[test]
    fn parse_empty_when_no_scripts() {
        assert!(Scripts::parse(&json!({})).is_empty());
        assert!(Scripts::parse(&json!({"scripts": 5})).is_empty());
    }
}
