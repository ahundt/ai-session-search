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

const FIXTURE: &str = include_str!("fixtures/search_surface_parity.json");

type Ids = Vec<(i64, String)>;

struct Fixture {
    _dir: tempfile::TempDir,
    db_path: PathBuf,
    home: PathBuf,
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
    std::fs::write(&config, "").unwrap();

    let role = |name: &str| match name {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        other => panic!("fixture role {other}"),
    };
    let message = |seq: i64, role: Role, content: String| Message {
        seq,
        role,
        ts: None,
        tool_name: None,
        kind: MessageKind::Conversation,
        tool_call_id: None,
        content,
        is_compaction: false,
        provenance: Default::default(),
    };
    let mut messages: Vec<Message> = spec["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            message(
                m["seq"].as_i64().unwrap(),
                role(m["role"].as_str().unwrap()),
                m["content"].as_str().unwrap().to_string(),
            )
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
        &ParsedSession {
            session: SessionRecord {
                id: session_id.clone(),
                provider: Provider::Claude,
                provider_session_id: session_id.trim_start_matches("claude:").into(),
                title: None,
                summary: None,
                cwd: None,
                repo_root: None,
                created_at: None,
                updated_at: None,
                last_message_at: None,
                preview_text: String::new(),
                source_path: "/fixture/parity.jsonl".into(),
                message_count: Some(messages.len() as i64),
                parse_version: "parity-fixture-v1".into(),
                raw_metadata_json: None,
                parse_warning: None,
                discovery_source: "test-fixture".into(),
                parent_session_id: None,
                agent_label: None,
            },
            transcript_text: String::new(),
            messages,
            file_edits: Vec::new(),
        },
        0,
        0,
    )
    .unwrap();
    drop(db);
    Fixture {
        _dir: dir,
        db_path,
        home,
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
    let mut command = Command::new(env!("CARGO_BIN_EXE_aise"));
    command
        .env("AI_SESSION_SEARCH_CONFIG", &f.config)
        .env("HOME", &f.home)
        .env("USERPROFILE", &f.home)
        .env("XDG_CONFIG_HOME", f.home.join(".config"))
        .env("APPDATA", f.home.join("AppData/Roaming"))
        .env("LOCALAPPDATA", f.home.join("AppData/Local"))
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
        let range = MessageFilters {
            session_id: Some(session_id.clone()),
            seq_from: Some(seq),
            seq_to: Some(seq),
            ..Default::default()
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
        let newest = MessageFilters {
            session_id: Some(session_id.clone()),
            limit,
            ..Default::default()
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
