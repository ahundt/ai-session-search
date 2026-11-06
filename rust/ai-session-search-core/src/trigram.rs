// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

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
use unicode_segmentation::UnicodeSegmentation;

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
/// fewer rows. If neither yields an all-usable (at least `MIN_TRIGRAM_CHARS`, bounded) literal set,
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
    best_groups(pattern).map(|(_, groups)| render_prefilter_groups(&groups))
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
pub(crate) fn render_prefilter_groups(groups: &[Vec<String>]) -> String {
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

/// Lowercase forms whose uppercase counterparts FTS5's trigram tokenizer does not fold to them.
///
/// FTS5 folds case from its own Unicode tables, which predate these case pairs (Cherokee, whose
/// ordinary written form is the uppercase syllabary; Georgian Mtavruli; Osage; Adlam; and Latin
/// letters added in later Unicode versions). Stored text keeps the uppercase scalar while a query
/// trigram carries the lowercase one, so a trigram containing one of these would exclude rows
/// the verifier accepts. 445 scalars in 60 inclusive ranges, sorted.
/// `the_unfolded_lowercase_table_matches_the_linked_fts5_tokenizer` recomputes this from the
/// SQLite this crate links and fails if a SQLite upgrade changes it.
const FTS5_UNFOLDED_LOWERCASE: &[(u32, u32)] = &[
    (0x019B, 0x019B),
    (0x025C, 0x025C),
    (0x0261, 0x0261),
    (0x0264, 0x0264),
    (0x026A, 0x026A),
    (0x026C, 0x026C),
    (0x0282, 0x0282),
    (0x0287, 0x0287),
    (0x029D, 0x029E),
    (0x03F3, 0x03F3),
    (0x0529, 0x0529),
    (0x052B, 0x052B),
    (0x052D, 0x052D),
    (0x052F, 0x052F),
    (0x10D0, 0x10FA),
    (0x10FD, 0x10FF),
    (0x13F8, 0x13FD),
    (0x1C8A, 0x1C8A),
    (0x1D8E, 0x1D8E),
    (0x2C5F, 0x2C5F),
    (0xA699, 0xA699),
    (0xA69B, 0xA69B),
    (0xA794, 0xA794),
    (0xA797, 0xA797),
    (0xA799, 0xA799),
    (0xA79B, 0xA79B),
    (0xA79D, 0xA79D),
    (0xA79F, 0xA79F),
    (0xA7B5, 0xA7B5),
    (0xA7B7, 0xA7B7),
    (0xA7B9, 0xA7B9),
    (0xA7BB, 0xA7BB),
    (0xA7BD, 0xA7BD),
    (0xA7BF, 0xA7BF),
    (0xA7C1, 0xA7C1),
    (0xA7C3, 0xA7C3),
    (0xA7C8, 0xA7C8),
    (0xA7CA, 0xA7CA),
    (0xA7CD, 0xA7CD),
    (0xA7CF, 0xA7CF),
    (0xA7D1, 0xA7D1),
    (0xA7D3, 0xA7D3),
    (0xA7D5, 0xA7D5),
    (0xA7D7, 0xA7D7),
    (0xA7D9, 0xA7D9),
    (0xA7DB, 0xA7DB),
    (0xA7F6, 0xA7F6),
    (0xAB53, 0xAB53),
    (0xAB70, 0xABBF),
    (0x104D8, 0x104FB),
    (0x10597, 0x105A1),
    (0x105A3, 0x105B1),
    (0x105B3, 0x105B9),
    (0x105BB, 0x105BC),
    (0x10CC0, 0x10CF2),
    (0x10D70, 0x10D85),
    (0x118C0, 0x118DF),
    (0x16E60, 0x16E7F),
    (0x16EBB, 0x16ED3),
    (0x1E922, 0x1E943),
];

fn fts5_leaves_uppercase_unfolded(lowercase: char) -> bool {
    let code = u32::from(lowercase);
    FTS5_UNFOLDED_LOWERCASE
        .binary_search_by(|&(start, end)| {
            if end < code {
                std::cmp::Ordering::Less
            } else if start > code {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// Decompose one required `literal` into OR-alternative AND-groups of lowercased 3-grams, e.g.
/// `econnreset` → `[[con, eco, ese, nnr, nre, onn, res, set]]`, or `None` when no trigram can
/// safely constrain it.
///
/// A row containing the literal contains every trigram of it, so requiring them all selects a
/// SUPERSET of matching rows (non-adjacent occurrences are fine; the caller verifies). That holds
/// only for trigrams FTS5 tokenizes the same way on both sides, so three kinds of position are
/// left out, and the trigrams of every remaining run of at least three characters are required:
///
/// - a grapheme whose lowercase spans several scalars (notably U+0130 → `i` + U+0307), which
///   FTS5 folds one scalar at a time;
/// - a lowercase form whose uppercase FTS5 does not fold to it ([`FTS5_UNFOLDED_LOWERCASE`]);
/// - a leading U+0307, which a stored U+0130 can supply as the second half of its fold.
///
/// A final `i` can be supplied by a stored U+0130 too, which FTS5 keeps as itself, so the
/// trigram ending there becomes two alternatives, one spelled with each. Leaving it out instead
/// would cost every short needle ending in `i` (`api`, `cli`) its prefilter.
///
/// Before this, any literal holding U+0130 was rejected whole, so `İstanbul` scanned an entire
/// index rather than using `stanbul`.
///
/// Sigma needs no exclusion. FTS5 folds `Σ`, `σ`, and `ς` onto one form, matching the verifier's
/// own rule (see `crate::util::fold_caseless_char`). The pre-v4 custom index stores the spelling
/// `str::to_lowercase` chose for each position instead, so a sigma query reaches its verifier by
/// a direct scan; `Db::prepare_content_prefilter` owns that decision, since only that path reads
/// the custom index. Time and memory are O(literal characters).
fn literal_to_trigrams(text: &str) -> Option<Vec<Vec<String>>> {
    let lowercase = text.to_lowercase();
    let mut chars: Vec<(char, bool)> = Vec::with_capacity(lowercase.len());
    for grapheme in lowercase.graphemes(true) {
        let expands = grapheme.chars().count() > 1;
        chars.extend(
            grapheme
                .chars()
                .map(|c| (c, !expands && !fts5_leaves_uppercase_unfolded(c))),
        );
    }
    if let Some(first) = chars.first_mut() {
        if first.0 == '\u{307}' {
            first.1 = false;
        }
    }

    let mut required: Vec<String> = Vec::new();
    let mut final_alternatives: Option<[String; 2]> = None;
    let mut index = 0;
    while index < chars.len() {
        if !chars[index].1 {
            index += 1;
            continue;
        }
        let start = index;
        while index < chars.len() && chars[index].1 {
            index += 1;
        }
        let run: Vec<char> = chars[start..index].iter().map(|&(c, _)| c).collect();
        if run.len() < MIN_TRIGRAM_CHARS {
            continue;
        }
        let ends_literal = index == chars.len();
        for window in run.windows(MIN_TRIGRAM_CHARS) {
            let trigram: String = window.iter().collect();
            let is_last = std::ptr::eq(window.as_ptr_range().end, run.as_ptr_range().end);
            if ends_literal && is_last && window[2] == 'i' {
                let dotted: String = [window[0], window[1], '\u{130}'].iter().collect();
                final_alternatives = Some([trigram, dotted]);
            } else {
                required.push(trigram);
            }
        }
    }

    let group = |extra: Option<String>| {
        let mut trigrams = required.clone();
        trigrams.extend(extra);
        trigrams.sort();
        trigrams.dedup();
        trigrams
    };
    match final_alternatives {
        Some([plain, dotted]) => Some(vec![group(Some(plain)), group(Some(dotted))]),
        None if required.is_empty() => None,
        None => Some(vec![group(None)]),
    }
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
        groups.extend(literal_to_trigrams(text)?);
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

    /// [`FTS5_UNFOLDED_LOWERCASE`] is exactly what the linked FTS5 tokenizer fails to fold.
    ///
    /// Every assigned scalar `h` whose lowercase is one different scalar is tokenized as `a{h}b`
    /// by the same trigram tokenizer the index uses, and `fts5vocab` reports the stored trigram.
    /// Where FTS5 stores something for `h` other than what it stores for `lower(h)`, `lower(h)`
    /// must be in the table, and nothing else may be: a missing entry drops real matches, an extra
    /// one costs a prefilter for nothing. A SQLite upgrade that changes its folding fails here and
    /// names the scalars to add or remove.
    #[test]
    fn the_unfolded_lowercase_table_matches_the_linked_fts5_tokenizer() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "create virtual table t using fts5(x, tokenize='trigram', detail='full');
             create virtual table v using fts5vocab(t, 'instance');",
        )
        .unwrap();
        let scalars: Vec<char> = (0x20_u32..=0x10_FFFF)
            .filter_map(char::from_u32)
            .filter(|&c| c != '"')
            .collect();
        {
            let tx = conn.unchecked_transaction().unwrap();
            let mut insert = tx
                .prepare("insert into t(rowid, x) values(?1, ?2)")
                .unwrap();
            for &c in &scalars {
                insert
                    .execute(rusqlite::params![u32::from(c), format!("a{c}b")])
                    .unwrap();
            }
            drop(insert);
            tx.commit().unwrap();
        }
        let mut stored = std::collections::HashMap::new();
        let mut query = conn.prepare("select term, doc from v").unwrap();
        for row in query
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
            })
            .unwrap()
        {
            let (term, doc) = row.unwrap();
            let middle: Vec<char> = term.chars().collect();
            if middle.len() == 3 {
                stored.insert(doc, middle[1]);
            }
        }

        let mut unfolded: Vec<u32> = Vec::new();
        for &h in &scalars {
            let mut lower = h.to_lowercase();
            let (Some(l), None) = (lower.next(), lower.next()) else {
                continue;
            };
            if l == h {
                continue;
            }
            if let (Some(of_h), Some(of_l)) = (stored.get(&u32::from(h)), stored.get(&u32::from(l)))
            {
                if of_h != of_l {
                    unfolded.push(u32::from(l));
                }
            }
        }
        unfolded.sort_unstable();
        unfolded.dedup();
        let table: Vec<u32> = FTS5_UNFOLDED_LOWERCASE
            .iter()
            .flat_map(|&(start, end)| start..=end)
            .collect();
        assert!(
            FTS5_UNFOLDED_LOWERCASE.windows(2).all(|w| w[0].1 < w[1].0),
            "ranges must be sorted and disjoint for the binary search"
        );
        let missing: Vec<String> = unfolded
            .iter()
            .filter(|c| !table.contains(c))
            .map(|c| format!("U+{c:04X}"))
            .collect();
        // An entry this toolchain's Unicode gives no case partner is tolerated: the minimum
        // supported Rust can predate a case pair the table already lists for newer ones, and an
        // unneeded entry only costs a prefilter on that scalar.
        let has_case_partner =
            |c: u32| char::from_u32(c).is_some_and(|c| c.to_uppercase().ne(std::iter::once(c)));
        let extra: Vec<String> = table
            .iter()
            .filter(|&&c| !unfolded.contains(&c) && has_case_partner(c))
            .map(|c| format!("U+{c:04X}"))
            .collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "FTS5_UNFOLDED_LOWERCASE disagrees with the linked tokenizer: \
             missing {missing:?}, extra {extra:?}"
        );
    }

    #[test]
    fn a_literal_holding_an_expanding_or_unfolded_character_keeps_its_safe_runs() {
        // `İstanbul` used to lose its whole prefilter to the U+0130, and scanned every row.
        let groups = trigram_prefilter_groups(&regex::escape("İstanbul")).expect("prefilterable");
        assert_eq!(
            groups,
            vec![vec!["anb", "bul", "nbu", "sta", "tan"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()]
        );
        // A final `i` may be the first half of a stored U+0130, so its trigram has two spellings,
        // and a short needle ending in `i` keeps a prefilter.
        assert_eq!(
            trigram_prefilter_groups("api").expect("prefilterable"),
            vec![vec!["api".to_string()], vec!["ap\u{130}".to_string()]]
        );
        // Cherokee lowercase forms are left out, because FTS5 stores the uppercase syllabary
        // unfolded; a needle of nothing else has no safe trigram and is scanned.
        assert_eq!(trigram_prefilter_groups("\u{13E3}\u{13B3}\u{13A9}"), None);
        // Only unsafe positions are lost: the ASCII run around them still constrains the set.
        assert_eq!(
            trigram_prefilter_groups("err\u{13E3}xyz").expect("prefilterable"),
            vec![vec!["err".to_string(), "xyz".to_string()]]
        );
    }
}
