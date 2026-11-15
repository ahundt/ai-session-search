// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

//! One case table, every message-search interface.
//!
//! `fixtures/search_surface_parity.json` holds the rows and the cases; the Python suite reads the
//! same file. Each shared case states an explicit `limit` or `all_results`, because an omitted
//! limit deliberately means different things per surface (REQ003), and compares result identity
//! and order as `[seq, role]` only, because presentation is deliberately surface-sized (REQ004).
//! The two documented differences are asserted separately so a change to either side fails.
//!
//! Interfaces reached here: `MessageService` on each `SearchSurface`, the `aise` binary's
//! `messages search` and `messages get`, and its MCP server's `search_messages` and `get_session`
//! over JSON-RPC. `tests/test_native_binding.py` runs the same fixture through the Python binding
//! and the Python entry point.
//!
//! Searches cover content and, through the fixture's tool rows, the derived `tool_name` field,
//! which has its own query path (`Db::search_derived_message_field`). Reads cover a seq held by
//! several rows and a newest-first cut inside a tie.
//!
//! What fails without which fix, measured by reverting it:
//! - the trigram prefilter superset (Cherokee by either spelling, regex included, and `abi`
//!   against `abİ`): 24 checks, every case on all six search interfaces here;
//! - the fuzzy ranking's stream-position tie-breaker: the fuzzy walks over the 40 tied pairs.
//!
//! The SQL row-id tie-breaker is not observable at this size: reverting it from
//! `MessageOrder::order_by_clause` leaves every case here passing, because SQLite serves these
//! reads by walking `idx_messages_session_seq`, which already orders a tie. Those cases are parity
//! checks. The plans that do expose the defect are reproduced by unit tests in `db.rs`:
//! `the_newest_window_selects_exactly_the_tail_of_the_oldest_order`,
//! `find_corrections_orders_rows_tied_on_every_sort_key_by_row_id_under_any_filter` and
//! `attributable_human_search_orders_tied_rows_by_row_id_under_any_filter`.
//!
//! Deliberate differences, asserted rather than compared:
//! - An omitted limit returns every literal, regex or no-text match on Rust, CLI and Python, and a
//!   configured finite page (15 by default) on MCP, whose results enter an agent's context
//!   directly and whose clients cap tool output, some silently. REQ003 in
//!   `docs/development/maintainer-requirements-and-design-decisions.md`; the caps and their
//!   evidence are in `docs/development/mcp-client-limits-and-measured-evidence.md`.
//! - Fuzzy search keeps a finite page everywhere and refuses `all_results` everywhere.
//! - Presentation (field and match views, `[mcp].max_tool_result_chars`) is surface-sized and
//!   never changes membership or order (REQ004), so only `[seq, role]` is compared.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use ai_session_search::config::Config;
use ai_session_search::db::Db;
use ai_session_search::message_search::{MatchWindow, SearchSurface};
use ai_session_search::models::{
    Message, MessageKind, ParsedSession, Provider, Role, SessionRecord,
};
use ai_session_search::service::MessageService;
use ai_session_search::{MessageQuery, MessageSearchRequest, MessageTarget, RequestedExtent};
use serde_json::{json, Value};

mod support;

const FIXTURE: &str = include_str!("fixtures/search_surface_parity.json");

#[test]
fn filtered_range_reads_share_event_dates_role_and_newest_order() {
    use ai_session_search::db::MessageOrder;
    use ai_session_search::models::MessageFilters;
    let mut f = fixture();
    let id = "claude:warning";
    f.spec["session_id"] = json!(id);
    let db = Db::open(&f.db_path).unwrap();
    let mut first = Message::new(0, Role::Assistant, "first");
    first.ts = ai_session_search::util::parse_datetime("2026-01-01T00:00:00Z");
    let mut second = Message::new(1, Role::Assistant, "second");
    second.ts = ai_session_search::util::parse_datetime("2026-02-01T00:00:00Z");
    db.upsert_session(
        &ParsedSession::new(
            SessionRecord::new(id, Provider::Claude, "warning", "/warning", "test", "test"),
            vec![first, second],
        ),
        0,
        0,
    )
    .unwrap();
    let filters = MessageFilters::default()
        .with_session_id(id)
        .with_role(Role::Assistant)
        .with_since(ai_session_search::util::parse_datetime("2026-02-01T00:00:00Z").unwrap())
        .with_limit(1);
    let config = Config::default();
    let service = MessageService::new(&config, &db, SearchSurface::Rust);
    let expected = vec![(1, "assistant".to_string())];
    assert_eq!(
        hit_ids(
            service
                .read_session(&filters, MessageOrder::NewestFirst)
                .unwrap()
        ),
        expected
    );
    assert_eq!(
        cli_read(
            &f,
            &[
                "--role".into(),
                "assistant".into(),
                "--since".into(),
                "2026-02-01T00:00:00Z".into(),
                "--limit".into(),
                "1".into(),
                "--order".into(),
                "newest".into()
            ]
        ),
        expected
    );
    let response = mcp_calls(
        &f,
        &[(
            "get_session",
            json!({"session_id":id,
        "role":"assistant", "since":"2026-02-01T00:00:00Z", "limit":1, "order":"newest"}),
        )],
    )
    .remove(0)
    .unwrap();
    assert_eq!(response["session_id"], id);
    assert_eq!(read_rows(&response), expected);
    for filter in [
        json!({"when":"2026-02"}),
        json!({"since":"2026-02-01T00:00:00Z"}),
        json!({"until":"2026-01-31T23:59:59Z"}),
        json!({"role":"assistant"}),
        json!({"kinds":["conversation"]}),
    ] {
        let mut read = filter.clone();
        read["session_id"] = json!(id);

        let mut search = filter;
        search["session_id"] = json!(id);
        search["all_results"] = json!(true);
        let check_cli_kinds = read.get("kinds").is_some();
        let mut range = read_rows(&mcp_calls(&f, &[("get_session", read)]).remove(0).unwrap());
        if check_cli_kinds {
            assert_eq!(
                cli_read(&f, &["--kinds".into(), "conversation".into()]),
                range
            );
        }
        let result = mcp_calls(&f, &[("search_messages", search)])
            .remove(0)
            .unwrap();
        let mut matches = ids_from_results(&result["results"]);
        range.sort();
        matches.sort();
        assert!(!range.is_empty());
        assert_eq!(matches, range);
    }
    for conflict in [
        json!({"seq":0}),
        json!({"summary":true}),
        json!({"transcript_lines":10}),
        json!({"until":"2026-03"}),
    ] {
        let mut args = conflict.clone();
        args["session_id"] = json!(id);
        args["when"] = json!("2026-02");
        let error = mcp_calls(&f, &[("get_session", args)])
            .remove(0)
            .unwrap_err();
        assert!(error.contains("when"), "{error}");
        assert!(
            error.contains(conflict.as_object().unwrap().keys().next().unwrap()),
            "{error}"
        );
    }
}

#[test]
fn message_session_classes_filter_before_paging_on_every_surface() {
    let f = fixture();
    let db = Db::open(&f.db_path).unwrap();
    for (id, parent) in [
        ("claude:class-root-a", None),
        ("claude:class-root-b", None),
        ("claude:class-child-a", Some("claude:class-root-a")),
        ("claude:class-child-b", Some("claude:class-root-b")),
        ("claude:class-nested", Some("claude:class-child-a")),
    ] {
        let mut session = SessionRecord::new(
            id,
            Provider::Claude,
            id,
            format!("/synthetic/classes/{id}"),
            "test",
            "test",
        );
        session.parent_session_id = parent.map(str::to_owned);
        session.cwd = Some("/synthetic/workspace".into());
        db.upsert_session(
            &ParsedSession::new(
                session,
                (0..2)
                    .map(|seq| {
                        let mut message = Message::new(seq, Role::Assistant, "class needle");
                        message.ts =
                            ai_session_search::util::parse_datetime("2026-01-15T00:00:00Z");
                        message
                    })
                    .collect(),
            ),
            0,
            0,
        )
        .unwrap();
    }
    let identities = |value: &Value| -> Vec<(String, i64)> {
        value["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|hit| {
                (
                    hit["message_ref"]["session_id"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                    hit["message_ref"]["message_seq"].as_i64().unwrap(),
                )
            })
            .collect()
    };
    for (selector, expected) in [
        (
            json!({"session_kinds":["subagent"]}),
            vec![
                ("claude:class-child-a".to_owned(), 0),
                ("claude:class-child-a".to_owned(), 1),
                ("claude:class-child-b".to_owned(), 0),
                ("claude:class-child-b".to_owned(), 1),
                ("claude:class-nested".to_owned(), 0),
                ("claude:class-nested".to_owned(), 1),
            ],
        ),
        (
            json!({"parent_session_id":"claude:class-root-a"}),
            vec![
                ("claude:class-child-a".to_owned(), 0),
                ("claude:class-child-a".to_owned(), 1),
            ],
        ),
        (
            json!({"session_kinds":["user"]}),
            vec![
                ("claude:class-root-a".to_owned(), 0),
                ("claude:class-root-a".to_owned(), 1),
                ("claude:class-root-b".to_owned(), 0),
                ("claude:class-root-b".to_owned(), 1),
            ],
        ),
        (
            json!({"parent_session_id":"claude:class-root-a","session_kinds":["subagent"]}),
            vec![
                ("claude:class-child-a".to_owned(), 0),
                ("claude:class-child-a".to_owned(), 1),
            ],
        ),
        (json!({"session_kinds":[]}), vec![]),
    ] {
        for mode in ["literal", "regex", "fuzzy"] {
            let mut args = selector.clone();
            args["query"] = json!("class needle");
            args["query_mode"] = json!(mode);
            args["limit"] = json!(100);
            args["include"] = json!([]);
            args["role"] = json!("assistant");
            args["kinds"] = json!(["conversation"]);
            args["providers"] = json!(["claude"]);
            args["workspace_path_prefix"] = json!("/synthetic/workspace");
            args["transcript_path_prefix"] = json!("/synthetic/classes");
            args["receipt_level"] = json!("full");
            args["when"] = json!("2026-01");
            let result = mcp_calls(&f, &[("search_messages", args.clone())])
                .remove(0)
                .unwrap();
            assert_eq!(identities(&result), expected, "{selector}, {mode}");
            assert_eq!(
                result["receipt"]["search_explanation"]["corpus"],
                json!(expected.len())
            );
            if !expected.is_empty() {
                assert_eq!(
                    result["receipt"]["search_explanation"]["candidates"],
                    json!(expected.len())
                );
            }
            let config = Config::default();
            let service = MessageService::new(&config, &db, SearchSurface::Rust);
            let query = match mode {
                "literal" => MessageQuery::literal("class needle").unwrap(),
                "regex" => MessageQuery::regex("class needle").unwrap(),
                _ => MessageQuery::fuzzy("class needle").unwrap(),
            };
            let mut builder = MessageSearchRequest::builder(query, MessageTarget::content())
                .role(Role::Assistant)
                .kind(MessageKind::Conversation)
                .provider(Provider::Claude)
                .time(
                    ai_session_search::message_search::RequestedTimeRange::new(
                        ai_session_search::util::parse_datetime("2026-01-01T00:00:00Z"),
                        ai_session_search::util::parse_datetime("2026-01-31T23:59:59Z"),
                    )
                    .unwrap(),
                )
                .workspace_path_prefix("/synthetic/workspace")
                .unwrap()
                .transcript_path_prefix("/synthetic/classes")
                .unwrap()
                .extent(RequestedExtent::page(Some(100), 0).unwrap());
            let mut cli_args = vec![
                "messages".to_owned(),
                "search".into(),
                "class needle".into(),
                "--query-mode".into(),
                mode.into(),
                "--format".into(),
                "json".into(),
                "--limit".into(),
                "100".into(),
                "--role".into(),
                "assistant".into(),
                "--kinds".into(),
                "conversation".into(),
                "--provider".into(),
                "claude".into(),
                "--workspace-path".into(),
                "/synthetic/workspace".into(),
                "--transcript-path".into(),
                "/synthetic/classes".into(),
                "--when".into(),
                "2026-01".into(),
            ];
            if let Some(kinds) = selector["session_kinds"].as_array() {
                builder = builder.session_kinds(
                    kinds
                        .iter()
                        .map(|kind| kind.as_str().unwrap().parse().unwrap())
                        .collect(),
                );
                if !kinds.is_empty() {
                    cli_args.extend([
                        "--session-kinds".into(),
                        kinds
                            .iter()
                            .map(|kind| kind.as_str().unwrap())
                            .collect::<Vec<_>>()
                            .join(","),
                    ]);
                }
            }
            if let Some(parent) = selector["parent_session_id"].as_str() {
                builder = builder.parent_session_id(parent).unwrap();
                cli_args.extend(["--parent-session".into(), parent.into()]);
            }
            let response = service.search(builder.build().unwrap()).unwrap();
            let rust_ids: Vec<_> = response
                .hits()
                .iter()
                .map(|hit| (hit.session_id.clone(), hit.message.seq))
                .collect();
            assert_eq!(rust_ids, expected);
            // CLI cannot spell an empty set; it intentionally requires at least one value.
            if selector["session_kinds"] != json!([]) {
                let output = aise(&f).args(&cli_args).output().unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(
                    identities(&serde_json::from_slice(&output.stdout).unwrap()),
                    expected
                );
            }
            let mut paged = Vec::new();
            for offset in 0..expected.len() {
                args["limit"] = json!(1);
                args["offset"] = json!(offset);
                paged.extend(identities(
                    &mcp_calls(&f, &[("search_messages", args.clone())])
                        .remove(0)
                        .unwrap(),
                ));
            }
            assert_eq!(paged, expected);
            if mode != "fuzzy" {
                args.as_object_mut().unwrap().remove("limit");
                args["offset"] = json!(0);
                args["all_results"] = json!(true);
                assert_eq!(
                    identities(
                        &mcp_calls(&f, &[("search_messages", args)])
                            .remove(0)
                            .unwrap()
                    ),
                    expected
                );
            }
        }
    }
    let config = Config::default();
    let service = MessageService::new(&config, &db, SearchSurface::Rust);
    let request = |parent: &str| {
        MessageSearchRequest::builder(
            MessageQuery::literal("class needle").unwrap(),
            MessageTarget::content(),
        )
        .parent_session_id(parent)
        .unwrap()
        .extent(RequestedExtent::all_results())
        .build()
        .unwrap()
    };
    assert_eq!(
        service
            .search(request("claude:class-root-a"))
            .unwrap()
            .hits()
            .len(),
        2
    );
    // Resolve parents before searching, even if the message query would match nothing.
    for parent in ["claude:class-root-", "claude:missing-parent"] {
        assert!(service.search(request(parent)).is_err());
        assert!(mcp_calls(
            &f,
            &[(
                "search_messages",
                json!({
                    "query":"class needle", "parent_session_id":parent,
                })
            )]
        )
        .remove(0)
        .is_err());
        assert!(!aise(&f)
            .args([
                "messages",
                "search",
                "class needle",
                "--parent-session",
                parent
            ])
            .output()
            .unwrap()
            .status
            .success());
    }
    for args in [
        json!({"session_kinds":["user"], "parent_session_id":"claude:class-root-a"}),
        json!({"session_kinds":["agent"]}),
        json!({"session_kinds":"subagent"}),
    ] {
        assert!(mcp_calls(&f, &[("search_messages", args)])
            .remove(0)
            .is_err());
    }
    assert!(
        MessageSearchRequest::builder(MessageQuery::All, MessageTarget::content())
            .session_kinds(vec![ai_session_search::models::SessionKind::User])
            .parent_session_id("claude:class-root-a")
            .unwrap()
            .build()
            .is_err()
    );
    let alias = aise(&f)
        .args([
            "messages",
            "search",
            "class needle",
            "--session-kind",
            "subagent",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(alias.status.success());
    assert_eq!(
        identities(&serde_json::from_slice(&alias.stdout).unwrap()).len(),
        6
    );
    let mut orphan = SessionRecord::new(
        "claude:surviving-child",
        Provider::Claude,
        "surviving-child",
        "/synthetic/orphan",
        "test",
        "test",
    );
    orphan.parent_session_id = Some("claude:absent-parent".into());
    db.upsert_session(
        &ParsedSession::new(
            orphan,
            vec![Message::new(0, Role::Assistant, "class needle")],
        ),
        0,
        0,
    )
    .unwrap();
    assert_eq!(
        service
            .search(request("claude:absent-parent"))
            .unwrap()
            .hits()
            .len(),
        1
    );
    assert_eq!(
        identities(
            &mcp_calls(
                &f,
                &[(
                    "search_messages",
                    json!({"query":"class needle", "parent_session_id":"claude:absent-parent"})
                )]
            )
            .remove(0)
            .unwrap()
        ),
        vec![("claude:surviving-child".into(), 0)]
    );
    let orphan_cli = aise(&f)
        .args([
            "messages",
            "search",
            "class needle",
            "--parent-session",
            "claude:absent-parent",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        orphan_cli.status.success(),
        "{}",
        String::from_utf8_lossy(&orphan_cli.stderr)
    );
    assert_eq!(
        identities(&serde_json::from_slice(&orphan_cli.stdout).unwrap()),
        vec![("claude:surviving-child".into(), 0)]
    );
    for (id, parent) in [
        ("claude:prefix-parent-long", None),
        ("claude:prefix-child", Some("claude:prefix-parent-long")),
    ] {
        let mut session = SessionRecord::new(
            id,
            Provider::Claude,
            id,
            format!("/synthetic/{id}"),
            "test",
            "test",
        );
        session.parent_session_id = parent.map(str::to_owned);
        db.upsert_session(
            &ParsedSession::new(
                session,
                vec![Message::new(0, Role::Assistant, "prefix needle")],
            ),
            0,
            0,
        )
        .unwrap();
    }
    let prefix_request = MessageSearchRequest::builder(
        MessageQuery::literal("prefix needle").unwrap(),
        MessageTarget::content(),
    )
    .parent_session_id("claude:prefix-parent-")
    .unwrap()
    .build()
    .unwrap();
    assert_eq!(service.search(prefix_request).unwrap().hits().len(), 1);
    let prefix_result = mcp_calls(
        &f,
        &[(
            "search_messages",
            json!({"query":"prefix needle","parent_session_id":"claude:prefix-parent-"}),
        )],
    )
    .remove(0)
    .unwrap();
    assert_eq!(
        identities(&prefix_result),
        vec![("claude:prefix-child".into(), 0)]
    );
    let prefix_cli = aise(&f)
        .args([
            "messages",
            "search",
            "prefix needle",
            "--parent-session",
            "claude:prefix-parent-",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(prefix_cli.status.success());
    assert_eq!(
        identities(&serde_json::from_slice(&prefix_cli.stdout).unwrap()),
        vec![("claude:prefix-child".into(), 0)]
    );
    let prefix = mcp_calls(
        &f,
        &[(
            "search_messages",
            json!({
                "query":"class needle", "parent_session_id":"claude:class-child-a",
            }),
        )],
    )
    .remove(0)
    .unwrap();
    assert_eq!(
        identities(&prefix),
        vec![
            ("claude:class-nested".into(), 0),
            ("claude:class-nested".into(), 1)
        ]
    );
}

#[test]
fn structured_context_preserves_tied_peers_and_exact_anchor_across_surfaces() {
    let f = fixture();
    let db = Db::open(&f.db_path).unwrap();
    let contents = [
        (0, "before first"),
        (0, "before second"),
        (1, "needle alpha"),
        (1, "needle beta"),
        (1, "unmatched peer"),
        (2, "after first"),
        (2, "after second"),
    ];
    let session = SessionRecord::new(
        "claude:context-ties",
        Provider::Claude,
        "context-ties",
        "/synthetic/context-ties",
        "test",
        "fixture",
    );
    db.upsert_session(
        &ParsedSession::new(
            session,
            contents
                .iter()
                .map(|(seq, content)| Message::new(*seq, Role::Assistant, *content))
                .collect(),
        ),
        0,
        0,
    )
    .unwrap();
    for query in ["needle", "alpha"] {
        let output = aise(&f)
            .args([
                "messages",
                "search",
                query,
                "--session-id",
                "claude:context-ties",
                "--context",
                "1",
                "--all-results",
                "--format",
                "json",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        let cli: Value = serde_json::from_slice(&output.stdout).unwrap();
        let mcp = mcp_calls(&f, &[("search_messages", json!({
            "query":query, "session_id":"claude:context-ties", "context":1, "all_results":true,
            "field_view":{"kind":"no_char_limit"}, "include":[],
        }))]).remove(0).unwrap();
        let cli_results = cli["results"].as_array().unwrap();
        let results = mcp["results"].as_array().unwrap();
        assert_eq!(results.len(), if query == "needle" { 2 } else { 1 });
        for (index, hit) in results.iter().enumerate() {
            assert_eq!(hit["context"], cli_results[index]["context"]);
            let before: Vec<_> = hit["context"]["messages_before"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["presentation"]["field_view"]["text"].as_str().unwrap())
                .collect();
            let after: Vec<_> = hit["context"]["messages_after"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["presentation"]["field_view"]["text"].as_str().unwrap())
                .collect();
            let anchor_index = 2 + index;
            assert_eq!(
                before,
                contents[..anchor_index]
                    .iter()
                    .map(|(_, text)| *text)
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                after,
                contents[anchor_index + 1..]
                    .iter()
                    .map(|(_, text)| *text)
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                hit["presentation"]["field_view"]["text"],
                contents[anchor_index].1
            );
            assert_eq!(before.len() + after.len() + 1, contents.len());
        }
    }
}

type Ids = Vec<(i64, String)>;

struct Fixture {
    _dir: tempfile::TempDir,
    db_path: PathBuf,
    config: PathBuf,
    spec: Value,
}

fn fixture() -> Fixture {
    let spec: Value = serde_json::from_str(FIXTURE).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("index.db");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let config = dir.path().join("config.toml");
    let _ = support::aise(dir.path());
    std::fs::copy(dir.path().join("isolated-config.toml"), &config).unwrap();

    let role = |name: &str| match name {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        other => panic!("fixture role {other}"),
    };
    let message = |seq: i64, role: Role, content: String| {
        let mut value = Message::new(seq, role, content);
        value.provenance = {
            let mut provenance = ai_session_search::models::MessageProvenance::default();
            provenance.authorship = if role == Role::User {
                ai_session_search::models::MessageAuthorship::Human
            } else {
                ai_session_search::models::MessageAuthorship::Agent
            };
            provenance.record_relation = ai_session_search::models::MessageRecordRelation::Original;
            provenance
        };
        value
    };
    let mut messages: Vec<Message> = spec["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            let mut message = message(
                m["seq"].as_i64().unwrap(),
                role(m["role"].as_str().unwrap()),
                m["content"].as_str().unwrap().to_string(),
            );
            if let Some(provenance) = m.get("provenance") {
                message.provenance = serde_json::from_value(provenance.clone()).unwrap();
            }
            if let Some(kind) = m.get("kind") {
                message.kind = serde_json::from_value(kind.clone()).unwrap();
            }
            message
        })
        .collect();
    let bulk = &spec["bulk"];
    for i in 0..bulk["count"].as_i64().unwrap() {
        messages.push(message(
            bulk["first_seq"].as_i64().unwrap() + i,
            role(bulk["role"].as_str().unwrap()),
            format!("{} {i}", bulk["content_prefix"].as_str().unwrap()),
        ));
    }
    let twins = &spec["twins"];
    for pair in 0..twins["pairs"].as_i64().unwrap() {
        for name in twins["roles"].as_array().unwrap() {
            messages.push(message(
                twins["first_seq"].as_i64().unwrap() + pair,
                role(name.as_str().unwrap()),
                twins["content"].as_str().unwrap().to_string(),
            ));
        }
    }
    let tool_twins = &spec["tool_twins"];
    for pair in 0..tool_twins["pairs"].as_i64().unwrap() {
        for row in tool_twins["rows"].as_array().unwrap() {
            let mut call = message(
                tool_twins["first_seq"].as_i64().unwrap() + pair,
                role(row[0].as_str().unwrap()),
                tool_twins["content"].as_str().unwrap().to_string(),
            );
            call.tool_name = Some(tool_twins["tool_name"].as_str().unwrap().to_string());
            call.kind = match row[1].as_str().unwrap() {
                "tool_call" => MessageKind::ToolCall,
                "tool_result" => MessageKind::ToolResult,
                other => panic!("fixture kind {other}"),
            };
            messages.push(call);
        }
    }
    let session_id = spec["session_id"].as_str().unwrap().to_string();
    let db = Db::open(&db_path).unwrap();
    db.upsert_session(
        &ParsedSession::new(
            {
                let mut value = SessionRecord::new(
                    session_id.clone(),
                    Provider::Claude,
                    session_id.trim_start_matches("claude:"),
                    "/fixture/parity.jsonl",
                    "parity-fixture-v1",
                    "test-fixture",
                );
                value.message_count = Some(messages.len() as i64);
                value
            },
            messages,
        ),
        0,
        0,
    )
    .unwrap();
    drop(db);
    Fixture {
        _dir: dir,
        db_path,
        config,
        spec,
    }
}

fn expected(spec: &Value, value: &Value) -> Ids {
    let pairs = |block: &Value, role_of: fn(&Value) -> &str| -> Ids {
        let first = block["first_seq"].as_i64().unwrap();
        let roles: Vec<String> = block[if block.get("rows").is_some() {
            "rows"
        } else {
            "roles"
        }]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| role_of(entry).to_string())
        .collect();
        (0..block["pairs"].as_i64().unwrap())
            .flat_map(|pair| roles.iter().map(move |role| (first + pair, role.clone())))
            .collect()
    };
    match value.as_str() {
        Some("twins") => return pairs(&spec["twins"], |name| name.as_str().unwrap()),
        Some("tool_twins") => return pairs(&spec["tool_twins"], |row| row[0].as_str().unwrap()),
        _ => {}
    }
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|pair| {
            (
                pair[0].as_i64().unwrap(),
                pair[1].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// One search, in interface-neutral terms. `limit: None` with `all_results: false` is the
/// deliberately surface-specific omitted limit.
#[derive(Clone, Debug)]
struct Search {
    role: Option<String>,
    mode: String,
    /// `None` searches content; otherwise the snake_case field name MCP and Python use.
    field: Option<String>,
    query: String,
    limit: Option<usize>,
    offset: usize,
    all_results: bool,
    latest_in_session: bool,
}

impl Search {
    fn from_case(case: &Value) -> Self {
        Self {
            role: case["role"].as_str().map(str::to_owned),
            mode: case["mode"].as_str().unwrap().into(),
            field: case["field"].as_str().map(str::to_string),
            query: case["query"].as_str().unwrap().into(),
            limit: case["limit"].as_u64().map(|n| n as usize),
            offset: 0,
            all_results: case["all_results"].as_bool().unwrap_or(false),
            latest_in_session: case["match_window"].as_str() == Some("latest"),
        }
    }
}

fn service_search(f: &Fixture, surface: SearchSurface, search: &Search) -> anyhow::Result<Ids> {
    let db = Db::open(&f.db_path)?;
    let config = Config::default();
    let service = MessageService::new(&config, &db, surface);
    let query = match search.mode.as_str() {
        _ if search.query.is_empty() => MessageQuery::All,
        "literal" => MessageQuery::literal(&search.query)?,
        "regex" => MessageQuery::regex(&search.query)?,
        "fuzzy" => MessageQuery::fuzzy(&search.query)?,
        other => panic!("mode {other}"),
    };
    let extent = if search.all_results {
        RequestedExtent::all_results_from(search.offset)
    } else {
        RequestedExtent::page(search.limit, search.offset)?
    };
    let target = match search.field.as_deref() {
        None => MessageTarget::content(),
        Some("tool_name") => MessageTarget::tool_name(),
        Some(other) => panic!("field {other}"),
    };
    let mut builder = MessageSearchRequest::builder(query, target).extent(extent);
    if let Some(value) = &search.role {
        builder = builder.role(value.parse::<Role>().unwrap());
    }
    if search.latest_in_session {
        builder = builder
            .session_id(f.spec["session_id"].as_str().unwrap())?
            .match_window(MatchWindow::Latest);
    }
    let response = service.search(builder.build()?)?;
    Ok(response
        .hits()
        .iter()
        .map(|hit| {
            (
                hit.message.seq,
                serde_json::to_value(hit.message.role)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string(),
            )
        })
        .collect())
}

fn aise(f: &Fixture) -> Command {
    let mut command = support::aise(f._dir.path());
    command
        .env("AI_SESSION_SEARCH_CONFIG", &f.config)
        .arg("--database")
        .arg(&f.db_path);
    command
}

fn ids_from_results(results: &Value) -> Ids {
    results
        .as_array()
        .unwrap_or_else(|| panic!("no results array in {results}"))
        .iter()
        .map(|hit| {
            (
                hit["message_ref"]["message_seq"].as_i64().unwrap(),
                hit["message_metadata"]["role"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            )
        })
        .collect()
}

/// The CLI's own result document, or its stderr when it refused the request.
fn cli_search(f: &Fixture, search: &Search) -> Result<Value, String> {
    let mut command = aise(f);
    command.args([
        "messages",
        "search",
        "--format",
        "json",
        "--index-refresh",
        "existing-only",
        "--skip-release-notification",
        "--query-mode",
        &search.mode,
        "--offset",
        &search.offset.to_string(),
    ]);
    if let Some(value) = &search.role {
        command.args(["--role", value]);
    }
    if let Some(field) = &search.field {
        command.args(["--field", &field.replace('_', "-")]);
    }
    if search.all_results {
        command.arg("--all-results");
    } else if let Some(limit) = search.limit {
        command.args(["--limit", &limit.to_string()]);
    }
    if search.latest_in_session {
        command.args([
            "--session-id",
            f.spec["session_id"].as_str().unwrap(),
            "--match-window",
            "latest",
        ]);
    }
    command.arg("--").arg(&search.query);
    let output = command.output().unwrap();
    if output.status.success() {
        Ok(serde_json::from_slice(&output.stdout).unwrap())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).into_owned())
    }
}

/// Every search through one `aise mcp serve` process, in order: the `structuredContent` of each
/// successful call, or the error text of each refused one.
fn mcp_searches(f: &Fixture, searches: &[Search]) -> Vec<Result<Value, String>> {
    let calls: Vec<(&str, Value)> = searches
        .iter()
        .map(|search| {
            let mut arguments = json!({
                "query": search.query,
                "query_mode": search.mode,
                "offset": search.offset,
                "index_refresh": "existing-only",
            });
            if let Some(value) = &search.role {
                arguments["role"] = json!(value);
            }
            if let Some(field) = &search.field {
                arguments["field"] = json!(field);
            }
            if search.all_results {
                arguments["all_results"] = json!(true);
            } else if let Some(limit) = search.limit {
                arguments["limit"] = json!(limit);
            }
            if search.latest_in_session {
                arguments["session_id"] = f.spec["session_id"].clone();
                arguments["match_window"] = json!("latest");
            }
            ("search_messages", arguments)
        })
        .collect();
    mcp_calls(f, &calls)
}

/// Every `(tool, arguments)` call through one `aise mcp serve` process, in order: each successful
/// call's structured result (its `structuredContent`, or its JSON text content when a tool returns
/// only text), or the error text of each refused one.
fn mcp_calls(f: &Fixture, calls: &[(&str, Value)]) -> Vec<Result<Value, String>> {
    let mut child = aise(f)
        .args(["mcp", "serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = vec![json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                   "clientInfo": {"name": "search-surface-parity", "version": "1"}}
    })
    .to_string()];
    lines.push(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string());
    for (index, (tool, arguments)) in calls.iter().enumerate() {
        lines.push(
            json!({"jsonrpc": "2.0", "id": index + 1, "method": "tools/call",
                   "params": {"name": tool, "arguments": arguments}})
            .to_string(),
        );
    }
    {
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all((lines.join("\n") + "\n").as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    let mut by_id = std::collections::HashMap::new();
    for line in String::from_utf8(output.stdout).unwrap().lines() {
        let response: Value = serde_json::from_str(line).unwrap();
        if let Some(id) = response["id"].as_u64() {
            by_id.insert(id as usize, response);
        }
    }
    (1..=calls.len())
        .map(|id| {
            let response = by_id
                .remove(&id)
                .unwrap_or_else(|| panic!("no MCP response for request {id}"));
            if let Some(error) = response.get("error") {
                return Err(error.to_string());
            }
            let result = &response["result"];
            if result["isError"].as_bool() == Some(true) {
                return Err(result["content"].to_string());
            }
            if !result["structuredContent"].is_null() {
                return Ok(result["structuredContent"].clone());
            }
            let text = result["content"][0]["text"].as_str().unwrap_or_default();
            serde_json::from_str(text).map_err(|error| format!("{error}: {text}"))
        })
        .collect()
}

#[test]
fn every_interface_returns_the_same_rows_in_the_same_order() {
    let f = fixture();
    let cases = f.spec["cases"].as_array().unwrap().clone();
    let searches: Vec<Search> = cases.iter().map(Search::from_case).collect();
    let mcp = mcp_searches(&f, &searches);
    let mut failures: Vec<String> = Vec::new();
    for ((case, search), mcp) in cases.iter().zip(&searches).zip(mcp) {
        let name = case["name"].as_str().unwrap();
        let want = expected(&f.spec, &case["expected"]);
        let mut check = |interface: String, got: Result<Ids, String>| match got {
            Ok(got) if got == want => {}
            Ok(got) => failures.push(format!(
                "{name} via {interface}: got {got:?}, want {want:?}"
            )),
            Err(error) => failures.push(format!("{name} via {interface}: refused: {error}")),
        };
        for &surface in SearchSurface::ALL {
            check(
                format!("MessageService {surface:?}"),
                service_search(&f, surface, search).map_err(|error| format!("{error:#}")),
            );
        }
        check(
            "aise messages search".into(),
            cli_search(&f, search).map(|doc| ids_from_results(&doc["results"])),
        );
        check(
            "MCP search_messages".into(),
            mcp.map(|doc| ids_from_results(&doc["results"])),
        );
    }
    assert!(
        failures.is_empty(),
        "{} failing checks:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn paging_one_row_at_a_time_reproduces_the_whole_order_on_every_interface() {
    let f = fixture();
    for walk in f.spec["page_walks"].as_array().unwrap() {
        let name = walk["name"].as_str().unwrap();
        let want = expected(&f.spec, &walk["expected"]);
        let page = walk["page"].as_u64().unwrap() as usize;
        let pages: Vec<Search> = (0..want.len() + page)
            .step_by(page)
            .map(|offset| Search {
                role: walk["role"].as_str().map(str::to_owned),
                mode: walk["mode"].as_str().unwrap().into(),
                field: walk["field"].as_str().map(str::to_string),
                query: walk["query"].as_str().unwrap().into(),
                limit: Some(page),
                offset,
                all_results: false,
                latest_in_session: false,
            })
            .collect();
        let concat = |chunks: Vec<Ids>| chunks.into_iter().flatten().collect::<Ids>();

        for &surface in SearchSurface::ALL {
            let walked = concat(
                pages
                    .iter()
                    .map(|search| service_search(&f, surface, search).unwrap())
                    .collect(),
            );
            assert_eq!(walked, want, "{name}: MessageService on {surface:?}");
        }
        let walked = concat(
            pages
                .iter()
                .map(|search| ids_from_results(&cli_search(&f, search).unwrap()["results"]))
                .collect(),
        );
        assert_eq!(walked, want, "{name}: aise messages search");
        let walked = concat(
            mcp_searches(&f, &pages)
                .into_iter()
                .map(|result| ids_from_results(&result.unwrap()["results"]))
                .collect(),
        );
        assert_eq!(walked, want, "{name}: MCP search_messages");
    }
}

/// REQ003: an omitted limit returns every match on Rust, CLI and Python, and MCP's configured
/// finite page, because MCP results enter an agent's context directly. Pinned both ways, so a
/// change to either the shared behavior or the deliberate MCP page fails here.
#[test]
fn an_omitted_limit_keeps_its_documented_per_surface_meaning() {
    let f = fixture();
    let spec = &f.spec["deliberate_differences"]["omitted_limit"];
    let all = spec["all_surfaces_but_mcp"].as_u64().unwrap() as usize;
    let mcp_page = spec["mcp_default_page"].as_u64().unwrap() as usize;
    let search = Search {
        role: None,
        mode: "literal".into(),
        field: None,
        query: spec["query"].as_str().unwrap().into(),
        limit: None,
        offset: 0,
        all_results: false,
        latest_in_session: false,
    };
    for &surface in SearchSurface::ALL {
        let got = service_search(&f, surface, &search).unwrap().len();
        let want = if surface == SearchSurface::Mcp {
            mcp_page
        } else {
            all
        };
        assert_eq!(got, want, "omitted limit on MessageService {surface:?}");
    }
    let cli = cli_search(&f, &search).unwrap();
    assert_eq!(ids_from_results(&cli["results"]).len(), all, "CLI");
    let mcp = mcp_searches(&f, std::slice::from_ref(&search))
        .pop()
        .unwrap()
        .unwrap();
    assert_eq!(ids_from_results(&mcp["results"]).len(), mcp_page, "MCP");
    // The rows MCP leaves out are announced, not silently dropped.
    assert_eq!(
        mcp["page"]["next_offset"].as_u64(),
        Some(mcp_page as u64),
        "MCP must name where the next page starts: {}",
        mcp["page"]
    );
}

/// REQ003: fuzzy search always needs a finite page, so every interface refuses `all_results`.
#[test]
fn fuzzy_all_results_is_refused_on_every_interface() {
    let f = fixture();
    let search = Search {
        role: None,
        mode: "fuzzy".into(),
        field: None,
        query: f.spec["deliberate_differences"]["fuzzy_rejects_all_results"]["query"]
            .as_str()
            .unwrap()
            .into(),
        limit: None,
        offset: 0,
        all_results: true,
        latest_in_session: false,
    };
    for &surface in SearchSurface::ALL {
        assert!(
            service_search(&f, surface, &search).is_err(),
            "MessageService {surface:?} accepted fuzzy all_results"
        );
    }
    assert!(
        cli_search(&f, &search).is_err(),
        "CLI accepted fuzzy all_results"
    );
    assert!(
        mcp_searches(&f, std::slice::from_ref(&search))
            .pop()
            .unwrap()
            .is_err(),
        "MCP accepted fuzzy all_results"
    );
}

/// The `[seq, role]` rows of a message-read result, wherever the interface puts them: the CLI
/// prints a bare array and MCP `get_session` nests one under its window object.
fn read_rows(value: &Value) -> Ids {
    fn find(value: &Value) -> Option<&Vec<Value>> {
        match value {
            Value::Array(rows)
                if rows
                    .first()
                    .is_some_and(|row| row.get("seq").is_some() && row.get("role").is_some()) =>
            {
                Some(rows)
            }
            Value::Array(items) => items.iter().find_map(find),
            Value::Object(fields) => fields.values().find_map(find),
            _ => None,
        }
    }
    find(value)
        .unwrap_or_else(|| panic!("no message rows in {value}"))
        .iter()
        .map(|row| {
            (
                row["seq"].as_i64().unwrap(),
                row["role"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn hit_ids(hits: Vec<ai_session_search::models::MessageHit>) -> Ids {
    hits.into_iter()
        .map(|hit| (hit.seq, hit.role.as_str().to_string()))
        .collect()
}

fn cli_read(f: &Fixture, selector: &[String]) -> Ids {
    let output = aise(f)
        .args(["messages", "get", f.spec["session_id"].as_str().unwrap()])
        .args(selector)
        .args([
            "--format",
            "json",
            "--index-refresh",
            "existing-only",
            "--skip-release-notification",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "aise messages get {selector:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    read_rows(&serde_json::from_slice(&output.stdout).unwrap())
}

/// `(session_id, seq)` locates rather than identifies a message. Reading a seq returns every row
/// that holds it, in the published total order, on every message-read interface; and the newest
/// rows of a session are the tail of that order even when the cut falls inside a tie.
#[test]
fn reading_a_shared_seq_returns_every_row_in_one_order_on_every_interface() {
    use ai_session_search::db::MessageOrder;
    use ai_session_search::models::MessageFilters;

    let f = fixture();
    let session_id = f.spec["session_id"].as_str().unwrap().to_string();
    let db = Db::open(&f.db_path).unwrap();
    let config = Config::default();
    let service = MessageService::new(&config, &db, SearchSurface::Rust);
    let mut failures: Vec<String> = Vec::new();
    let mut check = |name: &str, interface: &str, got: Ids, want: &Ids| {
        if &got != want {
            failures.push(format!(
                "{name} via {interface}: got {got:?}, want {want:?}"
            ));
        }
    };

    let reads = f.spec["reads"].as_array().unwrap();
    let mut calls: Vec<(&str, Value)> = Vec::new();
    for read in reads {
        let seq = read["seq"].as_i64().unwrap();
        calls.push((
            "get_session",
            json!({"session_id": session_id, "message_seq": seq, "context": 0}),
        ));
        calls.push((
            "get_session",
            json!({"session_id": session_id, "seq_from": seq, "seq_to": seq}),
        ));
    }
    let mut mcp = mcp_calls(&f, &calls).into_iter();
    for read in reads {
        let name = read["name"].as_str().unwrap();
        let want = expected(&f.spec, &read["expected"]);
        let seq = read["seq"].as_i64().unwrap();
        check(
            name,
            "MessageService::context",
            hit_ids(service.context(&session_id, seq, 0, 0).unwrap()),
            &want,
        );
        let range = {
            let mut value = MessageFilters::default();
            value.session_id = Some(session_id.clone());
            value.seq_from = Some(seq);
            value.seq_to = Some(seq);
            value
        };
        check(
            name,
            "MessageService::read_session",
            hit_ids(
                service
                    .read_session(&range, MessageOrder::OldestFirst)
                    .unwrap(),
            ),
            &want,
        );
        check(
            name,
            "aise messages get --seq",
            cli_read(&f, &["--seq".into(), seq.to_string()]),
            &want,
        );
        check(
            name,
            "aise messages get --seq-from --seq-to",
            cli_read(
                &f,
                &[
                    "--seq-from".into(),
                    seq.to_string(),
                    "--seq-to".into(),
                    seq.to_string(),
                ],
            ),
            &want,
        );
        for interface in [
            "MCP get_session message_seq",
            "MCP get_session seq_from/seq_to",
        ] {
            match mcp.next().unwrap() {
                Ok(value) => check(name, interface, read_rows(&value), &want),
                Err(error) => check(
                    name,
                    &format!("{interface}, refused: {error}"),
                    Ids::new(),
                    &want,
                ),
            }
        }
    }

    for read in f.spec["newest_reads"].as_array().unwrap() {
        let name = read["name"].as_str().unwrap();
        let want = expected(&f.spec, &read["expected"]);
        let limit = read["limit"].as_u64().unwrap() as usize;
        let newest = {
            let mut value = MessageFilters::default();
            value.session_id = Some(session_id.clone());
            value.limit = limit;
            value
        };
        check(
            name,
            "MessageService::read_session newest",
            hit_ids(
                service
                    .read_session(&newest, MessageOrder::NewestFirst)
                    .unwrap(),
            ),
            &want,
        );
        check(
            name,
            "aise messages get --order newest",
            cli_read(
                &f,
                &[
                    "--limit".into(),
                    limit.to_string(),
                    "--order".into(),
                    "newest".into(),
                ],
            ),
            &want,
        );
    }
    assert!(
        failures.is_empty(),
        "{} failing checks:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn asymmetric_context_reads_agree_across_surfaces() {
    let f = fixture();
    let db = Db::open(&f.db_path).unwrap();
    let config = Config::default();
    let service = MessageService::new(&config, &db, SearchSurface::Rust);
    let session = f.spec["session_id"].as_str().unwrap();
    for read in f.spec["context_reads"].as_array().unwrap() {
        let seq = read["seq"].as_i64().unwrap();
        let before = read["before"].as_i64().unwrap();
        let after = read["after"].as_i64().unwrap();
        let want = expected(&f.spec, &read["expected"]);
        assert_eq!(
            hit_ids(service.context(session, seq, before, after).unwrap()),
            want
        );
        assert_eq!(
            cli_read(
                &f,
                &[
                    "--seq".into(),
                    seq.to_string(),
                    "--context-before".into(),
                    before.to_string(),
                    "--context-after".into(),
                    after.to_string()
                ]
            ),
            want
        );
        for parameter in read["seq_parameters"].as_array().unwrap() {
            let mut arguments =
                json!({"session_id":session,"context_before":before,"context_after":after});
            arguments[parameter.as_str().unwrap()] = json!(seq);
            let results = mcp_calls(&f, &[("get_session", arguments)]);
            assert_eq!(read_rows(results[0].as_ref().unwrap()), want);
        }
    }
}

#[test]
fn timeline_keeps_all_rows_when_search_has_an_operation_default() {
    let f = fixture();
    let mut config = std::fs::read_to_string(&f.config).unwrap();
    config.push_str("\n[search.message-search]\ndefault_limit = 1\n");
    std::fs::write(&f.config, config).unwrap();
    let session = f.spec["session_id"].as_str().unwrap();
    let difference = &f.spec["deliberate_differences"]["timeline_operation_limit"];
    let timeline = aise(&f)
        .args([
            "messages",
            "timeline",
            session,
            "--grep",
            "bulk row",
            "--format",
            "json",
            "--index-refresh",
            "existing-only",
        ])
        .output()
        .unwrap();
    assert!(
        timeline.status.success(),
        "{}",
        String::from_utf8_lossy(&timeline.stderr)
    );
    let rows: Value = serde_json::from_slice(&timeline.stdout).unwrap();
    assert_eq!(
        rows.as_array().unwrap().len(),
        difference["timeline_rows"].as_u64().unwrap() as usize
    );
    let search = aise(&f)
        .args([
            "messages",
            "search",
            "bulk row",
            "--session-id",
            session,
            "--format",
            "json",
            "--index-refresh",
            "existing-only",
        ])
        .output()
        .unwrap();
    assert!(
        search.status.success(),
        "{}",
        String::from_utf8_lossy(&search.stderr)
    );
    let rows: Value = serde_json::from_slice(&search.stdout).unwrap();
    assert_eq!(
        rows["results"].as_array().unwrap().len(),
        difference["search_rows"].as_u64().unwrap() as usize
    );
    let help = aise(&f)
        .args(["messages", "timeline", "--help"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&help.stdout).contains("ignores message-search operation limits")
    );
}

#[test]
fn show_json_preserves_the_shared_transcript_window() {
    let f = fixture();
    let db = Db::open(&f.db_path).unwrap();
    let read = &f.spec["transcript_read"];
    let mut parsed = ParsedSession::new(
        SessionRecord::new(
            "claude:show",
            Provider::Claude,
            "show",
            "/fixture/show.jsonl",
            "test",
            "fixture",
        ),
        vec![],
    );
    parsed.transcript_text = read["source"].as_str().unwrap().into();
    db.upsert_session(&parsed, 0, 0).unwrap();
    let expected = ai_session_search::service::CatalogService::new(&db)
        .transcript("claude:show", -2)
        .unwrap();
    let output = aise(&f)
        .args([
            "show",
            "claude:show",
            "--transcript-lines",
            "-2",
            "--format",
            "json",
            "--index-refresh",
            "existing-only",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let got: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(got["text"], expected.text);
    assert_eq!(got["text"], read["expected"]);
    assert_eq!(got["total_lines"], read["total_lines"]);
    assert_eq!(got["lines_returned"], read["lines_returned"]);
    assert!(got.get("lines_label").is_none(), "{got:#}");
    assert_eq!(got["edge"], "tail");
    assert_eq!(got["complete"], false);
    assert!(got["session"].get("raw_metadata_json").is_none());
}

#[test]
fn timeline_and_repeats_query_mode_regex_preserves_legacy_matches() {
    let f = fixture();
    let session = f.spec["session_id"].as_str().unwrap();
    let pattern = f.spec["regex_alias"]["pattern"].as_str().unwrap();
    for command in ["timeline", "repeats"] {
        let run = |legacy: bool, mode: &str| {
            let mut cli = aise(&f);
            if command == "timeline" {
                cli.args(["messages", "timeline", session]);
                if legacy {
                    cli.args(["--regex", pattern]);
                } else {
                    cli.args(["--grep", pattern, "--query-mode", mode]);
                }
            } else {
                cli.args([
                    "repeats",
                    pattern,
                    "--session-id",
                    session,
                    "--phrase-min-words",
                    "1",
                    "--phrase-max-words",
                    "2",
                    "--min-matches",
                    "2",
                ]);
                if legacy {
                    cli.arg("--regex");
                } else {
                    cli.args(["--query-mode", mode]);
                }
            }
            let output = cli
                .args(["--format", "json", "--index-refresh", "existing-only"])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{command}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            if legacy {
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert!(
                    stderr.contains("deprecated") && stderr.contains("--query-mode regex"),
                    "{stderr}"
                );
            }
            let value = serde_json::from_slice::<Value>(&output.stdout).unwrap();
            if command == "repeats" {
                value["groups"].clone()
            } else {
                value
            }
        };
        let canonical = run(false, "regex");
        assert!(!canonical.as_array().unwrap().is_empty(), "{command}");
        assert_eq!(canonical, run(true, "regex"), "{command}");
        assert!(
            run(false, "literal").as_array().unwrap().is_empty(),
            "{command}"
        );
    }
}

#[test]
fn cli_empty_search_hints_are_conditional_and_preserve_machine_output() {
    let f = fixture();
    let run = |format: &str, extra: &[&str]| {
        let output = aise(&f)
            .args([
                "messages",
                "search",
                "harness-only sentinel",
                "--format",
                format,
                "--index-refresh",
                "existing-only",
            ])
            .args(extra)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };
    for format in ["table", "plain"] {
        let missing = run(format, &[]);
        let stderr = String::from_utf8_lossy(&missing.stderr);
        assert!(stderr.contains("--kinds harness-notice"), "{stderr}");
        assert!(
            stderr.contains("--field tool-argument") && stderr.contains("aise files search"),
            "{stderr}"
        );
        let included = run(format, &["--kinds", "harness-notice"]);
        assert!(String::from_utf8_lossy(&included.stdout).contains("harness-only sentinel"));
        assert!(!String::from_utf8_lossy(&included.stderr).contains("Harness notices are excluded"));
        let explicit = run(format, &["--kinds", "conversation"]);
        assert!(!String::from_utf8_lossy(&explicit.stderr).contains("[hint]"));
    }
    let json = run("json", &[]);
    assert!(!String::from_utf8_lossy(&json.stderr).contains("[hint]"));
    let value: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert!(value["results"].as_array().unwrap().is_empty());
    assert!(value["effective_request"]
        .get("harness_notices_excluded")
        .is_none());
}
