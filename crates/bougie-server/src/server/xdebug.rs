//! Per-request xdebug trigger detection. Spec: SERVER.md §6.
//!
//! A request is routed to the `xdebug` pool variant if any of:
//!
//! - Cookie `XDEBUG_SESSION` present (any value)
//! - Cookie `XDEBUG_TRIGGER` present
//! - Query param `XDEBUG_SESSION_START` set
//! - Query param `XDEBUG_TRIGGER` set
//! - Header `X-Bougie-Force-Xdebug: 1` (for scripted use)
//!
//! Cookie/query presence matches xdebug's own trigger discovery so
//! browser extensions like Xdebug Helper work without configuration.

use axum::http::HeaderMap;

const FORCE_HEADER: &str = "x-bougie-force-xdebug";

/// Returns `true` if the request should route to the `xdebug` variant.
pub fn is_xdebug_request(headers: &HeaderMap, query: &str) -> bool {
    if force_header_set(headers) {
        return true;
    }
    if cookie_present(headers, "XDEBUG_SESSION") || cookie_present(headers, "XDEBUG_TRIGGER") {
        return true;
    }
    query_param_present(query, "XDEBUG_SESSION_START")
        || query_param_present(query, "XDEBUG_TRIGGER")
}

fn force_header_set(headers: &HeaderMap) -> bool {
    headers
        .get(FORCE_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == "1")
}

/// Look for `name=` in any `Cookie:` header. Per RFC 6265 cookies are
/// `; `-separated `name=value` pairs within a single header; we accept
/// multiple Cookie headers too.
fn cookie_present(headers: &HeaderMap, name: &str) -> bool {
    for v in headers.get_all(axum::http::header::COOKIE) {
        let Ok(s) = v.to_str() else { continue };
        for pair in s.split(';') {
            let pair = pair.trim();
            // Match `name=value` (xdebug doesn't care about the value)
            // or a bare `name` token. Compare the key portion case-
            // sensitively — cookie names are case-sensitive per RFC.
            let key = pair.split('=').next().unwrap_or(pair).trim();
            if key == name {
                return true;
            }
        }
    }
    false
}

/// Look for `name` (with or without `=`) in a `&`-separated query
/// string. The query is already URL-decoded enough at this layer
/// because we only need the *key* — xdebug's trigger semantics ignore
/// the value.
fn query_param_present(query: &str, name: &str) -> bool {
    if query.is_empty() {
        return false;
    }
    for pair in query.split('&') {
        let key = pair.split('=').next().unwrap_or(pair);
        if key == name {
            return true;
        }
    }
    false
}

/// Which pool variant a request resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Normal,
    Xdebug,
}

impl Variant {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Xdebug => "xdebug",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn h(cookie: Option<&str>, force: Option<&str>) -> HeaderMap {
        let mut hm = HeaderMap::new();
        if let Some(c) = cookie {
            hm.insert(axum::http::header::COOKIE, HeaderValue::from_str(c).unwrap());
        }
        if let Some(v) = force {
            hm.insert(FORCE_HEADER, HeaderValue::from_str(v).unwrap());
        }
        hm
    }

    #[test]
    fn no_triggers_is_normal() {
        assert!(!is_xdebug_request(&HeaderMap::new(), ""));
        assert!(!is_xdebug_request(&h(Some("sid=abc"), None), "page=1"));
    }

    #[test]
    fn xdebug_session_cookie_triggers() {
        assert!(is_xdebug_request(&h(Some("XDEBUG_SESSION=phpstorm"), None), ""));
    }

    #[test]
    fn xdebug_trigger_cookie_triggers() {
        assert!(is_xdebug_request(&h(Some("XDEBUG_TRIGGER=1"), None), ""));
    }

    #[test]
    fn other_cookie_named_xdebug_does_not_trigger() {
        assert!(!is_xdebug_request(&h(Some("XDEBUG_OTHER=1"), None), ""));
    }

    #[test]
    fn cookie_in_compound_header_triggers() {
        let hm = h(Some("sid=abc; XDEBUG_SESSION=foo; theme=dark"), None);
        assert!(is_xdebug_request(&hm, ""));
    }

    #[test]
    fn query_param_xdebug_session_start_triggers() {
        assert!(is_xdebug_request(&HeaderMap::new(), "XDEBUG_SESSION_START=1"));
    }

    #[test]
    fn query_param_xdebug_trigger_triggers() {
        assert!(is_xdebug_request(&HeaderMap::new(), "page=1&XDEBUG_TRIGGER=1"));
    }

    #[test]
    fn query_param_unset_value_still_triggers() {
        assert!(is_xdebug_request(&HeaderMap::new(), "XDEBUG_TRIGGER"));
    }

    #[test]
    fn force_header_triggers() {
        assert!(is_xdebug_request(&h(None, Some("1")), ""));
    }

    #[test]
    fn force_header_zero_does_not_trigger() {
        assert!(!is_xdebug_request(&h(None, Some("0")), ""));
    }

    #[test]
    fn cookie_name_match_is_case_sensitive() {
        // RFC 6265: cookie names are case-sensitive. `xdebug_session`
        // lowercase is a different cookie name; don't trigger on it.
        assert!(!is_xdebug_request(&h(Some("xdebug_session=foo"), None), ""));
    }
}
