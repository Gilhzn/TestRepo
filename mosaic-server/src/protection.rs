//! Branch protection + path-scoped policy engine.
//!
//! Augments [`crate::auth::Policy`] (per-key push allowlist) with rules that
//! reason about *what* is being pushed, not just *who* is pushing. A rule is
//! attached to a glob pattern matching either a branch name or a file path
//! in the change body. Rules can require signed changes, demand a minimum
//! number of `Verdict::Approved` review approvals, block force-pushes,
//! and deny/allow specific authors.
//!
//! Rules live on disk at `<repo_root>/.mosaic/protection.json` and are
//! reloaded each time [`crate::AppState`] is constructed.

use mosaic_core::m1::change::Change;
use mosaic_core::m1_dag::dag::DagIndex;
use mosaic_core::m1_dag::refs::Frontier;
use mosaic_core::Hash;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Top-level set of protection rules persisted under `.mosaic/protection.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProtectionRules {
    #[serde(default)]
    pub branches: Vec<BranchRule>,
    #[serde(default)]
    pub paths: Vec<PathRule>,
}

/// One per-branch rule. `pattern` is matched against the branch name with
/// the simple glob in [`glob_match`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BranchRule {
    pub pattern: String,
    #[serde(default)]
    pub require_signed: bool,
    #[serde(default)]
    pub require_approvals: u32,
    #[serde(default)]
    pub allow_force_push: bool,
    /// `Identity::id()` strings that are explicitly banned (e.g. "human:alice@x").
    #[serde(default)]
    pub deny_authors: Vec<String>,
    /// If non-empty, only these author ids may push to a matching branch.
    #[serde(default)]
    pub allow_only_authors: Vec<String>,
}

/// One per-path rule. Matched against each `FileChange.path` in the bundle.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PathRule {
    pub pattern: String,
    #[serde(default)]
    pub deny_authors: Vec<String>,
    #[serde(default)]
    pub allow_only_authors: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProtectionError {
    #[error("change {change} on protected branch {branch} is not signed")]
    UnsignedChangeOnProtectedBranch { change: String, branch: String },
    #[error("change {change} needs {need} approval(s), has {have}")]
    ApprovalsRequired {
        change: String,
        have: u32,
        need: u32,
    },
    #[error("force push to branch {branch} denied by policy")]
    ForcePushDenied { branch: String },
    #[error("author {author} is banned from branch {branch}")]
    AuthorBannedFromBranch { author: String, branch: String },
    #[error("author {author} cannot touch path {path} (rule {pattern})")]
    AuthorBannedFromPath {
        author: String,
        path: String,
        pattern: String,
    },
}

impl ProtectionRules {
    /// Load rules from `<repo_root>/.mosaic/protection.json`. Missing file
    /// yields `Ok(default)`.
    pub fn load(repo_root: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = repo_root
            .as_ref()
            .join(".mosaic")
            .join("protection.json");
        match fs::read_to_string(&path) {
            Ok(s) => {
                let rules: Self = serde_json::from_str(&s).map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
                })?;
                Ok(rules)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self, repo_root: impl AsRef<Path>) -> std::io::Result<()> {
        let dir = repo_root.as_ref().join(".mosaic");
        fs::create_dir_all(&dir)?;
        let path = dir.join("protection.json");
        let body = serde_json::to_string_pretty(self).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
        })?;
        fs::write(path, body)?;
        Ok(())
    }

    /// True when *no* rules are configured.
    pub fn is_empty(&self) -> bool {
        self.branches.is_empty() && self.paths.is_empty()
    }

    /// Evaluate every applicable rule against an incoming push. Caller is
    /// expected to have already verified bundle signatures via the lower
    /// layer; this routine adds the higher-level policy gates.
    ///
    /// - `branch`: the target branch name.
    /// - `new_frontier`: the frontier the push would install.
    /// - `existing_frontier`: what `branch` currently resolves to on the
    ///   server (use `Frontier::default()` for a brand-new branch).
    /// - `changes`: every change carried in the bundle. Path rules are
    ///   evaluated against each change's file paths.
    /// - `approvals_per_change`: `Verdict::Approved` count per change id.
    /// - `dag`: server-side DAG index, used for ancestor lookups when
    ///   determining force-push.
    pub fn check_push(
        &self,
        branch: &str,
        new_frontier: &Frontier,
        existing_frontier: &Frontier,
        changes: &[Change],
        approvals_per_change: &HashMap<Hash, u32>,
        dag: &DagIndex,
    ) -> Result<(), ProtectionError> {
        let branch_rules: Vec<&BranchRule> = self
            .branches
            .iter()
            .filter(|r| glob_match(&r.pattern, branch))
            .collect();

        // Branch-level gates apply to every change going to this branch.
        for rule in &branch_rules {
            // Force-push: at least one existing tip must be reachable from
            // some new tip (or be equal to it). Otherwise this is a
            // non-fast-forward / force push.
            if !rule.allow_force_push && !existing_frontier.0.is_empty() {
                let ff = is_fast_forward(existing_frontier, new_frontier, dag);
                if !ff {
                    return Err(ProtectionError::ForcePushDenied {
                        branch: branch.to_string(),
                    });
                }
            }

            for change in changes {
                if rule.require_signed && change.verify().is_err() {
                    return Err(ProtectionError::UnsignedChangeOnProtectedBranch {
                        change: change.id().to_hex(),
                        branch: branch.to_string(),
                    });
                }

                if rule.require_approvals > 0 {
                    let have = approvals_per_change
                        .get(&change.id().0)
                        .copied()
                        .unwrap_or(0);
                    if have < rule.require_approvals {
                        return Err(ProtectionError::ApprovalsRequired {
                            change: change.id().to_hex(),
                            have,
                            need: rule.require_approvals,
                        });
                    }
                }

                let author_id = change.author.id();
                if rule.deny_authors.iter().any(|a| a == &author_id) {
                    return Err(ProtectionError::AuthorBannedFromBranch {
                        author: author_id,
                        branch: branch.to_string(),
                    });
                }
                if !rule.allow_only_authors.is_empty()
                    && !rule.allow_only_authors.iter().any(|a| a == &author_id)
                {
                    return Err(ProtectionError::AuthorBannedFromBranch {
                        author: author_id,
                        branch: branch.to_string(),
                    });
                }
            }
        }

        // Path-level gates: applied per (change, file) pair.
        for change in changes {
            let author_id = change.author.id();
            for file in &change.body {
                for rule in &self.paths {
                    if !glob_match(&rule.pattern, &file.path) {
                        continue;
                    }
                    if rule.deny_authors.iter().any(|a| a == &author_id) {
                        return Err(ProtectionError::AuthorBannedFromPath {
                            author: author_id,
                            path: file.path.clone(),
                            pattern: rule.pattern.clone(),
                        });
                    }
                    if !rule.allow_only_authors.is_empty()
                        && !rule.allow_only_authors.iter().any(|a| a == &author_id)
                    {
                        return Err(ProtectionError::AuthorBannedFromPath {
                            author: author_id,
                            path: file.path.clone(),
                            pattern: rule.pattern.clone(),
                        });
                    }
                }
            }
        }

        Ok(())
    }
}

/// A push is a fast-forward when every existing tip is either equal to or
/// an ancestor of at least one new tip. Anything else is a force push.
fn is_fast_forward(existing: &Frontier, new: &Frontier, dag: &DagIndex) -> bool {
    for existing_tip in &existing.0 {
        let covered = new.0.iter().any(|new_tip| {
            new_tip == existing_tip || dag.is_ancestor(existing_tip, new_tip)
        });
        if !covered {
            return false;
        }
    }
    true
}

/// Tiny glob matcher: `*` matches a single path segment (no `/`); `**`
/// matches any sequence including `/`; everything else is literal.
pub fn glob_match(pattern: &str, target: &str) -> bool {
    glob_match_inner(pattern.as_bytes(), target.as_bytes())
}

fn glob_match_inner(pat: &[u8], txt: &[u8]) -> bool {
    let mut pi = 0usize;
    let mut ti = 0usize;
    // Backtrack point for the most recent `*` (single-segment): pattern
    // position right after the `*`, text position to retry from.
    let mut star: Option<(usize, usize)> = None;
    // Backtrack point for the most recent `**` (multi-segment).
    let mut double_star: Option<(usize, usize)> = None;

    while ti < txt.len() {
        if pi < pat.len() {
            if pat[pi] == b'*' {
                if pi + 1 < pat.len() && pat[pi + 1] == b'*' {
                    // `**` — match zero-or-more of any byte; record
                    // backtrack and consume the second `*`.
                    pi += 2;
                    double_star = Some((pi, ti));
                    continue;
                } else {
                    pi += 1;
                    star = Some((pi, ti));
                    continue;
                }
            }
            if pat[pi] == txt[ti] {
                pi += 1;
                ti += 1;
                continue;
            }
        }

        // Mismatch: try backtracking through `*` (cannot cross `/`),
        // otherwise `**` (anything).
        if let Some((sp, st)) = star {
            if txt[st] != b'/' {
                pi = sp;
                ti = st + 1;
                star = Some((sp, ti));
                continue;
            } else {
                star = None;
            }
        }
        if let Some((sp, st)) = double_star {
            pi = sp;
            ti = st + 1;
            double_star = Some((sp, ti));
            continue;
        }
        return false;
    }

    // Consumed all of txt — pattern must be all `*`/`**` from here.
    while pi < pat.len() {
        if pat[pi] != b'*' {
            return false;
        }
        pi += 1;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use mosaic_core::m1::change::{ChangeBuilder, ChangeId, FileChange, FileKind};
    use mosaic_core::m1::identity::Identity;
    use mosaic_core::m1::signing::SigningKey;
    use mosaic_core::m1_dag::vclock::VectorClock;
    use tempfile::TempDir;

    fn human(email: &str) -> (Identity, SigningKey) {
        (
            Identity::human(email, None).unwrap(),
            SigningKey::generate(),
        )
    }

    fn fc(path: &str) -> FileChange {
        FileChange {
            path: path.into(),
            kind: FileKind::Text,
            patch: b"x".to_vec(),
            conflicts: Vec::new(),
        }
    }

    fn build_change(idn: &Identity, key: &SigningKey, intent: &str, file: &str) -> Change {
        ChangeBuilder::new(idn.clone(), key.clone())
            .intent(intent)
            .file(fc(file))
            .build()
            .unwrap()
    }

    /// Build a DagIndex with the given pre-registered changes (linear chain
    /// in argument order).
    fn linear_dag(changes: &[&Change]) -> DagIndex {
        let mut idx = DagIndex::new();
        let mut parents: Vec<Hash> = Vec::new();
        for c in changes {
            idx.register(c.id().0, parents.clone(), VectorClock::new())
                .unwrap();
            parents = vec![c.id().0];
        }
        idx
    }

    #[test]
    fn default_rules_allow_everything() {
        let rules = ProtectionRules::default();
        let (idn, key) = human("eyal@example.com");
        let change = build_change(&idn, &key, "x", "a.txt");
        let dag = linear_dag(&[&change]);
        let new = Frontier::from_iter([change.id().0]);
        let existing = Frontier::default();
        rules
            .check_push("main", &new, &existing, &[change], &HashMap::new(), &dag)
            .unwrap();
    }

    #[test]
    fn branch_rule_require_signed_rejects_unsigned() {
        let rules = ProtectionRules {
            branches: vec![BranchRule {
                pattern: "main".into(),
                require_signed: true,
                require_approvals: 0,
                allow_force_push: true,
                deny_authors: Vec::new(),
                allow_only_authors: Vec::new(),
            }],
            paths: Vec::new(),
        };
        let (idn, key) = human("eyal@example.com");
        let mut change = build_change(&idn, &key, "x", "a.txt");
        // Tamper to invalidate signature.
        change.intent = Some("tampered".into());

        let dag = DagIndex::new();
        let new = Frontier::from_iter([change.id().0]);
        let existing = Frontier::default();
        let err = rules
            .check_push("main", &new, &existing, &[change], &HashMap::new(), &dag)
            .unwrap_err();
        assert!(matches!(
            err,
            ProtectionError::UnsignedChangeOnProtectedBranch { .. }
        ));
    }

    #[test]
    fn branch_rule_require_approvals_threshold() {
        let rules = ProtectionRules {
            branches: vec![BranchRule {
                pattern: "main".into(),
                require_signed: false,
                require_approvals: 2,
                allow_force_push: true,
                deny_authors: Vec::new(),
                allow_only_authors: Vec::new(),
            }],
            paths: Vec::new(),
        };
        let (idn, key) = human("eyal@example.com");
        let change = build_change(&idn, &key, "x", "a.txt");
        let dag = linear_dag(&[&change]);
        let new = Frontier::from_iter([change.id().0]);
        let existing = Frontier::default();

        // 1 approval — should fail.
        let mut approvals = HashMap::new();
        approvals.insert(change.id().0, 1u32);
        let err = rules
            .check_push(
                "main",
                &new,
                &existing,
                std::slice::from_ref(&change),
                &approvals,
                &dag,
            )
            .unwrap_err();
        assert!(matches!(err, ProtectionError::ApprovalsRequired { .. }));

        // 2 approvals — should succeed.
        approvals.insert(change.id().0, 2u32);
        rules
            .check_push(
                "main",
                &new,
                &existing,
                std::slice::from_ref(&change),
                &approvals,
                &dag,
            )
            .unwrap();
    }

    #[test]
    fn force_push_blocked() {
        let rules = ProtectionRules {
            branches: vec![BranchRule {
                pattern: "main".into(),
                require_signed: false,
                require_approvals: 0,
                allow_force_push: false,
                deny_authors: Vec::new(),
                allow_only_authors: Vec::new(),
            }],
            paths: Vec::new(),
        };
        let (idn, key) = human("eyal@example.com");
        // Two unrelated changes with no ancestry.
        let a = build_change(&idn, &key, "a", "a.txt");
        let b = build_change(&idn, &key, "b", "b.txt");
        let mut dag = DagIndex::new();
        dag.register(a.id().0, vec![], VectorClock::new()).unwrap();
        dag.register(b.id().0, vec![], VectorClock::new()).unwrap();

        let existing = Frontier::from_iter([a.id().0]);
        let new = Frontier::from_iter([b.id().0]);
        let err = rules
            .check_push("main", &new, &existing, &[b], &HashMap::new(), &dag)
            .unwrap_err();
        assert!(matches!(err, ProtectionError::ForcePushDenied { .. }));
    }

    #[test]
    fn force_push_fast_forward_allowed() {
        let rules = ProtectionRules {
            branches: vec![BranchRule {
                pattern: "main".into(),
                require_signed: false,
                require_approvals: 0,
                allow_force_push: false,
                deny_authors: Vec::new(),
                allow_only_authors: Vec::new(),
            }],
            paths: Vec::new(),
        };
        let (idn, key) = human("eyal@example.com");
        let a = build_change(&idn, &key, "a", "a.txt");
        let b = ChangeBuilder::new(idn.clone(), key.clone())
            .intent("b")
            .dep(ChangeId(a.id().0))
            .file(fc("b.txt"))
            .build()
            .unwrap();
        let mut dag = DagIndex::new();
        dag.register(a.id().0, vec![], VectorClock::new()).unwrap();
        dag.register(b.id().0, vec![a.id().0], VectorClock::new())
            .unwrap();

        let existing = Frontier::from_iter([a.id().0]);
        let new = Frontier::from_iter([b.id().0]);
        rules
            .check_push("main", &new, &existing, &[b], &HashMap::new(), &dag)
            .unwrap();
    }

    #[test]
    fn author_banned_from_branch() {
        let (alice, alice_key) = human("alice@example.com");
        let rules = ProtectionRules {
            branches: vec![BranchRule {
                pattern: "main".into(),
                require_signed: false,
                require_approvals: 0,
                allow_force_push: true,
                deny_authors: vec![alice.id()],
                allow_only_authors: Vec::new(),
            }],
            paths: Vec::new(),
        };
        let change = build_change(&alice, &alice_key, "x", "a.txt");
        let dag = linear_dag(&[&change]);
        let new = Frontier::from_iter([change.id().0]);
        let existing = Frontier::default();
        let err = rules
            .check_push("main", &new, &existing, &[change], &HashMap::new(), &dag)
            .unwrap_err();
        assert!(matches!(
            err,
            ProtectionError::AuthorBannedFromBranch { .. }
        ));
    }

    #[test]
    fn author_banned_from_path() {
        let (alice, alice_key) = human("alice@example.com");
        let rules = ProtectionRules {
            branches: Vec::new(),
            paths: vec![PathRule {
                pattern: "secrets/**".into(),
                deny_authors: vec![alice.id()],
                allow_only_authors: Vec::new(),
            }],
        };
        let change = build_change(&alice, &alice_key, "x", "secrets/keys.env");
        let dag = linear_dag(&[&change]);
        let new = Frontier::from_iter([change.id().0]);
        let existing = Frontier::default();
        let err = rules
            .check_push("main", &new, &existing, &[change], &HashMap::new(), &dag)
            .unwrap_err();
        assert!(matches!(err, ProtectionError::AuthorBannedFromPath { .. }));
    }

    #[test]
    fn glob_match_one_segment() {
        assert!(glob_match("*", "foo"));
        assert!(!glob_match("*", "foo/bar"));
        assert!(glob_match("foo/*", "foo/bar"));
        assert!(!glob_match("foo/*", "foo/bar/baz"));
        assert!(glob_match("release/*", "release/1.0"));
    }

    #[test]
    fn glob_match_multi_segment() {
        assert!(glob_match("**", "foo"));
        assert!(glob_match("**", "foo/bar"));
        assert!(glob_match("**", "foo/bar/baz"));
        assert!(glob_match("src/**", "src/a.rs"));
        assert!(glob_match("src/**", "src/sub/a.rs"));
        assert!(!glob_match("src/**", "lib/a.rs"));
    }

    #[test]
    fn rules_round_trip_through_disk() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".mosaic")).unwrap();
        let rules = ProtectionRules {
            branches: vec![BranchRule {
                pattern: "main".into(),
                require_signed: true,
                require_approvals: 2,
                allow_force_push: false,
                deny_authors: vec!["human:bad@x".into()],
                allow_only_authors: vec!["human:good@x".into()],
            }],
            paths: vec![PathRule {
                pattern: "secrets/**".into(),
                deny_authors: vec!["human:alice@x".into()],
                allow_only_authors: Vec::new(),
            }],
        };
        rules.save(dir.path()).unwrap();
        let back = ProtectionRules::load(dir.path()).unwrap();
        assert_eq!(back.branches.len(), 1);
        assert_eq!(back.branches[0].pattern, "main");
        assert!(back.branches[0].require_signed);
        assert_eq!(back.branches[0].require_approvals, 2);
        assert!(!back.branches[0].allow_force_push);
        assert_eq!(back.branches[0].deny_authors, vec!["human:bad@x"]);
        assert_eq!(back.paths.len(), 1);
        assert_eq!(back.paths[0].pattern, "secrets/**");
    }
}
