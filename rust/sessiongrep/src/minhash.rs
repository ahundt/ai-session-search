//! MinHash + LSH near-duplicate detection over message content (task #227).
//!
//! Finds near-duplicate / repeated messages without the O(N²·V) TF-IDF graph aise uses. Each
//! message is reduced to a set of word-3-gram **shingles**, summarized by a fixed-length MinHash
//! **signature** (an unbiased estimator of Jaccard similarity — Broder 1997), then bucketed with
//! **LSH banding** so only messages that collide in at least one band are compared. Candidate
//! pairs are confirmed with the EXACT Jaccard over their shingle sets, so LSH only affects recall
//! (which pairs are proposed), never correctness (which pairs are reported).
//!
//! All hashing is deterministic (fixed FNV-1a + a seeded integer mix), NOT process-reseeded
//! `RandomState` — so results are stable across runs and a future persistent index stays valid.

use std::collections::{HashMap, HashSet};

/// Signature length (number of min-hashes). `BANDS * ROWS` must equal this.
pub const NUM_HASHES: usize = 128;
/// LSH bands; with `ROWS` this sets the ~similarity threshold ((1/BANDS)^(1/ROWS) ≈ 0.71) at
/// which pairs start colliding. The exact-Jaccard verify applies the caller's real threshold.
pub const BANDS: usize = 16;
/// Rows per band. `BANDS * ROWS == NUM_HASHES`.
pub const ROWS: usize = 8;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a hash of a byte slice (deterministic).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = FNV_OFFSET;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// Deterministic 64-bit mix of a value with a seed (SplitMix64-style finalizer). Used to derive
/// `NUM_HASHES` independent hash functions from one shingle hash without storing random state.
fn mix(x: u64, seed: u64) -> u64 {
    let mut h = x ^ seed.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

/// Word-`k`-gram shingles of `text`, each hashed to a u64. Whitespace-split, case-folded so
/// trivial case differences don't reduce similarity. A text shorter than `k` words yields a
/// single shingle of the whole (lower-cased) text, so short messages still compare sensibly.
pub fn shingles(text: &str, k: usize) -> HashSet<u64> {
    let words: Vec<String> = text.split_whitespace().map(|w| w.to_lowercase()).collect();
    let mut set = HashSet::new();
    if words.len() < k {
        if !words.is_empty() {
            set.insert(fnv1a(words.join(" ").as_bytes()));
        }
        return set;
    }
    for window in words.windows(k) {
        set.insert(fnv1a(window.join(" ").as_bytes()));
    }
    set
}

/// MinHash signature: for each of `NUM_HASHES` hash functions, the minimum hash over all
/// shingles. The fraction of equal positions between two signatures estimates their Jaccard.
pub fn signature(shingles: &HashSet<u64>) -> [u64; NUM_HASHES] {
    let mut sig = [u64::MAX; NUM_HASHES];
    for &s in shingles {
        for (i, slot) in sig.iter_mut().enumerate() {
            let h = mix(s, i as u64);
            if h < *slot {
                *slot = h;
            }
        }
    }
    sig
}

/// LSH band hashes: `BANDS` values, each a hash of `ROWS` consecutive signature entries. Two
/// messages are LSH candidates iff they share at least one `(band_index, band_hash)`.
pub fn bands(sig: &[u64; NUM_HASHES]) -> [u64; BANDS] {
    let mut out = [0u64; BANDS];
    for (b, slot) in out.iter_mut().enumerate() {
        let mut h = FNV_OFFSET;
        for r in 0..ROWS {
            for byte in sig[b * ROWS + r].to_le_bytes() {
                h ^= byte as u64;
                h = h.wrapping_mul(FNV_PRIME);
            }
        }
        *slot = h;
    }
    out
}

/// Exact Jaccard similarity of two shingle sets (the verification step). Two empty sets are
/// defined as identical (1.0); an empty vs non-empty set is 0.0.
pub fn jaccard(a: &HashSet<u64>, b: &HashSet<u64>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let intersection = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        intersection / union
    }
}

/// Find near-duplicate index pairs among `contents` whose exact word-3-gram Jaccard ≥ `threshold`.
/// LSH proposes candidates (only items sharing a band are compared — near-linear instead of the
/// O(N²) all-pairs of a TF-IDF graph), and the exact Jaccard decides each pair. Returns
/// `(i, j, similarity)` with `i < j`, sorted by similarity descending.
pub fn near_duplicate_pairs(contents: &[String], threshold: f64) -> Vec<(usize, usize, f64)> {
    let shingle_sets: Vec<HashSet<u64>> = contents.iter().map(|c| shingles(c, 3)).collect();
    let mut buckets: HashMap<(usize, u64), Vec<usize>> = HashMap::new();
    for (i, set) in shingle_sets.iter().enumerate() {
        for (band_index, &band_hash) in bands(&signature(set)).iter().enumerate() {
            buckets.entry((band_index, band_hash)).or_default().push(i);
        }
    }
    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    let mut pairs = Vec::new();
    for members in buckets.values() {
        for a in 0..members.len() {
            for b in (a + 1)..members.len() {
                let (i, j) = (members[a].min(members[b]), members[a].max(members[b]));
                if !seen.insert((i, j)) {
                    continue; // already compared via another shared band
                }
                let sim = jaccard(&shingle_sets[i], &shingle_sets[j]);
                if sim >= threshold {
                    pairs.push((i, j, sim));
                }
            }
        }
    }
    pairs.sort_by(|x, y| y.2.partial_cmp(&x.2).unwrap_or(std::cmp::Ordering::Equal));
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_are_deterministic() {
        let s = shingles("the quick brown fox jumps over", 3);
        assert_eq!(
            signature(&s),
            signature(&s),
            "same input → identical signature"
        );
    }

    #[test]
    fn jaccard_estimate_tracks_similarity() {
        // Identical → 1.0; disjoint → 0.0; overlapping → in-between, and the signature estimate
        // is close to the exact Jaccard.
        let a = shingles("alpha bravo charlie delta echo foxtrot golf", 3);
        let b = a.clone();
        let c = shingles("november oscar papa quebec romeo sierra tango", 3);
        assert_eq!(jaccard(&a, &b), 1.0);
        assert_eq!(jaccard(&a, &c), 0.0);

        let near = shingles("alpha bravo charlie delta echo foxtrot hotel", 3);
        let exact = jaccard(&a, &near);
        assert!(exact > 0.3 && exact < 1.0, "partial overlap: {exact}");
        // MinHash estimate = fraction of equal signature slots; should be near the exact value.
        let (sa, sn) = (signature(&a), signature(&near));
        let est = (0..NUM_HASHES).filter(|&i| sa[i] == sn[i]).count() as f64 / NUM_HASHES as f64;
        assert!((est - exact).abs() < 0.2, "estimate {est} ≈ exact {exact}");
    }

    #[test]
    fn near_duplicates_share_a_band_distinct_do_not() {
        // A genuine near-duplicate (one appended word → Jaccard ≈ 0.9, well above the LSH
        // threshold) collides in at least one band; an unrelated text does not. A pair that only
        // differs by a single SUBSTITUTED word (Jaccard ≈ 0.6, below threshold) is intentionally
        // NOT relied on here — LSH recall drops off below its threshold by design, and the exact
        // Jaccard verify is what enforces the caller's real similarity cutoff.
        let base = "fix the failing authentication test in the login flow before the next release tomorrow";
        let a = signature(&shingles(base, 3));
        let b = signature(&shingles(&format!("{base} please"), 3));
        let c = signature(&shingles(
            "completely unrelated note about database vacuum scheduling for the weekend maintenance window",
            3,
        ));
        let (ba, bb, bc) = (bands(&a), bands(&b), bands(&c));
        let shares = |x: &[u64; BANDS], y: &[u64; BANDS]| (0..BANDS).any(|i| x[i] == y[i]);
        assert!(shares(&ba, &bb), "near-duplicates collide in a band");
        assert!(!shares(&ba, &bc), "unrelated texts do not collide");
    }

    #[test]
    fn near_duplicate_pairs_finds_repeats_only() {
        let contents = vec![
            "fix the failing authentication test in the login flow before the release".to_string(),
            "fix the failing authentication test in the login flow before the release please"
                .to_string(), // near-duplicate of [0]
            "schedule the database vacuum for the weekend maintenance window tonight".to_string(),
        ];
        let pairs = near_duplicate_pairs(&contents, 0.8);
        assert_eq!(
            pairs.len(),
            1,
            "exactly one near-duplicate pair, the distinct one excluded"
        );
        assert_eq!((pairs[0].0, pairs[0].1), (0, 1));
        assert!(pairs[0].2 >= 0.8, "verified similarity {}", pairs[0].2);
    }
}
