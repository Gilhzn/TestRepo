use crate::error::{Error, Result};
use ed25519_dalek::{Signer, Verifier};
use rand::rngs::OsRng;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

pub const SIGNING_KEY_LEN: usize = 32;
pub const VERIFYING_KEY_LEN: usize = 32;
pub const SIGNATURE_LEN: usize = 64;

#[derive(Clone)]
pub struct SigningKey(ed25519_dalek::SigningKey);

impl SigningKey {
    pub fn generate() -> Self {
        Self(ed25519_dalek::SigningKey::generate(&mut OsRng))
    }

    pub fn from_bytes(bytes: &[u8; SIGNING_KEY_LEN]) -> Self {
        Self(ed25519_dalek::SigningKey::from_bytes(bytes))
    }

    pub fn to_bytes(&self) -> [u8; SIGNING_KEY_LEN] {
        self.0.to_bytes()
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        VerifyingKey(self.0.verifying_key())
    }

    pub fn sign(&self, msg: &[u8]) -> Signature {
        Signature(self.0.sign(msg))
    }
}

impl fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SigningKey(<redacted>)")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct VerifyingKey(ed25519_dalek::VerifyingKey);

impl VerifyingKey {
    pub fn from_bytes(bytes: &[u8; VERIFYING_KEY_LEN]) -> Result<Self> {
        ed25519_dalek::VerifyingKey::from_bytes(bytes)
            .map(Self)
            .map_err(|_| Error::BadSignature)
    }

    pub fn to_bytes(&self) -> [u8; VERIFYING_KEY_LEN] {
        self.0.to_bytes()
    }

    pub fn verify(&self, msg: &[u8], sig: &Signature) -> Result<()> {
        self.0.verify(msg, &sig.0).map_err(|_| Error::BadSignature)
    }
}

impl fmt::Debug for VerifyingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VerifyingKey({})", hex::encode(self.to_bytes()))
    }
}

impl Serialize for VerifyingKey {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.serialize_str(&hex::encode(self.to_bytes()))
        } else {
            s.serialize_bytes(&self.to_bytes())
        }
    }
}

impl<'de> Deserialize<'de> for VerifyingKey {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        use serde::de::Error as DeError;
        if d.is_human_readable() {
            let s = String::deserialize(d)?;
            let raw = hex::decode(&s).map_err(DeError::custom)?;
            if raw.len() != VERIFYING_KEY_LEN {
                return Err(DeError::custom("invalid verifying key length"));
            }
            let mut arr = [0u8; VERIFYING_KEY_LEN];
            arr.copy_from_slice(&raw);
            Self::from_bytes(&arr).map_err(DeError::custom)
        } else {
            let raw = <Vec<u8>>::deserialize(d)?;
            if raw.len() != VERIFYING_KEY_LEN {
                return Err(DeError::custom("invalid verifying key length"));
            }
            let mut arr = [0u8; VERIFYING_KEY_LEN];
            arr.copy_from_slice(&raw);
            Self::from_bytes(&arr).map_err(DeError::custom)
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Signature(ed25519_dalek::Signature);

impl Signature {
    pub fn from_bytes(bytes: &[u8; SIGNATURE_LEN]) -> Self {
        Self(ed25519_dalek::Signature::from_bytes(bytes))
    }

    pub fn to_bytes(&self) -> [u8; SIGNATURE_LEN] {
        self.0.to_bytes()
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Signature({})", hex::encode(self.to_bytes()))
    }
}

impl Serialize for Signature {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.serialize_str(&hex::encode(self.to_bytes()))
        } else {
            s.serialize_bytes(&self.to_bytes())
        }
    }
}

impl<'de> Deserialize<'de> for Signature {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        use serde::de::Error as DeError;
        if d.is_human_readable() {
            let s = String::deserialize(d)?;
            let raw = hex::decode(&s).map_err(DeError::custom)?;
            if raw.len() != SIGNATURE_LEN {
                return Err(DeError::custom("invalid signature length"));
            }
            let mut arr = [0u8; SIGNATURE_LEN];
            arr.copy_from_slice(&raw);
            Ok(Self::from_bytes(&arr))
        } else {
            let raw = <Vec<u8>>::deserialize(d)?;
            if raw.len() != SIGNATURE_LEN {
                return Err(DeError::custom("invalid signature length"));
            }
            let mut arr = [0u8; SIGNATURE_LEN];
            arr.copy_from_slice(&raw);
            Ok(Self::from_bytes(&arr))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_verify_round_trip() {
        let sk = SigningKey::generate();
        let vk = sk.verifying_key();
        let msg = b"mosaic change canonical bytes";
        let sig = sk.sign(msg);
        vk.verify(msg, &sig).unwrap();
    }

    #[test]
    fn verify_rejects_tampered_message() {
        let sk = SigningKey::generate();
        let vk = sk.verifying_key();
        let sig = sk.sign(b"original");
        assert!(matches!(vk.verify(b"tampered", &sig), Err(Error::BadSignature)));
    }

    #[test]
    fn verify_rejects_wrong_key() {
        let sk1 = SigningKey::generate();
        let sk2 = SigningKey::generate();
        let msg = b"hello";
        let sig = sk1.sign(msg);
        assert!(matches!(
            sk2.verifying_key().verify(msg, &sig),
            Err(Error::BadSignature)
        ));
    }

    #[test]
    fn key_byte_round_trip() {
        let sk = SigningKey::generate();
        let sk2 = SigningKey::from_bytes(&sk.to_bytes());
        assert_eq!(sk.to_bytes(), sk2.to_bytes());

        let vk = sk.verifying_key();
        let vk2 = VerifyingKey::from_bytes(&vk.to_bytes()).unwrap();
        assert_eq!(vk, vk2);
    }

    #[test]
    fn signature_serde_round_trip() {
        let sk = SigningKey::generate();
        let sig = sk.sign(b"x");
        let bytes = bincode::serialize(&sig).unwrap();
        let back: Signature = bincode::deserialize(&bytes).unwrap();
        assert_eq!(sig, back);
    }

    #[test]
    fn verifying_key_serde_round_trip() {
        let sk = SigningKey::generate();
        let vk = sk.verifying_key();
        let bytes = bincode::serialize(&vk).unwrap();
        let back: VerifyingKey = bincode::deserialize(&bytes).unwrap();
        assert_eq!(vk, back);
    }
}
