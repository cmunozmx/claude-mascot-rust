//! Claude Mascot hook shim (Rust port of the Python version).
//!
//! Registered in ~/.claude/settings.json. Claude Code invokes this once per
//! hook event with the event's tag as the first argument, e.g.:
//!
//!     mascot-hook AGENT_START
//!     mascot-hook NOTIFY
//!
//! For NOTIFY, Claude Code also writes a JSON payload to stdin; we pull a
//! short "message" field out of it with a minimal hand-rolled scan (no JSON
//! dependency, since this runs on every hook event and should stay snappy
//! to build and to start). Never panics: any failure here is swallowed so a
//! broken mascot never breaks your actual session.

use std::io::Read;
use std::net::UdpSocket;

const UDP_ADDR: &str = "127.0.0.1:8765";

/// Best-effort extraction of `"message":"..."` from a raw JSON blob,
/// without pulling in a JSON parser for this one field.
fn extract_message(raw: &str) -> Option<String> {
    let key = "\"message\"";
    let key_pos = raw.find(key)?;
    let after_key = &raw[key_pos + key.len()..];
    let colon = after_key.find(':')?;
    let after_colon = after_key[colon + 1..].trim_start();
    let after_colon = after_colon.strip_prefix('"')?;

    let mut result = String::new();
    let mut chars = after_colon.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(result),
            '\\' => {
                if let Some(next) = chars.next() {
                    result.push(next);
                }
            }
            _ => result.push(c),
        }
    }
    None
}

fn main() {
    let tag = match std::env::args().nth(1) {
        Some(t) => t.trim().to_uppercase(),
        None => return,
    };

    let payload = if tag == "NOTIFY" {
        let mut raw = String::new();
        let _ = std::io::stdin().read_to_string(&mut raw);
        match extract_message(&raw) {
            Some(msg) if !msg.is_empty() => format!("NOTIFY:{msg}"),
            _ => "NOTIFY:notification".to_string(),
        }
    } else {
        tag
    };

    if let Ok(sock) = UdpSocket::bind("127.0.0.1:0") {
        let _ = sock.send_to(payload.as_bytes(), UDP_ADDR);
    }
    // Any error here is intentionally ignored — see module docs.
}
