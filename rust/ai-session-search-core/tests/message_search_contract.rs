use std::path::Path;

use ai_session_search::config::Config;
use ai_session_search::db::{Db, SCHEMA_VERSION};
use ai_session_search::message_search::SearchSurface;
use ai_session_search::models::{
    Message, MessageFilters, MessageKind, MessageSearchMode, ParsedSession, Provider, Role,
    SearchField, SessionRecord,
};
use ai_session_search::service::MessageService;
use ai_session_search::{
    ContextWindow, DetailLevel, FieldViewBudget, MatchViewBudget, MessageQuery,
    MessageSearchInclude, MessageSearchRequest, MessageTarget, ProviderScope, ReceiptLevel,
    RequestedExtent, ResultSetExtent,
};
use serde::Deserialize;

const CONTRACT_FIXTURE: &str = include_str!("fixtures/message_search_contract.json");

#[test]
fn public_response_vocabulary_names_units_and_included_payloads() {
    assert_eq!(
        serde_json::to_value(ContextWindow::new(2, 3)).unwrap(),
        serde_json::json!({"messages_before": 2, "messages_after": 3})
    );
    assert_eq!(
        serde_json::to_value(ProviderScope::Selected {
            providers: vec![Provider::Claude, Provider::Codex],
        })
        .unwrap(),
        serde_json::json!({
            "kind": "selected",
            "providers": ["claude", "codex"],
        })
    );
    assert_eq!(
        serde_json::to_value([
            MessageSearchInclude::NormalizedSessionMetadata,
            MessageSearchInclude::ParsedReferences,
            MessageSearchInclude::RawProviderMetadata,
            MessageSearchInclude::RuntimeDiagnostics,
        ])
        .unwrap(),
        serde_json::json!([
            "normalized_session_metadata",
            "parsed_references",
            "raw_provider_metadata",
            "runtime_diagnostics",
        ])
    );
    assert_eq!(ResultSetExtent::All.as_str(), "all");
    assert_eq!(
        serde_json::to_value(FieldViewBudget::NoCharLimit).unwrap(),
        serde_json::json!({"kind": "no_char_limit"})
    );
}

#[derive(Debug, Deserialize)]
struct ContractFixture {
    messages: Vec<Message>,
    text_cases: Vec<TextCase>,
    all_cases: Vec<AllCase>,
    semantic_response_case: SemanticResponseCase,
}

#[derive(Debug, Deserialize)]
struct TextCase {
    field: SearchField,
    mode: MessageSearchMode,
    query: String,
    #[serde(default)]
    argument_path: Option<String>,
    expected_seq: Vec<i64>,
    expected_corpus: i64,
}

#[derive(Debug, Deserialize)]
struct AllCase {
    field: SearchField,
    #[serde(default)]
    argument_path: Option<String>,
    expected_seq: Vec<i64>,
}

#[derive(Debug, Deserialize)]
struct SemanticResponseCase {
    session_id: String,
    provider_session_id: String,
    message_seq: i64,
    query: String,
    content: String,
    literal_start: usize,
    literal_end: usize,
    boundary_view_chars: usize,
    match_view_chars: usize,
}

fn open_disposable_fixture() -> (tempfile::TempDir, Db, ContractFixture) {
    let fixture: ContractFixture = serde_json::from_str(CONTRACT_FIXTURE).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("message-search-contract.db");
    assert_disposable_database_path(directory.path(), &database_path);

    let db = Db::open(&database_path).unwrap();
    db.upsert_session(
        &ParsedSession {
            session: SessionRecord {
                id: "claude:message-search-contract".into(),
                provider: Provider::Claude,
                provider_session_id: "message-search-contract".into(),
                title: Some("Message search contract fixture".into()),
                summary: None,
                cwd: Some("/fixture/workspace".into()),
                repo_root: Some("/fixture".into()),
                created_at: None,
                updated_at: None,
                last_message_at: None,
                preview_text: String::new(),
                source_path: "/fixture/transcripts/contract.jsonl".into(),
                message_count: Some(fixture.messages.len() as i64),
                parse_version: "contract-fixture-v1".into(),
                raw_metadata_json: Some(r#"{"model":"gpt-test"}"#.into()),
                parse_warning: None,
                discovery_source: "test-fixture".into(),
                parent_session_id: None,
                agent_label: None,
            },
            transcript_text: String::new(),
            messages: fixture.messages.clone(),
            file_edits: Vec::new(),
        },
        0,
        0,
    )
    .unwrap();
    (directory, db, fixture)
}

fn assert_disposable_database_path(root: &Path, database_path: &Path) {
    assert!(root.is_dir(), "temporary database root must exist");
    assert_eq!(database_path.parent(), Some(root));
    assert_eq!(
        database_path.file_name().and_then(|name| name.to_str()),
        Some("message-search-contract.db")
    );
}

fn seqs(hits: Vec<ai_session_search::models::MessageHit>) -> Vec<i64> {
    hits.into_iter().map(|hit| hit.seq).collect()
}

fn insert_semantic_response_case(db: &Db, case: &SemanticResponseCase) {
    db.upsert_session(
        &ParsedSession {
            session: SessionRecord {
                id: case.session_id.clone(),
                provider: Provider::Codex,
                provider_session_id: case.provider_session_id.clone(),
                title: Some("Semantic response contract fixture".into()),
                summary: None,
                cwd: Some("/fixture/workspace".into()),
                repo_root: Some("/fixture".into()),
                created_at: None,
                updated_at: None,
                last_message_at: None,
                preview_text: String::new(),
                source_path: "/fixture/transcripts/semantic-response.jsonl".into(),
                message_count: Some(2),
                parse_version: "contract-fixture-v1".into(),
                raw_metadata_json: Some(r#"{"model":"gpt-test"}"#.into()),
                parse_warning: None,
                discovery_source: "test-fixture".into(),
                parent_session_id: None,
                agent_label: None,
            },
            transcript_text: case.content.clone(),
            messages: vec![
                Message {
                    seq: case.message_seq,
                    role: Role::User,
                    ts: None,
                    tool_name: None,
                    kind: MessageKind::Conversation,
                    tool_call_id: None,
                    content: case.content.clone(),
                    is_compaction: false,
                    provenance: Default::default(),
                },
                Message {
                    seq: case.message_seq + 1,
                    role: Role::Assistant,
                    ts: None,
                    tool_name: None,
                    kind: MessageKind::Conversation,
                    tool_call_id: None,
                    content: format!("second {}", case.content),
                    is_compaction: false,
                    provenance: Default::default(),
                },
            ],
            file_edits: Vec::new(),
        },
        0,
        0,
    )
    .unwrap();
}

#[test]
fn current_text_modes_fields_results_paging_and_planner_are_frozen() {
    let (_directory, db, fixture) = open_disposable_fixture();

    assert_eq!(fixture.text_cases.len(), 9);
    for case in fixture.text_cases {
        let filters = MessageFilters {
            kinds: Some(vec![MessageKind::ToolCall]),
            field: Some(case.field),
            argument_path: case.argument_path.clone(),
            match_mode: case.mode,
            limit: 10,
            ..Default::default()
        };
        let (hits, receipt) = db
            .search_messages_with_explain(&case.query, &filters, true)
            .unwrap_or_else(|error| panic!("{:?}/{:?}: {error:#}", case.field, case.mode));
        assert_eq!(seqs(hits), case.expected_seq, "{case:?}");

        let receipt = receipt.unwrap_or_else(|| panic!("missing planner receipt for {case:?}"));
        assert_eq!(receipt.corpus, case.expected_corpus, "{case:?}");
        assert!(receipt.candidates.is_some(), "{case:?}: {receipt:?}");

        let page = db
            .search_messages(
                &case.query,
                &MessageFilters {
                    limit: 1,
                    offset: 1,
                    ..filters
                },
            )
            .unwrap_or_else(|error| panic!("paged {:?}/{:?}: {error:#}", case.field, case.mode));
        assert_eq!(seqs(page), vec![case.expected_seq[1]], "paged {case:?}");
    }
}

#[test]
fn current_no_text_field_presence_semantics_are_frozen() {
    let (_directory, db, fixture) = open_disposable_fixture();

    assert_eq!(fixture.all_cases.len(), 3);
    for case in fixture.all_cases {
        let hits = db
            .search_messages(
                "",
                &MessageFilters {
                    field: Some(case.field),
                    argument_path: case.argument_path.clone(),
                    match_mode: MessageSearchMode::Literal,
                    ..Default::default()
                },
            )
            .unwrap_or_else(|error| panic!("all {:?}: {error:#}", case.field));
        assert_eq!(seqs(hits), case.expected_seq, "{case:?}");
    }
}

#[test]
fn current_validation_errors_are_consistent_across_fields() {
    let (_directory, db, _fixture) = open_disposable_fixture();

    for field in [
        SearchField::Content,
        SearchField::ToolName,
        SearchField::ToolArgument,
    ] {
        let filters = |mode, limit| MessageFilters {
            field: Some(field),
            argument_path: (field == SearchField::ToolArgument).then(|| "/cmd".into()),
            match_mode: mode,
            limit,
            ..Default::default()
        };
        let invalid_regex = db
            .search_messages("[", &filters(MessageSearchMode::Regex, 10))
            .unwrap_err()
            .to_string();
        assert!(
            invalid_regex.contains("invalid regex"),
            "{field:?}: {invalid_regex}"
        );

        let short_fuzzy = db
            .search_messages("ab", &filters(MessageSearchMode::Fuzzy, 10))
            .unwrap_err()
            .to_string();
        assert!(
            short_fuzzy.contains("at least 3 characters"),
            "{field:?}: {short_fuzzy}"
        );
    }
}

#[test]
fn typed_service_preserves_frozen_modes_fields_results_and_planner_receipts() {
    let (_directory, db, fixture) = open_disposable_fixture();
    let config = Config::default();
    let service = MessageService::new(&config, &db, SearchSurface::Rust);

    for case in fixture.text_cases {
        let query = match case.mode {
            MessageSearchMode::Literal => MessageQuery::literal(&case.query).unwrap(),
            MessageSearchMode::Regex => MessageQuery::regex(&case.query).unwrap(),
            MessageSearchMode::Fuzzy => MessageQuery::fuzzy(&case.query).unwrap(),
        };
        let target = match case.field {
            SearchField::Content => MessageTarget::content(),
            SearchField::ToolName => MessageTarget::tool_name(),
            SearchField::ToolArgument => {
                MessageTarget::tool_argument(case.argument_path.as_deref().unwrap_or("")).unwrap()
            }
        };
        let request = MessageSearchRequest::builder(query, target)
            .kind(MessageKind::ToolCall)
            .extent(RequestedExtent::page(Some(10), 0).unwrap())
            .receipt_level(ReceiptLevel::Full)
            .build()
            .unwrap();
        let response = service
            .search(request)
            .unwrap_or_else(|error| panic!("{:?}/{:?}: {error:#}", case.field, case.mode));

        assert_eq!(
            response
                .hits()
                .iter()
                .map(|hit| hit.seq)
                .collect::<Vec<_>>(),
            case.expected_seq,
            "{case:?}"
        );
        let receipt = response
            .search_explanation()
            .unwrap_or_else(|| panic!("missing typed planner receipt for {case:?}"));
        assert_eq!(receipt.corpus, case.expected_corpus, "{case:?}");
        assert!(receipt.candidates.is_some(), "{case:?}: {receipt:?}");
        assert!(response.parameter_origins().is_some(), "{case:?}");
        let document = serde_json::to_value(response.document()).unwrap();
        assert!(
            document["receipt"]["search_explanation"].is_object(),
            "{case:?}"
        );
        assert!(
            document["receipt"]["parameter_origins"].is_object(),
            "{case:?}"
        );
        assert!(
            document["receipt"]["ordered_digest"]
                .as_str()
                .is_some_and(|digest| digest.starts_with("sha256:")),
            "{case:?}"
        );
    }
}

#[test]
fn receipt_levels_distinguish_planner_summary_from_full_origins() {
    let (_directory, db, _fixture) = open_disposable_fixture();
    let config = Config::default();
    let service = MessageService::new(&config, &db, SearchSurface::Rust);

    for (level, expect_planner, expect_origins) in [
        (ReceiptLevel::None, false, false),
        (ReceiptLevel::Summary, true, false),
        (ReceiptLevel::Full, true, true),
    ] {
        let response = service
            .search(
                MessageSearchRequest::builder(
                    MessageQuery::literal("needle").unwrap(),
                    MessageTarget::content(),
                )
                .extent(RequestedExtent::page(Some(2), 0).unwrap())
                .receipt_level(level)
                .build()
                .unwrap(),
            )
            .unwrap();
        assert_eq!(
            response.search_explanation().is_some(),
            expect_planner,
            "{level:?}"
        );
        assert_eq!(
            response.parameter_origins().is_some(),
            expect_origins,
            "{level:?}"
        );
        let document = serde_json::to_value(response.document()).unwrap();
        let receipt = response
            .receipt_document()
            .map(|receipt| serde_json::to_value(receipt).unwrap());
        assert_eq!(
            receipt.as_ref(),
            document.get("receipt"),
            "incremental receipt framing must equal the canonical document for {level:?}"
        );
        match level {
            ReceiptLevel::None => assert!(document.get("receipt").is_none()),
            ReceiptLevel::Summary => {
                assert!(document["receipt"]["search_explanation"].is_object());
                assert!(document["receipt"].get("parameter_origins").is_none());
                assert!(document["receipt"].get("ordered_digest").is_none());
            }
            ReceiptLevel::Full => {
                assert!(document["receipt"]["search_explanation"].is_object());
                assert!(document["receipt"]["parameter_origins"].is_object());
                assert!(document["receipt"]["ordered_digest"].is_string());
            }
        }
    }
}

#[test]
fn ordered_digest_is_invariant_under_presentation_budgets() {
    let (_directory, db, fixture) = open_disposable_fixture();
    let case = fixture.semantic_response_case;
    insert_semantic_response_case(&db, &case);
    let config = Config::default();
    let service = MessageService::new(&config, &db, SearchSurface::Rust);

    let search = |field_view, match_view| {
        service
            .search(
                MessageSearchRequest::builder(
                    MessageQuery::literal(&case.query).unwrap(),
                    MessageTarget::content(),
                )
                .session_id(&case.session_id)
                .unwrap()
                .field_view(field_view)
                .match_view(match_view)
                .receipt_level(ReceiptLevel::Full)
                .build()
                .unwrap(),
            )
            .unwrap()
    };
    let compact = search(
        FieldViewBudget::max_chars(21).unwrap(),
        MatchViewBudget::max_chars(20).unwrap(),
    );
    let full = search(FieldViewBudget::NoCharLimit, MatchViewBudget::MinimalSpan);

    assert_ne!(
        compact.results()[0].field_view().text(),
        full.results()[0].field_view().text()
    );
    assert_eq!(compact.ordered_digest(), full.ordered_digest());
    assert_eq!(
        serde_json::to_value(compact.document()).unwrap()["receipt"]["ordered_digest"],
        serde_json::to_value(full.document()).unwrap()["receipt"]["ordered_digest"]
    );
}

#[test]
fn queryless_ordered_digest_distinguishes_the_selected_field() {
    let (_directory, db, fixture) = open_disposable_fixture();
    let case = fixture.semantic_response_case;
    insert_semantic_response_case(&db, &case);
    let config = Config::default();
    let service = MessageService::new(&config, &db, SearchSurface::Rust);
    let digest = |target| {
        service
            .search(
                MessageSearchRequest::builder(MessageQuery::All, target)
                    .session_id(&case.session_id)
                    .unwrap()
                    .receipt_level(ReceiptLevel::Full)
                    .build()
                    .unwrap(),
            )
            .unwrap()
            .ordered_digest()
    };

    assert_ne!(
        digest(MessageTarget::content()),
        digest(MessageTarget::tool_name())
    );
    assert_ne!(
        digest(MessageTarget::content()),
        digest(MessageTarget::tool_argument("/command").unwrap())
    );
}

#[test]
fn typed_service_preserves_frozen_no_text_field_presence() {
    let (_directory, db, fixture) = open_disposable_fixture();
    let config = Config::default();
    let service = MessageService::new(&config, &db, SearchSurface::Rust);

    for case in fixture.all_cases {
        let target = match case.field {
            SearchField::Content => MessageTarget::content(),
            SearchField::ToolName => MessageTarget::tool_name(),
            SearchField::ToolArgument => {
                MessageTarget::tool_argument(case.argument_path.as_deref().unwrap_or("")).unwrap()
            }
        };
        let response = service
            .search(
                MessageSearchRequest::builder(MessageQuery::All, target)
                    .build()
                    .unwrap(),
            )
            .unwrap_or_else(|error| panic!("all {:?}: {error:#}", case.field));
        assert_eq!(
            response
                .hits()
                .iter()
                .map(|hit| hit.seq)
                .collect::<Vec<_>>(),
            case.expected_seq,
            "{case:?}"
        );
        let document = serde_json::to_value(response.document()).unwrap();
        assert!(
            document["results"]
                .as_array()
                .unwrap()
                .iter()
                .all(|result| result.get("match").is_none()),
            "queryless results select values but do not claim match evidence: {case:?}"
        );
    }
}

#[test]
fn semantic_response_contract_exposes_one_hit_two_views_and_truthful_page_extent() {
    let (_directory, db, fixture) = open_disposable_fixture();
    let case = fixture.semantic_response_case;
    insert_semantic_response_case(&db, &case);
    let config = Config::default();
    let service = MessageService::new(&config, &db, SearchSurface::Rust);

    let response = service
        .search(
            MessageSearchRequest::builder(
                MessageQuery::literal(&case.query).unwrap(),
                MessageTarget::content(),
            )
            .session_id(&case.session_id)
            .unwrap()
            .extent(RequestedExtent::page(Some(1), 0).unwrap())
            .field_view(FieldViewBudget::max_chars(case.boundary_view_chars).unwrap())
            .match_view(MatchViewBudget::max_chars(case.match_view_chars).unwrap())
            .build()
            .unwrap(),
        )
        .unwrap();

    let result = &response.results()[0];
    assert_eq!(response.request().query(), Some(case.query.as_str()));
    assert_eq!(response.request().presentation().lines_per_message(), 0);
    assert!(response.request().include().is_empty());
    assert_eq!(result.message_ref().session_id(), case.session_id);
    assert_eq!(result.message_ref().message_seq(), case.message_seq);
    assert_eq!(result.field_view().text(), "always explore before");
    assert_eq!(result.field_view().field_start_char(), 0);
    assert_eq!(
        result.field_view().field_end_char_exclusive(),
        case.boundary_view_chars
    );
    assert_eq!(
        result
            .field_view()
            .extent()
            .additional_field_text()
            .as_str(),
        "after"
    );

    let match_view = result.match_view().expect("queried hit has match view");
    assert_eq!(match_view.text(), case.query);
    assert_eq!(match_view.field_start_char(), case.literal_start);
    assert_eq!(
        match_view.markers()[0].view_start_char,
        0,
        "markers are relative to the returned match view"
    );
    assert_eq!(
        match_view.markers()[0].view_end_char_exclusive,
        case.match_view_chars
    );
    let literal = result.literal_match().expect("literal occurrence");
    assert_eq!(literal.field_start_char, case.literal_start);
    assert_eq!(literal.field_end_char_exclusive, case.literal_end);

    assert_eq!(response.page().returned(), 1);
    assert_eq!(response.page().earlier_results().as_str(), "none");
    assert_eq!(response.page().result_set_extent().as_str(), "partial");
    assert_eq!(response.page().next_offset(), Some(1));

    let document = serde_json::to_value(response.document()).unwrap();
    assert_eq!(
        serde_json::to_value(response.result_document(0).unwrap()).unwrap(),
        document["results"][0],
        "incremental result framing must reuse the canonical result projection"
    );
    assert_eq!(
        serde_json::to_value(response.page_document()).unwrap(),
        document["page"],
        "incremental page framing must reuse the canonical page projection"
    );
    let top_level = document.as_object().expect("response document object");
    assert_eq!(
        top_level.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "effective_request",
            "page",
            "response_schema_version",
            "results"
        ],
        "the default document has one compact semantic core and no empty receipt"
    );
    assert_eq!(document["response_schema_version"], 1);
    assert_eq!(document["effective_request"]["query"], case.query);
    assert_eq!(document["effective_request"]["query_mode"], "literal");
    assert_eq!(
        document["effective_request"]["presentation"]["field_view"]["kind"],
        "max_chars"
    );
    assert_eq!(
        document["effective_request"]["presentation"]["field_view"]["max_chars"],
        case.boundary_view_chars
    );
    assert!(
        document["effective_request"]["presentation"]
            .get("detail")
            .is_none(),
        "resolved exact budgets make the input convenience preset redundant"
    );
    assert_eq!(
        document["results"][0]["message_ref"]["session_id"],
        case.session_id
    );
    assert_eq!(
        document["results"][0]["message_ref"]["message_seq"],
        case.message_seq
    );
    assert_eq!(
        document["results"][0]["message_metadata"]["provider"],
        "codex"
    );
    assert!(document["results"][0].get("expand").is_none());
    assert_eq!(
        document["results"][0]["presentation"]["field_view"]["text"],
        "always explore before"
    );
    assert_eq!(
        document["results"][0]["presentation"]["match_view"]["text"],
        case.query
    );
    assert_eq!(
        document["results"][0]["presentation"]["match_view"]["markers"][0],
        serde_json::json!({
            "view_start_char": 0,
            "view_end_char_exclusive": case.match_view_chars
        })
    );
    assert_eq!(
        document["results"][0]["presentation"]["match_view"]["field_start_char"],
        case.literal_start
    );
    assert_eq!(
        document["results"][0]["presentation"]["field_view"]["extent"]["field_total_chars"],
        case.content.chars().count()
    );
    assert_eq!(
        document["results"][0]["presentation"]["field_view"]["field_end_char_exclusive"],
        case.boundary_view_chars
    );
    assert_eq!(
        document["results"][0]["presentation"]["field_view"]["extent"]["additional_field_text"],
        "after"
    );
    assert_eq!(
        document["results"][0]["match"]["literal_occurrence"]["field_start_char"],
        case.literal_start
    );
    assert_eq!(
        document["results"][0]["match"]["literal_occurrence"]["field_end_char_exclusive"],
        case.literal_end
    );
    assert_eq!(document["page"]["earlier_results"], "none");
    assert_eq!(document["page"]["ordering"], "session-sequence");
    assert_eq!(document["page"]["consistency"], "per-call");
    assert_eq!(document["page"]["result_set_extent"], "partial");
    assert!(
        document.get("hits").is_none() && document.get("query").is_none(),
        "legacy Rust storage names must not leak into the canonical document"
    );
    let serialized = serde_json::to_string(&document).unwrap();
    for rejected_name in [
        "omitted_start",
        "omitted_end",
        "source_continues_before",
        "source_continues_after",
        "\"complete\":",
        "\"source_start\":",
        "\"source_end\":",
        "\"start\":",
        "\"end\":",
        "\"ref\":",
        "\"expand\":",
        "\"message\":",
        "\"planner\":",
        "\"origins\":",
        "\"boundary_view\":",
        "\"contains_full_field_text\":",
        "\"field_has_text_before\":",
        "\"field_has_text_after\":",
        "\"returned_chars\":",
        "\"original_chars\":",
        "\"request\":",
        "\"value_view\":",
        "\"value_start_char\":",
        "\"value_end_char\":",
        "\"value_text_coverage\":",
        "\"total_value_chars\":",
        "\"field_end_char\":",
        "\"field_text_coverage\":",
        "\"total_field_chars\":",
        "\"later_results\":",
        "\"start_char\":",
        "\"end_char\":",
    ] {
        assert!(
            !serialized.contains(rejected_name),
            "rejected ambiguous extent name leaked into the canonical document: {rejected_name}"
        );
    }
}

#[test]
fn empty_positive_offset_does_not_invent_earlier_results() {
    let (_directory, db, fixture) = open_disposable_fixture();
    let case = fixture.semantic_response_case;
    insert_semantic_response_case(&db, &case);
    let config = Config::default();
    let service = MessageService::new(&config, &db, SearchSurface::Rust);

    let response = service
        .search(
            MessageSearchRequest::builder(
                MessageQuery::literal(&case.query).unwrap(),
                MessageTarget::content(),
            )
            .session_id(&case.session_id)
            .unwrap()
            .extent(RequestedExtent::page(Some(1), 50).unwrap())
            .build()
            .unwrap(),
        )
        .unwrap();

    assert!(response.results().is_empty());
    assert_eq!(response.page().earlier_results().as_str(), "unknown");
    assert_eq!(response.page().result_set_extent().as_str(), "unknown");
}

#[test]
fn direct_response_serialization_uses_the_canonical_document_owner() {
    let (_directory, db, fixture) = open_disposable_fixture();
    let case = fixture.semantic_response_case;
    insert_semantic_response_case(&db, &case);
    let config = Config::default();
    let response = MessageService::new(&config, &db, SearchSurface::Rust)
        .search(
            MessageSearchRequest::builder(
                MessageQuery::literal(&case.query).unwrap(),
                MessageTarget::content(),
            )
            .session_id(&case.session_id)
            .unwrap()
            .build()
            .unwrap(),
        )
        .unwrap();

    assert_eq!(
        serde_json::to_value(&response).unwrap(),
        serde_json::to_value(response.document()).unwrap()
    );
}

#[test]
fn full_detail_preset_overrides_surface_line_and_character_defaults() {
    let (_directory, db, fixture) = open_disposable_fixture();
    let case = fixture.semantic_response_case;
    insert_semantic_response_case(&db, &case);
    let mut config = Config::default();
    config.mcp.lines_per_message = 1;
    config.mcp.preview_chars = 10;
    let response = MessageService::new(&config, &db, SearchSurface::Mcp)
        .search(
            MessageSearchRequest::builder(
                MessageQuery::literal(&case.query).unwrap(),
                MessageTarget::content(),
            )
            .session_id(&case.session_id)
            .unwrap()
            .detail(DetailLevel::Full)
            .build()
            .unwrap(),
        )
        .unwrap();

    assert_eq!(response.request().presentation().lines_per_message(), 0);
    assert_eq!(response.results()[0].field_view().text(), case.content);
    assert_eq!(
        response.results()[0]
            .field_view()
            .extent()
            .additional_field_text()
            .as_str(),
        "none"
    );
}

#[test]
fn every_advertised_include_and_context_parameter_changes_the_document() {
    let (_directory, db, fixture) = open_disposable_fixture();
    let case = fixture.semantic_response_case;
    insert_semantic_response_case(&db, &case);
    let config = Config::default();
    let response = MessageService::new(&config, &db, SearchSurface::Rust)
        .search(
            MessageSearchRequest::builder(
                MessageQuery::literal(&case.query).unwrap(),
                MessageTarget::content(),
            )
            .session_id(&case.session_id)
            .unwrap()
            .context(ContextWindow::new(0, 1))
            .includes([
                MessageSearchInclude::NormalizedSessionMetadata,
                MessageSearchInclude::ParsedReferences,
                MessageSearchInclude::RawProviderMetadata,
                MessageSearchInclude::RuntimeDiagnostics,
            ])
            .build()
            .unwrap(),
        )
        .unwrap();
    let document = serde_json::to_value(response.document()).unwrap();

    assert_eq!(
        document["included"]["normalized_session_metadata"][&case.session_id]
            ["provider_session_id"],
        case.provider_session_id
    );
    assert_eq!(
        document["included"]["raw_provider_metadata"][&case.session_id]["model"],
        "gpt-test"
    );
    assert_eq!(
        document["included"]["runtime_diagnostics"]["response_schema_version"],
        1
    );
    assert_eq!(
        document["included"]["runtime_diagnostics"]["database_schema_version"],
        SCHEMA_VERSION
    );
    assert_eq!(
        document["included"]["runtime_diagnostics"]["surface"],
        "rust"
    );
    assert!(document["included"]["runtime_diagnostics"]["config_digest"]
        .as_str()
        .is_some_and(|digest| digest.starts_with("sha256:")));
    assert_eq!(
        document["results"][0]["included"]["parsed_references"][0]["value"],
        "https://example.com"
    );
    assert_eq!(
        document["results"][0]["context"]["messages_after"][0]["message_ref"]["message_seq"],
        case.message_seq + 1
    );
}

#[test]
fn boundary_line_window_cannot_erase_a_match_on_line_ten_thousand() {
    let (_directory, db, fixture) = open_disposable_fixture();
    let case = fixture.semantic_response_case;
    let mut content = (0..9_999)
        .map(|line| format!("boundary line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    content.push_str("\nsemantic duplication survives");
    db.upsert_session(
        &ParsedSession {
            session: SessionRecord {
                id: case.session_id.clone(),
                provider: Provider::Codex,
                provider_session_id: case.provider_session_id.clone(),
                title: None,
                summary: None,
                cwd: Some("/fixture/workspace".into()),
                repo_root: Some("/fixture".into()),
                created_at: None,
                updated_at: None,
                last_message_at: None,
                preview_text: String::new(),
                source_path: "/fixture/transcripts/late-match.jsonl".into(),
                message_count: Some(1),
                parse_version: "contract-fixture-v1".into(),
                raw_metadata_json: None,
                parse_warning: None,
                discovery_source: "test-fixture".into(),
                parent_session_id: None,
                agent_label: None,
            },
            transcript_text: content.clone(),
            messages: vec![Message {
                seq: case.message_seq,
                role: Role::User,
                ts: None,
                tool_name: None,
                kind: MessageKind::Conversation,
                tool_call_id: None,
                content,
                is_compaction: false,
                provenance: Default::default(),
            }],
            file_edits: Vec::new(),
        },
        0,
        0,
    )
    .unwrap();
    let config = Config::default();
    let service = MessageService::new(&config, &db, SearchSurface::Rust);

    let response = service
        .search(
            MessageSearchRequest::builder(
                MessageQuery::literal(&case.query).unwrap(),
                MessageTarget::content(),
            )
            .session_id(&case.session_id)
            .unwrap()
            .message_lines(ai_session_search::LineWindow::from_signed(1).unwrap())
            .match_view(MatchViewBudget::max_chars(case.match_view_chars).unwrap())
            .build()
            .unwrap(),
        )
        .unwrap();

    let result = &response.results()[0];
    assert_eq!(result.field_view().text(), "boundary line 0");
    assert_eq!(
        result
            .field_view()
            .extent()
            .additional_field_text()
            .as_str(),
        "after"
    );
    assert_eq!(
        result.match_view().expect("match view").text(),
        case.query,
        "match proof is derived from authoritative full text, not the boundary line window"
    );
}
