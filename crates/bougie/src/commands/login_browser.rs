//! Browser sign-in for `bougie login`: OAuth 2.0 authorization code with PKCE
//! (RFC 6749 §4.1, RFC 7636), the flow `bougie login` prefers when the
//! registry advertises it.
//!
//! The browser opens the dashboard's consent page, and the approval comes
//! straight back to a listener on `127.0.0.1` for the length of the login —
//! no code to type, no polling. When the browser can't reach this machine (a
//! remote shell, a container), the user opens the printed link instead, and
//! the dashboard shows a code to paste here. Both are waited for at once;
//! whichever arrives first wins.
//!
//! The CLI is a public client: PKCE proves the code was asked for by this
//! process, so a code seen by anyone else is useless to them.

use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

use eyre::{Result, WrapErr, eyre};
use serde::Deserialize;

/// The client id the registry knows the bougie CLI by.
const CLIENT_ID: &str = "bougie-cli";

/// How long to wait for the browser before giving up.
const LOGIN_TIMEOUT: Duration = Duration::from_mins(10);

/// The parts of the registry's RFC 8414 metadata this flow needs.
#[derive(Debug, Deserialize)]
pub(crate) struct Metadata {
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    code_challenge_methods_supported: Vec<String>,
    /// sconce's own field: the dashboard page that shows a code to paste.
    #[serde(default)]
    bougie_paste_redirect_uri: Option<String>,
}

/// Fetch the registry's authorization-server metadata. `None` when the
/// registry doesn't offer this flow (an older sconce answers 404), so the
/// caller falls back to the device flow.
pub(crate) fn discover(client: &reqwest::blocking::Client, base: &str) -> Option<Metadata> {
    let resp = client
        .get(format!("{base}/.well-known/oauth-authorization-server"))
        .send()
        .ok()?;
    if !resp.status().is_success() {
        tracing::debug!(status = %resp.status(), "login: no authorization-server metadata");
        return None;
    }
    let meta: Metadata = resp.json().ok()?;
    meta.code_challenge_methods_supported
        .iter()
        .any(|m| m == "S256")
        .then_some(meta)
}

/// A code delivered by the browser, and the redirect URI it was issued for
/// (the token request must repeat it).
struct Delivered {
    code: String,
    redirect_uri: String,
}

/// Sign in through the browser. Returns the access token, or `Ok(None)` when
/// neither the loopback listener nor the paste page is available, so the
/// caller can fall back to the device flow.
pub(crate) fn sign_in(
    client: &reqwest::blocking::Client,
    meta: &Metadata,
) -> Result<Option<String>> {
    let verifier = random_string(64);
    let challenge = s256(&verifier);
    let state = random_string(32);

    let listener = TcpListener::bind("127.0.0.1:0").ok();
    let loopback_uri = listener
        .as_ref()
        .and_then(|l| l.local_addr().ok())
        .map(|a| format!("http://127.0.0.1:{}/callback", a.port()));
    let paste_uri = meta.bougie_paste_redirect_uri.clone();
    if loopback_uri.is_none() && paste_uri.is_none() {
        return Ok(None);
    }

    let auth_url = |redirect_uri: &str| {
        authorize_url(
            &meta.authorization_endpoint,
            redirect_uri,
            &challenge,
            &state,
        )
    };
    let (tx, rx) = mpsc::channel::<Result<Delivered>>();

    // Browser on this machine: open it on the loopback URL and listen.
    if let (Some(listener), Some(uri)) = (listener, loopback_uri.clone()) {
        let tx = tx.clone();
        let state = state.clone();
        std::thread::spawn(move || {
            let _ = tx.send(listen(&listener, &state).map(|code| Delivered {
                code,
                redirect_uri: uri,
            }));
        });
    }
    // Anywhere else: a link to open by hand, and a code to paste back.
    if let Some(uri) = paste_uri.clone() {
        let tx = tx.clone();
        let state = state.clone();
        std::thread::spawn(move || {
            if let Some(result) = read_pasted(&state) {
                let _ = tx.send(result.map(|code| Delivered {
                    code,
                    redirect_uri: uri,
                }));
            }
        });
    }
    drop(tx);

    let opened = loopback_uri
        .as_deref()
        .is_some_and(|uri| crate::commands::server::open_url(&auth_url(uri)).is_ok());
    if opened {
        eprintln!("Opened your browser to sign in.");
    }
    match paste_uri.as_deref() {
        Some(uri) => {
            let lead = if opened {
                "If it didn't open, or it's on another machine, visit"
            } else {
                "To sign in, visit"
            };
            eprintln!(
                "{lead}:\n\n    {}\n\nthen paste the code it shows here.\n",
                auth_url(uri)
            );
            eprint!("Code: ");
            let _ = std::io::stderr().flush();
        }
        None if !opened => {
            eprintln!(
                "To sign in, open this link in a browser on this machine:\n\n    {}\n",
                auth_url(loopback_uri.as_deref().unwrap_or_default())
            );
        }
        None => {}
    }

    let delivered = match rx.recv_timeout(LOGIN_TIMEOUT) {
        Ok(result) => result?,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            return Err(eyre!("sign-in timed out — run `bougie login` again"));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err(eyre!("sign-in was cancelled — run `bougie login` again"));
        }
    };
    // End the `Code: ` prompt line if the browser answered first.
    if paste_uri.is_some() && loopback_uri.as_deref() == Some(delivered.redirect_uri.as_str()) {
        eprintln!();
    }
    redeem(client, meta, &delivered, &verifier).map(Some)
}

/// The consent-page URL for one redirect URI.
fn authorize_url(endpoint: &str, redirect_uri: &str, challenge: &str, state: &str) -> String {
    let params = [
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", redirect_uri),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
    ];
    let sep = if endpoint.contains('?') { '&' } else { '?' };
    format!("{endpoint}{sep}{}", form_encode(&params))
}

/// Serve the loopback redirect until a request carrying our `state` arrives.
/// Anything else (a favicon fetch, a stray or forged request) is answered and
/// ignored.
fn listen(listener: &TcpListener, state: &str) -> Result<String> {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let Some(target) = read_request_target(&mut stream) else {
            continue;
        };
        let Some(query) = target
            .strip_prefix("/callback?")
            .or_else(|| (target == "/callback").then_some(""))
        else {
            respond(&mut stream, "404 Not Found", "Not found.");
            continue;
        };
        let params = parse_query(query);
        let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        if get("state") != Some(state) {
            respond(
                &mut stream,
                "400 Bad Request",
                "This sign-in link doesn't belong to the running bougie login.",
            );
            continue;
        }
        if let Some(error) = get("error") {
            respond(
                &mut stream,
                "200 OK",
                "Sign-in was not approved. You can close this tab.",
            );
            return Err(match error {
                "access_denied" => eyre!("sign-in was denied in the browser"),
                other => eyre!("sign-in failed: {other}"),
            });
        }
        if let Some(code) = get("code").filter(|c| !c.is_empty()) {
            respond(
                &mut stream,
                "200 OK",
                "Signed in. You can close this tab and return to your terminal.",
            );
            return Ok(code.to_owned());
        }
        respond(
            &mut stream,
            "400 Bad Request",
            "No sign-in code in this request.",
        );
    }
    Err(eyre!("the sign-in listener stopped"))
}

/// Read one HTTP request head and return its target (`/callback?…`).
fn read_request_target(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buf).ok()?;
        if n == 0 || head.len() > 16 * 1024 {
            return None;
        }
        head.extend_from_slice(&buf[..n]);
    }
    let head = String::from_utf8_lossy(&head);
    let mut parts = head.lines().next()?.split(' ');
    (parts.next()? == "GET").then_some(())?;
    parts.next().map(str::to_owned)
}

fn respond(stream: &mut TcpStream, status: &str, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>bougie login</title>\
         <body style=\"font-family:system-ui,sans-serif;margin:4rem auto;max-width:32rem\">\
         <p>{message}</p>"
    );
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// Wait for the user to paste `code#state` from the dashboard. `None` when
/// stdin closes (not a terminal, or nothing to read) — the listener may still
/// deliver.
fn read_pasted(state: &str) -> Option<Result<String>> {
    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        if stdin.lock().read_line(&mut line).ok()? == 0 {
            return None;
        }
        let pasted = line.trim();
        if pasted.is_empty() {
            continue;
        }
        return Some(match pasted.split_once('#') {
            Some((code, got)) if got == state && !code.is_empty() => Ok(code.to_owned()),
            Some(_) => Err(eyre!(
                "that code is from a different sign-in — run `bougie login` again"
            )),
            None => Err(eyre!(
                "that doesn't look like a sign-in code; copy the whole line the page shows"
            )),
        });
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

/// Trade the code and PKCE verifier for the access token.
fn redeem(
    client: &reqwest::blocking::Client,
    meta: &Metadata,
    delivered: &Delivered,
    verifier: &str,
) -> Result<String> {
    let body = form_encode(&[
        ("grant_type", "authorization_code"),
        ("code", &delivered.code),
        ("redirect_uri", &delivered.redirect_uri),
        ("client_id", CLIENT_ID),
        ("code_verifier", verifier),
    ]);
    let resp = client
        .post(&meta.token_endpoint)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(body)
        .send()
        .wrap_err("contacting the registry to finish signing in")?;
    let status = resp.status();
    if status.is_success() {
        let token: TokenResponse = resp.json().wrap_err("parsing the sign-in response")?;
        return Ok(token.access_token);
    }
    let body: serde_json::Value = resp.json().unwrap_or_default();
    match body.get("error").and_then(serde_json::Value::as_str) {
        Some("invalid_grant") => Err(eyre!(
            "the sign-in code was rejected — it may have expired or been used. Run `bougie login` again"
        )),
        Some(other) => Err(eyre!("finishing sign-in failed: {other}")),
        None => Err(eyre!("finishing sign-in failed with HTTP {status}")),
    }
}

/// `BASE64URL(SHA256(verifier))`, unpadded — the S256 challenge.
fn s256(verifier: &str) -> String {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// `len` random characters from the PKCE verifier alphabet (RFC 7636 §4.1),
/// from a CSPRNG seeded by the OS.
fn random_string(len: usize) -> String {
    use rand::Rng as _;
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| char::from(ALPHABET[rng.gen_range(0..ALPHABET.len())]))
        .collect()
}

/// `application/x-www-form-urlencoded` (also fine as a query string).
fn form_encode(params: &[(&str, &str)]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for (i, (k, v)) in params.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        for (j, s) in [k, v].into_iter().enumerate() {
            if j == 1 {
                out.push('=');
            }
            for b in s.bytes() {
                if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                    out.push(char::from(b));
                } else {
                    let _ = write!(out, "%{b:02X}");
                }
            }
        }
    }
    out
}

/// Split a query string into decoded `(name, value)` pairs.
fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| char::from(b).to_digit(16);
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                match (
                    bytes.get(i + 1).and_then(|&b| hex(b)),
                    bytes.get(i + 2).and_then(|&b| hex(b)),
                ) {
                    (Some(hi), Some(lo)) => {
                        out.push(u8::try_from(hi * 16 + lo).unwrap_or(b'?'));
                        i += 2;
                    }
                    _ => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s256_matches_openssl() {
        // printf %s "$v" | openssl dgst -sha256 -binary | basenc --base64url
        assert_eq!(
            s256("dBjftJeZ4CVP-mJ92K9u0IQUVjD9Kob7j4kN1_kmQ3Y"),
            "Ec9IvtMd785zId6XVxyiEaErn5iBJrDupf7sT_YRe6o"
        );
    }

    #[test]
    fn verifier_is_long_and_unreserved() {
        let v = random_string(64);
        assert_eq!(v.len(), 64);
        assert!(
            v.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
        );
        assert_ne!(v, random_string(64));
    }

    #[test]
    fn query_round_trip() {
        let encoded = form_encode(&[
            ("redirect_uri", "http://127.0.0.1:1/callback"),
            ("s", "a b&c"),
        ]);
        assert_eq!(
            encoded,
            "redirect_uri=http%3A%2F%2F127.0.0.1%3A1%2Fcallback&s=a%20b%26c"
        );
        assert_eq!(
            parse_query(&encoded),
            vec![
                (
                    "redirect_uri".to_owned(),
                    "http://127.0.0.1:1/callback".to_owned()
                ),
                ("s".to_owned(), "a b&c".to_owned()),
            ]
        );
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz+x"), "%zz x");
    }

    #[test]
    fn listener_ignores_wrong_state_and_returns_the_code() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || listen(&listener, "st8"));
        let get = |target: &str| {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(s, "GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
            let mut resp = String::new();
            s.read_to_string(&mut resp).unwrap();
            resp
        };
        assert!(get("/favicon.ico").starts_with("HTTP/1.1 404"));
        assert!(get("/callback?code=nope&state=other").starts_with("HTTP/1.1 400"));
        assert!(get("/callback?code=scac_1&state=st8").starts_with("HTTP/1.1 200"));
        assert_eq!(handle.join().unwrap().unwrap(), "scac_1");
    }

    #[test]
    fn listener_reports_a_denial() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || listen(&listener, "st8"));
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            s,
            "GET /callback?error=access_denied&state=st8 HTTP/1.1\r\n\r\n"
        )
        .unwrap();
        let err = handle.join().unwrap().unwrap_err().to_string();
        assert!(err.contains("denied"), "{err}");
    }
}
