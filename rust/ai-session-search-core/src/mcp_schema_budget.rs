// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

//! Measure the emitted MCP catalogue against every client limit it can silently breach.
//!
//! This server has never measured what it emits, which is how one `inputSchema` reached 9,954
//! bytes against a 5,000-byte client budget with nothing to notice. Every limit below is one
//! row swept over every advertised tool, so a tool added later is covered without editing an
//! assertion, and every row carries the four facts that let a maintainer re-evaluate it when
//! the client moves: what the number is for, where it was measured, and when to raise or lower
//! it. Each of these numbers has moved at least once -- Codex's schema budget went 4,000 to
//! 5,000, and one Gemini CLI fork carried 4,000,000 before lowering to 25,000.
//!
//! Two failure modes of this module are load-bearing, because a checker that reports a clean
//! bill is indistinguishable from a surface with no defects:
//!
//! 1. [`codex_visible_schema`] must keep property *names*. Codex types `properties` as a
//!    `BTreeMap<String, JsonSchema>`, so the names are map keys and survive the round trip;
//!    only their values recurse. Filtering every object down to the fourteen modelled keys
//!    instead measures `search_messages` at 62 bytes and passes every budget trivially.
//! 2. An empty or short catalogue is an error, never a pass. [`evaluate`] is always run against
//!    a catalogue whose size the caller has asserted.
//!
//! Two modes. [`Status::Pending`] is the ratchet: a row measuring a defect this repository has
//! not fixed yet reports its measurement without failing, and the commit that fixes it sets
//! `enforced: true` in the same change, so reverting the fix reverts the gate with it. `strict`
//! enforces every row and is expected to fail until the whole schema sequence has landed.

use clap::Args;
use serde_json::{json, Map, Value};

/// Emitted as a `maximum`, `i64::MAX` rejects nothing a caller could send, so it describes the
/// storage type rather than the parameter.
pub const FAKE_INTEGER_MAXIMUM: i64 = i64::MAX;

/// Exactly the keys Codex's `JsonSchema` models (`codex-rs/tools/src/json_schema.rs:41-74`).
/// Anything else is dropped by deserialization before a model sees it, and because the budget is
/// measured after that round trip, it is not even charged for.
const CODEX_SCALAR_KEYS: [&str; 6] = ["$ref", "type", "description", "encrypted", "enum", "required"];
const CODEX_MAP_KEYS: [&str; 3] = ["properties", "$defs", "definitions"];
const CODEX_LIST_KEYS: [&str; 3] = ["anyOf", "oneOf", "allOf"];

/// Root keywords VS Code rejects in an `inputSchema`, dropping the tool rather than degrading it.
pub const REJECTED_ROOT_COMBINATORS: [&str; 7] =
    ["anyOf", "oneOf", "allOf", "not", "if", "then", "else"];

/// What a caller loses when the artifact crosses the limit.
///
/// The distinction is not decoration: a silent breach and an announced one need different
/// margins, and reading them as the same thing is what makes a numerically smaller cap look
/// like the one worth defaulting to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureMode {
    /// The client neither errors nor marks the result. Nothing downstream reports it.
    Silent,
    /// The client reports the overflow and preserves what it could not deliver inline.
    Announced,
    /// The client refuses the artifact outright.
    Rejected,
    /// Breaching it costs bytes or clarity but no client behaviour changes.
    NoClientEffect,
}

/// Which artifact a row measures, and therefore what the sweep iterates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppliesTo {
    InputSchema,
    ToolDescription,
    ParameterDescription,
    ToolName,
    OutputSchema,
    /// A `tools/call` artifact. A catalogue cannot observe it, so the sweep reports it as
    /// unmeasured rather than as passing.
    Response,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pass,
    /// Inside the limit but past its review line.
    Warn,
    /// Over the limit, and the package that fixes it has not landed. Reported, never failed.
    Pending,
    Fail,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Warn => "WARN",
            Status::Pending => "PENDING",
            Status::Fail => "FAIL",
        }
    }
}

/// One (client, artifact, limit) triple, swept over every tool the catalogue advertises.
#[derive(Debug, Clone, Copy)]
pub struct HarnessLimit {
    pub name: &'static str,
    pub client: &'static str,
    pub artifact: &'static str,
    pub budget: usize,
    pub unit: &'static str,
    pub failure_mode: FailureMode,
    pub applies_to: AppliesTo,
    /// False while the defect this row measures is still scheduled for a later package.
    pub enforced: bool,
    /// The package that sets `enforced: true` in the same change that makes the row pass.
    pub enforced_by: &'static str,
    pub warn_at: Option<usize>,
    /// True when crossing the budget is a review signal rather than a breach.
    pub warn_only: bool,
    /// Why this number, and what the caller actually loses past it.
    pub rationale: &'static str,
    /// Where it was read, in what version, on what date.
    pub platform: &'static str,
    pub raise_when: &'static str,
    pub lower_when: &'static str,
}

/// Client-enforced limits. Each is a real cap read from that client's own source or documentation.
pub const HARNESS_LIMITS: &[HarnessLimit] = &[
    HarnessLimit {
        name: "codex-input-schema-bytes",
        client: "Codex 0.146.0",
        artifact: "Codex-normalized inputSchema",
        budget: 5_000,
        unit: "bytes",
        failure_mode: FailureMode::Silent,
        applies_to: AppliesTo::InputSchema,
        enforced: true,
        enforced_by: "",
        warn_at: None,
        warn_only: false,
        rationale: "MAX_COMPACT_TOOL_SCHEMA_BYTES. Past it Codex runs strip_schema_descriptions at \
                    every depth and hands the model a schema whose parameters have names and types \
                    and nothing else. No marker is emitted, nothing is logged, and no file keeps \
                    the deleted text.",
        platform: "Codex only, verified 2026-08-03 against codex-cli 0.146.0, \
                   codex-rs/tools/src/json_schema.rs:222. No other measured client bounds schema bytes.",
        raise_when: "Codex raises its own limit. It already moved 4,000 to 5,000 in b6f9aee16d \
                     (2026-07-08), so this is a moving target rather than a floor.",
        lower_when: "Codex lowers it, or a second client is found that bounds schema bytes tighter.",
    },
    HarnessLimit {
        name: "codex-input-schema-margin",
        client: "Codex 0.146.0",
        artifact: "Codex-normalized inputSchema",
        budget: 4_750,
        unit: "bytes",
        failure_mode: FailureMode::NoClientEffect,
        applies_to: AppliesTo::InputSchema,
        enforced: true,
        enforced_by: "",
        warn_at: None,
        warn_only: true,
        rationale: "A tripwire, not a limit. Crossing it breaks nothing; it reports that the margin \
                    against a cliff with no warning is thin. It is expected to fire: no measured \
                    route to a complete 37-field contract reaches it, and the achievable figure is \
                    about 4,950 bytes.",
        platform: "Derived from the row above, not from any client constant.",
        raise_when: "The achievable size drops far enough that a tighter line stays actionable.",
        lower_when: "Codex raises its budget and the extra headroom is genuinely available.",
    },
    HarnessLimit {
        name: "claude-code-tool-description-chars",
        client: "Claude Code 2.1.220",
        artifact: "tool.description",
        budget: 2_048,
        unit: "characters",
        failure_mode: FailureMode::Silent,
        applies_to: AppliesTo::ToolDescription,
        enforced: true,
        enforced_by: "",
        warn_at: Some(1_986),
        warn_only: false,
        rationale: "MAX_MCP_DESCRIPTION_LENGTH. Past it Claude Code appends a truncation marker to \
                    what it keeps, so text beyond this point is paid for and teaches nobody. The \
                    review line sits 62 characters below, the mean length of one conflict rule: the \
                    nine validator rules render verbatim into this channel and are the only part \
                    that grows without anyone editing a description.",
        platform: "Claude Code only; Codex does not cap this channel. services/mcp/client.ts:218, \
                   confirmed in 2.1.88 source and read again from the 2.1.220 binary.",
        raise_when: "Claude Code raises MAX_MCP_DESCRIPTION_LENGTH.",
        lower_when: "A second client is found that caps this channel lower.",
    },
    HarnessLimit {
        name: "vscode-parameter-description-chars",
        client: "VS Code / Copilot, GPT-4o family",
        artifact: "schema-internal description",
        budget: 1_024,
        unit: "characters",
        failure_mode: FailureMode::Silent,
        applies_to: AppliesTo::ParameterDescription,
        enforced: true,
        enforced_by: "",
        warn_at: None,
        warn_only: false,
        rationale: "gpt4oMaxStringLength. VS Code truncates every description inside the schema at \
                    this length before sending, so the tail costs wire bytes and reaches no model.",
        platform: "VS Code / Copilot, GPT-4o family only; it does not apply to the tool's own \
                   description. toolSchemaNormalizer.ts:79-87, fetched 2026-08-02.",
        raise_when: "VS Code raises the constant or retires that family from the normalizer.",
        lower_when: "Another family in the same normalizer is found with a tighter bound.",
    },
    HarnessLimit {
        name: "vscode-no-root-combinator",
        client: "VS Code / Copilot, all model families",
        artifact: "inputSchema root keywords",
        budget: 0,
        unit: "root combinators",
        failure_mode: FailureMode::Rejected,
        applies_to: AppliesTo::InputSchema,
        enforced: true,
        enforced_by: "",
        warn_at: None,
        warn_only: false,
        rationale: "VS Code rejects anyOf, oneOf, allOf, not, if, then and else at an inputSchema \
                    root for every model family, and the tool disappears rather than degrading. The \
                    Claude API rejects the first three at a root as well; Claude Code 2.1.195+ \
                    flattens them and older versions skip the tool. Nest the union under a property.",
        platform: "toolSchemaNormalizer.ts:118-125, fetched 2026-08-02; corroborated by current \
                   Claude Code MCP documentation.",
        raise_when: "Never. This is a structural rejection, not a budget.",
        lower_when: "Never; zero is already the floor.",
    },
    HarnessLimit {
        name: "mcp-output-schema-depth",
        client: "MCP specification",
        artifact: "outputSchema nesting depth",
        budget: 10,
        unit: "levels",
        failure_mode: FailureMode::Rejected,
        applies_to: AppliesTo::OutputSchema,
        enforced: false,
        enforced_by: "WP-GQ-deduplicate-and-correct-output-schemas",
        warn_at: None,
        warn_only: false,
        rationale: "The specification tells clients to apply a maximum schema depth to prevent a \
                    denial-of-service vector but prescribes no number. Measured here: 21, 18, 12, 9, \
                    9 and three at 8. A budget of 8 would fail on every tool including the five the \
                    extraction never touches, so 10 is what the work reaches and what those five \
                    already pass.",
        platform: "The specification, not any one client; no measured client enforces a depth bound \
                   today, so this is conformance headroom. Counted as containers only with the root \
                   at 1: a scalar inside an enum array is not a validator recursion level.",
        raise_when: "A legitimate schema needs more nesting than extraction can flatten. Name a \
                     repeated type first; a $ref is a leaf at its point of use.",
        lower_when: "A client is found that enforces a tighter bound.",
    },
    HarnessLimit {
        name: "mcp-output-schema-subschemas",
        client: "MCP specification",
        artifact: "outputSchema subschema count",
        budget: 250,
        unit: "subschemas",
        failure_mode: FailureMode::Rejected,
        applies_to: AppliesTo::OutputSchema,
        enforced: false,
        enforced_by: "WP-GQ-deduplicate-and-correct-output-schemas",
        warn_at: None,
        warn_only: false,
        rationale: "The same specification clause asks clients to cap the total number of \
                    subschemas. search_messages measures 256, so this is a reduction target rather \
                    than headroom. Naming a repeated shape collapses its whole inline subtree to one \
                    $ref position, which is what brings it under.",
        platform: "The specification, not any one client. Counted as the schema positions a \
                   validator may enter: the root, each properties/$defs/definitions value, each \
                   items, each anyOf/oneOf/allOf member, and an object additionalProperties.",
        raise_when: "Extraction genuinely needs more named types than this allows.",
        lower_when: "The count approaches the budget and validation cost becomes measurable.",
    },
    HarnessLimit {
        name: "anthropic-tool-name-charset",
        client: "Anthropic API and the MCP SDK",
        artifact: "tool name",
        budget: 64,
        unit: "characters matching ^[a-zA-Z0-9_-]{1,64}$",
        failure_mode: FailureMode::Rejected,
        applies_to: AppliesTo::ToolName,
        enforced: true,
        enforced_by: "",
        warn_at: None,
        warn_only: false,
        rationale: "The Anthropic API is the tightest of the three registries this server must \
                    satisfy: 64 characters and no dots, against the MCP SDK's 128 with dots allowed \
                    and Codex's 64. A name outside it is rejected at registration, not truncated.",
        platform: "Anthropic Messages API tool-name regex; Codex tools.rs:226; the SDK vendored in \
                   Claude Code 2.1.220.",
        raise_when: "The Anthropic API relaxes its pattern.",
        lower_when: "A registered client is found with a shorter name limit.",
    },
    HarnessLimit {
        name: "codex-tool-result-chars",
        client: "Codex 0.146.0",
        artifact: "serialized CallToolResult",
        budget: 48_000,
        unit: "characters",
        failure_mode: FailureMode::Silent,
        applies_to: AppliesTo::Response,
        enforced: false,
        enforced_by: "WP-F-bound-mcp-results-without-silent-reduction",
        warn_at: None,
        warn_only: false,
        rationale: "About 12,000 model tokens at four characters each. Codex is the only measured \
                    client that middle-truncates a result with no marker, which keeps a plausible \
                    head and tail and so looks complete. That, not being the smallest number, is why \
                    it sets the default ceiling: Gemini CLI's 40,000 is smaller and announces.",
        platform: "Codex core/src/tools/context.rs:147 with models-manager/models.json, 2026-08-03.",
        raise_when: "The deployment talks only to clients that announce truncation, or Codex raises \
                     its own cap.",
        lower_when: "A client is found that truncates silently below this. Antigravity is the live \
                     risk: it superseded Gemini CLI, holds three of the thirteen registrations, and \
                     its truncation behaviour is unverified.",
    },
    HarnessLimit {
        name: "claude-code-tool-result-tokens",
        client: "Claude Code 2.1.220",
        artifact: "tool-result tokens",
        budget: 25_000,
        unit: "tokens",
        failure_mode: FailureMode::Announced,
        applies_to: AppliesTo::Response,
        enforced: false,
        enforced_by: "WP-F-bound-mcp-results-without-silent-reduction",
        warn_at: None,
        warn_only: false,
        rationale: "MAX_MCP_OUTPUT_TOKENS. Over it, Claude Code persists the result to a file, links \
                    it, and supplies jq and grep instructions, so nothing is lost. Recorded so the \
                    difference between an announced overflow and an unannounced one stays measurable.",
        platform: "Claude Code utils/mcpValidation.ts:16, confirmed in 2.1.88 source and the 2.1.220 \
                   binary.",
        raise_when: "A tool declares _meta anthropic/maxResultSizeChars, up to its 500,000-character \
                     ceiling.",
        lower_when: "Claude Code lowers the default and stops persisting the overflow.",
    },
    HarnessLimit {
        name: "gemini-cli-tool-result-chars",
        client: "Gemini CLI (legacy registration)",
        artifact: "tool-result characters",
        budget: 40_000,
        unit: "characters",
        failure_mode: FailureMode::Announced,
        applies_to: AppliesTo::Response,
        enforced: false,
        enforced_by: "WP-F-bound-mcp-results-without-silent-reduction",
        warn_at: None,
        warn_only: false,
        rationale: "Numerically the smallest result cap measured, and the reason the default ceiling \
                    is not simply the smallest number: Gemini CLI keeps the first 20% and last 80% \
                    of its budget, saves the rest to a file, and states how many characters it \
                    omitted.",
        platform: "google-gemini/gemini-cli packages/core/src/config/config.ts:476 at HEAD f47d6c6f, \
                   2026-07-31. Forks differ by two orders of magnitude; qwen-code carries 25,000.",
        raise_when: "Never on its own account; Antigravity superseded this client and holds the live \
                     registrations.",
        lower_when: "A fork in use here is measured lower and does not announce.",
    },
];

/// This repository's own emitted-schema rules. Same mechanism, different authority: these come
/// from the style spec rather than a client's cap, and each names the fact a caller loses.
pub const SCHEMA_RULES: &[HarnessLimit] = &[
    HarnessLimit {
        name: "style-no-aise-vendor-keys",
        client: "S8-emit-no-aise-vendor-keys",
        artifact: "x-aise-* keys anywhere in a tool",
        budget: 0,
        unit: "keys",
        failure_mode: FailureMode::NoClientEffect,
        applies_to: AppliesTo::InputSchema,
        enforced: true,
        enforced_by: "",
        warn_at: None,
        warn_only: false,
        rationale: "No MCP client is specified to read them and none does, so they are wire bytes \
                    for the twelve registrations that forward schemas verbatim and nothing at all \
                    for the model. Scoped to this prefix rather than to x- generally: MCP 2026-07-28 \
                    defines x-mcp-header inside inputSchema with MUST-level client behaviour.",
        platform: "This repository. Grepped every .rs, .py, .ts and .js here: producers plus \
                   in-crate tests, and no external reader.",
        raise_when: "Never; an unread key is never worth emitting.",
        lower_when: "Never; zero is already the floor.",
    },
    HarnessLimit {
        name: "style-no-input-const",
        client: "S7-no-fact-lives-only-in-a-stripped-keyword",
        artifact: "const in an inputSchema",
        budget: 0,
        unit: "sites",
        failure_mode: FailureMode::NoClientEffect,
        applies_to: AppliesTo::InputSchema,
        enforced: true,
        enforced_by: "",
        warn_at: None,
        warn_only: false,
        rationale: "Codex models enum and does not model const, so a const discriminator reaches no \
                    model and the caller is shown an untyped, valueless tag. JSON Schema 2020-12 \
                    section 6.1.3 makes a single-value enum exactly equivalent, so the replacement \
                    is lossless by specification rather than by judgement. Scoped to inputSchema: no \
                    model reads an outputSchema, so the const sites there cost nothing.",
        platform: "Codex codex-rs/tools/src/json_schema.rs:41-74. VS Code's normalizer keeps const, \
                   so this is Codex-specific and costs nothing elsewhere.",
        raise_when: "Never; the enum form is equivalent and universally modelled.",
        lower_when: "Never; zero is already the floor.",
    },
    HarnessLimit {
        name: "style-no-fake-integer-maxima",
        client: "S13-every-bound-states-its-origin",
        artifact: "maximum equal to i64::MAX",
        budget: 0,
        unit: "sites",
        failure_mode: FailureMode::NoClientEffect,
        applies_to: AppliesTo::InputSchema,
        enforced: true,
        enforced_by: "",
        warn_at: None,
        warn_only: false,
        rationale: "i64::MAX rejects nothing a caller could send, so it was never a bound. It \
                    describes the storage type at 30 bytes a site, and both Codex and VS Code delete \
                    the keyword before a model sees it anyway.",
        platform: "This repository. Removing it cannot change what is accepted, because no i64 \
                   exceeds it.",
        raise_when: "Never; state a real bound instead.",
        lower_when: "Never; zero is already the floor.",
    },
    HarnessLimit {
        name: "style-local-refs-only",
        client: "S1-every-ref-is-a-local-pointer",
        artifact: "$ref targets",
        budget: 0,
        unit: "non-local refs",
        failure_mode: FailureMode::Rejected,
        applies_to: AppliesTo::OutputSchema,
        enforced: true,
        enforced_by: "",
        warn_at: None,
        warn_only: false,
        rationale: "MCP states that implementations MUST NOT automatically dereference a $ref that \
                    resolves to a network URI, so a non-local ref is unresolvable at the client and \
                    should be rejected rather than treated as permissive.",
        platform: "MCP 2026-07-28, basic section on $ref resolution.",
        raise_when: "Never.",
        lower_when: "Never; zero is already the floor.",
    },
    HarnessLimit {
        name: "style-reachable-definitions",
        client: "S4-extract-a-shape-only-above-break-even",
        artifact: "unreachable $defs entries",
        budget: 0,
        unit: "entries",
        failure_mode: FailureMode::NoClientEffect,
        applies_to: AppliesTo::OutputSchema,
        enforced: true,
        enforced_by: "",
        warn_at: None,
        warn_only: false,
        rationale: "Current Codex prunes definitions no $ref reaches before it measures the schema, \
                    so an unreachable entry is wire bytes that buy nothing. Keeping the emitted set \
                    reachable also keeps the local measurement equal to what the client counts.",
        platform: "Codex schema sanitization and pruning, openai/codex PR #23357, checked 2026-08-03.",
        raise_when: "Never.",
        lower_when: "Never; zero is already the floor.",
    },
];

/// Every row the sweep knows about, client caps and style rules together.
pub fn all_limits() -> impl Iterator<Item = &'static HarnessLimit> {
    HARNESS_LIMITS.iter().chain(SCHEMA_RULES.iter())
}

/// Per-tool ceilings for the artifacts this repository has not finished shrinking.
///
/// These are today's measurements, not targets. They exist so a change cannot make an
/// already-breached artifact worse while the package that fixes it is still pending: the sweep
/// reports those rules as [`Status::Pending`], and this table is what stops the pending window
/// being a free-for-all. Each entry lowers as its package lands, and a tool with no entry is a
/// tool nothing is watching, so [`ceiling_for`] refuses to guess one.
///
/// Measured in bytes, which is the unit the client budget is denominated in. That distinction
/// is not pedantry: `search_sessions` and `list_sessions` each carry one em dash, three UTF-8
/// bytes that a measurement escaping non-ASCII reports as the six characters `—`. Every
/// earlier figure for those two tools was three high for exactly that reason, which is one
/// reason this check lives in the language that counts what the client counts.
///
/// Columns: tool, Codex-normalized `inputSchema` bytes, `outputSchema` depth.
pub const EMITTED_ARTIFACT_CEILINGS: [(&str, usize, usize); 8] = [
    ("search_messages", 4_995, 18),
    ("run_skill_capability", 4_983, 21),
    ("get_session", 3_567, 12),
    ("search_sessions", 3_368, 8),
    ("list_sessions", 3_010, 8),
    ("query_session_index", 1_724, 8),
    ("get_resume_command", 348, 8),
    ("get_index_status", 238, 9),
];

/// The recorded `(bytes, depth)` ceiling for one tool, or `None` when nothing is watching it.
pub fn ceiling_for(tool: &str) -> Option<(usize, usize)> {
    EMITTED_ARTIFACT_CEILINGS
        .iter()
        .find(|(name, _, _)| *name == tool)
        .map(|(_, bytes, depth)| (*bytes, *depth))
}

/// One measurement of one row against one tool.
#[derive(Debug, Clone)]
pub struct Finding {
    pub limit: &'static HarnessLimit,
    pub tool: String,
    pub measured: usize,
    pub status: Status,
    /// What was measured and where, so a failure names a site rather than only a number.
    pub evidence: String,
}

// ---------------------------------------------------------------------------------------------
// Measurement primitives
// ---------------------------------------------------------------------------------------------

/// The schema as Codex models it, dropping every keyword it does not deserialize.
///
/// Read the `properties` arm before changing this. Codex types that field as
/// `BTreeMap<String, JsonSchema>`, so property *names* are map keys and survive; only their
/// values recurse. Filtering every object in the tree down to the modelled keys instead deletes
/// `query`, `limit` and the other 35 names, which measures `search_messages` at 62 bytes rather
/// than 9,954 and makes every budget assertion pass on a schema twice its budget.
pub fn codex_visible_schema(value: &Value) -> Value {
    let Some(object) = value.as_object() else {
        return value.clone();
    };
    let mut kept = Map::new();
    for key in CODEX_SCALAR_KEYS {
        if let Some(found) = object.get(key) {
            kept.insert(key.to_owned(), found.clone());
        }
    }
    if let Some(items) = object.get("items") {
        kept.insert("items".to_owned(), codex_visible_schema(items));
    }
    for key in CODEX_MAP_KEYS {
        if let Some(Value::Object(map)) = object.get(key) {
            let inner = map
                .iter()
                .map(|(name, schema)| (name.clone(), codex_visible_schema(schema)))
                .collect();
            kept.insert(key.to_owned(), Value::Object(inner));
        }
    }
    for key in CODEX_LIST_KEYS {
        if let Some(Value::Array(list)) = object.get(key) {
            kept.insert(
                key.to_owned(),
                Value::Array(list.iter().map(codex_visible_schema).collect()),
            );
        }
    }
    if let Some(extra) = object.get("additionalProperties") {
        let projected = match extra {
            Value::Bool(_) => extra.clone(),
            other => codex_visible_schema(other),
        };
        kept.insert("additionalProperties".to_owned(), projected);
    }
    Value::Object(kept)
}

/// Byte length of the compact serialization the client measures.
pub fn compact_len(value: &Value) -> usize {
    serde_json::to_string(value).map(|text| text.len()).unwrap_or(0)
}

/// Depth counting every nested container, arrays included, with the root at 1.
///
/// A scalar is not a level: what this bounds is validator recursion, and a string inside an
/// `enum` array is not a frame. Counting the terminal scalar instead reports every figure one
/// higher, which is the difference between this and the depths recorded in the plan's evidence
/// index.
pub fn schema_depth(value: &Value) -> usize {
    match value {
        Value::Object(map) => 1 + map.values().map(schema_depth).max().unwrap_or(0),
        Value::Array(list) => 1 + list.iter().map(schema_depth).max().unwrap_or(0),
        _ => 0,
    }
}

/// Count the schema positions a validator may enter.
pub fn subschema_count(value: &Value) -> usize {
    let Some(object) = value.as_object() else {
        return 0;
    };
    let mut total = 1;
    for key in CODEX_MAP_KEYS {
        if let Some(Value::Object(map)) = object.get(key) {
            total += map.values().map(subschema_count).sum::<usize>();
        }
    }
    for key in CODEX_LIST_KEYS {
        if let Some(Value::Array(list)) = object.get(key) {
            total += list.iter().map(subschema_count).sum::<usize>();
        }
    }
    if let Some(items) = object.get("items") {
        total += subschema_count(items);
    }
    if let Some(extra @ Value::Object(_)) = object.get("additionalProperties") {
        total += subschema_count(extra);
    }
    total
}

/// Yield every schema-internal description with the path that reaches it.
pub fn collect_descriptions(value: &Value, path: &str, found: &mut Vec<(String, String)>) {
    match value {
        Value::Object(map) => {
            for (key, member) in map {
                if key == "description" {
                    if let Some(text) = member.as_str() {
                        found.push((path.to_owned(), text.to_owned()));
                        continue;
                    }
                }
                collect_descriptions(member, &format!("{path}.{key}"), found);
            }
        }
        Value::Array(list) => {
            for (index, member) in list.iter().enumerate() {
                collect_descriptions(member, &format!("{path}[{index}]"), found);
            }
        }
        _ => {}
    }
}

/// Yield every `(path, value)` whose key matches `wanted`.
fn collect_keyed(value: &Value, wanted: &dyn Fn(&str) -> bool, path: &str, found: &mut Vec<(String, Value)>) {
    match value {
        Value::Object(map) => {
            for (key, member) in map {
                let here = if path.is_empty() { key.clone() } else { format!("{path}.{key}") };
                if wanted(key) {
                    found.push((here.clone(), member.clone()));
                }
                collect_keyed(member, wanted, &here, found);
            }
        }
        Value::Array(list) => {
            for (index, member) in list.iter().enumerate() {
                collect_keyed(member, wanted, &format!("{path}[{index}]"), found);
            }
        }
        _ => {}
    }
}

/// Every `$ref` target in the document.
pub fn collect_refs(value: &Value) -> Vec<String> {
    let mut found = Vec::new();
    collect_keyed(value, &|key| key == "$ref", "", &mut found);
    found
        .into_iter()
        .filter_map(|(_, target)| target.as_str().map(str::to_owned))
        .collect()
}

/// Names declared under `$defs` or `definitions` that no `$ref` in the document reaches.
pub fn unreachable_definitions(schema: &Value) -> Vec<String> {
    let reached: Vec<String> = collect_refs(schema)
        .into_iter()
        .map(|target| target.rsplit('/').next().unwrap_or_default().to_owned())
        .collect();
    let mut declared = Vec::new();
    for key in ["$defs", "definitions"] {
        let mut found = Vec::new();
        collect_keyed(schema, &|candidate| candidate == key, "", &mut found);
        for (_, member) in found {
            if let Value::Object(map) = member {
                declared.extend(map.keys().cloned());
            }
        }
    }
    let mut orphans: Vec<String> = declared
        .into_iter()
        .filter(|name| !reached.contains(name))
        .collect();
    orphans.sort();
    orphans.dedup();
    orphans
}

fn tool_name_is_acceptable(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Measure one row against one tool, returning the number and the site it came from.
fn measure(limit: &HarnessLimit, tool: &Value) -> (usize, String) {
    let name = tool["name"].as_str().unwrap_or("?");
    let input = &tool["inputSchema"];
    let output = &tool["outputSchema"];
    match limit.name {
        "codex-input-schema-bytes" | "codex-input-schema-margin" => (
            compact_len(&codex_visible_schema(input)),
            "as Codex counts it".to_owned(),
        ),
        "claude-code-tool-description-chars" => (
            tool["description"].as_str().unwrap_or_default().chars().count(),
            "characters of tool.description".to_owned(),
        ),
        "vscode-parameter-description-chars" => {
            let mut found = Vec::new();
            collect_descriptions(input, "inputSchema", &mut found);
            match found.iter().max_by_key(|(_, text)| text.chars().count()) {
                Some((path, text)) => (text.chars().count(), format!("longest at {name}.{path}")),
                None => (0, "no parameter description".to_owned()),
            }
        }
        "vscode-no-root-combinator" => {
            let present: Vec<&str> = REJECTED_ROOT_COMBINATORS
                .into_iter()
                .filter(|key| input.get(*key).is_some())
                .collect();
            let evidence = if present.is_empty() {
                "no root combinator".to_owned()
            } else {
                format!("root combinators present: {present:?}")
            };
            (present.len(), evidence)
        }
        "mcp-output-schema-depth" => (
            schema_depth(output),
            "deepest nested container, root at 1".to_owned(),
        ),
        "mcp-output-schema-subschemas" => (
            subschema_count(output),
            "schema positions a validator may enter".to_owned(),
        ),
        "anthropic-tool-name-charset" => {
            let acceptable = tool_name_is_acceptable(name);
            (if acceptable { 0 } else { 65 }, format!("tool name {name:?}"))
        }
        "style-no-aise-vendor-keys" => {
            let mut found = Vec::new();
            collect_keyed(tool, &|key| key.starts_with("x-aise-"), "", &mut found);
            let evidence = match found.first() {
                Some((path, _)) => format!("first at {path}"),
                None => "no x-aise-* key".to_owned(),
            };
            (found.len(), evidence)
        }
        "style-no-input-const" => {
            let mut found = Vec::new();
            collect_keyed(input, &|key| key == "const", "", &mut found);
            let evidence = match found.first() {
                Some((path, _)) => format!("first at inputSchema.{path}"),
                None => "no const".to_owned(),
            };
            (found.len(), evidence)
        }
        "style-no-fake-integer-maxima" => {
            let mut found = Vec::new();
            collect_keyed(input, &|key| key == "maximum", "", &mut found);
            let fake: Vec<String> = found
                .into_iter()
                .filter(|(_, value)| value.as_i64() == Some(FAKE_INTEGER_MAXIMUM))
                .map(|(path, _)| path)
                .collect();
            let evidence = match fake.first() {
                Some(path) => format!("first at inputSchema.{path}"),
                None => "no i64::MAX maximum".to_owned(),
            };
            (fake.len(), evidence)
        }
        "style-local-refs-only" => {
            let offending: Vec<String> = collect_refs(input)
                .into_iter()
                .chain(collect_refs(output))
                .filter(|target| !target.starts_with("#/"))
                .collect();
            let evidence = match offending.first() {
                Some(target) => format!("first non-local ref {target:?}"),
                None => "every ref is a local pointer".to_owned(),
            };
            (offending.len(), evidence)
        }
        "style-reachable-definitions" => {
            let mut orphans = unreachable_definitions(input);
            orphans.extend(unreachable_definitions(output));
            let evidence = if orphans.is_empty() {
                "every definition is reachable".to_owned()
            } else {
                format!("unreachable: {orphans:?}")
            };
            (orphans.len(), evidence)
        }
        other => (0, format!("no measurement is implemented for {other}")),
    }
}

/// Sweep every applicable row over every advertised tool.
///
/// Rows whose artifact is a `tools/call` result are skipped: a catalogue cannot observe them, and
/// reporting an unobserved stage as passing is the same defect as reporting it as zero bytes.
pub fn evaluate(tools: &[Value], strict: bool) -> Vec<Finding> {
    let mut findings = Vec::new();
    for limit in all_limits() {
        if limit.applies_to == AppliesTo::Response {
            continue;
        }
        for tool in tools {
            let (measured, evidence) = measure(limit, tool);
            let name = tool["name"].as_str().unwrap_or("?").to_owned();
            let over = measured > limit.budget;
            let status = if !over {
                match limit.warn_at {
                    Some(line) if measured > line => Status::Warn,
                    _ => Status::Pass,
                }
            } else if limit.warn_only {
                Status::Warn
            } else if limit.enforced || strict {
                Status::Fail
            } else {
                Status::Pending
            };
            findings.push(Finding { limit, tool: name, measured, status, evidence });
        }
    }
    findings
}

/// State the breach, and for a silent cap state that the caller gets no signal.
pub fn describe_breach(limit: &HarnessLimit, measured: usize) -> String {
    let head = format!(
        "{} measured {measured} {} against the {} {} limit of {}",
        limit.artifact, limit.unit, limit.budget, limit.unit, limit.client
    );
    match limit.failure_mode {
        FailureMode::Silent => format!(
            "{head}. The breach is silent: the client neither errors nor marks the result, so \
             nothing downstream reports it. {}",
            limit.rationale
        ),
        FailureMode::Announced => {
            format!("{head}. The client reports the overflow and preserves it. {}", limit.rationale)
        }
        FailureMode::Rejected => {
            format!("{head}. The client rejects the artifact outright. {}", limit.rationale)
        }
        FailureMode::NoClientEffect => format!("{head}. {}", limit.rationale),
    }
}

// ---------------------------------------------------------------------------------------------
// Stage ledger
// ---------------------------------------------------------------------------------------------

/// The MCP overhead stages, which have different owners and different optimization levers.
///
/// Collapsing any two of them reports a validator cost as a model-context cost, or a wire byte as
/// a token. The last three need a `tools/call` fixture, so a catalogue reports them with a status
/// rather than with a zero that would read as a saving.
pub const LEDGER_STAGES: [&str; 8] = [
    "raw_catalogue",
    "jsonrpc_catalogue_envelope",
    "input_schema_wire",
    "input_schema_client_normalized",
    "output_schema_declaration",
    "canonical_result",
    "call_tool_result",
    "jsonrpc_response",
];

/// The stages a `tools/list` capture cannot observe.
pub const RESPONSE_LEDGER_STAGES: [&str; 3] =
    ["canonical_result", "call_tool_result", "jsonrpc_response"];

fn fixture_required_stage(stage: &str) -> Value {
    json!({
        "artifact": stage.replace('_', " "),
        "status": "fixture-required",
        "interpretation": "a tools/call fixture is required. An unobserved stage carries a status \
                           rather than zero bytes, because zero would read as a saving.",
    })
}

/// Report each MCP overhead stage separately, with the artifact each one measures.
pub fn stage_ledger(tools: &[Value]) -> Value {
    let catalogue = Value::Array(tools.to_vec());
    let envelope = json!({ "jsonrpc": "2.0", "id": 2, "result": { "tools": tools } });
    let input_wire: usize = tools.iter().map(|tool| compact_len(&tool["inputSchema"])).sum();
    let input_normalized: usize = tools
        .iter()
        .map(|tool| compact_len(&codex_visible_schema(&tool["inputSchema"])))
        .sum();
    let output_wire: usize = tools.iter().map(|tool| compact_len(&tool["outputSchema"])).sum();

    let mut stages = Map::new();
    stages.insert(
        "raw_catalogue".to_owned(),
        json!({
            "artifact": "tools[] array",
            "bytes": compact_len(&catalogue),
            "tools": tools.len(),
            "interpretation": "wire, startup and parse cost only",
        }),
    );
    stages.insert(
        "jsonrpc_catalogue_envelope".to_owned(),
        json!({
            "artifact": "tools/list JSON-RPC message",
            "bytes": compact_len(&envelope),
            "interpretation": "the catalogue plus its protocol wrapper. This and raw_catalogue are \
                               two stages, not a discrepancy; a report naming a catalogue size must \
                               say which of the two it measured.",
        }),
    );
    stages.insert(
        "input_schema_wire".to_owned(),
        json!({
            "artifact": "every emitted inputSchema",
            "bytes": input_wire,
            "interpretation": "server-owned schema bytes before any client normalization",
        }),
    );
    stages.insert(
        "input_schema_client_normalized".to_owned(),
        json!({
            "artifact": "every inputSchema after current Codex normalization",
            "bytes": input_normalized,
            "interpretation": "model-facing definition cost; the hard Codex gate applies here",
        }),
    );
    stages.insert(
        "output_schema_declaration".to_owned(),
        json!({
            "artifact": "every emitted outputSchema",
            "bytes": output_wire,
            "model_facing_verified": false,
            "interpretation": "wire and validator cost. Model-facing only if a client is verified to \
                               forward or inline it; five read in source do neither.",
        }),
    );
    for stage in RESPONSE_LEDGER_STAGES {
        stages.insert(stage.to_owned(), fixture_required_stage(stage));
    }

    let per_tool: Vec<Value> = tools
        .iter()
        .map(|tool| {
            let input = &tool["inputSchema"];
            let output = &tool["outputSchema"];
            let normalized = codex_visible_schema(input);
            let normalized_bytes = compact_len(&normalized);
            let mut tool_stages = Map::new();
            tool_stages.insert(
                "raw_catalogue".to_owned(),
                json!({ "artifact": "this tool's entry in tools[]", "bytes": compact_len(tool) }),
            );
            tool_stages.insert(
                "jsonrpc_catalogue_envelope".to_owned(),
                json!({
                    "artifact": "tools/list JSON-RPC message",
                    "status": "catalogue-wide only",
                }),
            );
            tool_stages.insert(
                "input_schema_wire".to_owned(),
                json!({ "artifact": "emitted inputSchema", "bytes": compact_len(input) }),
            );
            tool_stages.insert(
                "input_schema_client_normalized".to_owned(),
                json!({
                    "artifact": "inputSchema after current Codex normalization",
                    "bytes": normalized_bytes,
                    "descriptions_retained": normalized_bytes <= 5_000,
                    "advertised_fields": input["properties"].as_object().map_or(0, Map::len),
                }),
            );
            tool_stages.insert(
                "output_schema_declaration".to_owned(),
                json!({
                    "artifact": "emitted outputSchema",
                    "bytes": compact_len(output),
                    "depth": schema_depth(output),
                    "subschemas": subschema_count(output),
                    "model_facing_verified": false,
                }),
            );
            for stage in RESPONSE_LEDGER_STAGES {
                tool_stages.insert(stage.to_owned(), fixture_required_stage(stage));
            }
            json!({ "tool": tool["name"], "stages": Value::Object(tool_stages) })
        })
        .collect();

    json!({ "stages": Value::Object(stages), "tools": per_tool })
}

// ---------------------------------------------------------------------------------------------
// Command
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Args)]
pub struct SchemaBudgetArgs {
    /// Print the per-stage overhead ledger as JSON instead of checking limits.
    #[arg(long)]
    pub ledger: bool,
    /// Print the advertised tool catalogue itself, so a measurement can be diffed against
    /// what a client receives rather than only against another measurement.
    #[arg(long)]
    pub catalogue: bool,
    /// Enforce every rule, including ones a later package is scheduled to fix.
    #[arg(long)]
    pub strict: bool,
}

/// Group the report by rule, so one breach reads as one finding rather than once per tool.
fn report(findings: &[Finding], strict: bool) -> bool {
    let mut order: Vec<(Status, &'static str)> = Vec::new();
    for finding in findings {
        if finding.status == Status::Pass {
            continue;
        }
        let key = (finding.status, finding.limit.name);
        if !order.contains(&key) {
            order.push(key);
        }
    }
    order.sort_by_key(|(status, name)| {
        let rank = match status {
            Status::Fail => 0,
            Status::Pending => 1,
            Status::Warn => 2,
            Status::Pass => 3,
        };
        (rank, *name)
    });

    for (status, limit_name) in &order {
        let group: Vec<&Finding> = findings
            .iter()
            .filter(|finding| finding.status == *status && finding.limit.name == *limit_name)
            .collect();
        let Some(first) = group.first() else { continue };
        let limit = first.limit;
        println!("{:<7} {} — {}, {}", status.label(), limit.name, limit.client, limit.artifact);
        if matches!(status, Status::Fail | Status::Pending) {
            let worst = group.iter().map(|finding| finding.measured).max().unwrap_or(limit.budget);
            println!("        {}", describe_breach(limit, worst));
        }
        let mut sorted = group.clone();
        sorted.sort_by_key(|finding| std::cmp::Reverse(finding.measured));
        for finding in sorted {
            println!(
                "        {}: {} {} against the {} limit ({})",
                finding.tool, finding.measured, limit.unit, limit.budget, finding.evidence
            );
        }
        if *status == Status::Pending {
            println!(
                "        Not enforced yet; {} sets enforced: true in the same change that makes \
                 it pass.",
                if limit.enforced_by.is_empty() { "a later package" } else { limit.enforced_by }
            );
        }
        println!("        Raise when: {}", limit.raise_when);
        println!("        Lower when: {}", limit.lower_when);
    }

    let unmeasured: Vec<&HarnessLimit> = all_limits()
        .filter(|limit| limit.applies_to == AppliesTo::Response)
        .collect();
    if !unmeasured.is_empty() {
        println!(
            "NOTE    {} response-artifact rows need a tools/call fixture and are not measured here;",
            unmeasured.len()
        );
        println!("        they land with WP-F-bound-mcp-results-without-silent-reduction:");
        for limit in &unmeasured {
            println!("        {}: {} {}", limit.name, limit.budget, limit.unit);
        }
    }

    let failures = findings.iter().filter(|finding| finding.status == Status::Fail).count();
    let passes = findings.iter().filter(|finding| finding.status == Status::Pass).count();
    let warns = findings.iter().filter(|finding| finding.status == Status::Warn).count();
    let mut pending: Vec<&str> = findings
        .iter()
        .filter(|finding| finding.status == Status::Pending)
        .map(|finding| finding.limit.name)
        .collect();
    pending.sort_unstable();
    pending.dedup();
    println!(
        "{} measurements: {passes} pass, {warns} warn, {} rules pending, {failures} fail",
        findings.len(),
        pending.len()
    );
    if failures == 0 && !pending.is_empty() && !strict {
        println!("Pending rules (measured, not yet enforced): {}", pending.join(", "));
    }
    failures == 0
}

/// Run the checker against this binary's own advertised catalogue.
pub fn run(args: &SchemaBudgetArgs, config: &crate::config::Config) -> anyhow::Result<()> {
    let listed = crate::mcp_server::advertised_tools(config);
    let tools = listed
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("tools/list did not return a tools array"))?;
    // A sweep over an empty or short catalogue satisfies every rule without measuring anything,
    // which is exactly the false clean bill this package exists to make impossible.
    let expected = EMITTED_ARTIFACT_CEILINGS.len();
    if tools.len() < expected {
        anyhow::bail!(
            "the catalogue advertises {} tools; expected {expected} so every limit is swept over \
             every tool. A budget report on a short catalogue cannot distinguish a passing tool \
             from an unmeasured one.",
            tools.len()
        );
    }
    if args.catalogue {
        println!("{}", serde_json::to_string_pretty(&listed)?);
        return Ok(());
    }
    if args.ledger {
        println!("{}", serde_json::to_string_pretty(&stage_ledger(tools))?);
        return Ok(());
    }
    if !report(&evaluate(tools, args.strict), args.strict) {
        anyhow::bail!("the emitted MCP catalogue breaches an enforced client limit");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The filter is load-bearing for every budget assertion, so pin it on its own.
    ///
    /// Without this, the 62-versus-9,954 collapse is invisible: a schema reduced to `{}` fits
    /// every budget, and the sweep reports a clean bill on the exact defect it exists to catch.
    #[test]
    fn codex_visible_schema_keeps_property_names() {
        let schema = json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Text to match." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 9223372036854775807i64 },
            },
        });
        let visible = codex_visible_schema(&schema);
        let properties = visible["properties"].as_object().expect("properties survive");
        assert_eq!(properties.len(), 2, "{visible}");
        assert_eq!(properties["query"]["description"], json!("Text to match."));
        assert!(properties["limit"].get("minimum").is_none(), "{visible}");
        assert!(properties["limit"].get("maximum").is_none(), "{visible}");
    }

    /// Codex models `enum` and does not model `const`. That asymmetry is why replacing the
    /// discriminators is worth doing and why it costs bytes in the one channel that binds.
    #[test]
    fn codex_visible_schema_keeps_enum_and_drops_const() {
        assert_eq!(codex_visible_schema(&json!({ "const": "max_chars" })), json!({}));
        assert_eq!(
            codex_visible_schema(&json!({ "enum": ["max_chars"] })),
            json!({ "enum": ["max_chars"] })
        );
    }

    #[test]
    fn codex_visible_schema_recurses_through_every_modeled_container() {
        let schema = json!({
            "$defs": { "View": { "type": "object", "default": 1 } },
            "items": { "type": "string", "minLength": 2 },
            "oneOf": [{ "type": "null", "default": null }],
            "additionalProperties": { "type": "string", "minLength": 1 },
        });
        let visible = codex_visible_schema(&schema);
        assert_eq!(visible["$defs"]["View"], json!({ "type": "object" }));
        assert_eq!(visible["items"], json!({ "type": "string" }));
        assert_eq!(visible["oneOf"], json!([{ "type": "null" }]));
        assert_eq!(visible["additionalProperties"], json!({ "type": "string" }));
        assert_eq!(
            codex_visible_schema(&json!({ "additionalProperties": false }))["additionalProperties"],
            json!(false)
        );
    }

    #[test]
    fn depth_counts_containers_and_stops_at_scalars() {
        assert_eq!(schema_depth(&json!({})), 1);
        assert_eq!(schema_depth(&json!({ "a": 1 })), 1);
        assert_eq!(schema_depth(&json!({ "a": { "b": 1 } })), 2);
        // An enum array is one level; the string inside it is not a validator frame.
        assert_eq!(schema_depth(&json!({ "enum": ["x"] })), 2);
    }

    #[test]
    fn every_limit_states_its_origin() {
        for limit in all_limits() {
            for (field, value) in [
                ("rationale", limit.rationale),
                ("platform", limit.platform),
                ("raise_when", limit.raise_when),
                ("lower_when", limit.lower_when),
            ] {
                assert!(
                    !value.trim().is_empty(),
                    "{} states no {field}; a bound without its provenance gets re-derived by \
                     guesswork the next time a client ships",
                    limit.name
                );
            }
            if !limit.enforced {
                assert!(
                    !limit.enforced_by.is_empty(),
                    "{} is unenforced and names no package that will enforce it, so the ratchet \
                     has no way to tighten",
                    limit.name
                );
            }
        }
    }

    #[test]
    fn limit_names_are_unique() {
        let mut names: Vec<&str> = all_limits().map(|limit| limit.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate limit rows: {names:?}");
    }

    #[test]
    fn a_silent_row_says_the_breach_is_invisible_and_an_announced_one_does_not() {
        for limit in all_limits() {
            let message = describe_breach(limit, limit.budget + 1);
            match limit.failure_mode {
                FailureMode::Silent => assert!(
                    message.contains("silent"),
                    "{} hides that its breach is invisible: {message}",
                    limit.name
                ),
                FailureMode::Announced => assert!(
                    !message.contains("silent"),
                    "{} claims silence for a client that announces: {message}",
                    limit.name
                ),
                _ => {}
            }
        }
    }

    fn minimal_tool() -> Value {
        json!({
            "name": "search_messages",
            "description": "Search indexed AI-session transcript messages.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": { "query": { "type": "string", "description": "Text to match." } },
            },
            "outputSchema": { "type": "object", "additionalProperties": false, "properties": {} },
        })
    }

    fn failed_rules(tool: Value) -> Vec<&'static str> {
        evaluate(&[tool], true)
            .into_iter()
            .filter(|finding| finding.status == Status::Fail)
            .map(|finding| finding.limit.name)
            .collect()
    }

    #[test]
    fn a_clean_tool_trips_no_rule() {
        assert_eq!(failed_rules(minimal_tool()), Vec::<&str>::new());
    }

    /// Every row fires on a tool built to breach exactly that row and nothing else.
    #[test]
    fn each_rule_catches_its_own_defect() {
        let cases: Vec<(&str, Value)> = vec![
            ("claude-code-tool-description-chars", {
                let mut tool = minimal_tool();
                tool["description"] = json!("x".repeat(2_049));
                tool
            }),
            ("vscode-parameter-description-chars", {
                let mut tool = minimal_tool();
                tool["inputSchema"]["properties"]["query"]["description"] = json!("y".repeat(1_025));
                tool
            }),
            ("vscode-no-root-combinator", {
                let mut tool = minimal_tool();
                tool["inputSchema"]["oneOf"] = json!([{ "required": ["query"] }]);
                tool
            }),
            ("anthropic-tool-name-charset", {
                let mut tool = minimal_tool();
                tool["name"] = json!("search.messages");
                tool
            }),
            ("style-no-aise-vendor-keys", {
                let mut tool = minimal_tool();
                tool["inputSchema"]["properties"]["query"]["x-aise-parameters"] = json!(["query"]);
                tool
            }),
            ("style-no-input-const", {
                let mut tool = minimal_tool();
                tool["inputSchema"]["properties"]["kind"] = json!({ "const": "max_chars" });
                tool
            }),
            ("style-no-fake-integer-maxima", {
                let mut tool = minimal_tool();
                tool["inputSchema"]["properties"]["limit"] =
                    json!({ "type": "integer", "maximum": 9223372036854775807i64 });
                tool
            }),
            ("style-local-refs-only", {
                let mut tool = minimal_tool();
                tool["outputSchema"] = json!({ "$ref": "https://example.invalid/s.json#/$defs/T" });
                tool
            }),
            ("style-reachable-definitions", {
                let mut tool = minimal_tool();
                tool["outputSchema"]["$defs"] = json!({ "Orphan": { "type": "string" } });
                tool
            }),
            ("mcp-output-schema-depth", {
                let mut tool = minimal_tool();
                let mut deep = json!({ "type": "string" });
                for _ in 0..12 {
                    deep = json!({ "type": "object", "properties": { "next": deep } });
                }
                tool["outputSchema"] = deep;
                tool
            }),
        ];
        for (rule, tool) in cases {
            let failed = failed_rules(tool);
            assert!(failed.contains(&rule), "{rule} did not fire; failures were {failed:?}");
        }
    }

    /// The ratchet: an unenforced breach is reported with its measurement, never failed.
    #[test]
    fn an_unenforced_breach_is_pending_rather_than_failing() {
        // Any rule still carrying `enforced: false` will do; depth is the one a synthetic tool
        // can breach without touching anything else.
        let rule = all_limits()
            .find(|limit| !limit.enforced && limit.applies_to != AppliesTo::Response)
            .expect("the ratchet has rules left to tighten");
        let mut tool = minimal_tool();
        let mut deep = json!({ "type": "string" });
        for _ in 0..24 {
            deep = json!({ "type": "object", "properties": { "next": deep } });
        }
        tool["outputSchema"] = deep;
        tool["inputSchema"]["properties"]["padding"] = json!({
            "type": "string",
            "description": "x".repeat(6_000),
        });

        let lenient = evaluate(std::slice::from_ref(&tool), false);
        let pending: Vec<&Finding> = lenient
            .iter()
            .filter(|finding| finding.limit.name == rule.name)
            .collect();
        assert_eq!(pending.len(), 1, "{}", rule.name);
        assert_eq!(pending[0].status, Status::Pending, "{}", rule.name);
        assert!(
            pending[0].measured > rule.budget,
            "a pending rule must publish the measurement it is waiting on"
        );
        assert!(
            lenient
                .iter()
                .all(|finding| finding.limit.name != rule.name
                    || finding.status != Status::Fail),
            "an unenforced rule failed the ratchet"
        );

        let strict = evaluate(&[tool], true);
        assert!(strict
            .iter()
            .any(|finding| finding.limit.name == rule.name && finding.status == Status::Fail));
    }

    #[test]
    fn the_ledger_reports_every_stage_and_marks_the_unobservable_ones() {
        let ledger = stage_ledger(&[minimal_tool()]);
        let stages = ledger["stages"].as_object().expect("stages");
        for stage in LEDGER_STAGES {
            assert!(stages.contains_key(stage), "ledger is missing {stage}");
        }
        for stage in RESPONSE_LEDGER_STAGES {
            assert_eq!(
                stages[stage]["status"], "fixture-required",
                "{stage} must carry a status rather than a zero that reads as a saving"
            );
            assert!(stages[stage].get("bytes").is_none(), "{stage} reported bytes it cannot observe");
        }
        // Wire bytes and model-facing bytes are different stages with different owners.
        assert!(
            stages["input_schema_wire"]["bytes"].as_u64()
                >= stages["input_schema_client_normalized"]["bytes"].as_u64()
        );
        assert_eq!(stages["output_schema_declaration"]["model_facing_verified"], json!(false));
        assert_eq!(stages["raw_catalogue"]["artifact"], "tools[] array");
        assert_eq!(
            stages["jsonrpc_catalogue_envelope"]["artifact"],
            "tools/list JSON-RPC message"
        );
    }
}
