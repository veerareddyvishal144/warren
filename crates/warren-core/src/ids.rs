//! Identifiers and API keys. Randomness comes from the kernel (`/dev/urandom`)
//! so this module has no dependency that could drift.

use std::io::Read;

use sha2::{Digest, Sha256};

use crate::INSTANCE_ID_LENGTH;

const LOWER_ALNUM: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
const BASE62: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// Every API key starts with this.
pub const KEY_PREFIX: &str = "sk_live_";

pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .expect("/dev/urandom must be readable");
    buf
}

fn random_string(alphabet: &[u8], n: usize) -> String {
    // Rejection sampling keeps the distribution uniform.
    let mut out = String::with_capacity(n);
    let limit = 256 - (256 % alphabet.len());
    while out.len() < n {
        for b in random_bytes(n * 2) {
            if (b as usize) < limit {
                out.push(alphabet[b as usize % alphabet.len()] as char);
                if out.len() == n {
                    break;
                }
            }
        }
    }
    out
}

/// 10 lowercase alphanumerics starting with a letter: a valid DNS label that can
/// never be mistaken for a port suffix.
pub fn new_instance_id() -> String {
    format!(
        "{}{}",
        random_string(&LOWER_ALNUM[..26], 1),
        random_string(LOWER_ALNUM, INSTANCE_ID_LENGTH - 1)
    )
}

/// 32 hex characters for backups, crons and keys.
pub fn new_id() -> String {
    hex::encode(random_bytes(16))
}

pub fn new_workspace_id() -> String {
    format!("ws_{}", random_string(LOWER_ALNUM, 20))
}

/// Mint a plaintext key, its storage hash and its display prefix.
pub fn new_api_key() -> (String, String, String) {
    let plain = format!("{KEY_PREFIX}{}", random_string(BASE62, 40));
    let hash = hash_key(&plain);
    let prefix = plain[..KEY_PREFIX.len() + 6].to_string();
    (plain, hash, prefix)
}

/// SHA-256 hex digest stored for a plaintext key.
pub fn hash_key(plain: &str) -> String {
    hex::encode(Sha256::digest(plain.as_bytes()))
}

/// Per-instance secret the guest gateway expects.
pub fn new_instance_token() -> String {
    random_string(BASE62, 40)
}

/// 32 random bytes as hex for the signed-URL HMAC key.
pub fn new_secret() -> String {
    hex::encode(random_bytes(32))
}

pub fn valid_instance_id(s: &str) -> bool {
    s.len() == INSTANCE_ID_LENGTH
        && s.bytes().enumerate().all(|(i, b)| {
            let lower = b.is_ascii_lowercase();
            let digit = b.is_ascii_digit();
            if i == 0 { lower } else { lower || digit }
        })
}

/// Template names: lowercase, digits and dashes, 3..=40 characters.
pub fn valid_template_name(s: &str) -> bool {
    (3..=40).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

/// Env variable names: `[A-Z_][A-Z0-9_]*`.
pub fn valid_env_key(s: &str) -> bool {
    let mut chars = s.bytes();
    match chars.next() {
        Some(b) if b.is_ascii_uppercase() || b == b'_' => {}
        _ => return false,
    }
    chars.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_ids_are_valid_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            let id = new_instance_id();
            assert!(valid_instance_id(&id), "{id}");
            assert!(seen.insert(id));
        }
        assert!(!valid_instance_id("0abcdefghi"));
        assert!(!valid_instance_id("abc"));
        assert!(!valid_instance_id("ABCDEFGHIJ"));
    }

    #[test]
    fn api_keys() {
        let (plain, hash, prefix) = new_api_key();
        assert_eq!(plain.len(), KEY_PREFIX.len() + 40);
        assert!(plain.starts_with(KEY_PREFIX));
        assert_eq!(hash_key(&plain), hash);
        assert_eq!(hash.len(), 64);
        assert_eq!(prefix, &plain[..KEY_PREFIX.len() + 6]);
    }

    #[test]
    fn validators() {
        assert!(valid_template_name("my-agent"));
        assert!(!valid_template_name("My_Agent"));
        assert!(!valid_template_name("ab"));
        assert!(valid_env_key("OPENAI_API_KEY"));
        assert!(!valid_env_key("1BAD"));
        assert!(!valid_env_key("lower"));
    }
}
