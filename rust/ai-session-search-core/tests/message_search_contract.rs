use std::path::Path;

use ai_session_search::config::Config;
use ai_session_search::db::Db;
use ai_session_search::message_search::SearchSurface;
use ai_session_search::models::{
    Message, MessageFilters, MessageKind, MessageSearchMode, ParsedSession, Provider, SearchField,
    SessionRecord,
};
use ai_session_search::service::MessageService;
use ai_session_search::{
    MessageQuery, MessageSearchRequest, MessageTarget, ReceiptLevel, RequestedExtent,
};
use serde::Deserialize;

const CONTRACT_FIXTURE: &str = include_str!("fixtures/message_search_contract.json");

#[derive(Debug, Deserialize)]
struct ContractFixture {
    messages: Vec<Message>,
    text_cases: Vec<TextCase>,
    all_cases: Vec<AllCase>,
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
                raw_metadata_json: None,
                parse_warning: None,
                discovery_source: "test-fixture".into(),
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

#[test]
fn current_text_modes_fields_results_paging_and_planner_are_frozen() {
    let (_directory, db, fixture) = open_disposable_fixture();

    assert_eq!(fixture.text_cases.len(), 9);
    for case in fixture.text_cases {
        let filters = MessageFilters {
            kind: Some(MessageKind::ToolCall),
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
                    match_mode: MessageSearchMode::Exact,
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
            MessageSearchMode::Exact => MessageQuery::literal(&case.query).unwrap(),
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
            .receipt_level(ReceiptLevel::Summary)
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
            .planner()
            .unwrap_or_else(|| panic!("missing typed planner receipt for {case:?}"));
        assert_eq!(receipt.corpus, case.expected_corpus, "{case:?}");
        assert!(receipt.candidates.is_some(), "{case:?}: {receipt:?}");
        assert!(response.origins().is_some(), "{case:?}");
    }
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
    }
}
