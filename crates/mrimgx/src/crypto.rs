//! Key derivation and AES as used by Reflect X (see `docs/ENCRYPTION.md` and
//! `src/libs/encryption/encryption.cpp` of the Macrium sample code).
//!
//! * derived key = PBKDF2-HMAC-SHA256(password UTF-8, salt = SHA-256(image id, 8 bytes), iterations, 32 bytes)
//! * password check: HMAC-SHA256 keyed with the derived key over an empty message equals `_encryption.hmac`
//! * metadata blocks: AES-ECB, data blocks: AES-CBC, both without padding; AES-128/192 use the
//!   first 16/24 bytes of the derived key (OpenSSL is handed the 32-byte buffer)
//! * data block IV = AES-256-ECB(key = SHA-256(derived key)) of
//!   `image id[8] | disk number u16 LE | partition number u16 LE | block index u32 LE`

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::{Aes128, Aes192, Aes256};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// Length of the PBKDF2 derived key.
pub(crate) const KEY_LEN: usize = 32;

enum Cipher {
    Aes128(Aes128),
    Aes192(Aes192),
    Aes256(Aes256),
}

/// Runs `$body` with `$c` bound to the concrete AES cipher.
macro_rules! with_cipher {
    ($self:expr, $c:ident => $body:expr) => {
        match &$self.cipher {
            Cipher::Aes128($c) => $body,
            Cipher::Aes192($c) => $body,
            Cipher::Aes256($c) => $body,
        }
    };
}

/// Unlocked cipher state of an encrypted image.
pub(crate) struct Crypto {
    cipher: Cipher,
    iv_cipher: Aes256,
    image_id: [u8; 8],
}

impl Crypto {
    /// Derives the 32-byte key from the password.
    pub fn derive_key(password: &str, image_id: &[u8; 8], iterations: u32) -> [u8; KEY_LEN] {
        let salt = Sha256::digest(image_id);
        let mut key = [0; KEY_LEN];
        pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, iterations, &mut key);
        key
    }

    /// HMAC-SHA256 of an empty message keyed with the derived key (stored hex-encoded in the JSON).
    #[cfg(test)]
    pub fn key_hmac(key: &[u8; KEY_LEN]) -> [u8; 32] {
        let mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
        mac.finalize().into_bytes().into()
    }

    /// Compares the derived key against the hex HMAC stored in the image.
    pub fn check_hmac(key: &[u8; KEY_LEN], hmac_hex: &str) -> Result<bool> {
        let expected =
            decode_hex(hmac_hex).filter(|h| h.len() == 32).ok_or_else(|| Error::corrupt("invalid encryption HMAC"))?;
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
        mac.update(&[]);
        Ok(mac.verify_slice(&expected).is_ok())
    }

    /// Creates the cipher for `aes_type` (`aes-128`, `aes-192` or `aes-256`).
    pub fn new(aes_type: &str, key: &[u8; KEY_LEN], image_id: [u8; 8]) -> Result<Self> {
        let cipher = match aes_type {
            "aes-128" => Cipher::Aes128(Aes128::new(GenericArray::from_slice(&key[..16]))),
            "aes-192" => Cipher::Aes192(Aes192::new(GenericArray::from_slice(&key[..24]))),
            "aes-256" => Cipher::Aes256(Aes256::new(GenericArray::from_slice(key))),
            other => return Err(Error::Unsupported(format!("encryption type {other:?}"))),
        };
        let iv_cipher = Aes256::new(&Sha256::digest(key));
        Ok(Self { cipher, iv_cipher, image_id })
    }

    /// IV of a data block.
    pub fn iv(&self, disk_number: u16, partition_number: u16, block_index: u32) -> [u8; 16] {
        let mut iv = [0; 16];
        iv[..8].copy_from_slice(&self.image_id);
        iv[8..10].copy_from_slice(&disk_number.to_le_bytes());
        iv[10..12].copy_from_slice(&partition_number.to_le_bytes());
        iv[12..].copy_from_slice(&block_index.to_le_bytes());
        self.iv_cipher.encrypt_block(GenericArray::from_mut_slice(&mut iv));
        iv
    }

    /// Decrypts a metadata block in place (AES-ECB, no padding).
    pub fn decrypt_ecb(&self, buf: &mut [u8]) -> Result<()> {
        check_len(buf)?;
        with_cipher!(self, c => buf.chunks_exact_mut(16).for_each(|b| c.decrypt_block(GenericArray::from_mut_slice(b))));
        Ok(())
    }

    /// Decrypts a data block in place (AES-CBC, no padding).
    pub fn decrypt_cbc(&self, iv: &[u8; 16], buf: &mut [u8]) -> Result<()> {
        check_len(buf)?;
        with_cipher!(self, c => {
            let mut prev = *iv;
            for b in buf.chunks_exact_mut(16) {
                let mut cur = [0; 16];
                cur.copy_from_slice(b);
                c.decrypt_block(GenericArray::from_mut_slice(b));
                b.iter_mut().zip(prev).for_each(|(x, p)| *x ^= p);
                prev = cur;
            }
        });
        Ok(())
    }

    /// Encrypts in place with AES-ECB (test image writer).
    #[cfg(test)]
    pub fn encrypt_ecb(&self, buf: &mut [u8]) {
        with_cipher!(self, c => buf.chunks_exact_mut(16).for_each(|b| c.encrypt_block(GenericArray::from_mut_slice(b))));
    }

    /// Encrypts in place with AES-CBC (test image writer).
    #[cfg(test)]
    pub fn encrypt_cbc(&self, iv: &[u8; 16], buf: &mut [u8]) {
        with_cipher!(self, c => {
            let mut prev = *iv;
            for b in buf.chunks_exact_mut(16) {
                b.iter_mut().zip(prev).for_each(|(x, p)| *x ^= p);
                c.encrypt_block(GenericArray::from_mut_slice(b));
                prev.copy_from_slice(b);
            }
        });
    }
}

fn check_len(buf: &[u8]) -> Result<()> {
    if buf.len() % 16 == 0 {
        Ok(())
    } else {
        Err(Error::corrupt(format!("encrypted block length {} is not a multiple of 16", buf.len())))
    }
}

/// Decodes a hex string; `None` if it is malformed.
pub(crate) fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}
