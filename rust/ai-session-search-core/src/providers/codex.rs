// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-FileCopyrightText: 2026 Nisarg Patel
// SPDX-License-Identifier: Apache-2.0

use super::health::{
    RecordDisposition::{Ignored, Indexed},
    RecordTypeRule,
};

// Schema: openai/codex 81b995a6c9a4c978f399775cfa5ce50702368749,
// codex-rs/protocol/src/{protocol,models}.rs and history/src/rollout_payload.rs.
// Native discriminator inventory. Unlisted names are unknown and reported.
pub(crate) const RECORD_TYPES: &[RecordTypeRule] = &[
    ("session_meta", Indexed("session_meta dispatch")),
    ("response_item", Indexed("response_item dispatch")),
    ("compacted", Indexed("compacted dispatch")),
    ("event_msg", Indexed("event_msg dispatch")),
    (
        "turn_context",
        Ignored("Turn configuration is retained for resume, not imported as conversation"),
    ),
    (
        "token_usage_record",
        Ignored("Usage telemetry is not conversation"),
    ),
    (
        "world_state",
        Ignored("Resume comparison baselines and patches are not imported as conversation"),
    ),
    (
        "inter_agent_communication_metadata",
        Ignored("Turn-trigger routing metadata is not conversation"),
    ),
    (
        "inter_agent_communication",
        Ignored("Inter-agent delivery payloads are not imported as conversation"),
    ),
    (
        "dynamic_tools_updated",
        Ignored("Dynamic tool declarations are not imported as conversation"),
    ),
    (
        "security_risk_score",
        Ignored("Security risk telemetry is not conversation"),
    ),
    (
        "retained_context",
        Ignored("Retained model-context manifests are not imported as conversation"),
    ),
    (
        "realtime_item",
        Ignored("Realtime presentation facts are not imported as conversation"),
    ),
    ("response_item.message", Indexed("response_item message")),
    (
        "response_item.function_call",
        Indexed("response_item function_call"),
    ),
    (
        "response_item.custom_tool_call",
        Indexed("response_item custom_tool_call"),
    ),
    (
        "response_item.function_call_output",
        Indexed("response_item function_call_output"),
    ),
    (
        "response_item.custom_tool_call_output",
        Indexed("response_item custom_tool_call_output"),
    ),
    (
        "response_item.additional_tools",
        Ignored("Tool declarations are not imported as conversation"),
    ),
    (
        "response_item.agent_message",
        Ignored("Inter-agent content is not imported as conversation"),
    ),
    (
        "response_item.reasoning",
        Ignored("Reasoning summaries and opaque/encrypted reasoning are not imported"),
    ),
    (
        "response_item.local_shell_call",
        Ignored("Legacy shell-call payloads are not imported by the function-call parser"),
    ),
    (
        "response_item.tool_search_call",
        Ignored("Tool-catalog search requests are not imported"),
    ),
    (
        "response_item.tool_search_output",
        Ignored("Tool-catalog search results are not imported"),
    ),
    (
        "response_item.web_search_call",
        Ignored(
            "Web-search response items are not imported; web_search_end owns indexed search output",
        ),
    ),
    (
        "response_item.image_generation_call",
        Ignored("Generated image payloads and revised prompts are not imported"),
    ),
    (
        "response_item.compaction",
        Ignored("Encrypted compaction payloads are not decoded"),
    ),
    (
        "response_item.compaction_summary",
        Ignored("Encrypted compaction payloads are not decoded"),
    ),
    (
        "response_item.configuration_update",
        Ignored("Backend configuration controls are not imported as conversation"),
    ),
    (
        "response_item.compaction_trigger",
        Ignored("Compaction request controls are not imported as conversation"),
    ),
    (
        "response_item.context_compaction",
        Ignored("Backend compaction controls are not imported as conversation"),
    ),
    (
        "response_item.other",
        Ignored("Protocol fallback response items are not imported"),
    ),
    (
        "response_item.message.user",
        Indexed("response_item message user"),
    ),
    (
        "response_item.message.assistant",
        Indexed("response_item message assistant"),
    ),
    (
        "response_item.message.developer",
        Ignored("Developer instruction context is not imported as conversation"),
    ),
    (
        "response_item.message.system",
        Ignored("System instruction context is not imported as conversation"),
    ),
    (
        "event_msg.error",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.warning",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.auth_recovery_started",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.auth_recovery_completed",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.guardian_warning",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.realtime_conversation_started",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.realtime_conversation_realtime",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.realtime_conversation_closed",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.realtime_conversation_sdp",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.model_reroute",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.model_verification",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.turn_moderation_metadata",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.safety_buffering",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.context_compacted",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.thread_rolled_back",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.task_started",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.thread_settings_applied",
        Indexed("event_msg thread_settings_applied"),
    ),
    (
        "event_msg.task_complete",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.token_count",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.agent_message",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.user_message",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.agent_reasoning",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.agent_reasoning_raw_content",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.agent_reasoning_section_break",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.session_configured",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.environment_connected",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.environment_disconnected",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.thread_goal_updated",
        Indexed("event_msg thread_goal_updated"),
    ),
    (
        "event_msg.thread_queue_changed",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.mcp_startup_update",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.mcp_startup_complete",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.mcp_tool_call_begin",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.mcp_tool_call_end",
        Indexed("event_msg mcp_tool_call_end"),
    ),
    (
        "event_msg.web_search_begin",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.web_search_end",
        Indexed("event_msg web_search_end"),
    ),
    (
        "event_msg.image_generation_begin",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.image_generation_end",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.exec_command_begin",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.exec_command_output_delta",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.terminal_interaction",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.exec_command_end",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.view_image_tool_call",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.exec_approval_request",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.request_permissions",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.request_user_input",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.dynamic_tool_call_request",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.dynamic_tool_call_response",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.elicitation_request",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.elicitation_abandoned",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.apply_patch_approval_request",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.guardian_assessment",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.deprecation_notice",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.stream_error",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.patch_apply_begin",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.patch_apply_updated",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.patch_apply_end",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.turn_diff",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.realtime_conversation_list_voices_response",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.plan_update",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    ("event_msg.turn_aborted", Indexed("event_msg turn_aborted")),
    (
        "event_msg.shutdown_complete",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.entered_review_mode",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.exited_review_mode",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.raw_response_item",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.raw_response_completed",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.item_started",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.item_completed",
        Ignored(
            "Canonical item/display events are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.hook_started",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.hook_completed",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.agent_message_content_delta",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.plan_delta",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.reasoning_content_delta",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.reasoning_raw_content_delta",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.collab_agent_spawn_begin",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.collab_agent_spawn_end",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.collab_agent_interaction_begin",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.collab_agent_interaction_end",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.collab_waiting_begin",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.collab_waiting_end",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.collab_close_begin",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.collab_close_end",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.collab_resume_begin",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.collab_resume_end",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.sub_agent_activity",
        Ignored(
            "UI/progress event payloads are not imported independently of response-item history",
        ),
    ),
    (
        "event_msg.turn_started",
        Ignored("Turn lifecycle alias is not imported as conversation"),
    ),
    (
        "event_msg.turn_complete",
        Ignored("Turn lifecycle alias is not imported as conversation"),
    ),
];

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

use crate::models::{
    ContentPartAuthorship, ContentPartOrigin, FileEdit, MessageAuthorship, MessageContentPart,
    MessageCorrelationAuthority, MessageInputOrigin, MessageRecordRelation, ParsedSession,
    Provider, SessionRecord, SourceFile,
};
use crate::providers::{walk_roots, ProviderDiscovery, ProviderPathWarning};
use crate::util::{
    extract_text, find_repo_root, format_transcript_line, minimal_record, normalize_path,
    observe_session_end, parse_datetime, parse_unix_millis, parse_unix_seconds, preview_from_text,
    truncate_for_display, RawMessage, UserRoleEvidence,
};

#[derive(Debug, Clone, Default)]
struct CodexMetadata {
    title: Option<String>,
    cwd: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    rollout_path: Option<String>,
    first_user_message: Option<String>,
    raw: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone)]
struct CodexRequestUserInputState {
    question_ids: Vec<String>,
    /// A call id is native identity, not a deduplication hint. Duplicate calls or outputs with
    /// one id are therefore ambiguous and must remain ordinary tool content.
    valid: bool,
    answered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexSidecarWarning {
    pub(crate) path: PathBuf,
    pub(crate) operation: &'static str,
    pub(crate) message: String,
}

#[derive(Debug, Default)]
struct CodexRootMetadata {
    root: PathBuf,
    threads: HashMap<String, CodexMetadata>,
    index_titles: HashMap<String, String>,
}

pub struct CodexAdapter {
    roots: Vec<PathBuf>,
    metadata: Vec<CodexRootMetadata>,
    sidecar_warnings: Vec<CodexSidecarWarning>,
}

impl CodexAdapter {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        let mut metadata = Vec::with_capacity(roots.len());
        let mut sidecar_warnings = Vec::new();
        for root in &roots {
            let home = codex_home_from_session_root(root);
            let state_path = home.join("state_5.sqlite");
            let index_path = home.join("session_index.jsonl");
            let threads = load_sidecar(&state_path, load_threads, &mut sidecar_warnings);
            let index_titles = load_sidecar(&index_path, load_index_titles, &mut sidecar_warnings);
            metadata.push(CodexRootMetadata {
                root: root.clone(),
                threads,
                index_titles,
            });
        }
        Self {
            roots,
            metadata,
            sidecar_warnings,
        }
    }

    #[cfg(test)]
    pub(crate) fn sidecar_warnings(&self) -> &[CodexSidecarWarning] {
        &self.sidecar_warnings
    }

    pub fn discover(&self) -> Vec<SourceFile> {
        self.discover_with_warnings().sources
    }

    pub(crate) fn discover_with_warnings(&self) -> ProviderDiscovery {
        let mut files = Vec::new();
        let walked = walk_roots(&self.roots, None);
        // Codex writes the compressed copy and then deletes the plain file, so both can exist for
        // a moment; the plain file is then the newer, authoritative copy. Deciding from this one
        // walk keeps exactly one of the two. If Codex deletes the plain file after the walk, its
        // read fails, the stored session stays as it was, and the next refresh finds the `.zst`.
        let walked_plain_rollouts: std::collections::HashSet<&Path> = walked
            .entries
            .iter()
            .map(|entry| entry.path.as_path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
            .collect();
        let shadowed_by_plain: Vec<bool> = walked
            .entries
            .iter()
            .map(|entry| {
                crate::source::is_compressed_jsonl(&entry.path)
                    && rollout_sibling(&entry.path)
                        .is_some_and(|plain| walked_plain_rollouts.contains(plain.as_path()))
            })
            .collect();
        for (entry, shadowed_by_plain) in walked.entries.into_iter().zip(shadowed_by_plain) {
            let path = &entry.path;
            let compressed = crate::source::is_compressed_jsonl(path);
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") && !compressed {
                continue;
            }
            if shadowed_by_plain {
                continue;
            }
            let mtime_ns = entry
                .metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|value| value.as_nanos() as i64)
                .unwrap_or_default();
            files.push(SourceFile {
                sidecar: None,
                provider: Provider::Codex,
                path: entry.path,
                mtime_ns,
                size_bytes: entry.metadata.len() as i64,
            });
        }
        let mut warnings = walked.warnings;
        warnings.extend(
            self.sidecar_warnings
                .iter()
                .map(|warning| ProviderPathWarning {
                    path: warning.path.clone(),
                    operation: warning.operation,
                    message: warning.message.clone(),
                }),
        );
        ProviderDiscovery {
            sources: files,
            warnings,
        }
    }

    pub fn parse(&self, source: &SourceFile) -> ParsedSession {
        match self.parse_inner(&source.path) {
            Ok(parsed) => parsed,
            Err(err) => minimal_record(Provider::Codex, &source.path, format!("{err:#}")),
        }
    }

    fn parse_inner(&self, path: &Path) -> Result<ParsedSession> {
        let reader = crate::source::open_source_reader(path)?;
        self.parse_reader(std::io::BufReader::new(reader), path)
    }

    /// Parse codex session lines from any reader. `parse_inner` calls this over the file; the
    /// incremental tail parser ([`crate::tail`]) calls it over an in-memory byte slice of the
    /// appended region, so the per-line logic lives in ONE place (a differential test asserts a
    /// tail parse equals a full parse). Streams line-by-line (task #241); `line_count` is tallied
    /// in this single pass. See claude::parse_reader for the equivalence/edge-case notes.
    pub fn parse_reader<R: std::io::BufRead>(
        &self,
        reader: R,
        path: &Path,
    ) -> Result<ParsedSession> {
        self.parse_reader_with_context(reader, path, None)
    }

    pub(crate) fn parse_reader_with_context<R: std::io::BufRead>(
        &self,
        reader: R,
        path: &Path,
        context: Option<crate::tail::TailContext>,
    ) -> Result<ParsedSession> {
        let mut line_count: usize = 0;
        let mut malformed_line_count: usize = 0;
        let mut valid_record_count: usize = 0;
        let mut unrecognized = super::health::UnrecognizedRecordTypes::for_context(context);
        let mut provider_session_id = self
            .extract_id(path)
            .unwrap_or_else(|| "unknown".to_string());
        let stored_session = context
            .and_then(|context| context.validated_session)
            .filter(|session| session.provider_session_id == provider_session_id);
        let stored_spawn = stored_session
            .map(|session| {
                (
                    session.parent_session_id.clone(),
                    session.agent_label.clone(),
                )
            })
            .unwrap_or_default();
        let mut session_is_agent = stored_spawn.0.is_some() || stored_spawn.1.is_some();
        let seeded_counts = session_is_agent;
        if seeded_counts {
            let metadata: Value = stored_session
                .and_then(|session| session.raw_metadata_json.as_deref())
                .and_then(|raw| serde_json::from_str(raw).ok())
                .unwrap_or_default();
            line_count = metadata["line_count"].as_u64().unwrap_or(0) as usize;
            valid_record_count = metadata["valid_record_count"].as_u64().unwrap_or(0) as usize;
            malformed_line_count = metadata["malformed_line_count"].as_u64().unwrap_or(0) as usize;
        }
        session_is_agent |= self
            .metadata_for_path(path)
            .and_then(|metadata| metadata.threads.get(&provider_session_id))
            .map(|metadata| {
                let raw = Value::Object(metadata.raw.clone());
                let (parent, label) = codex_spawn_origin(&raw);
                parent.is_some() || label.is_some()
            })
            .unwrap_or(false);
        // A forked or resumed session emits a SECOND session_meta carrying the parent's id.
        // Only the first one identifies this file, so bind the id once. See the guard below.
        let mut session_id_bound = false;
        let mut header_spawn_origin = stored_spawn;
        let mut spawn_header = stored_session
            .and_then(|session| session.raw_metadata_json.as_deref())
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .and_then(|metadata| metadata.get(SPAWN_HEADER_KEY).cloned());
        let mut cwd = None;
        let mut created_at = None;
        let mut updated_at = None;
        let mut transcript_lines = Vec::new();
        let mut messages = Vec::new();
        // call_id -> tool name, so a later function_call_output can be tagged with the tool.
        let mut tool_call_names: HashMap<String, String> = HashMap::new();
        // The native request_user_input call is the only safe authority for interpreting an
        // output's answer values as input. Keep the question ids so arbitrary tool JSON and
        // lookalike tool names cannot become user input by shape alone.
        let mut request_user_input_calls: HashMap<String, CodexRequestUserInputState> =
            HashMap::new();
        let mut requires_full_parse_for_tail = false;
        let mut file_edits: Vec<FileEdit> = Vec::new();
        let mut pending_file_mutations = crate::files::PendingFileMutations::default();
        let mut first_user = None;
        let mut last_user = None;
        let mut latest_goal: Option<Value> = None;
        let mut instruction_snapshots = std::collections::HashSet::new();
        // A fork replays its parent's history until its first `thread_settings_applied` event;
        // the parent's `session_meta` is optional. See `ForkReplay`.
        let mut fork_replay = ForkReplay::default();
        let mut fork_parent = None;
        let mut replayed_file_edits = None;

        for line in crate::util::lines_with_source_bytes(reader) {
            let (byte_offset, source_bytes, line) = line?;
            unrecognized.record(context, byte_offset);
            let is_new = context.is_none_or(|c| byte_offset >= c.new_bytes_start);
            let count_line = !seeded_counts || is_new;
            line_count += usize::from(count_line);
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(_) => {
                    malformed_line_count += usize::from(count_line);
                    continue;
                }
            };
            valid_record_count += usize::from(count_line);
            unrecognized.observe(Provider::Codex, &value);
            let timestamp = value
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_datetime);
            match value.get("type").and_then(Value::as_str) {
                Some("session_meta") => {
                    if let Some(payload) = value.get("payload") {
                        if let Some(text) = payload
                            .pointer("/base_instructions/text")
                            .and_then(Value::as_str)
                        {
                            // Like Claude prompt snapshots, context is byte-exact, untimed and
                            // content-addressed so resumed headers cannot multiply indexed text.
                            if !text.is_empty() && instruction_snapshots.insert(text.to_owned()) {
                                let identity = format!(
                                    "snapshot:base_instructions:{}",
                                    crate::hashing::sha256(text.as_bytes())
                                );
                                messages.push(
                                    RawMessage::harness_notice(text.to_owned(), None)
                                        .with_native_event_identity(
                                            MessageCorrelationAuthority::OpenAi,
                                            identity,
                                        ),
                                );
                            }
                        }
                        // First session_meta wins, matching cwd/created_at below. A fork's
                        // second session_meta carries the PARENT's id; taking it would bind
                        // this file's content to the parent's session, and the
                        // `on conflict(id) do update` upsert in db.rs would then overwrite
                        // the parent's row wholesale instead of storing this one.
                        if session_id_bound && fork_replay.forked {
                            fork_replay.start(
                                messages.len(),
                                transcript_lines.len(),
                                &first_user,
                                &last_user,
                            );
                        }
                        if !session_id_bound {
                            if context.is_none() {
                                spawn_header = Some(json!({
                                    "offset": byte_offset,
                                    "length": source_bytes.len(),
                                    "sha256": crate::hashing::sha256(&source_bytes),
                                }));
                            }
                            fork_parent = payload
                                .get("forked_from_id")
                                .and_then(Value::as_str)
                                .filter(|id| !id.trim().is_empty())
                                .map(str::to_owned);
                            fork_replay.forked = fork_parent.is_some();
                            if fork_replay.forked {
                                fork_replay.start(0, 0, &None, &None);
                            }
                            let (parent, label) = codex_spawn_origin(payload);
                            session_is_agent |= parent.is_some() || label.is_some();
                            header_spawn_origin = (parent, label);
                            if let Some(id) = payload.get("id").and_then(Value::as_str) {
                                provider_session_id = id.to_string();
                            }
                            session_id_bound = true;
                        }
                        if cwd.is_none() {
                            cwd = payload
                                .get("cwd")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned);
                        }
                        if created_at.is_none() {
                            created_at = payload
                                .get("timestamp")
                                .and_then(Value::as_str)
                                .and_then(parse_datetime);
                        }
                    }
                }
                Some("response_item") => {
                    let Some(payload) = value.get("payload") else {
                        continue;
                    };
                    let event_id = payload
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.trim().is_empty());
                    let item_type = payload.get("type").and_then(Value::as_str);
                    let role = payload.get("role").and_then(Value::as_str);
                    if item_type == Some("message") && matches!(role, Some("user" | "assistant")) {
                        let shell_direct_authorship = if session_is_agent {
                            ContentPartAuthorship::Agent
                        } else {
                            ContentPartAuthorship::Human
                        };
                        let shell_attribution = (role == Some("user"))
                            .then(|| {
                                codex_native_shell_command_attribution(
                                    payload,
                                    shell_direct_authorship,
                                )
                            })
                            .flatten();
                        let native_attribution = (role == Some("user")
                            && shell_attribution.is_none())
                        .then(|| codex_native_message_attribution(payload))
                        .flatten()
                        .map(|attribution| {
                            codex_agent_session_attribution(attribution, session_is_agent)
                        });
                        let text = extract_text(payload);
                        if text.trim().is_empty() {
                            continue;
                        }
                        // Codex injects approval-mode context (the prior agent transcript /
                        // AGENTS.md) as a role:user message. It is the harness talking, not the
                        // user, so it carries the same class as claude's hook feedback and
                        // caveats. It was previously re-tagged `tool`, which kept it out of
                        // user analytics but described it as tool output and gave one concept
                        // two representations across providers. See claude.rs is_harness_notice.
                        if role == Some("user")
                            && native_attribution.is_none()
                            && shell_attribution.is_none()
                            && is_codex_injected_context(&text)
                        {
                            messages.push(RawMessage::harness_notice(text, timestamp));
                            continue;
                        }
                        if role == Some("user") {
                            if first_user.is_none() {
                                first_user = Some(text.clone());
                            }
                            last_user = Some(text.clone());
                        }
                        observe_session_end(&mut updated_at, timestamp);
                        let mut raw_message = RawMessage::message(
                            role.unwrap_or("message"),
                            text.clone(),
                            timestamp,
                            None,
                        );
                        if let Some(shell_attribution) = shell_attribution {
                            raw_message = match shell_attribution {
                                CodexShellCommandAttribution::Parts(parts) => {
                                    let has_direct_input = parts.iter().any(|part| {
                                        part.authorship == ContentPartAuthorship::Human
                                            && part.origin == ContentPartOrigin::DirectInput
                                    });
                                    let raw_message = raw_message.with_content_parts(parts);
                                    if has_direct_input {
                                        raw_message.with_input_origin(
                                            MessageInputOrigin::DirectConversation,
                                        )
                                    } else {
                                        raw_message
                                    }
                                }
                                CodexShellCommandAttribution::Unknown => {
                                    raw_message.with_authorship(MessageAuthorship::Unknown)
                                }
                            };
                        } else if let Some(attribution) = native_attribution {
                            raw_message = match attribution {
                                CodexMessageAttribution::Whole {
                                    authorship,
                                    input_origin,
                                } => {
                                    let mut raw_message = raw_message.with_authorship(authorship);
                                    if let Some(input_origin) = input_origin {
                                        raw_message = raw_message.with_input_origin(input_origin);
                                    }
                                    raw_message
                                }
                                CodexMessageAttribution::Parts(parts) => {
                                    raw_message.with_content_parts(parts)
                                }
                            };
                        } else if is_codex_subagent_notification(&text) {
                            raw_message = raw_message.with_authorship(MessageAuthorship::Agent);
                        }
                        if let Some(event_id) = event_id {
                            raw_message = raw_message.with_native_event_identity(
                                MessageCorrelationAuthority::OpenAi,
                                event_id,
                            );
                        }
                        messages.push(raw_message);
                        transcript_lines.push(format_transcript_line(
                            role.unwrap_or("message"),
                            timestamp,
                            &text,
                        ));
                    } else if matches!(item_type, Some("function_call") | Some("custom_tool_call"))
                    {
                        let name = payload.get("name").and_then(Value::as_str);
                        // Record call_id -> tool name so the matching *_output can be tagged.
                        if let (Some(call_id), Some(name)) =
                            (payload.get("call_id").and_then(Value::as_str), name)
                        {
                            tool_call_names.insert(call_id.to_string(), name.to_string());
                            if name == "request_user_input" {
                                // Calls add rows; only responses need the prefix on a tail parse.
                                requires_full_parse_for_tail |= context.is_none();
                                let question_ids = codex_request_user_input_question_ids(
                                    &codex_tool_call_args(payload),
                                );
                                let state = CodexRequestUserInputState {
                                    valid: question_ids.is_some(),
                                    question_ids: question_ids.unwrap_or_default(),
                                    answered: false,
                                };
                                if request_user_input_calls
                                    .insert(call_id.to_string(), state)
                                    .is_some()
                                {
                                    // Reusing a native call id makes later answer correlation
                                    // ambiguous even if one of the two calls looked valid.
                                    if let Some(state) = request_user_input_calls.get_mut(call_id) {
                                        state.valid = false;
                                        state.answered = true;
                                        state.question_ids.clear();
                                    }
                                }
                            }
                        }
                        if let Some(name) = name {
                            let args = codex_tool_call_args(payload);
                            let mut raw_message = RawMessage::tool_call(
                                name,
                                args,
                                payload.get("call_id").and_then(Value::as_str),
                                timestamp,
                            );
                            if let Some(event_id) = event_id {
                                raw_message = raw_message.with_native_event_identity(
                                    MessageCorrelationAuthority::OpenAi,
                                    event_id,
                                );
                            }
                            messages.push(raw_message);
                        }
                        // Stage apply_patch changes until its correlated output proves success.
                        if let Some(patch) = apply_patch_text(payload) {
                            pending_file_mutations.stage_many(
                                payload.get("call_id").and_then(Value::as_str),
                                timestamp,
                                "apply_patch",
                                parse_apply_patch(&patch)
                                    .into_iter()
                                    .map(|(path, content)| (path, content, Vec::new()))
                                    .collect(),
                            );
                        }
                    } else if matches!(
                        item_type,
                        Some("function_call_output") | Some("custom_tool_call_output")
                    ) {
                        // Tool output → a Role::Tool message tagged with the tool that
                        // produced it (correlated by call_id), kept out of the human
                        // transcript/title/preview.
                        // A tail slice may contain this candidate response without the native
                        // call/question prefix. Mark it for a full parse; the marker is a
                        // correctness guard only and does not classify the output as input.
                        if codex_request_user_input_response(
                            payload.get("output").unwrap_or(&Value::Null),
                        )
                        .is_some()
                        {
                            requires_full_parse_for_tail |=
                                is_new && context.is_none_or(|c| c.prefix_truncated);
                        }
                        let output = codex_output_text(payload.get("output"));
                        let call_id = payload.get("call_id").and_then(Value::as_str);
                        let tool_name = call_id.and_then(|id| tool_call_names.get(id).cloned());
                        let answer_text = if tool_name.as_deref() == Some("request_user_input") {
                            call_id.and_then(|id| {
                                let state = request_user_input_calls.get_mut(id)?;
                                if !state.valid || state.answered {
                                    return None;
                                }
                                // Consume the first native output regardless of whether it is a
                                // response, cancellation, or error. A duplicate result must not
                                // duplicate user-input regions.
                                state.answered = true;
                                codex_request_user_input_answers(
                                    payload.get("output"),
                                    &state.question_ids,
                                )
                            })
                        } else {
                            None
                        };
                        pending_file_mutations.finish(
                            call_id,
                            codex_apply_patch_succeeded(payload.get("output")),
                            &mut file_edits,
                        );
                        if !output.trim().is_empty() || answer_text.is_some() {
                            observe_session_end(&mut updated_at, timestamp);
                            let output = codex_append_request_user_input_answers(
                                output.into_owned(),
                                answer_text.as_deref(),
                            );
                            let mut raw_message = RawMessage::tool_result_with_name(
                                tool_name,
                                output.content,
                                call_id,
                                timestamp,
                            );
                            if let Some(attribution) = output.attribution {
                                let attribution = match attribution {
                                    CodexAnswerAttribution::Whole => {
                                        CodexMessageAttribution::Whole {
                                            authorship: MessageAuthorship::Human,
                                            input_origin: None,
                                        }
                                    }
                                    CodexAnswerAttribution::Mixed(parts) => {
                                        CodexMessageAttribution::Parts(parts)
                                    }
                                };
                                raw_message = match codex_agent_session_attribution(
                                    attribution,
                                    session_is_agent,
                                ) {
                                    CodexMessageAttribution::Whole { authorship, .. } => {
                                        raw_message.with_authorship(authorship).with_input_origin(
                                            MessageInputOrigin::InteractiveAnswer,
                                        )
                                    }
                                    CodexMessageAttribution::Parts(parts) => raw_message
                                        .with_content_parts(parts)
                                        .with_input_origin(MessageInputOrigin::InteractiveAnswer),
                                };
                            }
                            if let Some(event_id) = event_id {
                                raw_message = raw_message.with_native_event_identity(
                                    MessageCorrelationAuthority::OpenAi,
                                    event_id,
                                );
                            }
                            messages.push(raw_message);
                        }
                    }
                }
                Some("compacted") => {
                    // replacement_history is model context, not new conversation. Retain the
                    // summary without replacing or reclassifying rows already published.
                    if let Some(text) = value.pointer("/payload/message").and_then(Value::as_str) {
                        if !text.trim().is_empty() {
                            messages.push(RawMessage::message(
                                "compaction",
                                text.to_owned(),
                                timestamp,
                                None,
                            ));
                            observe_session_end(&mut updated_at, timestamp);
                        }
                    }
                }
                Some("event_msg") => {
                    let payload = value.get("payload").unwrap_or(&value);
                    let event_type = payload
                        .get("type")
                        .or_else(|| payload.get("event_type"))
                        .and_then(Value::as_str);
                    if event_type == Some("turn_aborted") {
                        messages.push(RawMessage::harness_notice(payload.to_string(), timestamp));
                        observe_session_end(&mut updated_at, timestamp);
                    }
                    if event_type == Some("thread_settings_applied") {
                        // Closing a replay changes earlier rows even when its header is outside
                        // the overlap; append-only storage cannot reconcile those rows.
                        requires_full_parse_for_tail |=
                            is_new && context.is_none_or(|c| c.prefix_truncated);
                        if fork_replay.open.is_some() {
                            replayed_file_edits = Some(0..file_edits.len());
                        }
                        fork_replay.end(
                            messages.len(),
                            transcript_lines.len(),
                            &mut first_user,
                            &mut last_user,
                        );
                    }
                    if event_type == Some("thread_goal_updated") {
                        latest_goal = payload
                            .get("goal")
                            .cloned()
                            .or_else(|| Some(payload.clone()));
                    }
                    if let Some((tool_name, content)) =
                        codex_event_tool_message(event_type, payload)
                    {
                        messages.push(RawMessage::message(
                            "tool",
                            content.to_string(),
                            timestamp,
                            Some(tool_name),
                        ));
                    }
                }
                _ => {}
            }
        }

        let mut retracted = crate::models::RetractedRows::default();
        if let Some(range) = replayed_file_edits {
            // Only indexed parent edits can prove a replay is recoverable, regardless of
            // whether discovery has seen a parent filename yet. Storage retains unproven copies.
            retracted.file_edits = file_edits.drain(range).collect();
        }
        let messages = fork_replay.mark(messages, &mut transcript_lines);
        let root_metadata = self.metadata_for_path(path);
        let meta = root_metadata
            .and_then(|metadata| metadata.threads.get(&provider_session_id))
            .cloned()
            .unwrap_or_default();
        let title = meta
            .title
            .or_else(|| {
                root_metadata
                    .and_then(|metadata| metadata.index_titles.get(&provider_session_id))
                    .cloned()
            })
            // The first prompt can be outside overlap just like the spawn header.
            .or_else(|| {
                stored_session
                    .filter(|_| session_is_agent)
                    .and_then(|s| s.title.clone())
            })
            .or_else(|| first_user.clone())
            .map(|text| truncate_for_display(&text, 100));
        let summary = meta
            .first_user_message
            .or_else(|| first_user.clone())
            .map(|text| truncate_for_display(&text, 180));
        let cwd = cwd.or(meta.cwd);
        let repo_root = cwd.as_deref().and_then(find_repo_root);
        let created_at = created_at.or(meta.created_at);
        let updated_at = updated_at.or(meta.updated_at);
        let preview = last_user
            .clone()
            .or_else(|| first_user.clone())
            .or_else(|| summary.clone())
            .map(|text| preview_from_text(&text))
            .unwrap_or_else(|| "(no preview available)".to_string());
        let mut raw_metadata = json!({
            "line_count": line_count,
            "valid_record_count": valid_record_count,
            "rollout_path": meta.rollout_path,
            "session_path": normalize_path(path),
        });
        if let Some(parent) = fork_parent {
            raw_metadata["forked_from_id"] = json!(parent);
        }
        if malformed_line_count > 0 {
            raw_metadata["malformed_line_count"] = json!(malformed_line_count);
        }
        if requires_full_parse_for_tail || (context.is_none() && !retracted.file_edits.is_empty()) {
            // Replay edits need storage reconciliation against the indexed parent.
            // An answer may be in the appended tail while its native call and question ids are
            // before the overlap. The indexer owns the provider guard; this marker is the parser's
            // evidence that a tail-only parse cannot safely correlate that output.
            raw_metadata["requires_full_parse_for_tail"] = json!(true);
        }
        if let Value::Object(obj) = &mut raw_metadata {
            if let Some(goal) = latest_goal {
                obj.insert("latest_goal".to_string(), goal);
            }
            for (key, value) in meta.raw {
                if !value.is_null() {
                    obj.insert(key, value);
                }
            }
        }
        // Codex records a spawn as `source: {"subagent":{"thread_spawn":{...}}}`, carrying the
        // parent thread and an agent nickname. Lifting it into the typed fields is what makes
        // it queryable; the same value may arrive encoded as a string from state_5.sqlite.
        let (parent_session_id, agent_label) = codex_spawn_origin(&raw_metadata);
        // The rollout header carries spawn provenance even when state_5.sqlite is absent. Keep
        // it as typed session metadata so bounded tail parses can compare against the persisted
        // full-parse result; sidecar values remain authoritative when present.
        let parent_session_id = parent_session_id.or(header_spawn_origin.0);
        let agent_label = agent_label.or(header_spawn_origin.1);
        if let Some(header) = spawn_header {
            raw_metadata[SPAWN_HEADER_KEY] = header;
        }
        unrecognized.publish(&mut raw_metadata);
        let raw_metadata_json = Some(serde_json::to_string(&raw_metadata)?);
        let agent_started = parent_session_id.is_some() || agent_label.is_some();

        let session = SessionRecord {
            id: format!("codex:{provider_session_id}"),
            provider: Provider::Codex,
            provider_session_id,
            title,
            summary,
            cwd,
            repo_root,
            created_at,
            updated_at,
            last_message_at: updated_at,
            preview_text: preview,
            source_path: normalize_path(path),
            message_count: Some(messages.len() as i64),
            parse_version: crate::util::provider_parse_version(Provider::Codex).to_string(),
            raw_metadata_json,
            parse_warning: super::malformed_jsonl_warning(malformed_line_count),
            // A fixed label for what THIS ADAPTER reads (rollout .jsonl plus codex's
            // state_5.sqlite for titles), not computed per-file provenance. Every codex row
            // carries it, including rows built when state_5.sqlite is absent. Reading it as
            // evidence that a given file was matched against that database is a mistake that
            // has already cost one investigation: "all indexed rows are jsonl+sqlite and none
            // is plain jsonl" looks like proof that indexing is gated on the sqlite side, and
            // it is not. `threads` holds one row per thread while rollout files are per-session;
            // these counts describe different units.
            discovery_source: "jsonl+sqlite".to_string(),
            parent_session_id,
            agent_label,
        };

        let messages = crate::util::normalize_raw_messages_with_evidence(
            messages,
            Some(&session.id),
            if agent_started {
                UserRoleEvidence::AgentDelegationEvent
            } else {
                UserRoleEvidence::HumanInputEvent
            },
        );
        Ok(ParsedSession {
            session,
            transcript_text: transcript_lines.join("\n\n"),
            messages,
            file_edits,
            retracted,
        })
    }

    fn metadata_for_path(&self, path: &Path) -> Option<&CodexRootMetadata> {
        self.metadata
            .iter()
            .filter(|metadata| path.starts_with(&metadata.root))
            .max_by_key(|metadata| metadata.root.components().count())
    }

    fn extract_id(&self, path: &Path) -> Option<String> {
        rollout_thread_id(path).map(str::to_owned)
    }
}

pub(crate) const SPAWN_HEADER_KEY: &str = "codex_spawn_header";

#[derive(serde::Deserialize)]
struct SpawnHeaderCheckpoint {
    offset: u64,
    length: u64,
    sha256: String,
}

#[cfg(test)]
thread_local! {
    pub(crate) static TEST_SPAWN_HEADER_VALIDATION_BYTES: std::cell::RefCell<Vec<u64>> = const { std::cell::RefCell::new(Vec::new()) };
}

// Validate the record that supplied stored provenance, not an arbitrary file prefix.
// Hash one bounded range in O(header bytes) time and O(1) streaming memory.
// Missing checkpoints from older indexes safely request one full parse.
pub(crate) fn spawn_header_is_current(
    path: &Path,
    session: &SessionRecord,
    checkpoint_offset: i64,
) -> bool {
    use std::io::{Read as _, Seek as _};
    let checkpoint = session
        .raw_metadata_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|metadata| metadata.get(SPAWN_HEADER_KEY).cloned())
        .and_then(|value| serde_json::from_value::<SpawnHeaderCheckpoint>(value).ok());
    let Some(checkpoint) = checkpoint else {
        return false;
    };
    if checkpoint.length == 0
        || checkpoint
            .offset
            .checked_add(checkpoint.length)
            .is_none_or(|end| end > checkpoint_offset as u64)
    {
        return false;
    }
    let validate = || -> std::io::Result<bool> {
        let mut file = fs::File::open(path)?;
        file.seek(std::io::SeekFrom::Start(checkpoint.offset))?;
        let mut range = file.take(checkpoint.length);
        let digest = crate::hashing::sha256_reader(&mut range)?;
        #[cfg(test)]
        TEST_SPAWN_HEADER_VALIDATION_BYTES
            .with(|counts| counts.borrow_mut().push(checkpoint.length - range.limit()));
        Ok(range.limit() == 0 && digest == checkpoint.sha256)
    };
    validate().unwrap_or(false)
}

#[derive(Debug, Clone)]
enum CodexMessageAttribution {
    Whole {
        authorship: MessageAuthorship,
        input_origin: Option<MessageInputOrigin>,
    },
    Parts(Vec<MessageContentPart>),
}

#[derive(Debug, Clone)]
enum CodexShellCommandAttribution {
    Parts(Vec<MessageContentPart>),
    /// The producer claimed a shell command, but its positional metadata or exact frame was
    /// unusable. Do not let a role or legacy provenance guess make generated stdout input.
    Unknown,
}

#[derive(Debug, Clone)]
enum CodexAnswerAttribution {
    Whole,
    Mixed(Vec<MessageContentPart>),
}

#[derive(Debug, Clone)]
struct CodexOutputWithAnswers {
    content: String,
    attribution: Option<CodexAnswerAttribution>,
}

/// Decode aligned native shell items alongside ordinary positional provenance. Framing and
/// inter-item separators remain non-input. Work is O(B + P), for B content bytes and P parts;
/// output ranges use O(P) memory, with O(L) temporary text for the largest content item L.
fn codex_native_shell_command_attribution(
    payload: &Value,
    direct_authorship: ContentPartAuthorship,
) -> Option<CodexShellCommandAttribution> {
    const SHELL_KIND: &str = "shell.user_command";
    let kinds_value =
        payload.pointer("/internal_chat_message_metadata_passthrough/content_item_kinds")?;
    let Some(kinds) = kinds_value.as_array() else {
        return Some(CodexShellCommandAttribution::Unknown);
    };
    let Some(content) = payload.get("content").and_then(Value::as_array) else {
        return Some(CodexShellCommandAttribution::Unknown);
    };
    if kinds.len() != content.len() {
        return Some(CodexShellCommandAttribution::Unknown);
    }
    if !kinds.iter().any(|kind| kind.as_str() == Some(SHELL_KIND)) {
        return None;
    }
    let metadata = payload
        .pointer("/internal_chat_message_metadata_passthrough/content_item_metadata")
        .and_then(Value::as_array)
        .filter(|metadata| metadata.len() == content.len());
    let mut parts = Vec::new();
    let mut offset = 0;
    for (index, (item, kind)) in content.iter().zip(kinds).enumerate() {
        let text = extract_text(item);
        if text.is_empty() {
            continue;
        }
        if offset > 0 {
            parts.push(MessageContentPart {
                ordinal: parts.len() as u32,
                start_char: offset,
                end_char: offset + 1,
                authorship: ContentPartAuthorship::Generated,
                origin: ContentPartOrigin::HarnessContext,
            });
            offset += 1;
        }
        let item_parts = if kind.as_str() == Some(SHELL_KIND) {
            codex_user_shell_command_parts(&text, direct_authorship)
        } else {
            None
        };
        let item_parts = item_parts.unwrap_or_else(|| {
            let (mut authorship, origin) = if kind.as_str() == Some(SHELL_KIND) {
                (ContentPartAuthorship::Unknown, ContentPartOrigin::Unknown)
            } else {
                metadata
                    .and_then(|metadata| codex_content_item_provenance(&metadata[index]))
                    .unwrap_or((ContentPartAuthorship::Unknown, ContentPartOrigin::Unknown))
            };
            if authorship == ContentPartAuthorship::Human {
                authorship = direct_authorship;
            }
            vec![MessageContentPart {
                ordinal: 0,
                start_char: 0,
                end_char: text.chars().count(),
                authorship,
                origin,
            }]
        });
        let item_end = offset + text.chars().count();
        for mut part in item_parts {
            part.ordinal = parts.len() as u32;
            part.start_char += offset;
            part.end_char += offset;
            parts.push(part);
        }
        offset = item_end;
    }
    if parts
        .iter()
        .all(|part| part.authorship == ContentPartAuthorship::Unknown)
    {
        Some(CodexShellCommandAttribution::Unknown)
    } else {
        Some(CodexShellCommandAttribution::Parts(parts))
    }
}

/// Split one canonical Codex shell frame without rewriting its persisted content. The producer
/// interpolates command and stdout without escaping, so a delimiter in either field can create
/// more than one structurally valid frame. In that case return `None` rather than assigning an
/// invented author. All offsets are Unicode scalar positions, matching [`MessageContentPart`].
fn codex_user_shell_command_parts(
    content: &str,
    direct_authorship: ContentPartAuthorship,
) -> Option<Vec<MessageContentPart>> {
    const OUTER_OPEN: &str = "<user_shell_command>";
    const OUTER_CLOSE: &str = "</user_shell_command>";
    const FRAME_PREFIX: &str = "\n<command>\n";
    const FRAME_BOUNDARY: &str = "\n</command>\n<result>\n";
    const OUTPUT_PREFIX: &str = "Output:\n";
    const FRAME_SUFFIX: &str = "\n</result>\n";

    let outer_body = content.strip_prefix(OUTER_OPEN)?;
    let outer_close = outer_body.rfind(OUTER_CLOSE)?;
    if !outer_body[outer_close + OUTER_CLOSE.len()..].is_empty() {
        return None;
    }
    let frame = &outer_body[..outer_close];
    if !frame.starts_with(FRAME_PREFIX) {
        return None;
    }
    let command_body_start = FRAME_PREFIX.len();
    if !frame.ends_with(FRAME_SUFFIX) {
        return None;
    }
    let output_end = frame.len().checked_sub(FRAME_SUFFIX.len())?;
    let duration_prefix = "Duration: ";
    let mut candidate = None;
    for (boundary, _) in frame.match_indices(FRAME_BOUNDARY) {
        if boundary < command_body_start {
            continue;
        }
        let result_body = &frame[boundary + FRAME_BOUNDARY.len()..];
        let Some(exit_line_end) = result_body.find('\n') else {
            continue;
        };
        if !result_body.starts_with("Exit code: ") {
            continue;
        }
        let duration_body = &result_body[exit_line_end + 1..];
        let Some(duration_body) = duration_body.strip_prefix(duration_prefix) else {
            continue;
        };
        let Some(duration_line_end) = duration_body.find('\n') else {
            continue;
        };
        if !duration_body[..duration_line_end].ends_with(" seconds") {
            continue;
        }
        let output_start_in_result =
            exit_line_end + 1 + duration_prefix.len() + duration_line_end + 1;
        if !result_body[output_start_in_result..].starts_with(OUTPUT_PREFIX) {
            continue;
        }
        let output_start =
            boundary + FRAME_BOUNDARY.len() + output_start_in_result + OUTPUT_PREFIX.len();
        if output_start > output_end {
            continue;
        }
        if candidate.replace((boundary, output_start)).is_some() {
            // The native format has no escaping, so two valid candidates are irreducibly
            // ambiguous. Preserve the complete row for Any searches, but do not call either
            // candidate direct user input.
            return None;
        }
    }
    let (boundary, output_start) = candidate?;

    let command_start = OUTER_OPEN.len() + command_body_start;
    let command_end = OUTER_OPEN.len() + boundary;
    let output_start = OUTER_OPEN.len() + output_start;
    let output_end = OUTER_OPEN.len() + output_end;
    let content_chars = content.chars().count();
    let byte_to_chars = |byte| content[..byte].chars().count();
    let ranges = [
        (
            0,
            command_start,
            ContentPartAuthorship::Harness,
            ContentPartOrigin::HarnessContext,
        ),
        (
            command_start,
            command_end,
            direct_authorship,
            ContentPartOrigin::DirectInput,
        ),
        (
            command_end,
            output_start,
            ContentPartAuthorship::Generated,
            ContentPartOrigin::ToolPayload,
        ),
        (
            output_start,
            output_end,
            ContentPartAuthorship::Generated,
            ContentPartOrigin::ToolPayload,
        ),
        (
            output_end,
            content.len(),
            ContentPartAuthorship::Harness,
            ContentPartOrigin::HarnessContext,
        ),
    ];
    let mut parts = Vec::with_capacity(ranges.len());
    for (start, end, authorship, origin) in ranges {
        let start_char = byte_to_chars(start);
        let end_char = byte_to_chars(end);
        if start_char == end_char {
            continue;
        }
        parts.push(MessageContentPart {
            ordinal: parts.len() as u32,
            start_char,
            end_char,
            authorship,
            origin,
        });
    }
    parts
        .last()
        .is_some_and(|part| part.end_char == content_chars)
        .then_some(parts)
}

/// Generic native `provenance.type=user` identifies a user-role content item, not the physical
/// root user. A session with explicit spawn provenance is an agent session, so only exact native
/// actor evidence could override this mapping; the generic user marker cannot.
fn codex_agent_session_attribution(
    attribution: CodexMessageAttribution,
    session_is_agent: bool,
) -> CodexMessageAttribution {
    if !session_is_agent {
        return attribution;
    }
    match attribution {
        CodexMessageAttribution::Whole {
            authorship: MessageAuthorship::Human,
            ..
        } => CodexMessageAttribution::Whole {
            authorship: MessageAuthorship::Agent,
            input_origin: None,
        },
        CodexMessageAttribution::Whole {
            authorship,
            input_origin,
        } => CodexMessageAttribution::Whole {
            authorship,
            input_origin,
        },
        CodexMessageAttribution::Parts(parts) => CodexMessageAttribution::Parts(
            parts
                .into_iter()
                .map(|mut part| {
                    if part.authorship == ContentPartAuthorship::Human {
                        part.authorship = ContentPartAuthorship::Agent;
                    }
                    part
                })
                .collect(),
        ),
    }
}

/// Decode the producer's positional content metadata without interpreting prose. A missing or
/// misaligned vector is explicit Unknown evidence: it must not fall through to a role-based
/// Human guess, while older records with no metadata retain the existing normalization path.
fn codex_native_message_attribution(payload: &Value) -> Option<CodexMessageAttribution> {
    let metadata = payload
        .pointer("/internal_chat_message_metadata_passthrough/content_item_metadata")
        .and_then(Value::as_array)?;
    let Some(content) = payload.get("content").and_then(Value::as_array) else {
        return Some(CodexMessageAttribution::Whole {
            authorship: MessageAuthorship::Unknown,
            input_origin: None,
        });
    };
    if metadata.len() != content.len() {
        return Some(CodexMessageAttribution::Whole {
            authorship: MessageAuthorship::Unknown,
            input_origin: None,
        });
    }

    let full_text = extract_text(payload);
    let mut offset = 0usize;
    let mut observed = Vec::new();
    for (item, metadata) in content.iter().zip(metadata) {
        let text = extract_text(item);
        if text.is_empty() {
            continue;
        }
        if let Some((_, previous_end, _, _)) = observed.last_mut() {
            // `extract_text` joins content items with a newline. Keep that separator in the
            // preceding source region so the persisted scalar ranges remain gap-free.
            *previous_end += 1;
            offset += 1;
        }
        let Some((authorship, origin)) = codex_content_item_provenance(metadata) else {
            return Some(CodexMessageAttribution::Whole {
                authorship: MessageAuthorship::Unknown,
                input_origin: None,
            });
        };
        let end = offset + text.chars().count();
        observed.push((offset, end, authorship, origin));
        offset = end;
    }
    if observed.is_empty() || offset != full_text.chars().count() {
        return Some(CodexMessageAttribution::Whole {
            authorship: MessageAuthorship::Unknown,
            input_origin: None,
        });
    }

    let first_authorship = observed[0].2;
    if observed
        .iter()
        .all(|(_, _, authorship, _)| *authorship == first_authorship)
    {
        return Some(CodexMessageAttribution::Whole {
            authorship: first_authorship.into(),
            input_origin: (first_authorship == ContentPartAuthorship::Human
                && observed
                    .iter()
                    .all(|(_, _, _, origin)| *origin == ContentPartOrigin::DirectInput))
            .then_some(MessageInputOrigin::DirectConversation),
        });
    }

    Some(CodexMessageAttribution::Parts(
        observed
            .into_iter()
            .enumerate()
            .map(
                |(ordinal, (start_char, end_char, authorship, origin))| MessageContentPart {
                    ordinal: ordinal as u32,
                    start_char,
                    end_char,
                    authorship,
                    origin,
                },
            )
            .collect(),
    ))
}

fn codex_content_item_provenance(
    metadata: &Value,
) -> Option<(ContentPartAuthorship, ContentPartOrigin)> {
    let provenance = metadata.get("provenance").unwrap_or(metadata);
    let kind = provenance.get("type").and_then(Value::as_str);
    let legacy_tool = provenance
        .get("source_tool_namespace")
        .and_then(Value::as_str)
        .is_some();
    let legacy_harness = provenance
        .get("harness_injected")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match kind {
        Some("user") => Some((ContentPartAuthorship::Human, ContentPartOrigin::DirectInput)),
        Some("harness") | Some("skills") | Some("agents_md") | Some("developer_instructions") => {
            Some((
                ContentPartAuthorship::Harness,
                ContentPartOrigin::HarnessContext,
            ))
        }
        Some("tool") => Some((
            ContentPartAuthorship::Generated,
            ContentPartOrigin::ToolPayload,
        )),
        Some("unknown") | None if legacy_tool => Some((
            ContentPartAuthorship::Generated,
            ContentPartOrigin::ToolPayload,
        )),
        Some("unknown") | None if legacy_harness => Some((
            ContentPartAuthorship::Harness,
            ContentPartOrigin::HarnessContext,
        )),
        Some("unknown") | None => {
            Some((ContentPartAuthorship::Unknown, ContentPartOrigin::Unknown))
        }
        Some(_) => Some((ContentPartAuthorship::Unknown, ContentPartOrigin::Unknown)),
    }
}

fn codex_request_user_input_question_ids(args: &Value) -> Option<Vec<String>> {
    let questions = args.get("questions")?.as_array()?;
    if questions.is_empty() {
        return None;
    }
    let mut seen = HashSet::with_capacity(questions.len());
    let mut ids = Vec::with_capacity(questions.len());
    for question in questions {
        let id = question.get("id").and_then(Value::as_str)?.trim();
        if id.is_empty() || !seen.insert(id.to_string()) {
            return None;
        }
        ids.push(id.to_string());
    }
    Some(ids)
}

fn codex_request_user_input_response(value: &Value) -> Option<Value> {
    match value {
        Value::String(text) => serde_json::from_str::<Value>(text)
            .ok()
            .and_then(|parsed| codex_request_user_input_response(&parsed)),
        Value::Object(object) if object.contains_key("answers") => Some(value.clone()),
        Value::Object(object) => object
            .get("output")
            .and_then(codex_request_user_input_response),
        _ => None,
    }
}

/// Decode only the exact native response shape for the exact native call. This intentionally
/// rejects free-form output, cancellation/error objects, accepted acknowledgements, unknown
/// question ids, and non-string answers instead of classifying them from resemblance.
fn codex_request_user_input_answers(
    output: Option<&Value>,
    question_ids: &[String],
) -> Option<Vec<String>> {
    let response = codex_request_user_input_response(output?)?;
    let answers = response.get("answers")?.as_object()?;
    if answers
        .keys()
        .any(|id| !question_ids.iter().any(|known| known == id))
    {
        return None;
    }
    let mut values = Vec::new();
    for question_id in question_ids {
        let Some(answer) = answers.get(question_id) else {
            continue;
        };
        let answer_values = answer.get("answers")?.as_array()?;
        for value in answer_values {
            let text = value.as_str()?;
            if !text.trim().is_empty() {
                values.push(text.to_string());
            }
        }
    }
    (!values.is_empty()).then_some(values)
}

fn codex_append_request_user_input_answers(
    mut content: String,
    answers: Option<&[String]>,
) -> CodexOutputWithAnswers {
    let Some(answers) = answers.filter(|answers| !answers.is_empty()) else {
        return CodexOutputWithAnswers {
            content,
            attribution: None,
        };
    };
    if content.is_empty() && answers.len() == 1 {
        content.push_str(&answers[0]);
        return CodexOutputWithAnswers {
            content,
            attribution: Some(CodexAnswerAttribution::Whole),
        };
    }
    let mut parts = Vec::new();
    let mut cursor = content.chars().count();
    if cursor > 0 {
        parts.push(MessageContentPart {
            ordinal: 0,
            start_char: 0,
            end_char: cursor,
            authorship: ContentPartAuthorship::Generated,
            origin: ContentPartOrigin::ToolPayload,
        });
    }
    for answer in answers {
        if !content.is_empty() {
            content.push('\n');
            if let Some(previous) = parts
                .last_mut()
                .filter(|part| part.authorship == ContentPartAuthorship::Generated)
            {
                previous.end_char += 1;
            } else {
                parts.push(MessageContentPart {
                    ordinal: parts.len() as u32,
                    start_char: cursor,
                    end_char: cursor + 1,
                    authorship: ContentPartAuthorship::Generated,
                    origin: ContentPartOrigin::ToolPayload,
                });
            }
            cursor += 1;
        }
        let end = cursor + answer.chars().count();
        content.push_str(answer);
        parts.push(MessageContentPart {
            ordinal: parts.len() as u32,
            start_char: cursor,
            end_char: end,
            authorship: ContentPartAuthorship::Human,
            origin: ContentPartOrigin::DirectInput,
        });
        cursor = end;
    }
    CodexOutputWithAnswers {
        content,
        attribution: Some(CodexAnswerAttribution::Mixed(parts)),
    }
}

/// Decode a native patch call or a single literal shell invocation. Arbitrary code in
/// the exec tool is not a patch receipt: it may construct, branch, catch errors, or never
/// execute the patch. Never infer a mutation merely from a marker inside such code.
fn apply_patch_text(payload: &Value) -> Option<String> {
    let name = payload.get("name").and_then(Value::as_str)?;
    let args = codex_tool_call_args(payload);
    if name == "apply_patch" {
        return args
            .as_str()
            .or_else(|| args.get("input").and_then(Value::as_str))
            .map(str::to_owned);
    }
    if !matches!(
        name,
        "exec_command" | "shell_command" | "shell" | "local_shell"
    ) {
        return None;
    }
    let command = args.get("cmd").or_else(|| args.get("command"))?;
    let command = if let Some(command) = command.as_str() {
        command
    } else {
        let argv = command.as_array()?;
        if argv.len() == 2 && argv[0].as_str() == Some("apply_patch") {
            return argv[1]
                .as_str()
                .filter(|text| {
                    text.starts_with("*** Begin Patch\n")
                        && text.trim_end().ends_with("*** End Patch")
                })
                .map(str::to_owned);
        }
        if argv.len() != 3
            || !matches!(argv[0].as_str(), Some("bash" | "sh" | "zsh"))
            || !matches!(argv[1].as_str(), Some("-lc" | "-c"))
        {
            return None;
        }
        argv[2].as_str()?
    };
    let (header, body) = command.trim().split_once('\n')?;
    let delimiter = header.strip_prefix("apply_patch <<")?.trim();
    // Only a quoted heredoc has literal bytes; an unquoted one expands shell variables.
    let quote = delimiter.chars().next()?;
    if !matches!(quote, '\'' | '"') || !delimiter.ends_with(quote) || delimiter.len() < 3 {
        return None;
    }
    let delimiter = &delimiter[1..delimiter.len() - 1];
    if !delimiter
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return None;
    }
    let (patch, end) = body.trim_end().rsplit_once('\n')?;
    (end == delimiter
        && !patch.lines().any(|line| line == delimiter)
        && patch.starts_with("*** Begin Patch\n")
        && patch.ends_with("*** End Patch"))
    .then(|| patch.to_owned())
}

fn codex_tool_call_args(payload: &Value) -> Value {
    if let Some(input) = payload.get("input") {
        return input.clone();
    }
    if let Some(arguments) = payload.get("arguments") {
        if let Some(raw) = arguments.as_str() {
            return serde_json::from_str::<Value>(raw).unwrap_or_else(|_| json!(raw));
        }
        return arguments.clone();
    }
    Value::Null
}

fn codex_event_tool_message(event_type: Option<&str>, payload: &Value) -> Option<(String, Value)> {
    match event_type? {
        "thread_goal_updated" => {
            let goal = payload.get("goal").cloned().unwrap_or(Value::Null);
            Some((
                "thread_goal_updated".to_string(),
                json!({
                    "kind": "event_metadata",
                    "event_type": "thread_goal_updated",
                    "goal": goal,
                }),
            ))
        }
        "web_search_end" => {
            let action = payload.get("action").cloned().unwrap_or(Value::Null);
            Some((
                "web_search".to_string(),
                json!({
                    "kind": "event_metadata",
                    "event_type": "web_search_end",
                    "query": payload.get("query").cloned().unwrap_or(Value::Null),
                    "queries": payload
                        .get("queries")
                        .cloned()
                        .or_else(|| action.get("queries").cloned())
                        .unwrap_or(Value::Null),
                    "action": action,
                }),
            ))
        }
        "mcp_tool_call_end" => {
            let invocation = payload.get("invocation").cloned().unwrap_or(Value::Null);
            let server = invocation
                .get("server")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let tool = invocation
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let result_preview = payload
                .get("result")
                .map(extract_text)
                .filter(|text| !text.trim().is_empty())
                .map(|text| truncate_for_display(&text, 1000));
            Some((
                format!("mcp:{server}:{tool}"),
                json!({
                    "kind": "event_metadata",
                    "event_type": "mcp_tool_call_end",
                    "invocation": invocation,
                    "duration": payload.get("duration").cloned().unwrap_or(Value::Null),
                    "result_preview": result_preview,
                }),
            ))
        }
        _ => None,
    }
}

/// Parse an apply_patch payload into `(file_path, full_content?)` per file. `*** Add File:`
/// yields the new file content (its `+` lines), replayable like a Write; `*** Update File:`
/// and `*** Delete File:` are recorded path-only (a hunk is not a replayable Write/Edit
/// delta), so they appear in `files search`/`history`/`cross-ref` but not `files extract`.
fn parse_apply_patch(patch: &str) -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    let mut add_path: Option<String> = None;
    let mut add_lines = String::new();
    fn flush(
        add_path: &mut Option<String>,
        add_lines: &mut String,
        out: &mut Vec<(String, Option<String>)>,
    ) {
        if let Some(path) = add_path.take() {
            out.push((path, Some(std::mem::take(add_lines))));
        }
    }
    for line in patch.lines() {
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            flush(&mut add_path, &mut add_lines, &mut out);
            add_path = Some(path.trim().to_string());
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            flush(&mut add_path, &mut add_lines, &mut out);
            out.push((path.trim().to_string(), None));
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            flush(&mut add_path, &mut add_lines, &mut out);
            out.push((path.trim().to_string(), None));
        } else if line.starts_with("*** Begin Patch") || line.starts_with("*** End Patch") {
            flush(&mut add_path, &mut add_lines, &mut out);
        } else if add_path.is_some() {
            if let Some(content) = line.strip_prefix('+') {
                // Codex writes one LF after each added line, including an empty last line.
                // lines() also normalizes CRLF patch input, as the upstream parser does.
                add_lines.push_str(content);
                add_lines.push('\n');
            }
        }
    }
    flush(&mut add_path, &mut add_lines, &mut out);
    out
}

/// Extract the textual output of a codex function/tool-call result. The `output` field is
/// normally a plain string (stdout plus a short metadata header); when it is structured,
/// fall back to its nested text/content via [`extract_text`].
fn codex_output_text<'a>(output: Option<&'a Value>) -> std::borrow::Cow<'a, str> {
    // The common case is a plain `output` string — borrow it (no clone). Only the rare
    // structured form pays `extract_text` (owned), and `None` borrows a static empty. The caller
    // checks emptiness on the borrow before ever materializing an owned `String`, so a
    // whitespace-only multi-MB output is never cloned.
    match output {
        Some(Value::String(s)) => std::borrow::Cow::Borrowed(s),
        Some(other) => {
            let text = extract_text(other);
            if text.is_empty() {
                // Preserve a structured native result even when it has no display-text field
                // (notably request_user_input's `{answers: ...}` response).
                std::borrow::Cow::Owned(
                    serde_json::to_string(other).unwrap_or_else(|_| String::new()),
                )
            } else {
                std::borrow::Cow::Owned(text)
            }
        }
        None => std::borrow::Cow::Borrowed(""),
    }
}

/// Codex has persisted apply_patch results as either structured metadata or rendered text.
/// Only an explicit zero exit code plus the tool's success marker proves the patch landed.
fn codex_apply_patch_succeeded(output: Option<&Value>) -> bool {
    let Some(output) = output else {
        return false;
    };
    if let Some(exit_code) = output
        .pointer("/metadata/exit_code")
        .and_then(Value::as_i64)
    {
        return exit_code == 0
            && output
                .get("output")
                .and_then(Value::as_str)
                .is_some_and(|text| text.starts_with("Success. Updated the following files:"));
    }
    let Some(text) = output.as_str() else {
        return false;
    };
    if let Ok(structured) = serde_json::from_str::<Value>(text) {
        return codex_apply_patch_succeeded(Some(&structured));
    }
    (text.starts_with("Exit code: 0\n")
        && text.contains("\nOutput:\nSuccess. Updated the following files:"))
        || (text
            .lines()
            .any(|line| line == "Process exited with code 0")
            && text.contains("\nFinal output:\nSuccess. Updated the following files:"))
}

/// The parent history a forked rollout replays before its own records.
///
/// Codex writes a fork's own `session_meta`, optionally the parent's `session_meta`, a copy of
/// the parent's records, then the fork's first `event_msg` of type `thread_settings_applied`
/// and what happened in the fork. The copied rows are kept, so nothing is lost when the parent
/// file is gone, but marked as mirrors of the parent: they are not the fork's input, analytics
/// do not count them, and they stay out of its session text, title and preview. Only a file
/// whose own `session_meta` names `forked_from_id` is read this way, and a replay that never
/// reaches that event is left as it is.
#[derive(Default)]
struct ForkReplay {
    /// The file's own `session_meta` names the session it was forked from.
    forked: bool,
    /// Start coordinates and first/last-user state to restore when the parent replay closes.
    open: Option<(usize, usize, bool, Option<String>)>,
    /// Replayed `messages` and `transcript_lines` ranges once the fork's own records begin.
    closed: Option<(std::ops::Range<usize>, std::ops::Range<usize>)>,
}

impl ForkReplay {
    fn start(
        &mut self,
        messages: usize,
        transcript: usize,
        first_user: &Option<String>,
        last_user: &Option<String>,
    ) {
        if self.open.is_none() && self.closed.is_none() {
            self.open = Some((
                messages,
                transcript,
                first_user.is_some(),
                last_user.clone(),
            ));
        }
    }

    fn end(
        &mut self,
        messages: usize,
        transcript: usize,
        first_user: &mut Option<String>,
        last_user: &mut Option<String>,
    ) {
        if let Some((message_start, transcript_start, had_first, last)) = self.open.take() {
            if !had_first {
                *first_user = None;
            }
            *last_user = last;
            self.closed = Some((message_start..messages, transcript_start..transcript));
        }
    }

    fn mark(
        &self,
        messages: Vec<RawMessage>,
        transcript_lines: &mut Vec<String>,
    ) -> Vec<RawMessage> {
        let Some((replayed, transcript)) = &self.closed else {
            return messages;
        };
        transcript_lines.drain(transcript.clone());
        messages
            .into_iter()
            .enumerate()
            .map(|(index, message)| {
                if replayed.contains(&index) {
                    message.with_record_relation(MessageRecordRelation::Mirror)
                } else {
                    message
                }
            })
            .collect()
    }
}

/// Read Codex's spawn record out of session metadata as (parent session id, agent label).
/// `source` is an object in native rollout records and an encoded JSON string in state_5.sqlite;
/// both carry `{"subagent":{"thread_spawn":...}}` (or a label-only `other` record).
fn codex_spawn_origin(raw_metadata: &Value) -> (Option<String>, Option<String>) {
    let Some(source) = raw_metadata.get("source") else {
        return (None, None);
    };
    let decoded = source.as_str().map(serde_json::from_str::<Value>);
    let source = match decoded {
        Some(Ok(ref source)) => source,
        Some(Err(_)) => return (None, None),
        None => source,
    };
    let Some(subagent) = source.get("subagent") else {
        return (None, None);
    };
    let spawn = subagent.get("thread_spawn");
    // Stored as the parent row's whole session id, not the bare thread id, so a reader sees
    // the parent's `id` and this field written identically. See providers::spawn::parent_link.
    let parent = spawn
        .and_then(|spawn| spawn.get("parent_thread_id"))
        .and_then(Value::as_str)
        .map(|thread| crate::providers::spawn::parent_link(Provider::Codex, thread));
    let label = spawn
        .and_then(|spawn| spawn.get("agent_nickname"))
        .and_then(Value::as_str)
        .or_else(|| subagent.get("other").and_then(Value::as_str))
        .or_else(|| subagent.as_str())
        .map(ToOwned::to_owned);
    (parent, label)
}

fn is_codex_injected_context(text: &str) -> bool {
    let head = text.trim_start();
    head.starts_with("The following is the Codex agent history")
        || head.starts_with("# AGENTS.md instructions")
        // Approval/goal-mode wrappers: codex resubmits the active-thread goal and hook
        // output as role:user behind these markers ("Continue working toward the active
        // thread goal …"). They are injected context, not prompts — keep them out of
        // user/correction analytics, the title, and the transcript.
        || head.starts_with("<goal_context")
        || head.starts_with("<codex_internal_context")
        || head.starts_with("<hook_prompt")
        // Codex resubmits the session environment (date/timezone/cwd/sandbox) as a role:user
        // message behind this marker on every turn — injected context, not a prompt.
        || head.starts_with("<environment_context")
        // Codex emits this role:user control record after an interrupted model turn. It is an
        // event about the user, not text entered by the user.
        || head.starts_with("<turn_aborted")
}

/// Codex emits subagent status as a user-role contextual fragment. The exact wrapper is a
/// structural protocol marker; ordinary prompts that merely mention the tag remain untouched.
fn is_codex_subagent_notification(text: &str) -> bool {
    let text = text.trim();
    text.starts_with("<subagent_notification>") && text.ends_with("</subagent_notification>")
}

/// The other form of the same rollout: `x.jsonl` for `x.jsonl.zst`, and the reverse. The two are
/// one source, so the index keeps one session for them whichever form is on disk.
pub(crate) fn rollout_sibling(path: &Path) -> Option<PathBuf> {
    if crate::source::is_compressed_jsonl(path) {
        Some(path.with_extension(""))
    } else if path.extension() == Some(std::ffi::OsStr::new("jsonl")) {
        Some(path.with_extension("jsonl.zst"))
    } else {
        None
    }
}

/// The thread a rollout file belongs to, read from its name.
///
/// Codex names a thread's original rollout `rollout-<time>-<thread>.jsonl` and each rollout that
/// `thread/revert` starts afterwards `rollout-<time>-<thread>_<rollout>.jsonl`, either form
/// optionally compressed to `.jsonl.zst` (`RolloutFileName` in
/// codex-rs/rollout/src/rollout_file_name.rs). Every rollout's `session_meta.id` names the same
/// thread, so the thread is the ID ending the name, or the first ID of a `<thread>_<rollout>` pair.
/// An ID is accepted exactly as this parser always accepted one, so older names resolve as before.
pub(crate) fn rollout_thread_id(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    let stem = name
        .strip_suffix(".jsonl.zst")
        .or_else(|| name.strip_suffix(".jsonl"))?;
    let last = trailing_codex_id(stem)?;
    let thread = stem[..stem.len() - last.len()]
        .strip_suffix('_')
        .and_then(trailing_codex_id);
    Some(thread.unwrap_or(last))
}

/// The ID ending `text`: eight lowercase hex digits, a hyphen, then 27 lowercase hex digits or
/// hyphens, the shape of the UUIDs Codex writes.
fn trailing_codex_id(text: &str) -> Option<&str> {
    let id = text.get(text.len().checked_sub(36)?..)?;
    let (head, tail) = id.split_at(8);
    let hex = |byte: u8| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte);
    (head.bytes().all(hex)
        && tail.starts_with('-')
        && tail.bytes().all(|byte| byte == b'-' || hex(byte)))
    .then_some(id)
}

pub(crate) fn codex_home_from_session_root(root: &Path) -> PathBuf {
    root.parent().unwrap_or(root).to_path_buf()
}

struct SidecarLoad<T> {
    value: T,
    degraded: Option<String>,
}

fn load_sidecar<T: Default>(
    path: &Path,
    loader: impl FnOnce(&Path) -> Result<SidecarLoad<T>>,
    warnings: &mut Vec<CodexSidecarWarning>,
) -> T {
    match fs::metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return T::default(),
        Err(error) => {
            warnings.push(CodexSidecarWarning {
                path: path.to_path_buf(),
                operation: "read_sidecar",
                message: error.to_string(),
            });
            return T::default();
        }
    }
    match loader(path) {
        Ok(loaded) => {
            if let Some(message) = loaded.degraded {
                warnings.push(CodexSidecarWarning {
                    path: path.to_path_buf(),
                    operation: "read_sidecar",
                    message,
                });
            }
            loaded.value
        }
        Err(error) => {
            warnings.push(CodexSidecarWarning {
                path: path.to_path_buf(),
                operation: "read_sidecar",
                message: format!("{error:#}"),
            });
            T::default()
        }
    }
}

fn load_threads(path: &Path) -> Result<SidecarLoad<HashMap<String, CodexMetadata>>> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let columns = thread_columns(&conn)?;
    let opt = |name: &str| {
        if columns.iter().any(|column| column == name) {
            name.to_string()
        } else {
            format!("null as {name}")
        }
    };
    let opt_text = |name: &str| {
        if columns.iter().any(|column| column == name) {
            format!("cast({name} as text) as {name}")
        } else {
            format!("null as {name}")
        }
    };
    let sql = format!(
        "select id, title, cwd, created_at, updated_at, rollout_path, first_user_message, \
         {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {} from threads",
        opt("created_at_ms"),
        opt("updated_at_ms"),
        opt("recency_at_ms"),
        opt_text("source"),
        opt_text("model_provider"),
        opt_text("sandbox_policy"),
        opt_text("approval_mode"),
        opt_text("tokens_used"),
        opt_text("git_sha"),
        opt_text("git_branch"),
        opt_text("git_origin_url"),
        opt_text("cli_version"),
        opt_text("agent_nickname"),
        opt_text("agent_role"),
        opt_text("memory_mode"),
        opt_text("model"),
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        let created_at_ms = row.get::<_, Option<i64>>(7)?;
        let updated_at_ms = row.get::<_, Option<i64>>(8)?;
        let mut raw = serde_json::Map::new();
        raw.insert("created_at_ms".to_string(), json!(created_at_ms));
        raw.insert("updated_at_ms".to_string(), json!(updated_at_ms));
        raw.insert(
            "recency_at_ms".to_string(),
            json!(row.get::<_, Option<i64>>(9)?),
        );
        for (idx, key) in [
            "source",
            "model_provider",
            "sandbox_policy",
            "approval_mode",
            "tokens_used",
            "git_sha",
            "git_branch",
            "git_origin_url",
            "cli_version",
            "agent_nickname",
            "agent_role",
            "memory_mode",
            "model",
        ]
        .into_iter()
        .enumerate()
        {
            raw.insert(
                key.to_string(),
                json!(row.get::<_, Option<String>>(10 + idx)?),
            );
        }
        Ok((
            row.get::<_, String>(0)?,
            CodexMetadata {
                title: row.get::<_, Option<String>>(1)?,
                cwd: row.get::<_, Option<String>>(2)?,
                created_at: created_at_ms.and_then(parse_unix_millis).or_else(|| {
                    row.get::<_, Option<i64>>(3)
                        .ok()
                        .flatten()
                        .and_then(parse_unix_seconds)
                }),
                updated_at: updated_at_ms.and_then(parse_unix_millis).or_else(|| {
                    row.get::<_, Option<i64>>(4)
                        .ok()
                        .flatten()
                        .and_then(parse_unix_seconds)
                }),
                rollout_path: row.get::<_, Option<String>>(5)?,
                first_user_message: row.get::<_, Option<String>>(6)?,
                raw,
            },
        ))
    })?;

    let mut map = HashMap::new();
    let mut malformed_rows = 0usize;
    for row in rows {
        match row {
            Ok((id, meta)) => {
                map.insert(id, meta);
            }
            Err(_) => malformed_rows += 1,
        }
    }
    Ok(SidecarLoad {
        value: map,
        degraded: (malformed_rows > 0).then(|| {
            format!("ignored {malformed_rows} unreadable thread row(s); retained readable rows")
        }),
    })
}

fn thread_columns(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("pragma table_info(threads)")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    let mut columns = Vec::new();
    for row in rows {
        columns.push(row?);
    }
    Ok(columns)
}

fn load_index_titles(path: &Path) -> Result<SidecarLoad<HashMap<String, String>>> {
    // Lossy decode so a stray non-UTF-8 byte in the index sidecar never aborts codex indexing.
    let bytes = fs::read(path)?;
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let mut map = HashMap::new();
    let mut malformed_lines = usize::from(raw.contains(char::REPLACEMENT_CHARACTER));
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => {
                malformed_lines += 1;
                continue;
            }
        };
        if let (Some(id), Some(title)) = (
            value.get("id").and_then(Value::as_str),
            value.get("thread_name").and_then(Value::as_str),
        ) {
            map.insert(id.to_string(), title.to_string());
        } else {
            malformed_lines += 1;
        }
    }
    Ok(SidecarLoad {
        value: map,
        degraded: (malformed_lines > 0).then(|| {
            format!("ignored {malformed_lines} malformed sidecar line(s); retained readable rows")
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        codex_apply_patch_succeeded, codex_native_shell_command_attribution, codex_output_text,
        codex_spawn_origin, codex_user_shell_command_parts, is_codex_injected_context,
        rollout_thread_id, CodexAdapter, CodexShellCommandAttribution,
    };
    use crate::models::{
        ContentPartAuthorship, ContentPartOrigin, MessageAuthorship, MessageInputOrigin, Provider,
        Role,
    };
    use serde_json::json;

    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;

    #[test]
    fn a_rollout_name_gives_its_thread_with_or_without_a_revert_suffix() {
        let thread = "01a13899-5fe0-7651-a51f-0b42469588e6";
        let rollout = "01a1389c-f5fc-76b3-ba16-9ae7e18559d6";
        for name in [
            format!("rollout-2026-10-14T12-08-58-{thread}.jsonl"),
            format!("rollout-2026-10-14T12-12-53-{thread}_{rollout}.jsonl"),
            format!("rollout-2026-10-14T12-12-53-{thread}_{rollout}.jsonl.zst"),
            // Names this parser has always accepted resolve as they did.
            format!("rollout-{thread}.jsonl"),
            format!("{thread}.jsonl.zst"),
            format!("rollout-2026-10-14T12-08-58-x_{thread}.jsonl"),
        ] {
            assert_eq!(
                rollout_thread_id(std::path::Path::new(&name)),
                Some(thread),
                "{name}"
            );
        }
        for name in [
            "rollout-2026-10-14T12-08-58.jsonl".to_string(),
            format!("rollout-2026-10-14T12-08-58-{thread}.json"),
            format!(
                "rollout-2026-10-14T12-08-58-{}.jsonl",
                thread.to_uppercase()
            ),
            "rollout-\u{e9}-0123456789abcdef0123456789abcdef.jsonl".to_string(),
        ] {
            assert_eq!(
                rollout_thread_id(std::path::Path::new(&name)),
                None,
                "{name}"
            );
        }
    }

    fn write_rollout(root: &Path, session_id: &str, cwd: &str, prompt: &str) -> PathBuf {
        fs::create_dir_all(root).unwrap();
        let path = root.join(format!("rollout-2026-08-04T00-00-00-{session_id}.jsonl"));
        fs::write(
            &path,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session_id}\",\"cwd\":\"{cwd}\"}}}}\n{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"{prompt}\"}}]}}}}\n"
            ),
        )
        .unwrap();
        path
    }

    #[test]
    fn spawn_header_checkpoint_uses_byte_offsets_and_rejects_invalid_ranges() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("synthetic.jsonl");
        let header = json!({"type":"session_meta","padding":"invalid x","payload":{"id":"synthetic","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent","agent_nickname":"worker 🦀"}}}}}).to_string();
        let prefix = " \r\n";
        let mut source = format!("{prefix}{header}\r\n").into_bytes();
        source[prefix.len() + header.find("invalid x").unwrap() + "invalid ".len()] = 0xff;
        fs::write(&path, &source).unwrap();
        let parsed = CodexAdapter::new(vec![]).parse_inner(&path).unwrap();
        let mut session = parsed.session;
        let mut metadata: serde_json::Value =
            serde_json::from_str(session.raw_metadata_json.as_deref().unwrap()).unwrap();
        let stored = metadata[super::SPAWN_HEADER_KEY].clone();
        assert_eq!(stored["offset"], prefix.len());
        assert_eq!(stored["length"], header.len() + 1);
        assert_eq!(
            stored["sha256"],
            crate::hashing::sha256(&source[prefix.len()..source.len() - 1])
        );
        assert!(super::spawn_header_is_current(
            &path,
            &session,
            source.len() as i64
        ));
        for checkpoint in [
            serde_json::Value::Null,
            json!({"offset": u64::MAX, "length": 2, "sha256":"invalid"}),
            json!({"offset": 0, "length": source.len() + 1, "sha256":"invalid"}),
            json!({"offset": prefix.len(), "length": header.len(), "sha256":"invalid"}),
        ] {
            metadata[super::SPAWN_HEADER_KEY] = checkpoint;
            session.raw_metadata_json = Some(metadata.to_string());
            assert!(!super::spawn_header_is_current(
                &path,
                &session,
                source.len() as i64
            ));
        }
        metadata[super::SPAWN_HEADER_KEY] = stored;
        session.raw_metadata_json = Some(metadata.to_string());
        fs::write(&path, prefix).unwrap();
        assert!(!super::spawn_header_is_current(
            &path,
            &session,
            source.len() as i64
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_rollout_name_that_is_not_utf8_is_still_recognized_and_paired() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let compressed = Path::new(OsStr::from_bytes(b"/x/rollout-\xff.jsonl.zst"));
        let plain = Path::new(OsStr::from_bytes(b"/x/rollout-\xff.jsonl"));
        assert!(crate::source::is_compressed_jsonl(compressed));
        assert_eq!(super::rollout_sibling(compressed).as_deref(), Some(plain));
        assert_eq!(super::rollout_sibling(plain).as_deref(), Some(compressed));
        assert_eq!(super::rollout_sibling(Path::new("/x/notes.txt")), None);
    }

    #[test]
    fn a_compressed_rollout_is_discovered_and_parses_like_its_plain_bytes() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("sessions");
        let session_id = "019efd97-d602-7922-89dd-467272106505";
        let plain = write_rollout(&root, session_id, "/rollout", "compressed prompt");
        let adapter = CodexAdapter::new(vec![root.clone()]);
        let from_plain = adapter.parse(&adapter.discover()[0]);

        let bytes = fs::read(&plain).unwrap();
        let compressed = plain.with_extension("jsonl.zst");
        fs::write(&compressed, zstd::encode_all(bytes.as_slice(), 1).unwrap()).unwrap();
        // While both forms exist the plain file wins; then Codex deletes it.
        assert_eq!(adapter.discover().len(), 1);
        assert_eq!(adapter.discover()[0].path, plain);
        fs::remove_file(&plain).unwrap();

        let sources = adapter.discover();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].path, compressed);
        let from_compressed = adapter.parse(&sources[0]);
        assert_eq!(from_compressed.session.parse_warning, None);
        assert_eq!(from_compressed.session.id, from_plain.session.id);
        let contents = |parsed: &crate::models::ParsedSession| {
            parsed
                .messages
                .iter()
                .map(|message| message.content.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(contents(&from_compressed), contents(&from_plain));
    }

    #[test]
    fn a_corrupt_compressed_rollout_is_a_warned_parse_failure() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("sessions");
        fs::create_dir_all(&root).unwrap();
        let path =
            root.join("rollout-2026-08-04T00-00-00-019efd97-d602-7922-89dd-467272106505.jsonl.zst");
        fs::write(&path, b"not a zstd stream at all").unwrap();
        let adapter = CodexAdapter::new(vec![root]);
        let sources = adapter.discover();
        assert_eq!(sources.len(), 1);
        let parsed = adapter.parse(&sources[0]);
        assert!(
            parsed
                .session
                .parse_warning
                .as_deref()
                .is_some_and(|warning| warning.contains("zstd")),
            "{:?}",
            parsed.session.parse_warning
        );
        assert!(parsed.messages.is_empty());
    }

    #[test]
    fn metadata_is_scoped_to_each_configured_codex_home() {
        let temp = tempdir().unwrap();
        let session_id = "019efd97-d602-7922-89dd-467272106505";
        let home_a = temp.path().join("home-a");
        let home_b = temp.path().join("home-b");
        let root_a = home_a.join("sessions");
        let root_b = home_b.join("sessions");
        let file_a = write_rollout(&root_a, session_id, "/rollout/a", "prompt a");
        let file_b = write_rollout(&root_b, session_id, "/rollout/b", "prompt b");
        fs::write(
            home_a.join("session_index.jsonl"),
            format!(r#"{{"id":"{session_id}","thread_name":"title a"}}"#),
        )
        .unwrap();
        fs::write(
            home_b.join("session_index.jsonl"),
            format!(r#"{{"id":"{session_id}","thread_name":"title b"}}"#),
        )
        .unwrap();

        let adapter = CodexAdapter::new(vec![root_a, root_b]);
        let sources = adapter.discover();
        let parsed_a = adapter.parse(sources.iter().find(|source| source.path == file_a).unwrap());
        let parsed_b = adapter.parse(sources.iter().find(|source| source.path == file_b).unwrap());

        assert_eq!(parsed_a.session.title.as_deref(), Some("title a"));
        assert_eq!(parsed_b.session.title.as_deref(), Some("title b"));
        assert_eq!(parsed_a.session.cwd.as_deref(), Some("/rollout/a"));
        assert_eq!(parsed_b.session.cwd.as_deref(), Some("/rollout/b"));
    }

    #[test]
    fn absent_sidecars_are_quiet_but_malformed_index_is_degraded() {
        let temp = tempdir().unwrap();
        let home = temp.path().join("home");
        let root = home.join("sessions");
        write_rollout(
            &root,
            "019efd97-d602-7922-89dd-467272106505",
            "/rollout",
            "fallback title",
        );

        let quiet = CodexAdapter::new(vec![root.clone()]);
        assert!(quiet.sidecar_warnings().is_empty());

        fs::write(
            home.join("session_index.jsonl"),
            "{not json}\n{\"id\":\"kept\",\"thread_name\":\"kept title\"}\n",
        )
        .unwrap();
        let degraded = CodexAdapter::new(vec![root]);

        assert_eq!(degraded.sidecar_warnings().len(), 1);
        assert_eq!(degraded.sidecar_warnings()[0].operation, "read_sidecar");
        assert!(degraded.sidecar_warnings()[0].message.contains("malformed"));
    }

    #[test]
    fn codex_output_text_handles_string_and_structured() {
        // The common case: output is a plain stdout string.
        assert_eq!(
            codex_output_text(Some(&json!("hello\nworld"))),
            "hello\nworld"
        );
        // Defensive: a structured output falls back to its nested text.
        assert_eq!(
            codex_output_text(Some(&json!({"content": "nested out"}))),
            "nested out"
        );
        assert_eq!(codex_output_text(None), "");
    }

    #[test]
    fn forked_session_does_not_inherit_parent_spawn_attribution() {
        let source = json!({"subagent":{"thread_spawn":{"parent_thread_id":"grandparent","agent_nickname":"parent-agent"}}});
        let records = [
            json!({"type":"session_meta","payload":{"id":"own-fork","source":"cli"}}),
            json!({"type":"session_meta","payload":{"id":"parent","source":source}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user",
                "content":[{"type":"input_text","text":"own fork prompt"}],
                "internal_chat_message_metadata_passthrough":{"content_item_metadata":[{"provenance":{"type":"user"}}]}}}),
        ].into_iter().map(|value| value.to_string()+"\n").collect::<String>();
        let parsed = CodexAdapter::new(vec![])
            .parse_reader(
                std::io::Cursor::new(records),
                Path::new("/tmp/own-fork.jsonl"),
            )
            .unwrap();
        assert!(parsed.session.parent_session_id.is_none());
        assert!(parsed.session.agent_label.is_none());
        assert_eq!(
            parsed.messages[0].provenance.authorship,
            MessageAuthorship::Human
        );
    }

    /// A forked session's rollout carries TWO session_meta lines: its own first, then the
    /// inherited parent metadata. Binding the id to the last one silently rebinds this file
    /// to the parent's session, and the `on conflict(id) do update` upsert in db.rs then
    /// overwrites the parent's row instead of storing this session. A file that produces
    /// no row is invisible to health signals keyed only by indexed sessions.
    #[test]
    fn forked_session_keeps_its_own_id_not_the_parents() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let own = "019f5ce8-afff-76a3-86f2-174307f3fa07";
        let parent = "019f4a0e-d714-7cc3-8003-543a04a1a821";
        let file = root.join(format!("rollout-2026-07-13T15-16-21-{own}.jsonl"));
        // Field order and shape copied from a real forked rollout: the fork's own meta names
        // the parent in session_id/forked_from_id while `id` is its own; the second meta is
        // the parent's, whose `id` IS the parent.
        fs::write(
            &file,
            format!(
                r#"{{"timestamp":"2026-07-13T19:16:21.425Z","type":"session_meta","payload":{{"session_id":"{parent}","id":"{own}","forked_from_id":"{parent}","parent_thread_id":"{parent}","timestamp":"2026-07-13T19:16:21.181Z","cwd":"/tmp/fork"}}}}
{{"timestamp":"2026-07-13T19:16:21.425Z","type":"session_meta","payload":{{"session_id":"{parent}","id":"{parent}","timestamp":"2026-07-10T03:25:14.410Z","cwd":"/tmp/parent"}}}}
{{"timestamp":"2026-07-13T19:16:24.000Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"text","text":"continue in the fork"}}]}}}}
"#
            ),
        )
        .unwrap();

        let adapter = CodexAdapter::new(vec![root]);
        let sources = adapter.discover();
        assert_eq!(sources.len(), 1);
        let parsed = adapter.parse(&sources[0]);

        assert_eq!(
            parsed.session.provider_session_id, own,
            "fork must keep its own id; taking the parent's collides with the parent row"
        );
        // The first meta also supplies cwd, so the parent's later meta must not win there either.
        assert_eq!(parsed.session.cwd.as_deref(), Some("/tmp/fork"));
    }

    /// Parent replay remains searchable as mirror content and is excluded from the fork's
    /// input, transcript, title and preview once its settings event arrives.
    #[test]
    fn fork_without_parent_metadata_marks_replay_only_after_settings_arrive() {
        use crate::models::MessageRecordRelation;
        let temp = tempdir().unwrap();
        let file = temp.path().join("rollout-fork.jsonl");
        let prefix = [
            json!({"type":"compacted","payload":{"message":"parent summary"}}),
            json!({"type":"session_meta","payload":{"id":"fork","forked_from_id":"parent"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"parent prompt"}]}}),
        ];
        let adapter = CodexAdapter::new(vec![temp.path().to_path_buf()]);
        for closed in [false, true] {
            let mut records = prefix.to_vec();
            if closed {
                records
                    .push(json!({"type":"event_msg","payload":{"type":"thread_settings_applied"}}));
            }
            records.push(json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"fork task"}]}}));
            fs::write(
                &file,
                records
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .unwrap();
            let parsed = adapter.parse(&adapter.discover()[0]);
            assert_eq!(parsed.session.provider_session_id, "fork");
            assert!(
                parsed.session.parent_session_id.is_none(),
                "a fork is not a spawned agent"
            );
            for message in &parsed.messages {
                let replay = closed && message.content != "fork task";
                assert_eq!(
                    message.provenance.record_relation,
                    if replay {
                        MessageRecordRelation::Mirror
                    } else {
                        MessageRecordRelation::Original
                    },
                    "{closed}: {}",
                    message.content
                );
            }
            assert_eq!(parsed.transcript_text.contains("parent prompt"), !closed);
            if closed {
                assert_eq!(parsed.session.preview_text, "fork task");
            }
        }
    }

    #[test]
    fn forked_session_keeps_replayed_parent_history_only_as_mirror_rows() {
        use crate::models::MessageRecordRelation;
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let own = "019f5ce8-afff-76a3-86f2-174307f3fa08";
        let parent = "019f4a0e-d714-7cc3-8003-543a04a1a822";
        let file = root.join(format!("rollout-2026-07-13T15-16-21-{own}.jsonl"));
        let lines = [
            json!({"timestamp":"2026-07-13T19:16:21.425Z","type":"session_meta","payload":{"id":own,"forked_from_id":parent,"timestamp":"2026-07-13T19:16:21.181Z","cwd":"/tmp/fork","source":{"subagent":{"thread_spawn":{"parent_thread_id":parent,"agent_nickname":"worker"}}}}}),
            json!({"timestamp":"2026-07-13T19:16:21.425Z","type":"session_meta","payload":{"id":parent,"timestamp":"2026-07-10T03:25:14.410Z","cwd":"/tmp/parent"}}),
            json!({"timestamp":"2026-07-13T19:16:21.426Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"parent prompt"}]}}),
            json!({"timestamp":"2026-07-13T19:16:21.426Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"parent answer"}]}}),
            json!({"timestamp":"2026-07-13T19:16:21.427Z","type":"event_msg","payload":{"type":"thread_settings_applied"}}),
            json!({"timestamp":"2026-07-13T19:16:24.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"fork task"}]}}),
        ]
        .map(|line| line.to_string())
        .join("\n");
        fs::write(&file, lines).unwrap();

        let adapter = CodexAdapter::new(vec![root]);
        let parsed = adapter.parse(&adapter.discover()[0]);
        let rows: Vec<_> = parsed
            .messages
            .iter()
            .map(|m| (m.content.as_str(), m.provenance.record_relation))
            .collect();
        assert_eq!(
            rows,
            [
                ("parent prompt", MessageRecordRelation::Mirror),
                ("parent answer", MessageRecordRelation::Mirror),
                ("fork task", MessageRecordRelation::Original),
            ]
        );
        assert!(
            !parsed.transcript_text.contains("parent"),
            "{}",
            parsed.transcript_text
        );
        assert!(parsed.transcript_text.contains("fork task"));
        assert_eq!(parsed.session.preview_text, "fork task");
        assert_eq!(
            parsed.session.parent_session_id.as_deref(),
            Some(format!("codex:{parent}").as_str())
        );
    }

    #[test]
    fn indexes_function_call_output_as_tool_message_with_name() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let session_id = "019efd97-d602-7922-89dd-467272106505";
        // Real codex shapes: a function_call (name + call_id) then its function_call_output
        // (call_id + string output), plus a normal user message.
        let file = root.join(format!("rollout-2026-06-25T03-04-06-{session_id}.jsonl"));
        fs::write(
            &file,
            r#"{"type":"session_meta","payload":{"id":"019efd97-d602-7922-89dd-467272106505","timestamp":"2026-06-25T07:00:00.000Z","cwd":"/tmp/proj"}}
{"timestamp":"2026-06-25T07:06:23.136Z","type":"response_item","payload":{"type":"function_call","id":"fc_1","name":"exec_command","call_id":"call_1","arguments":"{\"cmd\":\"ls\"}"}}
{"timestamp":"2026-06-25T07:06:23.197Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_1","output":"Cargo.toml\nsrc"}}
{"timestamp":"2026-06-25T07:06:24.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"text","text":"list the files"}]}}
"#,
        )
        .unwrap();

        let adapter = CodexAdapter::new(vec![root]);
        let sources = adapter.discover();
        assert_eq!(sources.len(), 1);
        let parsed = adapter.parse(&sources[0]);
        assert_eq!(parsed.session.provider, Provider::Codex);

        let tool_input = parsed
            .messages
            .iter()
            .find(|m| m.role == Role::Tool && m.content.contains(r#""cmd":"ls""#))
            .expect("function_call input indexed as a Role::Tool message");
        assert_eq!(tool_input.tool_name.as_deref(), Some("exec_command"));
        assert!(tool_input.content.contains(r#""kind":"tool_call""#));
        assert_eq!(tool_input.kind, crate::models::MessageKind::ToolCall);
        assert_eq!(tool_input.tool_call_id.as_deref(), Some("call_1"));
        let tool_input_identity = tool_input
            .provenance
            .correlation_identity
            .as_ref()
            .expect("response_item payload.id is retained as message identity");
        assert_eq!(
            tool_input_identity.authority,
            crate::models::MessageCorrelationAuthority::OpenAi
        );
        assert_eq!(tool_input_identity.scope, format!("codex:{session_id}"));
        assert_eq!(tool_input_identity.id, "fc_1");
        let tool_output = parsed
            .messages
            .iter()
            .find(|m| m.role == Role::Tool && m.content == "Cargo.toml\nsrc")
            .expect("function_call_output indexed as a Role::Tool message");
        assert_eq!(tool_output.tool_name.as_deref(), Some("exec_command"));
        assert_eq!(tool_output.kind, crate::models::MessageKind::ToolResult);
        assert_eq!(tool_output.tool_call_id.as_deref(), Some("call_1"));
        assert!(
            tool_output.provenance.correlation_identity.is_none(),
            "shared call_id correlates distinct call/result messages and is not copy identity"
        );
        // The real user prompt is still indexed as a user message.
        let user = parsed
            .messages
            .iter()
            .find(|m| m.role == Role::User && m.content == "list the files")
            .expect("real user prompt");
        assert!(
            user.provenance.correlation_identity.is_none(),
            "missing payload.id must not be synthesized from timestamp, content, or sequence"
        );
        // Tool output stays out of the human transcript.
        assert!(!parsed.transcript_text.contains("Cargo.toml"));
    }

    #[test]
    fn native_content_metadata_overrides_user_role_and_preserves_regions() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let session_id = "019efd97-d602-7922-89dd-467272106505";
        let file = root.join(format!("rollout-2026-06-25T03-04-06-{session_id}.jsonl"));
        fs::write(
            &file,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"019efd97-d602-7922-89dd-467272106505","cwd":"/tmp/proj"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"direct-é"},{"type":"input_text","text":" same-token"},{"type":"input_text","text":" same-token"}],"internal_chat_message_metadata_passthrough":{"content_item_metadata":[{"provenance":{"type":"user"}},{"provenance":{"type":"harness","hook":false}},{"provenance":{"type":"tool","namespace":"functions"}}]}}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<subagent_notification>generated-sentinel</subagent_notification>"}]}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"misaligned-token"}],"internal_chat_message_metadata_passthrough":{"content_item_metadata":[]}}}"#,
            ),
        )
        .unwrap();

        let adapter = CodexAdapter::new(vec![root]);
        let parsed = adapter.parse(&adapter.discover()[0]);
        let mixed = parsed
            .messages
            .iter()
            .find(|message| message.content.contains("direct-é"))
            .expect("native mixed message");
        assert_eq!(mixed.provenance.authorship, MessageAuthorship::Mixed);
        assert_eq!(mixed.provenance.content_parts.len(), 3);
        assert_eq!(
            mixed.provenance.content_parts[0].authorship,
            ContentPartAuthorship::Human
        );
        assert_eq!(
            mixed.provenance.content_parts[0].origin,
            ContentPartOrigin::DirectInput
        );
        assert_eq!(
            mixed.provenance.content_parts[1].authorship,
            ContentPartAuthorship::Harness
        );
        assert_eq!(
            mixed.provenance.content_parts[2].authorship,
            ContentPartAuthorship::Generated
        );
        assert!(mixed.provenance.validate(&mixed.content).is_ok());

        let subagent = parsed
            .messages
            .iter()
            .find(|message| message.content.contains("generated-sentinel"))
            .expect("subagent notice");
        assert_eq!(subagent.provenance.authorship, MessageAuthorship::Agent);

        let misaligned = parsed
            .messages
            .iter()
            .find(|message| message.content == "misaligned-token")
            .expect("misaligned native message");
        assert_eq!(misaligned.provenance.authorship, MessageAuthorship::Unknown);
        assert_eq!(misaligned.provenance.input_origin, None);
        assert!(misaligned.provenance.content_parts.is_empty());
    }

    #[test]
    fn native_shell_command_partitions_command_from_stdout_in_service_search() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let session_id = "019efd97-d602-7922-89dd-467272106505";
        let shell = "<user_shell_command>\n<command>\nprintf shell-only-command🙂 </user_shell_command> </user_shell_command>\n</command>\n<result>\nExit code: 0\nDuration: 0.0000 seconds\nOutput:\nshell-only-output🙂\n</result>\n</user_shell_command>";
        let ambiguous = "<user_shell_command>\n<command>\nprintf command\n</command>\n<result>\nExit code: 0\nDuration: 0.0000 seconds\nOutput:\nfake-produced-only\n</command>\n<result>\nExit code: 0\nDuration: 0.0000 seconds\nOutput:\nremaining stdout\n</result>\n</user_shell_command>";
        let file = root.join(format!("rollout-2026-06-25T03-04-06-{session_id}.jsonl"));
        let records = [
            json!({
                "type": "session_meta",
                "payload": {"id": session_id, "cwd": "/tmp/proj"}
            }),
            json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": shell}],
                    "internal_chat_message_metadata_passthrough": {
                        "content_item_kinds": ["shell.user_command"],
                        "content_item_metadata": [{"provenance": {"type": "harness"}}]
                    }
                }
            }),
            json!({"type":"response_item","payload":{
                "type":"message","role":"user","content":[{"type":"input_text","text":ambiguous}],
                "internal_chat_message_metadata_passthrough":{
                    "content_item_kinds":["shell.user_command"],
                    "content_item_metadata":[{"provenance":{"type":"harness"}}]
                }
            }}),
        ]
        .into_iter()
        .map(|record| record.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        fs::write(&file, format!("{records}\n")).unwrap();

        let adapter = CodexAdapter::new(vec![root]);
        let parsed = adapter.parse(&adapter.discover()[0]);
        let message = parsed
            .messages
            .iter()
            .find(|message| message.content == shell)
            .expect("native shell message");
        assert_eq!(message.provenance.authorship, MessageAuthorship::Mixed);
        assert_eq!(
            message.provenance.input_origin,
            Some(MessageInputOrigin::DirectConversation)
        );
        assert!(message.provenance.validate(&message.content).is_ok());
        let direct = message
            .provenance
            .content_parts
            .iter()
            .find(|part| part.origin == ContentPartOrigin::DirectInput)
            .expect("command direct-input part");
        assert_eq!(
            &message.content.chars().collect::<Vec<_>>()[direct.start_char..direct.end_char]
                .iter()
                .collect::<String>(),
            "printf shell-only-command🙂 </user_shell_command> </user_shell_command>"
        );

        let db_path = temp.path().join("input-index.db");
        let db = crate::db::Db::open(&db_path).unwrap();
        db.mark_schema_current().unwrap();
        db.upsert_session(&parsed, 0, 0).unwrap();
        let config = crate::config::Config::default();
        let service = crate::service::MessageService::new(
            &config,
            &db,
            crate::message_search::SearchSurface::Rust,
        );
        for (query, scope, expected) in [
            ("fake-produced-only", Some(crate::models::Role::User), 0),
            ("fake-produced-only", None, 1),
            ("shell-only-command🙂", Some(crate::models::Role::User), 1),
            ("shell-only-output🙂", Some(crate::models::Role::User), 0),
            ("shell-only-output🙂", None, 1),
        ] {
            let request = {
                let builder = crate::message_search::MessageSearchRequest::builder(
                    crate::message_search::MessageQuery::literal(query).unwrap(),
                    crate::message_search::MessageTarget::content(),
                );
                if let Some(role) = scope {
                    builder.role(role)
                } else {
                    builder
                }
            }
            .build()
            .unwrap();
            assert_eq!(
                service.search(request).unwrap().hits().len(),
                expected,
                "{query}"
            );
        }
    }

    #[test]
    fn native_shell_and_other_parts_preserve_independent_input_regions() {
        let temp = tempdir().unwrap();
        let file = temp
            .path()
            .join("rollout-2026-06-25T03-04-06-019efd97-d602-7922-89dd-467272106505.jsonl");
        let shell = "<user_shell_command>\n<command>\nmixed-command🙂\n</command>\n<result>\nExit code: 0\nDuration: 0.0000 seconds\nOutput:\nmixed-stdout🙂\n</result>\n</user_shell_command>";
        for (shell, valid_shell) in [
            (shell.to_owned(), true),
            (format!("{shell}unexpected suffix"), false),
        ] {
            let payload = json!({"type":"response_item","payload":{
                "type":"message","role":"user","content":[
                    {"type":"input_text","text":"<environment_context>mixed-context-é</environment_context>"},
                    {"type":"input_text","text":"mixed-prompt-é"},
                    {"type":"input_text","text":shell}
                ],
                "internal_chat_message_metadata_passthrough":{
                    "content_item_kinds":[null,null,"shell.user_command"],
                    "content_item_metadata":[
                        {"provenance":{"type":"harness"}},
                        {"provenance":{"type":"user"}},
                        {"provenance":{"type":"harness"}}
                    ]
                }
            }});
            let mut assistant = payload.clone();
            assistant["payload"]["role"] = json!("assistant");
            fs::write(&file, format!("{payload}\n{assistant}\n")).unwrap();
            let adapter = CodexAdapter::new(vec![temp.path().to_path_buf()]);
            let parsed = adapter.parse(&adapter.discover()[0]);
            assert_eq!(parsed.messages.len(), 2);
            assert_eq!(
            parsed.messages[0].content,
            format!("<environment_context>mixed-context-é</environment_context>\nmixed-prompt-é\n{shell}")
        );
            assert!(parsed.messages[0]
                .provenance
                .validate(&parsed.messages[0].content)
                .is_ok());
            let db =
                crate::db::Db::open(&temp.path().join(format!("index-{valid_shell}.db"))).unwrap();
            db.mark_schema_current().unwrap();
            db.upsert_session(&parsed, 0, 0).unwrap();
            let config = crate::config::Config::default();
            let service = crate::service::MessageService::new(
                &config,
                &db,
                crate::message_search::SearchSurface::Rust,
            );
            for (query, expected) in [
                ("mixed-prompt-é", 1),
                ("mixed-command🙂", usize::from(valid_shell)),
                ("mixed-stdout🙂", 0),
                ("mixed-context-é", 0),
            ] {
                for (scope, count) in [(Some(crate::models::Role::User), expected), (None, 2)] {
                    let request = {
                        let builder = crate::message_search::MessageSearchRequest::builder(
                            crate::message_search::MessageQuery::literal(query).unwrap(),
                            crate::message_search::MessageTarget::content(),
                        );
                        if let Some(role) = scope {
                            builder.role(role)
                        } else {
                            builder
                        }
                    }
                    .build()
                    .unwrap();
                    assert_eq!(
                        service.search(request).unwrap().hits().len(),
                        count,
                        "{query} {scope:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn shell_frame_requires_native_kind_and_rejects_malformed_typed_text() {
        let shell = "<user_shell_command>\n<command>legacy command</command>\n<result>\nOutput:\nlegacy output\n</result>\n</user_shell_command>";
        let legacy = json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": shell}]
        });
        assert!(
            codex_native_shell_command_attribution(&legacy, ContentPartAuthorship::Human).is_none()
        );

        let malformed = json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "<user_shell_command>broken"}],
            "internal_chat_message_metadata_passthrough": {
                "content_item_kinds": ["shell.user_command"],
                "content_item_metadata": [{"provenance": {"type": "user"}}]
            }
        });
        assert!(matches!(
            codex_native_shell_command_attribution(&malformed, ContentPartAuthorship::Human),
            Some(CodexShellCommandAttribution::Unknown)
        ));

        let empty_command = "<user_shell_command>\n<command>\n\n</command>\n<result>\nExit code: 1\nDuration: 0.0000 seconds\nOutput:\nerror\n</result>\n</user_shell_command>";
        let parts = codex_user_shell_command_parts(empty_command, ContentPartAuthorship::Human)
            .expect("empty command still has a structurally valid frame");
        assert!(parts
            .iter()
            .all(|part| part.origin != ContentPartOrigin::DirectInput));
        assert!(crate::models::MessageProvenance {
            authorship: MessageAuthorship::Mixed,
            content_parts: parts,
            ..Default::default()
        }
        .validate(empty_command)
        .is_ok());
    }

    #[test]
    fn delegated_native_shell_command_is_agent_input_not_root_user_input() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let session_id = "019efd97-d602-7922-89dd-467272106505";
        let shell = "<user_shell_command>\n<command>\ndelegated-command\n</command>\n<result>\nExit code: 0\nDuration: 0.0000 seconds\nOutput:\ndelegated-output\n</result>\n</user_shell_command>";
        let file = root.join(format!("rollout-2026-06-25T03-04-06-{session_id}.jsonl"));
        let records = [
            json!({
                "type": "session_meta",
                "payload": {
                    "id": session_id,
                    "cwd": "/tmp/proj",
                    "source": r#"{"subagent":{"thread_spawn":{"parent_thread_id":"parent","agent_nickname":"worker"}}}"#
                }
            }),
            json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": shell}],
                    "internal_chat_message_metadata_passthrough": {
                        "content_item_kinds": ["shell.user_command"],
                        "content_item_metadata": [{"provenance": {"type": "harness"}}]
                    }
                }
            }),
            json!({"type":"response_item","payload":{
                "type":"message","role":"user",
                "content":[{"type":"input_text","text":"delegated-native-user"}],
                "internal_chat_message_metadata_passthrough":{
                    "content_item_metadata":[{"provenance":{"type":"user"}}]
                }
            }}),
        ]
        .into_iter()
        .map(|record| record.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        fs::write(&file, format!("{records}\n")).unwrap();
        let adapter = CodexAdapter::new(vec![root]);
        let parsed = adapter.parse(&adapter.discover()[0]);
        let delegated = parsed
            .messages
            .iter()
            .find(|message| message.content == "delegated-native-user")
            .unwrap();
        assert_eq!(
            delegated.provenance.authorship,
            MessageAuthorship::Agent,
            "native user-role provenance does not turn a known agent delegation into root user input");
        let message = parsed
            .messages
            .iter()
            .find(|message| message.content == shell)
            .expect("delegated shell message");
        assert_eq!(message.provenance.authorship, MessageAuthorship::Mixed);
        assert!(message.provenance.content_parts.iter().any(|part| {
            part.authorship == ContentPartAuthorship::Agent
                && part.origin == ContentPartOrigin::DirectInput
        }));
        let db = crate::db::Db::open(&temp.path().join("input-index.db")).unwrap();
        db.mark_schema_current().unwrap();
        db.upsert_session(&parsed, 0, 0).unwrap();
        let config = crate::config::Config::default();
        let service = crate::service::MessageService::new(
            &config,
            &db,
            crate::message_search::SearchSurface::Rust,
        );
        let request = crate::message_search::MessageSearchRequest::builder(
            crate::message_search::MessageQuery::literal("delegated-command").unwrap(),
            crate::message_search::MessageTarget::content(),
        )
        .role(crate::models::Role::User)
        .build()
        .unwrap();
        assert!(service.search(request).unwrap().hits().is_empty());
    }

    #[test]
    fn request_user_input_answers_are_correlated_in_question_order_once() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let session_id = "019efd97-d602-7922-89dd-467272106505";
        let file = root.join(format!("rollout-2026-06-25T03-04-06-{session_id}.jsonl"));
        fs::write(
            &file,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"019efd97-d602-7922-89dd-467272106505","cwd":"/tmp/proj"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"function_call","id":"fc_question","name":"request_user_input","call_id":"call_question","arguments":"{\"questions\":[{\"id\":\"q1\",\"header\":\"First\",\"question\":\"First?\"},{\"id\":\"q2\",\"header\":\"Second\",\"question\":\"Second?\"}]}"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"function_call_output","id":"fo_question","call_id":"call_question","output":"{\"answers\":{\"q2\":{\"answers\":[\"two \\\"quoted\\\"🙂\"]},\"q1\":{\"answers\":[\"one-é\"]}}}"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call_question","output":"{\"answers\":{\"q1\":{\"answers\":[\"duplicate-must-not-appear\"]}}}"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"function_call","name":"request_user_input_async","call_id":"call_async","arguments":"{\"questions\":[{\"id\":\"qa\",\"question\":\"Async?\"}]}"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call_async","output":"{\"accepted\":true}"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"function_call","name":"request_user_input","call_id":"call_cancel","arguments":"{\"questions\":[{\"id\":\"qc\",\"question\":\"Cancel?\"}]}"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call_cancel","output":{"error":"request_user_input cancelled"}}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call_missing","output":"{\"answers\":{\"qm\":{\"answers\":[\"missing-call-not-input\"]}}}"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"function_call","name":"lookalike","call_id":"call_lookalike","arguments":"{\"questions\":[{\"id\":\"ql\",\"question\":\"Lookalike?\"}]}"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call_lookalike","output":"{\"answers\":{\"ql\":{\"answers\":[\"not-input\"]}}}"}}"#,
            ),
        )
        .unwrap();

        let adapter = CodexAdapter::new(vec![root]);
        let parsed = adapter.parse(&adapter.discover()[0]);
        let answer = parsed
            .messages
            .iter()
            .find(|message| message.content.contains("one-é"))
            .expect("linked answer output");
        assert!(answer.content.contains(r#"{"answers":{"q2""#));
        assert!(answer.content.contains("one-é\ntwo"));
        assert!(!answer.content.contains("duplicate-must-not-appear"));
        assert_eq!(
            answer.provenance.input_origin,
            Some(MessageInputOrigin::InteractiveAnswer)
        );
        assert_eq!(
            answer
                .provenance
                .correlation_identity
                .as_ref()
                .map(|identity| identity.id.as_str()),
            Some("fo_question")
        );
        assert_eq!(answer.provenance.authorship, MessageAuthorship::Mixed);
        assert_eq!(
            answer.provenance.content_parts[0].authorship,
            ContentPartAuthorship::Generated
        );
        assert_eq!(
            answer.provenance.content_parts[1].authorship,
            ContentPartAuthorship::Human
        );
        assert!(answer.provenance.validate(&answer.content).is_ok());

        let async_ack = parsed
            .messages
            .iter()
            .find(|message| message.content.contains(r#"{"accepted":true}"#))
            .expect("async acknowledgement preserved");
        assert_eq!(
            async_ack.provenance.authorship,
            MessageAuthorship::Generated
        );
        assert!(async_ack.provenance.content_parts.is_empty());
        let cancelled = parsed
            .messages
            .iter()
            .find(|message| message.content.contains("request_user_input cancelled"))
            .expect("cancellation output preserved");
        assert_eq!(
            cancelled.provenance.authorship,
            MessageAuthorship::Generated
        );
        assert!(cancelled.provenance.content_parts.is_empty());
        let missing = parsed
            .messages
            .iter()
            .find(|message| message.content.contains("missing-call-not-input"))
            .expect("unmatched output preserved");
        assert_eq!(missing.provenance.authorship, MessageAuthorship::Generated);
        assert!(missing.provenance.content_parts.is_empty());
        let lookalike = parsed
            .messages
            .iter()
            .find(|message| message.content.contains("not-input"))
            .expect("lookalike output preserved");
        assert_eq!(
            lookalike.provenance.authorship,
            MessageAuthorship::Generated
        );
        assert!(lookalike.provenance.content_parts.is_empty());
        let db_path = temp.path().join("input-index.db");
        let db = crate::db::Db::open(&db_path).unwrap();
        db.mark_schema_current().unwrap();
        db.upsert_session(&parsed, 0, 0).unwrap();
        let config = crate::config::Config::default();
        let service = crate::service::MessageService::new(
            &config,
            &db,
            crate::message_search::SearchSurface::Rust,
        );
        for (query, expected) in [("one-é", 1), ("two \"quoted\"🙂", 1), ("one-é\ntwo", 0)] {
            let request = crate::message_search::MessageSearchRequest::builder(
                crate::message_search::MessageQuery::literal(query).unwrap(),
                crate::message_search::MessageTarget::content(),
            )
            .role(crate::models::Role::User)
            .build()
            .unwrap();
            assert_eq!(
                service.search(request).unwrap().hits().len(),
                expected,
                "{query}"
            );
        }
        let raw = parsed.session.raw_metadata_json.unwrap();
        assert!(raw.contains(r#""requires_full_parse_for_tail":true"#));
    }

    #[test]
    fn spawned_request_user_input_answer_stays_out_of_user_input_scope() {
        let temp = tempdir().unwrap();
        let root_path = temp.path().join("rollout-root.jsonl");
        let child_path = temp.path().join("rollout-child.jsonl");
        let call = r#"{"type":"response_item","payload":{"type":"function_call","name":"request_user_input","call_id":"ask-call","arguments":"{\"questions\":[{\"id\":\"q\",\"question\":\"Proceed?\"}]}"}}"#;
        let answer = r#"{"type":"response_item","payload":{"type":"function_call_output","id":"answer-event","call_id":"ask-call","output":"{\"answers\":{\"q\":{\"answers\":[\"same answer phrase\"]}}}"}}"#;
        let root_records = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"root-session\"}}}}\n{call}\n{answer}\n"
        );
        let child_records = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"child-session\",\"source\":\"{{\\\"subagent\\\":{{\\\"thread_spawn\\\":{{\\\"parent_thread_id\\\":\\\"root-session\\\",\\\"agent_nickname\\\":\\\"worker\\\"}}}}}}\"}}}}\n{call}\n{answer}\n"
        );
        fs::write(&root_path, root_records).unwrap();
        fs::write(&child_path, child_records).unwrap();

        let adapter = CodexAdapter::new(Vec::new());
        let root = adapter
            .parse_reader(
                std::io::Cursor::new(fs::read(&root_path).unwrap()),
                &root_path,
            )
            .unwrap();
        let child = adapter
            .parse_reader(
                std::io::Cursor::new(fs::read(&child_path).unwrap()),
                &child_path,
            )
            .unwrap();
        let root_answer = root
            .messages
            .iter()
            .find(|message| message.content.contains("same answer phrase"))
            .expect("root answer is retained");
        let child_answer = child
            .messages
            .iter()
            .find(|message| message.content.contains("same answer phrase"))
            .expect("spawned answer is retained");
        assert_eq!(
            root_answer.provenance.input_origin,
            Some(MessageInputOrigin::InteractiveAnswer)
        );
        assert!(root_answer.provenance.content_parts.iter().any(|part| {
            part.authorship == ContentPartAuthorship::Human
                && part.origin == ContentPartOrigin::DirectInput
        }));
        assert_eq!(
            child_answer.provenance.input_origin,
            Some(MessageInputOrigin::InteractiveAnswer)
        );
        assert!(child_answer.provenance.content_parts.iter().any(|part| {
            part.authorship == ContentPartAuthorship::Agent
                && part.origin == ContentPartOrigin::DirectInput
        }));
        assert!(child_answer.provenance.content_parts.iter().all(|part| {
            part.authorship != ContentPartAuthorship::Human
                || part.origin != ContentPartOrigin::DirectInput
        }));

        let db = crate::db::Db::open(&temp.path().join("input-index.db")).unwrap();
        db.mark_schema_current().unwrap();
        db.upsert_session(&root, 0, 0).unwrap();
        db.upsert_session(&child, 0, 0).unwrap();
        let config = crate::config::Config::default();
        let service = crate::service::MessageService::new(
            &config,
            &db,
            crate::message_search::SearchSurface::Rust,
        );
        let request = |scope| {
            {
                let builder = crate::message_search::MessageSearchRequest::builder(
                    crate::message_search::MessageQuery::literal("same answer phrase").unwrap(),
                    crate::message_search::MessageTarget::content(),
                );
                if let Some(role) = scope {
                    builder.role(role)
                } else {
                    builder
                }
            }
            .build()
            .unwrap()
        };
        assert_eq!(service.search(request(None)).unwrap().hits().len(), 2);
        assert_eq!(
            service
                .search(request(Some(crate::models::Role::User)))
                .unwrap()
                .hits()
                .len(),
            1,
            "the known spawned-session answer must not satisfy restricted input search"
        );
    }

    #[test]
    fn subagent_notification_marker_is_not_user_input() {
        assert!(super::is_codex_subagent_notification(
            "<subagent_notification>\n{\"status\":\"done\"}\n</subagent_notification>"
        ));
        assert!(!super::is_codex_subagent_notification(
            "please explain <subagent_notification> in this prompt"
        ));
    }

    #[test]
    fn fork_replay_edits_are_counted_only_without_an_indexed_parent_copy() {
        let parent = "019f4a0e-d714-7cc3-8003-543a04a1a822";
        let own = "019f5ce8-afff-76a3-86f2-174307f3fa08";
        for (parent_present, fork_parent) in [(true, parent), (false, parent), (false, own)] {
            let dir = tempdir().unwrap();
            let parent_path = dir.path().join(format!("rollout-{parent}.jsonl"));
            let path = dir.path().join(format!("rollout-{own}.jsonl"));
            let mutation = |id: &str, name: &str| {
                [
                    json!({"type":"response_item","payload":{"type":"custom_tool_call","call_id":id,"name":"apply_patch","input":format!("*** Begin Patch\n*** Add File: /p/{name}\n+content\n*** End Patch")}}),
                    json!({"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":id,"output":format!("Exit code: 0\nOutput:\nSuccess. Updated the following files:\nA /p/{name}\n")}}),
                ]
            };
            let parent_records = mutation("parent-call", "parent.rs");
            if parent_present {
                let mut rows = vec![json!({"type":"session_meta","payload":{"id":parent}})];
                rows.extend(parent_records.clone());
                fs::write(
                    &parent_path,
                    rows.into_iter()
                        .map(|r| r.to_string())
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
                .unwrap();
            }
            let mut rows = vec![
                json!({"type":"session_meta","payload":{"id":own,"forked_from_id":fork_parent}}),
            ];
            rows.extend(parent_records);
            rows.push(json!({"type":"event_msg","payload":{"type":"thread_settings_applied"}}));
            rows.extend(mutation("fork-call", "fork.rs"));
            fs::write(
                &path,
                rows.into_iter()
                    .map(|r| r.to_string())
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .unwrap();
            let adapter = CodexAdapter::new(vec![dir.path().to_path_buf()]);
            let source = adapter
                .discover()
                .into_iter()
                .find(|source| source.path == path)
                .unwrap();
            let parsed = adapter.parse(&source);
            let names: Vec<_> = parsed
                .file_edits
                .iter()
                .map(|edit| edit.file_name.as_str())
                .collect();
            assert_eq!(names, vec!["fork.rs"]);
            assert_eq!(parsed.retracted.file_edits.len(), 1);
            let db = crate::db::Db::open(&dir.path().join("index.db")).unwrap();
            if parent_present {
                let parent_source = adapter
                    .discover()
                    .into_iter()
                    .find(|source| source.path == parent_path)
                    .unwrap();
                db.upsert_session(&adapter.parse(&parent_source), 1, 1)
                    .unwrap();
            }
            let mut old = parsed.clone();
            old.session.parse_version = "codex-v11".into();
            old.file_edits.extend(parsed.retracted.file_edits.clone());
            old.retracted.file_edits.clear();
            old.file_edits.sort_by_key(|edit| edit.seq);
            db.upsert_session(&old, 1, 1).unwrap();
            db.upsert_session(&parsed, 1, 1).unwrap();
            assert_eq!(db.file_edit_count().unwrap(), 2);
        }
    }

    #[test]
    fn base_instructions_are_distinct_harness_snapshots() {
        use crate::models::{MessageAuthorship, MessageFilters, MessageKind};
        let dir = tempdir().unwrap();
        let path = dir
            .path()
            .join("rollout-00000000-0000-0000-0000-000000000001.jsonl");
        let adapter = CodexAdapter::new(vec![]);
        let body = format!(" instruction sentinel\r\n{}\r\n", "x".repeat(100_000));
        let meta = json!({"type":"session_meta","payload":{"id":"00000000-0000-0000-0000-000000000001","base_instructions":{"text":body}}}).to_string() + "\n";
        let parse = |text: &str| {
            adapter
                .parse_reader(std::io::Cursor::new(text), &path)
                .unwrap()
        };
        let once = parse(&meta);
        let repeated = parse(&meta.repeat(100));
        assert_eq!(repeated.messages.len(), 1);
        assert_eq!(repeated.messages, once.messages);
        let row = &repeated.messages[0];
        assert_eq!(row.content, body);
        assert_eq!(row.kind, MessageKind::HarnessNotice);
        assert_eq!(row.provenance.authorship, MessageAuthorship::Harness);
        assert!(row
            .provenance
            .correlation_identity
            .as_ref()
            .unwrap()
            .id
            .contains("base_instructions"));
        assert!(repeated.transcript_text.is_empty());
        let db = crate::db::Db::open(&dir.path().join("index.db")).unwrap();
        db.upsert_session(&repeated, 1, 1).unwrap();
        assert!(db
            .search_messages("instruction sentinel", &MessageFilters::default())
            .unwrap()
            .is_empty());
        let notices = MessageFilters {
            kinds: Some(vec![MessageKind::HarnessNotice]),
            ..Default::default()
        };
        assert_eq!(
            db.search_messages("instruction sentinel", &notices)
                .unwrap()
                .len(),
            1
        );
        let size = |_: &crate::db::Db| {
            rusqlite::Connection::open(dir.path().join("index.db"))
                .unwrap()
                .query_row(
                    "select sum(length(cast(content as blob))) from messages",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap()
        };
        println!(
            "instruction fixture: copies=100 input_bytes={} indexed_content_bytes={}",
            meta.len() * 100,
            size(&db)
        );
        assert_eq!(size(&db), body.len() as i64);
        let single_path = dir.path().join("single.db");
        let single_db = crate::db::Db::open(&single_path).unwrap();
        single_db.upsert_session(&once, 1, 1).unwrap();
        let allocated =
            |path: &Path| -> i64 {
                rusqlite::Connection::open(path).unwrap().query_row(
                "select page_count * page_size from pragma_page_count(), pragma_page_size()",
                [], |r| r.get(0)).unwrap()
            };
        let single_bytes = allocated(&single_path);
        let repeated_bytes = allocated(&dir.path().join("index.db"));
        println!("instruction fixture SQLite allocated bytes: single={single_bytes} repeated_100={repeated_bytes} delta={}", repeated_bytes-single_bytes);
        assert_eq!(single_bytes, repeated_bytes);
        let changed = json!({"type":"session_meta","payload":{"base_instructions":{"text":"changed instruction"}}}).to_string();
        assert_eq!(parse(&format!("{meta}{changed}")).messages.len(), 2);
        // Empty older parses must not retract the retained snapshot.
        let mut old = once.clone();
        old.messages.clear();
        old.session.parse_version = "codex-v12".into();
        db.upsert_session(&old, 2, 1).unwrap();
        db.upsert_session(&once, 3, 1).unwrap();
        assert_eq!(
            db.search_messages("instruction sentinel", &notices)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn base_instructions_tail_deduplicates_beyond_overlap() {
        use std::io::Write;
        let dir = tempdir().unwrap();
        let path = dir
            .path()
            .join("rollout-00000000-0000-0000-0000-000000000001.jsonl");
        let adapter = CodexAdapter::new(vec![]);
        let meta = json!({"type":"session_meta","payload":{"id":"00000000-0000-0000-0000-000000000001","base_instructions":{"text":"stable rule"}}}).to_string() + "\n";
        let initial = format!(
            "{meta}{}\n",
            json!({"type":"ignored","padding":"x".repeat(crate::tail::OVERLAP_BYTES as usize + 1)})
        );
        fs::write(&path, &initial).unwrap();
        let db = crate::db::Db::open(&dir.path().join("tail.db")).unwrap();
        db.upsert_session(
            &adapter
                .parse_reader(std::io::Cursor::new(&initial), &path)
                .unwrap(),
            1,
            initial.len() as i64,
        )
        .unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(meta.as_bytes())
            .unwrap();
        let run = crate::tail::tail_parse_until(
            &path,
            initial.len() as i64,
            None,
            &|| false,
            |r, p, c| adapter.parse_reader_with_context(r, p, Some(c)),
        )
        .unwrap();
        let crate::tail::TailParseRun::Complete(Some(tail)) = run else {
            panic!("snapshot must stay append-only")
        };
        db.append_tail(&tail, 2, (initial.len() + meta.len()) as i64)
            .unwrap();
        let count: i64 = rusqlite::Connection::open(dir.path().join("tail.db"))
            .unwrap()
            .query_row("select count(*) from messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        // Repeated snapshots must not leave sequence holes before later ordinary events.
        let event = json!({"type":"response_item","payload":{"type":"message","id":"synthetic-event","role":"user","content":[{"type":"input_text","text":"ordinary input"}]}}).to_string() + "\n";
        let changed =
            json!({"type":"session_meta","payload":{"base_instructions":{"text":"changed rule"}}})
                .to_string()
                + "\n";
        let mut offset = (initial.len() + meta.len()) as i64;
        for suffix in [
            format!("{meta}{event}{changed}{meta}"),
            format!("{changed}{event}"),
        ] {
            fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(suffix.as_bytes())
                .unwrap();
            let run = crate::tail::tail_parse_until(&path, offset, None, &|| false, |r, p, c| {
                adapter.parse_reader_with_context(r, p, Some(c))
            })
            .unwrap();
            let crate::tail::TailParseRun::Complete(Some(tail)) = run else {
                panic!("resumed snapshots must stay incremental")
            };
            offset += suffix.len() as i64;
            db.append_tail(&tail, 3, offset).unwrap();
        }
        let full = adapter
            .parse_reader(
                std::io::BufReader::new(fs::File::open(&path).unwrap()),
                &path,
            )
            .unwrap();
        let fresh = crate::db::Db::open(&dir.path().join("fresh.db")).unwrap();
        fresh.upsert_session(&full, 3, offset).unwrap();
        let rows = |name: &str| {
            let conn = rusqlite::Connection::open(dir.path().join(name)).unwrap();
            let mut stmt = conn
                .prepare(
                    "select json_array(session_id, provider, seq, role, ts, tool_name, kind,
                        tool_call_id, is_compaction, content, authorship, correlation_authority,
                        correlation_scope, correlation_id, input_delivery_state, input_origin,
                        record_relation) from messages order by seq",
                )
                .unwrap();
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(rows("tail.db"), rows("fresh.db"));
        assert_eq!(
            rows("tail.db").len(),
            4,
            "two distinct snapshots and two ordinary events, even with the same native id"
        );
        db.upsert_session(&full, 4, offset).unwrap();
        assert_eq!(
            rows("tail.db"),
            rows("fresh.db"),
            "full reconciliation must not retain extra archive rows"
        );
    }

    #[test]
    fn lifecycle_records_in_overlap_are_not_inserted_twice() {
        use std::io::Write;
        let dir = tempdir().unwrap();
        let path = dir
            .path()
            .join("rollout-00000000-0000-0000-0000-000000000003.jsonl");
        let adapter = CodexAdapter::new(vec![]);
        let initial =
            json!({"type":"session_meta","payload":{"id":"00000000-0000-0000-0000-000000000003"}})
                .to_string()
                + "\n";
        fs::write(&path, &initial).unwrap();
        let db = crate::db::Db::open(&dir.path().join("overlap.db")).unwrap();
        let before = adapter
            .parse_reader(std::io::Cursor::new(&initial), &path)
            .unwrap();
        db.upsert_session(&before, 1, initial.len() as i64).unwrap();
        let lifecycle = [
            json!({"type":"compacted","payload":{"message":"summary"}}),
            json!({"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted"}}),
        ]
        .into_iter()
        .map(|record| record.to_string() + "\n")
        .collect::<String>();
        let event = json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"continue"}]}}).to_string() + "\n";
        let mut offset = initial.len() as i64;
        // The second refresh sees both lifecycle records only in overlap; the third
        // adds identical new occurrences, which must not be deduplicated as snapshots.
        for (suffix, expected_new, expected_notices) in [
            (lifecycle.clone(), 2, 2),
            (event.clone(), 1, 2),
            (lifecycle.clone(), 2, 4),
            (event, 1, 4),
        ] {
            fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(suffix.as_bytes())
                .unwrap();
            assert!(offset < crate::tail::OVERLAP_BYTES);
            let run = crate::tail::tail_parse_until(&path, offset, None, &|| false, |r, p, c| {
                adapter.parse_reader_with_context(r, p, Some(c))
            })
            .unwrap();
            let crate::tail::TailParseRun::Complete(Some(tail)) = run else {
                panic!("lifecycle records must stay incremental")
            };
            assert_eq!(tail.new_messages.len(), expected_new);
            offset += suffix.len() as i64;
            db.append_tail(&tail, 2, offset).unwrap();
            let full = adapter
                .parse_reader(
                    std::io::BufReader::new(fs::File::open(&path).unwrap()),
                    &path,
                )
                .unwrap();
            let conn = rusqlite::Connection::open(dir.path().join("overlap.db")).unwrap();
            let mut stmt = conn
                .prepare("select seq, content from messages order by seq")
                .unwrap();
            let stored = stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(
                stored,
                full.messages
                    .iter()
                    .map(|m| (m.seq, m.content.clone()))
                    .collect::<Vec<_>>()
            );
            let notices: i64 = conn
                .query_row(
                    "select count(*) from messages where kind in ('compaction', 'harness_notice')",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(notices, expected_notices);
        }
    }

    #[test]
    fn lifecycle_records_are_searchable_without_rewriting_history() {
        use crate::models::{MessageAuthorship, MessageKind};
        use std::io::Write;
        let dir = tempdir().unwrap();
        let path = dir
            .path()
            .join("rollout-00000000-0000-0000-0000-000000000002.jsonl");
        let adapter = CodexAdapter::new(vec![]);
        let initial = format!(
            "{}\n{}\n",
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"original input"}]}}),
            json!({"type":"ignored","padding":"x".repeat(300_000)})
        );
        fs::write(&path, &initial).unwrap();
        let before = adapter
            .parse_reader(std::io::Cursor::new(&initial), &path)
            .unwrap();
        let suffix = [
            json!({"timestamp":"2026-01-01T00:01:00Z","type":"compacted","payload":{"message":"compaction sentinel","replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":"must not duplicate history"}]}]}}),
            json!({"timestamp":"2026-01-01T00:02:00Z","type":"event_msg","payload":{"type":"turn_aborted","turn_id":"synthetic-turn","reason":"interrupted"}})
        ].into_iter().map(|r| r.to_string() + "\n").collect::<String>();
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(suffix.as_bytes())
            .unwrap();
        let full = adapter
            .parse_reader(std::io::Cursor::new(format!("{initial}{suffix}")), &path)
            .unwrap();
        assert!(full.messages.starts_with(&before.messages));
        assert_eq!(full.messages.len(), 3);
        assert_eq!(full.messages[1].kind, MessageKind::Compaction);
        assert!(full.messages[1].is_compaction);
        assert_eq!(full.messages[2].kind, MessageKind::HarnessNotice);
        assert_eq!(
            full.messages[2].provenance.authorship,
            MessageAuthorship::Harness
        );
        assert!(full.messages[2].content.contains("interrupted"));
        assert!(full.messages[2].content.contains("turn_aborted"));
        assert_eq!(full.transcript_text, before.transcript_text);
        let run = crate::tail::tail_parse_until(
            &path,
            initial.len() as i64,
            None,
            &|| false,
            |r, p, c| adapter.parse_reader_with_context(r, p, Some(c)),
        )
        .unwrap();
        let crate::tail::TailParseRun::Complete(Some(tail)) = run else {
            panic!("lifecycle append must not force full parse")
        };
        assert_eq!(tail.new_messages.len(), 2);
        assert!(!tail
            .session
            .raw_metadata_json
            .as_deref()
            .unwrap()
            .contains("must not duplicate history"));
        let db = crate::db::Db::open(&dir.path().join("lifecycle.db")).unwrap();
        let mut old = before.clone();
        old.session.parse_version = "codex-v12".into();
        db.upsert_session(&old, 1, initial.len() as i64).unwrap();
        db.upsert_session(&full, 2, (initial.len() + suffix.len()) as i64)
            .unwrap();
        db.upsert_session(&full, 3, (initial.len() + suffix.len()) as i64)
            .unwrap();
        let conn = rusqlite::Connection::open(dir.path().join("lifecycle.db")).unwrap();
        assert_eq!(
            conn.query_row("select count(*) from messages", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            3
        );
    }

    #[test]
    fn shell_apply_patch_requires_a_single_command_and_correlated_success() {
        let dir = tempdir().unwrap();
        let path = dir
            .path()
            .join("rollout-00000000-0000-0000-0000-000000000003.jsonl");
        let patch = "*** Begin Patch\n*** Add File: /synthetic/new.txt\n+synthetic text\n*** Update File: /synthetic/old.txt\n@@\n-before\n+after\n*** End Patch";
        let command = format!("apply_patch <<'PATCH'\n{patch}\nPATCH");
        let success = "Chunk ID: synthetic\nWall time: 0.1 seconds\nProcess exited with code 0\nFinal output:\nSuccess. Updated the following files:\nA /synthetic/new.txt\nM /synthetic/old.txt";
        let call = |id: &str, name: &str, args: serde_json::Value| json!({"type":"response_item","payload":{"type":"function_call","call_id":id,"name":name,"arguments":args.to_string()}});
        let output = |id: &str, text: &str| json!({"type":"response_item","payload":{"type":"function_call_output","call_id":id,"output":text}});
        for (name, args) in [
            ("exec_command", json!({"cmd":command})),
            ("shell", json!({"command":["bash","-lc",command]})),
            ("shell_command", json!({"command":command})),
        ] {
            let rows = [
                call("good", name, args.clone()),
                output("unrelated", success),
                output("good", success),
                call("failed", name, args.clone()),
                output(
                    "failed",
                    "Process exited with code 1\nFinal output:\npatch failed",
                ),
                call("pending", name, args),
                call(
                    "quoted",
                    name,
                    json!({"cmd":format!("cat <<'PATCH'\n{patch}\nPATCH")}),
                ),
                output("quoted", success),
            ]
            .into_iter()
            .map(|r| r.to_string() + "\n")
            .collect::<String>();
            let parsed = CodexAdapter::new(vec![])
                .parse_reader(std::io::Cursor::new(rows), &path)
                .unwrap();
            assert_eq!(parsed.file_edits.len(), 2, "{name}");
            assert_eq!(
                parsed.file_edits[0].new_content.as_deref(),
                Some("synthetic text\n")
            );
            assert_eq!(parsed.file_edits[1].file_path, "/synthetic/old.txt");
        }
        assert!(super::apply_patch_text(&json!({"name":"exec","input":format!("const p = await tools.apply_patch({});", json!(patch))})).is_none());
        assert!(super::apply_patch_text(&json!({"name":"exec_command","arguments":json!({"cmd":format!("{command}\necho done")}).to_string()})).is_none());
        let early_end = command.replace("+synthetic text", "+synthetic text\nPATCH\necho injected");
        assert!(super::apply_patch_text(
            &json!({"name":"exec_command","arguments":json!({"cmd":early_end}).to_string()})
        )
        .is_none());
    }

    #[test]
    fn added_file_bytes_match_patch_lines() {
        for (body, expected) in [
            ("", ""),
            ("+hello\n", "hello\n"),
            ("+\n", "\n"),
            ("+hello\n+\n", "hello\n\n"),
            ("+hello\r\n+world\r\n", "hello\nworld\n"),
        ] {
            let patch =
                format!("*** Begin Patch\n*** Add File: /synthetic/new.txt\n{body}*** End Patch");
            assert_eq!(
                super::parse_apply_patch(&patch),
                vec![("/synthetic/new.txt".into(), Some(expected.into()))]
            );
        }
    }

    #[test]
    fn added_file_upgrade_preserves_colliding_occurrences_and_archive() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("rollout-synthetic.jsonl");
        let mut rows =
            vec![json!({"type":"session_meta","payload":{"id":"synthetic-collision","cwd":"/p"}})];
        for (i, patch) in [
            "*** Add File: /p/f.txt\n+x",
            "*** Delete File: /p/f.txt",
            "*** Add File: /p/f.txt\n+x\n+",
        ]
        .iter()
        .enumerate()
        {
            rows.push(json!({"type":"response_item","payload":{"type":"custom_tool_call","call_id":format!("call_{i}"),"name":"apply_patch","input":format!("*** Begin Patch\n{patch}\n*** End Patch")}}));
            rows.push(json!({"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":format!("call_{i}"),"output":"Exit code: 0\nWall time: 0.1 seconds\nOutput:\nSuccess. Updated the following files:\nA /p/f.txt\n"}}));
        }
        let source = rows
            .into_iter()
            .map(|row| row.to_string() + "\n")
            .collect::<String>();
        let parsed = CodexAdapter::new(vec![])
            .parse_reader(std::io::Cursor::new(source), &path)
            .unwrap();
        assert_eq!(parsed.file_edits.len(), 3);
        let mut old = parsed.clone();
        old.session.parse_version = "codex-v14".into();
        for edit in &mut old.file_edits {
            if let Some(content) = &mut edit.new_content {
                content.pop();
            }
        }
        let mut archive = old.file_edits[0].clone();
        archive.file_path = "/p/archive.txt".into();
        archive.file_name = "archive.txt".into();
        archive.seq = 3;
        old.file_edits.push(archive);
        let db = crate::db::Db::open(&temp.path().join("upgrade.db")).unwrap();
        let fresh = crate::db::Db::open(&temp.path().join("fresh.db")).unwrap();
        db.upsert_session(&old, 1, 1).unwrap();
        fresh.upsert_session(&parsed, 1, 1).unwrap();
        let archive_before = db
            .file_edits_for_session_id("archive.txt", &parsed.session.id)
            .unwrap();
        let connection = rusqlite::Connection::open(temp.path().join("upgrade.db")).unwrap();
        let row_ids = || {
            connection
                .prepare("select id from file_edits order by id")
                .unwrap()
                .query_map([], |row| row.get::<_, i64>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        let ids_before = row_ids();
        let expected = vec![Some("x\n".to_string()), None, Some("x\n\n".to_string())];
        for refresh in [2, 3] {
            db.upsert_session(&parsed, refresh, 1).unwrap();
            let versions = db
                .file_edits_for_session_id("f.txt", &parsed.session.id)
                .unwrap();
            assert_eq!(versions.len(), 3);
            assert_eq!(
                versions
                    .iter()
                    .map(|row| row.2.new_content.clone())
                    .collect::<Vec<_>>(),
                expected
            );
            let clean = fresh
                .file_edits_for_session_id("f.txt", &parsed.session.id)
                .unwrap();
            assert_eq!(
                versions.iter().map(|row| &row.2).collect::<Vec<_>>(),
                clean.iter().map(|row| &row.2).collect::<Vec<_>>()
            );
            let retained = db
                .file_edits_for_session_id("archive.txt", &parsed.session.id)
                .unwrap();
            assert_eq!(retained.len(), 1);
            assert_eq!(retained, archive_before);
            assert_eq!(row_ids(), ids_before);
        }
    }

    #[test]
    fn partial_file_refresh_pairs_repeated_writes_in_order() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("rollout-synthetic.jsonl");
        let source = json!({"type":"session_meta","payload":{"id":"synthetic-repeat","cwd":"/p"}})
            .to_string()
            + "\n";
        let mut parsed = CodexAdapter::new(vec![])
            .parse_reader(std::io::Cursor::new(source), &path)
            .unwrap();
        for (seq, content) in ["first", "middle", "first"].iter().enumerate() {
            parsed.file_edits.push(crate::models::FileEdit {
                seq: seq as i64,
                ts: None,
                tool: "write_file".into(),
                file_path: "/p/f.txt".into(),
                file_name: "f.txt".into(),
                new_content: Some((*content).into()),
                edits: vec![],
            });
        }
        let db = crate::db::Db::open(&temp.path().join("partial.db")).unwrap();
        db.upsert_session(&parsed, 1, 1).unwrap();
        parsed.file_edits.remove(0);
        let mut inserted = parsed.file_edits[0].clone();
        inserted.new_content = Some("inserted".into());
        parsed.file_edits.insert(1, inserted);
        for (seq, edit) in parsed.file_edits.iter_mut().enumerate() {
            edit.seq = seq as i64;
        }
        for refresh in [2, 3] {
            db.upsert_session(&parsed, refresh, 1).unwrap();
            let stored = db
                .file_edits_for_session_id("f.txt", &parsed.session.id)
                .unwrap();
            assert_eq!(
                stored
                    .iter()
                    .map(|row| row.2.new_content.as_deref())
                    .collect::<Vec<_>>(),
                vec![
                    Some("first"),
                    Some("middle"),
                    Some("inserted"),
                    Some("first")
                ]
            );
        }
    }

    fn assert_reordered_file_refresh_matches_each_occurrence_once(parser_changed: bool) {
        for with_archive in [false, true] {
            let temp = tempdir().unwrap();
            let path = temp.path().join("rollout-synthetic.jsonl");
            let source =
                json!({"type":"session_meta","payload":{"id":"synthetic-reorder","cwd":"/p"}})
                    .to_string()
                    + "\n";
            let mut stored = CodexAdapter::new(vec![])
                .parse_reader(std::io::Cursor::new(source), &path)
                .unwrap();
            for (seq, content) in ["A", "B"].iter().enumerate() {
                stored.file_edits.push(crate::models::FileEdit {
                    seq: seq as i64,
                    ts: None,
                    tool: "write_file".into(),
                    file_path: "/p/f.txt".into(),
                    file_name: "f.txt".into(),
                    new_content: Some((*content).into()),
                    edits: vec![],
                });
            }
            let mut parsed = stored.clone();
            parsed.file_edits.swap(0, 1);
            for (seq, edit) in parsed.file_edits.iter_mut().enumerate() {
                edit.seq = seq as i64;
            }
            if parser_changed {
                stored.session.parse_version = "codex-v14".into();
            }
            if with_archive {
                let mut archive = stored.file_edits[0].clone();
                archive.file_path = "/p/archive.txt".into();
                archive.file_name = "archive.txt".into();
                archive.seq = 2;
                stored.file_edits.push(archive);
            }
            let db = crate::db::Db::open(&temp.path().join("reordered.db")).unwrap();
            let fresh = crate::db::Db::open(&temp.path().join("fresh.db")).unwrap();
            db.upsert_session(&stored, 1, 1).unwrap();
            fresh.upsert_session(&parsed, 1, 1).unwrap();
            let fresh_contents = fresh
                .file_edits_for_session_id("f.txt", &parsed.session.id)
                .unwrap()
                .into_iter()
                .map(|row| row.2.new_content.unwrap())
                .collect::<Vec<_>>();
            assert_eq!(fresh_contents, ["B", "A"]);
            let archive_before = db
                .file_edits_for_session_id("archive.txt", &parsed.session.id)
                .unwrap();
            for refresh in [2, 3] {
                db.upsert_session(&parsed, refresh, 1).unwrap();
                let contents = db
                    .file_edits_for_session_id("f.txt", &parsed.session.id)
                    .unwrap()
                    .into_iter()
                    .map(|row| row.2.new_content.unwrap())
                    .collect::<Vec<_>>();
                println!("parser_changed={parser_changed} archive={with_archive} refresh={refresh} stored={contents:?} fresh={fresh_contents:?}");
                // Complete parses follow source order; archive reconciliation keeps stored order.
                let expected = if with_archive {
                    vec!["A", "B"]
                } else {
                    vec!["B", "A"]
                };
                assert_eq!(contents, expected);
                assert_eq!(
                    db.file_edits_for_session_id("archive.txt", &parsed.session.id)
                        .unwrap(),
                    archive_before
                );
            }
        }
    }

    #[test]
    fn reordered_file_refresh_matches_each_occurrence_once_same_parser() {
        assert_reordered_file_refresh_matches_each_occurrence_once(false);
    }

    #[test]
    fn reordered_file_refresh_matches_each_occurrence_once_changed_parser() {
        assert_reordered_file_refresh_matches_each_occurrence_once(true);
    }

    #[test]
    fn extracts_file_edits_from_apply_patch() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let session_id = "019efd97-d602-7922-89dd-467272106505";
        let file = root.join(format!("rollout-2026-06-25T03-04-06-{session_id}.jsonl"));
        // Real codex shape: apply_patch is a custom_tool_call whose `input` is the patch.
        fs::write(
            &file,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"019efd97-d602-7922-89dd-467272106505","timestamp":"2026-06-25T07:00:00.000Z","cwd":"/p"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-25T07:00:01.000Z","type":"response_item","payload":{"type":"custom_tool_call","status":"completed","call_id":"call_1","name":"apply_patch","input":"*** Begin Patch\n*** Update File: /p/a.rs\n@@\n-old\n+new\n*** Add File: /p/b.rs\n+line1\n+line2\n*** Delete File: /p/c.rs\n*** End Patch"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-25T07:00:02.000Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_1","output":"Exit code: 0\nWall time: 0.1 seconds\nOutput:\nSuccess. Updated the following files:\nM /p/a.rs\nA /p/b.rs\nD /p/c.rs\n"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-25T07:00:03.000Z","type":"response_item","payload":{"type":"custom_tool_call","status":"completed","call_id":"call_failed","name":"apply_patch","input":"*** Begin Patch\n*** Add File: /p/failed.rs\n+wrong\n*** End Patch"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-25T07:00:04.000Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_failed","output":"aborted by user after 53.3s"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-25T07:00:05.000Z","type":"response_item","payload":{"type":"custom_tool_call","status":"completed","call_id":"call_unresolved","name":"apply_patch","input":"*** Begin Patch\n*** Add File: /p/unresolved.rs\n+unknown\n*** End Patch"}}"#,
                "\n",
            ),
        )
        .unwrap();

        let adapter = CodexAdapter::new(vec![root]);
        let parsed = adapter.parse(&adapter.discover()[0]);

        let names: Vec<&str> = parsed
            .file_edits
            .iter()
            .map(|e| e.file_name.as_str())
            .collect();
        assert!(names.contains(&"a.rs"), "Update File recorded: {names:?}");
        assert!(names.contains(&"b.rs"), "Add File recorded: {names:?}");
        assert!(names.contains(&"c.rs"), "Delete File recorded: {names:?}");
        assert!(
            !names.contains(&"failed.rs"),
            "failed patch excluded: {names:?}"
        );
        assert!(
            !names.contains(&"unresolved.rs"),
            "unresolved patch excluded: {names:?}"
        );
        // Added file carries its new content (replayable); update/delete are path-only.
        let added = parsed
            .file_edits
            .iter()
            .find(|e| e.file_name == "b.rs")
            .unwrap();
        assert_eq!(added.tool, "apply_patch");
        assert_eq!(added.new_content.as_deref(), Some("line1\nline2\n"));
        let db = crate::db::Db::open(&temp.path().join("upgrade.db")).unwrap();
        let mut old = parsed.clone();
        old.session.parse_version = "codex-v14".into();
        old.retracted.file_edits.clear();
        for edit in &mut old.file_edits {
            if let Some(content) = &mut edit.new_content {
                content.pop();
            }
        }
        let mut archived = old
            .file_edits
            .iter()
            .find(|edit| edit.file_name == "b.rs")
            .unwrap()
            .clone();
        archived.file_path = "/p/archived.rs".into();
        archived.file_name = "archived.rs".into();
        old.file_edits.push(archived);
        db.upsert_session(&old, 1, 1).unwrap();
        db.upsert_session(&parsed, 2, 1).unwrap();
        db.upsert_session(&parsed, 3, 1).unwrap();
        let retained = db
            .file_edits_for_session_id("archived.rs", &parsed.session.id)
            .unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].2.new_content.as_deref(), Some("line1\nline2"));
        let stored = db
            .file_edits_for_session_id("b.rs", &parsed.session.id)
            .unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].2.new_content.as_deref(), Some("line1\nline2\n"));
        let updated = parsed
            .file_edits
            .iter()
            .find(|e| e.file_name == "a.rs")
            .unwrap();
        assert!(updated.new_content.is_none(), "Update File is path-only");
    }

    #[test]
    fn recognizes_only_explicit_successful_apply_patch_outputs() {
        let rendered = json!(
            "Exit code: 0\nWall time: 0.1 seconds\nOutput:\nSuccess. Updated the following files:\nM /p/a.rs\n"
        );
        let structured = json!({
            "output": "Success. Updated the following files:\nM /p/a.rs\n",
            "metadata": {"exit_code": 0, "duration_seconds": 0.1}
        });
        let serialized = json!(structured.to_string());
        assert!(codex_apply_patch_succeeded(Some(&rendered)));
        assert!(codex_apply_patch_succeeded(Some(&structured)));
        assert!(codex_apply_patch_succeeded(Some(&serialized)));
        for output in [
            json!("aborted by user after 53.3s"),
            json!("Exit code: 1\nOutput:\nInvalid Context"),
            json!({"output": "Success. Updated the following files:\nM /p/a.rs", "metadata": {"exit_code": 1}}),
            json!({"output": "unknown"}),
        ] {
            assert!(!codex_apply_patch_succeeded(Some(&output)), "{output}");
        }
        assert!(!codex_apply_patch_succeeded(None));
    }

    #[test]
    fn indexes_codex_event_metadata_and_thread_columns() {
        let temp = tempdir().unwrap();
        let codex_home = temp.path().join("codex-home");
        fs::create_dir_all(&codex_home).unwrap();
        let root = codex_home.join("sessions");
        fs::create_dir_all(&root).unwrap();
        let session_id = "019efd97-d602-7922-89dd-467272106505";
        let state = codex_home.join("state_5.sqlite");
        let conn = rusqlite::Connection::open(&state).unwrap();
        conn.execute_batch(
            "create table threads (
                id text primary key, title text, cwd text, created_at integer, updated_at integer,
                rollout_path text, first_user_message text, model text, reasoning_effort text,
                git_branch text, created_at_ms integer, updated_at_ms integer, tokens_used integer
            );",
        )
        .unwrap();
        conn.execute(
            "insert into threads
             (id,title,cwd,created_at,updated_at,rollout_path,first_user_message,model,
              reasoning_effort,git_branch,created_at_ms,updated_at_ms,tokens_used)
             values (?1,'Thread title','/repo',1,2,'/rollout','first user','gpt-test',
                     'high','feat/x',1000,2000,42)",
            [session_id],
        )
        .unwrap();

        let file = root.join(format!("rollout-2026-06-25T03-04-06-{session_id}.jsonl"));
        fs::write(
            &file,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"019efd97-d602-7922-89dd-467272106505","timestamp":"2026-06-25T07:00:00.000Z","cwd":"/repo"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-25T07:00:01.000Z","type":"event_msg","payload":{"type":"thread_goal_updated","goal":{"objective":"ship literal search","status":"in_progress"}}}"#,
                "\n",
                r#"{"timestamp":"2026-06-25T07:00:02.000Z","type":"event_msg","payload":{"type":"web_search_end","query":"aise url search","action":{"queries":["aise url search"]}}}"#,
                "\n",
                r#"{"timestamp":"2026-06-25T07:00:03.000Z","type":"event_msg","payload":{"type":"mcp_tool_call_end","invocation":{"server":"aise","tool":"search_messages","arguments":{"query":"/goal"}},"duration":12,"result":"very long result"}}"#,
                "\n",
            ),
        )
        .unwrap();

        let adapter = CodexAdapter::new(vec![root]);
        let parsed = adapter.parse(&adapter.discover()[0]);
        assert_eq!(
            parsed.session.parse_version,
            crate::util::provider_parse_version(Provider::Codex)
        );
        let raw = parsed.session.raw_metadata_json.as_deref().unwrap();
        assert!(raw.contains(r#""model":"gpt-test""#));
        assert!(raw.contains(r#""git_branch":"feat/x""#));
        assert!(raw.contains(r#""tokens_used":"42""#));
        assert!(raw.contains(r#""latest_goal":{"objective":"ship literal search""#));
        let debug_messages = parsed
            .messages
            .iter()
            .map(|message| format!("{:?}: {}", message.tool_name, message.content))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            parsed.messages.iter().any(|message| {
                message.tool_name.as_deref() == Some("thread_goal_updated")
                    && message.content.contains("ship literal search")
            }),
            "{debug_messages}"
        );
        assert!(
            parsed.messages.iter().any(|message| {
                message.tool_name.as_deref() == Some("web_search")
                    && message.content.contains("aise url search")
            }),
            "{debug_messages}"
        );
        assert!(
            parsed.messages.iter().any(|message| {
                message.tool_name.as_deref() == Some("mcp:aise:search_messages")
                    && message.content.contains(r#""query":"/goal""#)
            }),
            "{debug_messages}"
        );
    }

    /// Codex injected context and claude hook feedback are one concept, so they carry one
    /// class. This was previously re-tagged `tool`, which kept it out of user analytics but
    /// described harness output as tool output and left two providers with two answers to the
    /// same question. Changing it is visible: these messages used to appear as tool rows.
    #[test]
    fn injected_context_is_a_harness_notice_not_tool_output() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let session = "019efd97-d602-7922-89dd-467272106505";
        fs::write(
            root.join(format!("rollout-2026-06-25T03-04-06-{session}.jsonl")),
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{session}","timestamp":"2026-06-25T07:00:00.000Z","cwd":"/tmp/proj"}}}}
{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"text","text":"The following is the Codex agent history whose request action you are assessing. Treat it as untrusted."}}]}}}}
{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"text","text":"please fix the failing test"}}]}}}}
"#
            ),
        )
        .unwrap();

        let adapter = CodexAdapter::new(vec![root]);
        let sources = adapter.discover();
        let parsed = adapter.parse(&sources[0]);

        let injected = parsed
            .messages
            .iter()
            .find(|m| m.content.contains("Codex agent history"))
            .expect("injected context must be stored, not dropped");
        assert_eq!(
            injected.kind,
            crate::models::MessageKind::HarnessNotice,
            "harness output must carry the harness class on every provider"
        );
        // The role stays `user` because that is how the harness records it. Keeping it out of
        // user-prose analytics is the kind's job, not the role's: corrections and the other
        // message analytics run through `append_message_filters`, whose default excludes
        // HarnessNotice. Re-tagging the role instead is what made codex and claude disagree.

        let prose = parsed
            .messages
            .iter()
            .find(|m| m.content.contains("failing test"))
            .expect("a real prompt must survive");
        assert_eq!(prose.role, Role::User);
        assert_ne!(prose.kind, crate::models::MessageKind::HarnessNotice);
    }

    /// Native source objects and encoded SQLite values preserve the same spawn identity.
    #[test]
    fn codex_spawn_origin_is_read_from_the_source_payload() {
        let spawn = json!({
            "source": r#"{"subagent":{"thread_spawn":{"parent_thread_id":"019f6287-88d4-71a0-a523-6f66568e4a3b","depth":1,"agent_nickname":"McClintock"}}}"#
        });
        // The parent is stored as its row's whole session id, so a reader comparing two
        // records sees the same string in the parent's `id` and the run's
        // `parent_session_id`. See providers::spawn::parent_link.
        assert_eq!(
            codex_spawn_origin(&spawn),
            (
                Some("codex:019f6287-88d4-71a0-a523-6f66568e4a3b".to_string()),
                Some("McClintock".to_string())
            )
        );
        let native_object = json!({
            "source": {
                "subagent": {
                    "thread_spawn": {
                        "parent_thread_id": "019f6287-88d4-71a0-a523-6f66568e4a3b",
                        "agent_nickname": "McClintock"
                    }
                }
            }
        });
        assert_eq!(
            codex_spawn_origin(&native_object),
            codex_spawn_origin(&spawn)
        );

        for label in ["review", "compact", "memory_consolidation"] {
            let source = json!({"subagent":label});
            for value in [
                source.clone(),
                serde_json::Value::String(source.to_string()),
            ] {
                assert_eq!(
                    codex_spawn_origin(&json!({"source":value})),
                    (None, Some(label.into()))
                );
            }
        }

        // A spawn with no parent recorded still marks the session as agent-started.
        let other = json!({ "source": r#"{"subagent":{"other":"guardian"}}"# });
        assert_eq!(
            codex_spawn_origin(&other),
            (None, Some("guardian".to_string()))
        );

        // An ordinary user-started session has no origin, and neither malformed JSON nor a
        // missing key may panic: both are ordinary on real data.
        assert_eq!(
            codex_spawn_origin(&json!({ "source": "cli" })),
            (None, None)
        );
        assert_eq!(codex_spawn_origin(&json!({})), (None, None));
        assert_eq!(
            codex_spawn_origin(&json!({ "source": "{not json" })),
            (None, None)
        );
    }

    #[test]
    fn detects_injected_context_not_real_prompts() {
        assert!(is_codex_injected_context(
            "The following is the Codex agent history whose request action you are assessing."
        ));
        assert!(is_codex_injected_context(
            "# AGENTS.md instructions for /Users/x/proj"
        ));
        assert!(is_codex_injected_context(
            "# AGENTS.md instructions <INSTRUCTIONS> <!-- managed-hook -->"
        ));
        // Approval/goal-mode injections: codex wraps the active-thread goal and hook
        // output in these markers and submits them as role:user. They are not prompts.
        assert!(is_codex_injected_context(
            "<goal_context>\nContinue working toward the active thread goal."
        ));
        assert!(is_codex_injected_context(
            "<codex_internal_context source=\"goal\">\nContinue working toward the active thread goal."
        ));
        assert!(is_codex_injected_context(
            "<hook_prompt hook_run_id=\"stop:5:/home/example/.codex/hooks.json\">usage: managed hook"
        ));
        // Per-turn environment context (date/timezone/cwd/sandbox) resubmitted as role:user.
        assert!(is_codex_injected_context(
            "<environment_context>\n<current_date>2026-05-22</current_date>\n</environment_context>"
        ));
        // Codex records an interrupted turn as role:user even though this text is generated by the
        // harness rather than typed by the user.
        assert!(is_codex_injected_context(
            "<turn_aborted>\nThe user interrupted the previous turn on purpose.\n</turn_aborted>"
        ));
        // Leading whitespace must not defeat the marker match.
        assert!(is_codex_injected_context("\n  <goal_context>\nwork"));
        assert!(!is_codex_injected_context("please fix the failing test"));
        assert!(!is_codex_injected_context(
            "revert that change, it broke the build"
        ));
        // A real prompt that merely mentions the word goal must not be filtered.
        assert!(!is_codex_injected_context(
            "the goal context here is wrong, redo it"
        ));
        assert!(!is_codex_injected_context(
            "what does <turn_aborted> mean in a Codex transcript"
        ));
    }

    /// Differential guard for the streaming-parse refactor (task #241): identical output
    /// and `line_count` between the streaming `BufReader` path and the prior whole-file
    /// `fs::read_to_string` path. Fixture exercises blank/malformed lines and a final line
    /// without a trailing newline (line_count must be 5).
    #[test]
    fn streaming_parse_output_is_stable() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let session_id = "019efd97-d602-7922-89dd-467272106505";
        let file = root.join(format!("rollout-2026-06-25T03-04-06-{session_id}.jsonl"));
        // 4 physical lines, final line has no trailing newline:
        //   1 session_meta  2 user message  3 malformed (skipped)  4 assistant (no \n)
        let content = concat!(
            r#"{"type":"session_meta","payload":{"id":"019efd97-d602-7922-89dd-467272106505","timestamp":"2026-06-25T07:00:00.000Z","cwd":"/tmp/proj"}}"#,
            "\n",
            r#"{"timestamp":"2026-06-25T07:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"text","text":"do the thing"}]}}"#,
            "\n",
            "{bad json line\n",
            r#"{"timestamp":"2026-06-25T07:00:02.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"done"}]}}"#,
        );
        fs::write(&file, content).unwrap();

        let adapter = CodexAdapter::new(vec![root]);
        let parsed = adapter.parse(&adapter.discover()[0]);

        assert!(
            parsed
                .session
                .raw_metadata_json
                .as_deref()
                .unwrap()
                .contains("\"line_count\":4"),
            "line_count must be 4 (4 physical lines), got: {:?}",
            parsed.session.raw_metadata_json
        );
        assert_eq!(parsed.session.provider, Provider::Codex);
        assert_eq!(parsed.session.cwd.as_deref(), Some("/tmp/proj"));
        let contents: Vec<&str> = parsed.messages.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents, vec!["do the thing", "done"]);
        let roles: Vec<&str> = parsed.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "assistant"]);
        assert_eq!(
            parsed.messages[0].provenance.authorship,
            crate::models::MessageAuthorship::Human
        );
        assert_eq!(
            parsed.messages[1].provenance.authorship,
            crate::models::MessageAuthorship::Agent
        );
        assert!(parsed.transcript_text.contains("do the thing"));
        assert!(parsed.transcript_text.contains("done"));
        assert_eq!(
            parsed.session.parse_warning.as_deref(),
            Some("skipped 1 malformed JSONL record")
        );
    }

    /// Non-UTF-8 bytes must never panic or abort the parse — they are decoded lossily (U+FFFD).
    /// This input is not valid JSON even after lossy decoding, so it yields no messages, but
    /// parsing completes WITHOUT error (lossy recovery is not treated as a parse failure).
    #[test]
    fn non_utf8_garbage_is_diagnosed_without_aborting_the_parse() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let session_id = "019efd97-d602-7922-89dd-467272106505";
        let file = root.join(format!("rollout-2026-06-25T03-04-06-{session_id}.jsonl"));
        fs::write(&file, [b'{', 0xFF, 0xFE, b'}', b'\n']).unwrap();

        let adapter = CodexAdapter::new(vec![root]);
        let parsed = adapter.parse(&adapter.discover()[0]);
        assert!(parsed.messages.is_empty());
        assert_eq!(
            parsed.session.parse_warning.as_deref(),
            Some("skipped 1 malformed JSONL record")
        );
        assert_eq!(parsed.session.message_count, Some(0));
    }
}
