use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fmt;

pub const HASH_LEN: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Hash([u8; HASH_LEN]);

impl Hash {
    pub fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    pub fn from_bytes(bytes: [u8; HASH_LEN]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; HASH_LEN] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(s: &str) -> Result<Self> {
        let bytes = hex::decode(s)?;
        if bytes.len() != HASH_LEN {
            return Err(Error::InvalidHashLength(bytes.len()));
        }
        let mut arr = [0u8; HASH_LEN];
        arr.copy_from_slice(&bytes);
        Ok(Self(arr))
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", &self.to_hex()[..16])
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

pub struct Hasher(blake3::Hasher);

impl Hasher {
    pub fn new() -> Self {
        Self(blake3::Hasher::new())
    }

    pub fn update(&mut self, bytes: &[u8]) -> &mut Self {
        self.0.update(bytes);
        self
    }

    pub fn finalize(&self) -> Hash {
        Hash(*self.0.finalize().as_bytes())
    }
}

impl Default for Hasher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_hex() {
        let h = Hash::of(b"hello mosaic");
        let s = h.to_hex();
        let back = Hash::from_hex(&s).unwrap();
        assert_eq!(h, back);
    }

    #[test]
    fn streaming_matches_oneshot() {
        let data = b"the quick brown fox jumps over the lazy dog";
        let oneshot = Hash::of(data);
        let mut hasher = Hasher::new();
        hasher.update(&data[..10]);
        hasher.update(&data[10..]);
        assert_eq!(oneshot, hasher.finalize());
    }

    #[test]
    fn from_hex_rejects_wrong_length() {
        assert!(matches!(
            Hash::from_hex("dead"),
            Err(Error::InvalidHashLength(2))
        ));
    }
}
