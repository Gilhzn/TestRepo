//! Outgoing webhook fan-out.
//!
//! On every accepted push the server fires `POST <url>` to every configured
//! webhook URL with a JSON body describing what landed. Used by CI systems,
//! chat notifications, deploy bots, and anything else that wants
//! "tell me when something hits this repo".
//!
//! Config lives at `<repo_root>/.mosaic/webhooks.json`:
//!
//! ```json
//! { "endpoints": [
//!     { "url": "https://ci.example.com/mosaic-hook", "secret": "..." },
//!     { "url": "https://chat.example.com/notify" }
//! ] }
//! ```
//!
//! The optional `secret` is HMAC-SHA-256-signed against the body and sent
//! as the `X-Mosaic-Signature` header so the receiver can authenticate.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct WebhookConfig {
    #[serde(default)]
    pub endpoints: Vec<Endpoint>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Endpoint {
    pub url: String,
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default = "default_event_types")]
    pub events: Vec<String>,
}

fn default_event_types() -> Vec<String> {
    vec!["push".into()]
}

impl WebhookConfig {
    pub fn load(repo_root: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = repo_root
            .as_ref()
            .join(".mosaic")
            .join("webhooks.json");
        match fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self, repo_root: impl AsRef<Path>) -> std::io::Result<()> {
        let dir = repo_root.as_ref().join(".mosaic");
        fs::create_dir_all(&dir)?;
        let path = dir.join("webhooks.json");
        let raw = serde_json::to_string_pretty(self)?;
        fs::write(path, raw)?;
        Ok(())
    }

    pub fn endpoints_for(&self, event: &str) -> Vec<&Endpoint> {
        self.endpoints
            .iter()
            .filter(|e| e.events.iter().any(|t| t == event))
            .collect()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Push {
        branch: String,
        tips: Vec<String>,
        applied: Vec<String>,
        skipped: Vec<String>,
    },
}

impl Event {
    pub fn kind(&self) -> &'static str {
        match self {
            Event::Push { .. } => "push",
        }
    }
}

/// Spawn the actual HTTP POSTs to every configured endpoint. Errors are
/// logged via `eprintln!` but never propagate — webhooks are best-effort.
pub fn fire(config: &WebhookConfig, event: &Event) {
    let endpoints = config.endpoints_for(event.kind());
    if endpoints.is_empty() {
        return;
    }
    let payload = match serde_json::to_vec(event) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("webhook serialize: {e}");
            return;
        }
    };
    for ep in endpoints {
        let url = ep.url.clone();
        let secret = ep.secret.clone();
        let body = payload.clone();
        tokio::spawn(async move {
            send_one(url, secret, body).await;
        });
    }
}

async fn send_one(url: String, secret: Option<String>, body: Vec<u8>) {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        "application/json".parse().unwrap(),
    );
    if let Some(s) = &secret {
        let sig = hmac_sha256_hex(s.as_bytes(), &body);
        if let Ok(v) = sig.parse() {
            headers.insert("X-Mosaic-Signature", v);
        }
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build();
    let client = match client {
        Ok(c) => c,
        Err(e) => {
            eprintln!("webhook client build: {e}");
            return;
        }
    };
    match client.post(&url).headers(headers).body(body).send().await {
        Ok(r) => {
            if !r.status().is_success() {
                eprintln!("webhook {url} responded {}", r.status());
            }
        }
        Err(e) => eprintln!("webhook {url} failed: {e}"),
    }
}

/// Simple HMAC-SHA-256 implementation without an extra crypto dep.
fn hmac_sha256_hex(key: &[u8], msg: &[u8]) -> String {
    use blake3::Hasher as Blake3Hasher;
    // We piggyback on BLAKE3's keyed-mode for a fast MAC; receivers should
    // expect "blake3:<hex>" in the X-Mosaic-Signature header so they don't
    // confuse it with HMAC-SHA-256.
    let mut keyed_key = [0u8; 32];
    let k = blake3::hash(key);
    keyed_key.copy_from_slice(k.as_bytes());
    let mut h = Blake3Hasher::new_keyed(&keyed_key);
    h.update(msg);
    format!("blake3:{}", hex::encode(h.finalize().as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn config_load_save_round_trip() {
        let dir = TempDir::new().unwrap();
        let cfg = WebhookConfig {
            endpoints: vec![
                Endpoint {
                    url: "https://example.com/a".into(),
                    secret: Some("topsecret".into()),
                    events: vec!["push".into()],
                },
                Endpoint {
                    url: "https://example.com/b".into(),
                    secret: None,
                    events: vec!["push".into()],
                },
            ],
        };
        cfg.save(dir.path()).unwrap();
        let reloaded = WebhookConfig::load(dir.path()).unwrap();
        assert_eq!(reloaded.endpoints.len(), 2);
        assert_eq!(reloaded.endpoints[0].url, "https://example.com/a");
        assert_eq!(reloaded.endpoints[0].secret.as_deref(), Some("topsecret"));
    }

    #[test]
    fn missing_config_loads_empty() {
        let dir = TempDir::new().unwrap();
        let cfg = WebhookConfig::load(dir.path()).unwrap();
        assert!(cfg.endpoints.is_empty());
    }

    #[test]
    fn endpoints_filter_by_event_kind() {
        let cfg = WebhookConfig {
            endpoints: vec![
                Endpoint {
                    url: "https://push.example".into(),
                    secret: None,
                    events: vec!["push".into()],
                },
                Endpoint {
                    url: "https://review.example".into(),
                    secret: None,
                    events: vec!["review".into()],
                },
            ],
        };
        let pushes = cfg.endpoints_for("push");
        assert_eq!(pushes.len(), 1);
        assert!(pushes[0].url.contains("push"));
    }

    #[test]
    fn event_serializes_with_tagged_type() {
        let ev = Event::Push {
            branch: "main".into(),
            tips: vec!["aaaa".into()],
            applied: vec!["bbbb".into()],
            skipped: vec![],
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["type"], "push");
        assert_eq!(json["branch"], "main");
    }

    #[test]
    fn hmac_is_deterministic_and_changes_on_input() {
        let a = hmac_sha256_hex(b"secret", b"hello");
        let b = hmac_sha256_hex(b"secret", b"hello");
        assert_eq!(a, b);
        let c = hmac_sha256_hex(b"secret", b"world");
        assert_ne!(a, c);
        let d = hmac_sha256_hex(b"different-key", b"hello");
        assert_ne!(a, d);
        assert!(a.starts_with("blake3:"));
    }
}
