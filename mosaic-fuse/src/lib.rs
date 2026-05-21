//! Read-only FUSE filesystem exposing a Mosaic branch as a virtual working tree.
//!
//! A directory tree is materialized from the latest change reachable from
//! the branch frontier. Each path that appears in any change's `body` is a
//! file; intermediate path components are directories. File content is
//! served from the change body itself (no separate working-copy directory).
//!
//! This is a v1 snapshot mount: the tree is computed at mount time and held
//! in memory. Writes are not yet supported. Re-mount to see new changes.

use fuser::{FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, Request};
use mosaic_core::error::{Error, Result};
use mosaic_core::m1::change::{ChangeId, FileKind};
use mosaic_core::repo::Repository;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(1);
const ROOT_INODE: u64 = 1;

#[derive(Clone, Debug)]
struct Entry {
    name: String,
    parent: u64,
    inode: u64,
    kind: EntryKind,
}

#[derive(Clone, Debug)]
enum EntryKind {
    Dir,
    File { bytes: Vec<u8>, mode: u16 },
}

pub struct MosaicFs {
    by_inode: BTreeMap<u64, Entry>,
    children: BTreeMap<u64, BTreeMap<String, u64>>,
    next_inode: u64,
}

impl MosaicFs {
    /// Build the virtual tree by walking every change reachable from the
    /// branch's frontier, then keeping the **latest** value per file path
    /// in topological order so newer commits shadow older ones.
    pub fn from_branch(repo_root: impl AsRef<Path>, branch: &str) -> Result<Self> {
        let repo = Repository::open(repo_root)?;
        let frontier = match repo.refs().get(branch) {
            Ok(f) => f,
            Err(Error::RefNotFound(_)) => Default::default(),
            Err(e) => return Err(e),
        };

        let mut ancestors: std::collections::BTreeSet<mosaic_core::Hash> =
            Default::default();
        let mut queue: std::collections::VecDeque<mosaic_core::Hash> =
            frontier.0.iter().copied().collect();
        while let Some(h) = queue.pop_front() {
            if !ancestors.insert(h) {
                continue;
            }
            if let Some(ps) = repo.index().parents_of(&h) {
                for p in ps {
                    queue.push_back(*p);
                }
            }
        }

        let ordered = repo.topo_order(&ancestors);

        // Latest content wins per path.
        let mut latest_per_path: BTreeMap<String, (Vec<u8>, FileKind)> = BTreeMap::new();
        for h in ordered {
            let change = repo.load_change(&ChangeId(h))?;
            for file in change.body {
                latest_per_path.insert(file.path, (file.patch, file.kind));
            }
        }

        let mut fs = Self {
            by_inode: BTreeMap::new(),
            children: BTreeMap::new(),
            next_inode: ROOT_INODE + 1,
        };
        fs.by_inode.insert(
            ROOT_INODE,
            Entry {
                name: "/".into(),
                parent: ROOT_INODE,
                inode: ROOT_INODE,
                kind: EntryKind::Dir,
            },
        );
        fs.children.insert(ROOT_INODE, BTreeMap::new());

        for (path, (bytes, kind)) in latest_per_path {
            fs.insert_path(&path, bytes, kind);
        }

        Ok(fs)
    }

    fn insert_path(&mut self, path: &str, bytes: Vec<u8>, kind: FileKind) {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            return;
        }
        let mut parent = ROOT_INODE;
        for (i, seg) in parts.iter().enumerate() {
            let is_last = i + 1 == parts.len();
            let existing = self
                .children
                .get(&parent)
                .and_then(|m| m.get(*seg))
                .copied();
            let inode = match existing {
                Some(ino) => ino,
                None => {
                    let ino = self.next_inode;
                    self.next_inode += 1;
                    let new_kind = if is_last {
                        EntryKind::File {
                            bytes: std::mem::take(&mut Vec::clone(&bytes)),
                            mode: match kind {
                                FileKind::Binary => 0o644,
                                FileKind::Text => 0o644,
                                FileKind::Tree => 0o755,
                            },
                        }
                    } else {
                        EntryKind::Dir
                    };
                    let entry = Entry {
                        name: (*seg).to_string(),
                        parent,
                        inode: ino,
                        kind: new_kind,
                    };
                    self.by_inode.insert(ino, entry);
                    self.children
                        .entry(parent)
                        .or_default()
                        .insert((*seg).to_string(), ino);
                    if !is_last {
                        self.children.entry(ino).or_default();
                    }
                    ino
                }
            };
            if is_last {
                // Overwrite with the supplied content.
                if let Some(e) = self.by_inode.get_mut(&inode) {
                    e.kind = EntryKind::File {
                        bytes,
                        mode: match kind {
                            FileKind::Binary | FileKind::Text => 0o644,
                            FileKind::Tree => 0o755,
                        },
                    };
                }
                return;
            }
            parent = inode;
        }
    }

    pub fn inode_count(&self) -> usize {
        self.by_inode.len()
    }

    pub fn file_count(&self) -> usize {
        self.by_inode
            .values()
            .filter(|e| matches!(e.kind, EntryKind::File { .. }))
            .count()
    }

    pub fn dir_count(&self) -> usize {
        self.by_inode
            .values()
            .filter(|e| matches!(e.kind, EntryKind::Dir))
            .count()
    }

    /// Look up a path under the root and return its inode + content
    /// (for tests / debugging without a real mount).
    pub fn lookup_path(&self, path: &str) -> Option<&Entry> {
        let mut cur = ROOT_INODE;
        for seg in path.split('/').filter(|s| !s.is_empty()) {
            let next = self.children.get(&cur)?.get(seg)?;
            cur = *next;
        }
        self.by_inode.get(&cur)
    }

    pub fn read_file_path(&self, path: &str) -> Option<&[u8]> {
        let entry = self.lookup_path(path)?;
        match &entry.kind {
            EntryKind::File { bytes, .. } => Some(bytes),
            _ => None,
        }
    }

    fn file_attr(&self, entry: &Entry) -> FileAttr {
        let (kind, size, mode) = match &entry.kind {
            EntryKind::Dir => (FileType::Directory, 0, 0o755),
            EntryKind::File { bytes, mode } => (FileType::RegularFile, bytes.len() as u64, *mode),
        };
        FileAttr {
            ino: entry.inode,
            size,
            blocks: (size + 511) / 512,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind,
            perm: mode,
            nlink: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            flags: 0,
            blksize: 4096,
        }
    }
}

impl Filesystem for MosaicFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        let child_inode = self
            .children
            .get(&parent)
            .and_then(|m| m.get(name_str))
            .copied();
        match child_inode {
            Some(ino) => match self.by_inode.get(&ino) {
                Some(entry) => reply.entry(&TTL, &self.file_attr(entry), 0),
                None => reply.error(libc::ENOENT),
            },
            None => reply.error(libc::ENOENT),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
        match self.by_inode.get(&ino) {
            Some(entry) => reply.attr(&TTL, &self.file_attr(entry)),
            None => reply.error(libc::ENOENT),
        }
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let entry = match self.by_inode.get(&ino) {
            Some(e) => e,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        match &entry.kind {
            EntryKind::File { bytes, .. } => {
                let start = offset.max(0) as usize;
                if start >= bytes.len() {
                    reply.data(&[]);
                    return;
                }
                let end = (start + size as usize).min(bytes.len());
                reply.data(&bytes[start..end]);
            }
            EntryKind::Dir => reply.error(libc::EISDIR),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let entry = match self.by_inode.get(&ino) {
            Some(e) => e,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        if !matches!(entry.kind, EntryKind::Dir) {
            reply.error(libc::ENOTDIR);
            return;
        }

        let mut entries: Vec<(u64, FileType, String)> = Vec::new();
        entries.push((ino, FileType::Directory, ".".into()));
        entries.push((entry.parent, FileType::Directory, "..".into()));
        if let Some(children) = self.children.get(&ino) {
            for (name, child_ino) in children {
                let kind = match self.by_inode.get(child_ino).map(|e| &e.kind) {
                    Some(EntryKind::Dir) => FileType::Directory,
                    Some(EntryKind::File { .. }) => FileType::RegularFile,
                    None => continue,
                };
                entries.push((*child_ino, kind, name.clone()));
            }
        }

        for (i, (e_ino, e_kind, e_name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(e_ino, (i + 1) as i64, e_kind, &e_name) {
                break;
            }
        }
        reply.ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mosaic_core::m1::change::{ChangeBuilder, FileChange};
    use mosaic_core::m1::identity::Identity;
    use mosaic_core::m1::signing::SigningKey;
    use tempfile::TempDir;

    fn human() -> (Identity, SigningKey) {
        (
            Identity::human("dev@example.com", Some("Dev".into())).unwrap(),
            SigningKey::generate(),
        )
    }

    fn write_repo_with(files: &[(&str, &[u8])]) -> TempDir {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let (idn, key) = human();
        let mut builder = ChangeBuilder::new(idn, key).intent("seed");
        for (path, bytes) in files {
            builder = builder.file(FileChange {
                path: (*path).into(),
                kind: FileKind::Text,
                patch: bytes.to_vec(),
                conflicts: Vec::new(),
            });
        }
        let id = repo.commit(builder.build().unwrap()).unwrap();
        repo.advance_branch("main", id).unwrap();
        dir
    }

    #[test]
    fn empty_branch_yields_just_the_root() {
        let dir = TempDir::new().unwrap();
        let _ = Repository::init(dir.path()).unwrap();
        let fs = MosaicFs::from_branch(dir.path(), "main").unwrap();
        assert_eq!(fs.inode_count(), 1);
        assert_eq!(fs.file_count(), 0);
    }

    #[test]
    fn flat_files_appear_under_root() {
        let dir = write_repo_with(&[("README.md", b"hi"), ("LICENSE", b"MIT")]);
        let fs = MosaicFs::from_branch(dir.path(), "main").unwrap();
        assert_eq!(fs.file_count(), 2);
        assert_eq!(fs.read_file_path("README.md"), Some(&b"hi"[..]));
        assert_eq!(fs.read_file_path("LICENSE"), Some(&b"MIT"[..]));
    }

    #[test]
    fn nested_paths_create_directories() {
        let dir = write_repo_with(&[
            ("src/main.rs", b"fn main() {}"),
            ("src/lib.rs", b"pub fn lib() {}"),
            ("tests/it.rs", b"#[test] fn it() {}"),
        ]);
        let fs = MosaicFs::from_branch(dir.path(), "main").unwrap();
        // root + src + tests + 3 files = 6 inodes
        assert_eq!(fs.inode_count(), 6);
        assert_eq!(fs.dir_count(), 3);
        assert!(matches!(
            fs.lookup_path("src").unwrap().kind,
            EntryKind::Dir
        ));
        assert_eq!(
            fs.read_file_path("src/main.rs"),
            Some(&b"fn main() {}"[..])
        );
        assert_eq!(
            fs.read_file_path("tests/it.rs"),
            Some(&b"#[test] fn it() {}"[..])
        );
    }

    #[test]
    fn later_change_shadows_earlier_at_same_path() {
        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let (idn, key) = human();

        let c1 = ChangeBuilder::new(idn.clone(), key.clone())
            .intent("first")
            .file(FileChange {
                path: "a.txt".into(),
                kind: FileKind::Text,
                patch: b"first content".to_vec(),
                conflicts: Vec::new(),
            })
            .build()
            .unwrap();
        let id1 = repo.commit(c1).unwrap();

        let c2 = ChangeBuilder::new(idn, key)
            .intent("second")
            .dep(id1)
            .file(FileChange {
                path: "a.txt".into(),
                kind: FileKind::Text,
                patch: b"second content".to_vec(),
                conflicts: Vec::new(),
            })
            .build()
            .unwrap();
        let id2 = repo.commit(c2).unwrap();
        repo.advance_branch("main", id2).unwrap();

        let fs = MosaicFs::from_branch(dir.path(), "main").unwrap();
        assert_eq!(fs.read_file_path("a.txt"), Some(&b"second content"[..]));
    }

    #[test]
    fn missing_branch_yields_empty_root() {
        let dir = TempDir::new().unwrap();
        let _ = Repository::init(dir.path()).unwrap();
        let fs = MosaicFs::from_branch(dir.path(), "does-not-exist").unwrap();
        assert_eq!(fs.inode_count(), 1);
    }

    #[test]
    fn lookup_path_returns_none_for_missing() {
        let dir = write_repo_with(&[("a", b"a")]);
        let fs = MosaicFs::from_branch(dir.path(), "main").unwrap();
        assert!(fs.lookup_path("nope/here").is_none());
        assert!(fs.read_file_path("nope").is_none());
    }
}
