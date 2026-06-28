//! Custom, parallel-built trigram index — a regex/substring PREFILTER that can replace FTS5's
//! single-threaded trigram virtual table.
//!
//! WHY: FTS5 builds its trigram index in the single SQLite writer; tokenizing the whole corpus is
//! ~80% of a cold build (measured ~145 s / 1.8 GB on a 16-core host — see
//! `~/.claude/notes/sessiongrep_perf_benchmarks/`). The work is embarrassingly parallel, so we
//! tokenize with Rayon and bulk-load compact postings instead: measured ~5× faster to build, same
//! on-disk size, sub-3 ms candidate queries.
//!
//! DESIGN — base + delta, one common pathway:
//!   * The index is a **base** covering message ids ≤ `base_max_id`, built in parallel by [`build`].
//!   * Incremental reindex just inserts message rows — it does NO trigram work (no triggers).
//!   * A regex search's candidate set = `candidates(base) ∪ {ids > base_max_id}`; the caller's Rust
//!     regex re-verifies BOTH, so the recent "delta" (ids > base_max_id) is covered by a direct
//!     scan, bounded by the same corpus-size gate that already exists. When the delta grows past a
//!     threshold, the base is rebuilt (parallel). Cold build, incremental, and query share this one
//!     pathway; the base build is lazy/deferred by construction.
//!
//! CORRECTNESS: like the FTS5 prefilter this returns a SUPERSET of candidate rows (trigrams of a
//! required literal may appear non-adjacently in a candidate), which the caller verifies with the
//! real regex. Trigrams are lowercased 3-char grams (matching [`crate::trigram`] and FTS5's
//! case-insensitive trigram tokenizer).
//!
//! STORAGE: `trigram_postings(tg text primary key, ids blob)` — `ids` is the sorted message ids as
//! delta-encoded varints (the same compact form FTS5 uses internally); `trigram_meta(key, value)`
//! holds `base_max_id`.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use rusqlite::{params, Connection};

/// Create the index tables if absent. Safe to call repeatedly (used by `Db::init` and [`build`]).
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "create table if not exists trigram_postings (tg text primary key, ids blob not null) without rowid;
         create table if not exists trigram_meta (key text primary key, value integer not null);",
    )?;
    Ok(())
}

/// Highest message id covered by the base index (`0` if never built).
pub fn base_max_id(conn: &Connection) -> Result<i64> {
    ensure_schema(conn)?;
    let v: Option<i64> = conn
        .query_row(
            "select value from trigram_meta where key = 'base_max_id'",
            [],
            |r| r.get(0),
        )
        .ok();
    Ok(v.unwrap_or(0))
}

/// Build (or rebuild) the base trigram index over ALL current messages, in parallel. Returns the
/// new `base_max_id`. Replaces any existing postings in one transaction.
///
/// Memory: holds message content + the in-RAM postings map transiently (~content size + ~postings
/// size). For very large corpora this trades memory for build speed; a future refinement could
/// shard the trigram space across passes to bound peak memory.
pub fn build(conn: &Connection) -> Result<i64> {
    ensure_schema(conn)?;
    // Materialize (id, content) before going parallel: rusqlite Connection/Statement are not Sync.
    let mut select = conn.prepare("select id, content from messages")?;
    let rows: Vec<(i64, String)> = select
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(select);
    let base_max = rows.iter().map(|(id, _)| *id).max().unwrap_or(0);
    let postings = build_postings(&rows);

    let tx = conn.unchecked_transaction()?;
    tx.execute("delete from trigram_postings", [])?;
    {
        let mut stmt = tx.prepare("insert into trigram_postings (tg, ids) values (?1, ?2)")?;
        for (tg, ids) in &postings {
            stmt.execute(params![tg, encode_ids(ids)])?;
        }
    }
    tx.execute(
        "insert into trigram_meta (key, value) values ('base_max_id', ?1)
         on conflict(key) do update set value = excluded.value",
        params![base_max],
    )?;
    tx.commit()?;
    Ok(base_max)
}

/// Candidate message ids for the structured prefilter `groups` (OR of AND-groups, as produced by
/// [`crate::trigram::trigram_prefilter_groups`]): within a group every trigram's postings are
/// intersected (AND); across groups the results are unioned (OR). Only ids ≤ `base_max_id` are
/// returned (that is all the base covers); the caller adds the delta (`ids > base_max_id`).
pub fn candidates(conn: &Connection, groups: &[Vec<String>]) -> Result<HashSet<i64>> {
    let mut stmt = conn.prepare("select ids from trigram_postings where tg = ?1")?;
    let mut result: HashSet<i64> = HashSet::new();
    for group in groups {
        let mut acc: Option<Vec<i64>> = None;
        for tg in group {
            let ids: Vec<i64> = stmt
                .query_row(params![tg], |r| r.get::<_, Vec<u8>>(0))
                .ok()
                .map(|blob| decode_ids(&blob))
                .unwrap_or_default();
            acc = Some(match acc {
                None => ids,
                Some(prev) => intersect_sorted(&prev, &ids),
            });
            if acc.as_ref().is_some_and(|a| a.is_empty()) {
                break; // a required trigram is absent → this AND-group matches nothing
            }
        }
        if let Some(ids) = acc {
            result.extend(ids);
        }
    }
    Ok(result)
}

/// Parallel map-reduce: distinct lowercased 3-char trigrams of each doc → sorted unique ids.
fn build_postings(rows: &[(i64, String)]) -> Vec<(String, Vec<i64>)> {
    use rayon::prelude::*;
    let mut map: HashMap<String, Vec<i64>> = rows
        .par_iter()
        .fold(
            HashMap::new,
            |mut acc: HashMap<String, Vec<i64>>, (id, content)| {
                for tg in doc_trigrams(content) {
                    acc.entry(tg).or_default().push(*id);
                }
                acc
            },
        )
        .reduce(HashMap::new, |mut a, b| {
            for (k, mut v) in b {
                a.entry(k).or_default().append(&mut v);
            }
            a
        });
    map.par_iter_mut().for_each(|(_, v)| {
        v.sort_unstable();
        v.dedup();
    });
    map.into_iter().collect()
}

/// Distinct lowercased 3-character trigrams of `content` (matches [`crate::trigram`] tokenization).
fn doc_trigrams(content: &str) -> HashSet<String> {
    let chars: Vec<char> = content.to_lowercase().chars().collect();
    let mut set = HashSet::new();
    if chars.len() >= 3 {
        for w in chars.windows(3) {
            set.insert(w.iter().collect());
        }
    }
    set
}

/// Intersection of two ascending-sorted id lists (two-pointer).
fn intersect_sorted(a: &[i64], b: &[i64]) -> Vec<i64> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    out
}

/// Delta + LEB128 varint encode an ascending-sorted id list (the compact on-disk postings form).
fn encode_ids(ids: &[i64]) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut prev = 0i64;
    for &id in ids {
        let mut delta = (id - prev) as u64;
        prev = id;
        loop {
            let byte = (delta & 0x7f) as u8;
            delta >>= 7;
            if delta == 0 {
                buf.push(byte);
                break;
            }
            buf.push(byte | 0x80);
        }
    }
    buf
}

/// Inverse of [`encode_ids`].
fn decode_ids(buf: &[u8]) -> Vec<i64> {
    let mut out = Vec::new();
    let mut prev = 0i64;
    let mut cur = 0u64;
    let mut shift = 0u32;
    for &byte in buf {
        cur |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            prev += cur as i64;
            out.push(prev);
            cur = 0;
            shift = 0;
        } else {
            shift += 7;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trigram::trigram_prefilter_groups;

    fn seed(rows: &[(i64, &str)]) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "create table messages (id integer primary key, content text not null);",
        )
        .unwrap();
        for (id, content) in rows {
            conn.execute(
                "insert into messages (id, content) values (?1, ?2)",
                params![id, content],
            )
            .unwrap();
        }
        conn
    }

    #[test]
    fn encode_decode_roundtrip() {
        for ids in [
            vec![],
            vec![0i64],
            vec![1, 2, 3, 100, 100_000, 633_719],
            (0..1000).map(|i| i * 7).collect::<Vec<_>>(),
        ] {
            assert_eq!(decode_ids(&encode_ids(&ids)), ids);
        }
    }

    #[test]
    fn build_then_query_returns_superset_candidates() {
        let conn = seed(&[
            (1, "the deploy hit ECONNRESET again"),
            (2, "you forgot the tests"),
            (3, "totally unrelated content here"),
            (4, "another econnreset in the logs"),
        ]);
        let base_max = build(&conn).unwrap();
        assert_eq!(base_max, 4);
        assert_eq!(base_max_id(&conn).unwrap(), 4);

        // Single literal -> rows containing all its trigrams (case-insensitive).
        let groups = trigram_prefilter_groups("ECONNRESET").unwrap();
        let cands = candidates(&conn, &groups).unwrap();
        assert!(cands.contains(&1) && cands.contains(&4), "got {cands:?}");
        assert!(!cands.contains(&3), "unrelated row must not be a candidate");

        // Alternation -> union of groups.
        let groups = trigram_prefilter_groups("econnreset|forgot the").unwrap();
        let cands = candidates(&conn, &groups).unwrap();
        assert!(cands.contains(&1) && cands.contains(&2) && cands.contains(&4));
        assert!(!cands.contains(&3));
    }

    #[test]
    fn candidates_are_a_true_superset_of_regex_matches() {
        // The keystone: every row the regex matches must be in the candidate set.
        let rows: &[(i64, &str)] = &[
            (1, "please stop doing that"),
            (2, "we also need integration coverage"),
            (3, "nothing to see"),
            (4, "you (deleted) my helper"),
            (5, "STOP DOING this now"),
        ];
        let conn = seed(rows);
        build(&conn).unwrap();
        for pat in [r"\bstop doing\b", r"\balso need\b", "deleted"] {
            let re = regex::Regex::new(&format!("(?i){pat}")).unwrap();
            let groups = trigram_prefilter_groups(pat).unwrap();
            let cands = candidates(&conn, &groups).unwrap();
            for (id, content) in rows {
                if re.is_match(content) {
                    assert!(
                        cands.contains(id),
                        "SUPERSET VIOLATION: row {id} {content:?} matches {pat:?} but is not a candidate ({cands:?})",
                    );
                }
            }
        }
    }

    #[test]
    fn rebuild_replaces_and_updates_base_max() {
        let conn = seed(&[(1, "first econnreset")]);
        assert_eq!(build(&conn).unwrap(), 1);
        conn.execute(
            "insert into messages (id, content) values (7, 'later econnreset row')",
            [],
        )
        .unwrap();
        // Before rebuild, the base still only covers id 1.
        let groups = trigram_prefilter_groups("econnreset").unwrap();
        assert!(!candidates(&conn, &groups).unwrap().contains(&7));
        // After rebuild, base_max advances and id 7 is covered.
        assert_eq!(build(&conn).unwrap(), 7);
        assert!(candidates(&conn, &groups).unwrap().contains(&7));
    }
}
