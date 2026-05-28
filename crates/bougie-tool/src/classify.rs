//! Classify a `--with NAME` argument as either a Composer package
//! identifier (`vendor/name`) or a PHP extension (`intl`, `redis`).
//!
//! Strategy:
//!
//! - A `/` in the name unambiguously means "Composer package" (matches
//!   Composer's own naming rules). No further check.
//! - Otherwise, ask the supplied callback whether the name is a known
//!   PHP extension. The bougie binary fronts this with its
//!   `BASELINE_EXTENSIONS` / `BUILTIN_EXTENSIONS` lists and falls
//!   through to the bougie index for non-baseline names like `redis`.
//! - If the callback returns `Ok(false)` for a slash-free name we
//!   refuse to guess — the user picks between `bougie ext add NAME`
//!   first or rewriting as `vendor/NAME`. Better than silently doing
//!   something they didn't ask for.

use eyre::{Result, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classified {
    /// A PHP extension name (no vendor prefix). Lands in
    /// `$TOOL_DIR/conf.d/` and uses the shared extension store.
    Extension(String),
    /// A Composer package identifier (`vendor/name`, optionally with
    /// `@<constraint>` appended — we leave the constraint suffix
    /// attached to the string and let downstream parse it).
    ComposerPackage(String),
}

/// Returns `true` if the supplied bare name is a PHP extension that
/// bougie knows how to install for the tool's pinned PHP. Errors
/// indicate a hard failure (e.g. network unreachable while checking
/// the index) — they propagate.
pub type ExtensionClassifier = dyn Fn(&str) -> Result<bool> + Send + Sync;

pub fn classify(name: &str, is_known_ext: &ExtensionClassifier) -> Result<Classified> {
    if name.is_empty() {
        bail!("--with value is empty");
    }
    if name.contains('/') {
        return Ok(Classified::ComposerPackage(name.to_string()));
    }
    if is_known_ext(name)? {
        return Ok(Classified::Extension(name.to_string()));
    }
    bail!(
        "--with `{name}` looks like a single name with no `/` but isn't a known PHP extension. \
         If you meant a composer package, use the `<vendor>/<name>` form. \
         If you meant an extension that bougie's index doesn't know about, \
         install it project-wide first with `bougie ext add {name}`."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_form_is_composer_without_callback() {
        let cb: Box<ExtensionClassifier> =
            Box::new(|_: &str| -> Result<bool> { panic!("classifier should not be called for vendor/name") });
        let c = classify("phpstan/phpstan", cb.as_ref()).unwrap();
        assert_eq!(c, Classified::ComposerPackage("phpstan/phpstan".into()));
    }

    #[test]
    fn slash_form_keeps_at_constraint_attached() {
        let cb: Box<ExtensionClassifier> = Box::new(|_: &str| Ok(false));
        let c = classify("phpstan/phpstan@^1.10", cb.as_ref()).unwrap();
        assert_eq!(
            c,
            Classified::ComposerPackage("phpstan/phpstan@^1.10".into())
        );
    }

    #[test]
    fn bare_name_classified_as_extension_via_callback() {
        let cb: Box<ExtensionClassifier> = Box::new(|name: &str| Ok(name == "intl"));
        let c = classify("intl", cb.as_ref()).unwrap();
        assert_eq!(c, Classified::Extension("intl".into()));
    }

    #[test]
    fn bare_name_unknown_to_callback_errors_with_hint() {
        let cb: Box<ExtensionClassifier> = Box::new(|_: &str| Ok(false));
        let err = classify("phpstan", cb.as_ref()).unwrap_err().to_string();
        assert!(err.contains("isn't a known PHP extension"), "{err}");
        assert!(err.contains("vendor"), "{err}");
    }

    #[test]
    fn empty_name_errors() {
        let cb: Box<ExtensionClassifier> = Box::new(|_: &str| Ok(false));
        assert!(classify("", cb.as_ref()).is_err());
    }

    #[test]
    fn classifier_error_propagates() {
        let cb: Box<ExtensionClassifier> = Box::new(|_: &str| {
            Err(eyre::eyre!("network unreachable"))
        });
        let err = classify("intl", cb.as_ref()).unwrap_err().to_string();
        assert!(err.contains("network unreachable"), "{err}");
    }
}
