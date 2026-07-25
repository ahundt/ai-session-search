//! One content digest for every subsystem that needs to answer "are these the same bytes?".
//!
//! Analysis bundles, correction-policy identity, and the skill install manifest all need a stable
//! content hash. Each grew its own one-line helper, which is three chances for the algorithm, the
//! encoding, or the case of the hex to drift apart — and a manifest that hashes differently from
//! the thing it describes is worse than no manifest. This is the single definition.

use sha2::{Digest, Sha256};

/// Lowercase hex SHA-256 of `content`.
///
/// Callers digest the **exact bytes they read or wrote**, not a re-serialized value, so that a
/// whitespace- or comment-only edit still changes the digest.
pub(crate) fn sha256(content: &[u8]) -> String {
    format!("{:x}", Sha256::digest(content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_stable_lowercase_hex_and_byte_sensitive() {
        // Pinned against the well-known SHA-256 of the empty input, so a dependency swap or an
        // encoding change cannot quietly alter every manifest and policy identity at once.
        assert_eq!(
            sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_ne!(
            sha256(b"abc"),
            sha256(b"abc\n"),
            "trailing bytes must count"
        );
    }
}
