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
//! Two modes. A row whose defect this repository has not fixed yet reports a warning, and the
//! commit that fixes it sets `fail_on_breach: true` in the same change, so reverting the fix
//! reverts the gate with it. `strict` makes every measured breach fail until the whole schema
//! sequence has landed.

use clap::Args;
use serde_json::{json, Map, Value};

/// Emitted as a `maximum`, `i64::MAX` rejects nothing a caller could send, so it describes the
/// storage type rather than the parameter.
pub const FAKE_INTEGER_MAXIMUM: i64 = i64::MAX;

/// Exactly the keys Codex's `JsonSchema` models (`codex-rs/tools/src/json_schema.rs:41-74`).
/// Anything else is dropped by deserialization before a model sees it, and because the budget is
/// measured after that round trip, it is not even charged for.
const CODEX_SCALAR_KEYS: [&str; 6] = [
    "$ref",
    "type",
    "description",
    "encrypted",
    "enum",
    "required",
];
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
    /// Inside the limit but past its notice line; this is not a client-limit warning.
    Notice,
    /// Over a limit configured to report a warning instead of failing.
    Warning,
    Fail,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Notice => "NOTICE",
            Status::Warning => "WARNING",
            Status::Fail => "FAIL",
        }
    }
}

/// One (client, artifact, limit) triple, swept over every tool the catalogue advertises.
#[derive(Debug, Clone, Copy)]
pub struct HarnessLimit {
    pub name: &'static str,
    /// The external client, protocol, or repository policy that owns this rule.
    pub authority: &'static str,
    pub artifact: &'static str,
    pub budget: usize,
    pub unit: &'static str,
    pub failure_mode: FailureMode,
    pub applies_to: AppliesTo,
    /// True when an over-limit measurement fails this repository's default gate.
    pub fail_on_breach: bool,
    /// Descriptive subsystem that owns a future change to make a non-blocking breach fail.
    pub planned_enforcement: &'static str,
    /// A measured value strictly above this line receives a non-failing notice; the ceiling remains inclusive.
    pub notice_at: Option<usize>,
    /// True when crossing the budget is a warning rather than a failure.
    pub warning_only: bool,
    /// True when the budget is a client constant an operator may track between releases via
    /// `[mcp.client_limits]`. False for structural and style rows, where the number is not a
    /// moving cap and an override would silence the checker without changing what the client
    /// rejects; configuration refuses those by name and [`resolved_budget`] ignores them.
    pub overridable: bool,
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
        authority: "Codex 0.146.0",
        artifact: "Codex-normalized inputSchema",
        budget: 5_000,
        unit: "bytes",
        failure_mode: FailureMode::Silent,
        applies_to: AppliesTo::InputSchema,
        fail_on_breach: true,
        planned_enforcement: "",
        notice_at: None,
        warning_only: false,
        overridable: true,
        rationale: "MAX_COMPACT_TOOL_SCHEMA_BYTES. Past it Codex runs strip_schema_descriptions at \
                    every depth and hands the model a schema whose parameters have names and types \
                    and nothing else. No marker is emitted, nothing is logged, and no file keeps \
                    the deleted text.",
        platform: "Codex only, verified 2026-08-04 against codex-cli 0.146.0. The constant is in \
                   codex-rs/tools/src/json_schema.rs:222, whose content hashes to git blob \
                   7c9edd08e167626cc524962884baa3ad626247a0 at every release tag from \
                   rust-v0.145.0-alpha.18 through rust-v0.146.0-alpha.13, the newest tagged \
                   release, so the file this figure was read from is the file those releases \
                   ship rather than an untagged commit that might not be. No other measured \
                   client bounds schema bytes.",
        raise_when: "Codex raises its own limit. It already moved 4,000 to 5,000 in b6f9aee16d \
                     (2026-07-08), so this is a moving target rather than a floor.",
        lower_when: "Codex lowers it, or a second client is found that bounds schema bytes tighter.",
    },
    HarnessLimit {
        name: "codex-input-schema-margin",
        authority: "Codex 0.146.0",
        artifact: "Codex-normalized inputSchema",
        budget: 4_750,
        unit: "bytes",
        failure_mode: FailureMode::NoClientEffect,
        applies_to: AppliesTo::InputSchema,
        fail_on_breach: true,
        planned_enforcement: "",
        notice_at: None,
        warning_only: true,
        overridable: true,
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
        authority: "Claude Code 2.1.220",
        artifact: "tool.description",
        budget: 2_048,
        unit: "UTF-16 units",
        failure_mode: FailureMode::Silent,
        applies_to: AppliesTo::ToolDescription,
        fail_on_breach: true,
        planned_enforcement: "",
        notice_at: Some(1_986),
        warning_only: false,
        overridable: true,
        rationale: "MAX_MCP_DESCRIPTION_LENGTH. Past it Claude Code appends a truncation marker to \
                    what it keeps, so text beyond this point is paid for and teaches nobody. The \
                    notice threshold sits 62 characters below, the mean length of one conflict rule: the \
                    nine validator rules render verbatim into this channel and are the only part \
                    that grows without anyone editing a description.",
        platform: "Claude Code only; Codex does not cap this channel. services/mcp/client.ts:218, \
                   confirmed in 2.1.88 source and read again from the 2.1.220 binary. JavaScript \
                   length counts UTF-16 code units, so the checker counts the same: a \
                   supplementary-plane character costs two.",
        raise_when: "Claude Code raises MAX_MCP_DESCRIPTION_LENGTH.",
        lower_when: "A second client is found that caps this channel lower.",
    },
    HarnessLimit {
        name: "vscode-parameter-description-chars",
        authority: "VS Code / Copilot, GPT-4o family",
        artifact: "schema-internal description",
        budget: 1_024,
        unit: "UTF-16 units",
        failure_mode: FailureMode::Silent,
        applies_to: AppliesTo::ParameterDescription,
        fail_on_breach: true,
        planned_enforcement: "",
        notice_at: None,
        warning_only: false,
        overridable: true,
        rationale: "gpt4oMaxStringLength. VS Code truncates every description inside the schema at \
                    this length before sending, so the tail costs wire bytes and reaches no model.",
        platform: "VS Code / Copilot, GPT-4o family only; it does not apply to the tool's own \
                   description. toolSchemaNormalizer.ts:79-87, fetched 2026-08-02.",
        raise_when: "VS Code raises the constant or retires that family from the normalizer.",
        lower_when: "Another family in the same normalizer is found with a tighter bound.",
    },
    HarnessLimit {
        name: "vscode-no-root-combinator",
        authority: "VS Code / Copilot, all model families",
        artifact: "inputSchema root keywords",
        budget: 0,
        unit: "root combinators",
        failure_mode: FailureMode::Rejected,
        applies_to: AppliesTo::InputSchema,
        fail_on_breach: true,
        planned_enforcement: "",
        notice_at: None,
        warning_only: false,
        overridable: false,
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
        authority: "MCP specification",
        artifact: "outputSchema nesting depth",
        budget: 10,
        unit: "levels",
        failure_mode: FailureMode::Rejected,
        applies_to: AppliesTo::OutputSchema,
        fail_on_breach: true,
        planned_enforcement: "",
        notice_at: None,
        warning_only: false,
        overridable: false,
        rationale: "10 is this repository's own guard, not a number the MCP specification states. \
                    The specification tells clients to apply a maximum schema depth to prevent a \
                    denial-of-service vector and deliberately prescribes no value, so a report \
                    citing it as an MCP limit is citing something that does not exist. Was 21, 18, \
                    12, 9, 9 and three at 8; now 10, 10, 9, 9 and four at 8, by naming shapes a \
                    reader recognises in the three documents that paid for it. A blanket pass over \
                    every object was measured first and rejected: it reached similar numbers while \
                    naming two dozen shapes mechanically, without the descriptions the emitted \
                    schema policy requires, and every consumer navigating \
                    by path then resolves pointers to shapes with no names. Note what the count \
                    measures: document nesting, where a properties map is itself a level, so the \
                    seven levels of response data under search_messages measured as eighteen.",
        platform: "The specification, not any one client; no measured client enforces a depth bound \
                   today, so this is conformance headroom. Counted as containers only with the root \
                   at 1: a scalar inside an enum array is not a validator recursion level.",
        raise_when: "A legitimate schema needs more nesting than extraction can flatten. Name a \
                     repeated type first; a $ref is a leaf at its point of use.",
        lower_when: "A client is found that enforces a tighter bound.",
    },
    HarnessLimit {
        name: "mcp-output-schema-point-of-use-depth",
        authority: "this repository's schema policy",
        artifact: "outputSchema depth along a use path, with $ref as a leaf and $defs excluded",
        budget: 6,
        unit: "levels",
        failure_mode: FailureMode::NoClientEffect,
        applies_to: AppliesTo::OutputSchema,
        fail_on_breach: true,
        planned_enforcement: "",
        notice_at: Some(5),
        warning_only: false,
        overridable: false,
        rationale: "The resolved-depth guard can be satisfied by moving nesting into $defs without \
                    making anything easier to read, because a named shape is still as deep as it \
                    was. This measures what a reader actually walks: a $ref ends the path, and the \
                    $defs table is not on any path. 5 is the shape the response really has -- root, \
                    then metadata or results, then an item, then a field -- and 6 is the ceiling \
                    for the branches that legitimately carry one more level, such as a view inside \
                    a presentation inside a neighbouring message.",
        platform: "This repository's own readability guard. No client measures it, so it constrains \
                   nothing external; it exists so the resolved-depth number cannot be bought by \
                   naming shapes nobody would recognise.",
        raise_when: "A response genuinely gains a level of structure. Name the path and the reason \
                     in the requirements document first; do not raise it to make a test green.",
        lower_when: "The response shape is flattened and the lower figure holds across a release.",
    },
    HarnessLimit {
        name: "style-named-definitions-are-described",
        authority: "this repository's schema policy",
        artifact: "$defs entries without a description",
        budget: 0,
        unit: "undescribed definitions",
        failure_mode: FailureMode::NoClientEffect,
        applies_to: AppliesTo::OutputSchema,
        fail_on_breach: true,
        planned_enforcement: "",
        notice_at: None,
        warning_only: false,
        overridable: false,
        rationale: "A named shape with no description trades a deep document for an opaque one: \
                    the reader who follows the reference arrives somewhere whose name is the only \
                    thing telling them what it holds. This is the specific failure of a mechanical \
                    extraction pass, which is why extraction here is by hand and why this is \
                    checked rather than assumed.",
        platform: "This repository's own guard: every named type has a description.",
        raise_when: "Never. A shape worth naming is worth one sentence.",
        lower_when: "Never; zero is already the floor.",
    },
    HarnessLimit {
        name: "mcp-output-schema-subschemas",
        authority: "MCP specification",
        artifact: "outputSchema subschema count",
        budget: 250,
        unit: "subschemas",
        failure_mode: FailureMode::Rejected,
        applies_to: AppliesTo::OutputSchema,
        fail_on_breach: false,
        planned_enforcement: "output-schema structure and readability gate",
        notice_at: None,
        warning_only: false,
        overridable: false,
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
        authority: "Anthropic API and the MCP SDK",
        artifact: "tool name",
        budget: 64,
        unit: "characters matching ^[a-zA-Z0-9_-]{1,64}$",
        failure_mode: FailureMode::Rejected,
        applies_to: AppliesTo::ToolName,
        fail_on_breach: true,
        planned_enforcement: "",
        notice_at: None,
        warning_only: false,
        overridable: true,
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
        authority: "Codex 0.146.0",
        artifact: "serialized CallToolResult",
        budget: 48_000,
        unit: "characters",
        failure_mode: FailureMode::Silent,
        applies_to: AppliesTo::Response,
        fail_on_breach: false,
        planned_enforcement: "bounded response delivery gate",
        notice_at: None,
        warning_only: false,
        overridable: true,
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
        authority: "Claude Code 2.1.220",
        artifact: "tool-result tokens",
        budget: 25_000,
        unit: "tokens",
        failure_mode: FailureMode::Announced,
        applies_to: AppliesTo::Response,
        fail_on_breach: false,
        planned_enforcement: "bounded response delivery gate",
        notice_at: None,
        warning_only: false,
        overridable: true,
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
        authority: "Gemini CLI (legacy registration)",
        artifact: "tool-result characters",
        budget: 40_000,
        unit: "characters",
        failure_mode: FailureMode::Announced,
        applies_to: AppliesTo::Response,
        fail_on_breach: false,
        planned_enforcement: "bounded response delivery gate",
        notice_at: None,
        warning_only: false,
        overridable: true,
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
        authority: "this repository's schema policy: no project-only vendor keys",
        artifact: "x-aise-* keys anywhere in a tool",
        budget: 0,
        unit: "keys",
        failure_mode: FailureMode::NoClientEffect,
        applies_to: AppliesTo::InputSchema,
        fail_on_breach: true,
        planned_enforcement: "",
        notice_at: None,
        warning_only: false,
        overridable: false,
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
        authority: "this repository's schema policy: use model-visible constraints",
        artifact: "const in an inputSchema",
        budget: 0,
        unit: "sites",
        failure_mode: FailureMode::NoClientEffect,
        applies_to: AppliesTo::InputSchema,
        fail_on_breach: true,
        planned_enforcement: "",
        notice_at: None,
        warning_only: false,
        overridable: false,
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
        authority: "this repository's schema policy: every bound states its origin",
        artifact: "maximum equal to i64::MAX",
        budget: 0,
        unit: "sites",
        failure_mode: FailureMode::NoClientEffect,
        applies_to: AppliesTo::InputSchema,
        fail_on_breach: true,
        planned_enforcement: "",
        notice_at: None,
        warning_only: false,
        overridable: false,
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
        authority: "this repository's schema policy: every reference is local",
        artifact: "$ref targets",
        budget: 0,
        unit: "non-local refs",
        failure_mode: FailureMode::Rejected,
        applies_to: AppliesTo::OutputSchema,
        fail_on_breach: true,
        planned_enforcement: "",
        notice_at: None,
        warning_only: false,
        overridable: false,
        rationale: "MCP states that implementations MUST NOT automatically dereference a $ref that \
                    resolves to a network URI, so a non-local ref is unresolvable at the client and \
                    should be rejected rather than treated as permissive.",
        platform: "MCP 2026-07-28, basic section on $ref resolution.",
        raise_when: "Never.",
        lower_when: "Never; zero is already the floor.",
    },
    HarnessLimit {
        name: "style-reachable-definitions",
        authority: "this repository's schema policy: extract only useful shapes",
        artifact: "unreachable $defs entries",
        budget: 0,
        unit: "entries",
        failure_mode: FailureMode::NoClientEffect,
        applies_to: AppliesTo::OutputSchema,
        fail_on_breach: true,
        planned_enforcement: "",
        notice_at: None,
        warning_only: false,
        overridable: false,
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

/// Per-tool ceilings pinning each emitted artifact to today's measurement.
///
/// These are measurements, not targets: a ratchet so no change grows an artifact unnoticed.
/// Re-pin an entry in the same commit as the change that moves it, in either direction — down
/// when prose is trimmed, up only with the restored documentation that justifies it, as when
/// the four session tools took back the parameter text a deduplication pass had discarded. A
/// tool with no entry is a tool nothing is watching, so [`ceiling_for`] refuses to guess one.
///
/// Measured in bytes as Codex counts them, after its sanitize pass — the unit the client
/// budget is denominated in. That distinction is not pedantry: `search_sessions` and
/// `list_sessions` each carry one em dash, three UTF-8 bytes that a measurement escaping
/// non-ASCII reports as the six characters `—`. Every earlier figure for those two tools was
/// three high for exactly that reason, which is one reason this check lives in the language
/// that counts what the client counts.
///
/// Columns: tool, Codex-measured `inputSchema` bytes, `outputSchema` depth.
pub const EMITTED_ARTIFACT_CEILINGS: [(&str, usize, usize); 8] = [
    // 4_645 -> 4_706: `field_view`/`match_view` now state their omission value as the JSON a
    // caller sends (`{kind:max_chars,max_chars:220}`) because Claude Code drops `oneOf` from the
    // schema it shows the model, and `query`'s omission reads "without it every message the
    // filters allow is eligible". 294 bytes remain below Codex's 5,000-byte description-deletion
    // cliff.
    ("search_messages", 4_706, 10),
    // 4_747 -> 4_721: `field_view` states its omission value as the JSON a caller sends, `limit`
    // and `all_results` say "omit for" like every other property so the tool description's
    // omitted-values sentence can read them back. 279 bytes of headroom remain before Codex's
    // 5,000-byte cliff, past which it deletes every parameter description with no marker to the
    // model.
    ("run_skill_capability", 4_721, 9),
    ("get_session", 4_064, 10),
    // 4_250 -> 4_306: `query` now states its matching model (case-insensitive substring of the
    // whole query and of each word, fuzzy on title and paths, every-word bonus, no quote or
    // boolean operators) instead of "keywords, a phrase, or a code snippet". 694 bytes remain
    // below Codex's 5,000-byte cliff.
    ("search_sessions", 4_306, 8),
    ("list_sessions", 3_892, 8),
    ("query_session_index", 1_724, 8),
    ("get_resume_command", 374, 8),
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
    /// The budget actually applied: what the operator configured, or the measured default.
    /// Reported instead of `limit.budget` so a run says which number it judged against.
    pub budget: usize,
    /// The resolved notice threshold; notice applies only when measured is strictly above it.
    pub notice_at: Option<usize>,
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

/// The type Codex writes back for a schema that declares none, in its own precedence order.
///
/// `sanitize_json_schema` runs this ladder at `json_schema.rs:516-536` and hands the result to
/// `write_schema_types`, so a schema that omits a type is measured carrying the one Codex infers.
fn codex_inferred_type(map: &Map<String, Value>) -> Option<&'static str> {
    if map.contains_key("properties")
        || map.contains_key("required")
        || map.contains_key("additionalProperties")
    {
        Some("object")
    } else if map.contains_key("items") || map.contains_key("prefixItems") {
        Some("array")
    } else if map.contains_key("enum") || map.contains_key("format") {
        Some("string")
    } else if [
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
    ]
    .iter()
    .any(|key| map.contains_key(*key))
    {
        Some("number")
    } else {
        None
    }
}

/// Codex's sanitize pass, which runs *before* the budget is measured and can grow the schema.
///
/// `parse_tool_input_schema` calls `prepare_tool_input_schema` -- sanitize, then prune -- and only
/// then `compact_large_tool_schema`, whose `compact_normalized_schema_len` is the figure compared
/// against the 5,000-byte budget (`json_schema.rs:189-192, 229-260`). Normalizing without
/// sanitizing therefore measures an artifact the client never sees. Three of its rules change the
/// count: a missing type is inferred and written back, `const` becomes a single-value `enum`, and
/// an object or array type gains the child the deserializer requires.
///
/// Mirrors `sanitize_json_schema` at `json_schema.rs:466-542`, verified 2026-08-04 against
/// checkout 6751b54cae by compiling that file verbatim and comparing every advertised tool.
pub fn codex_sanitized_schema(value: &Value) -> Value {
    match value {
        // The boolean schema form, which Codex coerces to an accept-all string.
        Value::Bool(_) => json!({ "type": "string" }),
        Value::Array(list) => Value::Array(list.iter().map(codex_sanitized_schema).collect()),
        Value::Object(object) => {
            let mut map = object.clone();
            for key in CODEX_MAP_KEYS {
                if let Some(Value::Object(inner)) = map.get(key).cloned() {
                    let sanitized = inner
                        .iter()
                        .map(|(name, schema)| (name.clone(), codex_sanitized_schema(schema)))
                        .collect();
                    map.insert(key.to_owned(), Value::Object(sanitized));
                } else if key != "properties" && map.contains_key(key) {
                    // A definition table that is not an object is dropped rather than kept.
                    map.remove(key);
                }
            }
            for key in ["items", "prefixItems"] {
                if let Some(inner) = map.get(key).cloned() {
                    map.insert(key.to_owned(), codex_sanitized_schema(&inner));
                }
            }
            if let Some(extra) = map.get("additionalProperties").cloned() {
                if !matches!(extra, Value::Bool(_)) {
                    map.insert(
                        "additionalProperties".to_owned(),
                        codex_sanitized_schema(&extra),
                    );
                }
            }
            for key in CODEX_LIST_KEYS {
                if let Some(inner) = map.get(key).cloned() {
                    map.insert(key.to_owned(), codex_sanitized_schema(&inner));
                }
            }
            if let Some(constant) = map.remove("const") {
                map.insert("enum".to_owned(), Value::Array(vec![constant]));
            }

            let declared: Vec<String> = match map.get("type") {
                Some(Value::String(name)) => vec![name.clone()],
                Some(Value::Array(names)) => names
                    .iter()
                    .filter_map(|name| name.as_str().map(str::to_owned))
                    .collect(),
                _ => Vec::new(),
            };
            if declared.is_empty() {
                // A reference or a composition ends the ladder before a type is written.
                if map.contains_key("$ref") || CODEX_LIST_KEYS.iter().any(|k| map.contains_key(*k))
                {
                    return Value::Object(map);
                }
                match codex_inferred_type(&map) {
                    Some(inferred) => {
                        map.insert("type".to_owned(), Value::String(inferred.to_owned()));
                    }
                    // No recognized hint: Codex clears the object entirely, so whatever it held
                    // costs nothing. A description alone is measured as zero bytes, not as prose.
                    None => return json!({}),
                }
            }

            let types: Vec<String> = map
                .get("type")
                .map(|value| match value {
                    Value::String(name) => vec![name.clone()],
                    Value::Array(names) => names
                        .iter()
                        .filter_map(|name| name.as_str().map(str::to_owned))
                        .collect(),
                    _ => Vec::new(),
                })
                .unwrap_or_default();
            let types: Vec<&str> = types.iter().map(String::as_str).collect();
            if types.contains(&"object") && !map.contains_key("properties") {
                map.insert("properties".to_owned(), Value::Object(Map::new()));
            }
            if types.contains(&"array") && !map.contains_key("items") {
                map.insert("items".to_owned(), json!({ "type": "string" }));
            }
            Value::Object(map)
        }
        other => other.clone(),
    }
}

/// The artifact Codex actually measures: sanitized, then reduced to the keys it models.
pub fn codex_measured_schema(value: &Value) -> Value {
    codex_visible_schema(&codex_sanitized_schema(value))
}

/// Byte figure the Codex input-schema rows compare against their budget.
pub fn codex_measured_bytes(value: &Value) -> usize {
    compact_len(&codex_measured_schema(value))
}

/// Whether Codex hands this schema's descriptions to the model, or deletes them first.
///
/// The byte rows above measure a *proxy*. What a caller loses when the budget is breached is the
/// prose, and only the prose: `compact_large_tool_schema` re-checks the budget **before** each
/// pass (`json_schema.rs:229-236`), so a schema over the limit enters
/// `strip_schema_descriptions`, and that pass removes `description` from every reachable node
/// (`json_schema.rs`, the pass walks `for_each_schema_child_mut` with
/// `DefinitionTraversal::Include`). There is no partial outcome: over budget is zero descriptions,
/// under budget is all of them.
///
/// Measuring the prose directly rather than inferring it from a byte count is what keeps the two
/// from drifting. A limit is a number this repository copied from another project and can go
/// stale; "the model was shown what we wrote" is the property the number exists to protect, and it
/// is the one worth asserting.
///
/// Only pass 1 is modeled, because only pass 1 decides this question. Passes 2 to 4 --
/// `drop_schema_definitions`, `collapse_deep_schema_objects_from_root`,
/// `prune_schema_compositions` -- run after the descriptions are already gone and change what else
/// survives, which nothing here asks about.
// ponytail: models pass 1 only; extend to the later passes if a caller needs to know which
// structure survives a breach rather than whether the prose does.
pub fn codex_strips_descriptions(value: &Value) -> bool {
    codex_measured_bytes(value) > CODEX_COMPACT_TOOL_SCHEMA_BYTES
}

/// `MAX_COMPACT_TOOL_SCHEMA_BYTES`, `codex-rs/tools/src/json_schema.rs:222`.
///
/// Held beside the sanitize model rather than read back out of the limit table, because the table
/// is configurable per registration and this is the upstream constant the model is faithful to.
const CODEX_COMPACT_TOOL_SCHEMA_BYTES: usize = 5_000;

/// Byte length of the compact serialization the client measures.
pub fn compact_len(value: &Value) -> usize {
    serde_json::to_string(value)
        .map(|text| text.len())
        .unwrap_or(0)
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

/// The deepest path through a schema, as the JSON Pointer a reader would follow to reach it.
///
/// A depth figure alone says a schema is too deep and nothing about why, which is how "21" sat in
/// a report for a round without anyone being able to act on it. The pointer names the chain, so
/// the fix is a decision about those specific properties rather than a search.
pub fn deepest_pointer(value: &Value) -> String {
    fn walk(value: &Value, path: &mut Vec<String>, best: &mut (usize, Vec<String>)) {
        let depth = path.len();
        if depth > best.0 {
            *best = (depth, path.clone());
        }
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    path.push(key.clone());
                    walk(child, path, best);
                    path.pop();
                }
            }
            Value::Array(list) => {
                for (index, child) in list.iter().enumerate() {
                    path.push(index.to_string());
                    walk(child, path, best);
                    path.pop();
                }
            }
            _ => {}
        }
    }

    let mut best = (0, Vec::new());
    walk(value, &mut Vec::new(), &mut best);
    if best.1.is_empty() {
        return "/".to_owned();
    }
    format!(
        "/{}",
        best.1
            .iter()
            .map(|segment| segment.replace('~', "~0").replace('/', "~1"))
            .collect::<Vec<_>>()
            .join("/")
    )
}

/// Depth as a reader navigating the document experiences it, rather than as a validator does.
///
/// Two rules differ from [`schema_depth`], and both follow from what a `$ref` is at its point of
/// use: it is a leaf, so the walk stops there instead of continuing into the shape it names; and
/// the `$defs` table is not on any use path, so it is excluded rather than counted as though a
/// reader passed through it to reach a property.
///
/// This is the figure that answers "how deep is the response shape", which the resolved depth
/// cannot: naming a shape moves nesting out of the use path without removing it from the
/// document, so a resolved figure improves while the reader's experience is unchanged. Keeping
/// both means neither claim can be made by moving bytes around.
pub fn point_of_use_depth(value: &Value) -> usize {
    /// Levels of data below this schema node, counting only steps a value actually takes.
    ///
    /// Descending into a property or an array element is a level, because the value nests there.
    /// The `properties` map, an `items` wrapper, a `type` union and an `enum` list are not: they
    /// are how JSON Schema spells a level, not another one. That distinction is the whole reason
    /// this metric exists beside [`schema_depth`], which counts every container and therefore
    /// reports roughly twice the nesting a response really has.
    fn below(schema: &Value) -> usize {
        let Some(object) = schema.as_object() else {
            return 0;
        };
        // A reference ends the path: the reader follows a name they recognise, and the shape it
        // names is measured on its own from the definition table.
        if object.contains_key("$ref") {
            return 0;
        }
        let mut deepest = 0;
        if let Some(Value::Object(properties)) = object.get("properties") {
            for property in properties.values() {
                deepest = deepest.max(1 + below(property));
            }
        }
        if let Some(items) = object.get("items") {
            deepest = deepest.max(1 + below(items));
        }
        // A branch is an alternative at the same level, not a level of its own.
        for combinator in ["oneOf", "anyOf", "allOf"] {
            if let Some(Value::Array(variants)) = object.get(combinator) {
                for variant in variants {
                    deepest = deepest.max(below(variant));
                }
            }
        }
        deepest
    }

    // The document root is the first level, and every named shape is measured from its own root
    // because that is where a reader arriving by reference starts.
    let mut deepest = 1 + below(value);
    for table in ["$defs", "definitions"] {
        if let Some(Value::Object(definitions)) = value.get(table) {
            for definition in definitions.values() {
                deepest = deepest.max(1 + below(definition));
            }
        }
    }
    deepest
}

/// The document without its definition table, so a pointer names a path a reader actually walks.
fn without_definitions(value: &Value) -> Value {
    let Some(object) = value.as_object() else {
        return value.clone();
    };
    let kept: Map<String, Value> = object
        .iter()
        .filter(|(key, _)| key.as_str() != "$defs" && key.as_str() != "definitions")
        .map(|(key, child)| (key.clone(), child.clone()))
        .collect();
    Value::Object(kept)
}

/// Every named shape that carries no description.
///
/// Naming a shape and leaving it undescribed trades a deep document for an opaque one: the reader
/// who follows the reference arrives somewhere whose name is the only thing telling them what it
/// holds. That is the failure mode of a mechanical extraction pass, so it is checked rather than
/// trusted.
pub fn undescribed_definitions(value: &Value) -> Vec<String> {
    let mut missing = Vec::new();
    for table in ["$defs", "definitions"] {
        if let Some(Value::Object(defs)) = value.get(table) {
            for (name, schema) in defs {
                if schema.get("description").and_then(Value::as_str).is_none() {
                    missing.push(name.clone());
                }
            }
        }
    }
    missing.sort();
    missing
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
fn collect_keyed(
    value: &Value,
    wanted: &dyn Fn(&str) -> bool,
    path: &str,
    found: &mut Vec<(String, Value)>,
) {
    match value {
        Value::Object(map) => {
            for (key, member) in map {
                let here = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
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

/// Charset only; length is the row's budget so a configured override can move it.
fn tool_name_charset_ok(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Measure one row against one tool, returning the number and the site it came from.
///
/// `budget` is the resolved budget the caller will compare against. Only the tool-name row
/// reads it: a charset violation is rejected at any length, so it must report above whatever
/// budget is in force rather than as a length a raised budget could admit.
fn measure(limit: &HarnessLimit, tool: &Value, budget: usize) -> (usize, String) {
    let name = tool["name"].as_str().unwrap_or("?");
    let input = &tool["inputSchema"];
    let output = &tool["outputSchema"];
    match limit.name {
        "codex-input-schema-bytes" | "codex-input-schema-margin" => (
            codex_measured_bytes(input),
            "as Codex counts it, after its own sanitize pass".to_owned(),
        ),
        // Both description caps live in JavaScript clients, where `.length` counts UTF-16 code
        // units: a supplementary-plane character costs two against the client's budget, so it
        // must cost two here or a passing report describes text the client truncates.
        "claude-code-tool-description-chars" => (
            tool["description"]
                .as_str()
                .unwrap_or_default()
                .encode_utf16()
                .count(),
            "UTF-16 units of tool.description, the unit JavaScript length counts".to_owned(),
        ),
        "vscode-parameter-description-chars" => {
            let mut found = Vec::new();
            collect_descriptions(input, "inputSchema", &mut found);
            match found
                .iter()
                .max_by_key(|(_, text)| text.encode_utf16().count())
            {
                Some((path, text)) => (
                    text.encode_utf16().count(),
                    format!("longest at {name}.{path}"),
                ),
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
            format!(
                "deepest nested container, root at 1, at {}",
                deepest_pointer(output)
            ),
        ),
        "mcp-output-schema-point-of-use-depth" => (
            point_of_use_depth(output),
            format!(
                "deepest use path, $ref as a leaf, at {}",
                deepest_pointer(&without_definitions(output))
            ),
        ),
        "style-named-definitions-are-described" => {
            let missing = undescribed_definitions(output);
            let evidence = if missing.is_empty() {
                "every named shape carries a description".to_owned()
            } else {
                format!("undescribed: {}", missing.join(", "))
            };
            (missing.len(), evidence)
        }
        "mcp-output-schema-subschemas" => (
            subschema_count(output),
            "schema positions a validator may enter".to_owned(),
        ),
        "anthropic-tool-name-charset" => {
            let length = name.chars().count();
            if tool_name_charset_ok(name) {
                (length, format!("tool name {name:?}"))
            } else {
                (
                    length.max(budget.saturating_add(1)),
                    format!("tool name {name:?} has characters outside [A-Za-z0-9_-]"),
                )
            }
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

/// Measure one serialized `CallToolResult` against every response-artifact row.
///
/// A catalogue cannot observe these, so [`evaluate`] skips them and the report says so. They need
/// a `tools/call` fixture, and a fixture is something the tests own rather than the CLI: the only
/// honest corpus is a synthetic one, because a capture taken from a real index carries real
/// repository paths and cannot be committed or regenerated reproducibly.
///
/// Token rows are measured at four characters per token, the same ratio Claude Code uses for its
/// own truncation budget. That is an estimate and is labelled as one; the character rows are not.
pub fn evaluate_response(
    tool: &str,
    serialized_chars: usize,
    overrides: &ClientLimitOverrides,
    strict: bool,
) -> Vec<Finding> {
    HARNESS_LIMITS
        .iter()
        .filter(|limit| limit.applies_to == AppliesTo::Response)
        .map(|limit| {
            let measured = if limit.unit == "tokens" {
                serialized_chars.div_ceil(4)
            } else {
                serialized_chars
            };
            let budget = resolved_budget(limit, overrides);
            let over = measured > budget;
            let status = if !over {
                Status::Pass
            } else if strict || (limit.fail_on_breach && !limit.warning_only) {
                Status::Fail
            } else {
                Status::Warning
            };
            Finding {
                limit,
                budget,
                notice_at: None,
                tool: tool.to_owned(),
                measured,
                status,
                evidence: if limit.unit == "tokens" {
                    format!("{serialized_chars} characters at 4 characters per token, estimated")
                } else {
                    "characters of the serialized CallToolResult".to_owned()
                },
            }
        })
        .collect()
}

/// Sweep every applicable row over every advertised tool.
///
/// Rows whose artifact is a `tools/call` result are skipped: a catalogue cannot observe them, and
/// reporting an unobserved stage as passing is the same defect as reporting it as zero bytes.
pub fn evaluate(tools: &[Value], overrides: &ClientLimitOverrides, strict: bool) -> Vec<Finding> {
    let mut findings = Vec::new();
    for limit in all_limits() {
        if limit.applies_to == AppliesTo::Response {
            continue;
        }
        for tool in tools {
            let budget = resolved_budget(limit, overrides);
            let (measured, evidence) = measure(limit, tool, budget);
            let name = tool["name"].as_str().unwrap_or("?").to_owned();
            let notice_at = resolved_notice_at(limit, overrides);
            let over = measured > budget;
            let status = if !over {
                match notice_at {
                    Some(line) if measured > line => Status::Notice,
                    _ => Status::Pass,
                }
            } else if strict || (limit.fail_on_breach && !limit.warning_only) {
                Status::Fail
            } else {
                Status::Warning
            };
            findings.push(Finding {
                limit,
                budget,
                notice_at,
                tool: name,
                measured,
                status,
                evidence,
            });
        }
    }
    findings
}

/// Per-row budget overrides, keyed by the row name the checker reports.
pub type ClientLimitOverrides = std::collections::BTreeMap<String, usize>;

/// The budget in force for one row: what the operator configured, or what was measured.
///
/// A row's number is client policy read from that client's source at a pinned version, and client
/// policy moves. Reading it through configuration is what lets an operator track a client that
/// moved without waiting for a release built against the new value.
pub fn resolved_budget(limit: &HarnessLimit, overrides: &ClientLimitOverrides) -> usize {
    if !limit.overridable {
        // Never silent in practice: configuration rejects an override naming one of these rows,
        // so ignoring it here is an invariant, not a code path an operator can reach.
        return limit.budget;
    }
    overrides.get(limit.name).copied().unwrap_or(limit.budget)
}

/// Whether one named row takes a `[mcp.client_limits]` override, or `None` for a name that is
/// no row. Configuration uses it to refuse an override that could only silence the checker.
pub fn limit_overridable(name: &str) -> Option<bool> {
    all_limits()
        .find(|limit| limit.name == name)
        .map(|limit| limit.overridable)
}

/// The notice threshold, scaled to a configured budget so it keeps meaning the same thing.
///
/// The shipped notice thresholds sit at a fixed fraction below their budget. An operator who raises
/// the budget and keeps a threshold computed from the old one gets a tripwire that fires on
/// every healthy build, which is how a warning channel stops being read.
fn resolved_notice_at(limit: &HarnessLimit, overrides: &ClientLimitOverrides) -> Option<usize> {
    let notice_at = limit.notice_at?;
    let budget = resolved_budget(limit, overrides);
    if budget == limit.budget {
        return Some(notice_at);
    }
    Some((notice_at as u128 * budget as u128 / limit.budget.max(1) as u128) as usize)
}

/// Every declared row name, so configuration can reject a key that names no row.
pub fn declared_limit_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = all_limits().map(|limit| limit.name).collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Sweep every row, over every tool, and over a `tools/call` result when one was measured.
///
/// [`evaluate`] alone covers seven of the ten declared rows, because a catalogue cannot observe a
/// response. The three it skips are not unobservable in general, only unobservable from that one
/// artifact: `crate::mcp_server::measure_representative_response` builds a synthetic corpus and
/// runs a real search through the production dispatcher, and that result is exactly what the
/// response rows are declared against. Passing it here is what makes the shipped checker measure
/// its own table rather than most of it.
///
/// `response` stays optional because a fixture can fail to build, and a row with no artifact is
/// omitted so the report can name it as unmeasured. Scoring it as a pass would be the same defect
/// as reporting an unobserved stage as zero bytes.
pub fn evaluate_all(
    tools: &[Value],
    response: Option<(&str, usize)>,
    overrides: &ClientLimitOverrides,
    strict: bool,
) -> Vec<Finding> {
    let mut findings = evaluate(tools, overrides, strict);
    if let Some((tool, serialized_chars)) = response {
        findings.extend(evaluate_response(tool, serialized_chars, overrides, strict));
    }
    findings
}

/// Warn about every enforced schema breach in the catalogue this configuration actually emits.
///
/// The release gate measures the catalogue built from the default configuration, and that is not
/// the catalogue a given operator serves. Seven `search_messages` descriptions interpolate a
/// resolved number -- the page, both context counts, the line window, and the two view budgets --
/// so an extra decimal digit in any of them is an extra byte in the schema. The default leaves
/// five bytes under Codex's limit, and three ordinary three-digit settings spend six.
///
/// Past that limit Codex deletes every description at every depth, emits no marker, logs nothing,
/// and keeps no copy. Nothing downstream can report it, which leaves the server as the only place
/// that still knows. So it says so on stderr, where a client shows server output, rather than
/// letting the operator's own configuration quietly strip the schema their model reads.
///
/// This is a warning and not a refusal: the schema is degraded, not invalid, and a server that
/// refused to start would take away a working surface to protest a recoverable one.
pub fn configured_catalogue_warnings(
    listed: &Value,
    overrides: &ClientLimitOverrides,
) -> Vec<String> {
    let Some(tools) = listed.as_array() else {
        return Vec::new();
    };
    all_limits()
        // Warn-only rows are excluded on purpose. The 4,750-byte margin tripwire is expected to
        // fire on a healthy build, and an operator who sees it every startup learns to ignore the
        // channel that carries the real breach.
        .filter(|limit| {
            limit.fail_on_breach && !limit.warning_only && limit.applies_to != AppliesTo::Response
        })
        .flat_map(|limit| {
            let budget = resolved_budget(limit, overrides);
            tools.iter().filter_map(move |tool| {
                let (measured, _) = measure(limit, tool, budget);
                if measured <= budget {
                    return None;
                }
                let name = tool["name"].as_str().unwrap_or("?");
                // A structural rule must not advise the override that validation rejects and
                // that could not make the schema acceptable anyway.
                let remedy = if limit.overridable {
                    format!(
                        "Set [mcp.client_limits].{} to track a client that moved.",
                        limit.name
                    )
                } else {
                    "This is a structural rule rather than a client budget: fix the emitted \
                     schema; no configured number admits it."
                        .to_owned()
                };
                Some(format!(
                    "warning: {name} breaches {}: {} measured {measured} {} against the {budget} \
                     {} limit of {}. {} {remedy}",
                    limit.name,
                    limit.artifact,
                    limit.unit,
                    limit.unit,
                    limit.authority,
                    limit.rationale,
                ))
            })
        })
        .collect()
}

/// Warn when the configured delivery ceiling admits a result a client will silently truncate.
///
/// `mcp.max_tool_result_chars` decides what this server delivers; a client's own cap decides what
/// survives. The shipped 48,000 is Codex's, chosen because Codex is the measured client that
/// truncates from the middle with no marker while the others announce the overflow and persist it.
/// Raising the ceiling past a silent cap reinstates that truncation with every layer below working
/// as designed: the result is inside the configured ceiling so nothing errors, and the client
/// deletes the middle without saying so.
///
/// Only silent rows are compared. Exceeding an announced cap costs a round trip and a file read
/// rather than data, and warning about it would train an operator to ignore the channel that
/// carries the one that matters.
pub fn configured_ceiling_warnings(
    max_tool_result_chars: usize,
    overrides: &ClientLimitOverrides,
) -> Vec<String> {
    all_limits()
        .filter(|limit| {
            limit.applies_to == AppliesTo::Response && limit.failure_mode == FailureMode::Silent
        })
        .filter_map(|limit| {
            let budget = resolved_budget(limit, overrides);
            // Token budgets are compared at the same four-characters-per-token ratio the rows
            // are measured with, and the estimate is labelled where it is used.
            let budget_chars = if limit.unit == "tokens" {
                budget.saturating_mul(4)
            } else {
                budget
            };
            if max_tool_result_chars <= budget_chars {
                return None;
            }
            Some(format!(
                "warning: mcp.max_tool_result_chars is {max_tool_result_chars}, above the \
                 {budget_chars} characters {} keeps ({}). A result between those two sizes is \
                 delivered by this server and then truncated from the middle by that client with \
                 no marker. Lower the ceiling, or raise [mcp.client_limits].{} if that client \
                 raised its own.",
                limit.authority, limit.name, limit.name,
            ))
        })
        .collect()
}

/// State the breach, and for a silent cap state that the caller gets no signal.
pub fn describe_breach(limit: &HarnessLimit, budget: usize, measured: usize) -> String {
    let head = format!(
        "{} measured {measured} {} against the {budget} {} limit of {}",
        limit.artifact, limit.unit, limit.unit, limit.authority
    );
    match limit.failure_mode {
        FailureMode::Silent => format!(
            "{head}. The breach is silent: the client neither errors nor marks the result, so \
             nothing downstream reports it. {}",
            limit.rationale
        ),
        FailureMode::Announced => {
            format!(
                "{head}. The client reports the overflow and preserves it. {}",
                limit.rationale
            )
        }
        FailureMode::Rejected => {
            format!(
                "{head}. The client rejects the artifact outright. {}",
                limit.rationale
            )
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
pub const LEDGER_STAGES: [&str; 9] = [
    "raw_catalogue",
    "jsonrpc_catalogue_envelope",
    "input_schema_wire",
    "input_schema_client_normalized",
    "output_schema_declaration",
    "canonical_result",
    "call_tool_result",
    "jsonrpc_response",
    "harness_model_input",
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

/// What each client hands its model, which is the only stage that measures effectiveness.
///
/// Every stage above it is something this server or a transport owns. This one is owned by the
/// client, and the same bytes reach different models differently: Codex normalizes the schema and
/// drops what it does not model, Claude Code may defer a tool definition entirely, and OpenCode
/// synthesizes text from `structuredContent` when a result carries none. A row here is `verified`
/// only when that client's own transformation was read or measured, never inferred from another
/// client that happens to be installed.
fn harness_model_input_stage() -> Value {
    json!({
        "artifact": "what each client supplies to its model",
        "unit": "bytes",
        "interpretation": "the effectiveness measure. Client-owned, so a row is verified only from \
                           that client's source or a capture, never inferred from a sibling.",
        "clients": [
            {
                "client": "codex",
                "status": "verified",
                "transformation": "normalizes inputSchema to fourteen modeled keys, then strips \
                                   every description above 5000 bytes",
                "evidence": "codex-rs/tools/src/json_schema.rs",
                "measured_by": "input_schema_client_normalized, which mirrors that normalization",
            },
            {
                "client": "claude-code",
                "status": "unverified",
                "transformation": "tool search may defer MCP tool definitions, so upfront and \
                                   deferred discovery deliver different bytes",
                "rerun": "capture tools/list and one tools/call from an installed Claude Code \
                          session and record which mode was active",
            },
            {
                "client": "opencode",
                "status": "unverified",
                "transformation": "builds the model tool from description and inputSchema only, \
                                   and synthesizes text from structuredContent when a result \
                                   carries no text content",
                "rerun": "install opencode and capture one tools/call",
            },
        ],
    })
}

/// Report each MCP overhead stage separately, with the artifact each one measures.
///
/// `response` is the measured `tools/call` fixture. Passing `None` reports the three response
/// stages with a status and a rerun action rather than with zeros, because a zero in a byte
/// column reads as a saving and an absence is not one.
pub(crate) fn stage_ledger(
    tools: &[Value],
    response: Option<&crate::mcp_server::MeasuredResponse>,
) -> Value {
    let catalogue = Value::Array(tools.to_vec());
    let envelope = json!({ "jsonrpc": "2.0", "id": 2, "result": { "tools": tools } });
    let input_wire: usize = tools
        .iter()
        .map(|tool| compact_len(&tool["inputSchema"]))
        .sum();
    let input_normalized: usize = tools
        .iter()
        .map(|tool| compact_len(&codex_visible_schema(&tool["inputSchema"])))
        .sum();
    let output_wire: usize = tools
        .iter()
        .map(|tool| compact_len(&tool["outputSchema"]))
        .sum();

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
    match response {
        Some(measured) => {
            let identity = json!({
                "fixture": measured.fixture,
                "tool": measured.tool,
                "arguments": measured.arguments,
                "returned": measured.returned,
                "status": "verified",
                "unit": "characters",
                "algorithm": "Unicode scalar count, the same method the runtime ceiling enforces",
            });
            let with = |artifact: &str, chars: usize, interpretation: &str| {
                let mut stage = identity.clone();
                let object = stage.as_object_mut().expect("identity is an object");
                object.insert("artifact".to_owned(), json!(artifact));
                object.insert("characters".to_owned(), json!(chars));
                object.insert("interpretation".to_owned(), json!(interpretation));
                stage
            };
            stages.insert(
                "canonical_result".to_owned(),
                with(
                    "structuredContent",
                    measured.canonical_chars,
                    "product payload. Field and include-group costs belong to this stage.",
                ),
            );
            stages.insert(
                "call_tool_result".to_owned(),
                with(
                    "serialized CallToolResult",
                    measured.call_tool_result_chars,
                    "the artifact mcp.max_tool_result_chars is enforced against.",
                ),
            );
            stages.insert(
                "jsonrpc_response".to_owned(),
                with(
                    "tools/call JSON-RPC message",
                    measured.jsonrpc_chars,
                    "transport overhead. Report the delta from call_tool_result, but do not \
                     charge it to the dispatcher-owned ceiling, which does not own the wrapper.",
                ),
            );
        }
        None => {
            for stage in RESPONSE_LEDGER_STAGES {
                stages.insert(stage.to_owned(), fixture_required_stage(stage));
            }
        }
    }
    stages.insert(
        "harness_model_input".to_owned(),
        harness_model_input_stage(),
    );

    let per_tool: Vec<Value> = tools
        .iter()
        .map(|tool| {
            let input = &tool["inputSchema"];
            let output = &tool["outputSchema"];
            let normalized = codex_measured_schema(input);
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
    /// Fail every measured over-limit rule, including rows not yet in the default failure gate.
    #[arg(long)]
    pub strict: bool,
}

/// Group the report by rule, so one breach reads as one finding rather than once per tool.
fn report(findings: &[Finding]) -> bool {
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
            Status::Warning => 1,
            Status::Notice => 2,
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
        println!(
            "{:<10} {} — {}, {}",
            status.label(),
            limit.name,
            limit.authority,
            limit.artifact
        );
        let budget = first.budget;
        if matches!(status, Status::Fail | Status::Warning) {
            let worst = group
                .iter()
                .map(|finding| finding.measured)
                .max()
                .unwrap_or(budget);
            println!("        {}", describe_breach(limit, budget, worst));
        }
        let mut sorted = group.clone();
        sorted.sort_by_key(|finding| std::cmp::Reverse(finding.measured));
        for finding in sorted {
            if *status == Status::Notice {
                println!(
                    "        {}: {} {}; notice above {}, inclusive ceiling {} {} ({})",
                    finding.tool,
                    finding.measured,
                    limit.unit,
                    finding.notice_at.unwrap_or(finding.budget),
                    finding.budget,
                    limit.unit,
                    finding.evidence
                );
            } else {
                println!(
                    "        {}: {} {} against the {} limit ({})",
                    finding.tool, finding.measured, limit.unit, finding.budget, finding.evidence
                );
            }
        }
        if *status == Status::Warning {
            if limit.warning_only {
                println!(
                    "        Non-fatal warning by design; this measurement is over the ceiling."
                );
            } else {
                println!(
                    "        Default gate is non-blocking; --strict treats this breach as a failure. \
                     Tracked under {}.",
                    if limit.planned_enforcement.is_empty() {
                        "a later enforcement change"
                    } else {
                        limit.planned_enforcement
                    }
                );
            }
        }
        println!("        Raise when: {}", limit.raise_when);
        println!("        Lower when: {}", limit.lower_when);
    }

    // Derived from what was scored rather than from the row's kind: the response rows are measured
    // whenever a fixture built, so a fixed list here would keep announcing a gap that closed.
    let scored: std::collections::BTreeSet<&str> =
        findings.iter().map(|finding| finding.limit.name).collect();
    let unmeasured: Vec<&HarnessLimit> = all_limits()
        .filter(|limit| limit.applies_to == AppliesTo::Response)
        .filter(|limit| !scored.contains(limit.name))
        .collect();
    if !unmeasured.is_empty() {
        println!(
            "NOTE    {} response-artifact rows have no tools/call measurement in this run and are \
             reported as unmeasured, never as passing:",
            unmeasured.len()
        );
        for limit in &unmeasured {
            println!("        {}: {} {}", limit.name, limit.budget, limit.unit);
        }
    }

    let failures = findings
        .iter()
        .filter(|finding| finding.status == Status::Fail)
        .count();
    let passes = findings
        .iter()
        .filter(|finding| finding.status == Status::Pass)
        .count();
    let warnings = findings
        .iter()
        .filter(|finding| finding.status == Status::Warning)
        .count();
    let notices = findings
        .iter()
        .filter(|finding| finding.status == Status::Notice)
        .count();
    println!(
        "{} measurements: {passes} pass, {notices} notice, {warnings} warning, {failures} fail",
        findings.len()
    );
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
        // A fixture that cannot be built is reported as an absent measurement rather than
        // failing the command: the catalogue stages are still worth having, and a zero or a
        // guess in a response column would be worse than a stated gap.
        let measured = crate::mcp_server::measure_representative_response(config);
        if let Err(error) = &measured {
            eprintln!("warning: response stages are unmeasured: {error}");
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&stage_ledger(tools, measured.as_ref().ok()))?
        );
        return Ok(());
    }
    // The same fixture the ledger measures, so the response rows are swept here too. A fixture
    // that cannot be built leaves those rows unmeasured and says so, rather than failing a
    // catalogue check for a reason that has nothing to do with the catalogue.
    let response = match crate::mcp_server::measure_representative_response(config) {
        Ok(measured) => Some(measured),
        Err(error) => {
            eprintln!("warning: response rows are unmeasured: {error}");
            None
        }
    };
    let response = response
        .as_ref()
        .map(|measured| (measured.tool, measured.call_tool_result_chars));
    let overrides = &config.mcp.client_limits;
    // A configured delivery ceiling is not measured against a tool, so it is not a row the sweep
    // can carry. It is still a bound this configuration sets against a client's own, and it fails
    // silently, so it is reported beside them rather than left for the operator to notice.
    for warning in configured_ceiling_warnings(config.mcp.max_tool_result_chars, overrides) {
        println!("{warning}");
    }
    if !report(&evaluate_all(tools, response, overrides, args.strict)) {
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
        let properties = visible["properties"]
            .as_object()
            .expect("properties survive");
        assert_eq!(properties.len(), 2, "{visible}");
        assert_eq!(properties["query"]["description"], json!("Text to match."));
        assert!(properties["limit"].get("minimum").is_none(), "{visible}");
        assert!(properties["limit"].get("maximum").is_none(), "{visible}");
    }

    /// Codex models `enum` and does not model `const`. That asymmetry is why replacing the
    /// discriminators is worth doing and why it costs bytes in the one channel that binds.
    #[test]
    fn codex_visible_schema_keeps_enum_and_drops_const() {
        assert_eq!(
            codex_visible_schema(&json!({ "const": "max_chars" })),
            json!({})
        );
        assert_eq!(
            codex_visible_schema(&json!({ "enum": ["max_chars"] })),
            json!({ "enum": ["max_chars"] })
        );
    }

    /// Codex sanitizes before it measures, and sanitizing can make a schema *larger*.
    ///
    /// `sanitize_json_schema` infers a type for any schema that declares none, and a bare `enum`
    /// infers `"string"` (`json_schema.rs:524`), which `write_schema_types` then writes back
    /// (`:538`). Only afterwards does `compact_large_tool_schema` measure. So a discriminator
    /// emitted as `{"enum":["max_chars"]}` still costs the sixteen bytes of the type it omitted,
    /// and a checker that normalizes without sanitizing reports a schema smaller than the one
    /// the client charges for. Measured on the shipped catalogue, that gap was 48 bytes on
    /// `search_messages` and 64 on `run_skill_capability` -- enough on both to turn a breach
    /// into an apparent pass.
    #[test]
    fn an_enum_without_a_type_is_measured_with_the_type_codex_infers() {
        let bare = json!({ "enum": ["max_chars"] });
        let explicit = json!({ "type": "string", "enum": ["max_chars"] });
        assert_eq!(
            codex_measured_schema(&bare),
            codex_measured_schema(&explicit),
            "omitting a type Codex infers must not look cheaper than declaring it"
        );
        assert_eq!(
            codex_measured_bytes(&bare),
            codex_measured_bytes(&explicit),
            "the byte figure the budget rows compare against must agree too"
        );
    }

    /// The other two arms of the same inference ladder, and the one that erases a schema.
    #[test]
    fn codex_infers_the_type_its_sanitizer_would_write_back() {
        // `properties`/`required`/`additionalProperties` imply object; `items` implies array.
        assert_eq!(
            codex_measured_schema(&json!({ "properties": { "a": { "type": "string" } } }))["type"],
            json!("object")
        );
        assert_eq!(
            codex_measured_schema(&json!({ "items": { "type": "string" } }))["type"],
            json!("array")
        );
        // An object schema carrying none of the recognized hints is cleared outright, so a
        // bare description costs nothing rather than the bytes it appears to.
        assert_eq!(
            codex_measured_schema(&json!({ "description": "orphan" })),
            json!({})
        );
        // A `$ref` or a composition keyword ends the ladder before any type is written.
        assert_eq!(
            codex_measured_schema(&json!({ "$ref": "#/$defs/CharBudget" })),
            json!({ "$ref": "#/$defs/CharBudget" })
        );
    }

    /// Sanitizing fills required children, which also grows the measured artifact.
    #[test]
    fn codex_fills_the_children_its_sanitizer_requires() {
        let measured = codex_measured_schema(&json!({ "type": "array" }));
        assert_eq!(measured["items"], json!({ "type": "string" }));
        let measured = codex_measured_schema(&json!({ "type": "object" }));
        assert_eq!(measured["properties"], json!({}));
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
            if !limit.fail_on_breach {
                assert!(
                    !limit.planned_enforcement.is_empty(),
                    "{} is non-blocking and names no planned enforcement change, so the ratchet \
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
            let message = describe_breach(limit, limit.budget, limit.budget + 1);
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

    #[test]
    fn the_depth_ceiling_reports_a_notice_at_the_margin_without_breaching_the_limit() {
        let config = crate::config::Config::default();
        let tools = crate::mcp_server::advertised_tools(&config);
        let finding = evaluate(
            tools.as_array().expect("tools list"),
            &config.mcp.client_limits,
            true,
        )
        .into_iter()
        .find(|finding| {
            finding.limit.name == "mcp-output-schema-point-of-use-depth"
                && finding.tool == "search_messages"
        })
        .expect("search_messages depth finding");

        assert_eq!(finding.measured, finding.budget);
        assert_eq!(finding.notice_at, Some(5));
        assert_eq!(finding.status, Status::Notice);
        assert_eq!(Status::Notice.label(), "NOTICE");
    }

    #[test]
    fn warning_only_rows_start_after_the_inclusive_limit() {
        let config = crate::config::Config::default();
        let margin = all_limits()
            .find(|limit| limit.name == "codex-input-schema-margin")
            .expect("Codex margin rule");
        let mut tool = crate::mcp_server::advertised_tools(&config)
            .as_array()
            .expect("tools list")
            .iter()
            .find(|tool| tool["name"] == "search_messages")
            .cloned()
            .expect("search_messages tool");
        let mut at_limit = None;
        for _ in 0..=margin.budget {
            let (measured, _) = measure(margin, &tool, margin.budget);
            if measured == margin.budget {
                at_limit = Some(tool.clone());
                break;
            }
            let description = tool["inputSchema"]["properties"]["query"]["description"]
                .as_str()
                .expect("query description");
            tool["inputSchema"]["properties"]["query"]["description"] =
                json!(format!("{description}x"));
        }
        let at_limit = at_limit.expect("the margin rule should have a reachable exact boundary");
        let exact = evaluate(
            std::slice::from_ref(&at_limit),
            &ClientLimitOverrides::new(),
            false,
        )
        .into_iter()
        .find(|finding| finding.limit.name == margin.name)
        .expect("exact margin finding");
        assert_eq!(exact.measured, margin.budget);
        assert_eq!(exact.status, Status::Pass);

        let mut over_limit = at_limit;
        over_limit["inputSchema"]["properties"]["query"]["description"] = json!(format!(
            "{}x",
            over_limit["inputSchema"]["properties"]["query"]["description"]
                .as_str()
                .expect("query description")
        ));
        let over = evaluate(&[over_limit], &ClientLimitOverrides::new(), false)
            .into_iter()
            .find(|finding| finding.limit.name == margin.name)
            .expect("over-limit margin finding");
        assert!(over.measured > margin.budget);
        assert_eq!(over.status, Status::Warning);
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
        evaluate(&[tool], &ClientLimitOverrides::new(), true)
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
                tool["inputSchema"]["properties"]["query"]["description"] =
                    json!("y".repeat(1_025));
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
            assert!(
                failed.contains(&rule),
                "{rule} did not fire; failures were {failed:?}"
            );
        }
    }

    /// The ratchet: a non-blocking breach is reported as a warning until strict mode is used.
    #[test]
    fn a_non_blocking_breach_is_a_warning_until_strict_mode() {
        // Any rule still carrying `fail_on_breach: false` will do, so the synthetic tool has to breach
        // whichever one is picked rather than the one that happened to be first when this was
        // written. It is both deep and wide for that reason: when the depth rule was enforced,
        // selection moved to the subschema rule, which a bare chain of 24 does not breach, and
        // the test failed for a reason that had nothing to do with the ratchet.
        let Some(rule) = all_limits()
            .find(|limit| !limit.fail_on_breach && limit.applies_to != AppliesTo::Response)
        else {
            // Every rule is enforced, which is where the ratchet was always heading. There is no
            // non-blocking state left to exercise, and reporting that as a failure would ask for a
            // defect to be reintroduced so a test about defects could keep passing.
            assert!(
                all_limits()
                    .all(|limit| limit.fail_on_breach || limit.applies_to == AppliesTo::Response),
                "the ratchet is fully tightened"
            );
            return;
        };
        let mut tool = minimal_tool();
        let mut deep = json!({ "type": "string" });
        for _ in 0..24 {
            deep = json!({ "type": "object", "properties": { "next": deep } });
        }
        let mut wide = Map::new();
        for index in 0..300 {
            wide.insert(format!("field_{index}"), json!({ "type": "string" }));
        }
        wide.insert("deep".to_owned(), deep);
        tool["outputSchema"] = json!({ "type": "object", "properties": wide });
        tool["inputSchema"]["properties"]["padding"] = json!({
            "type": "string",
            "description": "x".repeat(6_000),
        });

        let lenient = evaluate(
            std::slice::from_ref(&tool),
            &ClientLimitOverrides::new(),
            false,
        );
        let warnings: Vec<&Finding> = lenient
            .iter()
            .filter(|finding| finding.limit.name == rule.name)
            .collect();
        assert_eq!(warnings.len(), 1, "{}", rule.name);
        assert_eq!(warnings[0].status, Status::Warning, "{}", rule.name);
        assert!(
            warnings[0].measured > rule.budget,
            "a non-blocking rule must publish the measurement it is tracking"
        );
        assert!(
            lenient
                .iter()
                .all(|finding| finding.limit.name != rule.name || finding.status != Status::Fail),
            "a non-blocking rule failed the lenient ratchet"
        );

        let strict = evaluate(&[tool], &ClientLimitOverrides::new(), true);
        assert!(strict
            .iter()
            .any(|finding| finding.limit.name == rule.name && finding.status == Status::Fail));
    }

    #[test]
    fn the_ledger_reports_every_stage_and_marks_the_unobservable_ones() {
        let ledger = stage_ledger(&[minimal_tool()], None);
        let stages = ledger["stages"].as_object().expect("stages");
        for stage in LEDGER_STAGES {
            assert!(stages.contains_key(stage), "ledger is missing {stage}");
        }
        for stage in RESPONSE_LEDGER_STAGES {
            assert_eq!(
                stages[stage]["status"], "fixture-required",
                "{stage} must carry a status rather than a zero that reads as a saving"
            );
            assert!(
                stages[stage].get("bytes").is_none(),
                "{stage} reported bytes it cannot observe"
            );
        }
        // The harness stage is per client, and a client is verified only from its own evidence.
        let clients = stages["harness_model_input"]["clients"]
            .as_array()
            .expect("harness_model_input names its clients");
        assert!(
            clients.len() >= 3,
            "the harness stage must name each client it reports on"
        );
        for client in clients {
            let status = client["status"]
                .as_str()
                .expect("a client carries a status");
            assert!(
                matches!(status, "verified" | "unverified" | "not-applicable"),
                "{status:?} is not one of the three states a harness row may hold"
            );
            if status == "verified" {
                assert!(
                    client.get("evidence").is_some(),
                    "a verified client must name the evidence: {client}"
                );
            } else {
                assert!(
                    client.get("rerun").is_some(),
                    "an unverified client must name the action that would verify it: {client}"
                );
            }
        }
        // Wire bytes and model-facing bytes are different stages with different owners.
        assert!(
            stages["input_schema_wire"]["bytes"].as_u64()
                >= stages["input_schema_client_normalized"]["bytes"].as_u64()
        );
        assert_eq!(
            stages["output_schema_declaration"]["model_facing_verified"],
            json!(false)
        );
        assert_eq!(stages["raw_catalogue"]["artifact"], "tools[] array");
        assert_eq!(
            stages["jsonrpc_catalogue_envelope"]["artifact"],
            "tools/list JSON-RPC message"
        );
    }

    /// A supplied fixture measures the three response stages through the real serializers.
    ///
    /// The stages nest, so their sizes must too: `structuredContent` sits inside the
    /// `CallToolResult`, which sits inside the JSON-RPC message. Asserting the ordering catches a
    /// stage measured against the wrong artifact, which is the failure that makes a ledger worse
    /// than no ledger -- every number present, one of them describing something else.
    #[test]
    fn a_supplied_fixture_measures_the_three_response_stages() {
        let config = crate::config::Config::default();
        let measured = crate::mcp_server::measure_representative_response(&config)
            .expect("the synthetic fixture builds without touching the configured index");
        let ledger = stage_ledger(&[minimal_tool()], Some(&measured));
        let stages = ledger["stages"].as_object().expect("stages");

        let chars = |stage: &str| {
            stages[stage]["characters"]
                .as_u64()
                .unwrap_or_else(|| panic!("{stage} reported no measurement: {}", stages[stage]))
        };
        let canonical = chars("canonical_result");
        let wrapped = chars("call_tool_result");
        let envelope = chars("jsonrpc_response");

        assert!(canonical > 0, "the fixture produced an empty response");
        assert!(
            canonical < wrapped,
            "structuredContent ({canonical}) is inside the CallToolResult ({wrapped}), so it \
             cannot be the larger of the two"
        );
        assert!(
            wrapped < envelope,
            "the CallToolResult ({wrapped}) is inside the JSON-RPC message ({envelope})"
        );

        for stage in RESPONSE_LEDGER_STAGES {
            assert_eq!(stages[stage]["status"], "verified");
            assert_eq!(
                stages[stage]["fixture"], measured.fixture,
                "every measured stage names the corpus that produced it, so a figure can be \
                 traced back to its input"
            );
            assert_eq!(
                stages[stage]["unit"], "characters",
                "the ceiling counts Unicode scalars, so the ledger must report the same unit \
                 rather than bytes that would differ on any non-ASCII content"
            );
        }
    }

    /// The whole ledger survives a JSON round trip.
    ///
    /// The saved baseline note that prompted this check was not invalid because the command
    /// emitted bad JSON; it was truncated by the capture that wrote it, and the truncation marker
    /// sat where a property name belonged. A parser is the only thing that tells those two apart,
    /// so one runs here rather than a reader deciding the file looked complete.
    #[test]
    fn the_ledger_is_complete_parseable_json() {
        let config = crate::config::Config::default();
        let measured = crate::mcp_server::measure_representative_response(&config).ok();
        let ledger = stage_ledger(&[minimal_tool()], measured.as_ref());

        let rendered = serde_json::to_string_pretty(&ledger).expect("the ledger serializes");
        let reparsed: Value = serde_json::from_str(&rendered)
            .expect("the emitted ledger must parse; a truncated capture fails exactly here");
        assert_eq!(reparsed, ledger, "the round trip changed the ledger");
        assert_eq!(
            reparsed["stages"].as_object().expect("stages").len(),
            LEDGER_STAGES.len(),
            "a truncated ledger loses trailing stages, which is what makes it look complete"
        );
    }

    /// The sweep measures every row it has an artifact for, response rows included.
    ///
    /// The ledger already builds a `tools/call` fixture, so a checker that still reports the three
    /// response rows as needing one is describing a gap it no longer has. Seven of ten rows
    /// measured reads as a complete sweep to anyone who does not count them.
    #[test]
    fn a_supplied_response_measurement_covers_the_response_rows() {
        let findings = evaluate_all(
            &[minimal_tool()],
            Some(("search_messages", 4_046)),
            &ClientLimitOverrides::new(),
            true,
        );

        let measured: std::collections::BTreeSet<&str> = findings
            .iter()
            .filter(|finding| finding.limit.applies_to == AppliesTo::Response)
            .map(|finding| finding.limit.name)
            .collect();
        let declared: std::collections::BTreeSet<&str> = all_limits()
            .filter(|limit| limit.applies_to == AppliesTo::Response)
            .map(|limit| limit.name)
            .collect();
        assert_eq!(
            measured, declared,
            "a response row went unmeasured while a fixture measurement was available"
        );
        assert!(
            findings
                .iter()
                .filter(|finding| finding.limit.applies_to == AppliesTo::Response)
                .all(|finding| finding.status == Status::Pass),
            "4,046 characters is inside every declared client result cap"
        );
    }

    /// No fixture means unmeasured, never passing.
    ///
    /// This is the same defect as reporting an unobserved stage as zero bytes: a row that silently
    /// disappears when its artifact is unavailable makes the surface look checked. The report says
    /// so out loud instead, and that only works if the findings genuinely omit the row.
    #[test]
    fn a_missing_response_measurement_leaves_the_rows_unmeasured_rather_than_passing() {
        let findings = evaluate_all(&[minimal_tool()], None, &ClientLimitOverrides::new(), true);

        assert!(
            findings
                .iter()
                .all(|finding| finding.limit.applies_to != AppliesTo::Response),
            "a response row was scored without an artifact to score it against"
        );
        assert!(
            all_limits().any(|limit| limit.applies_to == AppliesTo::Response),
            "there are response rows to omit in the first place"
        );
    }

    /// Configure `count` purpose bundles, the one knob with nothing bounding what it can add.
    ///
    /// A purpose name is a user-chosen string that lands twice in the emitted `search_messages`
    /// schema: once in an `enum` Codex models and keeps, and once in the prose listing the
    /// bundles this deployment configured. Nothing bounds a name's length or how many an operator
    /// defines, so this is the knob that still crosses the line now that streamlining the
    /// descriptions took the margin from 5 bytes to 425.
    ///
    /// The integer knobs no longer reach it. `search_messages_limit`, `preview_chars` and
    /// `lines_per_message` are interpolated into descriptions, so each one costs a byte per extra
    /// decimal digit -- three ordinary settings spent the 5-byte margin and cannot spend 425.
    fn with_purpose_bundles(config: &mut crate::config::Config, count: usize) {
        for index in 0..count {
            config.search.purposes.insert(
                format!("review-recent-work-{index}"),
                crate::config::PurposeDefinition {
                    version: std::num::NonZeroU32::new(1).expect("1 is nonzero"),
                    operation: crate::config::SearchOperation::MessageSearch,
                    preferences: Default::default(),
                },
            );
        }
    }

    /// The budget is a function of configuration, so one configuration does not prove it holds.
    ///
    /// Ten named purpose bundles is an ordinary thing for an operator to configure, and it puts
    /// `search_messages` past the limit at which Codex deletes all 37 descriptions and says
    /// nothing. Nothing in the schema bounds either the count or the name length, so no amount of
    /// streamlining closes this: the property worth asserting is that the server says so.
    #[test]
    fn a_plausible_configuration_can_spend_the_remaining_input_schema_margin() {
        let mut config = crate::config::Config::default();
        with_purpose_bundles(&mut config, 10);

        let listed = crate::mcp_server::advertised_tools(&config);
        let tools = listed.as_array().expect("tools list");
        let search = tools
            .iter()
            .find(|tool| tool["name"] == "search_messages")
            .expect("search_messages is served");
        // Sanitized, the way Codex counts it. Measuring the emitted bytes instead is what let the
        // catalogue ship over budget with the gate green.
        let measured = codex_measured_bytes(&search["inputSchema"]);

        let budget = all_limits()
            .find(|limit| limit.name == "codex-input-schema-bytes")
            .expect("the row exists")
            .budget;
        assert!(
            measured > budget,
            "ten purpose bundles measured {measured} bytes against {budget}. If it now fits, the \
             margin was widened and this test should be re-pinned to the configuration that \
             still spends it -- not deleted, because the schema stays configuration-dependent."
        );
    }

    /// Every word this server writes about a parameter reaches the model that has to use it.
    ///
    /// This is the property the byte rows are a proxy for, asserted directly. A limit is a number
    /// copied out of another project and it can go stale or be reconfigured; "the caller was shown
    /// what we wrote" cannot. Asserting only the proxy is how the catalogue shipped for a release
    /// with `search_messages` and `run_skill_capability` handing Codex 37 and 19 parameters with
    /// names and types and no prose at all, while the gate reported green -- the sweep measured
    /// bytes without Codex's sanitize pass, so the number it compared was not the number Codex
    /// compares. A count of surviving descriptions has no such gap to fall through.
    #[test]
    fn every_advertised_description_reaches_the_model() {
        let config = crate::config::Config::default();
        let catalogue = crate::mcp_server::advertised_tools(&config);
        let stripped: Vec<String> = catalogue
            .as_array()
            .expect("advertised_tools returns an array")
            .iter()
            .filter(|tool| codex_strips_descriptions(&tool["inputSchema"]))
            .map(|tool| {
                let mut found = Vec::new();
                collect_descriptions(&tool["inputSchema"], "", &mut found);
                format!(
                    "{}: all {} parameter descriptions deleted, {} bytes against {}",
                    tool["name"].as_str().unwrap_or_default(),
                    found.len(),
                    codex_measured_bytes(&tool["inputSchema"]),
                    CODEX_COMPACT_TOOL_SCHEMA_BYTES,
                )
            })
            .collect();
        assert!(
            stripped.is_empty(),
            "Codex deletes every description on these tools before its model sees them, with no \
             marker to the model, nothing logged, and no copy kept: {stripped:#?}"
        );
    }

    /// So the operator whose configuration spends the margin is told, rather than Codex silently
    /// deleting every description on the way to their model.
    #[test]
    fn a_configuration_that_breaches_the_budget_is_reported_to_the_operator() {
        let mut config = crate::config::Config::default();
        config.mcp.search_messages_limit = 1_000;
        config.mcp.preview_chars = 10_000;
        config.mcp.lines_per_message = 100;

        // Both directions, for the inflated configuration above and for the shipped default:
        // every tool measuring over the budget is named in a warning, and every warning names a
        // tool that really is over.
        //
        // Deliberately no hard-coded byte figure and no assumption that the default fits. The
        // previous version asserted 5001 bytes and an empty warning set for the default, both of
        // which came from measuring without Codex's sanitize pass. Pinning either would tie this
        // test to a defect rather than to the contract, and the second one made a live breach of
        // the shipped catalogue look like a passing case.
        for (label, config) in [
            ("shipped default", crate::config::Config::default()),
            ("every schema knob widened", config),
        ] {
            let catalogue = crate::mcp_server::advertised_tools(&config);
            let budget = resolved_budget(
                all_limits()
                    .find(|limit| limit.name == "codex-input-schema-bytes")
                    .expect("the row exists"),
                &config.mcp.client_limits,
            );
            let tools = catalogue
                .as_array()
                .expect("advertised_tools returns an array")
                .clone();
            let over: std::collections::BTreeSet<String> = tools
                .iter()
                .filter(|tool| codex_measured_bytes(&tool["inputSchema"]) > budget)
                .map(|tool| tool["name"].as_str().unwrap_or_default().to_owned())
                .collect();
            let warnings = configured_catalogue_warnings(&catalogue, &config.mcp.client_limits);
            let warned: std::collections::BTreeSet<String> = tools
                .iter()
                .map(|tool| tool["name"].as_str().unwrap_or_default().to_owned())
                .filter(|name| {
                    warnings.iter().any(|warning| {
                        warning.contains(name.as_str())
                            && warning.contains("codex-input-schema-bytes")
                    })
                })
                .collect();
            assert_eq!(
                over, warned,
                "{label}: the set of tools over the {budget}-byte budget and the set the operator \
                 is warned about must be the same set; warnings were {warnings:?}"
            );
        }
    }

    /// Every configuration knob that reaches the schema, swept, so no single one proves the budget.
    ///
    /// The schema is a function of configuration and the release gate measures one configuration.
    /// This is the matrix that closes that gap. It does not assert that every configuration fits,
    /// because that is not true and asserting it would only pin the fiction: a purpose bundle puts
    /// user-chosen names into an `enum` Codex keeps, and no bound on an arbitrary-length name
    /// exists. It asserts the property that is achievable and is the one an operator needs -- a
    /// configuration either fits, or the server says out loud that it does not.
    #[test]
    fn no_configuration_breaches_the_budget_without_telling_the_operator() {
        /// One configuration in the matrix: what to call it, and what it changes.
        type ConfiguredCase = (&'static str, Box<dyn Fn(&mut crate::config::Config)>);

        let cases: Vec<ConfiguredCase> = vec![
            ("default", Box::new(|_: &mut crate::config::Config| {})),
            (
                "page raised to four digits",
                Box::new(|config: &mut crate::config::Config| {
                    config.mcp.search_messages_limit = 1_000;
                }),
            ),
            (
                "preview chars raised to five digits",
                Box::new(|config: &mut crate::config::Config| config.mcp.preview_chars = 10_000),
            ),
            (
                "line window raised to three digits",
                Box::new(|config: &mut crate::config::Config| config.mcp.lines_per_message = 100),
            ),
            (
                // The realistic trigger, and the only one with no bound on what it can add. A
                // purpose name is a user-chosen string landing in an `enum` Codex keeps and in
                // the prose beside it, so one bundle costs about a hundred bytes and each further
                // one about forty-five. One bundle used to breach on its own, when the margin was
                // five bytes; it now fits, which is the point of having streamlined the
                // descriptions, and the case that still crosses is the operator with ten.
                "one configured purpose bundle",
                Box::new(|config: &mut crate::config::Config| {
                    with_purpose_bundles(config, 1);
                }),
            ),
            (
                "ten configured purpose bundles",
                Box::new(|config: &mut crate::config::Config| {
                    with_purpose_bundles(config, 10);
                }),
            ),
            (
                "every integer knob at its widest",
                Box::new(|config: &mut crate::config::Config| {
                    config.mcp.search_messages_limit = usize::MAX;
                    config.mcp.preview_chars = usize::MAX;
                    config.mcp.lines_per_message = i64::MAX;
                    config.mcp.summary_items = i64::MIN;
                    config.mcp.get_session_transcript_lines = i64::MIN;
                    config.mcp.search_sessions_limit = usize::MAX;
                    config.mcp.list_sessions_limit = usize::MAX;
                    config.mcp.query_max_cell_chars = usize::MAX;
                }),
            ),
        ];

        let mut breaching_cases = 0usize;
        for (label, apply) in cases {
            let mut config = crate::config::Config::default();
            apply(&mut config);
            let listed = crate::mcp_server::advertised_tools(&config);
            let overrides = &config.mcp.client_limits;
            let breaches: Vec<&Finding> =
                evaluate(listed.as_array().expect("tools list"), overrides, false)
                    .leak()
                    .iter()
                    .filter(|finding| finding.status == Status::Fail)
                    .collect();
            let warnings = configured_catalogue_warnings(&listed, overrides);
            assert_eq!(
                breaches.is_empty(),
                warnings.is_empty(),
                "{label}: a breach must be announced and an announcement must have a breach; \
                 breaches={:?} warnings={warnings:?}",
                breaches
                    .iter()
                    .map(|finding| (finding.limit.name, finding.tool.as_str(), finding.measured))
                    .collect::<Vec<_>>()
            );
            breaching_cases += usize::from(!breaches.is_empty());
        }

        // A matrix where nothing breaches proves only that the matrix is too timid. A sweep that
        // never crosses the line is not exercising the property it claims to, so when the margin
        // widens the matrix widens with it rather than the assertion being relaxed.
        assert!(
            breaching_cases > 0,
            "no configuration in the matrix reached a breach, so the announcement path above was \
             never exercised. Widen the matrix rather than trusting this test."
        );
    }

    /// The numbers are client policy, and clients move, so they are configuration and not literals.
    ///
    /// Codex already raised its schema budget from 4,000 to 5,000. An operator tracking a client
    /// that moved must be able to say so in `config.toml` rather than wait for a release built
    /// against the new number.
    #[test]
    fn a_configured_client_limit_replaces_the_measured_default() {
        let mut config = crate::config::Config::default();
        with_purpose_bundles(&mut config, 10);
        let listed = crate::mcp_server::advertised_tools(&config);
        let tools = listed.as_array().expect("tools list");

        assert!(
            !configured_catalogue_warnings(&listed, &config.mcp.client_limits).is_empty(),
            "this configuration must breach the shipped 5000, or the raised-budget half of this \
             test proves nothing"
        );

        // The same catalogue, against an operator who tracks a client that raised its budget.
        config
            .mcp
            .client_limits
            .insert("codex-input-schema-bytes".to_owned(), 6_000);
        assert!(
            configured_catalogue_warnings(&listed, &config.mcp.client_limits).is_empty(),
            "a configured budget must replace the compiled default"
        );
        assert!(
            evaluate(tools, &config.mcp.client_limits, false)
                .iter()
                .all(|finding| finding.status != Status::Fail),
            "the sweep must read the same configured budget the warning does"
        );
    }

    /// A row's budget resolves per harness: registration environment, then config file, then the
    /// measured default.
    ///
    /// `[mcp.client_limits]` is one setting for every harness a machine serves, and the numbers
    /// are per client: Codex bounds a schema at 5,000 bytes and Claude Code does not bound it at
    /// all. A registration's own `env` block is the only place a deployment can give one harness
    /// a different budget without giving every harness the same one, which is the same reason the
    /// result ceiling already reads `AI_SESSION_SEARCH_MAX_TOOL_RESULT_CHARS` from there.
    #[test]
    fn a_client_limit_resolves_from_the_registration_environment_first() {
        let row = "codex-input-schema-bytes";
        let shipped = all_limits()
            .find(|limit| limit.name == row)
            .expect("the row exists");

        let mut from_file = ClientLimitOverrides::new();
        from_file.insert(row.to_owned(), 6_000);
        assert_eq!(resolved_budget(shipped, &from_file), 6_000);
        assert_eq!(
            resolved_budget(shipped, &ClientLimitOverrides::new()),
            shipped.budget,
            "an unset row keeps the value measured from the client's own source"
        );

        // What a registration writes, and what reading it back has to produce.
        assert_eq!(
            crate::config::client_limit_env_var(row),
            "AI_SESSION_SEARCH_CLIENT_LIMIT_CODEX_INPUT_SCHEMA_BYTES"
        );
        let captured = crate::config::client_limits_from_environment(
            [(
                "AI_SESSION_SEARCH_CLIENT_LIMIT_CODEX_INPUT_SCHEMA_BYTES".to_owned(),
                "7000".to_owned(),
            )]
            .into_iter(),
        );
        // Captured raw; `Config::resolve` owns the parse so a bad value is rejected against
        // the variable that carried it rather than dropped.
        assert_eq!(captured.get(row).map(String::as_str), Some("7000"));

        // Environment over config file, so one harness can differ from the machine default.
        let mut merged = from_file.clone();
        merged.insert(
            row.to_owned(),
            captured.get(row).unwrap().parse().expect("numeric fixture"),
        );
        assert_eq!(
            resolved_budget(shipped, &merged),
            7_000,
            "the registration's own value must win over the machine-wide one"
        );
    }

    /// A variable naming no row is rejected, not ignored.
    ///
    /// Silently ignoring it leaves the shipped budget in force while the registration looks like
    /// it raised one, which is the same defect as a limit that appears configured and is not.
    #[test]
    fn an_environment_variable_naming_no_row_is_rejected() {
        let captured = crate::config::client_limits_from_environment(
            [(
                "AI_SESSION_SEARCH_CLIENT_LIMIT_CODEX_INPUT_SCHEMA_BYTE".to_owned(),
                "7000".to_owned(),
            )]
            .into_iter(),
        );
        let mut config = crate::config::Config::default();
        for (row, raw) in captured {
            config
                .mcp
                .client_limits
                .insert(row, raw.parse().expect("numeric fixture"));
        }
        let error = config
            .validate()
            .expect_err("a row name with a typo must not load");
        assert!(
            format!("{error:#}").contains("codex-input-schema-byte"),
            "the error must name the key that failed: {error:#}"
        );
    }

    /// The tool-name row measures the name it was given, so a configured budget changes the
    /// verdict in both directions, and a charset violation fails at any budget.
    ///
    /// This row's `raise_when` says "The Anthropic API relaxes its pattern", and the only way to
    /// track that between releases is `[mcp.client_limits]`. A sentinel measurement that never
    /// reads the budget makes that override dead: a lowered budget admits names it should
    /// reject, and a raised one admits names whose charset no length can cure.
    #[test]
    fn the_tool_name_budget_is_live_rather_than_a_sentinel() {
        let row = "anthropic-tool-name-charset";
        let verdict = |tool: Value, overrides: &ClientLimitOverrides| {
            evaluate(&[tool], overrides, true)
                .into_iter()
                .find(|finding| finding.limit.name == row)
                .expect("the row is swept")
                .status
        };

        let mut long_valid = minimal_tool();
        long_valid["name"] = json!("a".repeat(70));
        assert_eq!(
            verdict(long_valid.clone(), &ClientLimitOverrides::new()),
            Status::Fail,
            "70 characters is over the shipped 64"
        );
        let mut relaxed = ClientLimitOverrides::new();
        relaxed.insert(row.to_owned(), 100);
        assert_eq!(
            verdict(long_valid, &relaxed),
            Status::Pass,
            "a raised budget must admit the longer name it was raised for"
        );

        let mut mid = minimal_tool();
        mid["name"] = json!("b".repeat(40));
        let mut tightened = ClientLimitOverrides::new();
        tightened.insert(row.to_owned(), 32);
        assert_eq!(
            verdict(mid, &tightened),
            Status::Fail,
            "a lowered budget must reject a name only the shipped one admits"
        );

        let mut dotted = minimal_tool();
        dotted["name"] = json!("search.messages");
        assert_eq!(
            verdict(dotted, &relaxed),
            Status::Fail,
            "characters outside the charset are rejected at registration, and no length budget \
             admits them"
        );
    }

    /// Claude Code and VS Code cap description length in JavaScript, where `.length` counts
    /// UTF-16 code units, so a supplementary-plane character costs two against the client's
    /// budget. The checker must count the same units, or it reports PASS on text the client
    /// silently truncates.
    #[test]
    fn description_budgets_count_utf16_units_as_the_javascript_clients_do() {
        // 1,030 scalar values, 2,060 UTF-16 units: inside the 2,048 cap by scalar count, over
        // it as Claude Code counts.
        let mut tool = minimal_tool();
        tool["description"] = json!("\u{1D11E}".repeat(1_030));
        assert!(
            failed_rules(tool).contains(&"claude-code-tool-description-chars"),
            "2,060 UTF-16 units must breach the 2,048-unit cap"
        );

        // 513 scalar values, 1,026 UTF-16 units against VS Code's 1,024.
        let mut tool = minimal_tool();
        tool["inputSchema"]["properties"]["query"]["description"] = json!("\u{1D11E}".repeat(513));
        assert!(
            failed_rules(tool).contains(&"vscode-parameter-description-chars"),
            "1,026 UTF-16 units must breach the 1,024-unit cap"
        );
    }

    /// A structural rule's breach warning must not advise the budget override that validation
    /// rejects and that could not make the schema acceptable anyway.
    #[test]
    fn a_structural_breach_does_not_advise_a_budget_override() {
        let mut tool = minimal_tool();
        tool["inputSchema"]["oneOf"] = json!([{ "required": ["query"] }]);
        let listed = json!([tool]);
        let warnings = configured_catalogue_warnings(&listed, &ClientLimitOverrides::new());
        let combinator: Vec<&String> = warnings
            .iter()
            .filter(|warning| warning.contains("vscode-no-root-combinator"))
            .collect();
        assert!(
            !combinator.is_empty(),
            "the breach is reported: {warnings:?}"
        );
        for warning in combinator {
            assert!(
                !warning.contains("[mcp.client_limits]"),
                "a structural rule advises fixing the schema, not the number: {warning}"
            );
        }
    }

    /// A delivery ceiling above a silently-truncating client's cap is reported.
    ///
    /// `mcp.max_tool_result_chars` decides what this server will deliver; a client's own cap
    /// decides what survives. The shipped 48,000 is Codex's, chosen because Codex is the measured
    /// client that truncates from the middle with no marker while the others announce and persist.
    /// Raising the ceiling past it reinstates exactly that silent truncation, and every layer
    /// below is working as designed while it happens: the response is under the configured
    /// ceiling, so nothing errors, and Codex deletes the middle without saying so.
    ///
    /// Only silent rows are compared. Exceeding an announced cap costs a round trip and a file
    /// read, not data, and warning about it would train the operator to ignore the channel.
    #[test]
    fn a_delivery_ceiling_above_a_silent_client_cap_is_reported() {
        let overrides = ClientLimitOverrides::new();
        let tightest_silent = all_limits()
            .filter(|limit| {
                limit.applies_to == AppliesTo::Response && limit.failure_mode == FailureMode::Silent
            })
            .map(|limit| limit.budget)
            .min()
            .expect("at least one measured client truncates a result silently");

        assert!(
            configured_ceiling_warnings(tightest_silent, &overrides).is_empty(),
            "a ceiling exactly at the cap delivers nothing the client will cut"
        );
        assert!(
            configured_ceiling_warnings(
                crate::config::Config::default().mcp.max_tool_result_chars,
                &overrides,
            )
            .is_empty(),
            "the shipped default must not warn about itself"
        );

        let warnings = configured_ceiling_warnings(tightest_silent + 1, &overrides);
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("max_tool_result_chars")
                    && warning.contains(&tightest_silent.to_string())),
            "one character past the cap must name the ceiling and the cap: {warnings:?}"
        );

        // An operator tracking a client that raised its own cap says so, and the warning stops.
        let mut raised = ClientLimitOverrides::new();
        for limit in all_limits().filter(|limit| {
            limit.applies_to == AppliesTo::Response && limit.failure_mode == FailureMode::Silent
        }) {
            raised.insert(limit.name.to_owned(), tightest_silent * 4);
        }
        assert!(
            configured_ceiling_warnings(tightest_silent + 1, &raised).is_empty(),
            "a configured client cap must govern this check too"
        );
    }

    /// Strict mode fails an over-ceiling response even before the default gate is tightened.
    #[test]
    fn a_response_past_an_enforced_cap_fails_the_sweep() {
        let findings = evaluate_all(
            &[minimal_tool()],
            Some(("search_messages", 10_000_000)),
            &ClientLimitOverrides::new(),
            true,
        );

        let breached: Vec<&'static str> = findings
            .iter()
            .filter(|finding| {
                finding.limit.applies_to == AppliesTo::Response && finding.status != Status::Pass
            })
            .map(|finding| finding.limit.name)
            .collect();
        assert!(
            !breached.is_empty(),
            "ten million characters breaches every declared result cap"
        );
        assert!(
            findings.iter().any(|finding| {
                finding.limit.applies_to == AppliesTo::Response && finding.status == Status::Fail
            }),
            "strict mode must fail a non-blocking response breach"
        );
    }
}
