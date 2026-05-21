//! Tiny zero-dep benchmark harness.
//!
//! Spits out reproducible throughput numbers for the operations that
//! a real-team deployment depends on:
//!
//!   - CAS put + get          (BLAKE3 + zstd)
//!   - Three-way merge        (line-graph commutation)
//!   - Bundle build           (DAG walk + serialize + CAS lookups)
//!   - Bundle apply           (sig verify + DAG register)
//!   - FastCDC chunking       (large-blob dedup substrate)
//!
//! Run with: `cargo run --release --bench bench -p mosaic-core`
//! Tests don't build this; it's a binary harness.

use std::time::Instant;

use mosaic_core::chunker::{chunk_and_store, AVG_CHUNK};
use mosaic_core::hash::Hash;
use mosaic_core::m1::change::{ChangeBuilder, ChangeId, FileChange, FileKind};
use mosaic_core::m1::identity::Identity;
use mosaic_core::m1::signing::SigningKey;
use mosaic_core::m1_patch::line_graph::LineGraph;
use mosaic_core::m1_patch::merge::three_way_merge;
use mosaic_core::m1_patch::patch::{Op, Patch};
use mosaic_core::repo::Repository;
use mosaic_core::storage::{Cas, FsCas};
use mosaic_core::sync::{apply_bundle, build_bundle};
use std::io::Cursor;

struct Stat {
    label: &'static str,
    items: f64,
    item_unit: &'static str,
    seconds: f64,
}

impl Stat {
    fn print(&self) {
        let rate = self.items / self.seconds.max(1e-9);
        println!(
            "{:<48} {:>12.2} {} in {:>7.3} s  →  {:>12.2} {}/s",
            self.label, self.items, self.item_unit, self.seconds, rate, self.item_unit,
        );
    }
}

fn time<F: FnOnce()>(f: F) -> f64 {
    let t = Instant::now();
    f();
    t.elapsed().as_secs_f64()
}

fn pseudo(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E3779B97F4A7C15);
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

fn bench_cas_put_get() {
    let dir = tempfile::TempDir::new().unwrap();
    let cas = FsCas::open(dir.path()).unwrap();
    const N: usize = 500;
    const SIZE: usize = 4 * 1024; // 4 KB blobs

    let blobs: Vec<Vec<u8>> = (0..N).map(|i| pseudo(i as u64 + 1, SIZE)).collect();

    let put_secs = time(|| {
        for b in &blobs {
            cas.put(b).unwrap();
        }
    });
    Stat {
        label: "CAS put (4 KB blobs, fresh)",
        items: N as f64,
        item_unit: "ops",
        seconds: put_secs,
    }
    .print();
    Stat {
        label: "CAS put (4 KB blobs, fresh) — throughput",
        items: (N as f64 * SIZE as f64) / 1_048_576.0,
        item_unit: "MiB",
        seconds: put_secs,
    }
    .print();

    let hashes: Vec<Hash> = blobs.iter().map(|b| Hash::of(b)).collect();
    let get_secs = time(|| {
        for h in &hashes {
            let _ = cas.get(h).unwrap();
        }
    });
    Stat {
        label: "CAS get (4 KB blobs, verified)",
        items: N as f64,
        item_unit: "ops",
        seconds: get_secs,
    }
    .print();
}

fn bench_chunker() {
    let dir = tempfile::TempDir::new().unwrap();
    let cas = FsCas::open(dir.path()).unwrap();
    const SIZE_MB: usize = 64;
    let blob = pseudo(42, SIZE_MB * 1024 * 1024);

    let manifest = chunk_and_store(&cas, Cursor::new(&blob)).unwrap();
    let _ = AVG_CHUNK;

    let secs = time(|| {
        let _ = chunk_and_store(&cas, Cursor::new(&blob)).unwrap();
    });
    Stat {
        label: "FastCDC chunk-and-store 64 MiB",
        items: SIZE_MB as f64,
        item_unit: "MiB",
        seconds: secs,
    }
    .print();
    println!(
        "    → {} chunks (avg target = {} bytes)",
        manifest.chunks.len(),
        AVG_CHUNK
    );
}

fn bench_three_way_merge() {
    const N: usize = 200;
    let lines: Vec<Vec<u8>> = (0..N).map(|i| format!("line_{i:05}").into_bytes()).collect();
    let refs: Vec<&[u8]> = lines.iter().map(|v| v.as_slice()).collect();
    let creator = Hash::of(b"bench-merge");
    let base = LineGraph::from_lines(&creator, &refs);

    let anchor_blank = mosaic_core::m1_patch::line_graph::VertexId::derive(&creator, 50, refs[50]);
    let anchor_blank2 = mosaic_core::m1_patch::line_graph::VertexId::derive(&creator, 150, refs[150]);
    let next_after_50 = mosaic_core::m1_patch::line_graph::VertexId::derive(&creator, 51, refs[51]);
    let next_after_150 = mosaic_core::m1_patch::line_graph::VertexId::derive(&creator, 151, refs[151]);

    let alice = make_insert(b"alice-vertex", b"// alice", anchor_blank, next_after_50);
    let bob = make_insert(b"bob-vertex", b"// bob", anchor_blank2, next_after_150);

    const ROUNDS: usize = 1000;
    let secs = time(|| {
        for _ in 0..ROUNDS {
            let _r = three_way_merge(&base, &alice, &bob).unwrap();
        }
    });
    Stat {
        label: "three_way_merge on 200-line file (disjoint inserts)",
        items: ROUNDS as f64,
        item_unit: "merges",
        seconds: secs,
    }
    .print();
}

fn make_insert(
    tag: &[u8],
    line: &[u8],
    anchor: mosaic_core::m1_patch::line_graph::VertexId,
    before: mosaic_core::m1_patch::line_graph::VertexId,
) -> Patch {
    let creator = Hash::of(tag);
    let vertex = mosaic_core::m1_patch::line_graph::Vertex {
        id: mosaic_core::m1_patch::line_graph::VertexId::derive(&creator, 0, line),
        bytes: line.to_vec(),
        alive: true,
    };
    Patch::from_ops(vec![Op::InsertAfter {
        anchor,
        before,
        vertex,
    }])
}

fn bench_bundle_round_trip() {
    let src = tempfile::TempDir::new().unwrap();
    let dst = tempfile::TempDir::new().unwrap();
    let mut repo = Repository::init(src.path()).unwrap();
    let idn = Identity::human("bench@example.com", None).unwrap();
    let key = SigningKey::generate();

    const N: usize = 100;
    let mut last: Option<ChangeId> = None;
    let build_secs = time(|| {
        for i in 0..N {
            let mut b = ChangeBuilder::new(idn.clone(), key.clone())
                .intent(format!("change-{i}"))
                .file(FileChange {
                    path: format!("file_{i}.txt"),
                    kind: FileKind::Text,
                    patch: pseudo(i as u64 + 100, 256),
                    conflicts: Vec::new(),
                });
            if let Some(p) = last {
                b = b.dep(p);
            }
            let id = repo.commit(b.build().unwrap()).unwrap();
            repo.advance_branch("main", id).unwrap();
            last = Some(id);
        }
    });
    Stat {
        label: "commit 100 changes (chain) into local repo",
        items: N as f64,
        item_unit: "changes",
        seconds: build_secs,
    }
    .print();

    let all_ids: Vec<ChangeId> = repo
        .all_change_ids()
        .unwrap()
        .into_iter()
        .map(ChangeId)
        .collect();
    let mut bundle = None;
    let secs = time(|| {
        bundle = Some(build_bundle(&repo, &all_ids).unwrap());
    });
    Stat {
        label: "build_bundle of 100 changes",
        items: N as f64,
        item_unit: "changes",
        seconds: secs,
    }
    .print();

    let bundle = bundle.unwrap();
    let mut dst_repo = Repository::init(dst.path()).unwrap();
    let secs = time(|| {
        apply_bundle(&mut dst_repo, &bundle).unwrap();
    });
    Stat {
        label: "apply_bundle of 100 changes (signatures verified)",
        items: N as f64,
        item_unit: "changes",
        seconds: secs,
    }
    .print();
}

fn main() {
    println!("=== Mosaic benchmark harness ===");
    println!("Each run is a single sample on the current machine; numbers");
    println!("are rough but stable enough for relative comparisons.");
    println!();
    bench_cas_put_get();
    println!();
    bench_chunker();
    println!();
    bench_three_way_merge();
    println!();
    bench_bundle_round_trip();
    println!();
    println!("done.");
}
