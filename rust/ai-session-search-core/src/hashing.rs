//! One content digest for every subsystem that needs to answer "are these the same bytes?".
//!
//! Analysis bundles, correction-policy identity, and the skill install manifest all need a stable
//! content hash. Each grew its own one-line helper, which is three chances for the algorithm, the
//! encoding, or the case of the hex to drift apart — and a manifest that hashes differently from
//! the thing it describes is worse than no manifest. This is the single definition.

use std::io::{self, Write};

use serde::Serialize;
use sha2::{Digest, Sha256};

/// Lowercase hex SHA-256 of `content`.
///
/// Callers digest the **exact bytes they read or wrote**, not a re-serialized value, so that a
/// whitespace- or comment-only edit still changes the digest.
pub(crate) fn sha256(content: &[u8]) -> String {
    format!("{:x}", Sha256::digest(content))
}

/// Incremental SHA-256 over a domain-separated sequence of length-framed values.
///
/// Framing prevents adjacent values from becoming ambiguous (`["ab", "c"]` differs from
/// `["a", "bc"]`). Updating and retained memory are `O(value bytes)` and `O(1)` respectively.
pub(crate) struct FramedSha256 {
    digest: Sha256,
}

impl FramedSha256 {
    pub(crate) fn new(domain: &[u8]) -> Self {
        let mut digest = Sha256::new();
        digest.update(domain);
        digest.update([0]);
        Self { digest }
    }

    pub(crate) fn update_bytes(&mut self, value: &[u8]) {
        update_length_prefixed(&mut self.digest, value);
    }

    /// Append bytes whose complete length was already framed by the caller.
    ///
    /// This supports serializers that write one logical value in multiple chunks without making
    /// serializer buffer boundaries part of the digest. Callers must write the value length first.
    fn update_preframed_bytes(&mut self, value: &[u8]) {
        self.digest.update(value);
    }

    /// Append one compact JSON value with stable length framing and no value-sized buffer.
    ///
    /// Serialization runs twice: once to count bytes and once to hash them. Runtime is
    /// `O(serialized bytes)` with a constant factor of two; additional memory is `O(1)`.
    pub(crate) fn update_json(&mut self, value: &impl Serialize) -> serde_json::Result<()> {
        let mut counter = ByteCounter::default();
        serde_json::to_writer(&mut counter, value)?;
        self.update_u64(counter.bytes);
        serde_json::to_writer(PreframedDigestWriter(self), value)
    }

    pub(crate) fn update_u8(&mut self, value: u8) {
        self.digest.update([value]);
    }

    pub(crate) fn update_u64(&mut self, value: u64) {
        self.digest.update(value.to_be_bytes());
    }

    pub(crate) fn update_u32(&mut self, value: u32) {
        self.digest.update(value.to_be_bytes());
    }

    pub(crate) fn update_i64(&mut self, value: i64) {
        self.digest.update(value.to_be_bytes());
    }

    pub(crate) fn finish(self) -> String {
        let bytes = self.digest.finalize();
        let mut encoded = String::with_capacity("sha256:".len() + bytes.len() * 2);
        encoded.push_str("sha256:");
        for byte in bytes {
            use std::fmt::Write as _;
            write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
        }
        encoded
    }

    /// Finish as four signed words for fixed-width SQLite metadata.
    ///
    /// This preserves all 256 digest bits without allocating a hex string. Signed words are used
    /// because SQLite's INTEGER storage class is an `i64`. Runtime and retained memory are `O(1)`.
    pub(crate) fn finish_i64_words(self) -> [i64; 4] {
        let bytes = self.digest.finalize();
        std::array::from_fn(|word| {
            let start = word * std::mem::size_of::<i64>();
            i64::from_be_bytes(
                bytes[start..start + std::mem::size_of::<i64>()]
                    .try_into()
                    .expect("SHA-256 always contains four i64 words"),
            )
        })
    }
}

#[derive(Default)]
struct ByteCounter {
    bytes: u64,
}

impl Write for ByteCounter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(buffer.len() as u64)
            .ok_or_else(|| io::Error::other("serialized digest value exceeds u64 bytes"))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct PreframedDigestWriter<'digest>(&'digest mut FramedSha256);

impl Write for PreframedDigestWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.update_preframed_bytes(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn update_length_prefixed(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
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

    #[test]
    fn fixed_width_words_preserve_every_digest_byte_without_allocation() {
        let mut digest = FramedSha256::new(b"aise-word-test-v1");
        digest.update_bytes(b"same input");
        let words = digest.finish_i64_words();

        let mut repeated = FramedSha256::new(b"aise-word-test-v1");
        repeated.update_bytes(b"same input");
        assert_eq!(words, repeated.finish_i64_words());

        let mut changed = FramedSha256::new(b"aise-word-test-v1");
        changed.update_bytes(b"different input");
        assert_ne!(words, changed.finish_i64_words());
    }

    #[test]
    fn framed_digest_preserves_domain_value_boundaries_and_order() {
        let mut first = FramedSha256::new(b"aise-test-digest-v1");
        first.update_bytes(b"ab");
        first.update_bytes(b"c");

        let mut different_boundaries = FramedSha256::new(b"aise-test-digest-v1");
        different_boundaries.update_bytes(b"a");
        different_boundaries.update_bytes(b"bc");

        let mut different_order = FramedSha256::new(b"aise-test-digest-v1");
        different_order.update_bytes(b"c");
        different_order.update_bytes(b"ab");

        let mut different_domain = FramedSha256::new(b"aise-other-digest-v1");
        different_domain.update_bytes(b"ab");
        different_domain.update_bytes(b"c");

        let expected = first.finish();
        assert!(expected.starts_with("sha256:"));
        assert_eq!(expected.len(), "sha256:".len() + 64);
        assert_ne!(expected, different_boundaries.finish());
        assert_ne!(expected, different_order.finish());
        assert_ne!(expected, different_domain.finish());
    }

    #[test]
    fn streamed_json_matches_one_length_framed_compact_value() {
        let value = serde_json::json!({"ordered": [1, 2, 3], "text": "value"});
        let bytes = serde_json::to_vec(&value).unwrap();

        let mut streamed = FramedSha256::new(b"aise-json-digest-v1");
        streamed.update_json(&value).unwrap();
        let mut buffered = FramedSha256::new(b"aise-json-digest-v1");
        buffered.update_bytes(&bytes);

        assert_eq!(streamed.finish(), buffered.finish());
    }
}
