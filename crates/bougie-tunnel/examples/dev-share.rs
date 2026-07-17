//! Dev harness — dial a relay and expose a local port. NOT shipped; this is the
//! stand-in for the future `bougie share` command, used to exercise
//! `bougie-tunnel` (and `bougie-relay`) end-to-end against a local, possibly
//! self-signed, relay.
//!
//! Env:
//! ```text
//!   RELAY_ADDR        default 127.0.0.1:7443
//!   RELAY_SNI         default tunnel.bougie.test
//!   LOCAL_PORT        default 9000   (the local server to expose)
//!   PROJECT           default myshop (seeds the slug)
//!   PUBLIC            set → no view password (default: generated password)
//!   BOUGIE_TUNNEL_CA  path to a CA PEM to additionally trust (self-signed relay)
//! ```

use bougie_tunnel::{PasswordMode, TunnelClient, TunnelConfig};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let cfg = TunnelConfig {
        relay_addr: env("RELAY_ADDR", "127.0.0.1:7443"),
        relay_sni: env("RELAY_SNI", "tunnel.bougie.test"),
        local_port: env("LOCAL_PORT", "9000").parse()?,
        project: env("PROJECT", "myshop"),
        token: std::env::var("SHARE_TOKEN").ok().filter(|t| !t.is_empty()),
        slug: None,
        password_mode: if std::env::var("PUBLIC").is_ok() {
            PasswordMode::None
        } else {
            PasswordMode::Auto
        },
        password: None,
        resume: None,
    };

    let (handle, serving) = TunnelClient::new(cfg).open().await?;
    println!("SHARE_URL={}", handle.url);
    println!("SHARE_HOST={}", handle.host);
    if let Some(pw) = &handle.view_password {
        println!("SHARE_PASSWORD={pw}");
    }
    println!("share up; Ctrl-C to stop");
    serving.run().await?;
    Ok(())
}

fn env(k: &str, default: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| default.to_owned())
}
