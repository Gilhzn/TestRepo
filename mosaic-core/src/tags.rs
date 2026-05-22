//! Tags & releases.
//!
//! A tag is an immutable named pointer to a change — `v1.2.0`, a release
//! marker, a build snapshot. Two flavors:
//!
//!   - **Lightweight**: just `name → ChangeId`.
//!   - **Annotated**: carries a tagger identity, message, timestamp, and an
//!     ed25519 signature so a release can be cryptographically attributed.
//!
//! Stored as JSON at `<repo>/.mosaic/tags/<name>.json`. Names are validated
//! like ref names (no path separators, dots-only, control chars).

use crate::error::{Error, Result};
use crate::hash::{Hash, Hasher};
use crate::m1::change::{ChangeId, Tai64N};
use crate::m1::identity::Identity;
use crate::m1::signing::{Signature, SigningKey, VerifyingKey};
use crate::repo::Repository;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tag {
    pub name: String,
    pub target: ChangeId,
    /// Present for annotated/signed tags; None for lightweight.
    pub annotation: Option<Annotation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Annotation {
    pub tagger: Identity,
    pub tagger_key: VerifyingKey,
    pub ts: Tai64N,
    pub message: String,
    pub sig: Signature,
}

#[derive(Serialize)]
struct AnnotationView<'a> {
    name: &'a str,
    target: &'a ChangeId,
    tagger: &'a Identity,
    tagger_key: &'a VerifyingKey,
    ts: &'a Tai64N,
    message: &'a str,
}

impl Tag {
    pub fn lightweight(name: impl Into<String>, target: ChangeId) -> Self {
        Self {
            name: name.into(),
            target,
            annotation: None,
        }
    }

    pub fn annotated(
        name: impl Into<String>,
        target: ChangeId,
        tagger: Identity,
        message: impl Into<String>,
        key: &SigningKey,
    ) -> Self {
        let name = name.into();
        let message = message.into();
        let tagger_key = key.verifying_key();
        let ts = Tai64N::now();
        let view = AnnotationView {
            name: &name,
            target: &target,
            tagger: &tagger,
            tagger_key: &tagger_key,
            ts: &ts,
            message: &message,
        };
        let bytes =
            bincode::serialize(&view).expect("annotation canonical serialization infallible");
        let sig = key.sign(&bytes);
        Self {
            name,
            target,
            annotation: Some(Annotation {
                tagger,
                tagger_key,
                ts,
                message,
                sig,
            }),
        }
    }

    pub fn is_signed(&self) -> bool {
        self.annotation.is_some()
    }

    /// Verify an annotated tag's signature (no-op Ok for lightweight).
    pub fn verify(&self) -> Result<()> {
        let Some(a) = &self.annotation else {
            return Ok(());
        };
        let view = AnnotationView {
            name: &self.name,
            target: &self.target,
            tagger: &a.tagger,
            tagger_key: &a.tagger_key,
            ts: &a.ts,
            message: &a.message,
        };
        let bytes = bincode::serialize(&view)
            .map_err(|e| Error::Serialization(e.to_string()))?;
        a.tagger_key.verify(&bytes, &a.sig)
    }

    pub fn id(&self) -> Result<Hash> {
        let mut h = Hasher::new();
        h.update(b"mosaic.tag.v1");
        h.update(self.name.as_bytes());
        h.update(self.target.0.as_bytes());
        Ok(h.finalize())
    }
}

fn tags_dir(repo_parent: &Path) -> PathBuf {
    repo_parent.join(".mosaic").join("tags")
}

fn repo_parent(repo: &Repository) -> PathBuf {
    repo.root()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| repo.root().to_path_buf())
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || name.chars().any(|c| c.is_control())
    {
        return Err(Error::InvalidRefName(name.to_string()));
    }
    Ok(())
}

pub fn create(repo: &Repository, tag: &Tag) -> Result<()> {
    validate_name(&tag.name)?;
    tag.verify()?;
    let parent = repo_parent(repo);
    let dir = tags_dir(&parent);
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", tag.name));
    if path.exists() {
        return Err(Error::Serialization(format!(
            "tag {} already exists",
            tag.name
        )));
    }
    let raw = serde_json::to_string_pretty(tag)
        .map_err(|e| Error::Serialization(e.to_string()))?;
    fs::write(path, raw)?;
    Ok(())
}

pub fn get(repo: &Repository, name: &str) -> Result<Tag> {
    validate_name(name)?;
    let parent = repo_parent(repo);
    let path = tags_dir(&parent).join(format!("{name}.json"));
    let raw = fs::read_to_string(&path)
        .map_err(|_| Error::RefNotFound(format!("tag {name}")))?;
    let tag: Tag =
        serde_json::from_str(&raw).map_err(|e| Error::Serialization(e.to_string()))?;
    tag.verify()?;
    Ok(tag)
}

pub fn list(repo: &Repository) -> Result<Vec<Tag>> {
    let parent = repo_parent(repo);
    let dir = tags_dir(&parent);
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for e in fs::read_dir(&dir)? {
        let e = e?;
        if e.path().extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let raw = fs::read_to_string(e.path())?;
        if let Ok(tag) = serde_json::from_str::<Tag>(&raw) {
            out.push(tag);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

pub fn delete(repo: &Repository, name: &str) -> Result<()> {
    validate_name(name)?;
    let parent = repo_parent(repo);
    let path = tags_dir(&parent).join(format!("{name}.json"));
    fs::remove_file(&path).map_err(|_| Error::RefNotFound(format!("tag {name}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::m1::change::{ChangeBuilder, FileChange, FileKind};
    use crate::m1::signing::SigningKey;
    use tempfile::TempDir;

    fn seed(dir: &Path) -> (Repository, ChangeId, Identity, SigningKey) {
        let mut repo = Repository::init(dir).unwrap();
        let idn = Identity::human("rel@example.com", Some("Releaser".into())).unwrap();
        let key = SigningKey::generate();
        let id = repo
            .commit(
                ChangeBuilder::new(idn.clone(), key.clone())
                    .intent("v1")
                    .file(FileChange {
                        path: "x".into(),
                        kind: FileKind::Text,
                        patch: b"1".to_vec(),
                        conflicts: Vec::new(),
                    })
                    .build()
                    .unwrap(),
            )
            .unwrap();
        repo.advance_branch("main", id).unwrap();
        (repo, id, idn, key)
    }

    #[test]
    fn lightweight_tag_round_trip() {
        let dir = TempDir::new().unwrap();
        let (repo, id, _, _) = seed(dir.path());
        create(&repo, &Tag::lightweight("v1.0.0", id)).unwrap();
        let back = get(&repo, "v1.0.0").unwrap();
        assert_eq!(back.target, id);
        assert!(!back.is_signed());
    }

    #[test]
    fn annotated_tag_signs_and_verifies() {
        let dir = TempDir::new().unwrap();
        let (repo, id, idn, key) = seed(dir.path());
        let tag = Tag::annotated("v2.0.0", id, idn, "Second major release", &key);
        assert!(tag.verify().is_ok());
        create(&repo, &tag).unwrap();
        let back = get(&repo, "v2.0.0").unwrap();
        assert!(back.is_signed());
        assert_eq!(
            back.annotation.unwrap().message,
            "Second major release"
        );
    }

    #[test]
    fn tampered_annotation_fails_verify() {
        let dir = TempDir::new().unwrap();
        let (_repo, id, idn, key) = seed(dir.path());
        let mut tag = Tag::annotated("v3", id, idn, "msg", &key);
        if let Some(a) = tag.annotation.as_mut() {
            a.message = "evil rewrite".into();
        }
        assert!(tag.verify().is_err());
    }

    #[test]
    fn duplicate_tag_rejected() {
        let dir = TempDir::new().unwrap();
        let (repo, id, _, _) = seed(dir.path());
        create(&repo, &Tag::lightweight("dup", id)).unwrap();
        assert!(create(&repo, &Tag::lightweight("dup", id)).is_err());
    }

    #[test]
    fn list_is_alphabetical_and_delete_works() {
        let dir = TempDir::new().unwrap();
        let (repo, id, _, _) = seed(dir.path());
        create(&repo, &Tag::lightweight("v0.2", id)).unwrap();
        create(&repo, &Tag::lightweight("v0.1", id)).unwrap();
        let names: Vec<String> = list(&repo).unwrap().into_iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["v0.1", "v0.2"]);
        delete(&repo, "v0.1").unwrap();
        assert_eq!(list(&repo).unwrap().len(), 1);
    }

    #[test]
    fn invalid_names_rejected() {
        let dir = TempDir::new().unwrap();
        let (repo, id, _, _) = seed(dir.path());
        for bad in ["", "a/b", "..", "x\ny"] {
            assert!(create(&repo, &Tag::lightweight(bad, id)).is_err());
        }
    }
}
