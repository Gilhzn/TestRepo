//! Lifecycle hooks.
//!
//! Executable scripts in `<repo>/.mosaic/hooks/<name>` run at well-known
//! points. A non-zero exit from a *pre* hook aborts the operation; *post*
//! hooks are advisory (their exit code is reported but doesn't block).
//!
//! Recognized hook names: `pre-commit`, `post-commit`, `pre-push`,
//! `post-push`. The runner is generic, so a deployment can invent more.
//!
//! Hooks receive context via environment variables (e.g. `MOSAIC_BRANCH`,
//! `MOSAIC_INTENT`) and inherit the repo's working directory as CWD.

use crate::error::{Error, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookKind {
    /// A non-zero exit aborts the operation.
    Pre,
    /// Advisory; exit code reported but never blocks.
    Post,
}

#[derive(Debug, Clone)]
pub struct HookOutcome {
    pub ran: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl HookOutcome {
    pub fn not_present() -> Self {
        Self {
            ran: false,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    pub fn succeeded(&self) -> bool {
        !self.ran || self.exit_code == Some(0)
    }
}

fn hooks_dir(repo_parent: &Path) -> PathBuf {
    repo_parent.join(".mosaic").join("hooks")
}

fn classify(name: &str) -> HookKind {
    if name.starts_with("pre-") {
        HookKind::Pre
    } else {
        HookKind::Post
    }
}

/// Run the named hook if it exists and is executable. `cwd` is the repo's
/// working directory (the parent of `.mosaic`). `env` adds context vars.
pub fn run(
    repo_parent: &Path,
    name: &str,
    env: &BTreeMap<String, String>,
) -> Result<HookOutcome> {
    let path = hooks_dir(repo_parent).join(name);
    if !path.exists() {
        return Ok(HookOutcome::not_present());
    }

    // Ensure it's executable (best-effort on Unix).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.permissions().mode() & 0o111 == 0 {
                return Err(Error::Serialization(format!(
                    "hook {name} exists but is not executable (chmod +x it)"
                )));
            }
        }
    }

    let mut cmd = Command::new(&path);
    cmd.current_dir(repo_parent);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let output = cmd.output().map_err(Error::Io)?;
    Ok(HookOutcome {
        ran: true,
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Convenience: run a hook and turn a failing *pre* hook into an Err so the
/// caller can abort the operation cleanly.
pub fn run_gating(
    repo_parent: &Path,
    name: &str,
    env: &BTreeMap<String, String>,
) -> Result<HookOutcome> {
    let outcome = run(repo_parent, name, env)?;
    if matches!(classify(name), HookKind::Pre) && !outcome.succeeded() {
        return Err(Error::Serialization(format!(
            "{name} hook failed (exit {:?}); aborting.{}",
            outcome.exit_code,
            if outcome.stderr.trim().is_empty() {
                String::new()
            } else {
                format!(" stderr: {}", outcome.stderr.trim())
            }
        )));
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn write_hook(dir: &Path, name: &str, script: &str) {
        let hooks = dir.join(".mosaic").join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let path = hooks.join(name);
        std::fs::write(&path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
    }

    #[test]
    fn missing_hook_is_a_noop() {
        let dir = TempDir::new().unwrap();
        let out = run(dir.path(), "pre-commit", &BTreeMap::new()).unwrap();
        assert!(!out.ran);
        assert!(out.succeeded());
    }

    #[test]
    fn passing_pre_hook_succeeds() {
        let dir = TempDir::new().unwrap();
        write_hook(dir.path(), "pre-commit", "#!/bin/sh\nexit 0\n");
        let out = run_gating(dir.path(), "pre-commit", &BTreeMap::new()).unwrap();
        assert!(out.ran);
        assert!(out.succeeded());
    }

    #[test]
    fn failing_pre_hook_aborts() {
        let dir = TempDir::new().unwrap();
        write_hook(
            dir.path(),
            "pre-commit",
            "#!/bin/sh\necho 'blocked by policy' 1>&2\nexit 1\n",
        );
        let err = run_gating(dir.path(), "pre-commit", &BTreeMap::new()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("pre-commit hook failed"));
        assert!(msg.contains("blocked by policy"));
    }

    #[test]
    fn failing_post_hook_does_not_abort() {
        let dir = TempDir::new().unwrap();
        write_hook(dir.path(), "post-commit", "#!/bin/sh\nexit 3\n");
        // run_gating must NOT error for a post hook even on nonzero exit.
        let out = run_gating(dir.path(), "post-commit", &BTreeMap::new()).unwrap();
        assert!(out.ran);
        assert_eq!(out.exit_code, Some(3));
    }

    #[test]
    fn hook_receives_env_vars() {
        let dir = TempDir::new().unwrap();
        write_hook(
            dir.path(),
            "pre-commit",
            "#!/bin/sh\ntest \"$MOSAIC_BRANCH\" = \"main\" || exit 1\n",
        );
        let mut env = BTreeMap::new();
        env.insert("MOSAIC_BRANCH".to_string(), "main".to_string());
        assert!(run_gating(dir.path(), "pre-commit", &env).is_ok());

        env.insert("MOSAIC_BRANCH".to_string(), "wrong".to_string());
        assert!(run_gating(dir.path(), "pre-commit", &env).is_err());
    }

    #[test]
    fn non_executable_hook_errors() {
        let dir = TempDir::new().unwrap();
        let hooks = dir.path().join(".mosaic").join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::write(hooks.join("pre-commit"), "#!/bin/sh\nexit 0\n").unwrap();
        // No chmod +x.
        #[cfg(unix)]
        {
            let r = run(dir.path(), "pre-commit", &BTreeMap::new());
            assert!(r.is_err());
        }
    }
}
