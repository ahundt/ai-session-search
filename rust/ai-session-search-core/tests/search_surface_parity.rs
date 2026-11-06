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
//! `messages search`, and its MCP server over JSON-RPC. `tests/test_native_binding.py` runs the
//! same fixture through the Python binding and the Python entry point.
//!
//! What fails without which fix, measured by reverting it:
//! - the trigram prefilter superset (Cherokee by either spelling, regex included, and `abi`
//!   against `abİ`): 24 checks, every case on all six interfaces here;
//! - the total order: the fuzzy walks over the 40 tied pairs. The latest-window and three-row tie
//!   cases are parity checks only, because at this size SQLite serves them by walking
//!   `idx_messages_session_seq`, which already orders a tie; the sorted plan that exposed the
//!   defect on a real index is reproduced by the unit test
//!   `the_newest_window_selects_exactly_the_tail_of_the_oldest_order` in `db.rs`.
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
    if value.as_str() == Some("twins") {
        let twins = &spec["twins"];
        let first = twins["first_seq"].as_i64().unwrap();
        return (0..twins["pairs"].as_i64().unwrap())
            .flat_map(|pair| {
                twins["roles"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(move |name| (first + pair, name.as_str().unwrap().to_string()))
            })
            .collect();
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
    let mut builder = MessageSearchRequest::builder(query, MessageTarget::content()).extent(extent);
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
    for (index, search) in searches.iter().enumerate() {
        let mut arguments = json!({
            "query": search.query,
            "query_mode": search.mode,
            "offset": search.offset,
            "index_refresh": "existing-only",
        });
        if search.all_results {
            arguments["all_results"] = json!(true);
        } else if let Some(limit) = search.limit {
            arguments["limit"] = json!(limit);
        }
        if search.latest_in_session {
            arguments["session_id"] = f.spec["session_id"].clone();
            arguments["match_window"] = json!("latest");
        }
        lines.push(
            json!({"jsonrpc": "2.0", "id": index + 1, "method": "tools/call",
                   "params": {"name": "search_messages", "arguments": arguments}})
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
    (1..=searches.len())
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
            Ok(result["structuredContent"].clone())
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
