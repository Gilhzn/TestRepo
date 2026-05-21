use crate::agent::MosaicAgent;
use crate::session::Session;
use mosaic_core::error::Result;
use mosaic_core::m1::change::ChangeId;

/// A speculative branch. An agent can use this to try several approaches in
/// parallel: each `Speculation` is a real Mosaic branch the agent owns, and
/// at the end it can either `promote()` (publish) or `discard()` (delete
/// the branch ref; commits stay in the DAG and can be revived later).
pub struct Speculation<'a> {
    agent: &'a MosaicAgent,
    name: String,
}

impl<'a> Speculation<'a> {
    pub(crate) fn create(
        agent: &'a MosaicAgent,
        name: String,
        base_branch: &str,
    ) -> Result<Self> {
        let repo = agent.open_repo()?;
        let base = repo.refs().get(base_branch).unwrap_or_default();
        repo.refs().put(&name, &base)?;
        Ok(Self { agent, name })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn session(&self, intent: impl Into<String>) -> Result<Session> {
        self.agent.begin_session_on(self.name.clone(), intent)
    }

    /// Commit the speculation's tips into another branch (typically `main`)
    /// by writing the speculation's frontier there.
    pub fn promote(self, into: &str) -> Result<()> {
        let repo = self.agent.open_repo()?;
        let tips = repo.refs().get(&self.name)?;
        repo.refs().put(into, &tips)
    }

    pub fn discard(self) -> Result<()> {
        let repo = self.agent.open_repo()?;
        repo.refs().delete(&self.name)
    }

    pub fn history(&self) -> Result<Vec<ChangeId>> {
        self.agent.history_on(&self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MosaicAgent;
    use mosaic_core::m1::identity::Identity;
    use mosaic_core::m1::signing::SigningKey;
    use tempfile::TempDir;

    fn agent() -> (TempDir, MosaicAgent) {
        let dir = TempDir::new().unwrap();
        let idn = Identity::human("dev@example.com", Some("Dev".into())).unwrap();
        let agent = MosaicAgent::init(dir.path(), idn, SigningKey::generate()).unwrap();
        (dir, agent)
    }

    fn one_commit_on_main(agent: &MosaicAgent, intent: &str) -> ChangeId {
        let mut s = agent.begin_session(intent).unwrap();
        s.stage_text(format!("{intent}.txt"), intent.as_bytes().to_vec());
        s.commit().unwrap()
    }

    #[test]
    fn speculation_diverges_then_promotes() {
        let (_dir, a) = agent();
        let base = one_commit_on_main(&a, "base");

        let spec = a.try_branch("perf-experiment", "main").unwrap();
        let mut s = spec.session("try LRU cache").unwrap();
        s.stage_text("cache.rs", b"lru cache here".to_vec());
        let speculative_tip = s.commit().unwrap();

        let main_after_speculation = a.history_on("main").unwrap();
        assert_eq!(main_after_speculation, vec![base]);

        let spec_history = spec.history().unwrap();
        assert!(spec_history.contains(&speculative_tip));

        spec.promote("main").unwrap();
        let main_after_promote = a.history_on("main").unwrap();
        assert!(main_after_promote.contains(&speculative_tip));
    }

    #[test]
    fn speculation_can_be_discarded() {
        let (_dir, a) = agent();
        one_commit_on_main(&a, "base");

        let spec = a.try_branch("doomed", "main").unwrap();
        let mut s = spec.session("won't last").unwrap();
        s.stage_text("x", b"x".to_vec());
        s.commit().unwrap();

        spec.discard().unwrap();
        assert!(!a.branches().unwrap().iter().any(|b| b == "doomed"));
    }

    #[test]
    fn three_parallel_speculations_dont_interfere() {
        let (_dir, a) = agent();
        let _ = one_commit_on_main(&a, "base");

        let strategies = ["lru", "lfu", "tinylfu"];
        for s in strategies {
            let spec = a.try_branch(s, "main").unwrap();
            let mut sess = spec.session(format!("try {s}")).unwrap();
            sess.stage_text(format!("cache_{s}.rs"), s.as_bytes().to_vec());
            sess.commit().unwrap();
        }

        let branches = a.branches().unwrap();
        for s in strategies {
            assert!(branches.iter().any(|b| b == s));
        }
    }
}
