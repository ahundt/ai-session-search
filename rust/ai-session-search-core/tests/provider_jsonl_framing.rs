// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

use ai_session_search::{
    models::{Provider, SourceFile},
    providers::{
        antigravity::AntigravityAdapter, claude::ClaudeAdapter, codex::CodexAdapter,
        cursor::CursorAdapter, gemini_cli::GeminiCliAdapter, pi::PiAdapter,
    },
};

#[test]
fn non_json_whitespace_is_trimmed_only_by_antigravity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("synthetic.jsonl");
    let input = "\u{a0}{}\u{a0}\n";
    std::fs::write(&path, input).unwrap();
    for provider in [
        Provider::Claude,
        Provider::Codex,
        Provider::Pi,
        Provider::Cursor,
        Provider::Antigravity,
        Provider::GeminiCli,
    ] {
        let source = SourceFile::new(provider, path.clone(), 0, input.len() as i64);
        let parsed = match provider {
            Provider::Claude => ClaudeAdapter::new(vec![]).parse(&source),
            Provider::Codex => CodexAdapter::new(vec![]).parse(&source),
            Provider::Pi => PiAdapter::new(vec![]).parse(&source),
            Provider::Cursor => CursorAdapter::new(vec![]).parse(&source),
            Provider::Antigravity => AntigravityAdapter::new(vec![]).parse(&source),
            Provider::GeminiCli => GeminiCliAdapter::new(vec![]).parse(&source),
            _ => unreachable!(),
        };
        let metadata: serde_json::Value =
            serde_json::from_str(parsed.session.raw_metadata_json.as_deref().unwrap()).unwrap();
        let valid = metadata["valid_record_count"].as_u64().unwrap_or(0);
        let malformed = metadata["malformed_line_count"].as_u64().unwrap_or(0);
        assert_eq!(metadata["line_count"], 1, "{provider:?}");
        if provider == Provider::Antigravity {
            assert_eq!((valid, malformed), (1, 0));
            assert_eq!(parsed.session.parse_warning, None);
        } else {
            assert_eq!((valid, malformed), (0, 1), "{provider:?}");
            assert_eq!(
                parsed.session.parse_warning.as_deref(),
                Some("skipped 1 malformed JSONL record"),
                "{provider:?}"
            );
        }
    }
}
