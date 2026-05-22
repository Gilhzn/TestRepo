//! Secret scanning.
//!
//! A pre-commit guard that flags likely credentials before they land in
//! history — where, once committed and synced, they're effectively public
//! to everyone with the repo. Detects common high-signal patterns (AWS
//! keys, private-key blocks, bearer/API tokens, generic `secret = "..."`
//! assignments with high-entropy values).
//!
//! This is deliberately a *warning* gate the CLI surfaces, not a hard
//! cryptographic control: secret detection is heuristic and false-positive
//! prone, so the policy decision (block vs. warn vs. allow with `--force`)
//! lives at the call site, not here.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub path: String,
    pub line: usize,
    pub rule: String,
    pub excerpt: String,
}

/// Scan one file's bytes for likely secrets. `path` is used only for
/// reporting. Binary files (non-UTF-8) are skipped.
pub fn scan_file(path: &str, bytes: &[u8]) -> Vec<Finding> {
    let text = match std::str::from_utf8(bytes) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let mut findings = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line_no = i + 1;
        for (rule, hit) in scan_line(line) {
            findings.push(Finding {
                path: path.to_string(),
                line: line_no,
                rule: rule.to_string(),
                excerpt: redact(&hit),
            });
        }
    }
    findings
}

fn scan_line(line: &str) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();

    // Private key PEM headers.
    if line.contains("-----BEGIN") && line.contains("PRIVATE KEY-----") {
        out.push(("private-key-block", line.trim().to_string()));
    }

    // AWS access key id: AKIA followed by 16 uppercase alphanumerics.
    if let Some(m) = find_aws_key(line) {
        out.push(("aws-access-key-id", m));
    }

    // GitHub-style token prefixes.
    for prefix in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"] {
        if let Some(tok) = find_prefixed_token(line, prefix, 20) {
            out.push(("github-token", tok));
        }
    }

    // Slack tokens.
    if line.contains("xoxb-") || line.contains("xoxp-") {
        out.push(("slack-token", line.trim().to_string()));
    }

    // Generic assignment of a secret-ish key to a high-entropy literal.
    if let Some(val) = generic_secret_assignment(line) {
        if shannon_entropy(&val) > 3.5 && val.len() >= 16 {
            out.push(("high-entropy-secret-assignment", val));
        }
    }

    out
}

fn find_aws_key(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let needle = b"AKIA";
    for start in 0..bytes.len().saturating_sub(needle.len()) {
        if &bytes[start..start + 4] == needle {
            let rest = &line[start + 4..];
            let key: String = rest.chars().take(16).collect();
            if key.len() == 16
                && key
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            {
                return Some(format!("AKIA{key}"));
            }
        }
    }
    None
}

fn find_prefixed_token(line: &str, prefix: &str, min_suffix: usize) -> Option<String> {
    let idx = line.find(prefix)?;
    let rest = &line[idx + prefix.len()..];
    let suffix: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if suffix.len() >= min_suffix {
        Some(format!("{prefix}{suffix}"))
    } else {
        None
    }
}

fn generic_secret_assignment(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    let keys = [
        "secret", "password", "passwd", "token", "api_key", "apikey",
        "access_key", "private_key", "client_secret",
    ];
    if !keys.iter().any(|k| lower.contains(k)) {
        return None;
    }
    // Find a quoted value on the line.
    let bytes = line.as_bytes();
    let quote = bytes.iter().position(|&b| b == b'"' || b == b'\'')?;
    let qch = bytes[quote];
    let after = &line[quote + 1..];
    let end = after.find(qch as char)?;
    let val = after[..end].to_string();
    if val.is_empty() {
        None
    } else {
        Some(val)
    }
}

fn shannon_entropy(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut counts = std::collections::HashMap::new();
    for c in s.chars() {
        *counts.entry(c).or_insert(0u32) += 1;
    }
    let len = s.chars().count() as f64;
    -counts
        .values()
        .map(|&c| {
            let p = c as f64 / len;
            p * p.log2()
        })
        .sum::<f64>()
}

fn redact(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.len() <= 8 {
        return "********".to_string();
    }
    let head: String = trimmed.chars().take(4).collect();
    format!("{head}…(+{} chars redacted)", trimmed.len() - 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_aws_access_key() {
        let f = scan_file("config.txt", b"aws_key = AKIAIOSFODNN7EXAMPLE");
        assert!(f.iter().any(|x| x.rule == "aws-access-key-id"));
    }

    #[test]
    fn detects_private_key_block() {
        let f = scan_file(
            "id_rsa",
            b"-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA...",
        );
        assert!(f.iter().any(|x| x.rule == "private-key-block"));
    }

    #[test]
    fn detects_github_token() {
        let f = scan_file(
            "ci.env",
            b"GITHUB_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz0123456789",
        );
        assert!(f.iter().any(|x| x.rule == "github-token"));
    }

    #[test]
    fn detects_high_entropy_secret_assignment() {
        let f = scan_file(
            "settings.py",
            b"SECRET_KEY = \"x9Kf2mNp7qRsT4uV8wYz1aB3cD5eF6gH\"",
        );
        assert!(
            f.iter()
                .any(|x| x.rule == "high-entropy-secret-assignment"),
            "expected high-entropy finding, got {f:?}"
        );
    }

    #[test]
    fn ignores_low_entropy_assignment() {
        let f = scan_file("settings.py", b"password = \"password\"");
        // "password" is low entropy + short → not flagged by the entropy rule.
        assert!(!f
            .iter()
            .any(|x| x.rule == "high-entropy-secret-assignment"));
    }

    #[test]
    fn skips_binary_files() {
        let f = scan_file("blob.bin", &[0xff, 0xfe, 0x00, 0x01, 0x80]);
        assert!(f.is_empty());
    }

    #[test]
    fn clean_file_has_no_findings() {
        let f = scan_file("main.rs", b"fn main() { println!(\"hello\"); }");
        assert!(f.is_empty());
    }

    #[test]
    fn excerpt_is_redacted() {
        let f = scan_file(
            "x",
            b"api_key = \"x9Kf2mNp7qRsT4uV8wYz1aB3cD5eF6gH\"",
        );
        let finding = f
            .iter()
            .find(|x| x.rule == "high-entropy-secret-assignment")
            .unwrap();
        assert!(finding.excerpt.contains("redacted"));
        assert!(!finding.excerpt.contains("9Kf2mNp7qRsT4uV8wYz1"));
    }

    #[test]
    fn reports_correct_line_number() {
        let f = scan_file(
            "multi.txt",
            b"line one\nline two\naws = AKIAIOSFODNN7EXAMPLE\nline four",
        );
        let hit = f.iter().find(|x| x.rule == "aws-access-key-id").unwrap();
        assert_eq!(hit.line, 3);
    }
}
