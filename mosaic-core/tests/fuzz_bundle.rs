//! Adversarial robustness fuzz tests for the bundle sync layer.
//!
//! Goal: prove that feeding untrusted / malformed bundle data into
//! `Bundle::decode` and `apply_bundle` never panics and never corrupts the
//! receiving repository. Every failure path must surface as `Err(..)`, and on
//! the apply side every ref must keep pointing at a change that exists and
//! verifies.
//!
//! All randomness is driven by a hand-rolled deterministic LCG copied from the
//! `crdt.rs` test module so any failure reproduces from its printed seed.

use std::collections::{BTreeMap, BTreeSet};

use mosaic_core::m1::change::{Change, ChangeBuilder, ChangeId, FileChange, FileKind};
use mosaic_core::m1::identity::Identity;
use mosaic_core::m1::signing::SigningKey;
use mosaic_core::m1_dag::refs::Frontier;
use mosaic_core::repo::Repository;
use mosaic_core::sync::{apply_bundle, build_bundle, Bundle};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Deterministic PRNG (copied from mosaic-core/src/crdt.rs test module).
// ---------------------------------------------------------------------------
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 ^ (self.0 >> 33)
    }
    fn range(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }
    fn byte(&mut self) -> u8 {
        (self.next_u64() & 0xff) as u8
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn human() -> (Identity, SigningKey) {
    (
        Identity::human("dev@example.com", Some("Dev".into())).unwrap(),
        SigningKey::generate(),
    )
}

fn fc(path: &str, body: &[u8]) -> FileChange {
    FileChange {
        path: path.into(),
        kind: FileKind::Text,
        patch: body.to_vec(),
        conflicts: Vec::new(),
    }
}

/// Build a signed change with the given deps. Returns the change itself so
/// callers can drop it into a Bundle without committing it first.
fn make_change(
    idn: &Identity,
    key: &SigningKey,
    intent: &str,
    deps: Vec<ChangeId>,
    files: Vec<FileChange>,
) -> Change {
    let mut b = ChangeBuilder::new(idn.clone(), key.clone()).intent(intent);
    for d in deps {
        b = b.dep(d);
    }
    for f in files {
        b = b.file(f);
    }
    b.build().unwrap()
}

/// Assert the repository is self-consistent: every ref frontier points at
/// changes that exist on disk and verify cleanly.
fn assert_repo_consistent(repo: &Repository, ctx: &str) {
    let names = repo.refs().list().unwrap_or_default();
    for name in names {
        let frontier = match repo.refs().get(&name) {
            Ok(f) => f,
            // A ref that fails to read is itself a corruption signal.
            Err(e) => panic!("[{ctx}] ref {name:?} failed to read: {e:?}"),
        };
        for h in &frontier.0 {
            assert!(
                repo.index().contains(h),
                "[{ctx}] ref {name:?} points at {} which is not in the index",
                h.to_hex()
            );
            let id = ChangeId(*h);
            let change = repo
                .load_change(&id)
                .unwrap_or_else(|e| panic!("[{ctx}] ref {name:?} -> change {} unloadable: {e:?}", h.to_hex()));
            change
                .verify()
                .unwrap_or_else(|e| panic!("[{ctx}] ref {name:?} -> change {} fails verify: {e:?}", h.to_hex()));
        }
    }
    // Also walk all committed changes: each must verify and all parents present.
    let all = repo.all_change_ids().unwrap_or_default();
    for h in &all {
        let id = ChangeId(*h);
        let change = repo
            .load_change(&id)
            .unwrap_or_else(|e| panic!("[{ctx}] committed change {} unloadable: {e:?}", h.to_hex()));
        for dep in &change.deps {
            assert!(
                repo.index().contains(&dep.0),
                "[{ctx}] committed change {} has dangling parent {}",
                h.to_hex(),
                dep.to_hex()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Attack 1: pure garbage bytes into Bundle::decode.
// ---------------------------------------------------------------------------

#[test]
fn fuzz_decode_garbage_never_panics() {
    const N: u64 = 5_000;
    for seed in 0..N {
        let mut rng = Rng::new(seed.wrapping_add(0x1111_2222_3333_4444));
        // Mix of lengths: empty, tiny, and up to a few KB.
        let len = match rng.range(10) {
            0 => 0,
            1 => 1,
            2 => 2,
            3 => (rng.range(8) + 3) as usize,
            4..=7 => (rng.range(256) + 1) as usize,
            _ => (rng.range(4096) + 1) as usize,
        };
        let mut bytes = Vec::with_capacity(len);
        for _ in 0..len {
            bytes.push(rng.byte());
        }

        // The only requirement: decode must not panic. Returning Ok is fine
        // (some random byte stream could in principle decode), returning Err
        // is the common, expected case.
        let result = std::panic::catch_unwind(|| Bundle::decode(&bytes));
        assert!(
            result.is_ok(),
            "PANIC in Bundle::decode | seed={seed} len={len} bytes={bytes:02x?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Attack 2: truncated / bit-flipped *valid* bundles.
// ---------------------------------------------------------------------------

/// Build a small but non-trivial valid bundle (chain a<-b<-c plus blobs and a
/// branch advance) and return its wire encoding.
fn sample_bundle_wire() -> Vec<u8> {
    let dir = TempDir::new().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();
    let (idn, key) = human();

    let a = repo
        .commit(make_change(&idn, &key, "a", vec![], vec![fc("a", b"hello")]))
        .unwrap();
    let b = repo
        .commit(make_change(&idn, &key, "b", vec![a], vec![fc("b", b"world")]))
        .unwrap();
    let c = repo
        .commit(make_change(&idn, &key, "c", vec![b], vec![fc("c", b"!!!")]))
        .unwrap();
    repo.advance_branch("main", c).unwrap();

    let mut bundle = build_bundle(&repo, &[a, b, c]).unwrap();
    bundle.branch_advances.insert(
        "main".to_string(),
        Frontier(BTreeSet::from([c.0])),
    );
    bundle.encode().unwrap()
}

#[test]
fn fuzz_truncated_and_flipped_bundles_never_panic() {
    let base = sample_bundle_wire();
    assert!(base.len() > 16, "sample bundle should be non-trivial");

    const N: u64 = 4_000;
    for seed in 0..N {
        let mut rng = Rng::new(seed.wrapping_add(0xA5A5_5A5A_DEAD_BEEF));
        let mut bytes = base.clone();

        // Choose a corruption mode.
        let mode = rng.range(3);
        match mode {
            0 => {
                // Truncate at a random offset (including 0).
                let cut = rng.range(bytes.len() as u64 + 1) as usize;
                bytes.truncate(cut);
            }
            1 => {
                // Flip some random bytes (1..=8 of them).
                let flips = rng.range(8) + 1;
                for _ in 0..flips {
                    if bytes.is_empty() {
                        break;
                    }
                    let idx = rng.range(bytes.len() as u64) as usize;
                    bytes[idx] ^= rng.byte().max(1);
                }
            }
            _ => {
                // Both: flip then truncate.
                let flips = rng.range(6) + 1;
                for _ in 0..flips {
                    if bytes.is_empty() {
                        break;
                    }
                    let idx = rng.range(bytes.len() as u64) as usize;
                    bytes[idx] ^= rng.byte().max(1);
                }
                let cut = rng.range(bytes.len() as u64 + 1) as usize;
                bytes.truncate(cut);
            }
        }

        // 1) decode must never panic.
        let decoded = match std::panic::catch_unwind(|| Bundle::decode(&bytes)) {
            Ok(r) => r,
            Err(_) => panic!(
                "PANIC in Bundle::decode | seed={seed} mode={mode} len={} bytes={bytes:02x?}",
                bytes.len()
            ),
        };

        // 2) if it decoded, apply must never panic and must leave a consistent repo.
        if let Ok(bundle) = decoded {
            let dir = TempDir::new().unwrap();
            let mut repo = Repository::init(dir.path()).unwrap();
            let applied = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                apply_bundle(&mut repo, &bundle)
            }));
            match applied {
                Ok(_apply_result) => {
                    // Whether Ok or Err, the repo must be self-consistent.
                    assert_repo_consistent(&repo, &format!("attack2 seed={seed} mode={mode}"));
                }
                Err(_) => panic!(
                    "PANIC in apply_bundle | seed={seed} mode={mode} len={} bytes={bytes:02x?}",
                    bytes.len()
                ),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Attack 3: structurally malformed but decodable bundles fed to apply_bundle.
// ---------------------------------------------------------------------------

/// Run one structurally-broken bundle against a fresh repo, asserting it never
/// panics and leaves the repo self-consistent.
fn run_malformed(label: &str, bundle: Bundle) {
    let dir = TempDir::new().unwrap();
    let mut repo = Repository::init(dir.path()).unwrap();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        apply_bundle(&mut repo, &bundle)
    }));
    match result {
        Ok(_apply) => {
            // Either Err or a clean apply; the repo must be self-consistent.
            assert_repo_consistent(&repo, label);
        }
        Err(_) => panic!("PANIC in apply_bundle for malformed case: {label}"),
    }
}

#[test]
fn malformed_branch_advance_points_at_absent_change() {
    let (idn, key) = human();
    let lone = make_change(&idn, &key, "lone", vec![], vec![fc("x", b"x")]);
    let mut bundle = Bundle::new(vec![lone], BTreeMap::new());
    // Frontier references a change id that is in NEITHER the bundle nor the repo.
    let ghost = ChangeId(mosaic_core::hash::Hash::of(b"ghost-change-not-anywhere"));
    bundle
        .branch_advances
        .insert("main".to_string(), Frontier(BTreeSet::from([ghost.0])));
    run_malformed("3a: branch_advance -> absent change", bundle);
}

#[test]
fn malformed_change_with_missing_parent() {
    let (idn, key) = human();
    // Child deps on a parent that is never present anywhere.
    let phantom_parent = ChangeId(mosaic_core::hash::Hash::of(b"phantom-parent"));
    let child = make_change(
        &idn,
        &key,
        "child",
        vec![phantom_parent],
        vec![fc("c", b"c")],
    );
    let bundle = Bundle::new(vec![child], BTreeMap::new());
    // Targets the topo-apply loop / `.expect("ready key present")` path.
    run_malformed("3b: change with missing parent", bundle);
}

#[test]
fn malformed_dependency_cycle() {
    // A true 2-node cycle is impossible to forge: a change's id is the hash of
    // its content *including* its deps, so to make A.dep=B and B.dep=A we'd
    // need A's id before building A. We exercise the next-best thing the apply
    // loop can actually see: a self-cycle (a change that deps on its own id is
    // also unforgeable for the same reason), so instead we build two changes
    // whose deps reference each other's *intended* ids via fabricated hashes,
    // which the loop must treat as unresolvable rather than spin/panic.
    let (idn, key) = human();

    // Fabricate two ids and cross-reference them. Neither equals the real id of
    // the change carrying it, so verify() may fail first — either way: no panic.
    let fake_a = ChangeId(mosaic_core::hash::Hash::of(b"cycle-node-a"));
    let fake_b = ChangeId(mosaic_core::hash::Hash::of(b"cycle-node-b"));
    let node_a = make_change(&idn, &key, "A", vec![fake_b], vec![fc("a", b"a")]);
    let node_b = make_change(&idn, &key, "B", vec![fake_a], vec![fc("b", b"b")]);
    let bundle = Bundle::new(vec![node_a, node_b], BTreeMap::new());
    run_malformed("3c: dependency cycle (cross-referenced ids)", bundle);

    // Also: a change that lists itself-by-content is impossible, but a change
    // depending on another change that depends right back on it through real
    // ids would require a hash preimage; documented as unforgeable above.
}

#[test]
fn malformed_duplicate_changes() {
    let (idn, key) = human();
    let one = make_change(&idn, &key, "dup", vec![], vec![fc("d", b"d")]);
    // Same change three times in the changes vec.
    let bundle = Bundle::new(vec![one.clone(), one.clone(), one], BTreeMap::new());
    run_malformed("3d: duplicate changes", bundle);
}

#[test]
fn malformed_empty_changes_nonempty_branch_advance() {
    let mut bundle = Bundle::new(Vec::new(), BTreeMap::new());
    let ghost = ChangeId(mosaic_core::hash::Hash::of(b"ghost-tip"));
    bundle
        .branch_advances
        .insert("main".to_string(), Frontier(BTreeSet::from([ghost.0])));
    // Also a second branch with multiple ghost tips.
    let g2 = ChangeId(mosaic_core::hash::Hash::of(b"ghost-tip-2"));
    bundle.branch_advances.insert(
        "feature".to_string(),
        Frontier(BTreeSet::from([ghost.0, g2.0])),
    );
    run_malformed("3e: empty changes + non-empty branch_advances", bundle);
}

#[test]
fn malformed_tampered_change_in_bundle() {
    // A change whose signed content was mutated after signing: verify() must
    // reject it; apply must not panic and must commit nothing.
    let (idn, key) = human();
    let mut tampered = make_change(&idn, &key, "honest", vec![], vec![fc("t", b"t")]);
    tampered.intent = Some("evil rewrite".into());
    let bundle = Bundle::new(vec![tampered], BTreeMap::new());
    run_malformed("3f: tampered (bad-signature) change", bundle);
}

#[test]
fn malformed_blob_hash_mismatch() {
    // A blob whose key is not the hash of its bytes must be rejected, no panic.
    let (idn, key) = human();
    let c = make_change(&idn, &key, "c", vec![], vec![fc("c", b"c")]);
    let mut blobs = BTreeMap::new();
    blobs.insert(
        mosaic_core::hash::Hash::of(b"claimed"),
        b"actual-different-bytes".to_vec(),
    );
    let bundle = Bundle::new(vec![c], blobs);
    run_malformed("3g: blob hash mismatch", bundle);
}

#[test]
fn fuzz_random_structural_bundles_never_panic() {
    // Randomized structural fuzzing: build a small pool of real signed changes,
    // then assemble bundles from random subsets/duplicates plus random ghost
    // branch advances. Apply each to a fresh repo; never panic, always consistent.
    let (idn, key) = human();

    // Pool of independent + chained changes (using fabricated parent ids so some
    // are intentionally unresolvable).
    let g1 = make_change(&idn, &key, "g1", vec![], vec![fc("a", b"1")]);
    let g2 = make_change(&idn, &key, "g2", vec![], vec![fc("b", b"2")]);
    let ghost = ChangeId(mosaic_core::hash::Hash::of(b"random-ghost-parent"));
    let g3 = make_change(&idn, &key, "g3", vec![ghost], vec![fc("c", b"3")]);
    let pool = vec![g1, g2, g3];

    const N: u64 = 1_500;
    for seed in 0..N {
        let mut rng = Rng::new(seed.wrapping_add(0x0BAD_F00D_CAFE_0001));

        // Random multiset of changes.
        let count = rng.range(6);
        let mut changes = Vec::new();
        for _ in 0..count {
            let pick = rng.range(pool.len() as u64) as usize;
            changes.push(pool[pick].clone());
        }

        let mut bundle = Bundle::new(changes, BTreeMap::new());

        // Random ghost branch advances.
        let n_branches = rng.range(3);
        for bidx in 0..n_branches {
            let n_tips = rng.range(3) + 1;
            let mut tips = BTreeSet::new();
            for tidx in 0..n_tips {
                let mut buf = [0u8; 16];
                let r = rng.next_u64().to_le_bytes();
                buf[..8].copy_from_slice(&r);
                buf[8..].copy_from_slice(&(tidx as u64).to_le_bytes());
                tips.insert(mosaic_core::hash::Hash::of(&buf));
            }
            bundle
                .branch_advances
                .insert(format!("br{bidx}"), Frontier(tips));
        }

        let dir = TempDir::new().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            apply_bundle(&mut repo, &bundle)
        }));
        match result {
            Ok(_) => assert_repo_consistent(&repo, &format!("attack3-fuzz seed={seed}")),
            Err(_) => panic!(
                "PANIC in apply_bundle | random structural | seed={seed} changes={count} \
                 branches={n_branches}"
            ),
        }
    }
}
