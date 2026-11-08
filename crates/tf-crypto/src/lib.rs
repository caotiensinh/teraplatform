//! Encryption at rest and token hashing.
//!
//! Every ciphertext is AES-256-GCM under a key derived (HKDF-SHA256) from the
//! master key, a purpose ("state", "plan", "variable") and an id (workspace,
//! run). The same id is bound as associated data, so a blob copied to another
//! workspace or run fails to decrypt. Text form: `enc:v1:` + hex(nonce || ciphertext).

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::{Aead, KeyInit, OsRng, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use std::fmt;

pub const PREFIX: &str = "enc:v1:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CryptoError {
    BadKey(String),
    NotEncrypted,
    Corrupt,
}

impl fmt::Display for CryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CryptoError::BadKey(m) => write!(f, "bad master key: {m}"),
            CryptoError::NotEncrypted => f.write_str("stored data is not encrypted"),
            CryptoError::Corrupt => f.write_str("stored data could not be decrypted (wrong key, tampered, or moved)"),
        }
    }
}
impl std::error::Error for CryptoError {}

#[derive(Clone)]
pub struct Crypto {
    master: [u8; 32],
}

impl fmt::Debug for Crypto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Crypto(**redacted**)")
    }
}

impl Crypto {
    /// 64 hex characters.
    pub fn from_hex(s: &str) -> Result<Self, CryptoError> {
        let b = unhex(s.trim()).ok_or_else(|| CryptoError::BadKey("not hexadecimal".into()))?;
        let master: [u8; 32] = b.try_into().map_err(|_| CryptoError::BadKey("need 32 bytes (64 hex characters)".into()))?;
        Ok(Self { master })
    }

    /// A random key that lives only as long as the process.
    pub fn ephemeral() -> Self {
        let mut master = [0u8; 32];
        OsRng.fill_bytes(&mut master);
        Self { master }
    }

    fn cipher(&self, purpose: &str, id: &str) -> Aes256Gcm {
        let hk = Hkdf::<Sha256>::new(None, &self.master);
        let mut key = [0u8; 32];
        hk.expand(format!("{purpose}|{id}").as_bytes(), &mut key).expect("32 bytes is a valid HKDF length");
        Aes256Gcm::new_from_slice(&key).expect("32-byte key")
    }

    pub fn encrypt(&self, purpose: &str, id: &str, plaintext: &[u8]) -> String {
        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let ct = self
            .cipher(purpose, id)
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: plaintext, aad: id.as_bytes() })
            .expect("AES-GCM encryption does not fail for in-memory input");
        let mut out = nonce.to_vec();
        out.extend(ct);
        format!("{PREFIX}{}", hex(&out))
    }

    pub fn decrypt(&self, purpose: &str, id: &str, text: &str) -> Result<Vec<u8>, CryptoError> {
        let body = text.strip_prefix(PREFIX).ok_or(CryptoError::NotEncrypted)?;
        let raw = unhex(body).ok_or(CryptoError::Corrupt)?;
        if raw.len() < 12 + 16 {
            return Err(CryptoError::Corrupt);
        }
        let (nonce, ct) = raw.split_at(12);
        self.cipher(purpose, id)
            .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad: id.as_bytes() })
            .map_err(|_| CryptoError::Corrupt)
    }

    pub fn encrypt_str(&self, purpose: &str, id: &str, s: &str) -> String {
        self.encrypt(purpose, id, s.as_bytes())
    }

    pub fn decrypt_str(&self, purpose: &str, id: &str, text: &str) -> Result<String, CryptoError> {
        String::from_utf8(self.decrypt(purpose, id, text)?).map_err(|_| CryptoError::Corrupt)
    }
}

pub fn is_encrypted(text: &str) -> bool {
    text.starts_with(PREFIX)
}

pub fn sha256(data: &[u8]) -> Vec<u8> {
    Sha256::digest(data).to_vec()
}

pub fn hex(b: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push(H[(x >> 4) as usize] as char);
        s.push(H[(x & 15) as usize] as char);
    }
    s
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || !s.is_ascii() {
        return None;
    }
    s.as_bytes()
        .chunks(2)
        .map(|p| Some((nib(p[0])? << 4) | nib(p[1])?))
        .collect()
}

fn nib(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// MD5 of `data`. Only for the `Content-MD5` header the OpenTofu http backend uses to
/// detect transport corruption; never for anything security related.
pub fn md5(data: &[u8]) -> [u8; 16] {
    use md5::{Digest as _, Md5};
    Md5::digest(data).into()
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for ch in data.chunks(3) {
        let n = (ch[0] as u32) << 16 | (*ch.get(1).unwrap_or(&0) as u32) << 8 | *ch.get(2).unwrap_or(&0) as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if ch.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if ch.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Standard base64 with padding; `None` for anything malformed.
pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim().as_bytes();
    if s.is_empty() || s.len() % 4 != 0 {
        return if s.is_empty() { Some(Vec::new()) } else { None };
    }
    let val = |c: u8| B64.iter().position(|x| *x == c).map(|p| p as u32);
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for (i, q) in s.chunks(4).enumerate() {
        let last = i == s.len() / 4 - 1;
        let pad = q.iter().rev().take_while(|c| **c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for (j, c) in q.iter().enumerate() {
            n <<= 6;
            if j >= 4 - pad {
                continue;
            }
            n |= val(*c)?;
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// A fresh secret such as `tfk_<64 hex>`. Only its sha256 is ever stored.
pub fn random_token(prefix: &str) -> String {
    let mut b = [0u8; 32];
    OsRng.fill_bytes(&mut b);
    format!("{prefix}{}", hex(&b))
}

/// HMAC-SHA256 of `msg` under `key`.
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts keys of any length");
    m.update(msg);
    m.finalize().into_bytes().to_vec()
}

/// Does `claimed_hex` (a lowercase or uppercase hex string) equal the HMAC-SHA256 of `msg` under
/// `key`? Compared in constant time; anything that is not valid hex is simply `false`.
pub fn hmac_sha256_matches(key: &[u8], msg: &[u8], claimed_hex: &str) -> bool {
    match unhex(&claimed_hex.to_ascii_lowercase()) {
        Some(claimed) => ct_eq(&hmac_sha256(key, msg), &claimed),
        None => false,
    }
}

/// Constant-time comparison for secrets.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_the_rfc_4231_vectors() {
        assert_eq!(hex(&hmac_sha256(&[0x0b; 20], b"Hi There")), "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7");
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // a key longer than the block size is hashed first (vector 6)
        assert_eq!(
            hex(&hmac_sha256(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First")),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn a_claimed_hmac_is_checked_in_full_and_never_panics() {
        let good = hex(&hmac_sha256(b"secret", b"payload"));
        assert!(hmac_sha256_matches(b"secret", b"payload", &good));
        assert!(hmac_sha256_matches(b"secret", b"payload", &good.to_uppercase()), "hex case does not matter");
        assert!(!hmac_sha256_matches(b"other", b"payload", &good), "wrong key");
        assert!(!hmac_sha256_matches(b"secret", b"payload!", &good), "wrong message");
        let mut flipped = good.clone();
        flipped.replace_range(0..1, if good.starts_with('0') { "1" } else { "0" });
        assert!(!hmac_sha256_matches(b"secret", b"payload", &flipped), "one digit off");
        for bad in ["", "zz", "abc", &good[..62], &format!("{good}00"), "sha256=".to_string().as_str()] {
            assert!(!hmac_sha256_matches(b"secret", b"payload", bad), "{bad:?}");
        }
    }

    #[test]
    fn round_trip_and_format() {
        let c = Crypto::ephemeral();
        let t = c.encrypt_str("state", "ws1", "{\"secret\":\"hunter2\"}");
        assert!(t.starts_with(PREFIX));
        assert!(!t.contains("hunter2"));
        assert_eq!(c.decrypt_str("state", "ws1", &t).unwrap(), "{\"secret\":\"hunter2\"}");
    }

    #[test]
    fn nonces_differ_so_equal_plaintexts_do_not_match() {
        let c = Crypto::ephemeral();
        assert_ne!(c.encrypt_str("state", "ws1", "x"), c.encrypt_str("state", "ws1", "x"));
    }

    #[test]
    fn moved_or_wrong_purpose_or_wrong_key_fails() {
        let c = Crypto::ephemeral();
        let t = c.encrypt_str("state", "ws1", "x");
        assert_eq!(c.decrypt_str("state", "ws2", &t), Err(CryptoError::Corrupt), "other workspace");
        assert_eq!(c.decrypt_str("plan", "ws1", &t), Err(CryptoError::Corrupt), "other purpose");
        assert_eq!(Crypto::ephemeral().decrypt_str("state", "ws1", &t), Err(CryptoError::Corrupt), "other key");
    }

    #[test]
    fn tampering_is_detected() {
        let c = Crypto::ephemeral();
        let mut t = c.encrypt_str("state", "ws1", "hello world");
        let last = t.pop().unwrap();
        t.push(if last == '0' { '1' } else { '0' });
        assert_eq!(c.decrypt_str("state", "ws1", &t), Err(CryptoError::Corrupt));
        assert_eq!(c.decrypt_str("state", "ws1", "plain text"), Err(CryptoError::NotEncrypted));
        assert_eq!(c.decrypt_str("state", "ws1", "enc:v1:zz"), Err(CryptoError::Corrupt));
        assert_eq!(c.decrypt_str("state", "ws1", "enc:v1:00"), Err(CryptoError::Corrupt));
    }

    #[test]
    fn key_parsing() {
        assert!(Crypto::from_hex(&"ab".repeat(32)).is_ok());
        assert!(Crypto::from_hex("abcd").is_err());
        assert!(Crypto::from_hex(&"zz".repeat(32)).is_err());
        assert_eq!(format!("{:?}", Crypto::ephemeral()), "Crypto(**redacted**)");
    }

    #[test]
    fn same_master_reads_across_instances() {
        let k = "11".repeat(32);
        let a = Crypto::from_hex(&k).unwrap();
        let b = Crypto::from_hex(&k).unwrap();
        assert_eq!(b.decrypt_str("state", "w", &a.encrypt_str("state", "w", "v")).unwrap(), "v");
    }

    #[test]
    fn base64_matches_the_standard_vectors() {
        for (plain, enc) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64_encode(plain.as_bytes()), enc);
            assert_eq!(base64_decode(enc).unwrap(), plain.as_bytes());
        }
        assert!(base64_decode("Zg=").is_none() && base64_decode("Z===").is_none() && base64_decode("Zm9v!A==").is_none() && base64_decode("Zg==Zm9v").is_none());
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(base64_decode(&base64_encode(&all)).unwrap(), all);
    }

    #[test]
    fn md5_known_values_and_the_header_form_the_backend_sends() {
        assert_eq!(hex(&md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(&md5(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(base64_encode(&md5(b"abc")), "kAFQmDzST7DWlj99KOF/cg==");
    }

    #[test]
    fn hex_and_tokens() {
        assert_eq!(hex(&[0, 255, 16]), "00ff10");
        assert_eq!(unhex("00ff10").unwrap(), vec![0, 255, 16]);
        assert!(unhex("0").is_none() && unhex("gg").is_none());
        let t = random_token("tfk_");
        assert!(t.starts_with("tfk_") && t.len() == 4 + 64);
        assert_ne!(t, random_token("tfk_"));
        assert!(ct_eq(b"abc", b"abc") && !ct_eq(b"abc", b"abd") && !ct_eq(b"a", b"ab"));
    }
}
