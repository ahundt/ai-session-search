//! Cross-provider regression lock: one transcript file binds to exactly one session id.
//!
//! `sessions` is upserted with `on conflict(id) do update set ...` (db.rs), so two files that
//! parse to the SAME `provider_session_id` do not both get stored -- the second silently
//! overwrites the first, including its `source_path`. There is no error, no parse warning, and
//! no stale entry, because every health signal in the index is keyed by a session that was
//! successfully indexed. A file that produces no row is invisible.
//!
//! That is not hypothetical. Codex rollouts for forked/resumed sessions carry a SECOND
//! `session_meta` line holding the PARENT's id. The adapter bound the id last-wins, so each
//! fork overwrote its parent: 414 discovered codex files produced 349 rows, and
//! `get_index_status` reported the index healthy. Measured on real data 2026-07-24, the 65
//! missing files were exactly the 65 with more than one `session_meta` line.
//!
//! Each provider below reads an id out of file CONTENT, so each can regress the same way.
//!
//! Distinctness alone is NOT a sufficient assertion, and asserting only that was this file's
//! own first bug: when two transcripts each name the other, a last-wins binding SWAPS their
//! ids, which keeps the set size at N while pointing every row at the wrong session. Each test
//! therefore asserts the id each file actually binds to, by owner, not merely that the ids
//! differ. The swap case is covered explicitly for claude and pi; codex covers the collapse
//! case, where two forks of one parent both take the parent's id.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use ai_session_search::providers::claude::ClaudeAdapter;
use ai_session_search::providers::codex::CodexAdapter;
use ai_session_search::providers::pi::PiAdapter;
use tempfile::tempdir;

/// Assert every transcript bound to the id it belongs to. `parsed` is (expected, actual) pairs.
fn assert_bound_to_owner(label: &str, parsed: &[(String, String)]) {
    let wrong: Vec<_> = parsed
        .iter()
        .filter(|(expected, actual)| expected != actual)
        .collect();
    assert!(
        wrong.is_empty(),
        "{label}: {} of {} transcripts bound to a session they do not own {wrong:?}; the \
         on-conflict upsert would overwrite that session's row instead of storing this one",
        wrong.len(),
        parsed.len()
    );
    let distinct: std::collections::HashSet<_> =
        parsed.iter().map(|(_, actual)| actual.clone()).collect();
    assert_eq!(
        distinct.len(),
        parsed.len(),
        "{label}: transcripts collapsed onto {} distinct id(s)",
        distinct.len()
    );
}

#[test]
fn codex_forked_rollouts_each_keep_a_distinct_session_id() {
    let temp = tempdir().unwrap();
    let root: PathBuf = temp.path().to_path_buf();
    let parent = "019f4a0e-d714-7cc3-8003-543a04a1a821";
    // Two forks of the same parent, plus the parent itself: the exact real-world shape.
    let forks = [
        "019f5ce8-afff-76a3-86f2-174307f3fa07",
        "019f5ce8-c6d9-7f03-9a54-d1b18dddba08",
    ];
    // path -> the session that path's file belongs to, recorded as each file is written.
    let mut owners: HashMap<PathBuf, &str> = HashMap::new();

    let parent_path = root.join(format!("rollout-2026-07-10T03-25-14-{parent}.jsonl"));
    fs::write(
        &parent_path,
        format!(
            r#"{{"type":"session_meta","payload":{{"session_id":"{parent}","id":"{parent}","timestamp":"2026-07-10T03:25:14.410Z","cwd":"/tmp/parent"}}}}
{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"text","text":"parent turn"}}]}}}}
"#
        ),
    )
    .unwrap();
    owners.insert(parent_path, parent);

    for fork in forks {
        let fork_path = root.join(format!("rollout-2026-07-13T15-16-21-{fork}.jsonl"));
        fs::write(
            &fork_path,
            format!(
                r#"{{"type":"session_meta","payload":{{"session_id":"{parent}","id":"{fork}","forked_from_id":"{parent}","timestamp":"2026-07-13T19:16:21.181Z","cwd":"/tmp/fork"}}}}
{{"type":"session_meta","payload":{{"session_id":"{parent}","id":"{parent}","timestamp":"2026-07-10T03:25:14.410Z","cwd":"/tmp/parent"}}}}
{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"text","text":"fork turn"}}]}}}}
"#
            ),
        )
        .unwrap();
        owners.insert(fork_path, fork);
    }

    let adapter = CodexAdapter::new(vec![root], temp.path().join("nonexistent-home"));
    let sources = adapter.discover();
    assert_eq!(sources.len(), 3);
    // Owner comes from the write-side map above, not from re-parsing the filename, so the
    // assertion cannot drift with the rollout naming convention.
    let parsed: Vec<(String, String)> = sources
        .iter()
        .map(|source| {
            let owner = owners
                .get(source.path.as_path())
                .unwrap_or_else(|| panic!("discovered an unexpected file: {:?}", source.path));
            (
                (*owner).to_string(),
                adapter.parse(source).session.provider_session_id,
            )
        })
        .collect();
    assert_bound_to_owner("codex", &parsed);
}

#[test]
fn claude_transcripts_each_keep_a_distinct_session_id() {
    let temp = tempdir().unwrap();
    let root: PathBuf = temp.path().to_path_buf().join("-tmp-proj");
    fs::create_dir_all(&root).unwrap();
    let ids = [
        "2e55571b-9383-4fbc-b84f-19bbd779ed26",
        "3f260ba5-b3b0-4e79-9488-b9832ded0835",
    ];
    // Each file's trailing record names the OTHER session, the shape that lets a last-wins
    // binding retarget a whole file onto a session it merely mentions.
    for (i, id) in ids.iter().enumerate() {
        let other = ids[(i + 1) % ids.len()];
        fs::write(
            root.join(format!("{id}.jsonl")),
            format!(
                r#"{{"type":"user","sessionId":"{id}","cwd":"/tmp/proj","message":{{"role":"user","content":"first turn"}}}}
{{"type":"user","sessionId":"{other}","cwd":"/tmp/proj","message":{{"role":"user","content":"quoting the other session"}}}}
"#
            ),
        )
        .unwrap();
    }

    let adapter = ClaudeAdapter::new(vec![temp.path().to_path_buf()]);
    let sources = adapter.discover();
    assert_eq!(sources.len(), 2);
    let parsed: Vec<(String, String)> = sources
        .iter()
        .map(|source| {
            (
                source
                    .path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap()
                    .to_string(),
                adapter.parse(source).session.provider_session_id,
            )
        })
        .collect();
    assert_bound_to_owner("claude", &parsed);
}

#[test]
fn pi_transcripts_each_keep_a_distinct_session_id() {
    let temp = tempdir().unwrap();
    let root: PathBuf = temp.path().to_path_buf();
    // Real UUID shape, so PiAdapter::extract_id resolves the filename fallback too.
    let ids = [
        "3f260ba5-b3b0-4e79-9488-b9832ded0835",
        "0b3fdcac-1453-4891-9c43-9f2e1a2fb8c3",
    ];
    for (i, id) in ids.iter().enumerate() {
        let other = ids[(i + 1) % ids.len()];
        fs::write(
            root.join(format!("{id}.jsonl")),
            format!(
                r#"{{"type":"session","id":"{id}","cwd":"/tmp/proj","timestamp":"2026-07-13T19:16:21.181Z"}}
{{"type":"session","id":"{other}","cwd":"/tmp/other","timestamp":"2026-07-13T19:20:00.000Z"}}
"#
            ),
        )
        .unwrap();
    }

    let adapter = PiAdapter::new(vec![root]);
    let sources = adapter.discover();
    assert_eq!(sources.len(), 2);
    let parsed: Vec<(String, String)> = sources
        .iter()
        .map(|source| {
            (
                source
                    .path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap()
                    .to_string(),
                adapter.parse(source).session.provider_session_id,
            )
        })
        .collect();
    assert_bound_to_owner("pi", &parsed);
}
