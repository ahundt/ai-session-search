//! Byte-offset incremental tail-parse for append-only session logs (plan §7).
//!
//! Session JSONL files are append-only. After indexing a file up to a byte offset, a later
//! reindex of the GROWN file re-reads + re-parses the whole prefix only to discard it. This
//! module reads ONLY the appended bytes: it seeks to the stored checkpoint offset and parses
//! the tail, REUSING the provider's real per-line parser (`parse_reader`) over an in-memory byte
//! slice — so there is no second, drifting copy of the parse logic, and correctness is captured
//! by a differential test (a tail parse after an append == a full parse of the final file).
//!
//! Safety is the caller's (indexer's) responsibility and is layered: the fast path runs ONLY
//! when the file grew, the stored offset lies within it (no truncation), and a fingerprint of
//! the file head is unchanged (no rewrite/rotation) — otherwise a full parse. The tool-call-id →
//! tool-name map that spans lines (claude/codex/cursor) is rebuilt here by re-reading a bounded
//! backward OVERLAP before the offset, so a tool_result at the start of the tail whose tool_use
//! is within the overlap is still tagged correctly. The new rows are the SUFFIX of the
//! overlap+tail parse after the rows the overlap alone produces — which, because parsing is a
//! left-to-right per-line fold over identical leading bytes, is exactly the messages from the
//! newly appended lines.

use std::fs::File;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::Result;

use crate::models::{FileEdit, Message, ParsedSession, SessionRecord};

/// Bytes of the file head folded into the rewrite/rotation fingerprint. Matches Filebeat's
/// default fingerprint length: large enough to be unique per session (the head carries the
/// session id / first timestamp), small enough to read cheaply.
const FINGERPRINT_LEN: usize = 4096;

/// Backward overlap re-read before the checkpoint offset, to rebuild the tool-call-id → name
/// map for tool results at the start of the tail. 1 MiB is far more than the ~1-line gap
/// between a tool call and its result in practice; a tool whose result lands >1 MiB later (never
/// observed) would merely lose its `tool_name` label (its content is still indexed), self-healing
/// on the next full parse.
const OVERLAP_BYTES: i64 = 1 << 20;

/// Deterministic FNV-1a over `bytes`, hex-encoded. Stable across runs/versions (unlike
/// `DefaultHasher`), so a fingerprint persisted in one run is comparable in the next.
fn fnv1a_hex(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Read up to `buf.len()` bytes, looping over short reads (a `File` may return fewer bytes than
/// requested). Returns how many bytes were filled.
fn read_up_to<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = reader.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// Fingerprint of the file's first [`FINGERPRINT_LEN`] bytes (fewer if the file is shorter),
/// encoded as `len:hash`. Stored with the checkpoint and later compared via [`fingerprint_matches`]
/// — which re-hashes the SAME `len` bytes of the current file, so append-only growth (which only
/// adds bytes beyond `len`) leaves the fingerprint matching at any file size, while a rewritten
/// head changes it.
pub fn prefix_fingerprint(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; FINGERPRINT_LEN];
    let n = read_up_to(&mut file, &mut buf)?;
    Ok(format!("{n}:{}", fnv1a_hex(&buf[..n])))
}

/// True when the current file's first `len` bytes (where `len:hash` is a previously stored
/// [`prefix_fingerprint`]) still hash to `hash` — i.e. the file head is unchanged (an append, not
/// a rewrite/rotation). False on a malformed stored value, or if the file is now shorter than
/// `len` (the head was truncated). Append-stable: comparing over the STORED `len` ignores any
/// bytes added beyond it.
pub fn fingerprint_matches(path: &Path, stored: &str) -> Result<bool> {
    let Some((len_str, hash)) = stored.split_once(':') else {
        return Ok(false);
    };
    let Ok(len) = len_str.parse::<usize>() else {
        return Ok(false);
    };
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; len];
    let n = read_up_to(&mut file, &mut buf)?;
    if n < len {
        return Ok(false);
    }
    Ok(fnv1a_hex(&buf[..len]) == hash)
}

/// Offset just past the last `\n` in the file — the boundary up to which every line is COMPLETE.
/// Bytes after it are a partially written trailing line (a mid-flush append) and must not be
/// parsed yet. 0 when the file contains no newline. Reads only the file's tail, backward in
/// small chunks, so it is cheap even for a multi-GB file.
pub fn complete_prefix_offset(path: &Path) -> Result<i64> {
    let mut file = File::open(path)?;
    let size = file.seek(SeekFrom::End(0))?;
    complete_prefix_offset_inner(&mut file, size)
}

fn complete_prefix_offset_inner(file: &mut File, size: u64) -> Result<i64> {
    if size == 0 {
        return Ok(0);
    }
    let mut pos = size;
    let mut buf = [0u8; 8192];
    while pos > 0 {
        let chunk = pos.min(buf.len() as u64);
        pos -= chunk;
        file.seek(SeekFrom::Start(pos))?;
        let slice = &mut buf[..chunk as usize];
        file.read_exact(slice)?;
        if let Some(rel) = slice.iter().rposition(|&b| b == b'\n') {
            return Ok((pos + rel as u64 + 1) as i64);
        }
    }
    Ok(0)
}

/// New rows discovered by an incremental tail parse (to be appended by `Db::append_tail`).
pub struct TailParse {
    pub new_messages: Vec<Message>,
    pub new_file_edits: Vec<FileEdit>,
    /// Session metadata re-derived over the [overlap, EOF) window. Used to advance the volatile
    /// fields (updated_at / last_message_at, and a best-effort title/preview/cwd); the immutable
    /// fields (created_at, summary/first-user) stay as stored.
    pub session: SessionRecord,
    /// The newly appended human-transcript text (to append to the stored transcript blob and the
    /// session-level FTS). The suffix of the overlap+tail transcript after the overlap's, so it
    /// carries exactly the conversation lines from the appended messages.
    pub new_transcript: String,
    /// The new complete-line boundary to persist as the checkpoint offset.
    pub new_tail_offset: i64,
    /// The file-head fingerprint to persist with the checkpoint.
    pub new_fingerprint: String,
}

/// Parse ONLY the bytes appended to `path` after `checkpoint_offset`, using `parse_slice` (the
/// provider's real parser run over an in-memory `Cursor`).
///
/// Returns `Ok(None)` when no new COMPLETE line has been appended yet (only a partial, still-
/// being-written line) — the caller should skip the file, there is nothing new to index. The
/// caller is responsible for the truncation / fingerprint preconditions BEFORE calling this.
pub fn tail_parse<F>(
    path: &Path,
    checkpoint_offset: i64,
    parse_slice: F,
) -> Result<Option<TailParse>>
where
    F: Fn(Cursor<Vec<u8>>, &Path) -> Result<ParsedSession>,
{
    let mut file = File::open(path)?;
    let size = file.seek(SeekFrom::End(0))?;
    let new_offset = complete_prefix_offset_inner(&mut file, size)?;
    let new_fingerprint = prefix_fingerprint(path)?;
    // No new complete line since the checkpoint → nothing to append yet.
    if new_offset <= checkpoint_offset {
        return Ok(None);
    }

    // Re-read a bounded overlap before the checkpoint so the tool-id→name map (and any other
    // short-range cross-line state) is rebuilt for tool results at the start of the tail. Both
    // slices begin at the SAME `overlap_start`, so they skip any identical leading partial-line
    // fragment identically; the overlap parse's message count is therefore exactly the number of
    // overlap+tail messages that precede the newly appended lines.
    let overlap_start = (checkpoint_offset - OVERLAP_BYTES).max(0);
    let prefix_len = (checkpoint_offset - overlap_start) as usize;
    let all_len = (new_offset - overlap_start) as usize;

    file.seek(SeekFrom::Start(overlap_start as u64))?;
    let mut buf = vec![0u8; all_len];
    file.read_exact(&mut buf)?;

    let parsed_all = parse_slice(Cursor::new(buf.clone()), path)?;
    let parsed_overlap = parse_slice(Cursor::new(buf[..prefix_len].to_vec()), path)?;

    let m0 = parsed_overlap.messages.len();
    let e0 = parsed_overlap.file_edits.len();
    let new_messages = parsed_all.messages.get(m0..).unwrap_or(&[]).to_vec();
    let new_file_edits = parsed_all.file_edits.get(e0..).unwrap_or(&[]).to_vec();

    // The overlap transcript is a prefix of the overlap+tail transcript (both join the same
    // leading conversation lines), so the appended transcript is the remainder with the leading
    // "\n\n" join separator trimmed.
    let new_transcript = parsed_all
        .transcript_text
        .strip_prefix(&parsed_overlap.transcript_text)
        .unwrap_or(&parsed_all.transcript_text)
        .trim_start_matches('\n')
        .to_string();

    Ok(Some(TailParse {
        new_messages,
        new_file_edits,
        session: parsed_all.session,
        new_transcript,
        new_tail_offset: new_offset,
        new_fingerprint,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write(path: &Path, bytes: &[u8]) {
        let mut f = File::create(path).unwrap();
        f.write_all(bytes).unwrap();
    }

    #[test]
    fn complete_prefix_offset_stops_at_last_newline() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("f");
        // Ends with a newline → the whole file is complete lines.
        write(&p, b"line one\nline two\n");
        assert_eq!(complete_prefix_offset(&p).unwrap(), 18);
        // A partial trailing line (no newline) → boundary is just past the previous newline.
        write(&p, b"line one\nline two\npar");
        assert_eq!(complete_prefix_offset(&p).unwrap(), 18);
        // No newline at all → no complete line yet.
        write(&p, b"partial only");
        assert_eq!(complete_prefix_offset(&p).unwrap(), 0);
        // Empty file.
        write(&p, b"");
        assert_eq!(complete_prefix_offset(&p).unwrap(), 0);
    }

    #[test]
    fn fingerprint_matches_across_appends_but_not_head_rewrites() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("f");
        write(&p, b"HEADER line\nbody\n");
        let fp1 = prefix_fingerprint(&p).unwrap();
        // Append only → still matches (the stored head bytes are unchanged), even though the file
        // is now SMALLER than FINGERPRINT_LEN so the covered length grew.
        write(&p, b"HEADER line\nbody\nmore\n");
        assert!(
            fingerprint_matches(&p, &fp1).unwrap(),
            "append must keep the head fingerprint matching"
        );
        // Rewrite the head → no longer matches (rotation / different file at the same path).
        write(&p, b"DIFFERENT!!!\nbody\nmore\n");
        assert!(
            !fingerprint_matches(&p, &fp1).unwrap(),
            "a head rewrite must break the match"
        );
        // Truncated below the fingerprinted length → no longer matches (head bytes are gone).
        write(&p, b"HEAD");
        assert!(
            !fingerprint_matches(&p, &fp1).unwrap(),
            "truncation below the fingerprint must not match"
        );
    }
}
