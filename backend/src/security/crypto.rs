//! At-rest encryption with key rotation: AES-256-GCM (authenticated) with a key ring.
//!
//! Ciphertext layout: `v1 (1 byte) | key fingerprint (8 bytes) | nonce (12) | ciphertext+tag`.
//! The first configured key encrypts; any configured key decrypts (selected by fingerprint).
//! Rotation:
//!   1. prepend a new key to `APP_SECURITY__ENCRYPTION_KEYS` and restart
//!   2. `POST /admin/keys/rotate` re-encrypts every row with the new primary key
//!   3. drop the old key from the list

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use sha2::{Digest, Sha256};

const VERSION: u8 = 1;
const FP_LEN: usize = 8;
const NONCE_LEN: usize = 12;

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("invalid key: {0}")]
    InvalidKey(String),
    #[error("ciphertext could not be decrypted with any known key")]
    Decrypt,
}

struct KeyEntry {
    fingerprint: [u8; FP_LEN],
    cipher: Aes256Gcm,
}

pub struct KeyRing {
    keys: Vec<KeyEntry>,
}

impl std::fmt::Debug for KeyRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KeyRing({} keys, [REDACTED])", self.keys.len())
    }
}

fn fingerprint(raw: &[u8]) -> [u8; FP_LEN] {
    let digest = Sha256::digest(raw);
    let mut fp = [0u8; FP_LEN];
    fp.copy_from_slice(&digest[..FP_LEN]);
    fp
}

impl KeyRing {
    pub fn new(keys: &[String]) -> Result<Self, CryptoError> {
        if keys.is_empty() {
            return Err(CryptoError::InvalidKey(
                "at least one key is required".into(),
            ));
        }
        let keys = keys
            .iter()
            .map(|k| {
                let raw = B64
                    .decode(k.trim())
                    .map_err(|e| CryptoError::InvalidKey(e.to_string()))?;
                if raw.len() != 32 {
                    return Err(CryptoError::InvalidKey(
                        "key must be 32 bytes (base64)".into(),
                    ));
                }
                Ok(KeyEntry {
                    fingerprint: fingerprint(&raw),
                    cipher: Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&raw)),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { keys })
    }

    pub fn key_count(&self) -> usize {
        self.keys.len()
    }

    pub fn encrypt(&self, plaintext: &[u8]) -> Vec<u8> {
        let primary = &self.keys[0];
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ct = primary
            .cipher
            .encrypt(&nonce, plaintext)
            .expect("AES-GCM encryption cannot fail");
        let mut out = Vec::with_capacity(1 + FP_LEN + NONCE_LEN + ct.len());
        out.push(VERSION);
        out.extend_from_slice(&primary.fingerprint);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        out
    }

    pub fn decrypt(&self, blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if blob.len() < 1 + FP_LEN + NONCE_LEN + 16 || blob[0] != VERSION {
            return Err(CryptoError::Decrypt);
        }
        let fp = &blob[1..1 + FP_LEN];
        let nonce = Nonce::from_slice(&blob[1 + FP_LEN..1 + FP_LEN + NONCE_LEN]);
        let ct = &blob[1 + FP_LEN + NONCE_LEN..];
        let key = self
            .keys
            .iter()
            .find(|k| k.fingerprint == fp)
            .ok_or(CryptoError::Decrypt)?;
        key.cipher
            .decrypt(nonce, ct)
            .map_err(|_| CryptoError::Decrypt)
    }

    /// Re-encrypt with the current primary key.
    pub fn rotate(&self, blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
        Ok(self.encrypt(&self.decrypt(blob)?))
    }
}

pub fn generate_key() -> String {
    let key = Aes256Gcm::generate_key(&mut OsRng);
    B64.encode(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_rotation() {
        let (old, new) = (generate_key(), generate_key());
        let blob = KeyRing::new(std::slice::from_ref(&old))
            .unwrap()
            .encrypt(b"phi");
        let both = KeyRing::new(&[new.clone(), old]).unwrap();
        assert_eq!(both.decrypt(&blob).unwrap(), b"phi"); // old data still readable
        let rotated = both.rotate(&blob).unwrap();
        assert_eq!(
            KeyRing::new(&[new]).unwrap().decrypt(&rotated).unwrap(),
            b"phi"
        ); // old key retirable
        assert!(
            KeyRing::new(&[generate_key()])
                .unwrap()
                .decrypt(&blob)
                .is_err()
        );
    }

    #[test]
    fn tampering_is_detected() {
        let ring = KeyRing::new(&[generate_key()]).unwrap();
        let mut blob = ring.encrypt(b"phi");
        let last = blob.len() - 1;
        blob[last] ^= 1;
        assert!(ring.decrypt(&blob).is_err());
    }

    #[test]
    fn ciphertext_does_not_contain_plaintext_and_is_randomised() {
        let ring = KeyRing::new(&[generate_key()]).unwrap();
        let (a, b) = (
            ring.encrypt(b"Observation 39.1"),
            ring.encrypt(b"Observation 39.1"),
        );
        assert_ne!(a, b);
        assert!(!a.windows(4).any(|w| w == b"39.1"));
    }

    #[test]
    fn rejects_bad_keys() {
        assert!(KeyRing::new(&[]).is_err());
        assert!(KeyRing::new(&["short".into()]).is_err());
        assert!(
            KeyRing::new(&[generate_key()])
                .unwrap()
                .decrypt(b"junk")
                .is_err()
        );
    }
}
