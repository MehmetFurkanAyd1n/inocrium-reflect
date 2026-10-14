//! Key derivation, key wrapping and block sealing.
//!
//! A random 256-bit data encryption key (DEK) encrypts every block and the metadata. The DEK
//! is stored in each file header wrapped (AES-256-GCM) with a key encryption key (KEK) that is
//! derived from the password with Argon2id, so changing the password only rewrites headers.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

pub const KEY_LEN: usize = 32;
pub const TAG_LEN: usize = 16;
pub const SALT_LEN: usize = 32;
pub const WRAP_NONCE_LEN: usize = 12;
pub const WRAPPED_KEY_LEN: usize = KEY_LEN + TAG_LEN;

/// Nonce domains keep data, metadata and checkpoint nonces from ever colliding.
pub const DOMAIN_DATA: u32 = 0;
pub const DOMAIN_METADATA: u32 = 1;

/// Argon2id parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfParams {
    /// Memory cost in KiB.
    pub memory_kib: u32,
    /// Number of passes.
    pub iterations: u32,
    /// Degree of parallelism.
    pub parallelism: u32,
}

impl Default for KdfParams {
    fn default() -> Self {
        Self { memory_kib: 64 * 1024, iterations: 3, parallelism: 4 }
    }
}

impl KdfParams {
    /// Cheap parameters for tests.
    pub fn insecure_fast() -> Self {
        Self { memory_kib: 64, iterations: 1, parallelism: 1 }
    }

    pub fn derive(&self, password: &str, salt: &[u8]) -> Result<[u8; KEY_LEN]> {
        let params = Params::new(self.memory_kib, self.iterations, self.parallelism, Some(KEY_LEN))
            .map_err(|e| Error::InvalidArgument(format!("KDF parameters: {e}")))?;
        let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let mut out = [0u8; KEY_LEN];
        argon
            .hash_password_into(password.as_bytes(), salt, &mut out)
            .map_err(|e| Error::InvalidArgument(format!("KDF: {e}")))?;
        Ok(out)
    }
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::getrandom(&mut b).expect("system RNG unavailable");
    b
}

/// Wraps `dek` with `kek`; returns the sealed key (ciphertext + tag).
pub fn wrap_key(
    kek: &[u8; KEY_LEN],
    nonce: &[u8; WRAP_NONCE_LEN],
    dek: &[u8; KEY_LEN],
    aad: &[u8],
) -> [u8; WRAPPED_KEY_LEN] {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(kek));
    let sealed = cipher
        .encrypt(Nonce::from_slice(nonce), Payload { msg: dek, aad })
        .expect("AES-GCM encryption cannot fail for small inputs");
    let mut out = [0u8; WRAPPED_KEY_LEN];
    out.copy_from_slice(&sealed);
    out
}

pub fn unwrap_key(
    kek: &[u8; KEY_LEN],
    nonce: &[u8; WRAP_NONCE_LEN],
    wrapped: &[u8; WRAPPED_KEY_LEN],
    aad: &[u8],
) -> Result<[u8; KEY_LEN]> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(kek));
    let plain =
        cipher.decrypt(Nonce::from_slice(nonce), Payload { msg: wrapped, aad }).map_err(|_| Error::WrongPassword)?;
    let mut out = [0u8; KEY_LEN];
    out.copy_from_slice(&plain);
    Ok(out)
}

/// Block cipher keyed with the image DEK.
#[derive(Clone)]
pub struct Sealer {
    cipher: Aes256Gcm,
}

impl std::fmt::Debug for Sealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sealer(..)")
    }
}

impl Sealer {
    pub fn new(dek: &[u8; KEY_LEN]) -> Self {
        Self { cipher: Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(dek)) }
    }

    pub fn nonce(domain: u32, counter: u64) -> [u8; 12] {
        let mut n = [0u8; 12];
        n[..4].copy_from_slice(&domain.to_le_bytes());
        n[4..].copy_from_slice(&counter.to_le_bytes());
        n
    }

    pub fn seal(&self, domain: u32, counter: u64, plain: &[u8], aad: &[u8]) -> Vec<u8> {
        let nonce = Self::nonce(domain, counter);
        self.cipher.encrypt(Nonce::from_slice(&nonce), Payload { msg: plain, aad }).expect("AES-GCM encryption failed")
    }

    pub fn open(&self, domain: u32, counter: u64, sealed: &[u8], aad: &[u8]) -> Option<Vec<u8>> {
        let nonce = Self::nonce(domain, counter);
        self.cipher.decrypt(Nonce::from_slice(&nonce), Payload { msg: sealed, aad }).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_roundtrip_and_wrong_password() {
        let kdf = KdfParams::insecure_fast();
        let salt = random_bytes::<SALT_LEN>();
        let kek = kdf.derive("secret", &salt).unwrap();
        let dek = random_bytes::<KEY_LEN>();
        let nonce = random_bytes::<WRAP_NONCE_LEN>();
        let wrapped = wrap_key(&kek, &nonce, &dek, b"aad");
        assert_eq!(unwrap_key(&kek, &nonce, &wrapped, b"aad").unwrap(), dek);
        let bad = kdf.derive("Secret", &salt).unwrap();
        assert!(matches!(unwrap_key(&bad, &nonce, &wrapped, b"aad"), Err(Error::WrongPassword)));
    }
}
