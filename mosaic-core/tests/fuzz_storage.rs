//! Adversarial integrity audit for the CAS + chunker.
//!
//! Goal: prove the store NEVER returns silently-wrong data as `Ok`, and that
//! the chunker round-trips perfectly. Any round-trip mismatch, silent
//! wrong-data return, or panic is a CRITICAL bug.

use mosaic_core::chunker::{
    chunk_and_store, reassemble, AVG_CHUNK, MAX_CHUNK, MIN_CHUNK,
};
use mosaic_core::error::Error;
use mosaic_core::hash::Hash;
use mosaic_core::storage::{Cas, FsCas};
use std::fs;
use std::io::Cursor;
use std::path::PathBuf;
use tempfile::TempDir;

// Deterministic PRNG, copied verbatim from mosaic-core/src/crdt.rs tests.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1)
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
}

/// Generate `len` deterministic bytes from a seed.
fn gen_blob(seed: u64, len: usize) -> Vec<u8> {
    let mut rng = Rng::new(seed ^ 0xD1CE_5EED_u64);
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        out.extend_from_slice(&rng.next_u64().to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Reconstruct the on-disk path for a hash, mirroring FsCas's private layout:
/// `root/<hex[..2]>/<hex[2..]>`.
fn disk_path(cas: &FsCas, hash: &Hash) -> PathBuf {
    let hex = hash.to_hex();
    cas.root().join(&hex[..2]).join(&hex[2..])
}

// ---------------------------------------------------------------------------
// Test 1: Round-trip fuzz, including edge sizes and chunk boundaries.
// ---------------------------------------------------------------------------
#[test]
fn round_trip_fuzz() {
    let dir = TempDir::new().unwrap();
    let cas = FsCas::open(dir.path()).unwrap();

    // Edge sizes: empty, tiny, around min/avg/max chunk boundaries, multi-MB.
    let mut sizes: Vec<usize> = vec![0, 1, 2, 3, 7, 64, 1024];
    for boundary in [MIN_CHUNK, AVG_CHUNK, MAX_CHUNK] {
        let b = boundary as usize;
        for delta in [-2i64, -1, 0, 1, 2] {
            let s = b as i64 + delta;
            if s >= 0 {
                sizes.push(s as usize);
            }
        }
    }
    // A couple of larger multi-chunk blobs.
    sizes.push(10 * 1024 * 1024);
    sizes.push((MAX_CHUNK as usize) * 2 + 12345);

    let mut cases = 0usize;
    // Several seeds per size for randomized content.
    for size in sizes {
        for seed in 0..6u64 {
            let blob = gen_blob(seed.wrapping_add((size as u64).wrapping_mul(31)), size);
            let manifest = chunk_and_store(&cas, Cursor::new(&blob))
                .unwrap_or_else(|e| panic!("chunk_and_store failed seed={seed} size={size}: {e:?}"));

            assert_eq!(
                manifest.total_size, size as u64,
                "manifest total_size wrong: seed={seed} size={size}"
            );

            let got = reassemble(&cas, &manifest)
                .unwrap_or_else(|e| panic!("reassemble failed seed={seed} size={size}: {e:?}"));

            assert_eq!(
                got.len(),
                blob.len(),
                "ROUND-TRIP LENGTH MISMATCH: seed={seed} size={size} got_len={}",
                got.len()
            );
            assert!(
                got == blob,
                "ROUND-TRIP DATA MISMATCH: seed={seed} size={size}"
            );
            // Sanity: concatenated chunk sizes equal total.
            let sum: u64 = manifest.chunks.iter().map(|c| c.size as u64).sum();
            assert_eq!(
                sum, size as u64,
                "chunk sizes do not sum to total: seed={seed} size={size}"
            );
            cases += 1;
        }
    }
    eprintln!("round_trip_fuzz: exercised {cases} (size,seed) cases");
}

// ---------------------------------------------------------------------------
// Test 2: Corruption detection on read. Flip / truncate / append on the
// on-disk (zstd-compressed) blob, then assert get() never returns wrong Ok.
// ---------------------------------------------------------------------------
#[test]
fn corruption_detection_on_read() {
    let mut rng = Rng::new(0xC0FFEE);
    let mut total = 0usize;
    let mut detected = 0usize;
    let mut benign_ok = 0usize;

    for iter in 0..400u64 {
        // Fresh CAS per iteration so each blob's file is isolated.
        let dir = TempDir::new().unwrap();
        let cas = FsCas::open(dir.path()).unwrap();

        // Random blob size 1..=4096 (compressed file is small & easy to mangle).
        let size = (rng.range(4096) + 1) as usize;
        let blob = gen_blob(iter ^ 0xABCD, size);
        let hash = cas.put(&blob).unwrap();
        let path = disk_path(&cas, &hash);

        // Read raw compressed bytes.
        let original_raw = fs::read(&path).unwrap();
        assert!(!original_raw.is_empty(), "compressed file unexpectedly empty");

        // Choose a corruption kind: 0=flip, 1=truncate, 2=append, 3=zero-out byte.
        let kind = rng.range(4);
        let mut raw = original_raw.clone();
        match kind {
            0 => {
                let idx = rng.range(raw.len() as u64) as usize;
                let bit = 1u8 << (rng.range(8) as u8);
                raw[idx] ^= bit;
            }
            1 => {
                // Truncate to a strictly shorter length (possibly 0).
                let new_len = rng.range(raw.len() as u64) as usize;
                raw.truncate(new_len);
            }
            2 => {
                let extra = (rng.range(16) + 1) as usize;
                for _ in 0..extra {
                    raw.push(rng.range(256) as u8);
                }
            }
            _ => {
                let idx = rng.range(raw.len() as u64) as usize;
                raw[idx] = 0;
            }
        }

        // If the mutation was a no-op (e.g. XOR landed identically — it can't,
        // but truncate/zero could be no-ops), skip: nothing was corrupted.
        if raw == original_raw {
            continue;
        }

        fs::write(&path, &raw).unwrap();
        total += 1;

        // CRITICAL invariant: get() must NOT return Ok with bytes != original.
        // Use catch_unwind to also catch any panic and fail loudly.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cas.get(&hash)));

        match result {
            Err(_) => {
                panic!(
                    "PANIC in get() on corrupted blob: iter={iter} size={size} kind={kind}"
                );
            }
            Ok(Err(_)) => {
                // Any Err is acceptable (HashMismatch, or Io if zstd stream broke).
                detected += 1;
            }
            Ok(Ok(bytes)) => {
                // Only acceptable if bytes happen to equal original (benign).
                assert!(
                    bytes == blob,
                    "SILENT WRONG-DATA RETURN: get() returned Ok with corrupted bytes! \
                     iter={iter} size={size} kind={kind} got_len={}",
                    bytes.len()
                );
                benign_ok += 1;
            }
        }
    }

    eprintln!(
        "corruption_detection_on_read: {total} corruptions applied, {detected} detected as Err, \
         {benign_ok} decoded back to identical bytes (benign, never wrong data)"
    );
    assert!(total > 0, "no corruptions were actually applied");
}

// ---------------------------------------------------------------------------
// Test 3: Missing hash and content-mismatched file both yield clean Err.
// ---------------------------------------------------------------------------
#[test]
fn missing_and_mismatched_hash() {
    let dir = TempDir::new().unwrap();
    let cas = FsCas::open(dir.path()).unwrap();

    // 3a: get() on never-stored hashes => clean Err, no panic.
    for seed in 0..50u64 {
        let phantom = Hash::of(&gen_blob(seed, (seed % 17) as usize + 1));
        assert!(!cas.has(&phantom).unwrap());
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cas.get(&phantom)));
        match r {
            Ok(Err(_)) => {}
            Ok(Ok(_)) => panic!("get() returned Ok for never-stored hash seed={seed}"),
            Err(_) => panic!("get() PANICKED for never-stored hash seed={seed}"),
        }
    }

    // 3b: file whose content hashes to something else => HashMismatch.
    // Store blob A, then overwrite A's file with a valid zstd stream of blob B.
    for seed in 0..50u64 {
        let a = gen_blob(seed ^ 0x11, (seed % 100) as usize + 1);
        let b = gen_blob(seed ^ 0x22, (seed % 100) as usize + 5);
        let ha = cas.put(&a).unwrap();
        let hb = cas.put(&b).unwrap();
        if ha == hb {
            continue; // astronomically unlikely; skip if equal
        }
        // Re-encode b's compressed bytes and place them at a's path.
        let b_path = disk_path(&cas, &hb);
        let a_path = disk_path(&cas, &ha);
        let b_raw = fs::read(&b_path).unwrap();
        fs::write(&a_path, &b_raw).unwrap();

        match cas.get(&ha) {
            Err(Error::HashMismatch { expected, actual }) => {
                assert_eq!(expected, ha.to_hex());
                assert_eq!(actual, hb.to_hex());
            }
            Err(other) => panic!("expected HashMismatch, got {other:?} seed={seed}"),
            Ok(_) => panic!(
                "SILENT WRONG-DATA: get(hash_of_A) returned Ok but file held B! seed={seed}"
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Test 4: Corrupt one underlying chunk file of a multi-chunk blob, then
// reassemble => must error, never silently return wrong data, never panic.
// ---------------------------------------------------------------------------
#[test]
fn chunker_corruption_propagates() {
    let mut rng = Rng::new(0xBADC0DE);
    let mut cases = 0usize;
    let mut detected = 0usize;

    for iter in 0..20u64 {
        let dir = TempDir::new().unwrap();
        let cas = FsCas::open(dir.path()).unwrap();

        // Force a genuine multi-chunk split: a few * MAX_CHUNK of random data.
        let size = MAX_CHUNK as usize * (2 + (rng.range(3) as usize)) + (rng.range(9999) as usize);
        let blob = gen_blob(iter ^ 0x5151, size);

        let manifest = chunk_and_store(&cas, Cursor::new(&blob)).unwrap();
        assert!(
            manifest.chunks.len() > 1,
            "expected multi-chunk split for size={size}, got {} chunk(s)",
            manifest.chunks.len()
        );

        // Pick one chunk and corrupt its on-disk file by flipping a byte.
        let victim_idx = rng.range(manifest.chunks.len() as u64) as usize;
        let victim = &manifest.chunks[victim_idx];
        let path = disk_path(&cas, &victim.hash);
        let mut raw = fs::read(&path).unwrap();
        let bidx = rng.range(raw.len() as u64) as usize;
        let before = raw[bidx];
        raw[bidx] ^= 1 << (rng.range(8) as u8);
        if raw[bidx] == before {
            continue;
        }
        fs::write(&path, &raw).unwrap();
        cases += 1;

        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reassemble(&cas, &manifest)));
        match r {
            Err(_) => panic!(
                "PANIC in reassemble() after corrupting chunk {victim_idx}: iter={iter} size={size}"
            ),
            Ok(Err(_)) => {
                detected += 1;
            }
            Ok(Ok(bytes)) => {
                // Only acceptable if the corrupted chunk still decoded identically.
                assert!(
                    bytes == blob,
                    "SILENT WRONG-DATA: reassemble returned Ok with corrupted chunk! \
                     iter={iter} size={size} victim_chunk={victim_idx} got_len={}",
                    bytes.len()
                );
            }
        }
    }

    eprintln!(
        "chunker_corruption_propagates: {cases} multi-chunk blobs with one corrupted chunk, \
         {detected} surfaced as Err"
    );
    assert!(cases > 0, "no chunk corruptions were applied");
}
