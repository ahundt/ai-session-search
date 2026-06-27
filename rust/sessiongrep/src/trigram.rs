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
    let hir = regex_syntax::parse(pattern).ok()?;
    let mut best: Option<(usize, String)> = None;
    // Whole-pattern prefix AND suffix (both are required-substring sets); keep the more selective
    // (longer minimum literal ⇒ fewer candidate rows). This alone already captures a trailing
    // inner literal like `error.*ECONNRESET` (via the suffix `ECONNRESET`).
    for kind in [ExtractKind::Prefix, ExtractKind::Suffix] {
        consider_more_selective(&mut best, extract_query(&hir, kind));
    }
    // Inner literals: for a top-level concatenation, also take the prefix literals of each
    // REQUIRED element (skip min-0 repetitions like `.*`/`x?` and zero-width looks, whose match
    // is not guaranteed). Every match must contain every required element, so any one required
    // element's literals are a valid superset filter — this captures a selective literal flanked
    // on BOTH sides (`error.*ECONNRESET.*occurred` → `ECONNRESET`), which neither prefix nor
    // suffix can reach. Mirrors ripgrep's grep-regex inner-literal heuristic: a counted
    // repetition with min==0 is optional (skip); min>=1 is required (keep). Superset preserved:
    // alternations inside an element are OR'd by the Extractor's cross-product; concats AND.
    if let HirKind::Concat(elements) = hir.kind() {
        for element in elements {
            if matches!(element.kind(), HirKind::Repetition(rep) if rep.min == 0) {
                continue;
            }
            consider_more_selective(&mut best, extract_query(element, ExtractKind::Prefix));
        }
    }
    best.map(|(_, query)| query)
}

/// Extract `kind` (prefix/suffix) literals from `hir` and render them as a `(min_len, MATCH)`
/// query, or `None` when the sequence is unbounded / too short to index.
fn extract_query(hir: &Hir, kind: ExtractKind) -> Option<(usize, String)> {
    let mut extractor = Extractor::new();
    extractor.kind(kind);
    extractor.limit_class(CLASS_LIMIT);
    seq_to_match(&extractor.extract(hir))
}

/// Keep `candidate` if it is more selective (longer minimum literal) than the current `best`.
fn consider_more_selective(best: &mut Option<(usize, String)>, candidate: Option<(usize, String)>) {
    if let Some((min_len, query)) = candidate {
        if best.as_ref().is_none_or(|(best_len, _)| min_len > *best_len) {
            *best = Some((min_len, query));
        }
    }
}

/// Combine the prefilters of several patterns (OR). Returns `None` if ANY pattern cannot be
/// prefiltered — otherwise a candidate set built from the others would miss that pattern's
/// matches. Used for multi-pattern detection (e.g. corrections).
pub fn trigram_prefilter_all<I, S>(patterns: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut terms: Vec<String> = Vec::new();
    for pattern in patterns {
        // Any un-prefilterable pattern forces a full scan (else we'd miss its matches).
        terms.push(trigram_prefilter(pattern.as_ref())?);
    }
    if terms.is_empty() {
        return None;
    }
    // Each term is already an OR-group; OR is associative so a flat join is correct.
    Some(terms.join(" OR "))
}

/// Turn a literal sequence into `(min_literal_chars, OR-of-quoted-terms)`, or `None` if the
/// sequence is unbounded or any literal is too short to index. `min_literal_chars` is the
/// length of the shortest alternative — the weakest link that bounds the set's selectivity.
fn seq_to_match(seq: &Seq) -> Option<(usize, String)> {
    let literals = seq.literals()?; // None => infinite / unbounded
    if literals.is_empty() {
        return None;
    }
    let mut terms: Vec<String> = Vec::new();
    let mut min_len = usize::MAX;
    for literal in literals {
        let text = std::str::from_utf8(literal.as_bytes()).ok()?;
        let len = text.chars().count();
        if len < MIN_TRIGRAM_CHARS {
            return None; // can't constrain this alternative => unsafe to prefilter
        }
        min_len = min_len.min(len);
        // Lower-case: the trigram index is case-insensitive, so a lower-cased literal selects
        // a superset of the regex's matches. Double embedded quotes for FTS5 string syntax.
        let lowered = text.to_lowercase();
        terms.push(format!("\"{}\"", lowered.replace('"', "\"\"")));
    }
    terms.sort();
    terms.dedup();
    Some((min_len, terms.join(" OR ")))
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

    #[test]
    fn simple_literal_yields_one_term() {
        assert_eq!(trigram_prefilter("ECONNRESET").as_deref(), Some("\"econnreset\""));
    }

    #[test]
    fn alternation_yields_ored_terms() {
        // Sorted + deduped OR of the alternatives.
        assert_eq!(trigram_prefilter("foobar|bazqux").as_deref(), Some("\"bazqux\" OR \"foobar\""));
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
        // Prefix literal "no" is 2 chars; the suffix "that's"/"thats" is usable.
        let q = trigram_prefilter(r"\bno,?\s+that'?s\b").expect("suffix-prefilterable");
        let lits = quoted_literals(&q);
        assert!(lits.iter().any(|l| l.contains("that")), "{lits:?}");
    }

    #[test]
    fn prefilter_is_a_superset_of_regex_matches() {
        // THE correctness keystone: every text the (case-insensitive) regex matches must
        // contain at least one prefilter literal — i.e. the trigram MATCH would return it.
        // Patterns are the kind the corrections detector + user --regex actually use.
        let cases: &[(&str, &[&str])] = &[
            (r"\byou forgot\b", &["you forgot the tests", "well You Forgot it", "YOU FORGOT"]),
            (r"\byou (deleted|removed|reverted)\b", &["you deleted x", "you removed y", "You Reverted z"]),
            (r"\bno,?\s+that'?s\b", &["no, that's wrong", "no thats not it"]),
            (r"\balso need\b", &["we also need tests", "ALSO NEED more"]),
            (r"\bbut you\b", &["ok but you missed it"]),
            ("ECONNRESET", &["socket hang up ECONNRESET here"]),
            (r"\bstop doing\b", &["please stop doing that"]),
        ];
        for (pat, texts) in cases {
            let query = trigram_prefilter(pat)
                .unwrap_or_else(|| panic!("expected {pat:?} to be prefilterable"));
            let literals = quoted_literals(&query);
            assert!(!literals.is_empty(), "{pat:?} -> {query:?}");
            let re = regex::Regex::new(&format!("(?i){pat}")).unwrap();
            for text in *texts {
                assert!(re.is_match(text), "fixture {text:?} must match {pat:?}");
                let lowered = text.to_lowercase();
                assert!(
                    literals.iter().any(|l| lowered.contains(l.as_str())),
                    "SUPERSET VIOLATION: {text:?} matched {pat:?} but none of {literals:?} is a substring",
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
        let lits = quoted_literals(&q);
        assert_eq!(lits, vec!["econnreset".to_string()], "inner literal selected: {lits:?}");
        // Superset preserved: a real match still contains the chosen literal.
        let re = regex::Regex::new(r"(?i)error.*ECONNRESET.*occurred").unwrap();
        let text = "error: the deploy ECONNRESET and then it occurred again";
        assert!(re.is_match(text));
        assert!(text.to_lowercase().contains("econnreset"), "candidate is a superset");
        // An optional flanking element (min-0 repetition) must NOT be treated as required: the
        // selective required literal is still found, optional bits are skipped.
        let q2 = trigram_prefilter(r"(prefix)?ECONNRESET").expect("prefilterable");
        assert!(quoted_literals(&q2).iter().any(|l| l == "econnreset"), "{q2:?}");
    }

    #[test]
    fn all_combines_or_and_fails_closed_on_unprefilterable() {
        let q = trigram_prefilter_all([r"\byou forgot\b", "ECONNRESET"]).expect("both prefilterable");
        let lits = quoted_literals(&q);
        assert!(lits.iter().any(|l| l == "you forgot"));
        assert!(lits.iter().any(|l| l == "econnreset"));
        // If any pattern can't be prefiltered, the whole set must fall back to None.
        assert_eq!(trigram_prefilter_all([r"\byou forgot\b", "ab"]), None);
    }
}
