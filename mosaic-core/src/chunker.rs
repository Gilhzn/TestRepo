//! Content-defined chunking for large/binary blobs.
//!
//! Wraps `fastcdc` with a CAS so a single file becomes a manifest of chunk
//! hashes. Shared content across files dedupes at the chunk level.

use crate::error::Result;
use crate::hash::Hash;
use crate::storage::Cas;
use fastcdc::v2020::FastCDC;
use serde::{Deserialize, Serialize};
use std::io::Read;

pub const MIN_CHUNK: u32 = 512 * 1024;
pub const AVG_CHUNK: u32 = 2 * 1024 * 1024;
pub const MAX_CHUNK: u32 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChunkRef {
    pub hash: Hash,
    pub size: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub total_size: u64,
    pub chunks: Vec<ChunkRef>,
}

pub fn chunk_and_store<C: Cas>(cas: &C, mut reader: impl Read) -> Result<Manifest> {
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf)?;

    let mut chunks = Vec::new();
    for chunk in FastCDC::new(&buf, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK) {
        let slice = &buf[chunk.offset..chunk.offset + chunk.length];
        let hash = cas.put(slice)?;
        chunks.push(ChunkRef {
            hash,
            size: chunk.length as u32,
        });
    }

    Ok(Manifest {
        total_size: buf.len() as u64,
        chunks,
    })
}

pub fn reassemble<C: Cas>(cas: &C, manifest: &Manifest) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(manifest.total_size as usize);
    for chunk in &manifest.chunks {
        let bytes = cas.get(&chunk.hash)?;
        out.extend_from_slice(&bytes);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::FsCas;
    use std::io::Cursor;
    use tempfile::TempDir;

    fn pseudo_random(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed.wrapping_mul(0x9E3779B97F4A7C15);
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (state >> 33) as u8
            })
            .collect()
    }

    #[test]
    fn round_trip_small_blob() {
        let dir = TempDir::new().unwrap();
        let cas = FsCas::open(dir.path()).unwrap();
        let data = b"small blob fits in one chunk";

        let manifest = chunk_and_store(&cas, Cursor::new(data)).unwrap();
        assert_eq!(manifest.total_size, data.len() as u64);
        assert_eq!(reassemble(&cas, &manifest).unwrap(), data);
    }

    #[test]
    fn round_trip_multi_chunk_blob() {
        let dir = TempDir::new().unwrap();
        let cas = FsCas::open(dir.path()).unwrap();
        let data = pseudo_random(42, 50 * 1024 * 1024);

        let manifest = chunk_and_store(&cas, Cursor::new(&data)).unwrap();
        assert!(manifest.chunks.len() > 1, "expected multi-chunk split");
        assert_eq!(reassemble(&cas, &manifest).unwrap(), data);
    }

    #[test]
    fn dedup_across_files() {
        let dir = TempDir::new().unwrap();
        let cas = FsCas::open(dir.path()).unwrap();
        let shared = pseudo_random(7, 10 * 1024 * 1024);

        let mut a = shared.clone();
        a.extend_from_slice(b"-tail-A");
        let mut b = shared.clone();
        b.extend_from_slice(b"-tail-B");

        let ma = chunk_and_store(&cas, Cursor::new(&a)).unwrap();
        let mb = chunk_and_store(&cas, Cursor::new(&b)).unwrap();

        let shared_hashes: usize = ma
            .chunks
            .iter()
            .filter(|c| mb.chunks.iter().any(|d| d.hash == c.hash))
            .count();
        assert!(
            shared_hashes >= ma.chunks.len().saturating_sub(1),
            "expected most chunks shared between two near-identical blobs"
        );
    }
}
