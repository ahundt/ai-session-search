// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

use serde_json::{json, Value};

use crate::models::{ParsedSession, Provider};

pub(crate) const UNRECOGNIZED_RECORD_TYPES_KEY: &str = "unrecognized_record_types";

/// Each provider table names the indexed arm or why known content is deliberately omitted.
/// Anything absent from that table is unknown and reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordDisposition {
    Indexed(&'static str),
    Ignored(&'static str),
}

impl RecordDisposition {
    fn description(self) -> &'static str {
        match self {
            Self::Indexed(arm) | Self::Ignored(arm) => arm,
        }
    }
}

pub(crate) type RecordTypeRule = (&'static str, RecordDisposition);

/// Match the native discriminator at its own scope, not a coincidentally equal dotted name.
pub(crate) fn scoped_name_matches(known: &str, scope: &str, name: &str) -> bool {
    let split = known.rfind(['.', '/']).map_or(0, |index| index + 1);
    &known[..split] == scope && &known[split..] == name
}

pub(crate) fn rules(provider: Provider) -> &'static [RecordTypeRule] {
    match provider {
        Provider::Claude | Provider::ClaudeDesktop => super::claude::RECORD_TYPES,
        Provider::Codex => super::codex::RECORD_TYPES,
        Provider::Pi | Provider::PrimeAgent => super::pi::RECORD_TYPES,
        Provider::Cursor => super::cursor::RECORD_TYPES,
        Provider::Antigravity => super::antigravity::RECORD_TYPES,
        Provider::AiStudio => super::aistudio::RECORD_TYPES,
        Provider::GeminiCli => super::gemini_cli::RECORD_TYPES,
    }
}

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

    fn check(&mut self, provider: Provider, scope: &str, name: Option<&str>) {
        if !name.is_some_and(|name| {
            rules(provider).iter().any(|(known, disposition)| {
                scoped_name_matches(known, scope, name) && !disposition.description().is_empty()
            })
        }) {
            self.count_in(scope, name);
        }
    }

    /// Inspect native discriminators before empty-text filtering. Provider-local tables own
    /// classification; an unmatched discriminator is unknown, even when its payload is empty.
    pub(crate) fn observe(&mut self, provider: Provider, value: &Value) {
        let kind = value.get("type").and_then(Value::as_str);
        match provider {
            Provider::Codex => {
                self.check(provider, "", kind);
                if kind == Some("response_item") {
                    let payload = value.get("payload").unwrap_or(&Value::Null);
                    self.check(
                        provider,
                        "response_item.",
                        payload.get("type").and_then(Value::as_str),
                    );
                    if payload.get("type").and_then(Value::as_str) == Some("message") {
                        self.check(
                            provider,
                            "response_item.message.",
                            payload.get("role").and_then(Value::as_str),
                        );
                    }
                } else if kind == Some("event_msg") {
                    let payload = value.get("payload").unwrap_or(value);
                    self.check(
                        provider,
                        "event_msg.",
                        payload
                            .get("type")
                            .or_else(|| payload.get("event_type"))
                            .and_then(Value::as_str),
                    );
                }
            }
            Provider::Pi | Provider::PrimeAgent => {
                self.check(provider, "", kind);
                if kind == Some("message") {
                    let message = value.get("message").unwrap_or(&Value::Null);
                    self.check(
                        provider,
                        "message.",
                        message
                            .get("role")
                            .and_then(Value::as_str)
                            .or_else(|| message.get("type").and_then(Value::as_str)),
                    );
                }
            }
            Provider::Cursor | Provider::AiStudio => {
                self.check(provider, "", value.get("role").and_then(Value::as_str))
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
                self.check(provider, "", kind);
            }
            Provider::GeminiCli => {
                if let Some(set) = value.get("$set") {
                    if let Some(messages) = set.get("messages").and_then(Value::as_array) {
                        for message in messages {
                            self.observe(provider, message);
                        }
                    }
                } else if kind.is_some() {
                    self.check(provider, "", kind);
                } else if !rules(provider)
                    .iter()
                    .filter_map(|(name, _)| {
                        name.strip_prefix("metadata.")
                            .or_else(|| name.strip_prefix("operation."))
                    })
                    .any(|name| value.get(name).is_some())
                {
                    self.count(None);
                }
            }
            // Claude diagnoses system subtypes and attachment layouts at their dispatch arms.
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
    fn discriminator_scopes_do_not_hide_missing_or_unknown_types() {
        let mut codex = UnrecognizedRecordTypes::default();
        codex.observe(Provider::Codex, &json!({"type":"response_item.reasoning"}));
        assert_eq!(
            codex.counts,
            std::collections::BTreeMap::from([("response_item.reasoning".into(), 1)])
        );
        let mut gemini = UnrecognizedRecordTypes::default();
        gemini.observe(Provider::GeminiCli, &json!({"user":"not a discriminator"}));
        gemini.observe(Provider::GeminiCli, &json!({"type":"$set"}));
        assert_eq!(
            gemini.counts,
            std::collections::BTreeMap::from([("(no type)".into(), 1), ("$set".into(), 1)])
        );
    }

    #[test]
    fn seeded_inventory_preserves_admission_and_ignores_overlap_diagnostics() {
        let mut full = UnrecognizedRecordTypes::default();
        for n in 0..15 {
            full.count(Some(&format!("first-{n}")));
        }
        let mut metadata = json!({});
        full.publish(&mut metadata);
        let raw = metadata.to_string();
        let context = crate::tail::TailContext {
            health_seed: Some(&raw),
            new_bytes_start: 100,
            ..Default::default()
        };
        let mut tail = UnrecognizedRecordTypes::for_context(Some(context));
        tail.record(Some(context), 99);
        tail.count(Some("overlap-only"));
        tail.layout("future-layout", "overlap", &json!({}));
        tail.record(Some(context), 100);
        let unicode = "α".repeat(100);
        for name in [
            unicode.as_str(),
            "overflow-name",
            "first-0",
            unicode.as_str(),
        ] {
            tail.count(Some(name));
            full.count(Some(name));
        }
        assert_eq!(tail.counts, full.counts);
        assert_eq!(tail.counts.len(), 17);
        assert_eq!(tail.counts[UnrecognizedRecordTypes::OVERFLOW], 1);
        let overlap = crate::tail::TailContext {
            new_bytes_start: usize::MAX,
            ..context
        };
        let mut overlap_inventory = UnrecognizedRecordTypes::for_context(Some(overlap));
        overlap_inventory.record(Some(overlap), 100);
        overlap_inventory.count(Some("overlap-only"));
        assert!(overlap_inventory.counts.is_empty());
    }

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
