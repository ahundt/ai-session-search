// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

//! Public-surface characterization tests for the search-result authority boundary.
//!
//! The legacy `all` and `allowed-roots` modes are compatibility inputs for the
//! permissions policy. These tests freeze their result membership before that
//! policy is generalized.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use ai_session_search::config::{Config, IndexRefresh, SearchScopeConfig, SearchScopeMode};
use ai_session_search::db::Db;
use ai_session_search::message_search::{
    MessageQuery, MessageSearchRequest, MessageTarget, SourceCompleteness,
};
use ai_session_search::models::{
    FileEdit, FileQuery, Message, MessageKind, ParsedSession, Provider, Role, SearchFilters,
    SessionRecord,
};
use ai_session_search::search_scope::{
    PolicyDecision, SearchOperation, SessionPolicySelector, SessionPolicyTarget,
    TemporaryGrantCoverage, TrustedPolicyInputs,
};
use ai_session_search::{SessionSearch, TrustedAccessInputs};

fn parsed_session(id: &str, workspace: &Path, transcript: &Path) -> ParsedSession {
    ParsedSession {
        session: SessionRecord {
            id: id.to_owned(),
            provider: Provider::Claude,
            provider_session_id: id.replace(':', "-"),
            title: Some(format!("permission fixture {id}")),
            summary: None,
            cwd: Some(workspace.to_string_lossy().into_owned()),
            repo_root: Some(workspace.to_string_lossy().into_owned()),
            created_at: None,
            updated_at: None,
            last_message_at: None,
            preview_text: "permission-contract-needle".to_owned(),
            source_path: transcript.to_string_lossy().into_owned(),
            message_count: Some(1),
            parse_version: "permission-contract-v1".to_owned(),
            raw_metadata_json: None,
            parse_warning: None,
            discovery_source: "synthetic-test".to_owned(),
            parent_session_id: None,
            agent_label: None,
        },
        transcript_text: "permission-contract-needle transcript".to_owned(),
        messages: vec![
            Message {
                seq: 0,
                role: Role::User,
                ts: None,
                tool_name: None,
                kind: MessageKind::Conversation,
                tool_call_id: None,
                is_compaction: false,
                content: "permission-contract-needle user message".to_owned(),
                provenance: Default::default(),
            },
            Message {
                seq: 1,
                role: Role::Assistant,
                ts: None,
                tool_name: Some("Read".to_owned()),
                kind: MessageKind::Conversation,
                tool_call_id: None,
                is_compaction: false,
                content: "permission-contract-needle assistant message".to_owned(),
                provenance: Default::default(),
            },
        ],
        file_edits: vec![
            FileEdit {
                seq: 0,
                ts: None,
                tool: "Write".to_owned(),
                file_path: workspace
                    .join("permission.txt")
                    .to_string_lossy()
                    .into_owned(),
                file_name: "permission.txt".to_owned(),
                new_content: Some(format!("content from {id}")),
                edits: Vec::new(),
            },
            FileEdit {
                seq: 1,
                ts: None,
                tool: "Write".to_owned(),
                file_path: workspace.join("secret.txt").to_string_lossy().into_owned(),
                file_name: "secret.txt".to_owned(),
                new_content: Some(format!("secret from {id}")),
                edits: Vec::new(),
            },
        ],
    }
}

fn session_ids(app: &SessionSearch) -> BTreeSet<String> {
    app.catalog()
        .list_sessions(&SearchFilters {
            limit: 0,
            ..SearchFilters::default()
        })
        .unwrap()
        .into_iter()
        .map(|session| session.id)
        .collect()
}

fn seed_two_session_database(root: &Path, allowed: &Path, hidden: &Path) -> std::path::PathBuf {
    fs::create_dir_all(allowed).unwrap();
    fs::create_dir_all(hidden).unwrap();
    let db_path = root.join("index.db");
    let db = Db::open(&db_path).unwrap();
    db.upsert_session(
        &parsed_session(
            "claude:allowed",
            &allowed.join("project"),
            &hidden.join("allowed.jsonl"),
        ),
        0,
        0,
    )
    .unwrap();
    db.upsert_session(
        &parsed_session(
            "claude:hidden",
            &hidden.join("project"),
            &allowed.join("hidden.jsonl"),
        ),
        0,
        0,
    )
    .unwrap();
    drop(db);
    db_path
}

#[test]
fn operation_specific_blocks_reach_analyze_export_and_restore_sql() {
    let root = tempfile::tempdir().unwrap();
    let blocked = root.path().join("blocked");
    let other = root.path().join("other");
    let db_path = seed_two_session_database(root.path(), &blocked, &other);
    let mut config: Config = toml::from_str(&format!(
        r#"
[search.permissions]
default_profile = "operation"

[search.permissions.profiles.operation]
default = "allow"

[[search.permissions.profiles.operation.rules]]
rule_id = "no-analysis"
effect = "block"
resource = "session"
operation = ["analyze"]
session_workspace_root = [{blocked:?}]

[[search.permissions.profiles.operation.rules]]
rule_id = "no-export"
effect = "block"
resource = "session"
operation = ["export"]
session_workspace_root = [{blocked:?}]

[[search.permissions.profiles.operation.rules]]
rule_id = "no-restore"
effect = "block"
resource = "session"
operation = ["restore-files"]
session_workspace_root = [{blocked:?}]
"#,
        blocked = blocked.to_string_lossy(),
    ))
    .unwrap();
    config.index.db_path = Some(db_path.to_string_lossy().into_owned());
    config.index.refresh = IndexRefresh::ExistingOnly;
    let app = SessionSearch::open(config).unwrap();

    assert_eq!(session_ids(&app).len(), 2, "read remains allowed");
    let documents = app
        .analysis()
        .documents(
            &SearchFilters {
                limit: 10,
                ..SearchFilters::default()
            },
            None,
        )
        .unwrap();
    assert_eq!(documents.documents.len(), 1);
    assert_eq!(documents.documents[0].session.id, "claude:hidden");

    let export_error = app
        .exports()
        .render_full(
            "claude:allowed",
            ai_session_search::export::ExportFormat::Json,
        )
        .unwrap_err()
        .to_string();
    assert!(
        export_error.contains("no session matches"),
        "{export_error}"
    );
    let restore_error = app
        .files()
        .reconstruct(
            blocked.join("project/permission.txt").to_str().unwrap(),
            &FileQuery::default(),
            None,
        )
        .unwrap_err()
        .to_string();
    assert!(
        restore_error.contains("no file edits") || restore_error.contains("not found"),
        "{restore_error}"
    );
}

#[test]
fn admitted_workspace_grant_changes_only_matching_sql_membership() {
    let root = tempfile::tempdir().unwrap();
    let granted = root.path().join("grant");
    let sibling = root.path().join("sibling");
    let secret = granted.join("secret");
    fs::create_dir_all(&secret).unwrap();
    fs::create_dir_all(&sibling).unwrap();
    let db_path = root.path().join("index.db");
    let db = Db::open(&db_path).unwrap();
    for (id, workspace) in [
        ("claude:granted", granted.join("client")),
        ("claude:secret", secret.clone()),
        ("claude:sibling", sibling.clone()),
    ] {
        db.upsert_session(
            &parsed_session(id, &workspace, &root.path().join(format!("{id}.jsonl"))),
            0,
            0,
        )
        .unwrap();
    }
    drop(db);

    let mut config: Config = toml::from_str(&format!(
        r#"
[search.permissions]
default_profile = "restricted"

[search.permissions.profiles.restricted]
default = "block"

[[search.permissions.profiles.restricted.rules]]
rule_id = "secret"
effect = "hard-block"
resource = "session"
session_workspace_root = [{secret:?}]

[search.permissions.profiles.restricted.grant_envelope]
operation = ["read"]
session_workspace_root = [{granted:?}]
max_uses = 2
"#,
        secret = secret.to_string_lossy(),
        granted = granted.to_string_lossy(),
    ))
    .unwrap();
    config.index.db_path = Some(db_path.to_string_lossy().into_owned());
    config.index.refresh = IndexRefresh::ExistingOnly;
    let app = SessionSearch::open_with_policy_inputs(
        config,
        TrustedAccessInputs::default(),
        TrustedPolicyInputs {
            admitted_grant_selectors: vec![SessionPolicySelector::workspace_root(
                SearchOperation::Read,
                &granted,
            )],
            ..TrustedPolicyInputs::default()
        },
    )
    .unwrap();

    assert_eq!(
        session_ids(&app),
        BTreeSet::from(["claude:granted".to_owned()])
    );
    let messages = app
        .messages()
        .search(
            MessageSearchRequest::builder(
                MessageQuery::literal("permission-contract-needle").unwrap(),
                MessageTarget::content(),
            )
            .build()
            .unwrap(),
        )
        .unwrap();
    assert_eq!(messages.hits().len(), 2);
    assert!(messages
        .hits()
        .iter()
        .all(|hit| hit.session_id == "claude:granted"));
    let files = app.files().search(&FileQuery::default()).unwrap();
    assert_eq!(files.len(), 2);
    assert!(files
        .iter()
        .all(|file| Path::new(&file.file_path).starts_with(granted.join("client"))));
}

#[test]
fn legacy_scope_membership_is_identical_across_public_read_services() {
    let root = tempfile::tempdir().unwrap();
    let allowed = root.path().join("allowed");
    let hidden = root.path().join("hidden");
    let db_path = seed_two_session_database(root.path(), &allowed, &hidden);

    let mut unrestricted_config = Config::default();
    unrestricted_config.index.db_path = Some(db_path.to_string_lossy().into_owned());
    unrestricted_config.index.refresh = IndexRefresh::ExistingOnly;
    let unrestricted = SessionSearch::open(unrestricted_config).unwrap();

    let mut restricted_config = Config::default();
    restricted_config.index.db_path = Some(db_path.to_string_lossy().into_owned());
    restricted_config.index.refresh = IndexRefresh::ExistingOnly;
    restricted_config.search.scope = SearchScopeConfig {
        mode: SearchScopeMode::AllowedRoots,
        roots: vec![allowed.to_string_lossy().into_owned()],
        include_invocation_directory: false,
    };
    let restricted =
        SessionSearch::open_with_access_inputs(restricted_config, TrustedAccessInputs::default())
            .unwrap();

    assert_eq!(
        session_ids(&unrestricted),
        BTreeSet::from(["claude:allowed".to_owned(), "claude:hidden".to_owned()])
    );
    assert_eq!(
        session_ids(&restricted),
        BTreeSet::from(["claude:allowed".to_owned()])
    );

    let request = || {
        MessageSearchRequest::builder(
            MessageQuery::literal("permission-contract-needle").unwrap(),
            MessageTarget::content(),
        )
        .build()
        .unwrap()
    };
    assert_eq!(
        unrestricted
            .messages()
            .search(request())
            .unwrap()
            .hits()
            .len(),
        4
    );
    let restricted_messages = restricted.messages().search(request()).unwrap();
    assert_eq!(restricted_messages.hits().len(), 2);
    assert_eq!(restricted_messages.hits()[0].session_id, "claude:allowed");

    assert_eq!(
        unrestricted
            .files()
            .search(&FileQuery::default())
            .unwrap()
            .len(),
        4
    );
    let restricted_files = restricted.files().search(&FileQuery::default()).unwrap();
    assert_eq!(restricted_files.len(), 2);
    assert!(Path::new(&restricted_files[0].file_path).starts_with(&allowed));

    let hidden_error = restricted
        .catalog()
        .resolve_session("claude:hidden")
        .unwrap_err()
        .to_string();
    let missing_error = restricted
        .catalog()
        .resolve_session("claude:missing")
        .unwrap_err()
        .to_string();
    assert!(
        hidden_error.contains("no session matches"),
        "{hidden_error}"
    );
    assert!(
        missing_error.contains("no session matches"),
        "{missing_error}"
    );
    assert!(!hidden_error.contains(hidden.to_string_lossy().as_ref()));
}

#[test]
fn typed_session_policy_filters_before_session_message_and_file_reads() {
    let root = tempfile::tempdir().unwrap();
    let allowed = root.path().join("allowed");
    let hidden = root.path().join("hidden");
    let db_path = seed_two_session_database(root.path(), &allowed, &hidden);
    let allowed_toml = toml::Value::String(allowed.to_string_lossy().into_owned()).to_string();
    let secret_file_toml = toml::Value::String(
        allowed
            .join("project/secret.txt")
            .to_string_lossy()
            .into_owned(),
    )
    .to_string();
    let source = format!(
        r#"
[search.permissions]
default_profile = "project"

[search.permissions.profiles.project]
default = "hard-block"

[[search.permissions.profiles.project.rules]]
rule_id = "project-read"
effect = "allow"
resource = "session"
operation = ["read"]
session_workspace_root = [{allowed_toml}]

[[search.permissions.profiles.project.rules]]
rule_id = "hide-assistant"
effect = "hard-block"
resource = "message"
message_role = ["assistant"]

[[search.permissions.profiles.project.rules]]
rule_id = "hide-secret-files"
effect = "hard-block"
resource = "file-edit"
file_path = [{secret_file_toml}]
"#,
    );
    let mut config: Config = toml::from_str(&source).unwrap();
    config.validate().unwrap();
    config.index.db_path = Some(db_path.to_string_lossy().into_owned());
    config.index.refresh = IndexRefresh::ExistingOnly;

    let app = SessionSearch::open(config).unwrap();
    let visible_ids = session_ids(&app);
    assert_eq!(visible_ids, BTreeSet::from(["claude:allowed".to_owned()]));
    for (id, workspace, transcript) in [
        (
            "claude:allowed",
            allowed.join("project"),
            hidden.join("allowed.jsonl"),
        ),
        (
            "claude:hidden",
            hidden.join("project"),
            allowed.join("hidden.jsonl"),
        ),
    ] {
        let decision = app.search_policy().evaluate_session(
            &SessionPolicyTarget::new(
                SearchOperation::Read,
                Provider::Claude,
                &workspace,
                &transcript,
            ),
            TemporaryGrantCoverage::None,
        );
        assert_eq!(
            decision == PolicyDecision::Allow,
            visible_ids.contains(id),
            "pure evaluator and SQL membership diverged for {id}: {decision:?}"
        );
    }

    let response = app
        .messages()
        .search(
            MessageSearchRequest::builder(
                MessageQuery::literal("permission-contract-needle").unwrap(),
                MessageTarget::content(),
            )
            .build()
            .unwrap(),
        )
        .unwrap();
    assert_eq!(response.hits().len(), 1);
    assert_eq!(response.hits()[0].session_id, "claude:allowed");
    assert_eq!(
        response.source_completeness(),
        SourceCompleteness::PolicyRestricted
    );
    assert_eq!(app.files().search(&FileQuery::default()).unwrap().len(), 1);
    let analysis_error = app
        .analysis()
        .documents(
            &SearchFilters {
                limit: 10,
                ..SearchFilters::default()
            },
            None,
        )
        .unwrap_err()
        .to_string();
    assert!(
        analysis_error.contains("message-level policy"),
        "{analysis_error}"
    );
    assert!(
        analysis_error.contains("messages search"),
        "{analysis_error}"
    );
    let hidden_error = app
        .catalog()
        .resolve_session("claude:hidden")
        .unwrap_err()
        .to_string();
    assert!(
        hidden_error.contains("no session matches"),
        "{hidden_error}"
    );
}
