//! Hashing the core needs, behind a trait so each host can supply its own
//! implementation (the `rust-digest` feature brings `sha2` / `hmac`).

/// 32-byte output of SHA-256 / HMAC-SHA256.
pub type Hash32 = [u8; 32];

/// A host's hashing primitive failed (an FFI callback error, never a
/// logic error of the core).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("digest backend failed: {0}")]
pub struct DigestError(pub String);

pub type DigestResult = Result<Hash32, DigestError>;

/// The two primitives every logical ID and integrity check is built from.
pub trait Digest {
    fn sha256(&self, data: &[u8]) -> DigestResult;
    fn hmac_sha256(&self, key: &[u8], data: &[u8]) -> DigestResult;
}

impl<T: Digest + ?Sized> Digest for &T {
    fn sha256(&self, data: &[u8]) -> DigestResult {
        (**self).sha256(data)
    }
    fn hmac_sha256(&self, key: &[u8], data: &[u8]) -> DigestResult {
        (**self).hmac_sha256(key, data)
    }
}

/// `Digest` implemented with the `sha2` and `hmac` crates.
#[cfg(any(feature = "rust-digest", test))]
#[derive(Debug, Clone, Copy, Default)]
pub struct RustDigest;

#[cfg(any(feature = "rust-digest", test))]
impl Digest for RustDigest {
    fn sha256(&self, data: &[u8]) -> DigestResult {
        use sha2::Digest as _;
        Ok(sha2::Sha256::digest(data).into())
    }

    fn hmac_sha256(&self, key: &[u8], data: &[u8]) -> DigestResult {
        use hmac::Mac as _;
        let mut mac = <hmac::Hmac<sha2::Sha256> as hmac::Mac>::new_from_slice(key)
            .expect("HMAC accepts any key length");
        mac.update(data);
        Ok(mac.finalize().into_bytes().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_4231_test_case_2() {
        let mac = RustDigest
            .hmac_sha256(b"Jefe", b"what do ya want for nothing?")
            .unwrap();
        assert_eq!(
            hex::encode(mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn sha256_abc() {
        assert_eq!(
            hex::encode(RustDigest.sha256(b"abc").unwrap()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
