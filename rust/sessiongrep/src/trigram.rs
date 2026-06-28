//! Trigram-index prefiltering for fast regex / substring search.
//!
//! This is the Google Code Search technique (Russ Cox, "Regular Expression Matching with a
//! Trigram Index"): rather than run a regex over the whole corpus, extract the literal
//! substrings that EVERY match must contain, use an FTS5 trigram index to fetch only the
//! candidate rows containing those substrings, and run the full regex on just those.
//!
//! [`trigram_prefilter`] converts one regex into an FTS5 `MATCH` query selecting a SUPERSET
//! of the rows the regex can match, or `None` when the index cannot safely narrow the search
//! (so the caller must fall back to a full scan). The superset contract is the correctness
//! keystone: the prefilter must never drop a row the regex would match.

use regex_syntax::hir::literal::{ExtractKind, Extractor, Seq};
use regex_syntax::hir::{Hir, HirKind};

/// FTS5's trigram tokenizer only indexes runs of at least three characters, so a shorter
/// required literal cannot constrain the candidate set.
const MIN_TRIGRAM_CHARS: usize = 3;

/// Cap on character-class expansion during literal extraction. `[A-Z]` etc. would otherwise
/// blow a literal set up into thousands of entries; past this many the sequence is treated as
/// infinite (→ `None` → full scan). Mirrors the `regex-syntax` default but pinned for
/// predictable behavior across crate versions.
const CLASS_LIMIT: usize = 10;

/// Build an FTS5 trigram `MATCH` query selecting a superset of the rows `pattern` (a Rust
/// regex) can match, or `None` to fall back to a full scan.
///
/// We extract the regex's required **prefix** literals (every match must begin with one) AND
/// its **suffix** literals (every match must end with one), then keep whichever set is more
/// selective — i.e. has the longer minimum literal, since a longer required substring matches
/// fewer rows. If neither yields an all-usable (≥ [`MIN_TRIGRAM_CHARS`], bounded) literal set,
/// we return `None`: correctness first.
///
/// CALLER CONTRACT — this is a *candidate filter*, not a decision: `regex-syntax` treats
/// look-around (`\b`, `^`, `$`) as matching the empty string, so a candidate that contains a
/// required literal may still not match the full regex (e.g. `\bcat\b` vs `scatter`). The
/// caller MUST run the full regex on the returned candidates to confirm.
///
/// Note: a `(?i)`/case-insensitive regex makes literal extraction blow up into case variants
/// and usually yields `None` here (safe fall back). For known-literal callers (e.g. the
/// correction keyword fragments) pass the un-flagged pattern and let the case-insensitive
/// trigram index do the case folding — a lower-cased literal is a superset of any-case matches.
///
/// Selectivity is a known "black art": prefix/suffix literals can be unselective for patterns
/// whose only rare substring is *inner* (e.g. `error.*ECONNRESET` → prefix `error`). A custom
/// inner-literal extractor over the public `Seq`/`Literal` API would improve this; tracked as
/// a follow-up. Until then such patterns get a correct-but-broad prefilter or fall back.
pub fn trigram_prefilter(pattern: &str) -> Option<String> {
    best_groups(pattern).map(|(_, groups)| render_groups(&groups))
}

/// Like [`trigram_prefilter`] but returns the structured OR-of-AND trigram groups (each group a
/// set of lowercased 3-char trigrams that must ALL be present; groups are alternatives, any one
/// suffices) instead of an FTS5 `MATCH` string. This is what the custom trigram index
/// ([`crate::trigram_index`]) consumes directly — same SUPERSET contract and selectivity
/// heuristics, no FTS5 query-string round-trip.
pub fn trigram_prefilter_groups(pattern: &str) -> Option<Vec<Vec<String>>> {
    best_groups(pattern).map(|(_, groups)| groups)
}

/// Core selection: the most selective required-literal trigram groups for `pattern` as
/// `(min_literal_chars, OR-of-AND groups)`, or `None` to fall back to a full scan. Considers the
/// whole-pattern prefix AND suffix (both required-substring sets; keep the more selective = longer
/// minimum literal ⇒ fewer candidate rows), PLUS each required inner element's prefix literals.
///
/// Inner literals: for a top-level concatenation, take the prefix literals of each REQUIRED
/// element (skip min-0 repetitions like `.*`/`x?` and zero-width looks, whose match is not
/// guaranteed). Every match must contain every required element, so any one required element's
/// literals are a valid superset filter — this captures a selective literal flanked on BOTH sides
/// (`error.*ECONNRESET.*occurred` → `ECONNRESET`), which neither prefix nor suffix can reach.
/// Mirrors ripgrep's grep-regex inner-literal heuristic: min==0 repetition is optional (skip),
/// min>=1 is required (keep). Superset preserved: alternations inside an element are OR'd by the
/// Extractor's cross-product; concats AND.
fn best_groups(pattern: &str) -> Option<(usize, Vec<Vec<String>>)> {
    let hir = regex_syntax::parse(pattern).ok()?;
    let mut best: Option<(usize, Vec<Vec<String>>)> = None;
    for kind in [ExtractKind::Prefix, ExtractKind::Suffix] {
        consider_more_selective(&mut best, extract_groups(&hir, kind));
    }
    if let HirKind::Concat(elements) = hir.kind() {
        for element in elements {
            if matches!(element.kind(), HirKind::Repetition(rep) if rep.min == 0) {
                continue;
            }
            consider_more_selective(&mut best, extract_groups(element, ExtractKind::Prefix));
        }
    }
    best
}

/// Render OR-of-AND trigram groups as an FTS5 `MATCH` query (for the public
/// [`trigram_prefilter`]/[`trigram_prefilter_all`] utilities). Each trigram is quoted (embedded
/// quotes doubled per FTS5 syntax); a group's trigrams are AND'd; multiple groups are OR'd, each
/// parenthesized so one alternative's AND-chain stays independent of the others.
fn render_groups(groups: &[Vec<String>]) -> String {
    let render_group = |group: &Vec<String>| {
        group
            .iter()
            .map(|trigram| format!("\"{}\"", trigram.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" AND ")
    };
    if groups.len() == 1 {
        render_group(&groups[0])
    } else {
        groups
            .iter()
            .map(|group| format!("({})", render_group(group)))
            .collect::<Vec<_>>()
            .join(" OR ")
    }
}

/// Extract `kind` (prefix/suffix) literals from `hir` as `(min_len, OR-of-AND trigram groups)`,
/// or `None` when the sequence is unbounded / too short to index.
fn extract_groups(hir: &Hir, kind: ExtractKind) -> Option<(usize, Vec<Vec<String>>)> {
    let mut extractor = Extractor::new();
    extractor.kind(kind);
    extractor.limit_class(CLASS_LIMIT);
    seq_to_groups(&extractor.extract(hir))
}

/// Keep `candidate` if it is more selective (longer minimum literal) than the current `best`.
fn consider_more_selective(
    best: &mut Option<(usize, Vec<Vec<String>>)>,
    candidate: Option<(usize, Vec<Vec<String>>)>,
) {
    if let Some((min_len, groups)) = candidate {
        if best
            .as_ref()
            .is_none_or(|(best_len, _)| min_len > *best_len)
        {
            *best = Some((min_len, groups));
        }
    }
}

/// Combine the prefilters of several patterns into one OR query, for scanning a LARGE corpus for
/// any of several regexes at once. Returns `None` if ANY pattern cannot be prefiltered — otherwise
/// a candidate set built from the others would miss that pattern's matches. (Note: `corrections`
/// no longer uses this — it scans the small `role='user'` slice directly; see `Db::find_corrections`
/// — but this stays as a general public utility for multi-pattern prefiltering over a wide corpus.)
pub fn trigram_prefilter_all<I, S>(patterns: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut terms: Vec<String> = Vec::new();
    for pattern in patterns {
        // Any un-prefilterable pattern forces a full scan (else we'd miss its matches).
        // Parenthesize each pattern's query: it is an AND-of-trigrams chain (possibly itself an
        // OR of alternatives), and we OR-join across patterns, so wrap each to keep one pattern's
        // match independent of the others regardless of FTS5 operator precedence.
        terms.push(format!("({})", trigram_prefilter(pattern.as_ref())?));
    }
    if terms.is_empty() {
        return None;
    }
    // Each wrapped term is one pattern's full filter; OR is associative so a flat join is correct.
    Some(terms.join(" OR "))
}

/// Decompose one required `literal` into its sorted, deduped, lowercased overlapping 3-grams —
/// e.g. `econnreset` → `[con, eco, ese, nnr, nre, onn, res, set]`. Returns `None` if the literal
/// is shorter than [`MIN_TRIGRAM_CHARS`].
///
/// These are the AND-group: a row that contains the literal as a contiguous substring contains
/// ALL of these trigrams, so requiring all of them selects a SUPERSET of rows containing the
/// literal (the trigrams may be non-adjacent in a candidate — fine, the caller's regex verifies).
/// Char-based (not byte) to match FTS5's trigram tokenizer and the custom index's tokenization.
/// Lower-cased because both indexes fold case, so a lower-cased trigram set is a superset of the
/// regex's any-case matches.
fn literal_to_trigrams(text: &str) -> Option<Vec<String>> {
    let chars: Vec<char> = text.to_lowercase().chars().collect();
    if chars.len() < MIN_TRIGRAM_CHARS {
        return None;
    }
    let mut trigrams: Vec<String> = (0..=chars.len() - MIN_TRIGRAM_CHARS)
        .map(|i| chars[i..i + MIN_TRIGRAM_CHARS].iter().collect())
        .collect();
    trigrams.sort();
    trigrams.dedup();
    Some(trigrams)
}

/// Turn a literal sequence into `(min_literal_chars, OR-of-AND groups)`, or `None` if the sequence
/// is unbounded or any literal is too short to index. Each alternative literal becomes an AND-group
/// of its trigrams (see [`literal_to_trigrams`]); the alternatives are the OR. `min_literal_chars`
/// is the length of the shortest alternative — the weakest link that bounds the set's selectivity.
fn seq_to_groups(seq: &Seq) -> Option<(usize, Vec<Vec<String>>)> {
    let literals = seq.literals()?; // None => infinite / unbounded
    if literals.is_empty() {
        return None;
    }
    let mut groups: Vec<Vec<String>> = Vec::new();
    let mut min_len = usize::MAX;
    for literal in literals {
        let text = std::str::from_utf8(literal.as_bytes()).ok()?;
        let len = text.chars().count();
        if len < MIN_TRIGRAM_CHARS {
            return None; // can't constrain this alternative => unsafe to prefilter
        }
        min_len = min_len.min(len);
        groups.push(literal_to_trigrams(text)?);
    }
    // Sort+dedup so identical alternatives collapse and output is stable.
    groups.sort();
    groups.dedup();
    Some((min_len, groups))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse the `"..."`-quoted terms back out of an OR query (test helper).
    fn quoted_literals(query: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut chars = query.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '"' {
                continue;
            }
            let mut lit = String::new();
            while let Some(&n) = chars.peek() {
                chars.next();
                if n == '"' {
                    // Doubled quote = escaped literal quote.
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        lit.push('"');
                        continue;
                    }
                    break;
                }
                lit.push(n);
            }
            out.push(lit);
        }
        out
    }

    /// The test oracle for [`literal_to_trigram_and`]: the sorted, deduped lowercase 3-grams of
    /// `s` (empty if `s` has fewer than 3 chars).
    fn trigrams_of(s: &str) -> Vec<String> {
        let chars: Vec<char> = s.to_lowercase().chars().collect();
        if chars.len() < MIN_TRIGRAM_CHARS {
            return Vec::new();
        }
        let mut out: Vec<String> = (0..=chars.len() - MIN_TRIGRAM_CHARS)
            .map(|i| chars[i..i + MIN_TRIGRAM_CHARS].iter().collect())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Split a prefilter query into its OR-groups, each the set of ANDed trigrams. The grammar
    /// we emit is a flat `OR` of `AND`-chains, so splitting on " OR " then pulling the quoted
    /// trigrams of each group is sufficient (parens/`AND` are ignored by `quoted_literals`).
    fn parse_groups(query: &str) -> Vec<Vec<String>> {
        query.split(" OR ").map(quoted_literals).collect()
    }

    #[test]
    fn simple_literal_yields_one_term() {
        // One required literal → the AND of its trigrams, no OR.
        let q = trigram_prefilter("ECONNRESET").expect("prefilterable");
        assert!(!q.contains(" OR "), "single literal must not OR: {q}");
        assert!(q.contains(" AND "), "must AND its trigrams: {q}");
        assert_eq!(quoted_literals(&q), trigrams_of("econnreset"));
    }

    #[test]
    fn alternation_yields_ored_terms() {
        // Two alternatives → OR of two parenthesized AND-of-trigram groups.
        let q = trigram_prefilter("foobar|bazqux").expect("prefilterable");
        let groups = parse_groups(&q);
        assert_eq!(groups.len(), 2, "{q}");
        assert!(groups.contains(&trigrams_of("foobar")), "{q}");
        assert!(groups.contains(&trigrams_of("bazqux")), "{q}");
    }

    #[test]
    fn too_short_literal_falls_back_to_none() {
        // A 2-char literal can't be constrained by a trigram index.
        assert_eq!(trigram_prefilter("ab"), None);
        assert_eq!(trigram_prefilter("a|bb"), None);
    }

    #[test]
    fn unbounded_prefix_falls_back_to_none() {
        // Leading char class => infinite prefix; no suffix literal either.
        assert_eq!(trigram_prefilter("[a-z]+"), None);
        assert_eq!(trigram_prefilter(".*"), None);
    }

    #[test]
    fn suffix_used_when_prefix_is_too_short() {
        // Prefix literal "no" is 2 chars; the suffix "that's"/"thats" is usable. Its trigrams
        // (e.g. "tha", "hat") must appear in the prefilter.
        let q = trigram_prefilter(r"\bno,?\s+that'?s\b").expect("suffix-prefilterable");
        let lits = quoted_literals(&q);
        assert!(lits.iter().any(|l| l == "tha"), "{lits:?}");
        assert!(lits.iter().any(|l| l == "hat"), "{lits:?}");
    }

    #[test]
    fn prefilter_is_a_superset_of_regex_matches() {
        // THE correctness keystone: every text the (case-insensitive) regex matches must contain
        // ALL trigrams of at least one OR-group — i.e. the trigram AND-query would return it.
        // Patterns are the kind the corrections detector + user --regex actually use.
        let cases: &[(&str, &[&str])] = &[
            (
                r"\byou forgot\b",
                &["you forgot the tests", "well You Forgot it", "YOU FORGOT"],
            ),
            (
                r"\byou (deleted|removed|reverted)\b",
                &["you deleted x", "you removed y", "You Reverted z"],
            ),
            (
                r"\bno,?\s+that'?s\b",
                &["no, that's wrong", "no thats not it"],
            ),
            (r"\balso need\b", &["we also need tests", "ALSO NEED more"]),
            (r"\bbut you\b", &["ok but you missed it"]),
            ("ECONNRESET", &["socket hang up ECONNRESET here"]),
            (r"\bstop doing\b", &["please stop doing that"]),
        ];
        for (pat, texts) in cases {
            let query = trigram_prefilter(pat)
                .unwrap_or_else(|| panic!("expected {pat:?} to be prefilterable"));
            let groups = parse_groups(&query);
            assert!(!groups.is_empty(), "{pat:?} -> {query:?}");
            let re = regex::Regex::new(&format!("(?i){pat}")).unwrap();
            for text in *texts {
                assert!(re.is_match(text), "fixture {text:?} must match {pat:?}");
                let lowered = text.to_lowercase();
                assert!(
                    groups
                        .iter()
                        .any(|g| g.iter().all(|tri| lowered.contains(tri.as_str()))),
                    "SUPERSET VIOLATION: {text:?} matched {pat:?} but no group of {groups:?} \
                     is fully contained",
                );
            }
        }
    }

    #[test]
    fn inner_literal_beats_prefix_and_suffix() {
        // #228: a selective literal flanked on BOTH sides (`error.*ECONNRESET.*occurred`) is
        // captured by inner extraction — prefix `error` (5) and suffix `occurred` (8) are both
        // less selective than the inner `econnreset` (10), so it wins.
        let q = trigram_prefilter(r"error.*ECONNRESET.*occurred").expect("prefilterable");
        assert_eq!(
            parse_groups(&q),
            vec![trigrams_of("econnreset")],
            "inner literal selected: {q}"
        );
        // Superset preserved: a real match still contains the chosen literal's trigrams.
        let re = regex::Regex::new(r"(?i)error.*ECONNRESET.*occurred").unwrap();
        let text = "error: the deploy ECONNRESET and then it occurred again";
        assert!(re.is_match(text));
        assert!(
            text.to_lowercase().contains("econnreset"),
            "candidate is a superset"
        );
        // An optional flanking element (min-0 repetition) must NOT be treated as required: the
        // selective required literal is still found, optional bits are skipped.
        let q2 = trigram_prefilter(r"(prefix)?ECONNRESET").expect("prefilterable");
        let lits2 = quoted_literals(&q2);
        assert!(
            trigrams_of("econnreset").iter().all(|t| lits2.contains(t)),
            "{q2:?}"
        );
    }

    #[test]
    fn all_combines_or_and_fails_closed_on_unprefilterable() {
        let q =
            trigram_prefilter_all([r"\byou forgot\b", "ECONNRESET"]).expect("both prefilterable");
        let lits = quoted_literals(&q);
        assert!(
            trigrams_of("you forgot").iter().all(|t| lits.contains(t)),
            "{q}"
        );
        assert!(
            trigrams_of("econnreset").iter().all(|t| lits.contains(t)),
            "{q}"
        );
        // If any pattern can't be prefiltered, the whole set must fall back to None.
        assert_eq!(trigram_prefilter_all([r"\byou forgot\b", "ab"]), None);
    }

    #[test]
    fn structured_groups_match_the_fts5_string() {
        // The structured groups (custom-index input) must describe the SAME OR-of-AND trigram sets
        // as the rendered FTS5 string, so both prefilters select identical candidates.
        for pat in [
            "ECONNRESET",
            "foobar|bazqux",
            r"\byou (deleted|removed|reverted)\b",
            r"error.*ECONNRESET.*occurred",
        ] {
            let groups = trigram_prefilter_groups(pat).expect("prefilterable");
            let from_string = parse_groups(&trigram_prefilter(pat).unwrap());
            // Compare as sets-of-sets (order-independent).
            let norm = |gs: &[Vec<String>]| {
                let mut v: Vec<Vec<String>> = gs
                    .iter()
                    .map(|g| {
                        let mut g = g.clone();
                        g.sort();
                        g
                    })
                    .collect();
                v.sort();
                v
            };
            assert_eq!(norm(&groups), norm(&from_string), "mismatch for {pat:?}");
        }
        assert_eq!(trigram_prefilter_groups("ab"), None);
    }
}
