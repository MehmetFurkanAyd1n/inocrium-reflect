use md5::{Digest, Md5};
use rand::{distributions::Uniform, Rng};

/// One-way CHAP identity and Microsoft-compatible secret.
#[derive(Clone)]
pub struct ChapSecret {
    /// Initiator authentication name.
    pub user: String,
    /// Secret bytes, between twelve and sixteen printable ASCII characters.
    pub secret: Vec<u8>,
}

/// Generate sixteen random printable ASCII characters using the system-seeded RNG.
pub fn generate_chap_secret() -> ChapSecret {
    ChapSecret {
        user: "inocrium".into(),
        secret: rand::thread_rng().sample_iter(Uniform::new_inclusive(33u8, 126)).take(16).collect(),
    }
}

pub(crate) fn response(id: u8, secret: &[u8], challenge: &[u8]) -> [u8; 16] {
    let mut digest = Md5::new();
    digest.update([id]);
    digest.update(secret);
    digest.update(challenge);
    digest.finalize().into()
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut text = String::from("0x");
    for byte in bytes {
        use std::fmt::Write;
        write!(text, "{byte:02x}").expect("writing to String");
    }
    text
}

pub(crate) fn unhex(text: &str) -> Option<Vec<u8>> {
    let text = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X"))?;
    if text.len() % 2 != 0 || !text.is_ascii() {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok()).collect()
}

pub(crate) fn verify(received: &[u8], expected: &[u8; 16]) -> bool {
    received.len() == 16 && received.iter().zip(expected).fold(0, |a, (b, c)| a | (b ^ c)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn md5_vector_and_hex() {
        // RFC 1994's digest input is identifier || secret || challenge.
        assert_eq!(hex(&response(1, b"secret", b"challenge")), "0x46e47770030e2b4e26a0db72a7efec00");
        assert_eq!(unhex("0x01Ff"), Some(vec![1, 255]));
        assert!(unhex("0xz1").is_none());
        assert!(unhex("0x1").is_none());
        let secret = generate_chap_secret();
        assert_eq!(secret.secret.len(), 16);
        assert!(secret.secret.iter().all(|b| (33..=126).contains(b)));
    }
}
