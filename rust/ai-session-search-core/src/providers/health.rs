// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

use serde_json::{json, Value};

use crate::models::{ParsedSession, Provider};

pub(crate) const UNRECOGNIZED_RECORD_TYPES_KEY: &str = "unrecognized_record_types";

/// Names only, never payload text. Retains 16 names of at most 64 characters plus one
/// overflow bucket, matching the original Claude inventory bound.
pub(crate) struct UnrecognizedRecordTypes {
    pub(crate) counts: std::collections::BTreeMap<String, usize>,
    count_record: bool,
}

impl Default for UnrecognizedRecordTypes {
    fn default() -> Self {
        Self {
            counts: Default::default(),
            count_record: true,
        }
    }
}

impl UnrecognizedRecordTypes {
    const MAX_TYPES: usize = 16;
    const MAX_NAME_CHARS: usize = 64;
    const OVERFLOW: &'static str = "(other types)";

    pub(crate) fn for_context(context: Option<crate::tail::TailContext<'_>>) -> Self {
        let counts = context
            .filter(|context| context.new_bytes_start != usize::MAX)
            .and_then(|context| context.health_seed)
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .and_then(|metadata| metadata.get(UNRECOGNIZED_RECORD_TYPES_KEY).cloned())
            .and_then(|inventory| serde_json::from_value(inventory).ok())
            .unwrap_or_default();
        Self {
            counts,
            count_record: true,
        }
    }

    /// All diagnostics produced by one physical record share its byte-boundary decision.
    pub(crate) fn record(&mut self, context: Option<crate::tail::TailContext<'_>>, offset: usize) {
        self.count_record = context.is_none_or(|context| offset >= context.new_bytes_start);
    }

    pub(crate) fn count(&mut self, record_type: Option<&str>) {
        self.count_in("", record_type);
    }

    fn count_in(&mut self, scope: &str, record_type: Option<&str>) {
        self.add(scope, record_type, 1);
    }

    fn add(&mut self, scope: &str, record_type: Option<&str>, count: usize) {
        if !self.count_record {
            return;
        }
        let name: String = scope
            .chars()
            .chain(record_type.unwrap_or("(no type)").chars())
            .take(Self::MAX_NAME_CHARS)
            .collect();
        let name = if self.counts.contains_key(&name) || self.counts.len() < Self::MAX_TYPES {
            name
        } else {
            Self::OVERFLOW.to_string()
        };
        *self.counts.entry(name).or_default() += count;
    }

    fn check(&mut self, scope: &str, name: Option<&str>, handled: &[&str], ignored: &[&str]) {
        if !name.is_some_and(|name| handled.contains(&name) || ignored.contains(&name)) {
            self.count_in(scope, name);
        }
    }

    /// Observe before dispatch/empty-text filtering so unfamiliar empty records are visible too.
    /// Ignored lists contain format bookkeeping, not speculative future record names.
    pub(crate) fn observe(&mut self, provider: Provider, value: &Value) {
        let kind = value.get("type").and_then(Value::as_str);
        match provider {
            Provider::Codex => {
                self.check(
                    "",
                    kind,
                    &["session_meta", "response_item", "compacted", "event_msg"],
                    // Turn settings and numeric usage counters have no conversation body.
                    &["turn_context", "token_usage_record"],
                );
                if kind == Some("response_item") {
                    let payload = value.get("payload").unwrap_or(&Value::Null);
                    self.check(
                        "response_item.",
                        payload.get("type").and_then(Value::as_str),
                        &[
                            "message",
                            "function_call",
                            "custom_tool_call",
                            "function_call_output",
                            "custom_tool_call_output",
                        ],
                        &[],
                    );
                    if payload.get("type").and_then(Value::as_str) == Some("message") {
                        self.check(
                            "response_item.message.",
                            payload.get("role").and_then(Value::as_str),
                            &["user", "assistant"],
                            // Undispatched instruction text must remain visible in health.
                            &[],
                        );
                    }
                } else if kind == Some("event_msg") {
                    let payload = value.get("payload").unwrap_or(value);
                    self.check(
                        "event_msg.",
                        payload
                            .get("type")
                            .or_else(|| payload.get("event_type"))
                            .and_then(Value::as_str),
                        &[
                            "turn_aborted",
                            "thread_settings_applied",
                            "thread_goal_updated",
                            "web_search_end",
                            "mcp_tool_call_end",
                        ],
                        // Usage and task lifecycle counters; completion prose is a final-message echo.
                        &["token_count", "task_started", "task_complete"],
                    );
                }
            }
            Provider::Pi | Provider::PrimeAgent => {
                self.check(
                    "",
                    kind,
                    &[
                        "session",
                        "message",
                        "compaction",
                        "branch_summary",
                        "custom_message",
                    ],
                    // Model/usage settings, extension state, bookmarks and display metadata.
                    &[
                        "thinking_level_change",
                        "model_change",
                        "usage",
                        "custom",
                        "label",
                        "session_info",
                        "service_tier_change",
                        "child_usage_attributed",
                        "session_state",
                        "agent_status",
                        "git_state",
                    ],
                );
                if kind == Some("message") {
                    let message = value.get("message").unwrap_or(&Value::Null);
                    self.check(
                        "message.",
                        message
                            .get("role")
                            .and_then(Value::as_str)
                            .or_else(|| message.get("type").and_then(Value::as_str)),
                        &[
                            "user",
                            "assistant",
                            "toolResult",
                            "bashExecution",
                            "compactionSummary",
                            "compaction",
                            "branch_summary",
                            "custom",
                            "custom_message",
                        ],
                        &[],
                    );
                }
            }
            Provider::Cursor => {
                self.check(
                    "",
                    value.get("role").and_then(Value::as_str),
                    &["user", "assistant"],
                    &[],
                );
            }
            Provider::Antigravity => {
                if kind.is_none_or(str::is_empty)
                    && matches!(
                        value.get("source").and_then(Value::as_str),
                        Some("USER_EXPLICIT" | "USER" | "MODEL")
                    )
                {
                    return;
                }
                self.check(
                    "",
                    kind,
                    &[
                        "USER_INPUT",
                        "PLANNER_RESPONSE",
                        "SYSTEM_MESSAGE",
                        "ERROR_MESSAGE",
                        "CHECKPOINT",
                        "RUN_COMMAND",
                        "SEARCH_WEB",
                        "VIEW_FILE",
                        "CODE_ACTION",
                        "FILE_CHANGE",
                        "GREP_SEARCH",
                        "LIST_DIRECTORY",
                        "GENERIC",
                    ],
                    // Replay of conversation already present in the transcript.
                    &["CONVERSATION_HISTORY"],
                );
            }
            Provider::AiStudio => {
                self.check(
                    "",
                    value.get("role").and_then(Value::as_str),
                    &["user", "model", "assistant"],
                    &[],
                );
            }
            Provider::GeminiCli => {
                if let Some(set) = value.get("$set") {
                    if let Some(messages) = set.get("messages").and_then(Value::as_array) {
                        for message in messages {
                            self.observe(provider, message);
                        }
                    }
                } else if kind.is_some() {
                    self.check(
                        "",
                        kind,
                        &["user", "gemini", "warning", "info", "error"],
                        // Devtools transport logs, not chat turns.
                        &["console", "network"],
                    );
                } else if ![
                    "$rewindTo",
                    "$remove",
                    "$patch",
                    "sessionId",
                    "startTime",
                    "lastUpdated",
                    "kind",
                ]
                .iter()
                .any(|key| value.get(*key).is_some())
                {
                    self.count(None);
                }
            }
            // Claude dispatch also diagnoses attachment layouts and SDK system subtypes.
            Provider::Claude | Provider::ClaudeDesktop => {}
        }
    }

    pub(crate) fn publish(&self, metadata: &mut Value) {
        if !self.counts.is_empty() {
            metadata[UNRECOGNIZED_RECORD_TYPES_KEY] = json!(self.counts);
        }
    }

    /// Migration families parse two physical source files into one logical session.
    pub(crate) fn merge_sources(successor: &mut ParsedSession, predecessor: &ParsedSession) {
        let mut inventory = Self::default();
        for parsed in [&*successor, predecessor] {
            let metadata: Value = parsed
                .session
                .raw_metadata_json
                .as_deref()
                .and_then(|raw| serde_json::from_str(raw).ok())
                .unwrap_or_default();
            if let Some(counts) = metadata[UNRECOGNIZED_RECORD_TYPES_KEY].as_object() {
                for (name, count) in counts {
                    if let Some(count) = count.as_u64().and_then(|n| usize::try_from(n).ok()) {
                        inventory.add("", Some(name), count);
                    }
                }
            }
        }
        inventory.publish_session(successor);
    }

    pub(crate) fn publish_session(&self, parsed: &mut ParsedSession) {
        let mut metadata = parsed
            .session
            .raw_metadata_json
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok())
            .unwrap_or_else(|| json!({}));
        self.publish(&mut metadata);
        parsed.session.raw_metadata_json = Some(metadata.to_string());
    }

    pub(crate) fn layout(&mut self, kind: &str, difference: &str, value: &Value) {
        let keys = value
            .as_object()
            .map(|o| o.keys().cloned().collect::<Vec<_>>().join(","))
            .unwrap_or_else(|| {
                match value {
                    Value::Null => "null",
                    Value::Array(_) => "array",
                    Value::Bool(_) => "boolean",
                    Value::Number(_) => "number",
                    Value::String(_) => "string",
                    Value::Object(_) => unreachable!(),
                }
                .to_owned()
            });
        self.count(Some(&format!(
            "attachment/{kind} ({difference}; keys: {keys})"
        )));
    }

    pub(crate) fn warning(&self) -> Option<String> {
        if self.counts.is_empty() {
            return None;
        }
        Some(
            self.counts
                .iter()
                .map(|(name, count)| match count {
                    1 => format!("1 record of unrecognized type {name:?} was not indexed"),
                    count => {
                        format!("{count} records of unrecognized type {name:?} were not indexed")
                    }
                })
                .collect::<Vec<_>>()
                .join("; "),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_retains_claude_name_and_bucket_bounds() {
        let mut health = UnrecognizedRecordTypes::default();
        for _ in 0..2 {
            for n in 0..100 {
                health.count(Some(&format!("{n}{}", "α".repeat(100))));
            }
        }
        assert_eq!(health.counts.len(), 17);
        assert!(health.counts.keys().all(|name| name.chars().count() <= 64));
        assert_eq!(health.counts.values().sum::<usize>(), 200);
        assert_eq!(health.counts[UnrecognizedRecordTypes::OVERFLOW], 168);
    }
}
