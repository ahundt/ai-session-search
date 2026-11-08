// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

//! External client, alias, instruction, and skill integration lifecycle.

use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{json, Map, Value};

use crate::config::{IntegrationSurface, IntegrationsConfig};
use crate::text_file_transaction::{
    apply_symlink_change_after, execute_text_file_and_symlink_transaction_with,
    publish_symlink_change, recover_text_file_transaction, recovery_guidance,
    snapshot_symlink_image, snapshot_utf8_regular_file, transaction_recovery_required,
    with_text_file_transaction_read_lock, RecoveryOutcome, SymlinkChange, SymlinkImage,
    TextFileChange, TextFileImage,
};
#[cfg(test)]
use crate::text_file_transaction::{execute_text_file_transaction, publish_text_change};
use crate::util::which;
// Only the `#[cfg(unix)]` executable-bit branch renders a shell command, so an unconditional
// import is dead on Windows and warns there while Clippy's `-D warnings` job runs on Linux.
#[cfg(unix)]
use crate::util::render_posix_shell_command;

// Keep the MCP identity aligned with the product and distribution slug. `aise` is the executable;
// `ai_session_search` is a language identifier and a historical client configuration key.
const SERVER_NAME: &str = "ai-session-search";
const LEGACY_SERVER_NAMES: [&str; 2] = ["ai_session_search", "aise"];
/// Pi's default agent root and MCP filename:
/// https://github.com/earendil-works/pi/blob/c23b9ec1ef12a63573d739d4c294f81a377b7d57/packages/coding-agent/docs/configuration.md#agent-directory
const PI_AGENT_DIR: &str = ".pi/agent";
/// MCP file under the agent root, from the same Pi configuration reference above.
const PI_MCP_FILE: &str = "mcp.json";
/// Prime Agent's default root:
/// https://github.com/PrimeIntellect-ai/prime-agent/blob/74591cddef28d11249c3171112114fc4755375d7/crates/pa-cli/src/config.rs#L7-L9
const PRIME_AGENT_DIR: &str = ".prime/agent";
/// MCP shares the strict-JSON settings document:
/// https://github.com/PrimeIntellect-ai/prime-agent/blob/74591cddef28d11249c3171112114fc4755375d7/crates/pa-cli/src/mcp_command.rs#L77-L85
const PRIME_MCP_FILE: &str = "settings.json";
/// Cursor's global MCP location:
/// https://cursor.com/docs/mcp#configuration-locations
const CURSOR_MCP_PATH: &str = ".cursor/mcp.json";
/// Claude Code's user-scoped MCP file:
/// https://code.claude.com/docs/en/mcp#mcp-installation-scopes
const CLAUDE_USER_MCP_FILE: &str = ".claude.json";
/// Former aise global target. Claude documents `.mcp.json` only at the project root:
/// https://code.claude.com/docs/en/claude-directory#choose-the-right-file
const CLAUDE_SUPERSEDED_MCP_PATH: &str = ".claude/.mcp.json";
/// Gemini v0.28.0+ and Codex share this root to avoid literal-path skill conflicts:
/// https://github.com/google-gemini/gemini-cli/blob/62bab5b232c15b925d31e655ce844ab939c882b7/packages/core/src/skills/skillManager.ts#L71-L79
const SHARED_SKILLS_PATH: &str = ".agents/skills";
/// Former aise Gemini root, still read alongside the shared root:
/// https://github.com/google-gemini/gemini-cli/blob/62bab5b232c15b925d31e655ce844ab939c882b7/packages/core/src/config/storage.ts#L142-L148
const GEMINI_RETIRED_SKILLS_PATH: &str = ".gemini/skills";
const INSTRUCTIONS_FILE: &str = "AI_SESSION_SEARCH.md";
const INSTRUCTIONS_REFERENCE: &str = "@AI_SESSION_SEARCH.md";
const LEGACY_INSTRUCTIONS_LINE: &str = "Before guessing about prior AI work, use aise MCP or run `aise messages search --help` to recover session history from Claude Code, Claude Desktop local agent, Codex, Cursor, Antigravity, Pi coding agent, Google AI Studio, and Gemini CLI by query, repo/path/file, message context, and time range.";
/// The block for a harness that registers the MCP server (and the server's own `initialize`
/// instructions). It names the same four steps as `CLI_INSTRUCTIONS_LINE` in the same order,
/// with the MCP tool for each, so an agent reads one workflow whichever surface it holds.
const INSTRUCTIONS_LINE: &str = "Before guessing about prior AI work, use AI Session Search (`aise`) via the `ai-session-search` MCP server: `list_sessions(path_prefix, when, limit)` for recent sessions by directory or period, `search_sessions(query, when, limit)` for sessions by topic, `search_messages(query, context, limit)` for the exact turn, then read it with `get_session(session_id, message_seq)`. It searches Claude Code, Claude Desktop local agent, Codex, Cursor, Antigravity, Pi coding agent, Prime Agent, Google AI Studio, and Gemini CLI by query, path, context, and time range. The installed `ai-session-search` skill has the full workflow; without MCP, `aise` on the shell does the same (`aise <command> --help`).";
/// Historical CLI-only guidance, retained verbatim to recognize and refresh owned blocks.
const CLI_INSTRUCTIONS_LINE: &str = "Before guessing about prior AI work, use AI Session Search (`aise`) from the shell; on this harness the `aise` command and the installed skill are the whole integration. List recent sessions with `aise list --path <dir> --when 7d --limit 10`, find sessions by topic with `aise search \"<topic>\" --when 30d --limit 10`, find the exact turn with `aise messages search \"<phrase>\" --context 2 --limit 20`, then read it with `aise messages get <session-id> --seq <N> --context 3` or `aise show <session-id>`. It searches Claude Code, Claude Desktop local agent, Codex, Cursor, Antigravity, Pi coding agent, Prime Agent, Google AI Studio, and Gemini CLI by query, repo/path/file, message context, and time range. The installed `ai-session-search` skill documents the full workflow; `aise <command> --help` lists every current flag.";
/// SHA-256 of each instruction line an earlier release wrote and this build no longer does:
/// 1.0.0rc1's `INSTRUCTIONS_LINE`. The post-upgrade refresh replaces a block that still holds one
/// of these, or any line above, because aise wrote it; any other text is the user's. When a line
/// above changes, add the digest of its old text here.
const EARLIER_INSTRUCTION_LINE_DIGESTS: [&str; 1] =
    ["3efca133a9dfaf9e359dce78582c83601c4a31c832ebfd6ef1caf5aa4398cfc6"];
const INSTRUCTIONS_START: &str = "<!-- aise-instructions";
const INSTRUCTIONS_END: &str = "<!-- /aise-instructions -->";
const INSTRUCTIONS_FILE_START: &str = "<!-- ai-session-search-managed-file v1 -->";
const INSTRUCTIONS_FILE_END: &str = "<!-- /ai-session-search-managed-file -->";
/// Ownership stamp `aise` writes into every `SKILL.md` it manages.
///
/// `pub(crate)` because ownership is now *reported* by `aise skills list` as well as
/// enforced here: the same byte string has to mean the same thing on both surfaces.
pub(crate) const SKILL_MANAGED_MARKER: &str = "<!-- ai-session-search-managed-skill v1 -->";

/// Whether `text` carries the ownership marker in its exact managed-anchor position.
///
/// Quoting the marker later in prose or a code fence must never authorize an overwrite.
pub(crate) fn is_managed_skill_anchor(text: &str) -> bool {
    let mut lines = text.lines();
    if lines.next() != Some("---") {
        return false;
    }
    for line in lines.by_ref() {
        if line == "---" {
            return lines.next() == Some(SKILL_MANAGED_MARKER);
        }
    }
    false
}
/// One file of the managed skill, relative to the skill root.
///
/// Adding a file to the bundled skill is a row here, not a new code path: install, status, and
/// uninstall all iterate this slice.
#[derive(Debug)]
struct ManagedSkillFile {
    relative_path: &'static str,
    content: &'static str,
}

/// One embedded Agent Skill package managed as a unit.
#[derive(Debug)]
struct ManagedSkillPackage {
    name: &'static str,
    files: &'static [ManagedSkillFile],
}

/// Every file `aise` writes into the general harness-guidance skill directory.
///
/// An explicit list rather than a directory walk at build time: it is greppable, dependency-free,
/// and puts the complete set of managed paths in one auditable place, which is what makes
/// "modified or damaged" and "unmanaged extra file" answerable at all.
///
/// EVERY entry must be UTF-8 text. `text_file_transaction::snapshot_utf8_regular_file` refuses
/// non-UTF-8, so a binary asset could be embedded here yet not installed by this path.
///
/// `include_str!` resolves relative to THIS source file, so these read the crate-local mirror
/// under `rust/ai-session-search-core/skills/`, held byte-identical to the repo-root copy by
/// `tests/test_repository_contracts.py`.
/// One package rather than two: the Agent Skills specification requires a skill's `name` to match
/// its directory name, so a second package called `corrections` necessarily claims that generic
/// name in every flat skill root (`~/.claude/skills`, `~/.agents/skills`, ...), and its description
/// is loaded at startup in every session whether or not classification is ever run. The
/// specification explicitly permits any additional files beside `SKILL.md`, so message
/// classification ships here as side files instead.
const AI_SESSION_SEARCH_SKILL_FILES: &[ManagedSkillFile] = &[
    ManagedSkillFile {
        relative_path: "SKILL.md",
        content: include_str!("../skills/ai-session-search/SKILL.md"),
    },
    ManagedSkillFile {
        relative_path: crate::skill_catalog::CAPABILITY_FILE,
        content: include_str!("../skills/ai-session-search/aise-capability.toml"),
    },
    ManagedSkillFile {
        relative_path: "references/recover-prior-work-with-evidence.md",
        content: include_str!(
            "../skills/ai-session-search/references/recover-prior-work-with-evidence.md"
        ),
    },
    ManagedSkillFile {
        relative_path: "references/message-classification.md",
        content: include_str!("../skills/ai-session-search/references/message-classification.md"),
    },
    ManagedSkillFile {
        relative_path: "references/search-costs-and-receipts.md",
        content: include_str!(
            "../skills/ai-session-search/references/search-costs-and-receipts.md"
        ),
    },
];
pub(crate) const AI_SESSION_SEARCH_SKILL_NAME: &str = "ai-session-search";

const AI_SESSION_SEARCH_SKILL_PACKAGE: ManagedSkillPackage = ManagedSkillPackage {
    name: AI_SESSION_SEARCH_SKILL_NAME,
    files: AI_SESSION_SEARCH_SKILL_FILES,
};
const MANAGED_SKILL_PACKAGES: &[&ManagedSkillPackage] = &[&AI_SESSION_SEARCH_SKILL_PACKAGE];

/// Sibling package retired when message classification moved into this one.
///
/// Removed as a whole skill root rather than file by file: it was installed with its own
/// `SKILL.md` anchor, so ownership is decided by that anchor's marker and the install manifest,
/// exactly as uninstall decides it.
const RETIRED_SKILL_PACKAGE_NAME: &str = "corrections";
const RETIRED_SKILL_PACKAGE: ManagedSkillPackage = ManagedSkillPackage {
    name: RETIRED_SKILL_PACKAGE_NAME,
    files: AI_SESSION_SEARCH_SKILL_FILES,
};

/// Paths written by the prerelease layout before message classification became its own package.
///
/// Their bytes are intentionally not embedded. A valid install manifest is the ownership
/// authority, and migration removes a path only when the current bytes still match its recorded
/// digest.
const HISTORICAL_CORRECTION_PATHS: &[&str] = &[
    "corrections/policy.toml",
    "references/corrections-policy.md",
];

/// `SKILL.md` is the ownership anchor: the marker lives in it, so it decides whether `aise` may
/// write ANY file in the directory. Kept as a named constant because three call sites depend on
/// it being the same file.
const SKILL_ANCHOR_FILE: &str = "SKILL.md";

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum McpClient {
    All,
    Claude,
    Codex,
    Gemini,
    Antigravity,
    Pi,
    PrimeAgent,
    Cursor,
    Windsurf,
    Vscode,
    Zed,
    Opencode,
    Openclaw,
    Kilocode,
}

#[derive(Debug, Default, Args)]
pub struct IntegrationTargetsArgs {
    /// Harness/client to include. Repeat for multiple selections; omit for all detected harnesses.
    #[arg(long = "client", value_enum, default_value = "all")]
    pub clients: Vec<McpClient>,
    /// Harness/client to exclude from the selected set. Repeat for multiple selections.
    /// Omit to exclude none.
    #[arg(long = "exclude-client", value_enum)]
    pub excluded_clients: Vec<McpClient>,
    /// Extra JSON config path using the common { "mcpServers": ... } shape. Repeatable;
    /// omit to register only the clients discovery finds.
    #[arg(long = "json-mcp-config")]
    pub json_mcp_configs: Vec<PathBuf>,
    /// Extra VS Code-style JSON config path using { "servers": ... }. Repeatable; omit to
    /// register only the clients discovery finds.
    #[arg(long = "vscode-config")]
    pub vscode_configs: Vec<PathBuf>,
    /// Extra Zed JSON config path using { "context_servers": ... }. Repeatable; omit to
    /// register only the clients discovery finds.
    #[arg(long = "zed-config")]
    pub zed_configs: Vec<PathBuf>,
    /// Extra OpenCode JSON config path using { "mcp": { "servers": ... } }. Repeatable; omit to register
    /// only the clients discovery finds.
    #[arg(long = "opencode-config")]
    pub opencode_configs: Vec<PathBuf>,
    /// Extra Codex-style TOML config path using [mcp_servers.ai-session-search]. Repeatable;
    /// omit to register only the clients discovery finds.
    #[arg(long = "codex-config")]
    pub codex_configs: Vec<PathBuf>,
    /// Extra CLAUDE.md path where @AI_SESSION_SEARCH.md is managed. Repeatable; omit to
    /// manage only the discovered instruction files.
    #[arg(long = "claude-md")]
    pub claude_md_paths: Vec<PathBuf>,
    /// Extra GEMINI.md path where the managed AI Session Search (`aise`) note is managed.
    /// Repeatable; omit to manage only the discovered instruction files.
    #[arg(long = "gemini-md")]
    pub gemini_md_paths: Vec<PathBuf>,
    /// Extra AGENTS.md path where the managed AI Session Search (`aise`) note is managed.
    /// Repeatable; omit to manage only the discovered instruction files.
    #[arg(long = "agents-md")]
    pub agents_md_paths: Vec<PathBuf>,
    /// Exact managed skill DIRECTORY to install into, ending in `ai-session-search`,
    /// e.g. `~/.config/myagent/skills/ai-session-search`.
    ///
    /// The directory itself, not a file inside it. Each skill package keeps SKILL.md, its optional
    /// adjacent aise-capability.toml, and references together. Repeat for several exact package roots.
    /// Distinct from `aise skills <name-or-path>`, which runs a selected deterministic capability
    /// rather than naming an installation destination. Omit to install into the discovered
    /// skill roots.
    #[arg(long = "skill-root", value_name = "DIR")]
    pub skill_roots: Vec<PathBuf>,
}

impl IntegrationTargetsArgs {
    fn resolve(
        &self,
        no_instructions: bool,
        no_skill: bool,
        layout: &ClientLayout,
    ) -> Result<(Vec<Target>, Vec<InstructionTarget>, Vec<SkillTarget>)> {
        assemble_selected_targets_for_layout(
            McpTargetSelection {
                clients: &self.clients,
                excluded_clients: &self.excluded_clients,
                no_instructions,
                json_mcp_configs: &self.json_mcp_configs,
                vscode_configs: &self.vscode_configs,
                zed_configs: &self.zed_configs,
                opencode_configs: &self.opencode_configs,
                codex_configs: &self.codex_configs,
                claude_md_paths: &self.claude_md_paths,
                gemini_md_paths: &self.gemini_md_paths,
                agents_md_paths: &self.agents_md_paths,
                no_skill,
                skill_roots: &self.skill_roots,
            },
            layout,
        )
    }
}

#[derive(Debug, Args)]
#[command(
    after_help = "Default install configures executable aliases plus each detected harness's supported integrations. MCP-capable clients receive the local stdio server; Pi and Prime Agent also receive their native skill and MCP-first AGENTS.md guidance. Supported selectors: Claude Code/Desktop, ChatGPT Codex desktop and CLI/IDE, Gemini, Antigravity App/IDE/CLI, Pi, Prime Agent, Cursor, Windsurf, VS Code, Zed, OpenCode, OpenClaw, and KiloCode. Config shapes use the `ai-session-search` server key: mcpServers.ai-session-search, [mcp_servers.ai-session-search], VS Code servers.ai-session-search, Zed context_servers.ai-session-search, OpenCode and OpenClaw mcp.servers.ai-session-search, or Kilo mcp.ai-session-search as appropriate. Reinstall migrates the historical `ai_session_search` and `aise` keys, and entries in places a client no longer reads, without leaving duplicate servers. Use --no-mcp, --no-aliases, --no-instructions, or --no-skill to omit one component; --client selects specific clients; --dry-run previews every write. Canonical skill packages live under ~/.ai-session-search/skills, beside the app config and integration manifest. Harness-native discovery entries link to them from ~/.claude/skills, ~/.agents/skills (Codex and Gemini), ~/.gemini/config/skills, ~/.gemini/antigravity-cli/skills, ~/.pi/agent/skills, and ~/.prime/agent/skills. Repeat --skill-root for exact additional package destinations. After a non-dry-run installation commits, aise starts best-effort session index preparation in the background; run `aise doctor` to check readiness and freshness."
)]
pub struct IntegrationInstallArgs {
    #[command(flatten)]
    pub targets: IntegrationTargetsArgs,
    /// Print planned changes without writing files.
    #[arg(long)]
    pub dry_run: bool,
    /// Executable path to store in client configs. Omit to resolve the first `aise` on the
    /// installer's PATH and store its absolute path, so GUI clients and shells launch the same
    /// installation. Pass an explicit path to select a different installation.
    #[arg(long)]
    pub binary: Option<PathBuf>,
    /// Do not add AI Session Search MCP registrations to client configuration files.
    #[arg(long)]
    pub no_mcp: bool,
    /// Do not add AI Session Search (`aise`) guidance to CLAUDE.md, AGENTS.md, or GEMINI.md.
    #[arg(long)]
    pub no_instructions: bool,
    /// Do not install the AI Session Search skill for any selected skill-capable harness.
    #[arg(long)]
    pub no_skill: bool,
    /// Do not create the `aisearch` and `ai_session_search` executable aliases beside `aise`.
    #[arg(long)]
    pub no_aliases: bool,
    #[command(flatten)]
    pub transaction: IntegrationTransactionArgs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntegrationInstallOutcome {
    Configured,
    Previewed,
    NoTargets,
}

impl IntegrationInstallOutcome {
    pub(crate) fn should_start_initial_indexing(self) -> bool {
        matches!(self, Self::Configured)
    }
}

#[derive(Debug, Args)]
#[command(
    after_help = "Status checks MCP registrations, executable aliases, managed instructions, and installed AI Session Search skills by default. Use --no-mcp, --no-aliases, --no-instructions, or --no-skill to omit one component."
)]
pub struct IntegrationStatusArgs {
    #[command(flatten)]
    pub targets: IntegrationTargetsArgs,
    /// Do not inspect MCP registrations in client configuration files.
    #[arg(long)]
    pub no_mcp: bool,
    /// Do not inspect CLAUDE.md, AGENTS.md, or GEMINI.md instruction files.
    #[arg(long)]
    pub no_instructions: bool,
    /// Do not inspect installed AI Session Search skills.
    #[arg(long)]
    pub no_skill: bool,
    /// Do not inspect the `aisearch` and `ai_session_search` executable aliases.
    #[arg(long)]
    pub no_aliases: bool,
    /// Output format: table for people or JSON for scripts. Each JSON entry has `component`,
    /// `client`, `path`, `state`, and `current`, which is true when it matches this build.
    #[arg(long, value_enum, default_value_t = crate::cli::ReportOutputFormat::Table)]
    pub format: crate::cli::ReportOutputFormat,
    #[command(flatten)]
    pub transaction: IntegrationTransactionArgs,
}

#[derive(Debug, Default, Args)]
#[command(
    after_help = "Uninstall removes owned MCP registrations, executable aliases, managed instructions, and AI Session Search skills by default while preserving the `aise` executable, database, cache, other client configuration, and user-authored files. Use --keep-mcp, --keep-aliases, --keep-instructions, or --keep-skill to preserve one component.\n\nA skill directory is removed only when every file in it is exactly what install recorded. If anything differs, or holds a file aise did not write, the WHOLE directory is preserved and each reason is reported. --force-full-cleanup deletes everything under one exact --skill-root, including your own edits."
)]
pub struct IntegrationUninstallArgs {
    #[command(flatten)]
    pub targets: IntegrationTargetsArgs,
    /// Print planned changes without writing files.
    #[arg(long)]
    pub dry_run: bool,
    /// Preserve MCP registrations while removing other selected owned components.
    #[arg(long)]
    pub keep_mcp: bool,
    /// Preserve AI Session Search (`aise`) guidance while removing MCP registrations.
    #[arg(long)]
    pub keep_instructions: bool,
    /// Preserve installed AI Session Search skills while removing other owned components.
    #[arg(long)]
    pub keep_skill: bool,
    /// Also delete your own edits and any files you added under the skill directory.
    /// This cannot be undone.
    ///
    /// Normal uninstall preserves the whole skill directory whenever anything in it differs from
    /// what aise installed. This removes everything under one exact `--skill-root`, including
    /// files aise never wrote. It requires that exact root, refuses detected-client fan-out, and
    /// never touches your index, cache, or configuration. Pair with `--dry-run` first.
    #[arg(long)]
    pub force_full_cleanup: bool,
    /// Preserve executable aliases while removing MCP registrations and managed instructions.
    #[arg(long)]
    pub keep_aliases: bool,
    #[command(flatten)]
    pub transaction: IntegrationTransactionArgs,
}

#[derive(Debug, Clone, Default, Args)]
pub struct IntegrationTransactionArgs {
    /// Durable recovery receipt. Defaults beside the selected ai-session-search config file.
    /// Omit to use that default location.
    #[arg(long, value_name = "PATH")]
    pub transaction_receipt: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct IntegrationRecoverArgs {
    #[command(flatten)]
    pub transaction: IntegrationTransactionArgs,
}

#[derive(Debug, Subcommand)]
pub enum McpCmd {
    /// Serve MCP JSON-RPC over standard input/output.
    ///
    /// Use `aise integrations install` to register this server with a client, or
    /// `aise integrations status` to see which clients already have it.
    Serve,
    /// Measure the advertised tool catalogue against every client limit it can silently breach.
    ///
    /// Hidden because it answers a maintainer's question about this build's emitted artifacts,
    /// not a user's question about their sessions. `aise mcp schema-budget --help` still works,
    /// and the release gate runs it against the shipped executable so the artifact measured is
    /// the artifact published.
    #[command(hide = true)]
    SchemaBudget(crate::mcp_schema_budget::SchemaBudgetArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigFormat {
    JsonMcpServers,
    /// Pi uses `mcpServers` with optional `type`, parsed with JSON.parse:
    /// https://github.com/earendil-works/pi/blob/c23b9ec1ef12a63573d739d4c294f81a377b7d57/packages/coding-agent/src/extensions/mcp/config.ts#L81-L99
    Pi,
    /// `mcpServers` with a required stdio type tag:
    /// https://github.com/PrimeIntellect-ai/prime-agent/blob/74591cddef28d11249c3171112114fc4755375d7/crates/pa-cli/src/mcp_command.rs#L38-L61
    PrimeAgent,
    /// Cursor's STDIO field table requires `type: "stdio"` under `mcpServers`:
    /// https://cursor.com/docs/mcp#stdio-server-configuration
    Cursor,
    CodexToml,
    VscodeServers,
    ZedContextServers,
    OpenCode,
    /// Kilo Code 7: OpenCode's local-server entry, kept directly under `mcp`.
    Kilo,
    /// OpenClaw: `mcp.servers` in a JSON5 file.
    OpenClaw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientPlatform {
    Macos,
    Linux,
    Windows,
}

#[derive(Debug, Clone)]
struct ClientLayout {
    home: PathBuf,
    canonical_skill_root: PathBuf,
    surfaces: Vec<ClientSurface>,
    environment: LayoutEnvironment,
}

#[derive(Debug, Clone)]
struct ClientSurface {
    name: IntegrationSurface,
    client: McpClient,
    mcp: Vec<Target>,
    instruction: Option<InstructionTarget>,
    skill_roots: Vec<PathBuf>,
    retired_skill_roots: Vec<PathBuf>,
    detect_paths: Vec<PathBuf>,
    binaries: Vec<&'static str>,
}

/// Captured once per layout; tests supply values without changing the process environment.
#[derive(Debug, Clone)]
struct LayoutEnvironment {
    values: std::collections::BTreeMap<String, std::ffi::OsString>,
    cwd: PathBuf,
}

impl LayoutEnvironment {
    const KEYS: &'static [&'static str] = &[
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        "GEMINI_CLI_HOME",
        "PI_CODING_AGENT_DIR",
        "PRIME_AGENT_CODING_AGENT_DIR",
        "XDG_CONFIG_HOME",
        "FLATPAK_XDG_CONFIG_HOME",
        "OPENCLAW_CONFIG_PATH",
        "OPENCLAW_STATE_DIR",
        "OPENCLAW_HOME",
        "OPENCLAW_PROFILE",
    ];

    fn capture() -> Result<Self> {
        Ok(Self {
            values: Self::KEYS
                .iter()
                .filter_map(|key| std::env::var_os(key).map(|value| ((*key).to_string(), value)))
                .collect(),
            cwd: std::env::current_dir()
                .context("resolve harness paths from the working directory")?,
        })
    }

    fn get(&self, key: &str) -> Option<&std::ffi::OsStr> {
        self.values
            .get(key)
            .map(std::ffi::OsString::as_os_str)
            .filter(|value| !value.is_empty())
    }

    fn path(&self, key: &str, fallback: PathBuf) -> PathBuf {
        self.get(key)
            .map(|value| self.absolute(Path::new(value)))
            .unwrap_or(fallback)
    }

    fn absolute(&self, path: &Path) -> PathBuf {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        };
        crate::util::normalize_absolute_path(&absolute)
    }

    fn expanded(&self, path: &Path, home: &Path) -> PathBuf {
        let path = crate::util::expand_tilde_with_home(path, home);
        self.absolute(&path)
    }
}

impl ClientLayout {
    fn with_config(mut self, config: &IntegrationsConfig) -> Result<Self> {
        for surface in &mut self.surfaces {
            let Some(paths) = config.paths.get(&surface.name) else {
                continue;
            };
            paths.validate(surface.name)?;
            let resolve = |path: &str| self.environment.expanded(Path::new(path), &self.home);
            if let Some(path) = &paths.mcp_config {
                // One explicit endpoint replaces compatibility/superseded paths of this surface.
                surface.mcp.truncate(1);
                let target = &mut surface.mcp[0];
                target.path = resolve(path);
                target.detect_paths = target
                    .path
                    .parent()
                    .map(Path::to_path_buf)
                    .into_iter()
                    .collect();
                target.superseded_paths.clear();
            }
            if let Some(path) = &paths.instruction_file {
                if let Some(target) = &mut surface.instruction {
                    target.path = resolve(path);
                    target.detect_paths = target
                        .path
                        .parent()
                        .map(Path::to_path_buf)
                        .into_iter()
                        .collect();
                }
            }
            if let Some(roots) = &paths.skill_discovery_roots {
                surface.skill_roots = roots.iter().map(|path| resolve(path)).collect();
                surface.detect_paths = surface.skill_roots.clone();
            }
        }
        Ok(self)
    }
    #[cfg(test)]
    fn new(home: PathBuf, config: PathBuf, platform: ClientPlatform) -> Self {
        Self::with_environment(home, config, platform, &[]).unwrap()
    }

    #[cfg(test)]
    fn with_environment(
        home: PathBuf,
        config: PathBuf,
        platform: ClientPlatform,
        values: &[(&str, &str)],
    ) -> Result<Self> {
        let environment = LayoutEnvironment {
            values: values
                .iter()
                .map(|(key, value)| (key.to_string(), (*value).into()))
                .collect(),
            cwd: home.clone(),
        };
        Self::from_environment(home, config, platform, &environment)
    }

    fn from_discovered_dirs(
        home: Option<PathBuf>,
        config: Option<PathBuf>,
        platform: ClientPlatform,
    ) -> Result<Self> {
        let home = home.ok_or_else(missing_home_error)?;
        let config = config.unwrap_or_else(|| match platform {
            ClientPlatform::Macos => home.join("Library/Application Support"),
            ClientPlatform::Linux => home.join(".config"),
            ClientPlatform::Windows => home.join("AppData/Roaming"),
        });
        Self::from_environment(home, config, platform, &LayoutEnvironment::capture()?)
    }

    fn discover() -> Result<Self> {
        let platform = if cfg!(target_os = "macos") {
            ClientPlatform::Macos
        } else if cfg!(windows) {
            ClientPlatform::Windows
        } else {
            ClientPlatform::Linux
        };
        Self::from_discovered_dirs(dirs::home_dir(), dirs::config_dir(), platform)
    }

    /// The single defaults table for MCP, instruction, discovery, and detection destinations.
    /// Resolution is linear in the fixed number of surfaces; it performs no filesystem discovery.
    fn from_environment(
        home: PathBuf,
        config: PathBuf,
        platform: ClientPlatform,
        env: &LayoutEnvironment,
    ) -> Result<Self> {
        // https://code.claude.com/docs/en/claude-directory#change-the-location (CLAUDE_CONFIG_DIR).
        // Empty is unset; the global MCP file has a special home-level default.
        let claude = env.path("CLAUDE_CONFIG_DIR", home.join(".claude"));
        let claude_mcp = env
            .get("CLAUDE_CONFIG_DIR")
            .map(|_| claude.join(".claude.json"))
            .unwrap_or_else(|| home.join(CLAUDE_USER_MCP_FILE));
        // https://github.com/openai/codex/blob/main/codex-rs/utils/home-dir/src/lib.rs#L11-L22
        // Empty is unset; nonempty values are literal paths, without tilde expansion.
        // https://learn.chatgpt.com/docs/build-skills (user discovery remains ~/.agents/skills).
        let codex = env.path("CODEX_HOME", home.join(".codex"));
        let agents = home.join(".agents");
        // https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/utils/paths.ts#L20-L26
        // https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/config/storage.ts#L50-L56
        // Truthy alternative HOME, not a replacement for .gemini; Antigravity is independent.
        let gemini = env.path("GEMINI_CLI_HOME", home.clone()).join(".gemini");
        let antigravity = home.join(".gemini");
        // https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/src/config.ts#L587-L593
        let pi = env
            .get("PI_CODING_AGENT_DIR")
            .map(|value| env.expanded(Path::new(value), &home))
            .unwrap_or_else(|| home.join(".pi/agent"));
        // https://github.com/PrimeIntellect-ai/prime-agent/blob/main/prime-agent-runtime/src/rlm/mcp_base.py#L50-L59
        // Named variable only: the runtime's Pi fallback is not verified for host discovery.
        let prime = env
            .get("PRIME_AGENT_CODING_AGENT_DIR")
            .map(|value| env.expanded(Path::new(value), &home))
            .unwrap_or_else(|| home.join(".prime/agent"));
        // https://github.com/sindresorhus/xdg-basedir/blob/main/index.js#L9-L10: || treats empty as unset.
        // https://github.com/anomalyco/opencode/blob/70881b2937fab1e425416659445875952730934a/packages/opencode/src/global/index.ts#L9
        // https://github.com/Kilo-Org/kilocode/blob/main/packages/kilo-jetbrains/README.md#dev-storage-isolation
        let xdg = env.path("XDG_CONFIG_HOME", home.join(".config"));
        let opencode = xdg.join("opencode");
        let kilo = xdg.join("kilo");
        let app_support = home.join("Library/Application Support");
        let native_config = if platform == ClientPlatform::Macos {
            &app_support
        } else {
            &config
        };
        let desktop = native_config.join("Claude");
        let vscode = native_config.join("Code/User");
        // https://github.com/zed-industries/zed/blob/main/crates/paths/src/paths.rs#L111-L129
        // Linux var(Ok) includes an empty Flatpak value, unlike XDG's || semantics.
        // Windows casing remains the existing aise compatibility policy.
        let zed = match platform {
            ClientPlatform::Macos => home.join(".config"),
            ClientPlatform::Linux => env
                .values
                .get("FLATPAK_XDG_CONFIG_HOME")
                .map(|value| {
                    let path = PathBuf::from(value);
                    if path.is_absolute() {
                        path
                    } else {
                        env.cwd.join(path)
                    }
                })
                .unwrap_or_else(|| {
                    env.get("XDG_CONFIG_HOME")
                        .map(PathBuf::from)
                        .filter(|path| path.is_absolute())
                        .unwrap_or_else(|| config.clone())
                }),
            ClientPlatform::Windows => config.clone(),
        }
        .join("zed");
        let zed_superseded = if platform == ClientPlatform::Macos {
            vec![app_support.join("Zed/settings.json")]
        } else {
            Vec::new()
        };
        // https://github.com/openclaw/openclaw/blob/main/src/config/paths.ts#L222-L231
        // https://github.com/openclaw/openclaw/blob/main/src/config/state-dir.ts#L20-L49
        // https://github.com/openclaw/openclaw/blob/main/src/cli/profile-utils.ts#L4-L33
        // OpenClaw trims values, expands tilde, and resolves relative paths from invocation cwd.
        let trimmed = |key: &str| -> Option<&str> {
            env.get(key)
                .and_then(|value| value.to_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
        };
        // https://github.com/openclaw/openclaw/blob/main/packages/normalization-core/src/home-dir.ts#L3-L6
        let claw_home = trimmed("OPENCLAW_HOME")
            .filter(|value| *value != "undefined" && *value != "null")
            .map(|value| env.expanded(Path::new(value), &home))
            .unwrap_or_else(|| home.clone());
        let profile =
            trimmed("OPENCLAW_PROFILE").filter(|value| !value.eq_ignore_ascii_case("default"));
        if let Some(profile) = profile {
            if profile.len() > 64
                || !profile
                    .bytes()
                    .next()
                    .is_some_and(|b| b.is_ascii_alphanumeric())
                || !profile
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                bail!("invalid OPENCLAW_PROFILE {profile:?}; use 1-64 letters, digits, underscores or hyphens, starting with a letter or digit");
            }
        }
        let claw_state = trimmed("OPENCLAW_STATE_DIR")
            .map(|value| env.expanded(Path::new(value), &claw_home))
            .unwrap_or_else(|| {
                claw_home.join(format!(
                    ".openclaw{}",
                    profile.map(|p| format!("-{p}")).unwrap_or_default()
                ))
            });
        let claw_config = trimmed("OPENCLAW_CONFIG_PATH")
            .map(|value| env.expanded(Path::new(value), &claw_home))
            .unwrap_or_else(|| claw_state.join("openclaw.json"));

        let mcp = |label,
                   path: PathBuf,
                   format,
                   detect_paths: Vec<PathBuf>,
                   binaries: Vec<&'static str>,
                   superseded_paths| Target {
            label,
            path,
            format,
            detect_paths,
            detect_binaries: binaries,
            superseded_paths,
            codex_marketplace: None,
        };
        let json = |label, path: PathBuf, roots: Vec<PathBuf>, binaries: Vec<&'static str>| {
            mcp(
                label,
                path,
                ConfigFormat::JsonMcpServers,
                roots,
                binaries,
                Vec::new(),
            )
        };
        let instruction = |label, root: &Path, file, format, binaries: Vec<&'static str>| {
            Some(InstructionTarget {
                label,
                path: root.join(file),
                format,
                detect_paths: vec![root.to_path_buf()],
                detect_binaries: binaries,
            })
        };
        let surface =
            |name, client, mcp, instruction, skill_roots, detect_paths, binaries| ClientSurface {
                name,
                client,
                mcp,
                instruction,
                skill_roots,
                retired_skill_roots: if name == IntegrationSurface::Gemini {
                    vec![home.join(GEMINI_RETIRED_SKILLS_PATH)]
                } else {
                    Vec::new()
                },
                detect_paths,
                binaries,
            };
        let agy_config = antigravity.join("config");
        let agy_cli = antigravity.join("antigravity-cli");
        let agy_legacy = antigravity.join("antigravity");
        let surfaces = vec![
            surface(
                IntegrationSurface::ClaudeCode,
                McpClient::Claude,
                vec![mcp(
                    "claude code modern",
                    claude_mcp,
                    ConfigFormat::JsonMcpServers,
                    vec![claude.clone()],
                    vec!["claude"],
                    vec![home.join(CLAUDE_SUPERSEDED_MCP_PATH)],
                )],
                instruction(
                    "claude",
                    &claude,
                    "CLAUDE.md",
                    InstructionFormat::ClaudeImport,
                    vec!["claude"],
                ),
                vec![claude.join("skills")],
                vec![claude],
                vec!["claude"],
            ),
            surface(
                IntegrationSurface::ClaudeDesktop,
                McpClient::Claude,
                vec![json(
                    "claude desktop",
                    desktop.join("claude_desktop_config.json"),
                    vec![desktop],
                    vec![],
                )],
                None,
                vec![],
                vec![],
                vec![],
            ),
            surface(
                IntegrationSurface::Codex,
                McpClient::Codex,
                vec![mcp(
                    "codex",
                    codex.join("config.toml"),
                    ConfigFormat::CodexToml,
                    vec![codex.clone()],
                    vec!["codex"],
                    vec![],
                )],
                instruction(
                    "codex",
                    &codex,
                    "AGENTS.md",
                    InstructionFormat::InlineBlock,
                    vec!["codex"],
                ),
                vec![agents.join("skills")],
                vec![codex, agents],
                vec!["codex"],
            ),
            surface(
                IntegrationSurface::Gemini,
                McpClient::Gemini,
                vec![json(
                    "gemini",
                    gemini.join("settings.json"),
                    vec![gemini.clone()],
                    vec!["gemini"],
                )],
                instruction(
                    "gemini/antigravity",
                    &gemini,
                    "GEMINI.md",
                    InstructionFormat::InlineBlock,
                    vec!["gemini", "agy"],
                ),
                vec![home.join(SHARED_SKILLS_PATH)],
                vec![gemini],
                vec!["gemini"],
            ),
            surface(
                IntegrationSurface::Antigravity,
                McpClient::Antigravity,
                vec![
                    json(
                        "antigravity app/cli",
                        agy_config.join("mcp_config.json"),
                        vec![agy_config.clone(), agy_cli.clone(), agy_legacy.clone()],
                        vec!["agy"],
                    ),
                    json(
                        "antigravity cli",
                        agy_cli.join("settings.json"),
                        vec![agy_cli.clone()],
                        vec!["agy"],
                    ),
                    json(
                        "antigravity legacy",
                        agy_legacy.join("mcp_config.json"),
                        vec![agy_legacy],
                        vec![],
                    ),
                ],
                instruction(
                    "gemini/antigravity",
                    &antigravity,
                    "GEMINI.md",
                    InstructionFormat::InlineBlock,
                    vec!["gemini", "agy"],
                ),
                vec![agy_config.join("skills"), agy_cli.join("skills")],
                vec![agy_config, agy_cli],
                vec!["agy"],
            ),
            surface(
                IntegrationSurface::Pi,
                McpClient::Pi,
                vec![mcp(
                    "pi",
                    home.join(PI_AGENT_DIR).join(PI_MCP_FILE),
                    ConfigFormat::Pi,
                    vec![home.join(PI_AGENT_DIR)],
                    vec!["pi"],
                    Vec::new(),
                )],
                instruction(
                    "pi",
                    &pi,
                    "AGENTS.md",
                    InstructionFormat::InlineBlock,
                    vec!["pi"],
                ),
                vec![pi.join("skills")],
                vec![pi],
                vec!["pi"],
            ),
            surface(
                IntegrationSurface::PrimeAgent,
                McpClient::PrimeAgent,
                vec![mcp(
                    "prime-agent",
                    home.join(PRIME_AGENT_DIR).join(PRIME_MCP_FILE),
                    ConfigFormat::PrimeAgent,
                    vec![home.join(PRIME_AGENT_DIR)],
                    vec!["prime-agent"],
                    Vec::new(),
                )],
                instruction(
                    "prime-agent",
                    &prime,
                    "AGENTS.md",
                    InstructionFormat::InlineBlock,
                    vec!["prime-agent"],
                ),
                vec![prime.join("skills")],
                vec![prime],
                vec!["prime-agent"],
            ),
            surface(
                IntegrationSurface::Cursor,
                McpClient::Cursor,
                vec![mcp(
                    "cursor",
                    home.join(CURSOR_MCP_PATH),
                    ConfigFormat::Cursor,
                    vec![home.join(".cursor")],
                    vec![],
                    Vec::new(),
                )],
                None,
                vec![],
                vec![],
                vec![],
            ),
            surface(
                IntegrationSurface::Windsurf,
                McpClient::Windsurf,
                vec![json(
                    "windsurf",
                    home.join(".codeium/windsurf/mcp_config.json"),
                    vec![home.join(".codeium/windsurf")],
                    vec![],
                )],
                None,
                vec![],
                vec![],
                vec![],
            ),
            surface(
                IntegrationSurface::Vscode,
                McpClient::Vscode,
                vec![mcp(
                    "vscode",
                    vscode.join("mcp.json"),
                    ConfigFormat::VscodeServers,
                    vec![vscode.clone()],
                    vec!["code"],
                    vec![],
                )],
                None,
                vec![],
                vec![],
                vec![],
            ),
            surface(
                IntegrationSurface::Zed,
                McpClient::Zed,
                vec![mcp(
                    "zed",
                    zed.join("settings.json"),
                    ConfigFormat::ZedContextServers,
                    std::iter::once(zed)
                        .chain(
                            zed_superseded
                                .iter()
                                .filter_map(|path| path.parent().map(Path::to_path_buf)),
                        )
                        .collect(),
                    vec!["zed"],
                    zed_superseded,
                )],
                None,
                vec![],
                vec![],
                vec![],
            ),
            surface(
                IntegrationSurface::Opencode,
                McpClient::Opencode,
                vec![mcp(
                    "opencode",
                    opencode.join("opencode.json"),
                    ConfigFormat::OpenCode,
                    vec![opencode.clone()],
                    vec!["opencode"],
                    vec![],
                )],
                instruction(
                    "opencode",
                    &opencode,
                    "AGENTS.md",
                    InstructionFormat::InlineBlock,
                    vec!["opencode"],
                ),
                vec![],
                vec![],
                vec![],
            ),
            surface(
                IntegrationSurface::Openclaw,
                McpClient::Openclaw,
                vec![mcp(
                    "openclaw",
                    claw_config.clone(),
                    ConfigFormat::OpenClaw,
                    claw_config
                        .parent()
                        .map(Path::to_path_buf)
                        .into_iter()
                        .collect(),
                    vec![],
                    vec![],
                )],
                None,
                vec![],
                vec![],
                vec![],
            ),
            surface(
                IntegrationSurface::Kilocode,
                McpClient::Kilocode,
                vec![mcp(
                    "kilocode",
                    kilo.join("kilo.jsonc"),
                    ConfigFormat::Kilo,
                    vec![kilo],
                    vec!["kilo"],
                    vec![],
                )],
                None,
                vec![],
                vec![],
                vec![],
            ),
            surface(
                IntegrationSurface::KilocodeExtension,
                McpClient::Kilocode,
                vec![json(
                    "kilocode legacy vscode extension",
                    vscode.join("globalStorage/kilocode.kilo-code/settings/mcp_settings.json"),
                    vec![vscode.join("globalStorage/kilocode.kilo-code/settings")],
                    vec![],
                )],
                None,
                vec![],
                vec![],
                vec![],
            ),
        ];
        debug_assert_eq!(
            surfaces.len(),
            surfaces
                .iter()
                .map(|surface| surface.name)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        );
        Ok(Self {
            canonical_skill_root: home.join(".ai-session-search/skills"),
            home,
            surfaces,
            environment: env.clone(),
        })
    }
}

#[derive(Debug, Clone)]
struct Target {
    label: &'static str,
    path: PathBuf,
    format: ConfigFormat,
    detect_paths: Vec<PathBuf>,
    detect_binaries: Vec<&'static str>,
    /// Files an earlier aise wrote this client's entry to that the client never reads. Install
    /// and uninstall remove aise's entry from them, and status reports one left there as stale.
    superseded_paths: Vec<PathBuf>,
    /// Root of aise's own Codex plugin marketplace, set on the Codex user-config target once the
    /// aise app directory is known. Codex desktop's Plugins list shows only marketplace plugins.
    codex_marketplace: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct InstructionTarget {
    label: &'static str,
    path: PathBuf,
    format: InstructionFormat,
    detect_paths: Vec<PathBuf>,
    detect_binaries: Vec<&'static str>,
}

#[derive(Debug, Clone)]
struct SkillTarget {
    label: &'static str,
    /// The skill DIRECTORY, not one file inside it. A skill is a standard-shaped directory, and
    /// naming the file would make "is this whole tree ours?" unanswerable.
    root: PathBuf,
    package: &'static ManagedSkillPackage,
    /// Harness discovery entries which point at this app-owned canonical package.
    ///
    /// These are deliberately separate from `root`: `root` lives in the platform config
    /// namespace and is owned by aise, while these paths live in harness-owned namespaces.
    discovery_links: Vec<PathBuf>,
    /// Superseded harness entries, removable only through the normal ownership checks.
    retired_discovery_links: Vec<PathBuf>,
    discovery_clients: Vec<String>,
    detect_paths: Vec<PathBuf>,
    detect_binaries: Vec<&'static str>,
}

impl SkillTarget {
    /// The file whose managed marker decides ownership of this whole directory.
    fn anchor(&self) -> PathBuf {
        self.root.join(SKILL_ANCHOR_FILE)
    }

    /// Every managed path under this root, in package declaration order.
    #[cfg(test)]
    fn managed_paths(&self) -> impl Iterator<Item = (PathBuf, &'static str)> + '_ {
        self.package
            .files
            .iter()
            .map(|file| (self.root.join(file.relative_path), file.content))
    }
}

#[derive(Debug)]
struct UninstallPlan {
    mutations: Vec<PlannedFileMutation>,
    /// Per skill target: reasons its whole directory was preserved. Empty means it was removed.
    preserved_skills: Vec<Vec<String>>,
    /// Directories to try pruning after a successful transaction, deepest first.
    prune_skill_directories: Vec<PathBuf>,
    changed_targets: Vec<bool>,
    changed_instructions: Vec<bool>,
    changed_skills: Vec<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PlannedFileMutation {
    Write {
        path: PathBuf,
        original: Option<String>,
        content: String,
    },
    Remove {
        path: PathBuf,
        original: String,
    },
}

impl PlannedFileMutation {
    fn path(&self) -> &Path {
        match self {
            Self::Write { path, .. } | Self::Remove { path, .. } => path,
        }
    }

    fn as_change(&self) -> TextFileChange {
        match self {
            Self::Write {
                path,
                original,
                content,
            } => TextFileChange::write(
                path.clone(),
                original.clone().map(TextFileImage::new),
                content.clone(),
            ),
            Self::Remove { path, original } => {
                TextFileChange::remove(path.clone(), TextFileImage::new(original.clone()))
            }
        }
    }

    fn is_noop(&self) -> bool {
        matches!(
            self,
            Self::Write {
                original: Some(original),
                content,
                ..
            } if original == content
        )
    }
}

fn read_optional_utf8_regular_file(path: &Path) -> Result<Option<String>> {
    Ok(snapshot_utf8_regular_file(path)?.map(|image| image.text().to_string()))
}

fn planned_write(path: &Path, original: &Option<String>, content: String) -> PlannedFileMutation {
    PlannedFileMutation::Write {
        path: path.to_path_buf(),
        original: original.clone(),
        content,
    }
}

fn normalize_planned_mutations(
    mutations: impl IntoIterator<Item = PlannedFileMutation>,
) -> Result<Vec<PlannedFileMutation>> {
    let mut positions = std::collections::HashMap::<PathBuf, usize>::new();
    let mut normalized = Vec::new();
    for mutation in mutations {
        if mutation.is_noop() {
            continue;
        }
        let path = mutation.path().to_path_buf();
        if let Some(position) = positions.get(&path).copied() {
            if normalized[position] != mutation {
                return Err(anyhow!(
                    "multiple integration transformations produce conflicting changes for {}; pass each destination once",
                    path.display()
                ));
            }
        } else {
            positions.insert(path, normalized.len());
            normalized.push(mutation);
        }
    }
    Ok(normalized)
}

#[cfg(test)]
fn publish_planned_mutations(mutations: &[PlannedFileMutation]) -> Result<()> {
    for mutation in mutations {
        publish_text_change(&mutation.as_change())?;
    }
    Ok(())
}

#[cfg(test)]
fn execute_planned_transaction(receipt: &Path, mutations: &[PlannedFileMutation]) -> Result<()> {
    let changes = mutations
        .iter()
        .map(PlannedFileMutation::as_change)
        .collect::<Vec<_>>();
    execute_text_file_transaction(receipt, &changes)
}

fn execute_planned_transaction_with_links<F>(
    receipt: &Path,
    mutations: &[PlannedFileMutation],
    links: &[SymlinkChange],
    publish_link: F,
) -> Result<()>
where
    F: FnMut(usize, &SymlinkChange) -> Result<()>,
{
    let changes = mutations
        .iter()
        .map(PlannedFileMutation::as_change)
        .collect::<Vec<_>>();
    execute_text_file_and_symlink_transaction_with(receipt, &changes, links, publish_link)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstructionFormat {
    ClaudeImport,
    /// One managed inline block whose text leads with the MCP tools, for a harness that also
    /// receives an MCP registration from `aise integrations install`.
    InlineBlock,
    /// Historical CLI-only block, retained for upgrade ownership checks.
    InlineCliBlock,
}

impl InstructionFormat {
    /// The sentence this target's managed block carries.
    fn line(self) -> &'static str {
        match self {
            Self::ClaudeImport | Self::InlineBlock => INSTRUCTIONS_LINE,
            Self::InlineCliBlock => CLI_INSTRUCTIONS_LINE,
        }
    }
}

#[derive(Clone, Copy)]
struct McpTargetSelection<'a> {
    clients: &'a [McpClient],
    excluded_clients: &'a [McpClient],
    no_instructions: bool,
    json_mcp_configs: &'a [PathBuf],
    vscode_configs: &'a [PathBuf],
    zed_configs: &'a [PathBuf],
    opencode_configs: &'a [PathBuf],
    codex_configs: &'a [PathBuf],
    claude_md_paths: &'a [PathBuf],
    gemini_md_paths: &'a [PathBuf],
    agents_md_paths: &'a [PathBuf],
    no_skill: bool,
    skill_roots: &'a [PathBuf],
}

#[cfg(test)]
fn assemble_selected_targets(
    selection: McpTargetSelection<'_>,
) -> Result<(Vec<Target>, Vec<InstructionTarget>, Vec<SkillTarget>)> {
    assemble_selected_targets_for_layout(selection, &ClientLayout::discover()?)
}

fn assemble_selected_targets_for_layout(
    selection: McpTargetSelection<'_>,
    layout: &ClientLayout,
) -> Result<(Vec<Target>, Vec<InstructionTarget>, Vec<SkillTarget>)> {
    let (clients, detected_only) =
        resolve_client_selection(selection.clients, selection.excluded_clients)?;
    let mut targets = clients
        .iter()
        .copied()
        .flat_map(|client| targets_for_layout(client, layout))
        .filter(|target| !detected_only || target_detected(target))
        .chain(custom_targets(
            selection.json_mcp_configs,
            selection.vscode_configs,
            selection.zed_configs,
            selection.opencode_configs,
            selection.codex_configs,
        )?)
        .collect::<Vec<_>>();
    for target in &mut targets {
        target.path = layout.environment.absolute(&target.path);
    }
    dedupe_config_targets(&mut targets)?;
    let mut instruction_targets = if selection.no_instructions {
        Vec::new()
    } else {
        clients
            .iter()
            .copied()
            .flat_map(|client| instruction_targets_for_layout(client, layout))
            .filter(|target| !detected_only || instruction_detected(target))
            .chain(custom_instruction_targets(
                selection.claude_md_paths,
                selection.gemini_md_paths,
                selection.agents_md_paths,
            )?)
            .collect::<Vec<_>>()
    };
    for target in &mut instruction_targets {
        target.path = layout.environment.absolute(&target.path);
    }
    dedupe_instruction_targets(&mut instruction_targets)?;
    let mut skill_targets = if selection.no_skill {
        Vec::new()
    } else {
        clients
            .iter()
            .copied()
            .flat_map(|client| skill_targets_for_layout(client, layout))
            .filter(|target| !detected_only || skill_target_detected(target))
            .chain(custom_skill_targets(selection.skill_roots)?)
            .collect::<Vec<_>>()
    };
    for target in &mut skill_targets {
        target.root = layout.environment.absolute(&target.root);
        for link in &mut target.discovery_links {
            *link = layout.environment.absolute(link);
        }
    }
    dedupe_skill_targets(&mut skill_targets)?;
    Ok((targets, instruction_targets, skill_targets))
}

const CONCRETE_CLIENTS: [McpClient; 13] = [
    McpClient::Claude,
    McpClient::Codex,
    McpClient::Gemini,
    McpClient::Antigravity,
    McpClient::Pi,
    McpClient::PrimeAgent,
    McpClient::Cursor,
    McpClient::Windsurf,
    McpClient::Vscode,
    McpClient::Zed,
    McpClient::Opencode,
    McpClient::Openclaw,
    McpClient::Kilocode,
];

fn resolve_client_selection(
    included: &[McpClient],
    excluded: &[McpClient],
) -> Result<(Vec<McpClient>, bool)> {
    if excluded.contains(&McpClient::All) {
        bail!("--exclude-client all is invalid; select no clients by using only explicit custom paths");
    }
    let include_all = included.contains(&McpClient::All);
    if include_all && included.len() != 1 {
        bail!("--client all cannot be combined with another --client value");
    }
    let candidates = if include_all || included.is_empty() {
        CONCRETE_CLIENTS.to_vec()
    } else {
        included.to_vec()
    };
    let mut selected = Vec::new();
    for client in candidates {
        if client != McpClient::All && !excluded.contains(&client) && !selected.contains(&client) {
            selected.push(client);
        }
    }
    Ok((selected, include_all || included.is_empty()))
}

fn dedupe_config_targets(targets: &mut Vec<Target>) -> Result<()> {
    let mut seen = std::collections::HashMap::<PathBuf, ConfigFormat>::new();
    let mut conflict = None;
    targets.retain(|target| match seen.get(&target.path) {
        None => {
            seen.insert(target.path.clone(), target.format);
            true
        }
        Some(format) if *format == target.format => false,
        Some(first_format) => {
            conflict = Some((target.path.clone(), *first_format, target.format));
            false
        }
    });
    if let Some((path, first_format, second_format)) = conflict {
        bail!(
            "MCP destination {} was selected with incompatible config formats \
             ({first_format:?} and {second_format:?}); pass it through exactly one \
             format-specific option",
            path.display()
        );
    }
    Ok(())
}

fn dedupe_instruction_targets(targets: &mut Vec<InstructionTarget>) -> Result<()> {
    let mut seen = std::collections::HashMap::<PathBuf, InstructionFormat>::new();
    let mut conflict = None;
    targets.retain(|target| match seen.get(&target.path) {
        None => {
            seen.insert(target.path.clone(), target.format);
            true
        }
        Some(format) if *format == target.format => false,
        Some(_) => {
            conflict = Some(target.path.clone());
            false
        }
    });
    if let Some(path) = conflict {
        bail!(
            "instruction destination {} was selected with incompatible Markdown ownership formats; pass it through exactly one format-specific option",
            path.display()
        );
    }
    Ok(())
}

/// Drop true duplicates and reject incompatible ones, matching [`dedupe_config_targets`] and
/// [`dedupe_instruction_targets`]. Those key `(path, format)`; skills key `(path, label)`, because
/// [`SkillTarget`] carries a label rather than a format. Reaching the same root twice under one
/// label is a genuine duplicate and is dropped silently; the same root claimed by two different
/// labels means one destination was selected two ways, which the caller must resolve.
fn dedupe_skill_targets(targets: &mut Vec<SkillTarget>) -> Result<()> {
    let mut seen = std::collections::HashMap::<PathBuf, usize>::new();
    let mut conflict = None;
    let mut deduped: Vec<SkillTarget> = Vec::new();
    for target in targets.drain(..) {
        match seen.get(&target.root).copied() {
            None => {
                seen.insert(target.root.clone(), deduped.len());
                deduped.push(target);
            }
            Some(index)
                if deduped[index].label == target.label
                    && deduped[index].package.name == target.package.name =>
            {
                for link in target.discovery_links {
                    if !deduped[index].discovery_links.contains(&link) {
                        deduped[index].discovery_links.push(link);
                    }
                }
                for link in target.retired_discovery_links {
                    if !deduped[index].retired_discovery_links.contains(&link) {
                        deduped[index].retired_discovery_links.push(link);
                    }
                }
                for client in target.discovery_clients {
                    if !deduped[index].discovery_clients.contains(&client) {
                        deduped[index].discovery_clients.push(client);
                    }
                }
                for path in target.detect_paths {
                    if !deduped[index].detect_paths.contains(&path) {
                        deduped[index].detect_paths.push(path);
                    }
                }
                for binary in target.detect_binaries {
                    if !deduped[index].detect_binaries.contains(&binary) {
                        deduped[index].detect_binaries.push(binary);
                    }
                }
            }
            Some(index) => {
                conflict = Some((target.root.clone(), deduped[index].label, target.label));
            }
        }
    }
    *targets = deduped;
    if let Some((path, first_label, second_label)) = conflict {
        bail!(
            "skill destination {} was selected as both {first_label} and {second_label}; \
             pass it through exactly one of those options",
            path.display()
        );
    }
    Ok(())
}

pub(crate) fn install_with_receipt(
    args: IntegrationInstallArgs,
    default_receipt: &Path,
    config: &IntegrationsConfig,
) -> Result<IntegrationInstallOutcome> {
    install_for_layout(
        args,
        default_receipt,
        &ClientLayout::discover()?.with_config(config)?,
    )
}

fn install_for_layout(
    args: IntegrationInstallArgs,
    default_receipt: &Path,
    layout: &ClientLayout,
) -> Result<IntegrationInstallOutcome> {
    let (mut targets, instruction_targets, mut skill_targets) =
        args.targets
            .resolve(args.no_instructions, args.no_skill, layout)?;
    rebase_automatic_skill_roots(&mut skill_targets, default_receipt);
    attach_codex_marketplace(&mut targets, default_receipt);
    if args.no_mcp {
        targets.clear();
    }
    let has_mcp_targets = !targets.is_empty();
    let aliases = if args.no_aliases {
        None
    } else {
        Some(crate::executable_alias::ExecutableAliases::discover()?)
    };
    if let Some(aliases) = &aliases {
        aliases.preflight_install()?;
    }
    if targets.is_empty()
        && instruction_targets.is_empty()
        && skill_targets.is_empty()
        && aliases.is_none()
    {
        println!(
            "No supported MCP client config was detected. Use --client or a custom config path to create one."
        );
        return Ok(IntegrationInstallOutcome::NoTargets);
    }
    let binary = if has_mcp_targets {
        resolve_mcp_binary(args.binary.as_deref())?
    } else {
        PathBuf::new()
    };
    // The manifest lives beside the resolved config, which `default_receipt` already sits next
    // to, so both durable records land in one place rather than two.
    let manifest = crate::skill_manifest::manifest_path(default_receipt);
    let retired_skill_roots = plan_retire_sibling_package(
        &crate::skill_manifest::load_manifest(&manifest)?,
        &skill_targets,
    )?
    .retired_roots;
    let mutations = preflight_install(
        &targets,
        &instruction_targets,
        &skill_targets,
        &binary,
        Some(&manifest),
    )?;
    let skill_manifest = crate::skill_manifest::load_manifest(&manifest)?;
    let skill_file_changes = skill_targets
        .iter()
        .map(|target| {
            let migration = plan_migrate_discovery_copies(target, Some(&manifest))?;
            let package_changes = plan_refresh_skill_files(target, &skill_manifest, false)?
                .iter()
                .any(|mutation| !mutation.is_noop());
            Ok(package_changes || !migration.mutations.is_empty())
        })
        .collect::<Result<Vec<_>>>()?;
    let retired_discovery_links = plan_retire_sibling_discovery_links(&skill_targets)?;
    let mut planned_discovery_links = retired_discovery_links
        .iter()
        .map(|(link, expected)| SymlinkChange::remove(link.clone(), expected.clone()))
        .collect::<Vec<_>>();
    planned_discovery_links.extend(plan_install_skill_discovery_links(&skill_targets)?);
    let (retired_entries, retired_directories) = retired_discovery_outcomes(
        &skill_targets,
        &skill_manifest,
        &planned_discovery_links,
        args.dry_run,
    )?;
    let changed_discovery_paths = planned_discovery_links
        .iter()
        .map(|change| change.path.clone())
        .collect::<std::collections::HashSet<_>>();
    let discovery_link_changes = skill_targets
        .iter()
        .map(|target| {
            target
                .discovery_links
                .iter()
                .map(|link| changed_discovery_paths.contains(link))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let alias_guard = if args.dry_run {
        None
    } else {
        aliases.as_ref().map(|value| value.install()).transpose()?
    };
    if !args.dry_run {
        let receipt = selected_transaction_receipt(&args.transaction, default_receipt)?;
        execute_planned_transaction_with_links(
            &receipt,
            &mutations,
            &planned_discovery_links,
            |_, change| publish_install_skill_discovery_link(change, &skill_targets),
        )?;
        prune_retired_skill_directories(&retired_skill_roots);
        prune_emptied_skill_directories(&retired_directories);
    }
    if let Some(guard) = alias_guard {
        guard.commit();
    }
    if let Some(aliases) = &aliases {
        for line in aliases.install_lines(args.dry_run)? {
            println!("{line}");
        }
    }
    for target in targets {
        if args.dry_run {
            println!(
                "dry-run: would upsert {} MCP server in {}",
                target.label,
                target.path.display()
            );
        } else {
            println!(
                "configured {} MCP server in {}",
                target.label,
                target.path.display()
            );
        }
    }
    for target in instruction_targets {
        if args.dry_run {
            println!(
                "dry-run: would upsert {} instruction guidance in {}",
                target.label,
                target.path.display()
            );
        } else {
            println!(
                "configured {} instruction guidance in {}",
                target.label,
                target.path.display()
            );
        }
    }
    for (link, expected) in &retired_discovery_links {
        if args.dry_run {
            println!(
                "dry-run: would remove retired skill discovery link {} -> {}",
                link.display(),
                expected.display()
            );
        } else {
            println!(
                "removed retired skill discovery link {} -> {}",
                link.display(),
                expected.display()
            );
        }
    }
    for outcome in retired_entries {
        println!(
            "{}{} {}{}",
            if args.dry_run { "dry-run: " } else { "" },
            outcome.action,
            outcome.root,
            outcome
                .problem
                .map(|problem| format!(": {problem}"))
                .unwrap_or_default()
        );
    }
    for ((target, file_changed), link_changes) in skill_targets
        .into_iter()
        .zip(skill_file_changes)
        .zip(discovery_link_changes)
    {
        if args.dry_run && file_changed {
            println!(
                "dry-run: would install {} skill at {}",
                target.label,
                target.root.display()
            );
        } else if file_changed {
            println!(
                "installed {} skill at {}",
                target.label,
                target.root.display()
            );
        }
        for (link, changed) in target.discovery_links.iter().zip(link_changes) {
            if !changed {
                continue;
            }
            if args.dry_run {
                println!(
                    "dry-run: would link skill discovery entry {} -> {}",
                    link.display(),
                    target.root.display()
                );
            } else {
                println!(
                    "linked skill discovery entry {} -> {}",
                    link.display(),
                    target.root.display()
                );
            }
        }
    }
    if args.dry_run {
        println!("dry-run: no files were modified");
    } else if has_mcp_targets {
        println!("Restart your MCP client to load AI Session Search (`aise`).");
    }
    Ok(if args.dry_run {
        IntegrationInstallOutcome::Previewed
    } else {
        IntegrationInstallOutcome::Configured
    })
}

fn plan_install_skill_discovery_links(targets: &[SkillTarget]) -> Result<Vec<SymlinkChange>> {
    let mut changes = Vec::new();
    for target in targets {
        for link in &target.retired_discovery_links {
            if fs::symlink_metadata(link).is_ok_and(|metadata| metadata.file_type().is_symlink())
                && fs::read_link(link)? == target.root
            {
                changes.push(SymlinkChange::remove(link.clone(), target.root.clone()));
            }
        }
        for link in &target.discovery_links {
            match snapshot_symlink_image(link)
                .with_context(|| format!("inspect skill discovery entry {}", link.display()))?
            {
                SymlinkImage::Absent => {
                    changes.push(SymlinkChange::install(link.clone(), target.root.clone()));
                }
                SymlinkImage::Directory => {
                    changes.push(SymlinkChange::replace_directory(
                        link.clone(),
                        target.root.clone(),
                    ));
                }
                SymlinkImage::Link { target: existing } => {
                    let existing = existing.to_path_buf()?;
                    if existing == target.root {
                        continue;
                    }
                    bail!(
                        "skill discovery entry {} already points to {}; preserved it instead of \
                         replacing it with the canonical package at {}",
                        link.display(),
                        existing.display(),
                        target.root.display()
                    );
                }
            }
        }
    }
    Ok(changes)
}

fn publish_install_skill_discovery_link(
    change: &SymlinkChange,
    targets: &[SkillTarget],
) -> Result<()> {
    if matches!(&change.before, SymlinkImage::Directory) {
        let target = targets
            .iter()
            .find(|target| target.discovery_links.contains(&change.path))
            .ok_or_else(|| {
                anyhow!(
                    "no managed skill target owns discovery link {}",
                    change.path.display()
                )
            })?;
        if snapshot_symlink_image(&change.path)? != SymlinkImage::Directory {
            bail!(
                "{} changed after discovery-link preflight; refusing to overwrite concurrent edits",
                change.path.display()
            );
        }
        let mut legacy = target.clone();
        legacy.root = change.path.clone();
        legacy.discovery_links.clear();
        for directory in managed_skill_directories(&legacy) {
            match fs::remove_dir(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                    bail!(
                        "legacy skill directory {} changed during install; the canonical package remains installed at {}, and no discovery link replaced the changed directory",
                        change.path.display(),
                        target.root.display()
                    )
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "remove emptied legacy skill directory {}",
                            directory.display()
                        )
                    })
                }
            }
        }
        apply_symlink_change_after(change)
    } else {
        publish_symlink_change(change)
    }
}

#[cfg(all(test, unix))]
fn create_directory_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

// Unused today: every caller is a `#[cfg(unix)]` test, so no Windows test exercises a symlinked
// skill root. Kept because the gap is in the tests, not here, and `expect` rather than `allow` so
// that un-gating one of those tests fails the build until this attribute is removed.
#[cfg(all(test, windows))]
#[expect(dead_code)]
fn create_directory_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

pub(crate) fn status_with_receipt(
    args: IntegrationStatusArgs,
    default_receipt: &Path,
    config: &IntegrationsConfig,
) -> Result<()> {
    status_for_layout(
        args,
        default_receipt,
        &ClientLayout::discover()?.with_config(config)?,
    )
}

fn status_for_layout(
    args: IntegrationStatusArgs,
    default_receipt: &Path,
    layout: &ClientLayout,
) -> Result<()> {
    let receipt = selected_transaction_receipt(&args.transaction, default_receipt)?;
    let (mut targets, instruction_targets, mut skill_targets) =
        args.targets
            .resolve(args.no_instructions, args.no_skill, layout)?;
    rebase_automatic_skill_roots(&mut skill_targets, default_receipt);
    attach_codex_marketplace(&mut targets, default_receipt);
    if args.no_mcp {
        targets.clear();
    }
    let aliases = if args.no_aliases {
        None
    } else {
        Some(crate::executable_alias::ExecutableAliases::discover()?)
    };
    let mut entries = with_text_file_transaction_read_lock(&receipt, || {
        ensure_no_pending_transaction(&receipt)?;
        let mut entries =
            Vec::with_capacity(targets.len() + instruction_targets.len() + skill_targets.len());
        for target in &targets {
            entries.push(IntegrationStatusEntry::new(
                StatusComponent::Mcp,
                Some(target.label),
                &target.path,
                status_target(target)?,
            ));
        }
        for target in &instruction_targets {
            entries.push(IntegrationStatusEntry::new(
                StatusComponent::Instructions,
                Some(target.label),
                &target.path,
                status_instruction_file(target)?,
            ));
        }
        // Read once for the whole report: reloading per target would let two lines disagree if
        // something rewrote the manifest mid-run.
        let skill_manifest = crate::skill_manifest::load_manifest(
            &crate::skill_manifest::manifest_path(default_receipt),
        )?;
        for target in &skill_targets {
            entries.push(IntegrationStatusEntry::new(
                StatusComponent::Skill,
                Some(target.label),
                &target.root,
                status_skill_file(target, &skill_manifest)?,
            ));
            for link in &target.discovery_links {
                let (state, current) = status_skill_discovery_link(link, &target.root)?;
                entries.push(IntegrationStatusEntry {
                    current,
                    ..IntegrationStatusEntry::new(
                        StatusComponent::SkillDiscoveryLink,
                        Some(target.label),
                        link,
                        state,
                    )
                });
            }
        }
        Ok(entries)
    })?;
    if let Some(aliases) = aliases {
        for (alias, state) in aliases.statuses()? {
            entries.push(IntegrationStatusEntry::new(
                StatusComponent::ExecutableAlias,
                None,
                alias,
                state,
            ));
        }
    }
    match args.format {
        crate::cli::ReportOutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "integrations": entries }))?
            );
        }
        crate::cli::ReportOutputFormat::Table if entries.is_empty() => {
            println!("No supported MCP client config was detected.");
        }
        crate::cli::ReportOutputFormat::Table => {
            for entry in &entries {
                println!("{}", entry.line());
            }
        }
    }
    Ok(())
}

/// One line of `aise integrations status`, and one entry of its JSON form.
#[derive(Debug, serde::Serialize)]
struct IntegrationStatusEntry {
    component: StatusComponent,
    /// The harness the entry belongs to; executable aliases belong to none.
    client: Option<&'static str>,
    path: String,
    state: String,
    /// Whether the entry is exactly what this build installs.
    current: bool,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum StatusComponent {
    Mcp,
    Instructions,
    Skill,
    SkillDiscoveryLink,
    ExecutableAlias,
}

impl IntegrationStatusEntry {
    fn new(
        component: StatusComponent,
        client: Option<&'static str>,
        path: &Path,
        state: impl Into<String>,
    ) -> Self {
        let state = state.into();
        Self {
            component,
            client,
            path: path.display().to_string(),
            // Every status function reports an integration that matches this build as
            // "configured"; a discovery link says where it points and sets this itself.
            current: state == ManagedFileState::Current.label(),
            state,
        }
    }

    fn line(&self) -> String {
        let client = self.client.unwrap_or_default();
        match self.component {
            StatusComponent::ExecutableAlias => {
                format!("executable alias {}: {}", self.path, self.state)
            }
            StatusComponent::SkillDiscoveryLink => {
                format!("{client} discovery {}: {}", self.path, self.state)
            }
            StatusComponent::Mcp | StatusComponent::Instructions | StatusComponent::Skill => {
                format!("{client} {}: {}", self.path, self.state)
            }
        }
    }
}

/// The state word for a skill discovery link, and whether it points at the canonical root.
fn status_skill_discovery_link(link: &Path, canonical_root: &Path) -> Result<(String, bool)> {
    match fs::symlink_metadata(link) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(("missing".to_string(), false))
        }
        Err(error) => {
            Err(error).with_context(|| format!("inspect skill discovery entry {}", link.display()))
        }
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let target = fs::read_link(link)
                .with_context(|| format!("read skill discovery link {}", link.display()))?;
            if target == canonical_root {
                Ok((format!("linked -> {}", canonical_root.display()), true))
            } else {
                Ok((format!("modified link -> {}", target.display()), false))
            }
        }
        Ok(_) => Ok((
            "legacy copied directory or unmanaged entry".to_string(),
            false,
        )),
    }
}

fn ensure_no_pending_transaction(receipt: &Path) -> Result<()> {
    if transaction_recovery_required(receipt)? {
        bail!(
            "integration status is not authoritative while recovery receipt {} exists; {} first",
            receipt.display(),
            recovery_guidance(receipt)
        );
    }
    Ok(())
}

pub(crate) fn uninstall_with_receipt(
    args: IntegrationUninstallArgs,
    default_receipt: &Path,
    config: &IntegrationsConfig,
) -> Result<()> {
    uninstall_for_layout(
        args,
        default_receipt,
        &ClientLayout::discover()?.with_config(config)?,
    )
}

fn uninstall_for_layout(
    args: IntegrationUninstallArgs,
    default_receipt: &Path,
    layout: &ClientLayout,
) -> Result<()> {
    let (mut targets, instruction_targets, mut skill_targets) =
        args.targets
            .resolve(args.keep_instructions, args.keep_skill, layout)?;
    rebase_automatic_skill_roots(&mut skill_targets, default_receipt);
    attach_codex_marketplace(&mut targets, default_receipt);
    if args.keep_mcp {
        targets.clear();
    }
    if args.keep_skill {
        skill_targets.clear();
    }
    let aliases = if args.keep_aliases {
        None
    } else {
        Some(crate::executable_alias::ExecutableAliases::discover()?)
    };
    if targets.is_empty()
        && instruction_targets.is_empty()
        && skill_targets.is_empty()
        && aliases.is_none()
    {
        println!("No supported MCP client config was detected.");
        return Ok(());
    }
    if args.force_full_cleanup {
        validate_force_full_cleanup(&args, &skill_targets)?;
        skill_targets.retain(|target| target.label == CUSTOM_SKILL_TARGET_LABEL);
        for target in &skill_targets {
            println!(
                "FORCE CLEANUP: every file under {} will be deleted, including any you wrote. \
                 This cannot be undone.",
                target.root.display()
            );
        }
    } else {
        append_retired_sibling_skill_targets(&mut skill_targets)?;
    }
    let released_skill_targets = skill_targets.clone();
    let discovery_uninstall = prepare_skill_discovery_uninstall(
        &skill_targets,
        layout,
        &crate::skill_manifest::load_manifest(&crate::skill_manifest::manifest_path(
            default_receipt,
        ))?,
    )?;
    let discovery_link_changes = discovery_uninstall
        .links_to_remove
        .iter()
        .map(|(link, expected)| SymlinkChange::remove(link.clone(), expected.clone()))
        .collect::<Vec<_>>();
    skill_targets = discovery_uninstall.file_targets;
    let UninstallPlan {
        mutations,
        preserved_skills,
        prune_skill_directories,
        changed_targets,
        changed_instructions,
        changed_skills,
    } = preflight_uninstall(
        &targets,
        &instruction_targets,
        &skill_targets,
        &released_skill_targets,
        Some(&crate::skill_manifest::manifest_path(default_receipt)),
        args.force_full_cleanup,
    )?;

    let removed_discovery_links = if !args.dry_run {
        let receipt = selected_transaction_receipt(&args.transaction, default_receipt)?;
        execute_planned_transaction_with_links(
            &receipt,
            &mutations,
            &discovery_link_changes,
            |_, change| publish_symlink_change(change),
        )?;
        // Only after the transaction commits, and only directories aise itself creates.
        prune_emptied_skill_directories(&prune_skill_directories);
        discovery_link_changes
            .iter()
            .map(|change| change.path.clone())
            .collect()
    } else {
        Vec::new()
    };
    if args.dry_run {
        for (link, expected) in &discovery_uninstall.links_to_remove {
            println!(
                "dry-run: would remove owned skill discovery link {} -> {}",
                link.display(),
                expected.display()
            );
        }
    } else {
        for link in removed_discovery_links {
            println!("removed owned skill discovery link {}", link.display());
        }
    }
    for message in &discovery_uninstall.notices {
        println!("{message}");
    }
    for (target, changed) in targets.into_iter().zip(changed_targets) {
        if args.dry_run && changed {
            println!(
                "dry-run: would remove {} MCP server from {}",
                target.label,
                target.path.display()
            );
        } else if changed {
            println!(
                "removed {} MCP server from {}",
                target.label,
                target.path.display()
            );
        }
    }
    for (target, changed) in instruction_targets.into_iter().zip(changed_instructions) {
        if args.dry_run && changed {
            println!(
                "dry-run: would remove {} instruction guidance from {}",
                target.label,
                target.path.display()
            );
        } else if changed {
            println!(
                "removed {} instruction guidance from {}",
                target.label,
                target.path.display()
            );
        }
    }
    for ((target, changed), preserved) in skill_targets
        .into_iter()
        .zip(changed_skills)
        .zip(preserved_skills)
    {
        if !preserved.is_empty() {
            // Preservation is the interesting outcome, so it is reported first and in full: a
            // user who is not told WHY a directory survived cannot decide what to do about it.
            println!(
                "preserved {} skill directory {} ({} reason{}):",
                target.label,
                target.root.display(),
                preserved.len(),
                if preserved.len() == 1 { "" } else { "s" }
            );
            for reason in &preserved {
                println!("  {reason}");
            }
            println!(
                "  nothing was removed. Inspect the directory, then delete it yourself, or \
                 re-run with --force-full-cleanup --skill-root {} to delete everything in it.",
                target.root.display()
            );
            continue;
        }
        if args.dry_run && changed {
            println!(
                "dry-run: would remove {} skill from {}",
                target.label,
                target.root.display()
            );
        } else if changed {
            println!(
                "removed {} skill from {}",
                target.label,
                target.root.display()
            );
        }
    }
    if let Some(aliases) = aliases {
        for line in aliases.uninstall_lines(args.dry_run)? {
            println!("{line}");
        }
    }
    if args.dry_run {
        println!("dry-run: no files were modified");
    }
    Ok(())
}

fn append_retired_sibling_skill_targets(skill_targets: &mut Vec<SkillTarget>) -> Result<()> {
    let retired = skill_targets
        .iter()
        .filter(|target| target.package.name == AI_SESSION_SEARCH_SKILL_PACKAGE.name)
        .filter_map(|target| {
            let root = target.root.parent()?.join(RETIRED_SKILL_PACKAGE_NAME);
            let discovery_links = target
                .discovery_links
                .iter()
                .filter_map(|link| link.parent())
                .map(|parent| parent.join(RETIRED_SKILL_PACKAGE_NAME))
                .collect();
            Some(SkillTarget {
                label: "retired corrections",
                root,
                package: &RETIRED_SKILL_PACKAGE,
                discovery_links,
                retired_discovery_links: target
                    .retired_discovery_links
                    .iter()
                    .filter_map(|link| link.parent())
                    .map(|parent| parent.join(RETIRED_SKILL_PACKAGE_NAME))
                    .collect(),
                discovery_clients: target.discovery_clients.clone(),
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
            })
        })
        .collect::<Vec<_>>();
    skill_targets.extend(retired);
    dedupe_skill_targets(skill_targets)
}

#[derive(Debug, Default)]
struct SkillDiscoveryUninstall {
    file_targets: Vec<SkillTarget>,
    links_to_remove: Vec<(PathBuf, PathBuf)>,
    notices: Vec<String>,
}

fn prepare_skill_discovery_uninstall(
    selected_targets: &[SkillTarget],
    layout: &ClientLayout,
    manifest: &crate::skill_manifest::ManifestState,
) -> Result<SkillDiscoveryUninstall> {
    let mut plan = SkillDiscoveryUninstall::default();
    for target in selected_targets {
        if target.discovery_links.is_empty() {
            plan.file_targets.push(target.clone());
            continue;
        }

        let mut clients = manifest
            .installation(&target.root)
            .map(|entry| entry.discovery_clients.clone())
            .unwrap_or_default();
        // Older manifests lack users. Infer only the two formerly distinct, owned links.
        if clients.is_empty() {
            for (client, root) in [
                ("codex", SHARED_SKILLS_PATH),
                ("gemini", GEMINI_RETIRED_SKILLS_PATH),
            ] {
                if fs::read_link(layout.home.join(root).join(target.package.name))
                    .is_ok_and(|destination| destination == target.root)
                {
                    clients.push(client.to_string());
                }
            }
        }
        clients.retain(|client| !target.discovery_clients.contains(client));
        let retained_links = CONCRETE_CLIENTS
            .into_iter()
            .filter(|client| {
                clients
                    .iter()
                    .any(|name| client.to_possible_value().unwrap().get_name() == name)
            })
            .flat_map(|client| skill_targets_for_layout(client, layout))
            .flat_map(|other| other.discovery_links)
            .collect::<Vec<_>>();

        for link in target
            .discovery_links
            .iter()
            .chain(&target.retired_discovery_links)
        {
            if retained_links.contains(link) {
                plan.notices.push(format!("preserved shared skill discovery link {} because another installed harness uses it", link.display()));
                continue;
            }
            match fs::symlink_metadata(link) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("inspect skill discovery entry {}", link.display())
                    })
                }
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    let destination = fs::read_link(link)
                        .with_context(|| format!("read skill discovery link {}", link.display()))?;
                    if destination == target.root {
                        plan.links_to_remove
                            .push((link.clone(), target.root.clone()));
                    } else {
                        plan.notices.push(format!(
                            "preserved modified skill discovery link {} -> {}",
                            link.display(),
                            destination.display()
                        ));
                    }
                }
                Ok(_) => {
                    // Compatibility with releases which copied the package into the harness.
                    // The normal manifest/marker/extra-file checks still decide whether removal
                    // is allowed; this merely gives that directory to the existing safe planner.
                    let mut legacy = target.clone();
                    legacy.root = link.clone();
                    legacy.discovery_links.clear();
                    plan.file_targets.push(legacy);
                }
            }
        }

        let selected = target
            .discovery_links
            .iter()
            .chain(&target.retired_discovery_links)
            .cloned()
            .collect::<Vec<_>>();
        let another_owned_link =
            another_owned_discovery_link(layout, target.package, &selected, &target.root)?;
        if another_owned_link || !clients.is_empty() {
            plan.notices.push(format!(
                "preserved canonical skill package {} because another harness still links to it",
                target.root.display()
            ));
        } else {
            plan.file_targets.push(target.clone());
        }
    }
    dedupe_skill_targets(&mut plan.file_targets)?;
    Ok(plan)
}

fn another_owned_discovery_link(
    layout: &ClientLayout,
    package: &ManagedSkillPackage,
    selected: &[PathBuf],
    canonical_root: &Path,
) -> Result<bool> {
    for link in all_standard_discovery_links(layout, package)
        .into_iter()
        .filter(|link| !selected.contains(link))
    {
        let metadata = match fs::symlink_metadata(&link) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if missing_discovery_entry_is_benign(&link)? {
                    continue;
                }
                return Err(error)
                    .with_context(|| format!("inspect skill discovery entry {}", link.display()));
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect skill discovery entry {}", link.display()))
            }
        };
        if !metadata.file_type().is_symlink() {
            continue;
        }
        let destination = fs::read_link(&link)
            .with_context(|| format!("read skill discovery link {}", link.display()))?;
        if destination == canonical_root {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Distinguish an absent discovery entry from a descendant hidden behind a non-directory parent.
/// Windows reports both cases as `NotFound`, while Unix reports the latter as `NotADirectory`.
fn missing_discovery_entry_is_benign(path: &Path) -> Result<bool> {
    for ancestor in path.ancestors().skip(1) {
        match fs::metadata(ancestor) {
            Ok(metadata) => return Ok(metadata.is_dir()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "inspect parent of skill discovery entry {}",
                        ancestor.display()
                    )
                })
            }
        }
    }
    Ok(true)
}

fn all_standard_discovery_links(
    layout: &ClientLayout,
    package: &ManagedSkillPackage,
) -> Vec<PathBuf> {
    layout
        .surfaces
        .iter()
        .flat_map(|surface| {
            surface
                .skill_roots
                .iter()
                .chain(&surface.retired_skill_roots)
        })
        .map(|root| root.join(package.name))
        .collect()
}

/// Keep the canonical packages beside the actual selected app config, including when a caller
/// deliberately relocates that config with an explicit override.
///
/// `default_receipt` already lives beside the resolved config file. Custom `--skill-root`
/// destinations are intentionally left exactly where the caller put them.
fn rebase_automatic_skill_roots(targets: &mut [SkillTarget], default_receipt: &Path) {
    let app_root = app_root_of(default_receipt);
    for target in targets
        .iter_mut()
        .filter(|target| target.label != CUSTOM_SKILL_TARGET_LABEL)
    {
        target.root = app_root.join("skills").join(target.package.name);
    }
}

/// The aise app directory: where the resolved config, its receipt and the managed packages live.
fn app_root_of(default_receipt: &Path) -> &Path {
    default_receipt
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// Give the Codex user-config target aise's marketplace beside the skill packages, so a relocated
/// app config moves it too. A `--codex-config` target is an explicit file and gets none.
fn attach_codex_marketplace(targets: &mut [Target], default_receipt: &Path) {
    let root = app_root_of(default_receipt).join("codex-marketplace");
    for target in targets
        .iter_mut()
        .filter(|target| target.format == ConfigFormat::CodexToml && target.label == "codex")
    {
        target.codex_marketplace = Some(root.clone());
    }
}

pub(crate) fn recover_with_receipt(
    args: IntegrationRecoverArgs,
    default_receipt: &Path,
) -> Result<()> {
    let receipt = selected_transaction_receipt(&args.transaction, default_receipt)?;
    match recover_text_file_transaction(&receipt)? {
        RecoveryOutcome::RolledBack { paths } => println!(
            "restored {paths} path(s) from interrupted integration update; removed {}",
            receipt.display()
        ),
        RecoveryOutcome::Finalized { paths } => println!(
            "verified {paths} published integration path(s); removed stale receipt {}",
            receipt.display()
        ),
    }
    Ok(())
}

pub(crate) fn default_transaction_receipt(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(".ai-session-search-mcp-transaction.json")
}

fn selected_transaction_receipt(
    args: &IntegrationTransactionArgs,
    default_receipt: &Path,
) -> Result<PathBuf> {
    absolutize(&expand_tilde(
        args.transaction_receipt
            .as_deref()
            .unwrap_or(default_receipt),
    )?)
}

#[cfg(test)]
fn targets_for(client: McpClient) -> Result<Vec<Target>> {
    let layout = ClientLayout::discover()?;
    Ok(targets_for_layout(client, &layout))
}

fn targets_for_layout(client: McpClient, layout: &ClientLayout) -> Vec<Target> {
    layout
        .surfaces
        .iter()
        .filter(|surface| client == McpClient::All || surface.client == client)
        .flat_map(|surface| surface.mcp.iter().cloned())
        .filter(|target| client != McpClient::All || target_detected(target))
        .collect()
}

#[cfg(test)]
fn instruction_targets_for(client: McpClient) -> Result<Vec<InstructionTarget>> {
    let layout = ClientLayout::discover()?;
    Ok(instruction_targets_for_layout(client, &layout))
}

fn instruction_targets_for_layout(
    client: McpClient,
    layout: &ClientLayout,
) -> Vec<InstructionTarget> {
    layout
        .surfaces
        .iter()
        .filter(|surface| client == McpClient::All || surface.client == client)
        .filter_map(|surface| surface.instruction.clone())
        .filter(|target| client != McpClient::All || instruction_detected(target))
        .collect()
}

fn instruction_detected(target: &InstructionTarget) -> bool {
    target.path.exists()
        || target
            .detect_paths
            .iter()
            .any(|path| path.exists() && path.is_dir())
        || target
            .detect_binaries
            .iter()
            .any(|binary| which(binary).is_some())
}

fn skill_targets_for_layout(client: McpClient, layout: &ClientLayout) -> Vec<SkillTarget> {
    let canonical_root = &layout.canonical_skill_root;
    layout
        .surfaces
        .iter()
        .filter(|surface| {
            (client == McpClient::All || surface.client == client)
                && !surface.skill_roots.is_empty()
        })
        .flat_map(|surface| {
            MANAGED_SKILL_PACKAGES.iter().map(|package| {
                let mut target = skill_target_for_package(
                    "app",
                    canonical_root.join(package.name),
                    package,
                    surface.detect_paths.clone(),
                    surface.binaries.clone(),
                );
                target.discovery_links.extend(
                    surface
                        .skill_roots
                        .iter()
                        .map(|root| root.join(package.name)),
                );
                target.discovery_clients.push(
                    surface
                        .client
                        .to_possible_value()
                        .unwrap()
                        .get_name()
                        .to_string(),
                );
                target.retired_discovery_links.extend(
                    surface
                        .retired_skill_roots
                        .iter()
                        .map(|root| root.join(package.name)),
                );
                target
            })
        })
        .collect()
}

fn skill_target_for_package(
    label: &'static str,
    root: PathBuf,
    package: &'static ManagedSkillPackage,
    detect_paths: Vec<PathBuf>,
    detect_binaries: Vec<&'static str>,
) -> SkillTarget {
    SkillTarget {
        label,
        root,
        package,
        discovery_links: Vec::new(),
        retired_discovery_links: Vec::new(),
        discovery_clients: Vec::new(),
        detect_paths,
        detect_binaries,
    }
}

#[cfg(test)]
fn skill_target(
    label: &'static str,
    root: PathBuf,
    detect_paths: Vec<PathBuf>,
    detect_binaries: Vec<&'static str>,
) -> SkillTarget {
    skill_target_for_package(
        label,
        root,
        &AI_SESSION_SEARCH_SKILL_PACKAGE,
        detect_paths,
        detect_binaries,
    )
}

fn skill_target_detected(target: &SkillTarget) -> bool {
    target.root.exists()
        || target.discovery_links.iter().any(|path| path.exists())
        || target.detect_paths.iter().any(|path| path.is_dir())
        || target
            .detect_binaries
            .iter()
            .any(|binary| which(binary).is_some())
}

/// Label given to a skill root the caller named with `--skill-root`.
///
/// A named constant because `--force-full-cleanup` refuses anything else, and a literal in two
/// places could drift apart into a check that silently passes.
const CUSTOM_SKILL_TARGET_LABEL: &str = "custom";

fn custom_skill_targets(paths: &[PathBuf]) -> Result<Vec<SkillTarget>> {
    paths
        .iter()
        .map(|path| {
            let root = expand_tilde(path)?;
            let package = match root.file_name().and_then(|name| name.to_str()) {
                Some(AI_SESSION_SEARCH_SKILL_NAME) => &AI_SESSION_SEARCH_SKILL_PACKAGE,
                _ => {
                    bail!(
                        "custom skill root {} must end in {}",
                        root.display(),
                        AI_SESSION_SEARCH_SKILL_NAME
                    )
                }
            };
            Ok(skill_target_for_package(
                CUSTOM_SKILL_TARGET_LABEL,
                root,
                package,
                Vec::new(),
                Vec::new(),
            ))
        })
        .collect()
}

fn target_detected(target: &Target) -> bool {
    target.path.exists()
        || target
            .detect_paths
            .iter()
            .any(|path| path.exists() && path.is_dir())
        || target
            .detect_binaries
            .iter()
            .any(|binary| which(binary).is_some())
}

fn custom_instruction_targets(
    claude_md_paths: &[PathBuf],
    gemini_md_paths: &[PathBuf],
    agents_md_paths: &[PathBuf],
) -> Result<Vec<InstructionTarget>> {
    claude_md_paths
        .iter()
        .map(|path| {
            Ok(InstructionTarget {
                label: "custom-claude",
                path: expand_tilde(path)?,
                format: InstructionFormat::ClaudeImport,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
            })
        })
        .chain(gemini_md_paths.iter().map(|path| {
            Ok(InstructionTarget {
                label: "custom-gemini",
                path: expand_tilde(path)?,
                format: InstructionFormat::InlineBlock,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
            })
        }))
        .chain(agents_md_paths.iter().map(|path| {
            Ok(InstructionTarget {
                label: "custom-agents",
                path: expand_tilde(path)?,
                format: InstructionFormat::InlineBlock,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
            })
        }))
        .collect()
}

fn custom_targets(
    json_mcp_configs: &[PathBuf],
    vscode_configs: &[PathBuf],
    zed_configs: &[PathBuf],
    opencode_configs: &[PathBuf],
    codex_configs: &[PathBuf],
) -> Result<Vec<Target>> {
    json_mcp_configs
        .iter()
        .map(|path| {
            Ok(Target {
                label: "custom-json-mcp",
                path: expand_tilde(path)?,
                format: ConfigFormat::JsonMcpServers,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
                superseded_paths: Vec::new(),
                codex_marketplace: None,
            })
        })
        .chain(vscode_configs.iter().map(|path| {
            Ok(Target {
                label: "custom-vscode",
                path: expand_tilde(path)?,
                format: ConfigFormat::VscodeServers,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
                superseded_paths: Vec::new(),
                codex_marketplace: None,
            })
        }))
        .chain(zed_configs.iter().map(|path| {
            Ok(Target {
                label: "custom-zed",
                path: expand_tilde(path)?,
                format: ConfigFormat::ZedContextServers,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
                superseded_paths: Vec::new(),
                codex_marketplace: None,
            })
        }))
        .chain(opencode_configs.iter().map(|path| {
            Ok(Target {
                label: "custom-opencode",
                path: expand_tilde(path)?,
                format: ConfigFormat::OpenCode,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
                superseded_paths: Vec::new(),
                codex_marketplace: None,
            })
        }))
        .chain(codex_configs.iter().map(|path| {
            Ok(Target {
                label: "custom-codex",
                path: expand_tilde(path)?,
                format: ConfigFormat::CodexToml,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
                superseded_paths: Vec::new(),
                codex_marketplace: None,
            })
        }))
        .collect()
}

#[cfg(test)]
fn upsert_target(target: &Target, binary: &Path) -> Result<()> {
    publish_planned_mutations(&normalize_planned_mutations(plan_upsert_target(
        target, binary,
    )?)?)
}

fn preflight_install(
    targets: &[Target],
    instruction_targets: &[InstructionTarget],
    skill_targets: &[SkillTarget],
    binary: &Path,
    manifest_path: Option<&Path>,
) -> Result<Vec<PlannedFileMutation>> {
    let skill_manifest = match manifest_path {
        Some(path) => crate::skill_manifest::load_manifest(path)?,
        None => crate::skill_manifest::ManifestState::Absent,
    };
    let mut mutations = Vec::new();
    let mut retired_skill_roots = Vec::new();
    for target in targets {
        mutations.extend(plan_upsert_target(target, binary)?);
    }
    for target in instruction_targets {
        mutations.extend(plan_upsert_instruction_file(target)?);
    }
    for target in skill_targets {
        let migration = plan_migrate_discovery_copies(target, manifest_path)?;
        mutations.extend(migration.mutations);
        retired_skill_roots.extend(migration.retired_roots);
        let retired = plan_retire_discovery_entries(target, &skill_manifest)?;
        mutations.extend(retired.mutations);
        retired_skill_roots.extend(retired.retired_roots);
        if manifest_path.is_some() {
            mutations.extend(plan_retire_removed_managed_skill_files(
                target,
                &skill_manifest,
            )?);
        }
        mutations.extend(plan_refresh_skill_files(target, &skill_manifest, false)?);
    }
    if let Some(path) = manifest_path {
        if !skill_targets.is_empty() {
            mutations.extend(plan_record_skill_manifest(
                path,
                skill_targets,
                &retired_skill_roots,
            )?);
        }
    }
    normalize_planned_mutations(mutations)
}

/// Retire files that an earlier package version owned but the current package no longer ships.
///
/// A rename is an add plus a removal. The manifest is the durable evidence that makes the removal
/// safe: exact recorded bytes may be removed, while a modified file is preserved by refusing the
/// write before its transaction starts. Historical correction paths are excluded because their
/// removal has the stronger sibling-package requirement in [`plan_historical_skill_split`].
fn plan_retire_removed_managed_skill_files(
    target: &SkillTarget,
    manifest: &crate::skill_manifest::ManifestState,
) -> Result<Vec<PlannedFileMutation>> {
    let Some(installation) = manifest.installation(&target.root) else {
        return Ok(Vec::new());
    };
    let current_paths = target
        .package
        .files
        .iter()
        .map(|file| file.relative_path)
        .collect::<std::collections::BTreeSet<_>>();
    let mut mutations = Vec::new();

    for recorded_file in &installation.files {
        if current_paths.contains(recorded_file.relative_path.as_str())
            || (target.package.name == AI_SESSION_SEARCH_SKILL_PACKAGE.name
                && HISTORICAL_CORRECTION_PATHS.contains(&recorded_file.relative_path.as_str()))
        {
            continue;
        }
        let relative = Path::new(&recorded_file.relative_path);
        if recorded_file.relative_path.is_empty()
            || relative.is_absolute()
            || !relative
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
        {
            bail!(
                "refusing to retire invalid managed skill path {:?} recorded for {}; \
                 repair or move the install manifest before retrying",
                recorded_file.relative_path,
                target.root.display()
            );
        }

        let path = target.root.join(relative);
        let Some(original) = read_optional_utf8_regular_file(&path)? else {
            continue;
        };
        if recorded_file.bytes != original.len()
            || recorded_file.sha256 != crate::hashing::sha256(original.as_bytes())
        {
            bail!(
                "refusing to retire removed managed skill file {} because it differs from the \
                 install manifest; preserving it so aise cannot destroy a user edit or damaged \
                 file",
                path.display()
            );
        }
        mutations.push(PlannedFileMutation::Remove { path, original });
    }
    Ok(mutations)
}

/// Plan the manifest write that records what this install is about to place.
///
/// One more `PlannedFileMutation` in the SAME transaction, so the record and the files it
/// describes are published together or not at all. A manifest written afterwards could survive a
/// rollback and claim an install that never happened.
///
/// The recorded digests come from the embedded content rather than from re-reading disk: the
/// transaction is about to write exactly those bytes, and hashing the files afterwards would both
/// invert the ordering and risk recording whatever a concurrent writer left there instead.
///
/// The manifest is deliberately NOT in its own file list: it is not part of the skill, it must not
/// hash itself, and uninstall must never treat it as a skill file.
fn plan_record_skill_manifest(
    manifest_path: &Path,
    skill_targets: &[SkillTarget],
    retired_roots: &[PathBuf],
) -> Result<Vec<PlannedFileMutation>> {
    let state = crate::skill_manifest::load_manifest(manifest_path)?;
    let mut manifest = state.writable_manifest(manifest_path)?;
    let mut mutations = plan_historical_skill_split(&state, skill_targets)?;
    let retired_siblings = plan_retire_sibling_package(&state, skill_targets)?;
    mutations.extend(retired_siblings.mutations);
    for root in retired_siblings.retired_roots {
        manifest.forget(&root);
    }
    for root in retired_roots {
        manifest.forget(root);
    }
    for target in skill_targets {
        // Before this field existed, a shared link could only have been installed for Codex.
        let legacy_codex = state
            .installation(&target.root)
            .is_none_or(|entry| entry.discovery_clients.is_empty())
            && target.discovery_links.iter().any(|link| {
                link.parent()
                    .is_some_and(|root| root.ends_with(SHARED_SKILLS_PATH))
                    && fs::read_link(link).is_ok_and(|destination| destination == target.root)
            });
        let files = target
            .package
            .files
            .iter()
            .map(|file| (file.relative_path.to_string(), file.content))
            .collect::<Vec<_>>();
        manifest.record(&target.root, &files);
        let installation = manifest
            .installations
            .iter_mut()
            .find(|entry| {
                entry
                    .root
                    .to_path_buf()
                    .is_ok_and(|root| root == target.root)
            })
            .unwrap();
        installation
            .discovery_clients
            .extend(target.discovery_clients.iter().cloned());
        if legacy_codex {
            installation.discovery_clients.push("codex".to_string());
        }
        installation.discovery_clients.sort();
        installation.discovery_clients.dedup();
    }
    let original = read_optional_utf8_regular_file(manifest_path)?;
    mutations.push(planned_write(manifest_path, &original, manifest.to_json()?));
    Ok(mutations)
}

/// Remove the sibling `corrections` skill root that earlier releases installed.
///
/// Its rules now ship inside this package, so the sibling is dead weight that keeps claiming the
/// generic top-level name `corrections` in every harness skill directory. It is removed in the same
/// transaction that writes the replacement.
///
/// Ownership rules are uninstall's, not install's: [`plan_remove_skill_file`] refuses unless the
/// `SKILL.md` marker and the install manifest agree the tree is untouched, so a user edit or an
/// unmanaged extra file leaves the directory in place and is reported rather than deleted.
fn plan_retire_sibling_package(
    manifest: &crate::skill_manifest::ManifestState,
    skill_targets: &[SkillTarget],
) -> Result<SkillDiscoveryMigration> {
    let mut migration = SkillDiscoveryMigration::default();
    let mut planned = std::collections::BTreeSet::new();
    for target in skill_targets
        .iter()
        .filter(|target| target.package.name == AI_SESSION_SEARCH_SKILL_PACKAGE.name)
    {
        let candidates = std::iter::once(&target.root)
            .chain(target.discovery_links.iter())
            .filter_map(|path| path.parent())
            .map(|parent| parent.join(RETIRED_SKILL_PACKAGE_NAME));
        for root in candidates {
            // A caller may install into a directory that is itself named `corrections`, via
            // `--skill-root`. Retiring it would delete the package this very transaction is
            // writing, so the sibling search never selects the target or one of its own links.
            if root == target.root || target.discovery_links.contains(&root) {
                continue;
            }
            if !planned.insert(root.clone()) {
                continue;
            }
            match fs::symlink_metadata(&root) {
                Ok(metadata) if metadata.file_type().is_symlink() => continue,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("inspect retired skill package {}", root.display())
                    })
                }
            }
            let retired = SkillTarget {
                label: target.label,
                root: root.clone(),
                package: &AI_SESSION_SEARCH_SKILL_PACKAGE,
                discovery_links: Vec::new(),
                retired_discovery_links: Vec::new(),
                discovery_clients: Vec::new(),
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
            };
            let removal = plan_remove_skill_file(&retired, manifest, false)?;
            if !removal.preserved.is_empty() {
                bail!(
                    "refusing to retire the superseded corrections skill at {} because it may \
                     contain changes aise did not write:\n  {}\nmove or delete it yourself, then \
                     rerun install",
                    root.display(),
                    removal.preserved.join("\n  ")
                );
            }
            if removal.mutations.is_empty() {
                continue;
            }
            migration.mutations.extend(removal.mutations);
            migration.retired_roots.push(root);
        }
    }
    Ok(migration)
}

fn plan_retire_sibling_discovery_links(
    skill_targets: &[SkillTarget],
) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut links = Vec::new();
    let mut planned = std::collections::BTreeSet::new();
    for target in skill_targets
        .iter()
        .filter(|target| target.package.name == AI_SESSION_SEARCH_SKILL_PACKAGE.name)
    {
        let Some(canonical_parent) = target.root.parent() else {
            continue;
        };
        let expected = canonical_parent.join(RETIRED_SKILL_PACKAGE_NAME);
        for current_link in &target.discovery_links {
            let Some(link_parent) = current_link.parent() else {
                continue;
            };
            let link = link_parent.join(RETIRED_SKILL_PACKAGE_NAME);
            if !planned.insert(link.clone()) {
                continue;
            }
            match fs::symlink_metadata(&link) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("inspect retired skill discovery link {}", link.display())
                    })
                }
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    let existing = fs::read_link(&link).with_context(|| {
                        format!("read retired skill discovery link {}", link.display())
                    })?;
                    if existing != expected {
                        bail!(
                            "refusing to retire skill discovery link {} because it points to {} \
                             instead of the retired aise package {}; preserve or remove it yourself",
                            link.display(),
                            existing.display(),
                            expected.display()
                        );
                    }
                    links.push((link, expected.clone()));
                }
                Ok(_) => {}
            }
        }
    }
    Ok(links)
}

#[derive(Debug, Default)]
struct SkillDiscoveryMigration {
    mutations: Vec<PlannedFileMutation>,
    retired_roots: Vec<PathBuf>,
    notices: Vec<(PathBuf, String)>,
}

/// Retire obsolete copies using uninstall's marker, manifest, digest, and extra-file checks.
fn plan_retire_discovery_entries(
    target: &SkillTarget,
    manifest: &crate::skill_manifest::ManifestState,
) -> Result<SkillDiscoveryMigration> {
    let mut migration = SkillDiscoveryMigration::default();
    for link in &target.retired_discovery_links {
        match fs::symlink_metadata(link) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect retired skill entry {}", link.display()))
            }
            Ok(metadata) if metadata.file_type().is_symlink() => {
                if fs::read_link(link)? != target.root {
                    migration.notices.push((
                        link.clone(),
                        format!(
                            "preserved foreign or modified skill discovery link {}",
                            link.display()
                        ),
                    ));
                }
            }
            Ok(_) => {
                let mut legacy = target.clone();
                legacy.root = link.clone();
                legacy.discovery_links.clear();
                legacy.retired_discovery_links.clear();
                match plan_remove_skill_file(&legacy, manifest, false) {
                    Ok(removal) if removal.preserved.is_empty() => {
                        migration.mutations.extend(removal.mutations);
                        migration.retired_roots.push(link.clone());
                    }
                    Ok(removal) => migration.notices.push((
                        link.clone(),
                        format!(
                            "preserved modified skill directory {}: {}",
                            link.display(),
                            removal.preserved.join("; ")
                        ),
                    )),
                    Err(error) => migration.notices.push((
                        link.clone(),
                        format!("preserved skill directory {}: {error:#}", link.display()),
                    )),
                }
            }
        }
    }
    Ok(migration)
}

fn retired_discovery_outcomes(
    targets: &[SkillTarget],
    manifest: &crate::skill_manifest::ManifestState,
    links: &[SymlinkChange],
    dry_run: bool,
) -> Result<(Vec<SkillWriteOutcome>, Vec<PathBuf>)> {
    let mut outcomes = Vec::new();
    let mut directories = Vec::new();
    for target in targets {
        let retired = plan_retire_discovery_entries(target, manifest)?;
        for root in retired.retired_roots {
            let mut legacy = target.clone();
            legacy.root = root.clone();
            directories.extend(managed_skill_directories(&legacy));
            outcomes.push(SkillWriteOutcome {
                label: target.label.to_string(),
                root: root.display().to_string(),
                action: if dry_run {
                    "would remove retired discovery copy"
                } else {
                    "removed retired discovery copy"
                }
                .to_string(),
                problem: None,
                changed: !dry_run,
            });
        }
        for (root, problem) in retired.notices {
            outcomes.push(SkillWriteOutcome {
                label: target.label.to_string(),
                root: root.display().to_string(),
                action: "preserved retired discovery entry".to_string(),
                problem: Some(problem),
                changed: false,
            });
        }
        for link in links.iter().filter(|change| {
            change.after == SymlinkImage::Absent
                && target.retired_discovery_links.contains(&change.path)
        }) {
            outcomes.push(SkillWriteOutcome {
                label: target.label.to_string(),
                root: link.path.display().to_string(),
                action: if dry_run {
                    "would remove retired discovery link"
                } else {
                    "removed retired discovery link"
                }
                .to_string(),
                problem: None,
                changed: !dry_run,
            });
        }
    }
    Ok((outcomes, directories))
}

/// Replace an older copied harness package only when it is provably untouched.
///
/// Older releases wrote complete packages into each harness directory. A current install turns
/// those locations into discovery links, but the same ownership rules as uninstall apply: a
/// modified file, an unmanaged extra, or an unowned directory blocks migration before any write.
fn plan_migrate_discovery_copies(
    target: &SkillTarget,
    manifest_path: Option<&Path>,
) -> Result<SkillDiscoveryMigration> {
    let manifest = match manifest_path {
        Some(path) => crate::skill_manifest::load_manifest(path)?,
        None => crate::skill_manifest::ManifestState::Absent,
    };
    let mut migration = SkillDiscoveryMigration::default();
    for link in &target.discovery_links {
        match fs::symlink_metadata(link) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect skill discovery entry {}", link.display()))
            }
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let existing = fs::read_link(link)
                    .with_context(|| format!("read skill discovery link {}", link.display()))?;
                if existing != target.root {
                    bail!(
                        "refusing to replace skill discovery link {} because it points to {} \
                         instead of the aise package {}; preserve or remove it yourself",
                        link.display(),
                        existing.display(),
                        target.root.display()
                    );
                }
            }
            Ok(_) => {
                let mut legacy = target.clone();
                legacy.root = link.clone();
                legacy.discovery_links.clear();
                let removal = plan_remove_skill_file(&legacy, &manifest, false)?;
                if !removal.preserved.is_empty() {
                    bail!(
                        "refusing to replace legacy copied skill directory {} with a discovery \
                         link because it may contain user changes:\n  {}",
                        link.display(),
                        removal.preserved.join("\n  ")
                    );
                }
                migration.mutations.extend(removal.mutations);
                migration.retired_roots.push(link.clone());
            }
        }
    }
    Ok(migration)
}

/// Remove prerelease correction files only when valid manifest evidence proves they are untouched.
///
/// The old files and both replacement packages share one transaction. If evidence is missing,
/// damaged, or differs from disk, planning fails before any mutation is published.
fn plan_historical_skill_split(
    manifest: &crate::skill_manifest::ManifestState,
    skill_targets: &[SkillTarget],
) -> Result<Vec<PlannedFileMutation>> {
    let mut mutations = Vec::new();
    for general in skill_targets
        .iter()
        .filter(|target| target.package.name == AI_SESSION_SEARCH_SKILL_PACKAGE.name)
    {
        let recorded = manifest.installation(&general.root);

        for relative_path in HISTORICAL_CORRECTION_PATHS {
            let path = general.root.join(relative_path);
            let disk = read_optional_utf8_regular_file(&path)?;
            let recorded_file = recorded.and_then(|installation| installation.file(relative_path));
            match (disk, recorded_file) {
                (None, None) => {}
                (Some(_), None) => bail!(
                    "refusing to migrate unrecorded historical skill file {}; ownership is \
                     uncertain, so inspect or move it before installing the current \
                     ai-session-search package",
                    path.display()
                ),
                (None, Some(_)) => bail!(
                    "refusing to migrate historical skill file {} because the install manifest \
                     records it but it is missing; repair or move the damaged skill tree first",
                    path.display()
                ),
                (Some(original), Some(recorded_file)) => {
                    if recorded_file.bytes != original.len()
                        || recorded_file.sha256 != crate::hashing::sha256(original.as_bytes())
                    {
                        bail!(
                            "refusing to migrate historical skill file {} because it differs \
                             from the install manifest; inspect or move it so aise cannot destroy \
                             an edit or damaged file",
                            path.display()
                        );
                    }
                    mutations.push(PlannedFileMutation::Remove { path, original });
                }
            }
        }
    }
    Ok(mutations)
}

/// Plan the manifest write that forgets the roots this uninstall is removing.
///
/// Only the roots being removed lose their entry. An install into another harness must keep its
/// record, or a later uninstall there would report "ownership uncertain" about a tree `aise`
/// definitely wrote.
fn plan_forget_skill_manifest(
    manifest_path: &Path,
    skill_targets: &[SkillTarget],
    released_targets: &[SkillTarget],
) -> Result<Vec<PlannedFileMutation>> {
    let Some(original) = read_optional_utf8_regular_file(manifest_path)? else {
        return Ok(Vec::new());
    };
    let state = crate::skill_manifest::load_manifest(manifest_path)?;
    let mut manifest = state.writable_manifest(manifest_path)?;
    for installation in &mut manifest.installations {
        for target in released_targets {
            if installation.root.to_path_buf()? == target.root {
                installation
                    .discovery_clients
                    .retain(|client| !target.discovery_clients.contains(client));
            }
        }
    }
    for target in skill_targets {
        manifest.forget(&target.root);
    }
    if manifest.installations.is_empty() {
        // Nothing left to record. Removing the file rather than leaving an empty one keeps
        // "absent" meaning "no managed install", which is what a fresh machine also looks like.
        return Ok(vec![PlannedFileMutation::Remove {
            path: manifest_path.to_path_buf(),
            original,
        }]);
    }
    Ok(vec![planned_write(
        manifest_path,
        &Some(original),
        manifest.to_json()?,
    )])
}

/// Refuse `--force-full-cleanup` unless it names exactly what it will destroy.
///
/// The flag exists so a user CAN delete their own edits, deliberately. Every constraint here
/// exists so they cannot do it by accident:
///
/// 1. An exact `--skill-root` is required. Detected-client fan-out would let one flag delete
///    edits in three harness directories the user never named.
/// 2. `--keep-skill` cannot be combined with it: one says preserve, the other says destroy.
/// 3. Each named root must be provably aise's -- the managed marker, or an install-manifest
///    entry. Without that, this is an arbitrary recursive delete of a directory aise never wrote.
///
/// It never reaches the index, cache, or configuration: `skill_targets` are the only paths it can
/// ever see, and those are skill directories by construction.
fn validate_force_full_cleanup(
    args: &IntegrationUninstallArgs,
    skill_targets: &[SkillTarget],
) -> Result<()> {
    if args.keep_skill {
        bail!(
            "--force-full-cleanup and --keep-skill contradict each other: one deletes the whole \
             skill directory, the other preserves it. Pass exactly one"
        );
    }
    if args.targets.skill_roots.is_empty() {
        bail!(
            "--force-full-cleanup requires an exact --skill-root DIR. It deletes files you wrote, \
             so it will not run against automatically detected client directories. Run \
             `aise integrations status` to see the installed roots, then name one"
        );
    }
    // Detected client directories are DROPPED rather than refused. Erroring here was a trap:
    // once a skill is installed, `~/.claude` and friends are always detected, so a caller who
    // named an exact root could never satisfy the check and had no flag to escape it. Scoping to
    // the named roots is also the only reading of the flag that is safe -- it says "this exact
    // root" -- so resolving it is better than reporting it.
    let dropped: Vec<&Path> = skill_targets
        .iter()
        .filter(|target| target.label != CUSTOM_SKILL_TARGET_LABEL)
        .map(|target| target.root.as_path())
        .collect();
    for root in &dropped {
        println!(
            "--force-full-cleanup applies only to roots you named, so {} is left untouched. \
             Uninstall it separately, or name it with --skill-root.",
            root.display()
        );
    }
    Ok(())
}

fn preflight_uninstall(
    targets: &[Target],
    instruction_targets: &[InstructionTarget],
    skill_targets: &[SkillTarget],
    released_targets: &[SkillTarget],
    manifest_path: Option<&Path>,
    force_full_cleanup: bool,
) -> Result<UninstallPlan> {
    let skill_manifest = match manifest_path {
        Some(path) => crate::skill_manifest::load_manifest(path)?,
        None => crate::skill_manifest::ManifestState::Absent,
    };
    let mut mutations = Vec::new();
    let mut changed_targets = Vec::new();
    let mut changed_instructions = Vec::new();
    let mut changed_skills = Vec::new();
    for target in targets {
        let planned = plan_remove_target(target)?;
        changed_targets.push(!planned.is_empty());
        mutations.extend(planned);
    }
    for target in instruction_targets {
        let planned = plan_remove_instruction_file(target)?;
        changed_instructions.push(!planned.is_empty());
        mutations.extend(planned);
    }
    let mut preserved_skills = Vec::with_capacity(skill_targets.len());
    let mut prune_skill_directories = Vec::new();
    for target in skill_targets {
        let planned = plan_remove_skill_file(target, &skill_manifest, force_full_cleanup)?;
        changed_skills.push(!planned.mutations.is_empty());
        preserved_skills.push(planned.preserved);
        prune_skill_directories.extend(planned.prune);
        mutations.extend(planned.mutations);
    }
    if let Some(path) = manifest_path {
        // Only roots whose files were actually removed lose their record. Forgetting a preserved
        // tree would strip the very evidence that lets a later run explain why it was preserved.
        let removed: Vec<SkillTarget> = skill_targets
            .iter()
            .zip(&preserved_skills)
            .filter(|(_, preserved)| preserved.is_empty())
            .map(|(target, _)| target.clone())
            .collect();
        if !removed.is_empty() || !released_targets.is_empty() {
            mutations.extend(plan_forget_skill_manifest(
                path,
                &removed,
                released_targets,
            )?);
        }
    }
    Ok(UninstallPlan {
        mutations: normalize_planned_mutations(mutations)?,
        preserved_skills,
        prune_skill_directories,
        changed_targets,
        changed_instructions,
        changed_skills,
    })
}

/// Test the transaction path independently of manifest-aware ownership classification.
///
/// Production install and update callers use [`plan_refresh_skill_files`], which consults the
/// per-file manifest before planning any replacement. This lower-level helper intentionally
/// models only the marker-based, manifest-absent fixture used by transaction tests.
#[cfg(test)]
fn plan_upsert_skill_file(target: &SkillTarget) -> Result<Vec<PlannedFileMutation>> {
    let anchor = target.anchor();
    if let Some(text) = read_optional_utf8_regular_file(&anchor)?.as_deref() {
        if !is_managed_skill_anchor(text) {
            bail!(
                "refusing to replace unmanaged AI Session Search skill {}; move it or choose \
                 another --skill-root",
                anchor.display()
            );
        }
    }
    target
        .managed_paths()
        .map(|(path, content)| {
            let original = read_optional_utf8_regular_file(&path)?;
            Ok(planned_write(&path, &original, content.to_string()))
        })
        .collect()
}

/// Plan the writes that repair or update one owned skill directory.
///
/// `overwrite_changed` is the difference between the two callers. Normal install REFUSES a file
/// whose bytes changed since install, because silently replacing it would destroy an edit the
/// user may have meant; `aise skills restore` sets this to overwrite, which is what makes it the
/// explicit, named repair path rather than a surprise.
///
/// Unmanaged files are never in the package file list, so nothing here can touch them.
fn plan_refresh_skill_files(
    target: &SkillTarget,
    manifest: &crate::skill_manifest::ManifestState,
    overwrite_changed: bool,
) -> Result<Vec<PlannedFileMutation>> {
    let anchor = target.anchor();
    let owned_by_marker = read_optional_utf8_regular_file(&anchor)?
        .as_deref()
        .is_some_and(is_managed_skill_anchor);
    let recorded = manifest.installation(&target.root);
    if !owned_by_marker && recorded.is_none() && crate::durable_fs::entry_exists(&anchor)? {
        bail!(
            "refusing to write unmanaged AI Session Search skill {}; it carries no managed \
             marker and no install record, so aise did not write it",
            anchor.display()
        );
    }

    let mut mutations = Vec::new();
    for file in target.package.files {
        let path = target.root.join(file.relative_path);
        let state = classify_managed_file(
            &path,
            file.content,
            recorded.and_then(|installation| installation.file(file.relative_path)),
        )?;
        if !overwrite_changed
            && matches!(
                state,
                ManagedFileState::ModifiedOrDamaged | ManagedFileState::LegacyUnknown
            )
        {
            bail!(
                "refusing to overwrite {}: it differs from what install recorded, so replacing \
                 it could destroy an edit you meant to keep.\n\
                 Inspect it, or run `aise skills restore {} --skill-root {} --dry-run` to see \
                 exactly what a repair would rewrite",
                path.display(),
                target.package.name,
                target.root.display()
            );
        }
        let original = read_optional_utf8_regular_file(&path)?;
        mutations.push(planned_write(&path, &original, file.content.to_string()));
    }
    Ok(mutations)
}

/// One skill root as `aise skills update` and `aise skills restore` report it.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct SkillWriteOutcome {
    /// Which client root this is, or `custom` for a root the caller named.
    pub(crate) label: String,
    pub(crate) root: String,
    /// What happened, or would happen under `--dry-run`.
    pub(crate) action: String,
    /// Why nothing was written, when that is the outcome.
    pub(crate) problem: Option<String>,
    /// Whether files on disk changed. `action` is wording for a reader; this is what code tests.
    #[serde(skip)]
    pub(crate) changed: bool,
}

/// Bring every owned skill root up to the embedded content, or repair one exact root.
///
/// The SAME planner and transaction `integrations install` uses, restricted to skills. A separate
/// writing path would be a second place for ownership rules to drift.
///
/// # Errors
///
/// Returns an error when a root cannot be planned for a reason that is not about ownership, when
/// the transaction fails, or when `restore` names a root aise does not own.
pub(crate) fn write_owned_skills(
    explicit_roots: &[PathBuf],
    receipt_path: &Path,
    dry_run: bool,
    overwrite_changed: bool,
) -> Result<Vec<SkillWriteOutcome>> {
    let manifest_path = crate::skill_manifest::manifest_path(receipt_path);
    let manifest = crate::skill_manifest::load_manifest(&manifest_path)?;
    // A write command must not silently succeed, rewrite package bytes, or replace a damaged
    // ownership record. Status and conservative removal planning may still inspect this state.
    let _ = manifest.writable_manifest(&manifest_path)?;

    // Client discovery runs ONLY when no root was named. Naming roots is the precise request, so
    // resolving a home directory to answer it would make the command fail on a machine where HOME
    // is unset for a reason unrelated to what was asked.
    let mut targets: Vec<SkillTarget> = if explicit_roots.is_empty() {
        let layout = ClientLayout::discover()?;
        McpClient::value_variants()
            .iter()
            .copied()
            .flat_map(|client| skill_targets_for_layout(client, &layout))
            .filter(skill_target_detected)
            .collect()
    } else {
        Vec::new()
    };
    // Move the discovered roots beside the config file before comparing them with the recorded
    // ones, which install wrote there: with a config outside the default location, comparing the
    // unmoved roots added the recorded app root a second time, as custom, and the command refused.
    rebase_automatic_skill_roots(&mut targets, receipt_path);
    if explicit_roots.is_empty() {
        // Only directories that still exist: updating is not installing. A skill the user deleted
        // stays deleted, and a harness installed with --no-skill gets none;
        // `aise integrations install` or a named --skill-root puts one there on request.
        targets.retain(|target| target.root.is_dir());
        let discovered_roots = targets
            .iter()
            .map(|target| target.root.clone())
            .collect::<std::collections::HashSet<_>>();
        let recorded_roots = manifest_recorded_skill_roots(&manifest)?
            .into_iter()
            .filter(|root| root.is_dir() && !discovered_roots.contains(root))
            .collect::<Vec<_>>();
        targets.extend(custom_skill_targets(&recorded_roots)?);
    } else {
        targets.extend(custom_skill_targets(explicit_roots)?);
    }
    // Updating package bytes must not claim a newly detected harness as an installed user.
    for target in &mut targets {
        target.discovery_clients.clear();
    }
    dedupe_skill_targets(&mut targets)?;

    let mut outcomes = Vec::with_capacity(targets.len());
    let mut mutations = Vec::new();
    let mut written: Vec<SkillTarget> = Vec::new();
    let mut refreshable: Vec<SkillTarget> = Vec::new();
    for target in &targets {
        let status = status_skill_file(target, &manifest)?;
        match plan_refresh_skill_files(target, &manifest, overwrite_changed).and_then(
            |mut planned| {
                planned.extend(plan_retire_removed_managed_skill_files(target, &manifest)?);
                Ok(planned)
            },
        ) {
            Ok(planned) => {
                let noop = planned.iter().all(PlannedFileMutation::is_noop);
                refreshable.push(target.clone());
                outcomes.push(SkillWriteOutcome {
                    label: target.label.to_string(),
                    root: target.root.display().to_string(),
                    action: if noop {
                        "already current".to_string()
                    } else if dry_run {
                        format!("would rewrite from {status}")
                    } else {
                        format!("rewritten from {status}")
                    },
                    problem: None,
                    changed: !noop && !dry_run,
                });
                if !noop {
                    mutations.extend(planned);
                    written.push(target.clone());
                }
            }
            // Ownership and modified-file refusals are DIAGNOSED per root rather than aborting
            // the run: one hand-edited harness directory must not stop the other three from
            // being updated.
            Err(error) => outcomes.push(SkillWriteOutcome {
                label: target.label.to_string(),
                root: target.root.display().to_string(),
                action: "not written".to_string(),
                problem: Some(format!("{error:#}")),
                changed: false,
            }),
        }
    }

    let historical = plan_historical_skill_split(&manifest, &refreshable)?;
    let retired_siblings = plan_retire_sibling_package(&manifest, &refreshable)?;
    let retired_discovery_links = plan_retire_sibling_discovery_links(&refreshable)?;
    let migration_requires_manifest =
        !historical.is_empty() || !retired_siblings.mutations.is_empty();
    if !historical.is_empty() {
        mutations.extend(historical);
    }
    if migration_requires_manifest {
        // Re-record every successfully validated package in this sibling group. This is safe even
        // when its bytes were already current, and it prevents a retired package or old general
        // package record from surviving after its files move.
        for target in &refreshable {
            if !written.iter().any(|existing| {
                existing.root == target.root && existing.package.name == target.package.name
            }) {
                written.push(target.clone());
            }
        }
    }

    if !mutations.is_empty() || !retired_siblings.mutations.is_empty() {
        mutations.extend(plan_record_skill_manifest(&manifest_path, &written, &[])?);
    }
    let discovery_link_changes = retired_discovery_links
        .iter()
        .map(|(link, expected)| SymlinkChange::remove(link.clone(), expected.clone()))
        .collect::<Vec<_>>();
    let normalized = normalize_planned_mutations(mutations)?;
    if !dry_run && (!normalized.is_empty() || !discovery_link_changes.is_empty()) {
        execute_planned_transaction_with_links(
            receipt_path,
            &normalized,
            &discovery_link_changes,
            |_, change| publish_symlink_change(change),
        )?;
        prune_retired_skill_directories(&retired_siblings.retired_roots);
    }
    for root in retired_siblings.retired_roots {
        outcomes.push(SkillWriteOutcome {
            label: "retired corrections".to_string(),
            root: root.display().to_string(),
            action: if dry_run {
                "would remove retired package".to_string()
            } else {
                "removed retired package".to_string()
            },
            problem: None,
            changed: !dry_run,
        });
    }
    for (link, _) in retired_discovery_links {
        outcomes.push(SkillWriteOutcome {
            label: "retired corrections".to_string(),
            root: link.display().to_string(),
            action: if dry_run {
                "would remove retired discovery link".to_string()
            } else {
                "removed retired discovery link".to_string()
            },
            problem: None,
            changed: !dry_run,
        });
    }
    Ok(outcomes)
}

/// What the post-upgrade refresh did, so the caller can say so in one line.
#[derive(Debug)]
pub(crate) enum UpgradeRefresh {
    /// Already refreshed for this version, or no aise-owned skill is recorded as installed.
    NotNeeded,
    /// Every recorded skill root and every client instruction file was checked once.
    Checked {
        skills: Vec<SkillWriteOutcome>,
        instructions: Vec<InstructionRefresh>,
        /// Client config files whose aise MCP entry moved to where the client now reads it.
        mcp: Vec<PathBuf>,
    },
    /// Another aise process holds the integration lock; the next command tries again.
    Deferred,
}

/// What the post-upgrade refresh did to one client instruction file.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InstructionRefresh {
    /// Its aise block held text an earlier release wrote and now holds this build's.
    Updated(PathBuf),
    /// Its aise block holds text aise never wrote, so it was left as the user wrote it.
    Edited(PathBuf),
}

/// Bring owned, unedited integration files up to this build once after aise changes version.
///
/// Every release changes the packaged skill, if only its `metadata.version` line, so without this
/// an upgrade through `uv tool upgrade`, pip, Cargo, or a native archive left the installed skill
/// outdated until someone ran `aise integrations install`. Only `aise package update` refreshed it.
/// Skills go through the same conservative planner as `aise skills update`: files still
/// byte-identical to what aise recorded are rewritten, and an edited file is left alone and
/// reported. It visits only the roots the install manifest records, so it never discovers or
/// configures a new client. Instruction blocks in the clients' CLAUDE.md, AGENTS.md, and GEMINI.md
/// are replaced in place only while they hold text some release wrote.
///
/// # Errors
///
/// Returns an error when the manifest cannot be read, a root cannot be planned for a reason that is
/// not about ownership, or the transaction fails for a reason other than lock contention.
pub(crate) fn refresh_integrations_after_upgrade(
    receipt_path: &Path,
    config: &IntegrationsConfig,
) -> Result<UpgradeRefresh> {
    refresh_integrations_for_version(
        receipt_path,
        env!("CARGO_PKG_VERSION"),
        Some(&ClientLayout::discover()?.with_config(config)?),
    )
}

/// `layout` locates the client instruction files; `None` checks skills only.
fn refresh_integrations_for_version(
    receipt_path: &Path,
    version: &str,
    layout: Option<&ClientLayout>,
) -> Result<UpgradeRefresh> {
    if !is_newer_than_refreshed(receipt_path, version) {
        return Ok(UpgradeRefresh::NotNeeded);
    }
    let manifest_path = crate::skill_manifest::manifest_path(receipt_path);
    let manifest = crate::skill_manifest::load_manifest(&manifest_path)?;
    let mut roots = manifest_recorded_skill_roots(&manifest)
        .with_context(|| format!("read the skill install record {}", manifest_path.display()))?;
    // Instruction files hold no install record, so the recorded skill is the evidence that aise
    // installed integrations with this configuration; without one there is nothing to refresh,
    // and a process run with its own empty configuration never reads the user's client files.
    if roots.is_empty() {
        return Ok(UpgradeRefresh::NotNeeded);
    }
    // A recorded root whose directory is gone was removed by the user; an upgrade must not put it
    // back. `aise integrations install` restores it on request.
    roots.retain(|root| root.is_dir());
    let outcome = (|| {
        let mut skills = match layout {
            Some(layout) => refresh_gemini_skill_discovery(receipt_path, layout)?,
            None => Vec::new(),
        };
        // Migration may retire recorded copies and add the canonical package.
        roots =
            manifest_recorded_skill_roots(&crate::skill_manifest::load_manifest(&manifest_path)?)?;
        roots.retain(|root| root.is_dir());
        skills.extend(if roots.is_empty() {
            Vec::new()
        } else {
            write_owned_skills(&roots, receipt_path, false, false)?
        });
        let (instructions, mcp) = match layout {
            Some(layout) => {
                // Register newly supported MCP before replacing the old CLI-only ownership proof.
                let mcp = refresh_superseded_mcp_entries(receipt_path, layout, || {
                    resolve_mcp_binary(None)
                })?;
                (
                    refresh_earlier_instruction_blocks(receipt_path, layout)?,
                    mcp,
                )
            }
            None => (Vec::new(), Vec::new()),
        };
        Ok::<_, anyhow::Error>((skills, instructions, mcp))
    })();
    let (skills, instructions, mcp) = match outcome {
        Ok(checked) => checked,
        Err(error)
            if error
                .downcast_ref::<crate::text_file_transaction::IntegrationLockHeld>()
                .is_some() =>
        {
            return Ok(UpgradeRefresh::Deferred)
        }
        Err(error) => return Err(error),
    };
    // Recorded only after the check completed, so a failure is retried by the next command rather
    // than silenced until the next release; an edited file is an outcome, not a failure, and is
    // reported once. A config directory that cannot hold the marker only costs repeating a check
    // that is safe to repeat, so failing to write it is not reported.
    record_refreshed_version(receipt_path, version);
    Ok(UpgradeRefresh::Checked {
        skills,
        instructions,
        mcp,
    })
}

/// Only an owned legacy Gemini entry authorizes moving discovery during an upgrade.
fn refresh_gemini_skill_discovery(
    receipt: &Path,
    layout: &ClientLayout,
) -> Result<Vec<SkillWriteOutcome>> {
    let manifest_path = crate::skill_manifest::manifest_path(receipt);
    let manifest = crate::skill_manifest::load_manifest(&manifest_path)?;
    let mut targets = skill_targets_for_layout(McpClient::Gemini, layout);
    rebase_automatic_skill_roots(&mut targets, receipt);
    let mut outcomes = Vec::new();
    for target in targets {
        let retired = plan_retire_discovery_entries(&target, &manifest)?;
        let owned_link = target
            .retired_discovery_links
            .iter()
            .any(|link| fs::read_link(link).is_ok_and(|destination| destination == target.root));
        if owned_link && !target.root.is_dir() && retired.retired_roots.is_empty() {
            outcomes.push(SkillWriteOutcome {
                label: target.label.to_string(),
                root: target.retired_discovery_links[0].display().to_string(),
                action: "preserved retired discovery link".to_string(),
                problem: Some(format!(
                    "canonical package {} is missing; upgrade will not reinstall a deleted skill",
                    target.root.display()
                )),
                changed: false,
            });
            continue;
        }
        if !owned_link && retired.retired_roots.is_empty() {
            outcomes.extend(retired_discovery_outcomes(&[target], &manifest, &[], false)?.0);
            continue;
        }
        let targets = [target];
        let mutations =
            preflight_install(&[], &[], &targets, Path::new("aise"), Some(&manifest_path))?;
        let links = plan_install_skill_discovery_links(&targets)?;
        let (retired_outcomes, directories) =
            retired_discovery_outcomes(&targets, &manifest, &links, false)?;
        execute_planned_transaction_with_links(receipt, &mutations, &links, |_, change| {
            publish_install_skill_discovery_link(change, &targets)
        })?;
        prune_emptied_skill_directories(&directories);
        outcomes.extend(retired_outcomes);
        for change in links
            .iter()
            .filter(|change| change.after != SymlinkImage::Absent)
        {
            outcomes.push(SkillWriteOutcome {
                label: "app".to_string(),
                root: change.path.display().to_string(),
                action: "linked skill discovery entry".to_string(),
                problem: None,
                changed: true,
            });
        }
    }
    Ok(outcomes)
}

/// Move every aise MCP entry an earlier release wrote where its client no longer reads it: a
/// superseded container (`mcp.<name>` for OpenCode, `mcpServers` for OpenClaw) or a superseded
/// file (Zed's macOS Application Support settings). The entry is rewritten through the same
/// planner as `aise integrations install`, with the executable the old entry named, so an upgrade
/// never changes which binary a client launches. A client with no aise entry in an old place is
/// left alone, which keeps an upgrade from configuring new clients, and a file aise cannot read
/// or parse is skipped: `aise integrations install` reports it.
/// An untouched CLI-only managed block also proves an existing integration: register newly
/// supported MCP with the CLI's resolved aise before refreshing that block. Errors in this case
/// leave the CLI guidance intact and the upgrade refresh retryable.
fn refresh_superseded_mcp_entries(
    receipt_path: &Path,
    layout: &ClientLayout,
    resolve_cli_binary: impl Fn() -> Result<PathBuf>,
) -> Result<Vec<PathBuf>> {
    let mut seen = std::collections::HashSet::new();
    let mut mutations = Vec::new();
    let mut moved = Vec::new();
    let targets = CONCRETE_CLIENTS.into_iter().flat_map(|client| {
        targets_for_layout(client, layout)
            .into_iter()
            .map(move |target| (client, target))
    });
    for (client, mut target) in targets {
        attach_codex_marketplace(std::slice::from_mut(&mut target), receipt_path);
        if !seen.insert(target.path.clone()) {
            continue;
        }
        // Only the exact old managed CLI block authorizes a new registration. Edited blocks
        // and other harness instructions cannot turn discovery into an implicit install.
        let was_cli_only =
            instruction_targets_for_layout(client, layout)
                .iter()
                .any(|instruction| {
                    instruction.format == InstructionFormat::InlineBlock
                        && read_optional_utf8_regular_file(&instruction.path)
                            .ok()
                            .flatten()
                            .is_some_and(|text| {
                                text.lines().any(|line| {
                                    line.trim() == InstructionFormat::InlineCliBlock.line()
                                }) && matches!(
                                    refreshed_instruction_block(&text, instruction.format),
                                    BlockRefresh::Replace(_)
                                )
                            })
                });
        let existing_binary =
            superseded_entry_command(&target).or_else(|| codex_entry_without_marketplace(&target));
        let binary = match existing_binary {
            Some(binary) => binary,
            None if was_cli_only
                && matches!(status_target(&target)?, "missing" | "not configured") =>
            {
                binary_config_value(&resolve_cli_binary()?)?.to_string()
            }
            None => continue,
        };
        let planned = plan_upsert_target(&target, Path::new(&binary));
        if was_cli_only {
            mutations.extend(planned?);
            moved.push(target.path.clone());
        } else if let Ok(planned) = planned {
            mutations.extend(planned);
            moved.push(target.path.clone());
        }
    }
    let mutations = normalize_planned_mutations(mutations)?;
    if !mutations.is_empty() {
        execute_planned_transaction_with_links(receipt_path, &mutations, &[], |_, _| Ok(()))?;
    }
    Ok(moved)
}

/// The executable of aise's current Codex `[mcp_servers]` entry when aise's plugin marketplace is
/// missing or outdated beside it, as after an upgrade from a release that registered only the
/// server. `None` when the target has no marketplace root or no current aise entry.
fn codex_entry_without_marketplace(target: &Target) -> Option<String> {
    let root = target.codex_marketplace.as_deref()?;
    if status_codex_mcp_server(&target.path).ok()? != "configured"
        || codex_marketplace_is_current(&target.path, root)
    {
        return None;
    }
    let text = read_optional_utf8_regular_file(&target.path)
        .ok()
        .flatten()?;
    let document = text.parse::<toml_edit::DocumentMut>().ok()?;
    document
        .get("mcp_servers")?
        .get(SERVER_NAME)?
        .get("command")?
        .as_str()
        .map(str::to_string)
}

/// The executable named by an aise entry left in a superseded place or entry shape, if any.
fn superseded_entry_command(target: &Target) -> Option<String> {
    let layout = json_server_layout(target.format)?;
    let read = |path: &Path| {
        let text = read_optional_utf8_regular_file(path).ok().flatten()?;
        parse_json_object_or_empty(path, Some(&text), layout.syntax).ok()
    };
    let in_old_files = target.superseded_paths.iter().find_map(|path| {
        let root = read(path)?;
        std::iter::once(layout.container)
            .chain(layout.superseded.iter().copied())
            .find_map(|keys| owned_entry_command(&root, keys))
    });
    in_old_files.or_else(|| {
        let root = read(&target.path)?;
        layout
            .superseded
            .iter()
            .find_map(|keys| owned_entry_command(&root, keys))
            .or_else(|| {
                let entry = json_container(&root, layout.container)?.get(SERVER_NAME)?;
                // Earlier aise omitted the transport tag. Repair only that known shape;
                // explicit transports and custom argument lists remain the user's choice.
                (entry.get("type").is_none()
                    && json_entry_is_current(entry, ConfigFormat::JsonMcpServers)
                    && !json_entry_is_current(entry, target.format))
                .then(|| owned_entry_command(&root, layout.container))
                .flatten()
            })
    })
}

/// The command of aise's entry under `keys`: a string, or the first element of a command array.
fn owned_entry_command(root: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    let servers = json_container(root, keys)?;
    std::iter::once(SERVER_NAME)
        .chain(LEGACY_SERVER_NAMES)
        .find_map(|name| {
            let command = servers.get(name)?.get("command")?;
            command
                .as_str()
                .or_else(|| command.as_array()?.first()?.as_str())
                .filter(|command| !command.is_empty())
                .map(str::to_string)
        })
}

/// Replace every client instruction block that holds text an earlier release wrote.
///
/// The block is rewritten where it stands, unlike `aise integrations install`, which moves it to
/// the end of the file. A symlinked, non-UTF-8, or unreadable file is skipped: install refuses such
/// files, so aise never wrote a block into one.
fn refresh_earlier_instruction_blocks(
    receipt_path: &Path,
    layout: &ClientLayout,
) -> Result<Vec<InstructionRefresh>> {
    let mut seen = std::collections::HashSet::new();
    let mut mutations = Vec::new();
    let mut refreshes = Vec::new();
    for target in CONCRETE_CLIENTS
        .into_iter()
        .flat_map(|client| instruction_targets_for_layout(client, layout))
    {
        if !seen.insert(target.path.clone()) {
            continue;
        }
        let path = match target.format {
            InstructionFormat::ClaudeImport => {
                let imports_it = read_optional_utf8_regular_file(&target.path)
                    .ok()
                    .flatten()
                    .is_some_and(|text| text.lines().any(is_instruction_reference_line));
                if !imports_it {
                    continue;
                }
                aise_instruction_path(&target.path)
            }
            InstructionFormat::InlineBlock | InstructionFormat::InlineCliBlock => {
                target.path.clone()
            }
        };
        let Some(original) = read_optional_utf8_regular_file(&path).ok().flatten() else {
            continue;
        };
        let refreshed = match target.format {
            InstructionFormat::ClaudeImport => refreshed_instruction_file(&original),
            format => refreshed_instruction_block(&original, format),
        };
        match refreshed {
            BlockRefresh::Current => {}
            BlockRefresh::Edited => refreshes.push(InstructionRefresh::Edited(path)),
            BlockRefresh::Replace(next) => {
                mutations.push(planned_write(&path, &Some(original), next));
                refreshes.push(InstructionRefresh::Updated(path));
            }
        }
    }
    let mutations = normalize_planned_mutations(mutations)?;
    if !mutations.is_empty() {
        execute_planned_transaction_with_links(receipt_path, &mutations, &[], |_, _| Ok(()))?;
    }
    Ok(refreshes)
}

enum BlockRefresh {
    /// Already this build's text, or not an aise block this refresh can read.
    Current,
    Edited,
    Replace(String),
}

/// Whether `line` is instruction text some release of aise wrote.
fn is_aise_instruction_line(line: &str) -> bool {
    [
        INSTRUCTIONS_LINE,
        CLI_INSTRUCTIONS_LINE,
        LEGACY_INSTRUCTIONS_LINE,
    ]
    .contains(&line)
        || EARLIER_INSTRUCTION_LINE_DIGESTS
            .contains(&crate::hashing::sha256(line.as_bytes()).as_str())
}

/// The inline block in AGENTS.md or GEMINI.md, brought up to this build if aise wrote its text.
fn refreshed_instruction_block(text: &str, format: InstructionFormat) -> BlockRefresh {
    if text.matches(INSTRUCTIONS_START).count() != 1 || text.matches(INSTRUCTIONS_END).count() != 1
    {
        return BlockRefresh::Current;
    }
    let (Some(start), Some(end)) = (text.find(INSTRUCTIONS_START), text.find(INSTRUCTIONS_END))
    else {
        return BlockRefresh::Current;
    };
    let end = end + INSTRUCTIONS_END.len();
    if end <= start {
        return BlockRefresh::Current;
    }
    let block = &text[start..end];
    // Written with the block's own line endings, so a CRLF file (common on Windows) stays CRLF.
    let current = with_line_endings_of(block, instruction_block(format).trim_end());
    if block == current {
        return BlockRefresh::Current;
    }
    let body = block
        .split_once('\n')
        .and_then(|(_, rest)| rest.strip_suffix(INSTRUCTIONS_END))
        .map(str::trim);
    if !body.is_some_and(is_aise_instruction_line) {
        return BlockRefresh::Edited;
    }
    BlockRefresh::Replace(format!("{}{current}{}", &text[..start], &text[end..]))
}

/// AI_SESSION_SEARCH.md, which CLAUDE.md imports, brought up to this build if aise wrote its text.
fn refreshed_instruction_file(original: &str) -> BlockRefresh {
    if is_current_instruction_file(original) {
        return BlockRefresh::Current;
    }
    let text = &original.replace("\r\n", "\n");
    let body = if text.trim_end() == legacy_instruction_file_content().trim_end() {
        Some(LEGACY_INSTRUCTIONS_LINE)
    } else {
        text.trim_end()
            .strip_prefix("# AI Session Search (`aise`)\n\n")
            .and_then(|managed| managed.strip_prefix(INSTRUCTIONS_FILE_START))
            .and_then(|managed| managed.strip_suffix(INSTRUCTIONS_FILE_END))
            .map(str::trim)
    };
    if body.is_some_and(is_aise_instruction_line) {
        BlockRefresh::Replace(with_line_endings_of(original, &instruction_file_content()))
    } else {
        BlockRefresh::Edited
    }
}

/// `text`, which uses `\n`, with CRLF line endings when `sample` uses them.
fn with_line_endings_of(sample: &str, text: &str) -> String {
    if sample.contains("\r\n") {
        text.replace('\n', "\r\n")
    } else {
        text.to_string()
    }
}

/// Whether `version` is newer than the version that last refreshed the installed integrations.
///
/// Only a newer version refreshes. Two installs of different versions, such as an old Cargo build
/// on PATH beside a newer uv install that MCP clients launch, must not rewrite the skill back and
/// forth, and the older one must never downgrade it.
fn is_newer_than_refreshed(receipt_path: &Path, version: &str) -> bool {
    let marker = crate::skill_manifest::refreshed_version_path(receipt_path);
    let Ok(recorded) = fs::read_to_string(marker) else {
        return true;
    };
    let recorded = recorded.trim();
    match (
        semver::Version::parse(version),
        semver::Version::parse(recorded),
    ) {
        (Ok(running), Ok(recorded)) => running > recorded,
        _ => version != recorded,
    }
}

/// Record that `version` checked every installed integration, unless a newer one already did.
fn record_refreshed_version(receipt_path: &Path, version: &str) {
    if is_newer_than_refreshed(receipt_path, version) {
        let marker = crate::skill_manifest::refreshed_version_path(receipt_path);
        let _ = fs::write(marker, format!("{version}\n"));
    }
}

/// Run [`refresh_integrations_after_upgrade`] for a CLI or MCP command and report it on stderr.
///
/// Stdout carries command output and the MCP protocol, so every line goes to stderr, and nothing
/// is printed when nothing changed. `[integrations] refresh_after_upgrade = false` turns it off.
pub(crate) fn refresh_integrations_after_upgrade_and_report(
    config: &crate::config::Config,
    config_path: &Path,
) {
    if !config.integrations.refresh_after_upgrade {
        return;
    }
    let receipt = default_transaction_receipt(config_path);
    let version = env!("CARGO_PKG_VERSION");
    match refresh_integrations_after_upgrade(&receipt, &config.integrations) {
        Ok(UpgradeRefresh::NotNeeded | UpgradeRefresh::Deferred) => {}
        Ok(UpgradeRefresh::Checked {
            skills,
            instructions,
            mcp,
        }) => {
            for path in mcp {
                eprintln!(
                    "aise: moved the aise MCP server entry in {} to where its client reads it, \
                     for {version}",
                    path.display()
                );
            }
            for outcome in skills {
                if let Some(problem) = outcome.problem {
                    eprintln!(
                        "aise: left the skill at {} unchanged after upgrading to {version}: {problem}",
                        outcome.root
                    );
                } else if outcome.changed {
                    // A skill rewrite is the common case and gets the plain sentence; other
                    // changes, such as removing a retired package, say what they did.
                    if outcome.action.starts_with("rewritten") {
                        eprintln!(
                            "aise: updated the installed skill at {} to {version}",
                            outcome.root
                        );
                    } else {
                        eprintln!("aise: {} at {} for {version}", outcome.action, outcome.root);
                    }
                }
            }
            for refresh in instructions {
                match refresh {
                    InstructionRefresh::Updated(path) => eprintln!(
                        "aise: updated the aise instructions in {} to {version}",
                        path.display()
                    ),
                    InstructionRefresh::Edited(path) => eprintln!(
                        "aise: left the aise instructions in {} unchanged after upgrading to \
                         {version}: they differ from any text aise wrote, so replacing them could \
                         destroy an edit you meant to keep.\nInspect them, or run `aise \
                         integrations install --no-skill` to replace them with this version's",
                        path.display()
                    ),
                }
            }
        }
        Err(error) => eprintln!(
            "aise: could not update the installed integrations for {version}: {error:#}\nEvery \
             command retries until this succeeds; `aise skills update` retries now and explains \
             any repair, and `aise integrations status` shows their state. To stop retrying, set `[integrations] \
             refresh_after_upgrade = false` in {}",
            config_path.display()
        ),
    }
}

fn manifest_recorded_skill_roots(
    manifest: &crate::skill_manifest::ManifestState,
) -> Result<Vec<PathBuf>> {
    match manifest {
        crate::skill_manifest::ManifestState::Absent => Ok(Vec::new()),
        crate::skill_manifest::ManifestState::Unreadable(problem) => {
            bail!("cannot enumerate recorded skill roots: {problem}")
        }
        crate::skill_manifest::ManifestState::Loaded(manifest) => manifest
            .installations
            .iter()
            .map(|installation| {
                installation
                    .root
                    .to_path_buf()
                    .context("decode a skill root from the install manifest")
            })
            .collect(),
    }
}

/// What removing one skill directory would do, and what it refuses to touch.
#[derive(Debug, Default)]
struct SkillRemovalPlan {
    mutations: Vec<PlannedFileMutation>,
    /// Reasons the whole tree is being preserved. Non-empty means nothing is removed.
    preserved: Vec<String>,
    /// Directories to try pruning afterwards, deepest first. Only ones this removal emptied.
    prune: Vec<PathBuf>,
}

/// Every file under `root`, relative to it, as slash-separated strings.
///
/// Used to find entries `aise` did not write. A skill directory is small and flat by design, so a
/// full walk is cheap and, unlike a shallow read, actually notices a file dropped into
/// `references/`.
fn skill_tree_entries(root: &Path) -> Result<Vec<String>> {
    fn walk(base: &Path, dir: &Path, found: &mut Vec<String>) -> Result<()> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read skill directory {}", dir.display()))
            }
        };
        for entry in entries {
            let entry = entry
                .with_context(|| format!("failed to list skill directory {}", dir.display()))?;
            let path = entry.path();
            // `symlink_metadata`, not `metadata`: a symlink to a directory must be recorded as an
            // entry rather than followed out of the tree.
            let meta = std::fs::symlink_metadata(&path)
                .with_context(|| format!("failed to inspect {}", path.display()))?;
            if meta.is_dir() {
                walk(base, &path, found)?;
            } else if let Ok(relative) = path.strip_prefix(base) {
                found.push(
                    relative
                        .components()
                        .map(|component| component.as_os_str().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join("/"),
                );
            }
        }
        Ok(())
    }
    let mut found = Vec::new();
    walk(root, root, &mut found)?;
    found.sort();
    Ok(found)
}

/// Directories under a skill root that this removal may have emptied, deepest first.
///
/// Derived from the package file list rather than by walking, so pruning can only ever consider
/// directories `aise` itself created. Nothing recursive: each is removed only if already empty.
fn managed_skill_directories(target: &SkillTarget) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = target
        .package
        .files
        .iter()
        .filter_map(|file| Path::new(file.relative_path).parent())
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(|parent| target.root.join(parent))
        .collect();
    dirs.sort();
    dirs.dedup();
    // Deepest first, then the root itself last.
    dirs.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    dirs.push(target.root.clone());
    dirs
}

fn prune_retired_skill_directories(roots: &[PathBuf]) {
    for root in roots {
        let target = SkillTarget {
            label: "retired corrections",
            root: root.clone(),
            package: &RETIRED_SKILL_PACKAGE,
            discovery_links: Vec::new(),
            retired_discovery_links: Vec::new(),
            discovery_clients: Vec::new(),
            detect_paths: Vec::new(),
            detect_binaries: Vec::new(),
        };
        prune_emptied_skill_directories(&managed_skill_directories(&target));
    }
}

/// Exact directories that may become empty after removing an already-enumerated skill tree.
///
/// Used only by explicit force cleanup. It derives ancestors from relative file entries that were
/// read under the exact authorized root; it never walks or removes a directory recursively.
fn skill_tree_directories(root: &Path, entries: &[String]) -> Vec<PathBuf> {
    let mut directories = std::collections::BTreeSet::new();
    for entry in entries {
        let mut parent = Path::new(entry).parent();
        while let Some(relative) = parent {
            if relative.as_os_str().is_empty() {
                break;
            }
            directories.insert(root.join(relative));
            parent = relative.parent();
        }
    }
    let mut directories = directories.into_iter().collect::<Vec<_>>();
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    directories.push(root.to_path_buf());
    directories
}

/// Plan the removal of one managed skill directory.
///
/// Normal uninstall removes a tree ONLY when every managed file is exactly what install recorded
/// and nothing else lives in the directory. Any other state preserves the whole directory and
/// says why. Removing an unchanged `SKILL.md` while leaving a user's edited policy would orphan
/// their data behind a skill that no longer declares itself, which is worse than leaving both.
///
/// `force` is the explicitly destructive path: it removes every entry under the root, including
/// files `aise` never wrote. Its caller is responsible for the ownership and exact-root checks.
fn plan_remove_skill_file(
    target: &SkillTarget,
    manifest: &crate::skill_manifest::ManifestState,
    force: bool,
) -> Result<SkillRemovalPlan> {
    let anchor = target.anchor();
    let anchor_text = read_optional_utf8_regular_file(&anchor)?;
    let entries = skill_tree_entries(&target.root)?;
    if anchor_text.is_none() && entries.is_empty() {
        return Ok(SkillRemovalPlan::default());
    }

    let owned_by_marker = anchor_text.as_deref().is_some_and(is_managed_skill_anchor);
    let recorded = manifest.installation(&target.root);
    if !owned_by_marker && recorded.is_none() {
        bail!(
            "refusing to remove unmanaged AI Session Search skill {}; delete it manually if \
             you're sure, or pass --keep-skill to leave it untouched",
            anchor.display()
        );
    }

    let mut reasons = Vec::new();
    for file in target.package.files {
        let path = target.root.join(file.relative_path);
        match classify_managed_file(
            &path,
            file.content,
            recorded.and_then(|installation| installation.file(file.relative_path)),
        )? {
            ManagedFileState::Current | ManagedFileState::OutdatedUntouched => {}
            // `Missing` is fine: there is simply nothing to remove for that entry.
            ManagedFileState::Missing => {}
            ManagedFileState::ModifiedOrDamaged => reasons.push(format!(
                "{} differs from what install recorded (modified or damaged)",
                path.display()
            )),
            ManagedFileState::LegacyUnknown => reasons.push(format!(
                "{} is not the version this build ships and there is no install record, so \
                 whether you changed it cannot be determined",
                path.display()
            )),
        }
    }

    // Files the manifest records that this build no longer ships, as when a package is retired.
    // They are owned — aise wrote them — but the loop above cannot vet them, because there is no
    // embedded copy to compare against. Comparing to the recorded digest is what keeps an edit from
    // being deleted as though it were untouched.
    let shipped: std::collections::BTreeSet<&str> = target
        .package
        .files
        .iter()
        .map(|file| file.relative_path)
        .collect();
    let recorded_only = recorded
        .into_iter()
        .flat_map(|installation| installation.files.iter())
        .filter(|file| !shipped.contains(file.relative_path.as_str()));
    let mut recorded_only_paths = std::collections::BTreeSet::new();
    for file in recorded_only {
        recorded_only_paths.insert(file.relative_path.as_str());
        let path = target.root.join(&file.relative_path);
        let Some(text) = read_optional_utf8_regular_file(&path)? else {
            continue;
        };
        if file.sha256 != crate::hashing::sha256(text.as_bytes()) {
            reasons.push(format!(
                "{} differs from what install recorded (modified or damaged)",
                path.display()
            ));
        }
    }

    // What this build ships, plus whatever the manifest records for this root. The manifest is the
    // ownership authority, so a file aise installed under an earlier layout stays owned even after
    // it leaves the current package list; judging by the current list alone would report a file
    // aise itself wrote as "not written by aise" and block its own cleanup.
    let managed: std::collections::BTreeSet<&str> = shipped
        .iter()
        .copied()
        .chain(recorded_only_paths.iter().copied())
        .collect();
    let unmanaged: Vec<&String> = entries
        .iter()
        .filter(|entry| !managed.contains(entry.as_str()))
        .collect();
    for entry in &unmanaged {
        reasons.push(format!(
            "{} was not written by aise",
            target.root.join(entry).display()
        ));
    }

    if !reasons.is_empty() && !force {
        return Ok(SkillRemovalPlan {
            preserved: reasons,
            ..Default::default()
        });
    }

    // Managed files always; every other entry only under force.
    let removable: Vec<String> = if force {
        entries
    } else {
        entries
            .into_iter()
            .filter(|entry| managed.contains(entry.as_str()))
            .collect()
    };
    let prune = if force {
        skill_tree_directories(&target.root, &removable)
    } else {
        managed_skill_directories(target)
    };
    let mut mutations = Vec::with_capacity(removable.len());
    for entry in removable {
        let path = target.root.join(&entry);
        if let Some(original) = read_optional_utf8_regular_file(&path)? {
            mutations.push(PlannedFileMutation::Remove { path, original });
        } else if force {
            // A non-UTF-8 or non-regular entry cannot go through the text transaction. Refuse
            // rather than silently leaving it behind while claiming a full cleanup.
            bail!(
                "cannot remove {}: it is not a regular UTF-8 text file, so this transaction \
                 cannot record it for rollback. Delete it manually, then re-run",
                path.display()
            );
        }
    }
    Ok(SkillRemovalPlan {
        mutations,
        preserved: Vec::new(),
        prune,
    })
}

/// Remove directories this uninstall emptied, best effort.
///
/// Deliberately NOT recursive and deliberately not an error: `remove_dir` refuses a non-empty
/// directory, which is exactly the guard wanted here. A leftover empty directory is untidy; a
/// recursive delete that raced a user creating a file in it is data loss.
fn prune_emptied_skill_directories(directories: &[PathBuf]) {
    for directory in directories {
        let _ = std::fs::remove_dir(directory);
    }
}

/// One managed file's state, relative to both the embedded bytes and the install record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ManagedFileState {
    /// Byte-identical to what this build embeds.
    Current,
    /// Not current, but exactly the bytes recorded at install: an older aise, untouched.
    OutdatedUntouched,
    /// Not current, and there is no install record to compare against, so whether anyone edited
    /// it is unknowable. Never reported as either "untouched" or "modified".
    LegacyUnknown,
    /// The file aise wrote is gone.
    Missing,
    /// Differs from what install recorded. Intent is unknowable, so this never claims "edited".
    ModifiedOrDamaged,
}

impl ManagedFileState {
    /// The word `status` prints for a whole directory in this state.
    const fn label(self) -> &'static str {
        match self {
            Self::Current => "configured",
            Self::OutdatedUntouched => "outdated, untouched",
            Self::LegacyUnknown => "legacy, ownership uncertain",
            Self::Missing => "missing",
            Self::ModifiedOrDamaged => "modified or damaged",
        }
    }
}

/// Classify one managed file against the embedded bytes and the install record.
///
/// The record is what makes "outdated" and "modified" separable. Byte comparison alone answers
/// only "does this equal the current build", and a NO there covers both an older install nobody
/// touched and a file somebody rewrote -- reporting one as the other tells the user something
/// false about their own filesystem (finding S13).
fn classify_managed_file(
    path: &Path,
    embedded: &str,
    recorded: Option<&crate::skill_manifest::InstalledSkillFile>,
) -> Result<ManagedFileState> {
    let Some(text) = read_optional_utf8_regular_file(path)? else {
        return Ok(ManagedFileState::Missing);
    };
    if text == embedded {
        return Ok(ManagedFileState::Current);
    }
    Ok(match recorded {
        None => ManagedFileState::LegacyUnknown,
        Some(entry) if entry.sha256 == crate::hashing::sha256(text.as_bytes()) => {
            ManagedFileState::OutdatedUntouched
        }
        Some(_) => ManagedFileState::ModifiedOrDamaged,
    })
}

/// Report one skill directory's state, considering EVERY managed file.
///
/// A directory whose `SKILL.md` is current but whose `corrections/policy.toml` is missing is not
/// `configured`: reporting the anchor alone would call a half-installed skill healthy, which is
/// what an interrupted upgrade leaves behind.
///
/// The whole directory takes its most alarming file's state, ordered by how much a user needs to
/// know: `modified or damaged` outranks `missing`, which outranks `legacy`, which outranks
/// `outdated`. Summarizing to the mildest state would hide the one file that needs attention.
fn status_skill_file(
    target: &SkillTarget,
    manifest: &crate::skill_manifest::ManifestState,
) -> Result<String> {
    let anchor_text = read_optional_utf8_regular_file(&target.anchor())?;
    // Present but unowned: whatever else is in the directory, `aise` will not touch it.
    if anchor_text
        .as_deref()
        .is_some_and(|text| !is_managed_skill_anchor(text))
    {
        return Ok("modified or unmanaged".to_string());
    }

    let recorded = manifest.installation(&target.root);
    let mut states = Vec::with_capacity(target.package.files.len());
    for file in target.package.files {
        states.push(classify_managed_file(
            &target.root.join(file.relative_path),
            file.content,
            recorded.and_then(|installation| installation.file(file.relative_path)),
        )?);
    }

    let absent = states
        .iter()
        .filter(|state| **state == ManagedFileState::Missing)
        .count();
    if absent == states.len() {
        return Ok(ManagedFileState::Missing.label().to_string());
    }
    // Some files gone is a different fact from every file gone, and `max()` alone gave both the
    // same word: an install one reference file short of a newer build reported `missing` while
    // four of five files were byte-current, which reads as "nothing is installed" about a working
    // skill. The count is the actionable part, and the repair rewrites only what is absent.
    if absent > 0 {
        let remaining = states
            .iter()
            .filter(|state| **state != ManagedFileState::Missing)
            .copied()
            .max()
            .unwrap_or(ManagedFileState::Current);
        let mut label = format!(
            "incomplete: {absent} of {} managed files missing; `aise integrations install` \
             restores them",
            states.len()
        );
        // A package can be short some files and hold damaged ones at the same time. Reporting only
        // the count would drop the second fact, which the repair does not address.
        if remaining != ManagedFileState::Current {
            label.push_str(&format!("; remaining files {}", remaining.label()));
        }
        return Ok(label);
    }
    let worst = states
        .into_iter()
        .max()
        .unwrap_or(ManagedFileState::Missing);
    Ok(worst.label().to_string())
}

/// Where a JSON client reads its servers, and containers aise wrote to before that the client
/// no longer reads. Install moves aise's entry out of a superseded container, uninstall clears
/// both, and status reports an entry left there as stale, the same way old server names are
/// handled. Add a container here when a client moves its servers. `syntax` is what the client's
/// own parser accepts, so aise can edit any file the client can read.
struct JsonServerLayout {
    container: &'static [&'static str],
    superseded: &'static [&'static [&'static str]],
    syntax: JsonSyntax,
}

/// JSONC: comments and trailing commas (VS Code, Zed, OpenCode, Kilo, Claude, Gemini).
/// JSON5: also unquoted keys, single quotes and the other JSON5 forms (OpenClaw).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonSyntax {
    /// Strict JSON, as read by Pi's JSON.parse.
    Json,
    Jsonc,
    Json5,
}

fn json_server_layout(format: ConfigFormat) -> Option<JsonServerLayout> {
    use JsonSyntax::{Json, Json5, Jsonc};
    let (container, superseded, syntax): (
        &'static [&'static str],
        &'static [&'static [&'static str]],
        JsonSyntax,
    ) = match format {
        ConfigFormat::JsonMcpServers => (&["mcpServers"], &[], Jsonc),
        ConfigFormat::Pi => (&["mcpServers"], &[], Json),
        ConfigFormat::PrimeAgent => (&["mcpServers"], &[], Json),
        ConfigFormat::Cursor => (&["mcpServers"], &[], Jsonc),
        ConfigFormat::VscodeServers => (&["servers"], &[], Jsonc),
        ConfigFormat::ZedContextServers => (&["context_servers"], &[], Jsonc),
        // OpenCode 2 reads `mcp.servers` and ignores servers directly under `mcp`.
        ConfigFormat::OpenCode => (&["mcp", "servers"], &[&["mcp"]], Jsonc),
        ConfigFormat::Kilo => (&["mcp"], &[], Jsonc),
        // OpenClaw reads `mcp.servers` from JSON5; earlier aise wrote a top-level `mcpServers`.
        ConfigFormat::OpenClaw => (&["mcp", "servers"], &[&["mcpServers"]], Json5),
        ConfigFormat::CodexToml => return None,
    };
    Some(JsonServerLayout {
        container,
        superseded,
        syntax,
    })
}

fn json_server_entry(format: ConfigFormat, binary: &str) -> Value {
    match format {
        ConfigFormat::VscodeServers | ConfigFormat::PrimeAgent | ConfigFormat::Cursor => {
            json!({"type": "stdio", "command": binary, "args": ["mcp", "serve"]})
        }
        ConfigFormat::OpenCode | ConfigFormat::Kilo => {
            json!({"type": "local", "command": [binary, "mcp", "serve"]})
        }
        ConfigFormat::JsonMcpServers
        | ConfigFormat::Pi
        | ConfigFormat::ZedContextServers
        | ConfigFormat::OpenClaw
        | ConfigFormat::CodexToml => {
            json!({"command": binary, "args": ["mcp", "serve"]})
        }
    }
}

/// The same client configuration at one of `target`'s superseded paths.
fn superseded_targets(target: &Target) -> impl Iterator<Item = Target> + '_ {
    target.superseded_paths.iter().map(|path| Target {
        path: path.clone(),
        superseded_paths: Vec::new(),
        ..target.clone()
    })
}

fn plan_remove_superseded(target: &Target) -> Result<Vec<PlannedFileMutation>> {
    let mut mutations = Vec::new();
    for old in superseded_targets(target) {
        mutations.extend(plan_remove_target_file(&old)?);
    }
    Ok(mutations)
}

fn plan_upsert_target(target: &Target, binary: &Path) -> Result<Vec<PlannedFileMutation>> {
    let binary = binary_config_value(binary)?;
    let mut mutations = match target.format {
        ConfigFormat::CodexToml => {
            plan_upsert_codex_mcp_server(&target.path, binary, target.codex_marketplace.as_deref())?
        }
        format => plan_upsert_json_server(&target.path, format, json_server_entry(format, binary))?,
    };
    mutations.extend(plan_remove_superseded(target)?);
    Ok(mutations)
}

#[cfg(test)]
fn remove_target(target: &Target) -> Result<bool> {
    let mutations = normalize_planned_mutations(plan_remove_target(target)?)?;
    let changed = !mutations.is_empty();
    publish_planned_mutations(&mutations)?;
    Ok(changed)
}

fn plan_remove_target(target: &Target) -> Result<Vec<PlannedFileMutation>> {
    let mut mutations = plan_remove_target_file(target)?;
    mutations.extend(plan_remove_superseded(target)?);
    Ok(mutations)
}

fn plan_remove_target_file(target: &Target) -> Result<Vec<PlannedFileMutation>> {
    match target.format {
        ConfigFormat::CodexToml => {
            plan_remove_codex_mcp_server(&target.path, target.codex_marketplace.as_deref())
        }
        format => plan_remove_json_server(&target.path, format),
    }
}

fn status_target(target: &Target) -> Result<&'static str> {
    let status = status_target_file(target)?;
    for old in superseded_targets(target) {
        if matches!(status_target_file(&old)?, "configured" | "stale") {
            return Ok("stale");
        }
    }
    Ok(status)
}

fn status_target_file(target: &Target) -> Result<&'static str> {
    match target.format {
        ConfigFormat::CodexToml => {
            let status = status_codex_mcp_server(&target.path)?;
            Ok(match target.codex_marketplace.as_deref() {
                Some(root)
                    if status == "configured"
                        && !codex_marketplace_is_current(&target.path, root) =>
                {
                    "stale"
                }
                _ => status,
            })
        }
        format => status_json_server(&target.path, format),
    }
}

#[cfg(test)]
fn upsert_json_mcp_server(path: &Path, entry: Value) -> Result<()> {
    let mutations = normalize_planned_mutations(plan_upsert_json_server(
        path,
        ConfigFormat::JsonMcpServers,
        entry,
    )?)?;
    publish_planned_mutations(&mutations)
}

#[cfg(test)]
fn remove_json_mcp_server(path: &Path) -> Result<bool> {
    let mutations =
        normalize_planned_mutations(plan_remove_json_server(path, ConfigFormat::JsonMcpServers)?)?;
    let changed = !mutations.is_empty();
    publish_planned_mutations(&mutations)?;
    Ok(changed)
}

fn json_layout_for(format: ConfigFormat) -> Result<JsonServerLayout> {
    json_server_layout(format)
        .ok_or_else(|| anyhow!("{format:?} is not a JSON client configuration format"))
}

/// The syntax a client's own parser accepts. JSONC clients reject the other JSON5 forms, so aise
/// does too rather than write a file the client cannot read; jsonc-parser's defaults are JSON5.
fn jsonc_parse_options(syntax: JsonSyntax) -> jsonc_parser::ParseOptions {
    match syntax {
        JsonSyntax::Json5 => jsonc_parser::ParseOptions::default(),
        JsonSyntax::Json | JsonSyntax::Jsonc => jsonc_parser::ParseOptions {
            allow_comments: syntax == JsonSyntax::Jsonc,
            allow_trailing_commas: syntax == JsonSyntax::Jsonc,
            allow_loose_object_property_names: false,
            allow_missing_commas: false,
            allow_single_quoted_strings: false,
            allow_hexadecimal_numbers: false,
            allow_unary_plus_numbers: false,
            allow_bare_decimal_point_numbers: false,
            allow_non_finite_numbers: false,
            allow_extended_string_escapes: false,
        },
    }
}

fn json_container<'a>(
    root: &'a Map<String, Value>,
    keys: &[&str],
) -> Option<&'a Map<String, Value>> {
    keys.iter()
        .try_fold(root, |current, key| current.get(*key)?.as_object())
}

/// A syntax tree of the file that keeps its comments, whitespace and trailing commas, so an
/// edit changes only aise's entries. `parse_json_object_or_empty` has already rejected invalid
/// text and a non-object top level, so the root here is an object or empty.
fn jsonc_document(
    path: &Path,
    text: Option<&str>,
    syntax: JsonSyntax,
) -> Result<jsonc_parser::cst::CstRootNode> {
    let text = text
        .filter(|text| !text.trim().is_empty())
        .unwrap_or("{}\n");
    jsonc_parser::cst::CstRootNode::parse(text, &jsonc_parse_options(syntax))
        .with_context(|| format!("failed to parse JSON in {}", path.display()))
}

/// The object at `keys` under `root`, or `None` when a key is absent. With `create`, absent
/// keys are inserted as empty objects. A present non-object value is an error naming the key,
/// since overwriting it would destroy user configuration.
fn jsonc_container(
    path: &Path,
    root: &jsonc_parser::cst::CstObject,
    keys: &[&str],
    create: bool,
) -> Result<Option<jsonc_parser::cst::CstObject>> {
    let mut current = root.clone();
    for key in keys {
        let next = match current.get(key) {
            None if !create => return Ok(None),
            None => current.object_value_or_create(key),
            Some(_) => current.object_value(key),
        };
        current = next.ok_or_else(|| {
            let dotted = keys.join(".");
            anyhow!(
                "{} has a non-object `{key}` value on the path `{dotted}`; fix the JSON so \
                 `{key}` is `{{}}`-shaped, or back up and move the file aside before rerunning \
                 `aise integrations install`",
                path.display()
            )
        })?;
    }
    Ok(Some(current))
}

fn remove_owned_jsonc_entries(servers: &jsonc_parser::cst::CstObject) -> bool {
    let mut removed = false;
    for name in std::iter::once(SERVER_NAME).chain(LEGACY_SERVER_NAMES) {
        if let Some(property) = servers.get(name) {
            property.remove();
            removed = true;
        }
    }
    removed
}

/// Removes aise's entries from every superseded container. Errors only when a superseded key
/// holds a non-object, which the user must resolve.
fn clear_superseded_jsonc_servers(
    path: &Path,
    root: &jsonc_parser::cst::CstObject,
    layout: &JsonServerLayout,
) -> Result<bool> {
    let mut removed = false;
    for keys in layout.superseded {
        if let Some(servers) = jsonc_container(path, root, keys, false)? {
            removed |= remove_owned_jsonc_entries(&servers);
        }
    }
    Ok(removed)
}

fn jsonc_input(value: &Value) -> jsonc_parser::cst::CstInputValue {
    use jsonc_parser::cst::CstInputValue;
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(value) => CstInputValue::Bool(*value),
        Value::Number(value) => CstInputValue::Number(value.to_string()),
        Value::String(value) => CstInputValue::String(value.clone()),
        Value::Array(items) => CstInputValue::Array(items.iter().map(jsonc_input).collect()),
        Value::Object(map) => CstInputValue::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), jsonc_input(value)))
                .collect(),
        ),
    }
}

fn jsonc_content(root: &jsonc_parser::cst::CstRootNode) -> String {
    let mut content = root.to_string();
    if !content.ends_with('\n') {
        content.push('\n');
    }
    content
}

fn plan_upsert_json_server(
    path: &Path,
    format: ConfigFormat,
    entry: Value,
) -> Result<Vec<PlannedFileMutation>> {
    let layout = json_layout_for(format)?;
    let original = read_optional_utf8_regular_file(path)?;
    parse_json_object_or_empty(path, original.as_deref(), layout.syntax)?;
    let document = jsonc_document(path, original.as_deref(), layout.syntax)?;
    let root = document.object_value_or_set();
    clear_superseded_jsonc_servers(path, &root, &layout)?;
    let servers = jsonc_container(path, &root, layout.container, true)?
        .expect("create inserts every missing container");
    for legacy_name in LEGACY_SERVER_NAMES {
        if let Some(property) = servers.get(legacy_name) {
            property.remove();
        }
    }
    let value = jsonc_input(&entry);
    match servers.get(SERVER_NAME) {
        Some(property) => property.set_value(value),
        None => {
            servers.append(SERVER_NAME, value);
        }
    }
    Ok(vec![planned_write(
        path,
        &original,
        jsonc_content(&document),
    )])
}

fn plan_remove_json_server(path: &Path, format: ConfigFormat) -> Result<Vec<PlannedFileMutation>> {
    let layout = json_layout_for(format)?;
    let original = read_optional_utf8_regular_file(path)?;
    let Some(text) = original.as_deref() else {
        return Ok(Vec::new());
    };
    parse_json_object_or_empty(path, Some(text), layout.syntax)?;
    let document = jsonc_document(path, Some(text), layout.syntax)?;
    let root = document.object_value_or_set();
    let mut removed = jsonc_container(path, &root, layout.container, false)?
        .is_some_and(|servers| remove_owned_jsonc_entries(&servers));
    removed |= clear_superseded_jsonc_servers(path, &root, &layout)?;
    if removed {
        Ok(vec![planned_write(
            path,
            &original,
            jsonc_content(&document),
        )])
    } else {
        Ok(Vec::new())
    }
}

#[cfg(test)]
fn upsert_codex_mcp_server(path: &Path, binary: &Path) -> Result<()> {
    let binary = binary_config_value(binary)?;
    publish_planned_mutations(&normalize_planned_mutations(plan_upsert_codex_mcp_server(
        path, binary, None,
    )?)?)
}

fn plan_upsert_codex_mcp_server(
    path: &Path,
    binary: &str,
    marketplace: Option<&Path>,
) -> Result<Vec<PlannedFileMutation>> {
    let original = read_optional_utf8_regular_file(path)?;
    let text = original.as_deref().unwrap_or_default();
    let mut document = parse_codex_document(path, text)?;
    let servers = document
        .entry("mcp_servers")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .ok_or_else(|| {
            anyhow!(
                "{} has non-table mcp_servers; convert it to [mcp_servers] before installing aise",
                path.display()
            )
        })?;
    let mut server = toml_edit::Table::new();
    server.insert("command", toml_edit::value(binary));
    let mut args = toml_edit::Array::new();
    args.extend(["mcp", "serve"]);
    server.insert("args", toml_edit::value(args));
    for legacy_name in LEGACY_SERVER_NAMES {
        servers.remove(legacy_name);
    }
    servers.insert(SERVER_NAME, toml_edit::Item::Table(server));
    let mut mutations = Vec::new();
    if let Some(root) = marketplace {
        let source = root.to_str().ok_or_else(|| {
            anyhow!(
                "the aise Codex marketplace path {} is not valid UTF-8 and cannot be written to {}",
                root.display(),
                path.display()
            )
        })?;
        let marketplaces = document
            .entry("marketplaces")
            .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
            .as_table_mut()
            .ok_or_else(|| {
                anyhow!(
                    "{} has non-table marketplaces; convert it to [marketplaces] before installing aise",
                    path.display()
                )
            })?;
        // The same keys `codex plugin marketplace add <dir>` writes for a local marketplace.
        let mut entry = toml_edit::Table::new();
        entry.insert("source_type", toml_edit::value("local"));
        entry.insert("source", toml_edit::value(source));
        marketplaces.insert(SERVER_NAME, toml_edit::Item::Table(entry));
        for (file, content) in codex_marketplace_files(root, binary)? {
            let original = read_optional_utf8_regular_file(&file)?;
            mutations.push(planned_write(&file, &original, content));
        }
    }

    let content = document.to_string();
    parse_codex_document(path, &content).with_context(|| {
        format!(
            "refusing to publish generated Codex TOML for {}",
            path.display()
        )
    })?;
    mutations.insert(0, planned_write(path, &original, content));
    Ok(mutations)
}

#[cfg(test)]
fn remove_codex_mcp_server(path: &Path) -> Result<bool> {
    let mutations = normalize_planned_mutations(plan_remove_codex_mcp_server(path, None)?)?;
    let changed = !mutations.is_empty();
    publish_planned_mutations(&mutations)?;
    Ok(changed)
}

fn plan_remove_codex_mcp_server(
    path: &Path,
    marketplace: Option<&Path>,
) -> Result<Vec<PlannedFileMutation>> {
    let mut mutations = Vec::new();
    if let Some(root) = marketplace {
        for (file, _) in codex_marketplace_files(root, "")? {
            if let Some(original) = read_optional_utf8_regular_file(&file)? {
                mutations.push(PlannedFileMutation::Remove {
                    path: file,
                    original,
                });
            }
        }
    }
    let original = read_optional_utf8_regular_file(path)?;
    let Some(text) = original.as_deref() else {
        return Ok(mutations);
    };
    let mut document = parse_codex_document(path, text)?;
    let mut removed = document
        .get_mut("mcp_servers")
        .and_then(toml_edit::Item::as_table_mut)
        .is_some_and(remove_owned_toml_server_entries);
    if marketplace.is_some() {
        // The marketplace aise registered, and the install record Codex adds when the user
        // installs its plugin, which would otherwise point at a marketplace that no longer exists.
        let installed = format!("{SERVER_NAME}@{SERVER_NAME}");
        for (table, key) in [
            ("marketplaces", SERVER_NAME),
            ("plugins", installed.as_str()),
        ] {
            let Some(entries) = document
                .get_mut(table)
                .and_then(toml_edit::Item::as_table_mut)
            else {
                continue;
            };
            removed |= entries.remove(key).is_some();
            if entries.is_empty() {
                document.remove(table);
            }
        }
    }
    if removed {
        let content = document.to_string();
        parse_codex_document(path, &content).with_context(|| {
            format!(
                "refusing to publish generated Codex TOML for {}",
                path.display()
            )
        })?;
        mutations.insert(0, planned_write(path, &original, content));
    }
    Ok(mutations)
}

/// The developer name Codex shows for the plugin; Cargo.toml carries no `authors`.
const CODEX_PLUGIN_DEVELOPER: &str = "Andrew Hundt";

/// The three files of aise's Codex marketplace under `root`, with their content: the
/// marketplace listing, and an MCP-only plugin whose manifest names no skills, because the skill
/// already reaches Codex through `~/.agents/skills` and a plugin copy would list it twice. The
/// listing uses `AVAILABLE`, so the plugin appears for the user to install; the bare
/// `[mcp_servers]` entry already serves the tools under the same name, and Codex runs one server.
fn codex_marketplace_files(root: &Path, binary: &str) -> Result<Vec<(PathBuf, String)>> {
    let plugin = root.join("plugins").join(SERVER_NAME);
    let listing = json!({
        "name": SERVER_NAME,
        "interface": {"displayName": "AI Session Search"},
        "plugins": [{
            "name": SERVER_NAME,
            "source": {"source": "local", "path": format!("./plugins/{SERVER_NAME}")},
            "policy": {"installation": "AVAILABLE", "authentication": "ON_INSTALL"},
            "category": "Productivity",
        }],
    });
    let manifest = json!({
        "name": SERVER_NAME,
        "version": env!("CARGO_PKG_VERSION"),
        "description": env!("CARGO_PKG_DESCRIPTION"),
        "author": {"name": CODEX_PLUGIN_DEVELOPER},
        "homepage": env!("CARGO_PKG_HOMEPAGE"),
        "repository": env!("CARGO_PKG_REPOSITORY"),
        "license": env!("CARGO_PKG_LICENSE"),
        "mcpServers": "./.mcp.json",
        "interface": {
            "displayName": "AI Session Search",
            "shortDescription": "Search your local AI coding sessions",
            "longDescription": env!("CARGO_PKG_DESCRIPTION"),
            "developerName": CODEX_PLUGIN_DEVELOPER,
            "category": "Productivity",
            "capabilities": ["Read"],
            "websiteURL": env!("CARGO_PKG_HOMEPAGE"),
        },
    });
    let servers = json!({"mcpServers": {SERVER_NAME: json_server_entry(
        ConfigFormat::JsonMcpServers,
        binary,
    )}});
    let pretty =
        |value: &Value| Ok::<_, anyhow::Error>(serde_json::to_string_pretty(value)? + "\n");
    Ok(vec![
        (
            root.join(".agents")
                .join("plugins")
                .join("marketplace.json"),
            pretty(&listing)?,
        ),
        (
            plugin.join(".codex-plugin").join("plugin.json"),
            pretty(&manifest)?,
        ),
        (plugin.join(".mcp.json"), pretty(&servers)?),
    ])
}

/// Whether `config` registers aise's marketplace at `root` and its files are aise's. The plugin
/// version and the executable are not compared, so neither an upgrade nor `--binary` alone makes
/// the plugin read stale; the listing and the manifest must match this build's otherwise.
fn codex_marketplace_is_current(config: &Path, root: &Path) -> bool {
    let registered = read_optional_utf8_regular_file(config)
        .ok()
        .flatten()
        .and_then(|text| text.parse::<toml_edit::DocumentMut>().ok())
        .is_some_and(|document| {
            let field = |key: &str| {
                document
                    .get("marketplaces")
                    .and_then(|marketplaces| marketplaces.get(SERVER_NAME))
                    .and_then(|entry| entry.get(key))
                    .and_then(toml_edit::Item::as_str)
                    .map(str::to_string)
            };
            field("source_type").as_deref() == Some("local")
                && field("source").as_deref() == root.to_str()
        });
    let Ok(expected) = codex_marketplace_files(root, "") else {
        return false;
    };
    let read_json = |path: &Path| {
        read_optional_utf8_regular_file(path)
            .ok()
            .flatten()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
    };
    let [(listing, listing_text), (manifest, manifest_text), (servers, _)] = expected.as_slice()
    else {
        return false;
    };
    let same_except = |path: &Path, text: &str, ignored: &str| {
        let (Some(mut found), Ok(mut wanted)) =
            (read_json(path), serde_json::from_str::<Value>(text))
        else {
            return false;
        };
        for value in [&mut found, &mut wanted] {
            if let Some(object) = value.as_object_mut() {
                object.remove(ignored);
            }
        }
        found == wanted
    };
    registered
        && same_except(listing, listing_text, "")
        && same_except(manifest, manifest_text, "version")
        && read_json(servers).is_some_and(|servers| {
            servers["mcpServers"]
                .get(SERVER_NAME)
                .is_some_and(|entry| json_entry_is_current(entry, ConfigFormat::JsonMcpServers))
        })
}

fn parse_codex_document(path: &Path, text: &str) -> Result<toml_edit::DocumentMut> {
    text.parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("failed to parse TOML in {}", path.display()))
}

fn remove_owned_toml_server_entries(servers: &mut toml_edit::Table) -> bool {
    let mut removed = servers.remove(SERVER_NAME).is_some();
    for legacy_name in LEGACY_SERVER_NAMES {
        removed |= servers.remove(legacy_name).is_some();
    }
    removed
}

fn binary_config_value(binary: &Path) -> Result<&str> {
    binary.to_str().ok_or_else(|| {
        anyhow!(
            "MCP executable path {} is not valid UTF-8 and cannot be represented in JSON or TOML \
             client configuration; install aise at a UTF-8 path or pass a different path with \
             --binary",
            binary.display()
        )
    })
}

fn parse_json_object_or_empty(
    path: &Path,
    text: Option<&str>,
    syntax: JsonSyntax,
) -> Result<Map<String, Value>> {
    let text = text.unwrap_or_default();
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match jsonc_parser::parse_to_serde_value::<Value>(text, &jsonc_parse_options(syntax))
        .with_context(|| format!("failed to parse JSON in {}", path.display()))?
    {
        Value::Object(map) => Ok(map),
        _ => Err(anyhow!(
            "{} must contain a JSON object at the top level, not an array or scalar; fix the \
             file's contents, or back up and move it aside before rerunning `aise integrations install`",
            path.display()
        )),
    }
}

fn json_array_is_strings(value: Option<&Value>, expected: &[&str]) -> bool {
    value.and_then(Value::as_array).is_some_and(|items| {
        items.len() == expected.len()
            && items
                .iter()
                .zip(expected)
                .all(|(item, expected)| item.as_str() == Some(*expected))
    })
}

fn json_entry_is_current(entry: &Value, format: ConfigFormat) -> bool {
    let Some(entry) = entry.as_object() else {
        return false;
    };
    match format {
        ConfigFormat::OpenCode | ConfigFormat::Kilo => {
            entry.get("type").and_then(Value::as_str) == Some("local")
                && entry
                    .get("command")
                    .and_then(Value::as_array)
                    .is_some_and(|command| {
                        command.len() == 3
                            && command[0].as_str().is_some_and(|value| !value.is_empty())
                            && command[1].as_str() == Some("mcp")
                            && command[2].as_str() == Some("serve")
                    })
        }
        ConfigFormat::JsonMcpServers
        | ConfigFormat::Pi
        | ConfigFormat::PrimeAgent
        | ConfigFormat::Cursor
        | ConfigFormat::VscodeServers
        | ConfigFormat::ZedContextServers
        | ConfigFormat::OpenClaw => {
            let command_is_set = entry
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|command| !command.is_empty());
            let type_is_valid = !matches!(
                format,
                ConfigFormat::VscodeServers | ConfigFormat::PrimeAgent | ConfigFormat::Cursor
            ) || entry.get("type").and_then(Value::as_str) == Some("stdio");
            command_is_set
                && type_is_valid
                && json_array_is_strings(entry.get("args"), &["mcp", "serve"])
        }
        ConfigFormat::CodexToml => false,
    }
}

fn status_json_server(path: &Path, format: ConfigFormat) -> Result<&'static str> {
    let layout = json_layout_for(format)?;
    let text = read_optional_utf8_regular_file(path)?;
    if text.is_none() {
        return Ok("missing");
    }
    let root = parse_json_object_or_empty(path, text.as_deref(), layout.syntax)?;
    let servers = json_container(&root, layout.container);
    let has_legacy_name = |servers: &Map<String, Value>| {
        LEGACY_SERVER_NAMES
            .iter()
            .any(|name| servers.contains_key(*name))
    };
    let left_in_superseded = layout.superseded.iter().any(|keys| {
        json_container(&root, keys)
            .is_some_and(|old| old.contains_key(SERVER_NAME) || has_legacy_name(old))
    });
    Ok(match servers.and_then(|servers| servers.get(SERVER_NAME)) {
        Some(entry) if json_entry_is_current(entry, format) && !left_in_superseded => "configured",
        Some(_) => "stale",
        None if left_in_superseded || servers.is_some_and(has_legacy_name) => "stale",
        None => "not configured",
    })
}

fn status_codex_mcp_server(path: &Path) -> Result<&'static str> {
    let text = read_optional_utf8_regular_file(path)?;
    let Some(text) = text else {
        return Ok("missing");
    };
    let root = parse_codex_document(path, &text)?;
    let servers = root.get("mcp_servers").and_then(toml_edit::Item::as_table);
    let entry = servers
        .and_then(|servers| servers.get(SERVER_NAME))
        .and_then(toml_edit::Item::as_table);
    Ok(match entry {
        Some(entry)
            if entry
                .get("command")
                .and_then(toml_edit::Item::as_value)
                .and_then(toml_edit::Value::as_str)
                .is_some_and(|command| !command.is_empty())
                && entry
                    .get("args")
                    .and_then(toml_edit::Item::as_value)
                    .and_then(toml_edit::Value::as_array)
                    .is_some_and(|items| {
                        items.len() == 2
                            && items.get(0).and_then(toml_edit::Value::as_str) == Some("mcp")
                            && items.get(1).and_then(toml_edit::Value::as_str) == Some("serve")
                    }) =>
        {
            "configured"
        }
        Some(_) => "stale",
        None if servers.is_some_and(|servers| {
            LEGACY_SERVER_NAMES
                .iter()
                .any(|name| servers.contains_key(name))
        }) =>
        {
            "stale"
        }
        None => "not configured",
    })
}

#[cfg(test)]
fn upsert_instruction_file(target: &InstructionTarget) -> Result<()> {
    publish_planned_mutations(&normalize_planned_mutations(plan_upsert_instruction_file(
        target,
    )?)?)
}

#[cfg(test)]
fn remove_instruction_file(target: &InstructionTarget) -> Result<bool> {
    let mutations = normalize_planned_mutations(plan_remove_instruction_file(target)?)?;
    let changed = !mutations.is_empty();
    publish_planned_mutations(&mutations)?;
    Ok(changed)
}

fn plan_upsert_instruction_file(target: &InstructionTarget) -> Result<Vec<PlannedFileMutation>> {
    match target.format {
        InstructionFormat::ClaudeImport => plan_upsert_claude_instruction_file(&target.path),
        InstructionFormat::InlineBlock | InstructionFormat::InlineCliBlock => {
            plan_upsert_inline_instruction_file(&target.path, target.format)
        }
    }
}

fn plan_remove_instruction_file(target: &InstructionTarget) -> Result<Vec<PlannedFileMutation>> {
    match target.format {
        InstructionFormat::ClaudeImport => plan_remove_claude_instruction_file(&target.path),
        InstructionFormat::InlineBlock | InstructionFormat::InlineCliBlock => {
            plan_remove_inline_instruction_file(&target.path)
        }
    }
}

fn status_instruction_file(target: &InstructionTarget) -> Result<&'static str> {
    match target.format {
        InstructionFormat::ClaudeImport => status_claude_instruction_file(&target.path),
        InstructionFormat::InlineBlock | InstructionFormat::InlineCliBlock => {
            status_inline_instruction_file(&target.path, target.format)
        }
    }
}

fn plan_upsert_claude_instruction_file(path: &Path) -> Result<Vec<PlannedFileMutation>> {
    let original = read_optional_utf8_regular_file(path)?;
    let next = upsert_claude_instruction_text(original.as_deref().unwrap_or_default())?;
    let mut mutations = vec![plan_write_aise_instruction_file(path)?];
    mutations.push(planned_write(path, &original, next));
    Ok(mutations)
}

fn plan_remove_claude_instruction_file(path: &Path) -> Result<Vec<PlannedFileMutation>> {
    let original = read_optional_utf8_regular_file(path)?;
    let mut mutations = Vec::new();
    if let Some(text) = original.as_deref() {
        if let Some(next) = remove_claude_instruction_text(text)? {
            mutations.push(planned_write(path, &original, next));
        }
    }
    if let Some(removal) = plan_remove_aise_instruction_file(path)? {
        mutations.push(removal);
    }
    Ok(mutations)
}

fn status_claude_instruction_file(path: &Path) -> Result<&'static str> {
    let text = read_optional_utf8_regular_file(path)?;
    let Some(text) = text else {
        return Ok("missing");
    };
    let instruction_file = aise_instruction_path(path);
    let instruction_text = read_optional_utf8_regular_file(&instruction_file)?;
    if !text.lines().any(is_instruction_reference_line) {
        return Ok(
            if instruction_text
                .as_deref()
                .is_some_and(is_managed_instruction_file)
            {
                "orphaned managed file"
            } else {
                "not configured"
            },
        );
    }
    Ok(match instruction_text.as_deref() {
        None => "instruction file missing",
        Some(content) if is_current_instruction_file(content) => "configured",
        Some(content) if is_managed_instruction_file(content) => "outdated",
        Some(_) => "instruction file modified",
    })
}

fn upsert_claude_instruction_text(text: &str) -> Result<String> {
    let without_legacy = remove_inline_instruction_block(text)?.unwrap_or_else(|| text.to_string());
    let without_existing_ref =
        remove_instruction_reference(&without_legacy)?.unwrap_or(without_legacy);
    let removed = without_existing_ref;
    let mut next = removed.trim_end().to_string();
    if !next.is_empty() {
        next.push_str("\n\n");
    }
    next.push_str(INSTRUCTIONS_REFERENCE);
    next.push('\n');
    Ok(next)
}

fn remove_claude_instruction_text(text: &str) -> Result<Option<String>> {
    let without_reference = remove_instruction_reference(text)?;
    let base = without_reference.as_deref().unwrap_or(text);
    let without_legacy = remove_inline_instruction_block(base)?;
    Ok(without_legacy.or(without_reference))
}

fn plan_upsert_inline_instruction_file(
    path: &Path,
    format: InstructionFormat,
) -> Result<Vec<PlannedFileMutation>> {
    let original = read_optional_utf8_regular_file(path)?;
    let next = upsert_inline_instruction_text(original.as_deref().unwrap_or_default(), format)?;
    let mut mutations = vec![planned_write(path, &original, next)];
    if let Some(removal) = plan_remove_aise_instruction_file(path)? {
        mutations.push(removal);
    }
    Ok(mutations)
}

fn plan_remove_inline_instruction_file(path: &Path) -> Result<Vec<PlannedFileMutation>> {
    let original = read_optional_utf8_regular_file(path)?;
    let Some(original_text) = original.as_deref() else {
        return Ok(Vec::new());
    };
    let removed_inline = remove_inline_instruction_block(original_text)?;
    let base = removed_inline.as_deref().unwrap_or(original_text);
    let removed_reference = remove_instruction_reference(base)?;
    let mut mutations = Vec::new();
    if let Some(next) = removed_reference.or(removed_inline) {
        mutations.push(planned_write(path, &original, next));
    }
    if let Some(removal) = plan_remove_aise_instruction_file(path)? {
        mutations.push(removal);
    }
    Ok(mutations)
}

fn status_inline_instruction_file(path: &Path, format: InstructionFormat) -> Result<&'static str> {
    let text = read_optional_utf8_regular_file(path)?;
    let Some(text) = text else {
        return Ok("missing");
    };
    remove_inline_instruction_block(&text)?;
    let starts = text.matches(INSTRUCTIONS_START).count();
    let ends = text.matches(INSTRUCTIONS_END).count();
    if starts == 0 && ends == 0 {
        return Ok("not configured");
    }
    let start = text
        .find(INSTRUCTIONS_START)
        .expect("validated managed block has a start marker");
    let end = start
        + text[start..]
            .find(INSTRUCTIONS_END)
            .expect("validated managed block has an end marker")
        + INSTRUCTIONS_END.len();
    Ok(
        if starts == 1
            && ends == 1
            && text[start..end].replace("\r\n", "\n").trim_end()
                == instruction_block(format).trim_end()
        {
            "configured"
        } else {
            "outdated"
        },
    )
}

fn upsert_inline_instruction_text(text: &str, format: InstructionFormat) -> Result<String> {
    let without_inline = remove_inline_instruction_block(text)?.unwrap_or_else(|| text.to_string());
    let removed = remove_instruction_reference(&without_inline)?.unwrap_or(without_inline);
    let mut next = removed.trim_end().to_string();
    if !next.is_empty() {
        next.push_str("\n\n");
    }
    next.push_str(&instruction_block(format));
    Ok(next)
}

fn remove_instruction_reference(text: &str) -> Result<Option<String>> {
    let mut removed = false;
    let mut lines = Vec::new();
    for line in text.lines() {
        if is_instruction_reference_line(line) {
            removed = true;
        } else {
            lines.push(line);
        }
    }
    if !removed {
        return Ok(None);
    }
    let mut next = lines.join("\n");
    while next.contains("\n\n\n") {
        next = next.replace("\n\n\n", "\n\n");
    }
    if !next.is_empty() {
        next.push('\n');
    }
    Ok(Some(next))
}

fn remove_inline_instruction_block(text: &str) -> Result<Option<String>> {
    let mut next = text.to_string();
    let mut removed = false;
    loop {
        let start = next.find(INSTRUCTIONS_START);
        let end = next.find(INSTRUCTIONS_END);
        let Some(start) = start else {
            if end.is_some() {
                return Err(anyhow!(
                    "found an `{INSTRUCTIONS_END}` marker with no matching `{INSTRUCTIONS_START}\
                     ...-->` before it; this file's aise-managed instruction block is corrupted, \
                     remove the unmatched end marker after reviewing the surrounding text, then \
                     rerun `aise integrations install`"
                ));
            }
            break;
        };
        if end.is_some_and(|end| end < start) {
            return Err(anyhow!(
                "found an `{INSTRUCTIONS_END}` marker before its matching \
                 `{INSTRUCTIONS_START}...-->`; this file's aise-managed instruction block is \
                 corrupted, remove the out-of-order end marker after reviewing the surrounding \
                 text, then rerun `aise integrations install`"
            ));
        }
        let end_relative = next[start..].find(INSTRUCTIONS_END).ok_or_else(|| {
            anyhow!(
                "found a `{INSTRUCTIONS_START}...-->` marker with no matching \
                 `{INSTRUCTIONS_END}`; this file's aise-managed instruction block is corrupted; \
                 restore the missing end marker, or remove the unmatched start marker and partial \
                 managed text after reviewing it, then rerun `aise integrations install`"
            )
        })?;
        let mut end = start + end_relative + INSTRUCTIONS_END.len();
        if next[end..].starts_with('\n') {
            end += 1;
        }
        let mut start = start;
        if end == next.len() && next[..start].ends_with("\n\n") {
            start -= 1;
        }
        next.replace_range(start..end, "");
        removed = true;
    }
    if !removed {
        return Ok(None);
    }
    while next.contains("\n\n\n") {
        next = next.replace("\n\n\n", "\n\n");
    }
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    Ok(Some(next))
}

fn is_instruction_reference_line(line: &str) -> bool {
    line.trim() == INSTRUCTIONS_REFERENCE
}

fn plan_write_aise_instruction_file(instruction_ref_path: &Path) -> Result<PlannedFileMutation> {
    let path = aise_instruction_path(instruction_ref_path);
    let original = read_optional_utf8_regular_file(&path)?;
    if original
        .as_deref()
        .is_some_and(|text| !is_managed_instruction_file(text))
    {
        return Err(anyhow!(
            "refusing to replace unmanaged instruction file {}; move it aside manually and \
             rerun `aise integrations install`, or pass --no-instructions to skip managing it",
            path.display()
        ));
    }
    Ok(planned_write(&path, &original, instruction_file_content()))
}

fn plan_remove_aise_instruction_file(
    instruction_ref_path: &Path,
) -> Result<Option<PlannedFileMutation>> {
    let path = aise_instruction_path(instruction_ref_path);
    let Some(text) = read_optional_utf8_regular_file(&path)? else {
        return Ok(None);
    };
    Ok(
        is_managed_instruction_file(&text).then_some(PlannedFileMutation::Remove {
            path,
            original: text,
        }),
    )
}

fn aise_instruction_path(instruction_ref_path: &Path) -> PathBuf {
    instruction_ref_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(INSTRUCTIONS_FILE)
}

fn instruction_file_content() -> String {
    format!(
        "# AI Session Search (`aise`)\n\n{INSTRUCTIONS_FILE_START}\n{INSTRUCTIONS_LINE}\n{INSTRUCTIONS_FILE_END}\n"
    )
}

pub(crate) fn agent_instructions() -> &'static str {
    INSTRUCTIONS_LINE
}

fn legacy_instruction_file_content() -> String {
    format!("# aise\n\n{LEGACY_INSTRUCTIONS_LINE}\n")
}

/// Line endings do not count: an editor or Git on Windows may have turned them into CRLF.
fn is_current_instruction_file(text: &str) -> bool {
    text.replace("\r\n", "\n").trim_end() == instruction_file_content().trim_end()
}

fn is_managed_instruction_file(text: &str) -> bool {
    let text = &text.replace("\r\n", "\n");
    if text.trim_end() == legacy_instruction_file_content().trim_end() {
        return true;
    }
    let trimmed = text.trim_end();
    let Some(managed) = trimmed.strip_prefix("# AI Session Search (`aise`)\n\n") else {
        return false;
    };
    managed.starts_with(INSTRUCTIONS_FILE_START) && managed.ends_with(INSTRUCTIONS_FILE_END)
}

fn instruction_block(format: InstructionFormat) -> String {
    format!(
        "<!-- aise-instructions v1 -->\n{}\n{INSTRUCTIONS_END}\n",
        format.line()
    )
}

fn resolve_mcp_binary(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return validate_mcp_binary(absolutize(&expand_tilde(path)?)?);
    }
    resolve_detected_mcp_binary(which("aise"))
}

fn resolve_detected_mcp_binary(detected: Option<PathBuf>) -> Result<PathBuf> {
    let path =
        detected.ok_or_else(|| anyhow!("aise is not on PATH; pass --binary /path/to/aise"))?;
    validate_mcp_binary(absolutize(&path)?)
}

fn validate_mcp_binary(path: PathBuf) -> Result<PathBuf> {
    let metadata = fs::metadata(&path).with_context(|| {
        format!(
            "MCP binary {} does not exist or is not accessible",
            path.display()
        )
    })?;
    if !metadata.is_file() {
        return Err(anyhow!(
            "MCP binary {} is not a regular file; pass the path to the `aise` executable \
             with --binary",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            let chmod_command = render_posix_shell_command(&[
                "chmod".to_string(),
                "+x".to_string(),
                path.to_string_lossy().into_owned(),
            ])?;
            return Err(anyhow!(
                "MCP binary {} is not executable (mode {:o}); run `{chmod_command}` or pass a \
                 different path with --binary",
                path.display(),
                metadata.permissions().mode() & 0o777
            ));
        }
    }
    Ok(path)
}

fn absolutize(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

/// Expand `~` for an install destination, refusing when no home directory exists.
///
/// One definition, shared with skill discovery and scaffolding, so the three places that decide
/// where a file lands cannot disagree about what `~/skills` means.
fn expand_tilde(path: &Path) -> Result<PathBuf> {
    crate::util::expand_tilde_required(path)
}

fn missing_home_error() -> anyhow::Error {
    anyhow!(
        "cannot determine the home directory for MCP client configuration; set HOME or USERPROFILE"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn json_array_is_strings_requires_exact_length_and_values() {
        assert!(json_array_is_strings(
            Some(&json!(["mcp", "serve"])),
            &["mcp", "serve"]
        ));
        // Wrong length, wrong value, non-string element, non-array, and absent all fail.
        assert!(!json_array_is_strings(
            Some(&json!(["mcp"])),
            &["mcp", "serve"]
        ));
        assert!(!json_array_is_strings(
            Some(&json!(["mcp", "other"])),
            &["mcp", "serve"]
        ));
        assert!(!json_array_is_strings(
            Some(&json!(["mcp", 7])),
            &["mcp", "serve"]
        ));
        assert!(!json_array_is_strings(Some(&json!("mcp serve")), &["mcp"]));
        assert!(!json_array_is_strings(None, &["mcp", "serve"]));
    }

    #[test]
    fn json_entry_is_current_matches_each_config_format_shape() {
        // JSON mcpServers: string command plus args ["mcp","serve"].
        let ok = json!({"command": "aise", "args": ["mcp", "serve"]});
        assert!(json_entry_is_current(&ok, ConfigFormat::JsonMcpServers));
        // Missing args or empty command are not current.
        assert!(!json_entry_is_current(
            &json!({"command": "aise"}),
            ConfigFormat::JsonMcpServers
        ));
        assert!(!json_entry_is_current(
            &json!({"command": "", "args": ["mcp", "serve"]}),
            ConfigFormat::JsonMcpServers
        ));
        // VS Code additionally requires type="stdio".
        assert!(!json_entry_is_current(&ok, ConfigFormat::VscodeServers));
        assert!(json_entry_is_current(
            &json!({"command": "aise", "type": "stdio", "args": ["mcp", "serve"]}),
            ConfigFormat::VscodeServers
        ));
        // OpenCode 2: type="local" and command=[bin,"mcp","serve"]; the V1 `enabled` shape is not.
        assert!(json_entry_is_current(
            &json!({"type": "local", "command": ["aise", "mcp", "serve"]}),
            ConfigFormat::OpenCode
        ));
        assert!(!json_entry_is_current(
            &json!({"enabled": true, "command": ["aise", "mcp", "serve"]}),
            ConfigFormat::OpenCode
        ));
        // CodexToml is never "current" via this JSON predicate, and a non-object is false.
        assert!(!json_entry_is_current(&ok, ConfigFormat::CodexToml));
        assert!(!json_entry_is_current(
            &json!("not an object"),
            ConfigFormat::JsonMcpServers
        ));
    }

    #[test]
    fn environment_layout_uses_harness_roots_on_every_platform() {
        let dir = tempdir().unwrap();
        let home = dir.path().join("home");
        let redirected = dir.path().join("work profile λ");
        let value = redirected.to_str().unwrap();
        for platform in [
            ClientPlatform::Macos,
            ClientPlatform::Linux,
            ClientPlatform::Windows,
        ] {
            for (variable, client, mcp, instruction, skill) in [
                (
                    "CLAUDE_CONFIG_DIR",
                    McpClient::Claude,
                    Some(".claude.json"),
                    Some("CLAUDE.md"),
                    Some("skills"),
                ),
                (
                    "CODEX_HOME",
                    McpClient::Codex,
                    Some("config.toml"),
                    Some("AGENTS.md"),
                    None,
                ),
                (
                    "GEMINI_CLI_HOME",
                    McpClient::Gemini,
                    Some(".gemini/settings.json"),
                    Some(".gemini/GEMINI.md"),
                    Some(".gemini/skills"),
                ),
                (
                    "PI_CODING_AGENT_DIR",
                    McpClient::Pi,
                    None,
                    Some("AGENTS.md"),
                    Some("skills"),
                ),
                (
                    "PRIME_AGENT_CODING_AGENT_DIR",
                    McpClient::PrimeAgent,
                    None,
                    Some("AGENTS.md"),
                    Some("skills"),
                ),
                (
                    "XDG_CONFIG_HOME",
                    McpClient::Opencode,
                    Some("opencode/opencode.json"),
                    Some("opencode/AGENTS.md"),
                    None,
                ),
                (
                    "XDG_CONFIG_HOME",
                    McpClient::Kilocode,
                    Some("kilo/kilo.jsonc"),
                    None,
                    None,
                ),
                (
                    "FLATPAK_XDG_CONFIG_HOME",
                    McpClient::Zed,
                    Some("zed/settings.json"),
                    None,
                    None,
                ),
                (
                    "OPENCLAW_CONFIG_PATH",
                    McpClient::Openclaw,
                    Some(""),
                    None,
                    None,
                ),
                (
                    "OPENCLAW_STATE_DIR",
                    McpClient::Openclaw,
                    Some("openclaw.json"),
                    None,
                    None,
                ),
                (
                    "OPENCLAW_HOME",
                    McpClient::Openclaw,
                    Some(".openclaw/openclaw.json"),
                    None,
                    None,
                ),
            ] {
                let layout = ClientLayout::with_environment(
                    home.clone(),
                    home.join("platform-config"),
                    platform,
                    &[(variable, value)],
                )
                .unwrap();
                let targets = targets_for_layout(client, &layout);
                if let Some(suffix) = mcp {
                    if variable != "FLATPAK_XDG_CONFIG_HOME" || platform == ClientPlatform::Linux {
                        let expected = if suffix.is_empty() {
                            redirected.clone()
                        } else {
                            redirected.join(suffix)
                        };
                        assert_eq!(targets[0].path, expected, "{platform:?} {variable}");
                        assert!(
                            targets[0]
                                .detect_paths
                                .contains(&expected.parent().unwrap().to_path_buf()),
                            "{variable}: {:?}",
                            targets[0].detect_paths
                        );
                    }
                }
                if let Some(suffix) = instruction {
                    let instructions = instruction_targets_for_layout(client, &layout);
                    assert_eq!(
                        instructions[0].path,
                        redirected.join(suffix),
                        "{platform:?} {variable}"
                    );
                    assert!(instructions[0]
                        .detect_paths
                        .contains(&instructions[0].path.parent().unwrap().to_path_buf()));
                }
                if let Some(suffix) = skill {
                    let skills = skill_targets_for_layout(client, &layout);
                    let link = redirected.join(suffix).join("ai-session-search");
                    assert!(
                        skills[0].discovery_links.contains(&link),
                        "{platform:?} {variable}"
                    );
                    assert!(all_standard_discovery_links(
                        &layout,
                        &AI_SESSION_SEARCH_SKILL_PACKAGE
                    )
                    .contains(&link));
                }
                if client == McpClient::Codex {
                    assert_eq!(
                        skill_targets_for_layout(client, &layout)[0].discovery_links,
                        [home.join(".agents/skills/ai-session-search")]
                    );
                    let mut targets = targets;
                    let receipt = dir.path().join("aise/receipt.json");
                    attach_codex_marketplace(&mut targets, &receipt);
                    assert_eq!(
                        targets[0].codex_marketplace,
                        Some(dir.path().join("aise/codex-marketplace"))
                    );
                }
                if client == McpClient::Gemini {
                    assert_eq!(
                        targets_for_layout(McpClient::Antigravity, &layout)[0].path,
                        home.join(".gemini/config/mcp_config.json")
                    );
                    assert_eq!(
                        instruction_targets_for_layout(McpClient::Antigravity, &layout)[0].path,
                        home.join(".gemini/GEMINI.md")
                    );
                }
                if variable == "PI_CODING_AGENT_DIR" {
                    assert_eq!(
                        instruction_targets_for_layout(McpClient::PrimeAgent, &layout)[0].path,
                        home.join(".prime/agent/AGENTS.md")
                    );
                }
            }
            let layout = ClientLayout::with_environment(
                home.clone(),
                home.join("platform-config"),
                platform,
                &[("OPENCLAW_PROFILE", "work")],
            )
            .unwrap();
            assert_eq!(
                targets_for_layout(McpClient::Openclaw, &layout)[0].path,
                home.join(".openclaw-work/openclaw.json")
            );
        }
    }

    #[test]
    fn environment_layout_preserves_empty_tilde_relative_and_profile_semantics() {
        let dir = tempdir().unwrap();
        let home = dir.path().join("home");
        let config = home.join("native");
        for platform in [
            ClientPlatform::Macos,
            ClientPlatform::Linux,
            ClientPlatform::Windows,
        ] {
            let defaults = ClientLayout::new(home.clone(), config.clone(), platform);
            let empty = LayoutEnvironment::KEYS
                .iter()
                .filter(|key| **key != "FLATPAK_XDG_CONFIG_HOME")
                .map(|key| (*key, ""))
                .collect::<Vec<_>>();
            let layout =
                ClientLayout::with_environment(home.clone(), config.clone(), platform, &empty)
                    .unwrap();
            for client in CONCRETE_CLIENTS {
                assert_eq!(
                    targets_for_layout(client, &layout)
                        .iter()
                        .map(|target| &target.path)
                        .collect::<Vec<_>>(),
                    targets_for_layout(client, &defaults)
                        .iter()
                        .map(|target| &target.path)
                        .collect::<Vec<_>>()
                );
            }
            let layout = ClientLayout::with_environment(
                home.clone(),
                config.clone(),
                platform,
                &[
                    ("PI_CODING_AGENT_DIR", "~/π agent"),
                    ("PRIME_AGENT_CODING_AGENT_DIR", "relative prime"),
                    ("GEMINI_CLI_HOME", "relative gemini"),
                    ("OPENCLAW_HOME", " ~/claw home "),
                    ("OPENCLAW_PROFILE", " Work_2 "),
                ],
            )
            .unwrap();
            assert_eq!(
                instruction_targets_for_layout(McpClient::Pi, &layout)[0].path,
                home.join("π agent/AGENTS.md")
            );
            assert_eq!(
                instruction_targets_for_layout(McpClient::PrimeAgent, &layout)[0].path,
                home.join("relative prime/AGENTS.md")
            );
            assert_eq!(
                targets_for_layout(McpClient::Gemini, &layout)[0].path,
                home.join("relative gemini/.gemini/settings.json")
            );
            assert_eq!(
                targets_for_layout(McpClient::Openclaw, &layout)[0].path,
                home.join("claw home/.openclaw-Work_2/openclaw.json")
            );
            for (vars, expected) in [
                (
                    vec![("OPENCLAW_PROFILE", "DEFAULT")],
                    home.join(".openclaw/openclaw.json"),
                ),
                (
                    vec![
                        ("OPENCLAW_HOME", "~/claw"),
                        ("OPENCLAW_PROFILE", "team"),
                        ("OPENCLAW_STATE_DIR", "~/state"),
                    ],
                    home.join("claw/state/openclaw.json"),
                ),
                (
                    vec![
                        ("OPENCLAW_STATE_DIR", "state"),
                        ("OPENCLAW_CONFIG_PATH", " team.json5 "),
                    ],
                    home.join("team.json5"),
                ),
            ] {
                let layout =
                    ClientLayout::with_environment(home.clone(), config.clone(), platform, &vars)
                        .unwrap();
                assert_eq!(
                    targets_for_layout(McpClient::Openclaw, &layout)[0].path,
                    expected
                );
            }
            for invalid in ["../escape", "-leading", "bad profile", &"a".repeat(65)] {
                let error = ClientLayout::with_environment(
                    home.clone(),
                    config.clone(),
                    platform,
                    &[("OPENCLAW_PROFILE", invalid)],
                )
                .unwrap_err();
                assert!(error.to_string().contains("OPENCLAW_PROFILE"));
            }
        }
        let flatpak = ClientLayout::with_environment(
            home.clone(),
            config,
            ClientPlatform::Linux,
            &[("FLATPAK_XDG_CONFIG_HOME", "")],
        )
        .unwrap();
        assert_eq!(
            targets_for_layout(McpClient::Zed, &flatpak)[0].path,
            home.join("zed/settings.json")
        );
    }

    fn parse_integration_args<T: clap::Args>(arguments: Vec<std::ffi::OsString>) -> T {
        let matches = T::augment_args(clap::Command::new("aise"))
            .try_get_matches_from(
                std::iter::once(std::ffi::OsString::from("aise")).chain(arguments),
            )
            .unwrap();
        T::from_arg_matches(&matches).unwrap()
    }

    #[test]
    fn configured_endpoints_replace_categories_and_keep_their_native_formats() {
        let dir = tempdir().unwrap();
        let home = dir.path().join("home");
        for platform in [
            ClientPlatform::Macos,
            ClientPlatform::Linux,
            ClientPlatform::Windows,
        ] {
            let automatic = ClientLayout::with_environment(
                home.clone(),
                home.join("native"),
                platform,
                &[
                    ("CODEX_HOME", "env-codex"),
                    ("GEMINI_CLI_HOME", "env-gemini"),
                ],
            )
            .unwrap();
            let mut text = String::new();
            for surface in &automatic.surfaces {
                let name = surface.name.as_str();
                text.push_str(&format!("[paths.{name}]\n"));
                if !surface.mcp.is_empty() {
                    text.push_str(&format!("mcp_config = '~/configured/{name}/mcp'\n"));
                }
                if surface.instruction.is_some() {
                    text.push_str(&format!(
                        "instruction_file = '~/configured/{name}/instructions.md'\n"
                    ));
                }
                if !surface.skill_roots.is_empty() {
                    text.push_str(&format!("skill_discovery_roots = ['~/configured/{name}/skills', '~/shared/{name}']\n"));
                }
            }
            let config: IntegrationsConfig = toml::from_str(&text).unwrap();
            let layout = automatic.clone().with_config(&config).unwrap();
            assert_eq!(
                config.paths.len(),
                layout.surfaces.len(),
                "config surfaces match the current client table"
            );
            for (before, after) in automatic.surfaces.iter().zip(&layout.surfaces) {
                let root = home.join("configured").join(after.name.as_str());
                if let Some(target) = after.mcp.first() {
                    assert_eq!(
                        after.mcp.len(),
                        1,
                        "one endpoint replaces compatibility files"
                    );
                    assert_eq!(target.path, root.join("mcp"));
                    assert_eq!(target.format, before.mcp[0].format);
                    assert_eq!(target.detect_paths, [root.clone()]);
                    assert!(target.superseded_paths.is_empty());
                }
                if let Some(target) = &after.instruction {
                    assert_eq!(target.path, root.join("instructions.md"));
                    assert_eq!(target.detect_paths, [root.clone()]);
                }
                if !after.skill_roots.is_empty() {
                    assert_eq!(
                        after.skill_roots,
                        [
                            root.join("skills"),
                            home.join("shared").join(after.name.as_str())
                        ]
                    );
                    assert_eq!(after.detect_paths, after.skill_roots);
                }
            }
            let extra = home.join("extra.toml");
            let mut arguments = IntegrationTargetsArgs {
                clients: CONCRETE_CLIENTS.to_vec(),
                codex_configs: vec![extra.clone()],
                ..Default::default()
            };
            let (targets, _, skills) = arguments.resolve(false, false, &layout).unwrap();
            assert!(targets.iter().any(|target| target.path == extra));
            assert!(!targets.iter().any(|target| target.path
                == automatic
                    .surfaces
                    .iter()
                    .find(|surface| surface.name == IntegrationSurface::Codex)
                    .unwrap()
                    .mcp[0]
                    .path));
            assert!(skills
                .iter()
                .flat_map(|target| &target.discovery_links)
                .any(|path| path == &home.join("configured/codex/skills/ai-session-search")));
            arguments
                .json_mcp_configs
                .push(home.join("configured/codex/unused/../mcp"));
            assert!(arguments
                .resolve(false, false, &layout)
                .unwrap_err()
                .to_string()
                .contains("incompatible config formats"));
        }
        let automatic = ClientLayout::new(home.clone(), home.join("native"), ClientPlatform::Linux);
        let config: IntegrationsConfig =
            toml::from_str("[paths.codex]\nskill_discovery_roots=[]\n").unwrap();
        let layout = automatic.with_config(&config).unwrap();
        assert!(skill_targets_for_layout(McpClient::Codex, &layout).is_empty());
        assert!(
            !all_standard_discovery_links(&layout, &AI_SESSION_SEARCH_SKILL_PACKAGE)
                .contains(&home.join(".agents/skills/ai-session-search"))
        );
    }

    #[test]
    fn configured_openclaw_and_kilo_paths_preserve_shapes_during_refresh_and_removal() {
        let dir = tempdir().unwrap();
        let home = dir.path().join("home");
        let config: IntegrationsConfig = toml::from_str("[paths.openclaw]\nmcp_config='~/team/claw.json5'\n[paths.kilocode]\nmcp_config='~/team/kilo.jsonc'\n").unwrap();
        let layout = ClientLayout::new(home.clone(), home.join("native"), ClientPlatform::Linux)
            .with_config(&config)
            .unwrap();
        let claw = targets_for_layout(McpClient::Openclaw, &layout).remove(0);
        fs::create_dir_all(claw.path.parent().unwrap()).unwrap();
        fs::write(
            &claw.path,
            "{ // team\n mcpServers: { aise: { command: '/bin/aise', args: ['mcp','serve'] } } }\n",
        )
        .unwrap();
        let receipt = dir.path().join("aise/receipt.json");
        assert_eq!(
            refresh_superseded_mcp_entries(&receipt, &layout, || panic!(
                "preserve the registered executable"
            ))
            .unwrap(),
            [claw.path.clone()]
        );
        assert_eq!(status_target(&claw).unwrap(), "configured");
        let value: Value = jsonc_parser::parse_to_serde_value(
            &fs::read_to_string(&claw.path).unwrap(),
            &Default::default(),
        )
        .unwrap();
        assert_eq!(value["mcp"]["servers"][SERVER_NAME]["command"], "/bin/aise");
        assert!(value["mcpServers"].get(SERVER_NAME).is_none());
        let kilo = targets_for_layout(McpClient::Kilocode, &layout).remove(0);
        upsert_target(&kilo, Path::new("/bin/aise")).unwrap();
        let value: Value = jsonc_parser::parse_to_serde_value(
            &fs::read_to_string(&kilo.path).unwrap(),
            &jsonc_parse_options(JsonSyntax::Jsonc),
        )
        .unwrap();
        assert_eq!(value["mcp"][SERVER_NAME]["type"], "local");
        assert_eq!(
            value["mcp"][SERVER_NAME]["command"],
            json!(["/bin/aise", "mcp", "serve"])
        );
        for target in [claw, kilo] {
            assert!(remove_target(&target).unwrap());
            assert_eq!(status_target(&target).unwrap(), "not configured");
        }
    }

    #[cfg(unix)]
    #[test]
    fn environment_layout_is_shared_by_install_status_uninstall_and_refresh() {
        for platform in [
            ClientPlatform::Macos,
            ClientPlatform::Linux,
            ClientPlatform::Windows,
        ] {
            let dir = tempdir().unwrap();
            let home = dir.path().join("home");
            let codex = dir.path().join("codex work");
            let pi = dir.path().join("pi work");
            let layout = ClientLayout::with_environment(
                home.clone(),
                home.join("native"),
                platform,
                &[
                    ("CODEX_HOME", codex.to_str().unwrap()),
                    ("PI_CODING_AGENT_DIR", pi.to_str().unwrap()),
                ],
            )
            .unwrap();
            let receipt = dir.path().join("aise/receipt.json");
            let binary = std::env::current_exe().unwrap();
            let args = parse_integration_args::<IntegrationInstallArgs>(vec![
                "--client".into(),
                "codex".into(),
                "--client".into(),
                "pi".into(),
                "--no-aliases".into(),
                "--binary".into(),
                binary.into_os_string(),
            ]);
            install_for_layout(args, &receipt, &layout).unwrap();
            let target = targets_for_layout(McpClient::Codex, &layout).remove(0);
            assert_eq!(target.path, codex.join("config.toml"));
            assert_eq!(status_target(&target).unwrap(), "configured");
            assert!(!home.join(".codex").exists());
            let args = parse_integration_args::<IntegrationStatusArgs>(vec![
                "--client".into(),
                "codex".into(),
                "--client".into(),
                "pi".into(),
                "--no-aliases".into(),
            ]);
            status_for_layout(args, &receipt, &layout).unwrap();
            let instruction = codex.join("AGENTS.md");
            let text = fs::read_to_string(&instruction)
                .unwrap()
                .replace(INSTRUCTIONS_LINE, RC1_INSTRUCTIONS_LINE);
            fs::write(&instruction, text).unwrap();
            assert!(matches!(
                refresh_integrations_for_version(&receipt, "9.9.9", Some(&layout)).unwrap(),
                UpgradeRefresh::Checked { .. }
            ));
            assert!(fs::read_to_string(&instruction)
                .unwrap()
                .contains(INSTRUCTIONS_LINE));
            let canonical = dir.path().join("aise/skills/ai-session-search");
            let args = parse_integration_args::<IntegrationUninstallArgs>(vec![
                "--client".into(),
                "codex".into(),
                "--keep-aliases".into(),
            ]);
            uninstall_for_layout(args, &receipt, &layout).unwrap();
            assert_eq!(status_target(&target).unwrap(), "not configured");
            assert!(
                canonical.join("SKILL.md").exists(),
                "Pi still links to the canonical package"
            );
            assert!(pi.join("skills/ai-session-search/SKILL.md").exists());
            let args = parse_integration_args::<IntegrationUninstallArgs>(vec![
                "--client".into(),
                "pi".into(),
                "--keep-aliases".into(),
            ]);
            uninstall_for_layout(args, &receipt, &layout).unwrap();
            assert!(!canonical.exists());
        }
    }

    #[test]
    fn injected_platform_layout_resolves_opencode_and_kilocode_paths() {
        for (platform, home, config, expected_opencode, expected_kilocode) in [
            (
                ClientPlatform::Macos,
                "/Users/alice",
                "/Users/alice/Library/Application Support",
                "/Users/alice/.config/opencode/opencode.json",
                "/Users/alice/Library/Application Support/Code/User/globalStorage/kilocode.kilo-code/settings/mcp_settings.json",
            ),
            (
                ClientPlatform::Linux,
                "/home/alice",
                "/home/alice/.config",
                "/home/alice/.config/opencode/opencode.json",
                "/home/alice/.config/Code/User/globalStorage/kilocode.kilo-code/settings/mcp_settings.json",
            ),
            (
                ClientPlatform::Windows,
                "C:/Users/Alice",
                "C:/Users/Alice/AppData/Roaming",
                "C:/Users/Alice/.config/opencode/opencode.json",
                "C:/Users/Alice/AppData/Roaming/Code/User/globalStorage/kilocode.kilo-code/settings/mcp_settings.json",
            ),
        ] {
            let layout = ClientLayout::new(PathBuf::from(home), PathBuf::from(config), platform);
            let opencode = targets_for_layout(McpClient::Opencode, &layout);
            let kilocode = targets_for_layout(McpClient::Kilocode, &layout);

            assert_eq!(opencode.len(), 1);
            assert_eq!(opencode[0].path, PathBuf::from(expected_opencode));
            assert_eq!(kilocode.len(), 2);
            assert_eq!(kilocode[1].path, PathBuf::from(expected_kilocode));
        }
    }

    /// Kilo Code 7 loads `~/.config/kilo/kilo.jsonc` (servers directly under `mcp`, OpenCode's
    /// local entry) in every mode; it merges the legacy extension `mcp_settings.json` only inside
    /// VS Code or its daemon (Kilo-Org/kilocode packages/opencode/src/kilocode/config/config.ts),
    /// so the legacy file stays a second target for older extensions.
    #[test]
    fn kilocode_writes_kilo_jsonc_beside_the_legacy_extension_file() {
        let dir = tempdir().unwrap();
        let home = dir.path().join("home");
        for (platform, config) in [
            (
                ClientPlatform::Macos,
                home.join("Library/Application Support"),
            ),
            (ClientPlatform::Linux, home.join(".config")),
            (ClientPlatform::Windows, home.join("AppData/Roaming")),
        ] {
            let layout = ClientLayout::new(home.clone(), config, platform);
            let targets = targets_for_layout(McpClient::Kilocode, &layout);
            assert_eq!(targets.len(), 2, "{platform:?}");
            assert_eq!(targets[0].path, home.join(".config/kilo/kilo.jsonc"));
            assert!(targets[1]
                .path
                .ends_with("kilocode.kilo-code/settings/mcp_settings.json"));
        }

        let path = home.join(".config/kilo/kilo.jsonc");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "{\n  // user note\n  \"mcp\": {\"user-server\": {\"command\": [\"user\"], \"type\": \"local\"}},\n  \"permission\": {\"bash\": \"allow\"},\n}\n",
        )
        .unwrap();
        let layout = ClientLayout::new(home.clone(), home.join(".config"), ClientPlatform::Linux);
        let target = targets_for_layout(McpClient::Kilocode, &layout).remove(0);
        assert_eq!(status_target(&target).unwrap(), "not configured");
        upsert_target(&target, Path::new("/bin/aise")).unwrap();
        let written = fs::read_to_string(&path).unwrap();
        assert!(written.contains("// user note"), "{written}");
        let data: Value =
            jsonc_parser::parse_to_serde_value(&written, &jsonc_parse_options(JsonSyntax::Jsonc))
                .unwrap();
        assert_eq!(
            data["mcp"][SERVER_NAME],
            json!({"type": "local", "command": ["/bin/aise", "mcp", "serve"]})
        );
        assert_eq!(data["mcp"]["user-server"]["command"][0], "user");
        assert_eq!(data["permission"]["bash"], "allow");
        assert_eq!(status_target(&target).unwrap(), "configured");
        assert!(remove_target(&target).unwrap());
        assert_eq!(status_target(&target).unwrap(), "not configured");
    }

    /// OpenClaw reads servers from `mcp.servers` (openclaw/openclaw src/config/mcp-config.ts) and
    /// parses `~/.openclaw/openclaw.json` as JSON5 (src/config/io.load.ts). aise wrote a top-level
    /// `mcpServers`, which OpenClaw never reads.
    #[test]
    fn openclaw_writes_mcp_servers_in_json5_and_moves_the_top_level_entry() {
        let dir = tempdir().unwrap();
        let home = dir.path().join("home");
        let layout = ClientLayout::new(home.clone(), home.join(".config"), ClientPlatform::Linux);
        let target = targets_for_layout(McpClient::Openclaw, &layout).remove(0);
        assert_eq!(target.path, home.join(".openclaw/openclaw.json"));
        fs::create_dir_all(target.path.parent().unwrap()).unwrap();
        fs::write(
            &target.path,
            "{\n  // gateway settings\n  gateway: { port: 18789 },\n  mcpServers: {\n    'ai-session-search': { command: '/old/aise', args: ['mcp', 'serve'] },\n  },\n}\n",
        )
        .unwrap();
        assert_eq!(status_target(&target).unwrap(), "stale");

        upsert_target(&target, Path::new("/bin/aise")).unwrap();
        let written = fs::read_to_string(&target.path).unwrap();
        assert!(written.contains("// gateway settings"), "{written}");
        let data: Value =
            jsonc_parser::parse_to_serde_value(&written, &Default::default()).unwrap();
        assert_eq!(
            data["mcp"]["servers"][SERVER_NAME],
            json!({"command": "/bin/aise", "args": ["mcp", "serve"]})
        );
        assert!(data["mcpServers"].get(SERVER_NAME).is_none(), "{written}");
        assert_eq!(data["gateway"]["port"], 18789);
        assert_eq!(status_target(&target).unwrap(), "configured");

        assert!(remove_target(&target).unwrap());
        assert_eq!(status_target(&target).unwrap(), "not configured");
    }

    /// Zed's settings.json and VS Code's mcp.json are JSONC: Zed writes a comment header into a
    /// new settings file. A strict JSON parse failed the whole install, every client included,
    /// and rewriting through serde_json would drop the user's comments.
    #[test]
    fn jsonc_comments_and_trailing_commas_survive_install_and_uninstall() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let original = "// Zed settings\n//\n// keep this header\n{\n  \"theme\": \"One Dark\", // keep this note\n  \"context_servers\": {\n    \"user-server\": {\"command\": \"user\"},\n  },\n}\n";
        fs::write(&path, original).unwrap();
        let target = Target {
            label: "zed",
            path: path.clone(),
            format: ConfigFormat::ZedContextServers,
            detect_paths: Vec::new(),
            detect_binaries: Vec::new(),
            superseded_paths: Vec::new(),
            codex_marketplace: None,
        };
        assert_eq!(status_target(&target).unwrap(), "not configured");

        upsert_target(&target, Path::new("/bin/aise")).unwrap();
        upsert_target(&target, Path::new("/bin/aise")).unwrap();
        let written = fs::read_to_string(&path).unwrap();
        assert!(
            written.starts_with("// Zed settings\n//\n// keep this header\n"),
            "{written}"
        );
        assert!(written.contains("// keep this note"), "{written}");
        assert_eq!(written.matches(SERVER_NAME).count(), 1, "{written}");
        assert_eq!(status_target(&target).unwrap(), "configured");

        assert!(remove_target(&target).unwrap());
        let removed = fs::read_to_string(&path).unwrap();
        assert!(removed.contains("// keep this note") && !removed.contains(SERVER_NAME));
        assert!(removed.contains("user-server"));
        assert_eq!(status_target(&target).unwrap(), "not configured");

        fs::write(&path, "// a comment\n[1, 2]\n").unwrap();
        let error = upsert_target(&target, Path::new("/bin/aise")).unwrap_err();
        assert!(format!("{error:#}").contains("top level"), "{error:#}");
    }

    /// Zed's `paths::config_dir()` per platform; only macOS has a superseded aise location.
    #[test]
    fn injected_platform_layout_resolves_zed_settings_on_every_platform() {
        for (platform, home, config, expected, superseded) in [
            (
                ClientPlatform::Macos,
                "/Users/alice",
                "/Users/alice/Library/Application Support",
                "/Users/alice/.config/zed/settings.json",
                Some("/Users/alice/Library/Application Support/Zed/settings.json"),
            ),
            (
                ClientPlatform::Linux,
                "/home/alice",
                "/home/alice/.config",
                "/home/alice/.config/zed/settings.json",
                None,
            ),
            (
                ClientPlatform::Windows,
                "C:/Users/Alice",
                "C:/Users/Alice/AppData/Roaming",
                "C:/Users/Alice/AppData/Roaming/zed/settings.json",
                None,
            ),
        ] {
            let layout = ClientLayout::new(PathBuf::from(home), PathBuf::from(config), platform);
            let zed = targets_for_layout(McpClient::Zed, &layout);
            assert_eq!(zed.len(), 1);
            assert_eq!(zed[0].path, PathBuf::from(expected));
            assert_eq!(
                zed[0].superseded_paths,
                superseded
                    .into_iter()
                    .map(PathBuf::from)
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn claude_legacy_file_is_only_a_superseded_path() {
        for action in ["install", "upgrade", "uninstall"] {
            let dir = tempdir().unwrap();
            let (receipt, layout) = home_with_rc1_instructions(dir.path());
            let targets = targets_for_layout(McpClient::Claude, &layout);
            assert_eq!(targets.len(), 2);
            let target = targets
                .iter()
                .find(|target| target.label == "claude code modern")
                .unwrap();
            let legacy = layout.home.join(".claude/.mcp.json");
            assert_eq!(target.superseded_paths, std::slice::from_ref(&legacy));
            assert!(!targets.iter().any(|target| target.path == legacy));
            let original = "{ // keep comment\n\"mcpServers\":{\"ai-session-search\":{\"command\":\"/custom/aise\",\"args\":[\"mcp\",\"serve\"]},\"ai_session_search\":{\"command\":\"/custom/aise\"},\"aise\":{\"command\":\"/custom/aise\"},\"other\":{\"command\":\"keep\"}},\"keep\":true}\n";
            fs::write(&legacy, original).unwrap();
            assert_eq!(status_target(target).unwrap(), "stale");
            match action {
                "install" => {
                    let planned = plan_upsert_target(target, Path::new("/custom/aise")).unwrap();
                    assert_eq!(
                        fs::read_to_string(&legacy).unwrap(),
                        original,
                        "planning is read-only"
                    );
                    publish_planned_mutations(&planned).unwrap();
                }
                "upgrade" => {
                    refresh_integrations_for_version(&receipt, "9.9.9", Some(&layout)).unwrap();
                }
                "uninstall" => {
                    assert!(remove_target(target).unwrap());
                }
                _ => unreachable!(),
            }
            let old = fs::read_to_string(&legacy).unwrap();
            assert!(old.contains("// keep comment"));
            let old = parse_json_object_or_empty(&legacy, Some(&old), JsonSyntax::Jsonc).unwrap();
            assert_eq!(old["mcpServers"], json!({"other":{"command":"keep"}}));
            assert_eq!(old["keep"], true);
            if action == "uninstall" {
                assert!(!target.path.exists());
            } else {
                assert_eq!(status_target(target).unwrap(), "configured");
                let modern: Value =
                    serde_json::from_str(&fs::read_to_string(&target.path).unwrap()).unwrap();
                assert_eq!(modern["mcpServers"][SERVER_NAME]["command"], "/custom/aise");
            }
        }
        let dir = tempdir().unwrap();
        let layout = ClientLayout::new(
            dir.path().join("home"),
            dir.path().join("config"),
            ClientPlatform::Linux,
        );
        let target = targets_for_layout(McpClient::Claude, &layout).remove(0);
        publish_planned_mutations(&plan_upsert_target(&target, Path::new("/custom/aise")).unwrap())
            .unwrap();
        assert!(!layout.home.join(".claude/.mcp.json").exists());
    }

    #[test]
    fn cursor_requires_stdio_without_changing_other_mcp_clients() {
        let dir = tempdir().unwrap();
        let layout = ClientLayout::new(
            dir.path().join("home"),
            dir.path().join("config"),
            ClientPlatform::Linux,
        );
        let target = targets_for_layout(McpClient::Cursor, &layout).remove(0);
        let entry = json_server_entry(target.format, "/custom/aise");
        assert_eq!(entry["type"], "stdio");
        let untyped = json!({"command":"/custom/aise","args":["mcp","serve"]});
        assert!(!json_entry_is_current(&untyped, target.format));
        assert!(json_entry_is_current(&entry, target.format));
        assert_eq!(
            json_server_entry(ConfigFormat::JsonMcpServers, "/custom/aise").get("type"),
            None
        );
        assert!(json_entry_is_current(
            &untyped,
            ConfigFormat::JsonMcpServers
        ));
        fs::create_dir_all(target.path.parent().unwrap()).unwrap();
        let original = "{ // keep comment\n\"mcpServers\": {\"ai-session-search\": {\"command\":\"/custom/aise\",\"args\":[\"mcp\",\"serve\"]},\"other\": {\"command\":\"keep\"}},\"keep\":true}\n";
        fs::write(&target.path, original).unwrap();
        assert_eq!(status_target(&target).unwrap(), "stale");
        let receipt = dir.path().join("transaction.json");
        assert_eq!(
            refresh_superseded_mcp_entries(&receipt, &layout, || panic!("keep the old executable"))
                .unwrap(),
            std::slice::from_ref(&target.path)
        );
        assert_eq!(status_target(&target).unwrap(), "configured");
        let written = fs::read_to_string(&target.path).unwrap();
        assert!(written.contains("// keep comment"));
        let root =
            parse_json_object_or_empty(&target.path, Some(&written), JsonSyntax::Jsonc).unwrap();
        assert_eq!(root["mcpServers"][SERVER_NAME], entry);
        assert_eq!(root["mcpServers"]["other"]["command"], "keep");
        assert!(normalize_planned_mutations(
            plan_upsert_target(&target, Path::new("/custom/aise")).unwrap()
        )
        .unwrap()
        .is_empty());
        assert!(remove_target(&target).unwrap());
        assert_eq!(status_target(&target).unwrap(), "not configured");
        for custom in [
            json!({"type":"http","command":"/custom/aise","args":["mcp","serve"]}),
            json!({"command":"/custom/aise","args":["mcp","serve","--custom"]}),
        ] {
            let original = json!({"mcpServers": {SERVER_NAME: custom}}).to_string();
            fs::write(&target.path, &original).unwrap();
            assert!(refresh_superseded_mcp_entries(&receipt, &layout, || panic!(
                "preserve custom entries"
            ))
            .unwrap()
            .is_empty());
            assert_eq!(fs::read_to_string(&target.path).unwrap(), original);
        }
    }

    #[test]
    fn prime_agent_requires_stdio_in_strict_json_settings() {
        let dir = tempdir().unwrap();
        let layout = ClientLayout::new(
            dir.path().join("home"),
            dir.path().join("config"),
            ClientPlatform::Linux,
        );
        let targets = targets_for_layout(McpClient::PrimeAgent, &layout);
        assert_eq!(targets.len(), 1);
        let target = &targets[0];
        assert_eq!(target.path, layout.home.join(".prime/agent/settings.json"));
        fs::create_dir_all(target.path.parent().unwrap()).unwrap();
        fs::write(&target.path, r#"{"mcpServers":{"ai-session-search":{"command":"/custom/aise","args":["mcp","serve"]},"other":{"type":"http","url":"https://example.com/mcp"}},"theme":"keep"}"#).unwrap();
        assert_eq!(status_target(target).unwrap(), "stale");
        publish_planned_mutations(&plan_upsert_target(target, Path::new("/custom/aise")).unwrap())
            .unwrap();
        let data: Value = serde_json::from_str(&fs::read_to_string(&target.path).unwrap()).unwrap();
        assert_eq!(
            data["mcpServers"][SERVER_NAME],
            json!({"type":"stdio","command":"/custom/aise","args":["mcp","serve"]})
        );
        assert_eq!(data["theme"], "keep");
        assert_eq!(data["mcpServers"]["other"]["type"], "http");
        assert_eq!(status_target(target).unwrap(), "configured");
        assert!(normalize_planned_mutations(
            plan_upsert_target(target, Path::new("/custom/aise")).unwrap()
        )
        .unwrap()
        .is_empty());
        assert!(remove_target(target).unwrap());
        fs::write(&target.path, "{ // not strict JSON\n\"theme\": \"keep\", }").unwrap();
        assert!(plan_upsert_target(target, Path::new("/custom/aise")).is_err());
        assert_eq!(
            instruction_targets_for_layout(McpClient::PrimeAgent, &layout)[0].format,
            InstructionFormat::InlineBlock
        );
    }

    #[test]
    fn pi_registers_stdio_mcp_in_its_strict_json_config() {
        let dir = tempdir().unwrap();
        let layout = ClientLayout::new(
            dir.path().join("home"),
            dir.path().join("config"),
            ClientPlatform::Linux,
        );
        let targets = targets_for_layout(McpClient::Pi, &layout);
        assert_eq!(targets.len(), 1);
        let target = &targets[0];
        assert_eq!(target.path, layout.home.join(".pi/agent/mcp.json"));
        fs::create_dir_all(target.path.parent().unwrap()).unwrap();
        fs::write(
            &target.path,
            r#"{"mcpServers":{"other":{"command":"keep"}},"keep":true}"#,
        )
        .unwrap();
        publish_planned_mutations(&plan_upsert_target(target, Path::new("/custom/aise")).unwrap())
            .unwrap();
        let data: Value = serde_json::from_str(&fs::read_to_string(&target.path).unwrap()).unwrap();
        assert_eq!(
            data["mcpServers"][SERVER_NAME],
            json!({"command":"/custom/aise","args":["mcp","serve"]})
        );
        assert_eq!(data["mcpServers"]["other"]["command"], "keep");
        assert_eq!(data["keep"], true);
        assert_eq!(status_target(target).unwrap(), "configured");
        assert!(normalize_planned_mutations(
            plan_upsert_target(target, Path::new("/custom/aise")).unwrap()
        )
        .unwrap()
        .is_empty());
        assert!(remove_target(target).unwrap());
        assert_eq!(status_target(target).unwrap(), "not configured");
        fs::write(&target.path, "{ // not valid for Pi\n\"keep\": true, }").unwrap();
        assert!(plan_upsert_target(target, Path::new("/custom/aise")).is_err());
    }

    #[test]
    fn an_upgrade_registers_mcp_for_existing_pi_family_cli_integrations() {
        for (agent_dir, config_file) in
            [(".pi/agent", "mcp.json"), (".prime/agent", "settings.json")]
        {
            let dir = tempdir().unwrap();
            let (receipt, layout) = home_with_rc1_instructions(dir.path());
            let instructions = layout.home.join(agent_dir).join("AGENTS.md");
            fs::create_dir_all(instructions.parent().unwrap()).unwrap();
            fs::write(
                &instructions,
                instruction_block(InstructionFormat::InlineCliBlock),
            )
            .unwrap();
            refresh_superseded_mcp_entries(&receipt, &layout, || Ok(PathBuf::from("/custom/aise")))
                .unwrap();
            refresh_earlier_instruction_blocks(&receipt, &layout).unwrap();
            let path = layout.home.join(agent_dir).join(config_file);
            let data: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
            assert_eq!(data["mcpServers"][SERVER_NAME]["command"], "/custom/aise");
            assert_eq!(
                fs::read_to_string(instructions).unwrap(),
                instruction_block(InstructionFormat::InlineBlock)
            );
            assert!(!layout.home.join(".cursor/mcp.json").exists());
        }
    }

    #[test]
    fn cli_to_mcp_upgrade_requires_untouched_guidance_and_a_resolved_binary() {
        let dir = tempdir().unwrap();
        let (receipt, layout) = home_with_rc1_instructions(dir.path());
        let instructions = layout.home.join(".pi/agent/AGENTS.md");
        let old = instruction_block(InstructionFormat::InlineCliBlock);
        for text in [
            String::new(),
            old.replace(CLI_INSTRUCTIONS_LINE, "my guidance"),
        ] {
            fs::write(&instructions, &text).unwrap();
            assert!(refresh_superseded_mcp_entries(&receipt, &layout, || panic!(
                "must not resolve a new binary"
            ))
            .unwrap()
            .is_empty());
            assert!(!layout.home.join(".pi/agent/mcp.json").exists());
        }
        fs::write(&instructions, &old).unwrap();
        assert!(
            refresh_superseded_mcp_entries(&receipt, &layout, || bail!("no aise on PATH")).is_err()
        );
        assert_eq!(fs::read_to_string(&instructions).unwrap(), old);
        let target = targets_for_layout(McpClient::Pi, &layout).remove(0);
        fs::write(&target.path, "invalid JSON").unwrap();
        assert!(refresh_superseded_mcp_entries(&receipt, &layout, || panic!(
            "invalid config must be preserved"
        ))
        .is_err());
        assert_eq!(fs::read_to_string(&target.path).unwrap(), "invalid JSON");
    }

    #[test]
    fn pi_family_paths_are_platform_independent_home_relative_contracts() {
        for (platform, home, config) in [
            (
                ClientPlatform::Macos,
                "/Users/alice",
                "/Users/alice/Library/Application Support",
            ),
            (ClientPlatform::Linux, "/home/alice", "/home/alice/.config"),
            (
                ClientPlatform::Windows,
                "C:/Users/Alice",
                "C:/Users/Alice/AppData/Roaming",
            ),
        ] {
            let layout = ClientLayout::new(PathBuf::from(home), PathBuf::from(config), platform);
            for (client, agent_dir) in [
                (McpClient::Pi, ".pi/agent"),
                (McpClient::PrimeAgent, ".prime/agent"),
            ] {
                let expected_root = PathBuf::from(home).join(agent_dir);
                assert_eq!(
                    instruction_targets_for_layout(client, &layout)[0].path,
                    expected_root.join("AGENTS.md")
                );
                assert_eq!(
                    skill_targets_for_layout(client, &layout)[0].discovery_links,
                    vec![expected_root.join("skills/ai-session-search")]
                );
            }
        }
    }

    #[test]
    fn injected_layout_rejects_missing_home_instead_of_using_cwd() {
        let error = ClientLayout::from_discovered_dirs(
            None,
            Some(PathBuf::from("/tmp/config")),
            ClientPlatform::Linux,
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "cannot determine the home directory for MCP client configuration; set HOME or USERPROFILE"
        );
    }

    #[test]
    fn upsert_json_preserves_existing_servers() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        fs::write(
            &path,
            r#"{"mcpServers":{"aise":{"command":"oldest"},"ai_session_search":{"command":"legacy"},"other":{"command":"other"}},"keep":true}"#,
        )
        .unwrap();

        upsert_json_mcp_server(&path, json!({"command": "/bin/aise"})).unwrap();
        let data: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let servers = data["mcpServers"].as_object().unwrap();
        assert_eq!(SERVER_NAME, "ai-session-search");
        assert!(servers.contains_key("other"));
        assert!(!servers.contains_key("aise"));
        assert!(!servers.contains_key("ai_session_search"));
        assert_eq!(servers[SERVER_NAME]["command"], "/bin/aise");
        assert_eq!(data["keep"], true);
    }

    #[test]
    fn uninstall_json_preserves_other_servers_and_root_keys() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        fs::write(
            &path,
            r#"{"mcpServers":{"ai-session-search":{"command":"/current"},"aise":{"command":"/oldest"},"ai_session_search":{"command":"/legacy"},"other":{"command":"other"}},"keep":true}"#,
        )
        .unwrap();

        assert!(remove_json_mcp_server(&path).unwrap());
        let data: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let servers = data["mcpServers"].as_object().unwrap();
        assert!(!servers.contains_key("aise"));
        assert!(!servers.contains_key("ai_session_search"));
        assert!(!servers.contains_key("ai-session-search"));
        assert!(servers.contains_key("other"));
        assert_eq!(data["keep"], true);
    }

    /// Zed reads `~/.config/zed/settings.json` on macOS (`paths::config_dir()` in Zed's source).
    /// Earlier aise wrote `~/Library/Application Support/Zed/settings.json`, which Zed never reads.
    #[test]
    fn zed_on_macos_uses_dot_config_and_moves_the_entry_out_of_application_support() {
        let dir = tempdir().unwrap();
        let home = dir.path().join("home");
        let layout = ClientLayout::new(
            home.clone(),
            home.join("Library").join("Application Support"),
            ClientPlatform::Macos,
        );
        let old = home.join("Library/Application Support/Zed/settings.json");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        fs::write(
            &old,
            r#"{"context_servers":{"ai-session-search":{"command":"/old/aise","args":["mcp","serve"]},
                "user-server":{"command":"user"}}}"#,
        )
        .unwrap();
        let target = targets_for_layout(McpClient::Zed, &layout).remove(0);
        assert_eq!(target.path, home.join(".config/zed/settings.json"));
        assert!(
            target_detected(&target),
            "the Application Support directory alone still detects Zed"
        );
        assert_eq!(status_target(&target).unwrap(), "stale");

        upsert_target(&target, Path::new("/bin/aise")).unwrap();
        let new: Value = serde_json::from_str(&fs::read_to_string(&target.path).unwrap()).unwrap();
        assert_eq!(new["context_servers"][SERVER_NAME]["command"], "/bin/aise");
        let left: Value = serde_json::from_str(&fs::read_to_string(&old).unwrap()).unwrap();
        assert!(left["context_servers"].get(SERVER_NAME).is_none());
        assert_eq!(left["context_servers"]["user-server"]["command"], "user");
        assert_eq!(status_target(&target).unwrap(), "configured");

        assert!(remove_target(&target).unwrap());
        assert_eq!(status_target(&target).unwrap(), "not configured");

        let linux = ClientLayout::new(home.clone(), home.join(".config"), ClientPlatform::Linux);
        let linux_target = targets_for_layout(McpClient::Zed, &linux).remove(0);
        assert_eq!(linux_target.path, home.join(".config/zed/settings.json"));
        assert!(linux_target.superseded_paths.is_empty());
    }

    #[test]
    fn vscode_and_zed_use_their_native_container_keys() {
        let dir = tempdir().unwrap();
        let vscode = dir.path().join("vscode.json");
        let zed = dir.path().join("zed.json");

        upsert_target(
            &Target {
                label: "vscode",
                path: vscode.clone(),
                format: ConfigFormat::VscodeServers,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
                superseded_paths: Vec::new(),
                codex_marketplace: None,
            },
            Path::new("/bin/aise"),
        )
        .unwrap();
        upsert_target(
            &Target {
                label: "zed",
                path: zed.clone(),
                format: ConfigFormat::ZedContextServers,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
                superseded_paths: Vec::new(),
                codex_marketplace: None,
            },
            Path::new("/bin/aise"),
        )
        .unwrap();

        let vscode_data: Value =
            serde_json::from_str(&fs::read_to_string(vscode).unwrap()).unwrap();
        assert_eq!(vscode_data["servers"][SERVER_NAME]["type"], "stdio");
        assert_eq!(vscode_data["servers"][SERVER_NAME]["command"], "/bin/aise");
        assert_eq!(
            vscode_data["servers"][SERVER_NAME]["args"],
            json!(["mcp", "serve"])
        );
        let zed_data: Value = serde_json::from_str(&fs::read_to_string(zed).unwrap()).unwrap();
        assert_eq!(
            zed_data["context_servers"][SERVER_NAME]["command"],
            "/bin/aise"
        );
        assert_eq!(
            zed_data["context_servers"][SERVER_NAME]["args"],
            json!(["mcp", "serve"])
        );
    }

    /// OpenCode 2 reads local servers from `mcp.servers.<name>` with `type: "local"` and ignores
    /// a server placed directly under `mcp`: `opencode mcp list` 1.18.31 reported "No MCP servers
    /// configured" for the flat entry aise wrote before. https://opencode.ai/docs/mcp-servers/
    #[test]
    fn opencode_writes_mcp_servers_and_moves_its_flat_entry() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("opencode.json");
        fs::write(
            &path,
            r#"{"mcp":{"ai-session-search":{"command":["/old/aise","mcp","serve"],"enabled":true},
                "user-server":{"command":["user"],"enabled":true}}}"#,
        )
        .unwrap();
        let target = Target {
            label: "opencode",
            path: path.clone(),
            format: ConfigFormat::OpenCode,
            detect_paths: Vec::new(),
            detect_binaries: Vec::new(),
            superseded_paths: Vec::new(),
            codex_marketplace: None,
        };
        assert_eq!(status_target(&target).unwrap(), "stale");

        upsert_target(&target, Path::new("/bin/aise")).unwrap();
        upsert_target(&target, Path::new("/bin/aise")).unwrap();
        let data: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            data["mcp"]["servers"][SERVER_NAME],
            json!({"type": "local", "command": ["/bin/aise", "mcp", "serve"]})
        );
        assert!(data["mcp"].get(SERVER_NAME).is_none(), "flat entry moved");
        assert_eq!(data["mcp"]["user-server"]["command"][0], "user");
        assert_eq!(status_target(&target).unwrap(), "configured");

        assert!(remove_target(&target).unwrap());
        let data: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(data["mcp"]["servers"].get(SERVER_NAME).is_none());
        assert_eq!(data["mcp"]["user-server"]["command"][0], "user");
        assert_eq!(status_target(&target).unwrap(), "not configured");
    }

    #[test]
    fn remove_target_matches_each_config_shape() {
        let dir = tempdir().unwrap();
        let binary = Path::new("/bin/aise");
        let targets = [
            Target {
                label: "json",
                path: dir.path().join("json.json"),
                format: ConfigFormat::JsonMcpServers,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
                superseded_paths: Vec::new(),
                codex_marketplace: None,
            },
            Target {
                label: "vscode",
                path: dir.path().join("vscode.json"),
                format: ConfigFormat::VscodeServers,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
                superseded_paths: Vec::new(),
                codex_marketplace: None,
            },
            Target {
                label: "zed",
                path: dir.path().join("zed.json"),
                format: ConfigFormat::ZedContextServers,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
                superseded_paths: Vec::new(),
                codex_marketplace: None,
            },
            Target {
                label: "opencode",
                path: dir.path().join("opencode.json"),
                format: ConfigFormat::OpenCode,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
                superseded_paths: Vec::new(),
                codex_marketplace: None,
            },
            Target {
                label: "codex",
                path: dir.path().join("config.toml"),
                format: ConfigFormat::CodexToml,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
                superseded_paths: Vec::new(),
                codex_marketplace: None,
            },
        ];

        for target in &targets {
            upsert_target(target, binary).unwrap();
            if matches!(target.format, ConfigFormat::JsonMcpServers) {
                let data: Value =
                    serde_json::from_str(&fs::read_to_string(&target.path).unwrap()).unwrap();
                assert_eq!(
                    data["mcpServers"][SERVER_NAME]["args"],
                    json!(["mcp", "serve"])
                );
            }
            assert_eq!(status_target(target).unwrap(), "configured");
            assert!(remove_target(target).unwrap());
            assert_eq!(status_target(target).unwrap(), "not configured");
        }
    }

    /// Codex desktop's Plugins list shows only marketplace plugins
    /// (https://developers.openai.com/codex/plugins/build). aise registers its own marketplace,
    /// `ai-session-search`, the way `codex plugin marketplace add <dir>` does, and lists an
    /// MCP-only plugin in it: the skill already reaches Codex through `~/.agents/skills`, and a
    /// plugin copy would list it twice. The shared personal marketplace is never edited.
    #[test]
    fn codex_registers_an_aise_marketplace_with_an_mcp_only_plugin() {
        let dir = tempdir().unwrap();
        let config = dir.path().join("home/.codex/config.toml");
        let root = dir.path().join("app/codex-marketplace");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, "# mine\n[existing]\nvalue = true\n").unwrap();
        let target = Target {
            label: "codex",
            path: config.clone(),
            format: ConfigFormat::CodexToml,
            detect_paths: Vec::new(),
            detect_binaries: Vec::new(),
            superseded_paths: Vec::new(),
            codex_marketplace: Some(root.clone()),
        };
        assert_eq!(status_target(&target).unwrap(), "not configured");

        upsert_target(&target, Path::new("/bin/aise")).unwrap();
        let toml = fs::read_to_string(&config).unwrap();
        let marketplace = root.join(".agents/plugins/marketplace.json");
        let plugin = root.join("plugins").join(SERVER_NAME);
        let snapshot = |paths: &[PathBuf]| {
            paths
                .iter()
                .map(|path| fs::read_to_string(path).unwrap())
                .collect::<Vec<_>>()
        };
        let files = [
            config.clone(),
            marketplace.clone(),
            plugin.join(".codex-plugin/plugin.json"),
            plugin.join(".mcp.json"),
        ];
        let first = snapshot(&files);
        upsert_target(&target, Path::new("/bin/aise")).unwrap();
        assert_eq!(snapshot(&files), first, "reinstall is byte-idempotent");

        let document: toml_edit::DocumentMut = toml.parse().unwrap();
        assert!(toml.contains("# mine") && toml.contains("[existing]"));
        assert_eq!(
            document["marketplaces"][SERVER_NAME]["source_type"].as_str(),
            Some("local")
        );
        assert_eq!(
            document["marketplaces"][SERVER_NAME]["source"].as_str(),
            root.to_str()
        );
        let listed: Value = serde_json::from_str(&first[1]).unwrap();
        assert_eq!(listed["name"], SERVER_NAME);
        assert_eq!(listed["plugins"][0]["name"], SERVER_NAME);
        assert_eq!(
            listed["plugins"][0]["source"]["path"],
            "./plugins/ai-session-search"
        );
        let manifest: Value = serde_json::from_str(&first[2]).unwrap();
        assert_eq!(manifest["mcpServers"], "./.mcp.json");
        assert!(
            manifest.get("skills").is_none(),
            "the skill comes from ~/.agents/skills"
        );
        let servers: Value = serde_json::from_str(&first[3]).unwrap();
        assert_eq!(
            servers["mcpServers"][SERVER_NAME],
            json!({"command": "/bin/aise", "args": ["mcp", "serve"]})
        );
        assert_eq!(status_target(&target).unwrap(), "configured");

        fs::remove_file(plugin.join(".mcp.json")).unwrap();
        assert_eq!(status_target(&target).unwrap(), "stale");
        upsert_target(&target, Path::new("/bin/aise")).unwrap();

        // Installing the plugin from Codex's Plugins page records it under [plugins].
        let installed = format!(
            "{}\n[plugins.\"{SERVER_NAME}@{SERVER_NAME}\"]\nenabled = true\n",
            fs::read_to_string(&config).unwrap()
        );
        fs::write(&config, installed).unwrap();
        assert!(remove_target(&target).unwrap());
        let left = fs::read_to_string(&config).unwrap();
        assert!(!left.contains(SERVER_NAME), "{left}");
        assert!(left.contains("# mine") && left.contains("[existing]"));
        assert!(!marketplace.exists() && !plugin.join(".mcp.json").exists());
        assert_eq!(status_target(&target).unwrap(), "not configured");
    }

    #[test]
    fn codex_upsert_is_idempotent_and_preserves_other_sections() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[existing]\nvalue = true\n").unwrap();

        upsert_codex_mcp_server(&path, Path::new("/bin/aise")).unwrap();
        upsert_codex_mcp_server(&path, Path::new("/bin/aise")).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[existing]\nvalue = true"));
        assert_eq!(text.matches("[mcp_servers.ai-session-search]").count(), 1);
        assert!(text.contains("command = \"/bin/aise\""));
        assert!(text.contains("args = [\"mcp\", \"serve\"]"));
        assert!(!text.contains("startup_timeout_sec"));
    }

    #[test]
    fn status_distinguishes_stale_execution_contracts() {
        let dir = tempdir().unwrap();
        let json_path = dir.path().join("client.json");
        fs::write(
            &json_path,
            r#"{"mcpServers":{"aise":{"command":"aise-mcp","args":[]}}}"#,
        )
        .unwrap();
        assert_eq!(
            status_json_server(&json_path, ConfigFormat::JsonMcpServers).unwrap(),
            "stale"
        );

        let opencode_path = dir.path().join("opencode.json");
        fs::write(
            &opencode_path,
            r#"{"mcp":{"aise":{"command":["aise-mcp"],"enabled":true}}}"#,
        )
        .unwrap();
        assert_eq!(
            status_json_server(&opencode_path, ConfigFormat::OpenCode).unwrap(),
            "stale"
        );

        let codex_path = dir.path().join("config.toml");
        fs::write(
            &codex_path,
            "[mcp_servers.ai_session_search]\ncommand = \"aise-mcp\"\n",
        )
        .unwrap();
        assert_eq!(status_codex_mcp_server(&codex_path).unwrap(), "stale");

        upsert_codex_mcp_server(&codex_path, Path::new("aise")).unwrap();
        assert_eq!(status_codex_mcp_server(&codex_path).unwrap(), "configured");
    }

    #[test]
    fn explicit_mcp_binary_must_exist() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("missing-aise");

        let error = resolve_mcp_binary(Some(&missing)).unwrap_err();

        assert!(error.to_string().contains("does not exist"));
    }

    #[cfg(unix)]
    #[test]
    fn explicit_mcp_binary_must_be_executable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let binary = dir.path().join("aise tool's");
        fs::write(&binary, "not executable").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o644)).unwrap();

        let error = resolve_mcp_binary(Some(&binary)).unwrap_err().to_string();
        let expected_command = render_posix_shell_command(&[
            "chmod".to_string(),
            "+x".to_string(),
            binary.to_string_lossy().into_owned(),
        ])
        .unwrap();

        assert!(error.contains("not executable"), "{error}");
        assert!(error.contains(&expected_command), "{error}");
        assert!(error.contains("--binary"), "{error}");
    }

    #[test]
    fn explicit_mcp_binary_must_be_a_regular_file() {
        let dir = tempdir().unwrap();
        let not_a_file = dir.path().join("some-dir");
        fs::create_dir(&not_a_file).unwrap();

        let error = resolve_mcp_binary(Some(&not_a_file))
            .unwrap_err()
            .to_string();

        assert!(error.contains("is not a regular file"), "{error}");
        assert!(error.contains("--binary"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn implicit_mcp_binary_persists_the_detected_absolute_executable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let binary = dir.path().join("aise");
        fs::write(&binary, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();

        let resolved = resolve_detected_mcp_binary(Some(binary.clone())).unwrap();

        assert_eq!(resolved, binary);
        assert!(resolved.is_absolute());
    }

    #[test]
    fn codex_remove_handles_quoted_and_nested_tables_without_touching_other_content() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "# keep this comment\n[a]\nx = 1\n\n[mcp_servers.\"aise\"]\ncommand = \"/old\"\n\n[mcp_servers.\"aise\".env]\nTOKEN = \"managed\"\n\n[b]\ny = 2\n",
        )
        .unwrap();

        assert!(remove_codex_mcp_server(&path).unwrap());
        let output = fs::read_to_string(&path).unwrap();
        output.parse::<toml_edit::DocumentMut>().unwrap();
        assert!(output.contains("# keep this comment"));
        assert!(output.contains("[a]\nx = 1"));
        assert!(output.contains("[b]\ny = 2"));
        assert!(!output.contains("managed"));
        assert!(!output.contains("mcp_servers"));
    }

    #[test]
    fn codex_upsert_uses_toml_string_encoding_and_preserves_comments() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "# keep this comment\n[existing]\nvalue = true\n").unwrap();
        let binary = Path::new("C:\\Program Files\\aise\"quoted\nname.exe");

        upsert_codex_mcp_server(&path, binary).unwrap();

        let output = fs::read_to_string(&path).unwrap();
        let parsed: toml::Value = toml::from_str(&output).unwrap();
        assert_eq!(
            parsed["mcp_servers"][SERVER_NAME]["command"].as_str(),
            binary.to_str()
        );
        assert!(output.contains("# keep this comment"));
    }

    #[cfg(unix)]
    #[test]
    fn config_generation_rejects_non_utf8_binary_paths_instead_of_replacing_bytes() {
        use std::os::unix::ffi::OsStringExt;

        let dir = tempdir().unwrap();
        let target = Target {
            label: "custom",
            path: dir.path().join("mcp.json"),
            format: ConfigFormat::JsonMcpServers,
            detect_paths: Vec::new(),
            detect_binaries: Vec::new(),
            superseded_paths: Vec::new(),
            codex_marketplace: None,
        };
        let binary = PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/aise-\xff".to_vec()));

        let error = plan_upsert_target(&target, &binary)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not valid UTF-8"), "{error}");
        assert!(!target.path.exists());
    }

    #[test]
    fn inline_instruction_upsert_adds_replaces_and_stays_single() {
        let original = "# Team rules\n";
        let first =
            upsert_inline_instruction_text(original, InstructionFormat::InlineBlock).unwrap();
        assert!(first.contains(instruction_block(InstructionFormat::InlineBlock).trim_end()));
        assert!(first.contains("# Team rules"));

        let stale = first.replace(INSTRUCTIONS_LINE, "old wording");
        let updated =
            upsert_inline_instruction_text(&stale, InstructionFormat::InlineBlock).unwrap();
        assert!(updated.contains(INSTRUCTIONS_LINE));
        assert!(!updated.contains("old wording"));
        assert_eq!(updated.matches(INSTRUCTIONS_START).count(), 1);
    }

    #[test]
    fn inline_instruction_upsert_collapses_duplicates_and_uninstall_removes_every_block() {
        let duplicated = format!(
            "# Team rules\n\n{}Keep this.\n\n{}",
            instruction_block(InstructionFormat::InlineBlock),
            instruction_block(InstructionFormat::InlineBlock)
        );

        let updated =
            upsert_inline_instruction_text(&duplicated, InstructionFormat::InlineBlock).unwrap();
        assert_eq!(updated.matches(INSTRUCTIONS_START).count(), 1);
        assert_eq!(updated.matches(INSTRUCTIONS_END).count(), 1);
        assert!(updated.contains("# Team rules"));
        assert!(updated.contains("Keep this."));

        let removed = remove_inline_instruction_block(&duplicated)
            .unwrap()
            .unwrap();
        assert!(!removed.contains(INSTRUCTIONS_START));
        assert!(!removed.contains(INSTRUCTIONS_END));
        assert!(removed.contains("# Team rules"));
        assert!(removed.contains("Keep this."));
    }

    #[test]
    fn inline_instruction_remove_only_deletes_managed_block() {
        let input = format!(
            "# Team rules\n\n{}Keep this.\n",
            instruction_block(InstructionFormat::InlineBlock)
        );
        let output = remove_inline_instruction_block(&input).unwrap().unwrap();
        assert!(output.contains("# Team rules"));
        assert!(output.contains("Keep this."));
        assert!(output.contains("# Team rules\n\nKeep this."));
        assert!(!output.contains(INSTRUCTIONS_START));
    }

    #[test]
    fn inline_instruction_remove_rejects_malformed_block_with_a_concrete_fix() {
        // Start marker present, no matching end marker: must name what's missing and give a
        // copy-pastable recovery step, not just restate "malformed".
        let err = remove_inline_instruction_block("before\n<!-- aise-instructions v1 -->\npartial")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no matching `<!-- /aise-instructions -->`"),
            "{err}"
        );
        assert!(err.contains("rerun `aise integrations install`"), "{err}");
        assert!(!err.contains("without end marker"), "{err}");

        // End marker present, no start marker at all.
        let err = remove_inline_instruction_block("before\n<!-- /aise-instructions -->\nafter\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no matching `<!-- aise-instructions"), "{err}");
        assert!(err.contains("rerun `aise integrations install`"), "{err}");
        assert!(!err.contains("without start marker"), "{err}");

        // End marker appears before its start marker.
        let err = remove_inline_instruction_block(
            "before\n<!-- /aise-instructions -->\nmiddle\n<!-- aise-instructions v1 -->\nafter\n",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("before its matching `<!-- aise-instructions"),
            "{err}"
        );
        assert!(err.contains("rerun `aise integrations install`"), "{err}");
    }

    #[test]
    fn instruction_content_names_product_and_gives_an_exact_mcp_workflow() {
        let content = instruction_file_content();
        assert!(content.contains("AI Session Search (`aise`)"));
        for tool in [
            "`list_sessions(",
            "`search_sessions(",
            "`search_messages(",
            "`get_session(",
        ] {
            assert!(content.contains(tool), "missing {tool}: {content}");
        }
        assert!(
            content.contains("installed `ai-session-search` skill"),
            "the block names the skill that holds the full workflow: {content}"
        );
        assert!(
            content.contains("`aise <command> --help`"),
            "the block names the CLI fallback: {content}"
        );
        // Both blocks walk the same four steps in the same order, so an agent that reads the
        // MCP block on one harness and the CLI block on another meets one workflow.
        for line in [INSTRUCTIONS_LINE, CLI_INSTRUCTIONS_LINE] {
            let lower = line.to_ascii_lowercase();
            let positions: Vec<usize> = [
                "recent sessions",
                "by topic",
                "the exact turn",
                "then read it",
            ]
            .iter()
            .map(|phrase| {
                lower
                    .find(phrase)
                    .unwrap_or_else(|| panic!("missing {phrase:?}: {line}"))
            })
            .collect();
            assert!(
                positions.windows(2).all(|pair| pair[0] < pair[1]),
                "steps out of order: {line}"
            );
        }
        for provider in [
            "Claude Code",
            "Claude Desktop local agent",
            "Codex",
            "Cursor",
            "Antigravity",
            "Pi coding agent",
            "Prime Agent",
            "Google AI Studio",
            "Gemini CLI",
        ] {
            assert!(content.contains(provider), "missing {provider}: {content}");
        }
    }

    #[test]
    fn claude_instruction_uses_import_file_and_migrates_inline_block() {
        let dir = tempdir().unwrap();
        let claude_md = dir.path().join("CLAUDE.md");
        fs::write(
            &claude_md,
            format!(
                "# Team rules\n\n{}Keep this.\n",
                instruction_block(InstructionFormat::InlineBlock)
            ),
        )
        .unwrap();
        let target = InstructionTarget {
            label: "claude",
            path: claude_md.clone(),
            format: InstructionFormat::ClaudeImport,
            detect_paths: Vec::new(),
            detect_binaries: Vec::new(),
        };

        upsert_instruction_file(&target).unwrap();
        let claude_text = fs::read_to_string(&claude_md).unwrap();
        assert!(claude_text.contains(INSTRUCTIONS_REFERENCE));
        assert!(!claude_text.contains(INSTRUCTIONS_START));
        assert_eq!(
            fs::read_to_string(dir.path().join(INSTRUCTIONS_FILE)).unwrap(),
            instruction_file_content()
        );
        assert_eq!(status_instruction_file(&target).unwrap(), "configured");

        assert!(remove_instruction_file(&target).unwrap());
        let claude_text = fs::read_to_string(&claude_md).unwrap();
        assert!(claude_text.contains("# Team rules"));
        assert!(claude_text.contains("Keep this."));
        assert!(!claude_text.contains(INSTRUCTIONS_REFERENCE));
        assert!(!dir.path().join(INSTRUCTIONS_FILE).exists());
    }

    #[test]
    fn claude_instruction_upgrades_owned_legacy_file_but_refuses_unmanaged_content() {
        let dir = tempdir().unwrap();
        let claude_md = dir.path().join("CLAUDE.md");
        let instruction_path = dir.path().join(INSTRUCTIONS_FILE);
        fs::write(&claude_md, "# Team rules\n\n@AI_SESSION_SEARCH.md\n").unwrap();
        fs::write(&instruction_path, legacy_instruction_file_content()).unwrap();
        let target = InstructionTarget {
            label: "claude",
            path: claude_md,
            format: InstructionFormat::ClaudeImport,
            detect_paths: Vec::new(),
            detect_binaries: Vec::new(),
        };

        assert_eq!(status_instruction_file(&target).unwrap(), "outdated");
        upsert_instruction_file(&target).unwrap();
        assert_eq!(status_instruction_file(&target).unwrap(), "configured");
        assert_eq!(
            fs::read_to_string(&instruction_path).unwrap(),
            instruction_file_content()
        );

        fs::write(&instruction_path, "# User-owned notes\nDo not replace.\n").unwrap();
        assert_eq!(
            status_instruction_file(&target).unwrap(),
            "instruction file modified"
        );
        let error = upsert_instruction_file(&target).unwrap_err();
        assert!(error.to_string().contains("refusing to replace unmanaged"));
        assert_eq!(
            fs::read_to_string(instruction_path).unwrap(),
            "# User-owned notes\nDo not replace.\n"
        );
    }

    #[test]
    fn inline_instruction_status_distinguishes_current_and_outdated_content() {
        let dir = tempdir().unwrap();
        let agents_md = dir.path().join("AGENTS.md");
        let target = InstructionTarget {
            label: "codex",
            path: agents_md.clone(),
            format: InstructionFormat::InlineBlock,
            detect_paths: Vec::new(),
            detect_binaries: Vec::new(),
        };
        fs::write(
            &agents_md,
            instruction_block(InstructionFormat::InlineBlock),
        )
        .unwrap();
        assert_eq!(status_instruction_file(&target).unwrap(), "configured");
        fs::write(
            &agents_md,
            instruction_block(InstructionFormat::InlineBlock)
                .replace(INSTRUCTIONS_LINE, LEGACY_INSTRUCTIONS_LINE),
        )
        .unwrap();
        assert_eq!(status_instruction_file(&target).unwrap(), "outdated");
    }

    #[test]
    fn agents_instruction_uses_inline_block_not_import_reference() {
        let dir = tempdir().unwrap();
        let agents_md = dir.path().join("AGENTS.md");
        let target = InstructionTarget {
            label: "codex",
            path: agents_md.clone(),
            format: InstructionFormat::InlineBlock,
            detect_paths: Vec::new(),
            detect_binaries: Vec::new(),
        };

        upsert_instruction_file(&target).unwrap();
        let agents_text = fs::read_to_string(&agents_md).unwrap();
        assert!(agents_text.contains(INSTRUCTIONS_START));
        assert!(agents_text.contains(INSTRUCTIONS_LINE));
        assert!(!agents_text.contains(INSTRUCTIONS_REFERENCE));
        assert!(!dir.path().join(INSTRUCTIONS_FILE).exists());
        assert_eq!(status_instruction_file(&target).unwrap(), "configured");

        assert!(remove_instruction_file(&target).unwrap());
        let agents_text = fs::read_to_string(&agents_md).unwrap();
        assert!(!agents_text.contains(INSTRUCTIONS_START));
    }

    #[test]
    fn agents_instruction_migrates_old_import_reference() {
        let dir = tempdir().unwrap();
        let agents_md = dir.path().join("AGENTS.md");
        fs::write(&agents_md, "# Team rules\n\n@AI_SESSION_SEARCH.md\n").unwrap();
        fs::write(
            dir.path().join(INSTRUCTIONS_FILE),
            instruction_file_content(),
        )
        .unwrap();
        let target = InstructionTarget {
            label: "codex",
            path: agents_md.clone(),
            format: InstructionFormat::InlineBlock,
            detect_paths: Vec::new(),
            detect_binaries: Vec::new(),
        };

        upsert_instruction_file(&target).unwrap();
        let agents_text = fs::read_to_string(&agents_md).unwrap();
        assert!(agents_text.contains(INSTRUCTIONS_START));
        assert!(!agents_text.contains(INSTRUCTIONS_REFERENCE));
        assert!(!dir.path().join(INSTRUCTIONS_FILE).exists());
    }

    // PATTERN: the three dedupers share one contract. An identical duplicate is dropped
    // SILENTLY (that is intended, not a defect); an *incompatible* duplicate — the same
    // destination claimed two different ways — is an error naming both claims.
    // `dedupe_config_targets:552-575` keys `(path, format)`, `dedupe_instruction_targets:577-598`
    // keys `(path, format)`, and skills key `(path, label)` because `SkillTarget` carries a
    // label rather than a format. Only skills diverged, returning `()` and dropping both cases.
    #[test]
    fn dedupe_skill_targets_drops_true_duplicates_and_names_label_conflicts() {
        let root = PathBuf::from("/tmp/aise-dedupe-fixture/skills/ai-session-search");

        let mut duplicates = vec![
            skill_target("claude", root.clone(), Vec::new(), Vec::new()),
            skill_target("claude", root.clone(), Vec::new(), Vec::new()),
        ];
        dedupe_skill_targets(&mut duplicates).unwrap();
        assert_eq!(
            duplicates.len(),
            1,
            "the same root under the same label is a true duplicate and is dropped silently"
        );

        let mut conflicting = vec![
            skill_target("claude", root.clone(), Vec::new(), Vec::new()),
            skill_target("custom", root.clone(), Vec::new(), Vec::new()),
        ];
        let err = dedupe_skill_targets(&mut conflicting)
            .expect_err("one root claimed by two labels must not be silently dropped")
            .to_string();
        for expected in ["claude", "custom", "ai-session-search"] {
            assert!(
                err.contains(expected),
                "error must name both claims and the path; missing {expected:?} in {err:?}"
            );
        }
    }

    // The skill lifecycle test below uses `publish_planned_mutations`, a `#[cfg(test)]` helper
    // that writes each change directly. That bypasses the receipt, the lock, preimage
    // verification, and reverse-order rollback — i.e. everything that makes install recoverable,
    // and everything that must hold once install publishes a whole directory rather than one
    // file. This test drives the same plan through `execute_planned_transaction`, the path
    // production actually uses.
    #[test]
    fn skill_install_survives_the_real_transaction_path() {
        let dir = tempdir().unwrap();
        // No install record: these tests predate the manifest and exercise the
        // byte-comparison path a legacy install still takes.
        let absent = crate::skill_manifest::ManifestState::Absent;
        let receipt = dir.path().join("state/install-receipt.json");
        let root = dir.path().join("skills/corrections");
        let target = skill_target_for_package(
            "test",
            root.clone(),
            &AI_SESSION_SEARCH_SKILL_PACKAGE,
            Vec::new(),
            Vec::new(),
        );

        let mutations =
            normalize_planned_mutations(plan_upsert_skill_file(&target).unwrap()).unwrap();
        assert_eq!(
            mutations.len(),
            target.package.files.len(),
            "every managed file is published, not just the anchor"
        );
        execute_planned_transaction(&receipt, &mutations).unwrap();

        for file in target.package.files {
            assert_eq!(
                fs::read_to_string(root.join(file.relative_path)).unwrap(),
                file.content,
                "{} did not land",
                file.relative_path
            );
        }
        assert_eq!(status_skill_file(&target, &absent).unwrap(), "configured");

        // A skill missing one reference file is NOT `configured`: reporting the anchor alone
        // would call a half-installed directory healthy, which is what an interrupted upgrade
        // leaves behind. It is not `missing` either, which would say the package is absent when
        // every other file is current.
        fs::remove_file(root.join("references/message-classification.md")).unwrap();
        let partial = status_skill_file(&target, &absent).unwrap();
        assert!(
            partial.starts_with("incomplete: 1 of 5 managed files missing"),
            "a deleted managed file is reported with its count, the actionable fact, rather than \
             summarized away by the files that are still current or overstated as an absent \
             package: {partial}"
        );
        assert!(partial.contains("aise integrations install"), "{partial}");
        let repair = normalize_planned_mutations(plan_upsert_skill_file(&target).unwrap()).unwrap();
        assert_eq!(repair.len(), 1, "repair rewrites only the missing file");
        execute_planned_transaction(&receipt, &repair).unwrap();
        assert_eq!(status_skill_file(&target, &absent).unwrap(), "configured");
        assert!(
            !receipt.exists(),
            "a committed transaction must not leave its receipt behind"
        );

        // Re-planning against the now-current file yields nothing to do, so install is its own
        // idempotent repair (`is_noop`) even through the real path.
        let repeat = normalize_planned_mutations(plan_upsert_skill_file(&target).unwrap()).unwrap();
        assert!(
            repeat.is_empty(),
            "an already-current managed file must plan no mutation"
        );

        // ALL-OR-NOTHING across files. This is the assertion the `#[cfg(test)]` bypass cannot
        // satisfy and the reason this test exists: `publish_planned_mutations` applies changes one
        // at a time, so a later failure leaves earlier files already written. The real transaction
        // verifies every preimage before applying any change, so a doomed set writes nothing.
        //
        // Probed by swapping in `publish_planned_mutations`: the receipt and stale-preimage
        // assertions still passed (the bypass verifies preimages too, and never creates a
        // receipt to leave behind), so only this one actually distinguishes the two paths.
        let first = dir
            .path()
            .join("skills/ai-session-search/references/first.md");
        let second = dir
            .path()
            .join("skills/ai-session-search/references/second.md");
        fs::create_dir_all(second.parent().unwrap()).unwrap();
        fs::write(&second, "on disk\n").unwrap();

        let doomed = vec![
            planned_write(
                &first,
                &None,
                "written only if the set commits\n".to_string(),
            ),
            // Expects bytes that are not there, so the set as a whole must be refused.
            planned_write(
                &second,
                &Some("a preimage that no longer matches\n".to_string()),
                "never applied\n".to_string(),
            ),
        ];
        assert!(
            execute_planned_transaction(&receipt, &doomed).is_err(),
            "a set containing a stale preimage must be refused"
        );
        assert!(
            !first.exists(),
            "no file may be written when a later change in the same set is refused"
        );
        assert_eq!(
            fs::read_to_string(&second).unwrap(),
            "on disk\n",
            "the refused transaction must leave existing bytes untouched"
        );
        assert!(
            !receipt.exists(),
            "a refused transaction must not strand a receipt"
        );
    }

    #[test]
    fn managed_skill_content_uses_current_product_surface() {
        let skill_content = AI_SESSION_SEARCH_SKILL_FILES
            .iter()
            .find(|file| file.relative_path == SKILL_ANCHOR_FILE)
            .expect("the managed file list includes its own ownership anchor")
            .content;
        assert!(skill_content.starts_with("---\nname: ai-session-search\n"));
        assert!(skill_content.contains(SKILL_MANAGED_MARKER));
        assert!(skill_content.contains("aise messages search"));
        assert!(skill_content.contains("query_session_index"));
        assert!(skill_content.contains("short distinctive fragment"));
        assert!(skill_content.contains("Literal is the default: the query text as typed"));
        assert!(skill_content.contains("List recent sessions by metadata, then read them"));
        assert!(skill_content.contains("aise list --path ~/source/project --when 7d --limit 10"));
        assert!(skill_content.contains("one-significant-figure measurements"));
        assert!(skill_content.contains("Recover an edited file"));
        assert!(skill_content
            .contains("aise messages search \"foreign key\" --workspace-path ~/source/project"));
        assert!(skill_content.contains(
            "`--workspace-path` on\n   message search, `--path` on session commands and analytics"
        ));
        let costs = AI_SESSION_SEARCH_SKILL_FILES
            .iter()
            .find(|file| file.relative_path == "references/search-costs-and-receipts.md")
            .expect("the managed skill includes its cost and receipt reference")
            .content;
        assert!(costs.contains("O(T + N + W log W)"));
        assert!(costs.contains("`corpus`"));
        assert!(
            !skill_content.contains("aise messages search \"foreign key\" --path ~/source/project"),
            "the installed skill must use the command-specific message-search scope flag"
        );
        assert!(!skill_content.contains("aise messages inspect"));
        assert!(!skill_content.contains("aise tools search"));
        assert!(is_managed_skill_anchor(skill_content));

        let prior_work_recovery = AI_SESSION_SEARCH_SKILL_FILES
            .iter()
            .find(|file| file.relative_path == "references/recover-prior-work-with-evidence.md")
            .expect("the managed skill includes its prior-work evidence recovery workflow")
            .content;
        assert!(prior_work_recovery.contains("bounded evidence packet"));
        assert!(prior_work_recovery.contains("The harness starts and supervises the subagent"));
    }

    #[test]
    fn marker_quoted_in_a_skill_body_does_not_authorize_overwrite() {
        let dir = tempdir().unwrap();
        let parent = dir.path().join("skills");
        let general_root = parent.join("ai-session-search");
        let corrections_root = parent.join("corrections");
        fs::create_dir_all(&corrections_root).unwrap();
        let unmanaged = format!(
            "---\nname: corrections\ndescription: user owned\n---\n\n\
             This document mentions the marker without granting ownership:\n\
             ```html\n{SKILL_MANAGED_MARKER}\n```\n"
        );
        fs::write(corrections_root.join("SKILL.md"), &unmanaged).unwrap();
        let targets = vec![
            skill_target_for_package(
                "test",
                general_root.clone(),
                &AI_SESSION_SEARCH_SKILL_PACKAGE,
                vec![],
                vec![],
            ),
            skill_target_for_package(
                "test",
                corrections_root.clone(),
                &AI_SESSION_SEARCH_SKILL_PACKAGE,
                vec![],
                vec![],
            ),
        ];
        let manifest_path =
            crate::skill_manifest::manifest_path(&dir.path().join("state/config.toml"));

        let error = preflight_install(&[], &[], &targets, Path::new("aise"), Some(&manifest_path))
            .expect_err("body text must not spoof the managed ownership anchor");
        let message = format!("{error:#}");
        assert!(
            message.contains("refusing to write unmanaged AI Session Search skill")
                && message.contains("no managed marker and no install record"),
            "the refusal must name both missing ownership proofs: {message}"
        );
        assert!(!general_root.exists(), "preflight must publish nothing");
        assert_eq!(
            fs::read_to_string(corrections_root.join("SKILL.md")).unwrap(),
            unmanaged
        );
        assert!(!manifest_path.exists());
    }

    #[test]
    fn one_client_install_publishes_both_managed_skill_packages_in_one_transaction() {
        let dir = tempdir().unwrap();
        let layout = ClientLayout::new(
            dir.path().join("home"),
            dir.path().join("config"),
            ClientPlatform::Macos,
        );
        let targets = skill_targets_for_layout(McpClient::Claude, &layout);
        assert_eq!(
            targets
                .iter()
                .map(|target| target.package.name)
                .collect::<Vec<_>>(),
            vec!["ai-session-search"]
        );

        let receipt = dir.path().join("state/transaction.json");
        let manifest_path = crate::skill_manifest::manifest_path(&dir.path().join("config.toml"));
        let mutations =
            preflight_install(&[], &[], &targets, Path::new("aise"), Some(&manifest_path)).unwrap();
        assert_eq!(
            mutations.len(),
            MANAGED_SKILL_PACKAGES
                .iter()
                .map(|package| package.files.len())
                .sum::<usize>()
                + 1
        );
        execute_planned_transaction(&receipt, &mutations).unwrap();

        let manifest = crate::skill_manifest::load_manifest(&manifest_path).unwrap();
        for target in &targets {
            assert!(manifest.installation(&target.root).is_some());
            for file in target.package.files {
                assert_eq!(
                    fs::read_to_string(target.root.join(file.relative_path)).unwrap(),
                    file.content
                );
            }
        }
    }

    /// A routine integration reinstall must not act like the explicitly destructive
    /// `skills restore` path when one managed side file changed after installation.
    #[test]
    fn integration_reinstall_refuses_a_changed_managed_side_file() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("skills/ai-session-search");
        let target = skill_target_for_package(
            CUSTOM_SKILL_TARGET_LABEL,
            root.clone(),
            &AI_SESSION_SEARCH_SKILL_PACKAGE,
            vec![],
            vec![],
        );
        let config_path = dir.path().join("state/config.toml");
        let manifest_path = crate::skill_manifest::manifest_path(&config_path);
        let receipt = dir.path().join("state/install-transaction.json");

        let first = preflight_install(
            &[],
            &[],
            std::slice::from_ref(&target),
            Path::new("aise"),
            Some(&manifest_path),
        )
        .unwrap();
        execute_planned_transaction(&receipt, &first).unwrap();

        let policy = root.join("aise-capability.toml");
        fs::write(&policy, "my retained integration policy\n").unwrap();
        let manifest_before = fs::read(&manifest_path).unwrap();

        let error = preflight_install(
            &[],
            &[],
            std::slice::from_ref(&target),
            Path::new("aise"),
            Some(&manifest_path),
        )
        .expect_err("integration reinstall must refuse a changed managed file");
        let message = format!("{error:#}");
        assert!(
            message.contains("could destroy an edit you meant to keep")
                && message.contains("skills restore"),
            "the refusal must explain the retained edit and explicit repair path: {message}"
        );
        assert_eq!(
            fs::read_to_string(&policy).unwrap(),
            "my retained integration policy\n",
            "planning a reinstall must leave the edit untouched"
        );
        assert_eq!(
            fs::read(&manifest_path).unwrap(),
            manifest_before,
            "a refused reinstall must not rewrite its ownership evidence"
        );
        assert!(
            !receipt.exists(),
            "a refused reinstall must not publish a transaction receipt"
        );
    }

    #[test]
    fn retiring_the_sibling_package_refuses_when_one_of_its_files_was_edited() {
        let dir = tempdir().unwrap();
        let parent = dir.path().join("skills");
        let general_root = parent.join("ai-session-search");
        let corrections_root = parent.join(RETIRED_SKILL_PACKAGE_NAME);
        let targets = vec![skill_target_for_package(
            "test",
            general_root.clone(),
            &AI_SESSION_SEARCH_SKILL_PACKAGE,
            vec![],
            vec![],
        )];
        fs::create_dir_all(&general_root).unwrap();
        fs::write(
            general_root.join("SKILL.md"),
            AI_SESSION_SEARCH_SKILL_FILES[0].content,
        )
        .unwrap();

        let retired_skill_md = "---\nname: corrections\ndescription: retired\n---\n\
                                <!-- ai-session-search-managed-skill v1 -->\nbody\n";
        let recorded_capability = "schema_version = 1\n";
        fs::create_dir_all(&corrections_root).unwrap();
        fs::write(corrections_root.join("SKILL.md"), retired_skill_md).unwrap();
        // Recorded at install, then edited by the user afterwards.
        fs::write(
            corrections_root.join("capability.toml"),
            "schema_version = 1\n# my own rule\n",
        )
        .unwrap();

        let config_path = dir.path().join("state/config.toml");
        let manifest_path = crate::skill_manifest::manifest_path(&config_path);
        fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        let mut manifest = crate::skill_manifest::SkillInstallManifest::default();
        manifest.record(
            &corrections_root,
            &[
                ("SKILL.md".to_string(), retired_skill_md),
                ("capability.toml".to_string(), recorded_capability),
            ],
        );
        fs::write(&manifest_path, manifest.to_json().unwrap()).unwrap();

        let error = preflight_install(&[], &[], &targets, Path::new("aise"), Some(&manifest_path))
            .expect_err("an edited file in the retired package must block its removal");
        let message = format!("{error:#}");
        assert!(
            message.contains("capability.toml"),
            "the refusal must name the edited file: {message}"
        );
        assert!(
            corrections_root.join("capability.toml").is_file(),
            "the user's edit must survive"
        );
    }

    #[test]
    fn install_retires_the_sibling_corrections_package_and_historical_files() {
        let dir = tempdir().unwrap();
        let parent = dir.path().join("skills");
        let general_root = parent.join("ai-session-search");
        let corrections_root = parent.join(RETIRED_SKILL_PACKAGE_NAME);
        let targets = vec![skill_target_for_package(
            "test",
            general_root.clone(),
            &AI_SESSION_SEARCH_SKILL_PACKAGE,
            vec![],
            vec![],
        )];
        let historical_policy = "historical policy\n";
        let historical_reference = "historical reference\n";
        fs::create_dir_all(general_root.join("corrections")).unwrap();
        fs::create_dir_all(general_root.join("references")).unwrap();
        fs::write(
            general_root.join("SKILL.md"),
            AI_SESSION_SEARCH_SKILL_FILES[0].content,
        )
        .unwrap();
        fs::write(
            general_root.join("corrections/policy.toml"),
            historical_policy,
        )
        .unwrap();
        fs::write(
            general_root.join("references/corrections-policy.md"),
            historical_reference,
        )
        .unwrap();
        fs::write(general_root.join("references/my-notes.md"), "preserve me\n").unwrap();

        // A sibling package installed by an earlier release, marker and all.
        let retired_skill_md = "---\nname: corrections\ndescription: retired\n---\n                                <!-- ai-session-search-managed-skill v1 -->\nbody\n";
        let retired_capability = "schema_version = 1\n";
        fs::create_dir_all(&corrections_root).unwrap();
        fs::write(corrections_root.join("SKILL.md"), retired_skill_md).unwrap();
        fs::write(
            corrections_root.join("aise-capability.toml"),
            retired_capability,
        )
        .unwrap();

        let config_path = dir.path().join("state/config.toml");
        let manifest_path = crate::skill_manifest::manifest_path(&config_path);
        fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        let mut historical_manifest = crate::skill_manifest::SkillInstallManifest::default();
        historical_manifest.record(
            &general_root,
            &[
                (
                    "SKILL.md".to_string(),
                    AI_SESSION_SEARCH_SKILL_FILES[0].content,
                ),
                ("corrections/policy.toml".to_string(), historical_policy),
                (
                    "references/corrections-policy.md".to_string(),
                    historical_reference,
                ),
            ],
        );
        historical_manifest.record(
            &corrections_root,
            &[
                ("SKILL.md".to_string(), retired_skill_md),
                ("aise-capability.toml".to_string(), retired_capability),
            ],
        );
        fs::write(&manifest_path, historical_manifest.to_json().unwrap()).unwrap();

        let mutations =
            preflight_install(&[], &[], &targets, Path::new("aise"), Some(&manifest_path)).unwrap();
        execute_planned_transaction(&dir.path().join("state/transaction.json"), &mutations)
            .unwrap();

        assert!(!general_root.join("corrections/policy.toml").exists());
        assert!(!general_root
            .join("references/corrections-policy.md")
            .exists());
        assert_eq!(
            fs::read_to_string(general_root.join("references/my-notes.md")).unwrap(),
            "preserve me\n"
        );
        // Every file aise wrote is gone, so the directory is no longer a discoverable skill: the
        // Agent Skills specification keys discovery on SKILL.md. An emptied directory may remain,
        // which `prune_emptied_skill_directories` treats as untidy rather than as data loss worth
        // a recursive delete.
        assert!(
            !corrections_root.join("SKILL.md").exists(),
            "removing the anchor is what stops the retired package being discovered"
        );
        assert!(!corrections_root.join("aise-capability.toml").exists());
        assert_eq!(
            fs::read_dir(&corrections_root)
                .map(|entries| entries.count())
                .unwrap_or(0),
            0,
            "no aise-written file may survive in the retired package"
        );
        for file in AI_SESSION_SEARCH_SKILL_PACKAGE.files {
            assert_eq!(
                fs::read_to_string(general_root.join(file.relative_path)).unwrap(),
                file.content
            );
        }
        let migrated = crate::skill_manifest::load_manifest(&manifest_path).unwrap();
        let general = migrated.installation(&general_root).unwrap();
        assert_eq!(
            general
                .files
                .iter()
                .map(|file| file.relative_path.as_str())
                .collect::<Vec<_>>(),
            AI_SESSION_SEARCH_SKILL_PACKAGE
                .files
                .iter()
                .map(|file| file.relative_path)
                .collect::<Vec<_>>()
        );
        assert!(
            migrated.installation(&corrections_root).is_none(),
            "the retired root must be forgotten so a later uninstall does not claim it"
        );
    }

    #[test]
    fn package_update_retires_only_an_exact_recorded_removed_file() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("skills/ai-session-search");
        let target = skill_target_for_package(
            "test",
            root.clone(),
            &AI_SESSION_SEARCH_SKILL_PACKAGE,
            vec![],
            vec![],
        );
        for file in target.package.files {
            let path = root.join(file.relative_path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, file.content).unwrap();
        }
        let retired_relative = "references/delegated-session-research.md";
        let retired_content = "# Delegated session research\n";
        let retired_path = root.join(retired_relative);
        fs::write(&retired_path, retired_content).unwrap();

        let manifest_path =
            crate::skill_manifest::manifest_path(&dir.path().join("state/config.toml"));
        fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        let mut manifest = crate::skill_manifest::SkillInstallManifest::default();
        let mut recorded = target
            .package
            .files
            .iter()
            .map(|file| (file.relative_path.to_string(), file.content))
            .collect::<Vec<_>>();
        recorded.push((retired_relative.to_string(), retired_content));
        manifest.record(&root, &recorded);
        fs::write(&manifest_path, manifest.to_json().unwrap()).unwrap();

        let mutations = preflight_install(
            &[],
            &[],
            std::slice::from_ref(&target),
            Path::new("aise"),
            Some(&manifest_path),
        )
        .unwrap();
        assert!(mutations.iter().any(
            |mutation| matches!(mutation, PlannedFileMutation::Remove { path, .. } if path == &retired_path)
        ));
        execute_planned_transaction(&dir.path().join("state/transaction.json"), &mutations)
            .unwrap();

        assert!(!retired_path.exists());
        let installed = crate::skill_manifest::load_manifest(&manifest_path).unwrap();
        assert!(installed
            .installation(&root)
            .unwrap()
            .file(retired_relative)
            .is_none());
    }

    #[test]
    fn package_update_preserves_a_modified_removed_file_before_any_write() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("skills/ai-session-search");
        let target = skill_target_for_package(
            "test",
            root.clone(),
            &AI_SESSION_SEARCH_SKILL_PACKAGE,
            vec![],
            vec![],
        );
        for file in target.package.files {
            let path = root.join(file.relative_path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, file.content).unwrap();
        }
        let retired_relative = "references/delegated-session-research.md";
        let retired_path = root.join(retired_relative);
        fs::write(&retired_path, "# My preserved research workflow\n").unwrap();

        let manifest_path =
            crate::skill_manifest::manifest_path(&dir.path().join("state/config.toml"));
        fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        let mut manifest = crate::skill_manifest::SkillInstallManifest::default();
        let mut recorded = target
            .package
            .files
            .iter()
            .map(|file| (file.relative_path.to_string(), file.content))
            .collect::<Vec<_>>();
        recorded.push((
            retired_relative.to_string(),
            "# Delegated session research\n",
        ));
        manifest.record(&root, &recorded);
        let manifest_before = manifest.to_json().unwrap();
        fs::write(&manifest_path, &manifest_before).unwrap();

        let error = preflight_install(
            &[],
            &[],
            std::slice::from_ref(&target),
            Path::new("aise"),
            Some(&manifest_path),
        )
        .expect_err("a modified retired file must be preserved");
        assert!(format!("{error:#}").contains("preserving it"));
        assert_eq!(
            fs::read_to_string(&retired_path).unwrap(),
            "# My preserved research workflow\n"
        );
        assert_eq!(fs::read_to_string(&manifest_path).unwrap(), manifest_before);
    }

    #[test]
    fn package_update_rejects_an_unsafe_removed_manifest_path() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("skills/ai-session-search");
        let outside = root.parent().unwrap().join("outside.md");
        let target = skill_target_for_package(
            "test",
            root.clone(),
            &AI_SESSION_SEARCH_SKILL_PACKAGE,
            vec![],
            vec![],
        );
        for file in target.package.files {
            let path = root.join(file.relative_path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, file.content).unwrap();
        }
        fs::write(&outside, "must remain outside\n").unwrap();

        let manifest_path =
            crate::skill_manifest::manifest_path(&dir.path().join("state/config.toml"));
        fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        let mut manifest = crate::skill_manifest::SkillInstallManifest::default();
        let mut recorded = target
            .package
            .files
            .iter()
            .map(|file| (file.relative_path.to_string(), file.content))
            .collect::<Vec<_>>();
        recorded.push(("../outside.md".to_string(), "must remain outside\n"));
        manifest.record(&root, &recorded);
        fs::write(&manifest_path, manifest.to_json().unwrap()).unwrap();

        let error = preflight_install(
            &[],
            &[],
            std::slice::from_ref(&target),
            Path::new("aise"),
            Some(&manifest_path),
        )
        .expect_err("a manifest path must never escape its skill root");
        assert!(format!("{error:#}").contains("invalid managed skill path"));
        assert_eq!(
            fs::read_to_string(outside).unwrap(),
            "must remain outside\n"
        );
    }

    #[test]
    fn package_split_refuses_modified_historical_files_before_any_write() {
        let dir = tempdir().unwrap();
        let parent = dir.path().join("skills");
        let general_root = parent.join("ai-session-search");
        let corrections_root = parent.join("corrections");
        fs::create_dir_all(general_root.join("corrections")).unwrap();
        fs::write(
            general_root.join("SKILL.md"),
            AI_SESSION_SEARCH_SKILL_FILES[0].content,
        )
        .unwrap();
        fs::write(
            general_root.join("corrections/policy.toml"),
            "edited after install\n",
        )
        .unwrap();

        let config_path = dir.path().join("state/config.toml");
        let manifest_path = crate::skill_manifest::manifest_path(&config_path);
        fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        let mut historical_manifest = crate::skill_manifest::SkillInstallManifest::default();
        historical_manifest.record(
            &general_root,
            &[
                (
                    "SKILL.md".to_string(),
                    AI_SESSION_SEARCH_SKILL_FILES[0].content,
                ),
                (
                    "corrections/policy.toml".to_string(),
                    "recorded historical bytes\n",
                ),
            ],
        );
        let manifest_before = historical_manifest.to_json().unwrap();
        fs::write(&manifest_path, &manifest_before).unwrap();
        let skill_before = fs::read_to_string(general_root.join("SKILL.md")).unwrap();
        let targets = vec![
            skill_target_for_package(
                "test",
                general_root.clone(),
                &AI_SESSION_SEARCH_SKILL_PACKAGE,
                vec![],
                vec![],
            ),
            skill_target_for_package(
                "test",
                corrections_root.clone(),
                &AI_SESSION_SEARCH_SKILL_PACKAGE,
                vec![],
                vec![],
            ),
        ];

        let error = preflight_install(&[], &[], &targets, Path::new("aise"), Some(&manifest_path))
            .expect_err("modified historical bytes must block the split");
        assert!(format!("{error:#}").contains("differs from the install manifest"));
        assert_eq!(
            fs::read_to_string(general_root.join("corrections/policy.toml")).unwrap(),
            "edited after install\n"
        );
        assert_eq!(
            fs::read_to_string(general_root.join("SKILL.md")).unwrap(),
            skill_before
        );
        assert!(!corrections_root.exists());
        assert_eq!(fs::read_to_string(&manifest_path).unwrap(), manifest_before);
    }

    #[test]
    fn skill_update_finishes_a_pending_split_when_current_files_are_already_present() {
        let dir = tempdir().unwrap();
        let parent = dir.path().join("skills");
        let general_root = parent.join("ai-session-search");
        let corrections_root = parent.join("corrections");
        for (root, package) in [
            (&general_root, &AI_SESSION_SEARCH_SKILL_PACKAGE),
            (&corrections_root, &AI_SESSION_SEARCH_SKILL_PACKAGE),
        ] {
            for file in package.files {
                let path = root.join(file.relative_path);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, file.content).unwrap();
            }
        }
        let historical_policy = "historical policy\n";
        fs::create_dir_all(general_root.join("corrections")).unwrap();
        fs::write(
            general_root.join("corrections/policy.toml"),
            historical_policy,
        )
        .unwrap();

        let config_path = dir.path().join("state/config.toml");
        let manifest_path = crate::skill_manifest::manifest_path(&config_path);
        fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        let mut manifest = crate::skill_manifest::SkillInstallManifest::default();
        manifest.record(
            &general_root,
            &[
                (
                    "SKILL.md".to_string(),
                    AI_SESSION_SEARCH_SKILL_FILES[0].content,
                ),
                ("corrections/policy.toml".to_string(), historical_policy),
            ],
        );
        manifest.record(
            &corrections_root,
            &AI_SESSION_SEARCH_SKILL_PACKAGE
                .files
                .iter()
                .map(|file| (file.relative_path.to_string(), file.content))
                .collect::<Vec<_>>(),
        );
        fs::write(&manifest_path, manifest.to_json().unwrap()).unwrap();

        let dry_run = write_owned_skills(
            std::slice::from_ref(&general_root),
            &config_path,
            true,
            false,
        )
        .unwrap();
        assert!(dry_run.iter().any(|outcome| {
            outcome.root == corrections_root.display().to_string()
                && outcome.action == "would remove retired package"
        }));
        assert!(general_root.join("corrections/policy.toml").exists());
        assert!(corrections_root.exists());

        let outcomes = write_owned_skills(
            std::slice::from_ref(&general_root),
            &config_path,
            false,
            false,
        )
        .unwrap();
        assert!(
            outcomes.iter().all(|outcome| outcome.problem.is_none()),
            "{outcomes:#?}"
        );
        assert!(!general_root.join("corrections/policy.toml").exists());
        assert!(!corrections_root.exists());
        let migrated = crate::skill_manifest::load_manifest(&manifest_path).unwrap();
        assert_eq!(
            migrated.installation(&general_root).unwrap().files.len(),
            AI_SESSION_SEARCH_SKILL_PACKAGE.files.len()
        );
    }

    /// `update` never destroys an edit; `restore` does, and that is the whole difference.
    ///
    /// A routine command that silently replaced a changed file would be the surprise `restore`
    /// exists to make explicit and opt-in.
    #[test]
    fn update_refuses_a_changed_file_and_restore_is_the_named_repair() {
        let dir = tempdir().unwrap();
        let receipt = dir.path().join("receipt.json");
        let root = dir.path().join("skills/ai-session-search");
        let roots = vec![root.clone()];
        let policy = root.join("aise-capability.toml");
        let user_file = root.join("references/my-notes.md");

        // First write creates the tree and records it.
        let outcomes = write_owned_skills(&roots, &receipt, false, false).unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].problem.is_none(), "{outcomes:#?}");
        fs::write(&user_file, "mine").unwrap();

        // Already current: nothing to do, and it says so rather than rewriting.
        let outcomes = write_owned_skills(&roots, &receipt, false, false).unwrap();
        assert_eq!(outcomes[0].action, "already current");

        fs::write(&policy, "my own rules\n").unwrap();
        let outcomes = write_owned_skills(&roots, &receipt, false, false).unwrap();
        let problem = outcomes[0]
            .problem
            .as_deref()
            .expect("a changed file must be refused by update");
        assert!(
            problem.contains("could destroy an edit you meant to keep")
                && problem.contains("skills restore"),
            "the refusal must name the repair path: {problem}"
        );
        assert_eq!(
            fs::read_to_string(&policy).unwrap(),
            "my own rules\n",
            "update must not have touched it"
        );

        // Dry-run restore reports the rewrite without performing it.
        let outcomes = write_owned_skills(&roots, &receipt, true, true).unwrap();
        assert_eq!(outcomes[0].action, "would rewrite from modified or damaged");
        assert_eq!(fs::read_to_string(&policy).unwrap(), "my own rules\n");

        // Restore overwrites the managed file and leaves the user's own file alone.
        let outcomes = write_owned_skills(&roots, &receipt, false, true).unwrap();
        assert_eq!(outcomes[0].action, "rewritten from modified or damaged");
        assert_eq!(
            fs::read_to_string(&policy).unwrap(),
            AI_SESSION_SEARCH_SKILL_FILES
                .iter()
                .find(|file| file.relative_path == "aise-capability.toml")
                .unwrap()
                .content
        );
        assert_eq!(
            fs::read_to_string(&user_file).unwrap(),
            "mine",
            "a file aise did not write is never rewritten, even by restore"
        );
    }

    /// Installs the packaged skill at `root`, then makes it look like an older release installed
    /// it: the policy file and its manifest record both hold older bytes nobody edited.
    fn install_skill_as_an_older_release(receipt: &Path, root: &Path) -> PathBuf {
        let outcomes = write_owned_skills(
            std::slice::from_ref(&root.to_path_buf()),
            receipt,
            false,
            false,
        )
        .unwrap();
        assert!(outcomes[0].problem.is_none(), "{outcomes:#?}");
        let manifest_path = crate::skill_manifest::manifest_path(receipt);
        let policy = root.join("aise-capability.toml");
        let older = "# an older shipped policy\nschema_version = 1\n";
        fs::write(&policy, older).unwrap();
        let mut recorded = crate::skill_manifest::load_manifest(&manifest_path)
            .unwrap()
            .writable_manifest(&manifest_path)
            .unwrap();
        recorded.record(
            root,
            &AI_SESSION_SEARCH_SKILL_FILES
                .iter()
                .map(|file| {
                    let content = if file.relative_path == "aise-capability.toml" {
                        older
                    } else {
                        file.content
                    };
                    (file.relative_path.to_string(), content)
                })
                .collect::<Vec<_>>(),
        );
        fs::write(&manifest_path, recorded.to_json().unwrap()).unwrap();
        policy
    }

    fn embedded_policy() -> &'static str {
        AI_SESSION_SEARCH_SKILL_FILES
            .iter()
            .find(|file| file.relative_path == "aise-capability.toml")
            .unwrap()
            .content
    }

    #[test]
    fn an_upgrade_refreshes_an_unedited_skill_once_without_a_command() {
        // Upgrading through uv, pip, Cargo, or a native archive left the skill outdated until the
        // user ran `aise integrations install`; only `aise package update` refreshed it.
        let dir = tempdir().unwrap();
        let receipt = default_transaction_receipt(&dir.path().join("config.toml"));
        let root = dir.path().join("skills/ai-session-search");
        let policy = install_skill_as_an_older_release(&receipt, &root);

        let UpgradeRefresh::Checked {
            skills: outcomes, ..
        } = refresh_integrations_for_version(&receipt, "9.9.9", None).unwrap()
        else {
            panic!("an outdated install must be refreshed");
        };
        assert_eq!(outcomes.len(), 1, "{outcomes:#?}");
        assert_eq!(outcomes[0].action, "rewritten from outdated, untouched");
        assert_eq!(fs::read_to_string(&policy).unwrap(), embedded_policy());

        assert!(
            matches!(
                refresh_integrations_for_version(&receipt, "9.9.9", None).unwrap(),
                UpgradeRefresh::NotNeeded
            ),
            "a version is refreshed once, not on every command"
        );
        assert!(
            matches!(
                refresh_integrations_for_version(&receipt, "9.9.10", None).unwrap(),
                UpgradeRefresh::Checked { .. }
            ),
            "the next version checks again"
        );
    }

    #[test]
    fn an_upgrade_leaves_an_edited_skill_alone_and_reports_it_once() {
        let dir = tempdir().unwrap();
        let receipt = default_transaction_receipt(&dir.path().join("config.toml"));
        let root = dir.path().join("skills/ai-session-search");
        let policy = install_skill_as_an_older_release(&receipt, &root);
        fs::write(&policy, "# my own rules\nschema_version = 1\n").unwrap();

        let UpgradeRefresh::Checked {
            skills: outcomes, ..
        } = refresh_integrations_for_version(&receipt, "9.9.9", None).unwrap()
        else {
            panic!("the edited root must be checked and reported");
        };
        assert!(outcomes[0].problem.is_some(), "{outcomes:#?}");
        assert_eq!(
            fs::read_to_string(&policy).unwrap(),
            "# my own rules\nschema_version = 1\n",
            "an automatic refresh must never overwrite an edit"
        );
        assert!(matches!(
            refresh_integrations_for_version(&receipt, "9.9.9", None).unwrap(),
            UpgradeRefresh::NotNeeded
        ));
    }

    #[test]
    fn status_json_carries_each_line_with_whether_it_matches_this_build() {
        // Scripts read `current` instead of parsing the words after the colon, which differ by
        // component and have changed between releases.
        let skill = IntegrationStatusEntry::new(
            StatusComponent::Skill,
            Some("claude"),
            Path::new("/h/skills/ai-session-search"),
            ManagedFileState::Current.label(),
        );
        assert_eq!(
            skill.line(),
            "claude /h/skills/ai-session-search: configured"
        );
        assert_eq!(
            serde_json::to_value(&skill).unwrap(),
            json!({
                "component": "skill",
                "client": "claude",
                "path": "/h/skills/ai-session-search",
                "state": "configured",
                "current": true
            })
        );
        let outdated = IntegrationStatusEntry::new(
            StatusComponent::Instructions,
            Some("codex"),
            Path::new("/h/.codex/AGENTS.md"),
            "outdated",
        );
        assert!(!outdated.current);
        assert_eq!(outdated.line(), "codex /h/.codex/AGENTS.md: outdated");
        let alias = IntegrationStatusEntry::new(
            StatusComponent::ExecutableAlias,
            None,
            Path::new("/bin/aisearch"),
            "configured",
        );
        assert_eq!(alias.line(), "executable alias /bin/aisearch: configured");
        assert_eq!(serde_json::to_value(&alias).unwrap()["client"], Value::Null);

        let dir = tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        let link = dir.path().join("link");
        let (state, current) = status_skill_discovery_link(&link, &root).unwrap();
        assert_eq!((state.as_str(), current), ("missing", false));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&root, &link).unwrap();
            let (state, current) = status_skill_discovery_link(&link, &root).unwrap();
            assert_eq!(state, format!("linked -> {}", root.display()));
            assert!(current);
        }
    }

    #[test]
    fn changing_instruction_text_keeps_the_old_text_refreshable() {
        // Instruction blocks have no install record, so the post-upgrade refresh recognizes a
        // block aise wrote only by its text. Changing either line below without recording the old
        // one would leave every installed block "outdated" until the user ran
        // `aise integrations install`, the manual upgrade step the refresh exists to remove.
        // Before changing the text, add the old line's digest to
        // `EARLIER_INSTRUCTION_LINE_DIGESTS`, then update the pin here.
        assert_eq!(
            crate::hashing::sha256(INSTRUCTIONS_LINE.as_bytes()),
            "80e00c44e12fb57a3ba7cebd804880a7ce28ebd004e7442a0415ea32cb5399ab"
        );
        assert_eq!(
            crate::hashing::sha256(CLI_INSTRUCTIONS_LINE.as_bytes()),
            "a7bfbb5efc4fd40b0268ce38c0091e17844e2edddd9b8cdfbf7be91588c54442"
        );
        assert!(is_aise_instruction_line(RC1_INSTRUCTIONS_LINE));
    }

    /// The line 1.0.0rc1 wrote into every instruction block, including Pi's.
    const RC1_INSTRUCTIONS_LINE: &str = "Before guessing about prior AI work, use AI Session Search (`aise`): call the `ai-session-search` MCP `search_sessions` tool to find relevant sessions or `search_messages` for message-level matches, then pass a returned session ID to `get_session`. It searches Claude Code, Claude Desktop local agent, Codex, Cursor, Antigravity, Pi coding agent, Google AI Studio, and Gemini CLI by query, repo/path/file, message context, and time range. If MCP is unavailable, run `aise messages search --help`.";

    /// A home holding what 1.0.0rc1 installed for Claude Code, Codex, and Pi, plus the recorded
    /// skill that shows aise installed integrations with this configuration.
    fn home_with_rc1_instructions(dir: &Path) -> (PathBuf, ClientLayout) {
        let receipt = default_transaction_receipt(&dir.join("config.toml"));
        install_skill_as_an_older_release(&receipt, &dir.join("skills/ai-session-search"));
        let home = dir.join("home");
        let rc1_block =
            format!("<!-- aise-instructions v1 -->\n{RC1_INSTRUCTIONS_LINE}\n{INSTRUCTIONS_END}");
        for agents in [
            home.join(".codex/AGENTS.md"),
            home.join(".pi/agent/AGENTS.md"),
        ] {
            fs::create_dir_all(agents.parent().unwrap()).unwrap();
            fs::write(&agents, format!("# Mine\n\n{rc1_block}\n\n# Also mine\n")).unwrap();
        }
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(
            home.join(".claude/CLAUDE.md"),
            "# Mine\n\n@AI_SESSION_SEARCH.md\n",
        )
        .unwrap();
        fs::write(
            home.join(".claude/AI_SESSION_SEARCH.md"),
            format!(
                "# AI Session Search (`aise`)\n\n{INSTRUCTIONS_FILE_START}\n{RC1_INSTRUCTIONS_LINE}\n{INSTRUCTIONS_FILE_END}\n"
            ),
        )
        .unwrap();
        let layout = ClientLayout::new(home.clone(), home.join(".config"), ClientPlatform::Linux);
        (receipt, layout)
    }

    /// An upgrade needs nothing beyond replacing the executable (scripts/verify_upgrade_path.py),
    /// so an MCP entry an earlier release wrote where its client no longer reads it is moved by the
    /// first command after the upgrade, keeping the executable that entry names.
    #[test]
    fn an_upgrade_moves_mcp_entries_out_of_places_their_client_no_longer_reads() {
        let dir = tempdir().unwrap();
        let (receipt, layout) = home_with_rc1_instructions(dir.path());
        let home = layout.home.clone();
        let opencode = home.join(".config/opencode/opencode.json");
        fs::create_dir_all(opencode.parent().unwrap()).unwrap();
        fs::write(
            &opencode,
            r#"{"mcp":{"ai-session-search":{"command":["/custom/aise","mcp","serve"],"enabled":true}}}"#,
        )
        .unwrap();
        let openclaw = home.join(".openclaw/openclaw.json");
        fs::create_dir_all(openclaw.parent().unwrap()).unwrap();
        fs::write(
            &openclaw,
            "{ gateway: { port: 1 }, mcpServers: { 'ai-session-search': { command: '/custom/aise', args: ['mcp', 'serve'] } } }\n",
        )
        .unwrap();

        let UpgradeRefresh::Checked { mcp, .. } =
            refresh_integrations_for_version(&receipt, "9.9.9", Some(&layout)).unwrap()
        else {
            panic!("the recorded install must be checked");
        };
        assert_eq!(mcp, [opencode.clone(), openclaw.clone()]);
        for client in [McpClient::Opencode, McpClient::Openclaw] {
            let target = targets_for_layout(client, &layout).remove(0);
            assert_eq!(status_target(&target).unwrap(), "configured", "{client:?}");
        }
        let data: Value = serde_json::from_str(&fs::read_to_string(&opencode).unwrap()).unwrap();
        assert_eq!(
            data["mcp"]["servers"][SERVER_NAME]["command"][0], "/custom/aise",
            "the executable the earlier entry named is kept"
        );
        assert!(
            !home.join(".config/zed/settings.json").exists(),
            "a client with no aise entry is never configured by an upgrade"
        );
    }

    /// An earlier release registered only `[mcp_servers]` with Codex; the first command after the
    /// upgrade adds aise's plugin marketplace with the executable that entry names, while a home
    /// whose Codex config has no aise entry is left without one.
    #[test]
    fn an_upgrade_adds_the_codex_marketplace_to_an_existing_codex_entry() {
        let dir = tempdir().unwrap();
        let (receipt, layout) = home_with_rc1_instructions(dir.path());
        let config = layout.home.join(".codex/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(
            &config,
            "[mcp_servers.ai-session-search]\ncommand = \"/custom/aise\"\nargs = [\"mcp\", \"serve\"]\n",
        )
        .unwrap();

        let UpgradeRefresh::Checked { mcp, .. } =
            refresh_integrations_for_version(&receipt, "9.9.9", Some(&layout)).unwrap()
        else {
            panic!("the recorded install must be checked");
        };
        assert_eq!(mcp, std::slice::from_ref(&config));
        let mut targets = targets_for_layout(McpClient::Codex, &layout);
        attach_codex_marketplace(&mut targets, &receipt);
        assert_eq!(status_target(&targets[0]).unwrap(), "configured");
        let servers = app_root_of(&receipt)
            .join("codex-marketplace/plugins")
            .join(SERVER_NAME)
            .join(".mcp.json");
        assert!(fs::read_to_string(servers)
            .unwrap()
            .contains("/custom/aise"));

        let empty = tempdir().unwrap();
        let (receipt, layout) = home_with_rc1_instructions(empty.path());
        refresh_integrations_for_version(&receipt, "9.9.9", Some(&layout)).unwrap();
        assert!(!app_root_of(&receipt).join("codex-marketplace").exists());
    }

    #[test]
    fn an_upgrade_replaces_instruction_blocks_an_earlier_release_wrote_where_they_stand() {
        // 1.0.0rc2 changed the instruction text, so every block 1.0.0rc1 installed read
        // "outdated" until the user ran `aise integrations install`.
        let dir = tempdir().unwrap();
        let (receipt, layout) = home_with_rc1_instructions(dir.path());

        let UpgradeRefresh::Checked { instructions, .. } =
            refresh_integrations_for_version(&receipt, "9.9.9", Some(&layout)).unwrap()
        else {
            panic!("the recorded install must be checked");
        };
        let home = &layout.home;
        assert_eq!(
            instructions,
            [
                InstructionRefresh::Updated(home.join(".claude/AI_SESSION_SEARCH.md")),
                InstructionRefresh::Updated(home.join(".codex/AGENTS.md")),
                InstructionRefresh::Updated(home.join(".pi/agent/AGENTS.md")),
            ]
        );
        for client in [McpClient::Claude, McpClient::Codex, McpClient::Pi] {
            for target in instruction_targets_for_layout(client, &layout) {
                assert_eq!(
                    status_instruction_file(&target).unwrap(),
                    "configured",
                    "{client:?}"
                );
            }
        }
        assert_eq!(
            fs::read_to_string(home.join(".codex/AGENTS.md")).unwrap(),
            format!(
                "# Mine\n\n{}\n\n# Also mine\n",
                instruction_block(InstructionFormat::InlineBlock).trim_end()
            ),
            "the block is replaced where it stands and the user's text around it is kept"
        );
        assert_eq!(
            fs::read_to_string(home.join(".claude/CLAUDE.md")).unwrap(),
            "# Mine\n\n@AI_SESSION_SEARCH.md\n"
        );
        assert!(matches!(
            refresh_integrations_for_version(&receipt, "9.9.9", Some(&layout)).unwrap(),
            UpgradeRefresh::NotNeeded
        ));
    }

    #[test]
    fn an_upgrade_keeps_crlf_line_endings_and_status_reads_crlf_blocks_as_current() {
        // Git and editors on Windows often store these files with CRLF. The refresh rewrote such
        // a block with LF lines inside a CRLF file, and status called a current CRLF block
        // "outdated" or a current AI_SESSION_SEARCH.md edited.
        let dir = tempdir().unwrap();
        let (receipt, layout) = home_with_rc1_instructions(dir.path());
        let codex = layout.home.join(".codex/AGENTS.md");
        let claude_file = layout.home.join(".claude/AI_SESSION_SEARCH.md");
        for path in [&codex, &claude_file] {
            let crlf = fs::read_to_string(path).unwrap().replace('\n', "\r\n");
            fs::write(path, crlf).unwrap();
        }
        for target in instruction_targets_for_layout(McpClient::Claude, &layout) {
            assert_eq!(status_instruction_file(&target).unwrap(), "outdated");
        }

        let UpgradeRefresh::Checked { instructions, .. } =
            refresh_integrations_for_version(&receipt, "9.9.9", Some(&layout)).unwrap()
        else {
            panic!("the recorded install must be checked");
        };
        assert!(instructions.contains(&InstructionRefresh::Updated(codex.clone())));
        assert!(instructions.contains(&InstructionRefresh::Updated(claude_file.clone())));
        for path in [&codex, &claude_file] {
            let text = fs::read_to_string(path).unwrap();
            assert!(!text.replace("\r\n", "").contains('\n'), "{text:?}");
        }
        for client in [McpClient::Claude, McpClient::Codex] {
            for target in instruction_targets_for_layout(client, &layout) {
                assert_eq!(
                    status_instruction_file(&target).unwrap(),
                    "configured",
                    "{client:?}"
                );
            }
        }
    }

    #[test]
    fn an_upgrade_leaves_an_edited_instruction_block_alone_and_reports_it() {
        let dir = tempdir().unwrap();
        let (receipt, layout) = home_with_rc1_instructions(dir.path());
        let codex = layout.home.join(".codex/AGENTS.md");
        let edited = fs::read_to_string(&codex)
            .unwrap()
            .replace(RC1_INSTRUCTIONS_LINE, "Search my sessions with aise first.");
        fs::write(&codex, &edited).unwrap();

        let UpgradeRefresh::Checked { instructions, .. } =
            refresh_integrations_for_version(&receipt, "9.9.9", Some(&layout)).unwrap()
        else {
            panic!("the recorded install must be checked");
        };
        assert!(instructions.contains(&InstructionRefresh::Edited(codex.clone())));
        assert_eq!(fs::read_to_string(&codex).unwrap(), edited);
    }

    #[test]
    fn an_upgrade_reads_no_client_file_without_a_recorded_install() {
        // A process that runs with its own empty configuration, as every test does, must not
        // rewrite the client files of whoever runs it.
        let dir = tempdir().unwrap();
        let (_, layout) = home_with_rc1_instructions(dir.path());
        let receipt = default_transaction_receipt(&dir.path().join("other/config.toml"));
        let codex = layout.home.join(".codex/AGENTS.md");
        let before = fs::read_to_string(&codex).unwrap();

        assert!(matches!(
            refresh_integrations_for_version(&receipt, "9.9.9", Some(&layout)).unwrap(),
            UpgradeRefresh::NotNeeded
        ));
        assert_eq!(fs::read_to_string(&codex).unwrap(), before);
    }

    #[test]
    fn an_upgrade_does_not_restore_a_skill_the_user_deleted() {
        let dir = tempdir().unwrap();
        let receipt = default_transaction_receipt(&dir.path().join("config.toml"));
        let root = dir.path().join("skills/ai-session-search");
        install_skill_as_an_older_release(&receipt, &root);
        fs::remove_dir_all(&root).unwrap();

        let UpgradeRefresh::Checked { skills, .. } =
            refresh_integrations_for_version(&receipt, "9.9.9", None).unwrap()
        else {
            panic!("the recorded install must still be checked");
        };
        assert!(skills.is_empty(), "{skills:#?}");
        assert!(!root.exists(), "a deleted skill must stay deleted");
    }

    #[test]
    fn an_older_aise_never_downgrades_a_skill_a_newer_one_refreshed() {
        // With an old Cargo build on PATH beside a newer uv install that MCP clients launch,
        // alternating commands would otherwise rewrite the skill back and forth.
        let dir = tempdir().unwrap();
        let receipt = default_transaction_receipt(&dir.path().join("config.toml"));
        let root = dir.path().join("skills/ai-session-search");
        let policy = install_skill_as_an_older_release(&receipt, &root);
        assert!(matches!(
            refresh_integrations_for_version(&receipt, "1.0.0-rc.5", None).unwrap(),
            UpgradeRefresh::Checked { .. }
        ));
        let refreshed = fs::read_to_string(&policy).unwrap();
        fs::write(&policy, "# an older shipped policy\nschema_version = 1\n").unwrap();

        assert!(matches!(
            refresh_integrations_for_version(&receipt, "1.0.0-rc.4", None).unwrap(),
            UpgradeRefresh::NotNeeded
        ));
        assert_ne!(
            fs::read_to_string(&policy).unwrap(),
            refreshed,
            "left for the newer version"
        );
    }

    #[test]
    fn a_failed_refresh_is_retried_by_the_next_command() {
        // The marker records a completed check. Written after a failure, it would silence the
        // problem until the next release.
        let dir = tempdir().unwrap();
        let receipt = default_transaction_receipt(&dir.path().join("config.toml"));
        let root = dir.path().join("skills/ai-session-search");
        install_skill_as_an_older_release(&receipt, &root);
        let manifest_path = crate::skill_manifest::manifest_path(&receipt);
        fs::write(&manifest_path, "{ not json").unwrap();

        assert!(refresh_integrations_for_version(&receipt, "9.9.9", None).is_err());
        assert!(!crate::skill_manifest::refreshed_version_path(&receipt).exists());
        assert!(refresh_integrations_for_version(&receipt, "9.9.9", None).is_err());
    }

    #[test]
    fn an_upgrade_with_no_installed_skill_writes_nothing() {
        let dir = tempdir().unwrap();
        let receipt = default_transaction_receipt(&dir.path().join("config.toml"));

        assert!(matches!(
            refresh_integrations_for_version(&receipt, "9.9.9", None).unwrap(),
            UpgradeRefresh::NotNeeded
        ));
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            0,
            "a user who never installed integrations gets no files"
        );
    }

    #[test]
    fn an_upgrade_defers_while_another_aise_holds_the_integration_lock() {
        let dir = tempdir().unwrap();
        let receipt = default_transaction_receipt(&dir.path().join("config.toml"));
        let root = dir.path().join("skills/ai-session-search");
        let policy = install_skill_as_an_older_release(&receipt, &root);
        let mut lock =
            crate::durable_fs::open_file_lock(&crate::text_file_transaction::lock_path(&receipt))
                .unwrap();
        let guard = lock.try_write().unwrap();

        assert!(matches!(
            refresh_integrations_for_version(&receipt, "9.9.9", None).unwrap(),
            UpgradeRefresh::Deferred
        ));
        drop(guard);
        assert!(
            matches!(
                refresh_integrations_for_version(&receipt, "9.9.9", None).unwrap(),
                UpgradeRefresh::Checked { .. }
            ),
            "a deferred refresh runs on the next command"
        );
        assert_eq!(fs::read_to_string(&policy).unwrap(), embedded_policy());
    }

    #[test]
    fn routine_skill_update_includes_every_manifest_recorded_custom_root() {
        let dir = tempdir().unwrap();
        let receipt = dir.path().join("state/transaction.json");
        let root = dir.path().join("custom/ai-session-search");
        fs::create_dir_all(receipt.parent().unwrap()).unwrap();
        fs::create_dir_all(&root).unwrap();
        for file in AI_SESSION_SEARCH_SKILL_PACKAGE.files {
            let path = root.join(file.relative_path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, file.content).unwrap();
        }
        let manifest_path = crate::skill_manifest::manifest_path(&receipt);
        let mut manifest = crate::skill_manifest::SkillInstallManifest::default();
        manifest.record(
            &root,
            &AI_SESSION_SEARCH_SKILL_PACKAGE
                .files
                .iter()
                .map(|file| (file.relative_path.to_string(), file.content))
                .collect::<Vec<_>>(),
        );
        fs::write(&manifest_path, manifest.to_json().unwrap()).unwrap();

        let loaded = crate::skill_manifest::load_manifest(&manifest_path).unwrap();
        assert_eq!(manifest_recorded_skill_roots(&loaded).unwrap(), vec![root]);
    }

    /// A directory aise never wrote is diagnosed per root, not written and not fatal.
    #[test]
    fn writing_an_unowned_root_is_refused_without_stopping_the_owned_ones() {
        let dir = tempdir().unwrap();
        let receipt = dir.path().join("receipt.json");
        let owned = dir.path().join("owned/ai-session-search");
        let foreign = dir.path().join("foreign/ai-session-search");
        fs::create_dir_all(&foreign).unwrap();
        fs::write(foreign.join("SKILL.md"), "---\nname: mine\n---\n").unwrap();

        let outcomes =
            write_owned_skills(&[owned.clone(), foreign.clone()], &receipt, false, false).unwrap();
        assert_eq!(outcomes.len(), 2);
        let foreign_outcome = outcomes
            .iter()
            .find(|outcome| outcome.root == foreign.display().to_string())
            .unwrap();
        assert!(
            foreign_outcome
                .problem
                .as_deref()
                .is_some_and(|text| text.contains("no managed marker and no install record")),
            "{foreign_outcome:#?}"
        );
        assert_eq!(
            fs::read_to_string(foreign.join("SKILL.md")).unwrap(),
            "---\nname: mine\n---\n"
        );
        assert!(
            owned.join("SKILL.md").is_file(),
            "one refused root must not stop the others from being written"
        );
    }

    /// A manifest that names a file which is now gone must report `missing`, not `configured`.
    ///
    /// The record says aise wrote three files. If one has since been deleted, summarizing the two
    /// that survive as healthy hides the only fact worth acting on.
    #[test]
    fn a_manifest_naming_a_vanished_file_reports_it_missing() {
        let dir = tempdir().unwrap();
        let receipt = dir.path().join("receipt.json");
        let manifest_path = crate::skill_manifest::manifest_path(&dir.path().join("config.toml"));
        let root = dir.path().join("skills/corrections");
        let target = skill_target_for_package(
            CUSTOM_SKILL_TARGET_LABEL,
            root.clone(),
            &AI_SESSION_SEARCH_SKILL_PACKAGE,
            vec![],
            vec![],
        );

        let mutations = preflight_install(
            &[],
            &[],
            std::slice::from_ref(&target),
            Path::new("aise"),
            Some(&manifest_path),
        )
        .unwrap();
        execute_planned_transaction(&receipt, &mutations).unwrap();
        fs::remove_file(root.join("references/message-classification.md")).unwrap();

        let manifest = crate::skill_manifest::load_manifest(&manifest_path).unwrap();
        let partial = status_skill_file(&target, &manifest).unwrap();
        assert!(
            partial.starts_with("incomplete: 1 of 5 managed files missing"),
            "{partial}"
        );

        // Every file gone is a different fact from one file gone, and the words have to differ:
        // `aise integrations status` printed `missing` for a package whose other four files were
        // byte-current, which reads as "nothing is installed" about a working skill.
        for file in target.package.files {
            let path = root.join(file.relative_path);
            if path.exists() {
                fs::remove_file(path).unwrap();
            }
        }
        assert_eq!(status_skill_file(&target, &manifest).unwrap(), "missing");
    }

    /// A truncated or future-schema manifest must leave every command RUNNABLE.
    ///
    /// These are the commands you reach for when something is wrong, so refusing to run because
    /// the diagnostic file is itself damaged would strand the user. It reports uncertainty
    /// instead, and never claims a tree is untouched on evidence it cannot read.
    #[test]
    fn a_damaged_manifest_degrades_to_uncertainty_rather_than_failing_the_command() {
        let dir = tempdir().unwrap();
        let receipt = dir.path().join("receipt.json");
        let manifest_path = crate::skill_manifest::manifest_path(&dir.path().join("config.toml"));
        let root = dir.path().join("skills/corrections");
        let target = skill_target_for_package(
            CUSTOM_SKILL_TARGET_LABEL,
            root.clone(),
            &AI_SESSION_SEARCH_SKILL_PACKAGE,
            vec![],
            vec![],
        );

        let mutations = preflight_install(
            &[],
            &[],
            std::slice::from_ref(&target),
            Path::new("aise"),
            Some(&manifest_path),
        )
        .unwrap();
        execute_planned_transaction(&receipt, &mutations).unwrap();
        fs::write(root.join("aise-capability.toml"), "edited\n").unwrap();

        for damaged in [
            "",
            "{ truncated",
            r#"{"schema_version": 99, "installations": []}"#,
        ] {
            fs::write(&manifest_path, damaged).unwrap();
            let manifest =
                crate::skill_manifest::load_manifest(&manifest_path).unwrap_or_else(|error| {
                    panic!("reading a damaged manifest must not fail: {error:#}")
                });
            assert_eq!(
                status_skill_file(&target, &manifest).unwrap(),
                "legacy, ownership uncertain",
                "with no readable record, whether the file was edited is unknowable: {damaged:?}"
            );
            // And uninstall must still be able to decide -- conservatively.
            let plan = plan_remove_skill_file(&target, &manifest, false).unwrap();
            assert!(
                plan.mutations.is_empty() && !plan.preserved.is_empty(),
                "an unreadable record must preserve the tree, not delete on a guess: {damaged:?}"
            );
        }
    }

    #[test]
    fn damaged_manifest_blocks_install_and_skill_writes_before_mutation() {
        for damaged in [
            "",
            "{ truncated",
            r#"{"schema_version": 99, "installations": []}"#,
        ] {
            let dir = tempdir().unwrap();
            let config_path = dir.path().join("state/config.toml");
            let manifest_path = crate::skill_manifest::manifest_path(&config_path);
            fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
            fs::write(&manifest_path, damaged).unwrap();

            let root = dir.path().join("skills/ai-session-search");
            let target = skill_target_for_package(
                CUSTOM_SKILL_TARGET_LABEL,
                root.clone(),
                &AI_SESSION_SEARCH_SKILL_PACKAGE,
                vec![],
                vec![],
            );
            let error = preflight_install(
                &[],
                &[],
                std::slice::from_ref(&target),
                Path::new("aise"),
                Some(&manifest_path),
            )
            .expect_err("install must refuse damaged ownership evidence");
            assert!(format!("{error:#}").contains("repair or move"));
            assert!(!root.exists(), "preflight must not publish package files");
            assert_eq!(fs::read_to_string(&manifest_path).unwrap(), damaged);

            let error = write_owned_skills(std::slice::from_ref(&root), &config_path, false, false)
                .expect_err("update and restore must refuse damaged ownership evidence");
            assert!(format!("{error:#}").contains("repair or move"));
            assert!(
                !root.exists(),
                "write command must fail before package creation"
            );
            assert_eq!(fs::read_to_string(&manifest_path).unwrap(), damaged);
        }
    }

    /// `--force-full-cleanup` must refuse every way of reaching it by accident.
    #[test]
    fn force_full_cleanup_refuses_anything_but_one_explicitly_named_owned_root() {
        let named = skill_target(
            CUSTOM_SKILL_TARGET_LABEL,
            PathBuf::from("/tmp/skills/ai-session-search"),
            vec![],
            vec![],
        );
        let detected = skill_target(
            "claude",
            PathBuf::from("/home/test/.claude/skills/ai-session-search"),
            vec![],
            vec![],
        );

        let mut args = IntegrationUninstallArgs {
            force_full_cleanup: true,
            ..Default::default()
        };

        // No --skill-root at all: the flag must not fan out to detected client directories.
        let error =
            validate_force_full_cleanup(&args, std::slice::from_ref(&detected)).unwrap_err();
        assert!(
            format!("{error:#}").contains("requires an exact --skill-root"),
            "{error:#}"
        );

        // A root was named and a detected directory came along with it. That must NOT be an
        // error: once a skill is installed, client directories are always detected, so refusing
        // would leave a caller who named an exact root with no way to satisfy the check. The
        // detected roots are dropped instead, which is the only safe reading of "this exact root".
        args.targets.skill_roots = vec![PathBuf::from("/tmp/skills/ai-session-search")];
        validate_force_full_cleanup(&args, &[named.clone(), detected]).unwrap();

        // Contradictory intent.
        args.keep_skill = true;
        let error = validate_force_full_cleanup(&args, std::slice::from_ref(&named)).unwrap_err();
        assert!(format!("{error:#}").contains("contradict"), "{error:#}");

        args.keep_skill = false;
        validate_force_full_cleanup(&args, &[named]).unwrap();
    }

    /// Force cleanup deletes the caller's own files -- that is its purpose -- but only inside the
    /// named root, and it still refuses a directory that is provably not aise's.
    #[test]
    fn force_full_cleanup_removes_user_files_inside_the_root_and_nothing_outside_it() {
        let dir = tempdir().unwrap();
        let receipt = dir.path().join("state/receipt.json");
        let absent = crate::skill_manifest::ManifestState::Absent;
        let root = dir.path().join("skills/ai-session-search");
        let target = skill_target(CUSTOM_SKILL_TARGET_LABEL, root.clone(), vec![], vec![]);

        publish_planned_mutations(
            &normalize_planned_mutations(plan_upsert_skill_file(&target).unwrap()).unwrap(),
        )
        .unwrap();
        let user_file = root.join("references/my-notes.md");
        fs::create_dir_all(user_file.parent().unwrap()).unwrap();
        fs::write(&user_file, "mine").unwrap();
        let sibling = dir.path().join("skills/unrelated.md");
        fs::write(&sibling, "not part of the skill").unwrap();

        let plan = plan_remove_skill_file(&target, &absent, true).unwrap();
        assert!(
            plan.preserved.is_empty(),
            "force does not preserve; that is the point of the flag"
        );
        execute_planned_transaction(
            &receipt,
            &normalize_planned_mutations(plan.mutations).unwrap(),
        )
        .unwrap();
        prune_emptied_skill_directories(&plan.prune);

        assert!(
            !user_file.exists(),
            "force cleanup deletes the caller's own files"
        );
        assert!(!root.exists(), "the emptied skill root itself is pruned");
        assert_eq!(
            fs::read_to_string(&sibling).unwrap(),
            "not part of the skill",
            "nothing outside the named root may be touched"
        );

        // An arbitrary unowned directory is still refused, even with force.
        let unowned = dir.path().join("skills/notaskill");
        fs::create_dir_all(&unowned).unwrap();
        fs::write(unowned.join("SKILL.md"), "---\nname: notaskill\n---\n").unwrap();
        let error = plan_remove_skill_file(
            &skill_target(CUSTOM_SKILL_TARGET_LABEL, unowned.clone(), vec![], vec![]),
            &absent,
            true,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("unmanaged"), "{error:#}");
        assert!(unowned.join("SKILL.md").exists());
    }

    /// S13: the install record is what separates "you have an older aise" from "you edited this".
    ///
    /// Byte comparison alone answers only "does this equal the current build", and a NO covers
    /// both. Reporting the first as the second tells the user something FALSE about their own
    /// filesystem -- uninstall then says "modified since install, your edits are kept" about a
    /// file they never touched, and preserves a stale directory forever.
    #[test]
    fn the_install_record_separates_an_older_install_from_an_edited_one() {
        let dir = tempdir().unwrap();
        let receipt = dir.path().join("state/install-receipt.json");
        let manifest_path = crate::skill_manifest::manifest_path(&dir.path().join("config.toml"));
        let root = dir.path().join("skills/corrections");
        let target = skill_target_for_package(
            "test",
            root.clone(),
            &AI_SESSION_SEARCH_SKILL_PACKAGE,
            vec![],
            vec![],
        );
        let policy = root.join("aise-capability.toml");

        let mutations = preflight_install(
            &[],
            &[],
            std::slice::from_ref(&target),
            Path::new("aise"),
            Some(&manifest_path),
        )
        .unwrap();
        execute_planned_transaction(&receipt, &mutations).unwrap();
        let manifest = crate::skill_manifest::load_manifest(&manifest_path).unwrap();
        assert_eq!(status_skill_file(&target, &manifest).unwrap(), "configured");

        // Simulate an OLDER install: rewrite the file AND the record to the same older bytes, so
        // disk matches the manifest but not what this build embeds. Nobody edited anything.
        let older = "# an older shipped policy\nschema_version = 1\n";
        fs::write(&policy, older).unwrap();
        let mut recorded = manifest.writable_manifest(&manifest_path).unwrap();
        recorded.record(
            &root,
            &target
                .package
                .files
                .iter()
                .map(|file| {
                    let content = if file.relative_path == "aise-capability.toml" {
                        older
                    } else {
                        file.content
                    };
                    (file.relative_path.to_string(), content)
                })
                .collect::<Vec<_>>(),
        );
        fs::write(&manifest_path, recorded.to_json().unwrap()).unwrap();
        let manifest = crate::skill_manifest::load_manifest(&manifest_path).unwrap();
        assert_eq!(
            status_skill_file(&target, &manifest).unwrap(),
            "outdated, untouched",
            "disk equals the record but not this build: an older install nobody touched"
        );

        // Now the user really does edit it. Same "not current" byte comparison, opposite meaning.
        fs::write(&policy, "# my own rules\nschema_version = 1\n").unwrap();
        assert_eq!(
            status_skill_file(&target, &manifest).unwrap(),
            "modified or damaged",
            "disk differs from the record: changed since install"
        );

        // Never "modified" vs "damaged": nothing on disk carries that intent.
        assert!(
            !status_skill_file(&target, &manifest)
                .unwrap()
                .contains("edited"),
            "the report must not claim to know whether a change was intentional"
        );

        // Without any record, the same edited file can only be reported as uncertain -- which is
        // exactly the state this whole mechanism exists to replace.
        assert_eq!(
            status_skill_file(&target, &crate::skill_manifest::ManifestState::Absent).unwrap(),
            "legacy, ownership uncertain"
        );
    }

    /// The manifest is published by the SAME transaction as the files it describes, and it
    /// records only the roots this run touched.
    ///
    /// Written afterwards, it could survive a rollback and claim an install that never happened;
    /// written for every known root, one harness's uninstall would erase another's record.
    #[test]
    fn the_install_manifest_travels_with_the_files_it_describes() {
        let dir = tempdir().unwrap();
        let receipt = dir.path().join("state/install-receipt.json");
        let manifest_path = crate::skill_manifest::manifest_path(&dir.path().join("config.toml"));
        let claude = skill_target(
            "claude",
            dir.path().join("claude/ai-session-search"),
            vec![],
            vec![],
        );
        let codex = skill_target(
            "codex",
            dir.path().join("codex/ai-session-search"),
            vec![],
            vec![],
        );

        let mutations = preflight_install(
            &[],
            &[],
            std::slice::from_ref(&claude),
            Path::new("aise"),
            Some(&manifest_path),
        )
        .unwrap();
        assert_eq!(
            mutations.len(),
            claude.package.files.len() + 1,
            "one mutation per managed file, plus the manifest, in one transaction"
        );
        execute_planned_transaction(&receipt, &mutations).unwrap();

        let state = crate::skill_manifest::load_manifest(&manifest_path).unwrap();
        let recorded = state
            .installation(&claude.root)
            .expect("the installed root is recorded");
        assert_eq!(recorded.files.len(), claude.package.files.len());
        for file in claude.package.files {
            let entry = recorded
                .file(file.relative_path)
                .unwrap_or_else(|| panic!("{} is recorded", file.relative_path));
            assert_eq!(entry.bytes, file.content.len());
            assert_eq!(
                entry.sha256,
                crate::hashing::sha256(file.content.as_bytes()),
                "the digest must describe the bytes actually written"
            );
        }
        assert!(
            !manifest_path.file_name().is_some_and(|name| claude
                .package
                .files
                .iter()
                .any(|file| file.relative_path == name)),
            "the manifest is not itself a managed skill file"
        );

        // A second root joins the record without disturbing the first.
        let second = preflight_install(
            &[],
            &[],
            std::slice::from_ref(&codex),
            Path::new("aise"),
            Some(&manifest_path),
        )
        .unwrap();
        execute_planned_transaction(&receipt, &second).unwrap();
        let state = crate::skill_manifest::load_manifest(&manifest_path).unwrap();
        assert!(state.installation(&claude.root).is_some());
        assert!(state.installation(&codex.root).is_some());

        // Uninstalling ONE root forgets only that root.
        let plan = preflight_uninstall(
            &[],
            &[],
            std::slice::from_ref(&claude),
            &[],
            Some(&manifest_path),
            false,
        )
        .unwrap();
        execute_planned_transaction(&receipt, &plan.mutations).unwrap();
        let state = crate::skill_manifest::load_manifest(&manifest_path).unwrap();
        assert!(
            state.installation(&claude.root).is_none(),
            "the removed root must lose its record"
        );
        assert!(
            state.installation(&codex.root).is_some(),
            "another harness's record must survive, or its own uninstall becomes ambiguous"
        );

        // Removing the last root leaves no manifest, so "absent" keeps meaning "nothing managed".
        let plan = preflight_uninstall(
            &[],
            &[],
            std::slice::from_ref(&codex),
            &[],
            Some(&manifest_path),
            false,
        )
        .unwrap();
        execute_planned_transaction(&receipt, &plan.mutations).unwrap();
        assert!(!manifest_path.exists());
    }

    #[test]
    fn managed_skill_lifecycle_refuses_unowned_files() {
        let dir = tempdir().unwrap();
        // No install record: these tests predate the manifest and exercise the
        // byte-comparison path a legacy install still takes.
        let absent = crate::skill_manifest::ManifestState::Absent;
        let root = dir.path().join("skills/corrections");
        let anchor = root.join(SKILL_ANCHOR_FILE);
        let target = skill_target_for_package(
            "test",
            root.clone(),
            &AI_SESSION_SEARCH_SKILL_PACKAGE,
            Vec::new(),
            Vec::new(),
        );

        publish_planned_mutations(
            &normalize_planned_mutations(plan_upsert_skill_file(&target).unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(status_skill_file(&target, &absent).unwrap(), "configured");

        let legacy_anchor = AI_SESSION_SEARCH_SKILL_FILES[0].content.replacen(
            "# AI Session Search",
            "# Older AI Session Search",
            1,
        );
        fs::write(&anchor, legacy_anchor).unwrap();
        assert_eq!(
            status_skill_file(&target, &absent).unwrap(),
            "legacy, ownership uncertain",
            "without an install record, whether anyone edited this is unknowable"
        );
        publish_planned_mutations(
            &normalize_planned_mutations(plan_upsert_skill_file(&target).unwrap()).unwrap(),
        )
        .unwrap();

        // S12: a file the USER added makes the WHOLE directory ambiguous, so normal uninstall
        // removes nothing and says why. Removing the managed files while leaving their notes
        // would orphan that file behind a skill that no longer declares itself.
        let user_file = root.join("references/my-notes.md");
        fs::create_dir_all(user_file.parent().unwrap()).unwrap();
        fs::write(&user_file, "mine").unwrap();
        let plan = plan_remove_skill_file(&target, &absent, false).unwrap();
        assert!(
            plan.mutations.is_empty(),
            "an unmanaged entry must stop the removal entirely"
        );
        assert_eq!(plan.preserved.len(), 1);
        assert!(
            plan.preserved[0].contains("my-notes.md")
                && plan.preserved[0].contains("not written by aise"),
            "the reason must name the offending file: {:?}",
            plan.preserved
        );
        for file in target.package.files {
            assert!(root.join(file.relative_path).exists());
        }

        // With the user's file gone, the tree is unambiguously aise's and removal proceeds.
        fs::remove_file(&user_file).unwrap();
        let plan = plan_remove_skill_file(&target, &absent, false).unwrap();
        assert!(plan.preserved.is_empty());
        publish_planned_mutations(&normalize_planned_mutations(plan.mutations).unwrap()).unwrap();
        for file in target.package.files {
            assert!(
                !root.join(file.relative_path).exists(),
                "{} should have been removed",
                file.relative_path
            );
        }

        // S5: directories aise created and this removal emptied are pruned; nothing else.
        prune_emptied_skill_directories(&plan.prune);
        assert!(
            !root.exists(),
            "the emptied managed package root and its subdirectories must not be left behind"
        );

        fs::create_dir_all(&root).unwrap();
        fs::write(&anchor, "---\nname: user-skill\n---\n").unwrap();
        assert!(plan_upsert_skill_file(&target)
            .unwrap_err()
            .to_string()
            .contains("unmanaged"));
        assert!(plan_remove_skill_file(&target, &absent, false)
            .unwrap_err()
            .to_string()
            .contains("unmanaged"));
        assert_eq!(
            status_skill_file(&target, &absent).unwrap(),
            "modified or unmanaged"
        );
    }

    #[test]
    fn skill_targets_cover_supported_skill_harnesses() {
        let layout = ClientLayout::new(
            PathBuf::from("/home/test"),
            PathBuf::from("/home/test/.config"),
            ClientPlatform::Linux,
        );
        let claude = skill_targets_for_layout(McpClient::Claude, &layout);
        let codex = skill_targets_for_layout(McpClient::Codex, &layout);
        let gemini = skill_targets_for_layout(McpClient::Gemini, &layout);
        let antigravity = skill_targets_for_layout(McpClient::Antigravity, &layout);
        let pi = skill_targets_for_layout(McpClient::Pi, &layout);
        let prime_agent = skill_targets_for_layout(McpClient::PrimeAgent, &layout);
        let canonical = PathBuf::from("/home/test/.ai-session-search/skills/ai-session-search");
        assert_eq!(
            claude[0].root, canonical,
            "the app owns one canonical package in its platform config namespace"
        );
        assert_eq!(codex[0].root, canonical);
        assert_eq!(gemini[0].root, canonical);
        assert_eq!(antigravity[0].root, canonical);
        assert_eq!(pi[0].root, canonical);
        assert_eq!(prime_agent[0].root, canonical);
        assert_eq!(
            claude[0].discovery_links[0],
            PathBuf::from("/home/test/.claude/skills/ai-session-search")
        );
        assert_eq!(
            codex[0].discovery_links[0],
            PathBuf::from("/home/test/.agents/skills/ai-session-search")
        );
        assert_eq!(
            gemini[0].discovery_links[0],
            PathBuf::from("/home/test/.agents/skills/ai-session-search")
        );
        assert_eq!(
            antigravity[0].discovery_links[0],
            PathBuf::from("/home/test/.gemini/config/skills/ai-session-search")
        );
        assert_eq!(
            antigravity[0].discovery_links[1],
            PathBuf::from("/home/test/.gemini/antigravity-cli/skills/ai-session-search"),
            "Antigravity CLI has a distinct current global skill directory"
        );
        assert_eq!(
            pi[0].discovery_links[0],
            PathBuf::from("/home/test/.pi/agent/skills/ai-session-search")
        );
        assert_eq!(
            prime_agent[0].discovery_links[0],
            PathBuf::from("/home/test/.prime/agent/skills/ai-session-search")
        );
        assert!(skill_targets_for_layout(McpClient::Opencode, &layout).is_empty());
    }

    #[cfg(unix)]
    fn install_test_skills(targets: &[SkillTarget], receipt: &Path) -> Vec<SymlinkChange> {
        let manifest = crate::skill_manifest::manifest_path(receipt);
        let mutations =
            preflight_install(&[], &[], targets, Path::new("aise"), Some(&manifest)).unwrap();
        let links = plan_install_skill_discovery_links(targets).unwrap();
        let (_, directories) = retired_discovery_outcomes(
            targets,
            &crate::skill_manifest::load_manifest(&manifest).unwrap(),
            &links,
            false,
        )
        .unwrap();
        execute_planned_transaction_with_links(receipt, &mutations, &links, |_, change| {
            publish_install_skill_discovery_link(change, targets)
        })
        .unwrap();
        prune_emptied_skill_directories(&directories);
        links
    }

    #[cfg(unix)]
    #[test]
    fn gemini_install_retires_owned_copy_but_preserves_and_reports_foreign_or_modified_entries() {
        for kind in [
            "copy",
            "edited-copy",
            "extra-file",
            "foreign-copy",
            "foreign-link",
        ] {
            let dir = tempdir().unwrap();
            let layout = ClientLayout::new(
                dir.path().join("home"),
                dir.path().join("config"),
                ClientPlatform::Linux,
            );
            let targets = skill_targets_for_layout(McpClient::Gemini, &layout);
            let receipt = layout.home.join(".ai-session-search/transaction.json");
            let old = layout
                .home
                .join(GEMINI_RETIRED_SKILLS_PATH)
                .join(AI_SESSION_SEARCH_SKILL_NAME);
            let mut legacy = skill_target("legacy", old.clone(), vec![], vec![]);
            legacy.retired_discovery_links.clear();
            if kind == "foreign-link" {
                fs::create_dir_all(old.parent().unwrap()).unwrap();
                create_directory_symlink(&dir.path().join("foreign"), &old).unwrap();
            } else if kind == "foreign-copy" {
                fs::create_dir_all(&old).unwrap();
                fs::write(old.join("SKILL.md"), "my skill").unwrap();
            } else {
                install_test_skills(&[legacy], &receipt);
                if kind == "edited-copy" {
                    fs::write(old.join("SKILL.md"), "my skill").unwrap();
                }
                if kind == "extra-file" {
                    fs::write(old.join("custom.txt"), "my file").unwrap();
                }
            }
            let manifest = crate::skill_manifest::load_manifest(
                &crate::skill_manifest::manifest_path(&receipt),
            )
            .unwrap();
            let links = plan_install_skill_discovery_links(&targets).unwrap();
            let (outcomes, _) =
                retired_discovery_outcomes(&targets, &manifest, &links, true).unwrap();
            assert!(
                fs::symlink_metadata(&old).is_ok(),
                "dry-run preserves {kind}"
            );
            if kind == "copy" {
                assert!(outcomes
                    .iter()
                    .any(|outcome| outcome.action == "would remove retired discovery copy"));
            } else {
                assert!(
                    outcomes.iter().any(|outcome| outcome.problem.is_some()),
                    "{kind}: {outcomes:?}"
                );
            }
            install_test_skills(&targets, &receipt);
            assert_eq!(fs::symlink_metadata(&old).is_ok(), kind != "copy", "{kind}");
            assert!(
                status_skill_discovery_link(
                    &layout
                        .home
                        .join(SHARED_SKILLS_PATH)
                        .join(AI_SESSION_SEARCH_SKILL_NAME),
                    &targets[0].root
                )
                .unwrap()
                .1
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn gemini_install_uses_one_shared_discovery_link_and_reports_current() {
        let dir = tempdir().unwrap();
        let layout = ClientLayout::new(
            dir.path().join("home"),
            dir.path().join("config"),
            ClientPlatform::Linux,
        );
        let targets = skill_targets_for_layout(McpClient::Gemini, &layout);
        let receipt = layout.home.join(".ai-session-search/transaction.json");
        install_test_skills(&targets, &receipt);
        let shared = layout.home.join(".agents/skills/ai-session-search");
        assert_eq!(targets[0].discovery_links, vec![shared.clone()]);
        assert!(
            status_skill_discovery_link(&shared, &targets[0].root)
                .unwrap()
                .1
        );
        let manifest =
            crate::skill_manifest::load_manifest(&crate::skill_manifest::manifest_path(&receipt))
                .unwrap();
        assert_eq!(
            status_skill_file(&targets[0], &manifest).unwrap(),
            "configured"
        );
        assert!(!layout
            .home
            .join(".gemini/skills/ai-session-search")
            .exists());
    }

    #[cfg(unix)]
    #[test]
    fn gemini_install_retires_owned_legacy_link_and_dry_run_plans_both_paths() {
        let dir = tempdir().unwrap();
        let layout = ClientLayout::new(
            dir.path().join("home"),
            dir.path().join("config"),
            ClientPlatform::Linux,
        );
        let targets = skill_targets_for_layout(McpClient::Gemini, &layout);
        let receipt = layout.home.join(".ai-session-search/transaction.json");
        let old = layout.home.join(".gemini/skills/ai-session-search");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        create_directory_symlink(&targets[0].root, &old).unwrap();
        let planned = plan_install_skill_discovery_links(&targets).unwrap();
        assert!(planned
            .iter()
            .any(|change| change.path == old && change.after == SymlinkImage::Absent));
        assert!(planned
            .iter()
            .any(|change| change.path == layout.home.join(".agents/skills/ai-session-search")));
        assert!(
            fs::symlink_metadata(&old).is_ok(),
            "planning must not mutate the old link"
        );
        install_test_skills(&targets, &receipt);
        assert!(fs::symlink_metadata(old).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn gemini_upgrade_retires_owned_link_or_copy_and_keeps_modified_entries() {
        for kind in ["link", "copy", "edited-copy", "foreign-link"] {
            let dir = tempdir().unwrap();
            let layout = ClientLayout::new(
                dir.path().join("home"),
                dir.path().join("config"),
                ClientPlatform::Linux,
            );
            let target = skill_targets_for_layout(McpClient::Gemini, &layout).remove(0);
            let receipt = layout.home.join(".ai-session-search/transaction.json");
            let old = layout.home.join(".gemini/skills/ai-session-search");
            let mut legacy = target.clone();
            legacy.discovery_links.clear();
            legacy.retired_discovery_links.clear();
            legacy.discovery_clients.clear();
            if kind.contains("copy") {
                legacy.root = old.clone();
            }
            install_test_skills(&[legacy], &receipt);
            if kind == "link" || kind == "foreign-link" {
                fs::create_dir_all(old.parent().unwrap()).unwrap();
                let destination = if kind == "link" {
                    target.root.clone()
                } else {
                    dir.path().join("foreign")
                };
                create_directory_symlink(&destination, &old).unwrap();
            }
            if kind == "edited-copy" {
                fs::write(old.join("SKILL.md"), "my skill").unwrap();
            }
            let UpgradeRefresh::Checked { skills, .. } =
                refresh_integrations_for_version(&receipt, "9.9.9", Some(&layout)).unwrap()
            else {
                panic!("refresh must run");
            };
            if kind == "link" || kind == "copy" {
                assert!(fs::symlink_metadata(&old).is_err(), "{kind}");
                assert!(
                    status_skill_discovery_link(
                        &layout.home.join(".agents/skills/ai-session-search"),
                        &target.root
                    )
                    .unwrap()
                    .1,
                    "{kind}"
                );
            } else {
                assert!(fs::symlink_metadata(&old).is_ok(), "{kind}");
                assert!(
                    skills
                        .iter()
                        .any(|outcome| outcome.root == old.display().to_string()
                            && outcome.problem.is_some()),
                    "preservation must be reported: {skills:?}"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn gemini_shared_link_survives_partial_uninstall_and_last_user_removes_it() {
        let dir = tempdir().unwrap();
        let layout = ClientLayout::new(
            dir.path().join("home"),
            dir.path().join("config"),
            ClientPlatform::Linux,
        );
        let receipt = layout.home.join(".ai-session-search/transaction.json");
        let gemini = skill_targets_for_layout(McpClient::Gemini, &layout);
        let codex = skill_targets_for_layout(McpClient::Codex, &layout);
        install_test_skills(&codex, &receipt);
        install_test_skills(&gemini, &receipt);
        let manifest_path = crate::skill_manifest::manifest_path(&receipt);
        let manifest = crate::skill_manifest::load_manifest(&manifest_path).unwrap();
        let partial = prepare_skill_discovery_uninstall(&gemini, &layout, &manifest).unwrap();
        assert!(partial.links_to_remove.is_empty());
        assert!(partial.file_targets.is_empty());
        let plan = preflight_uninstall(
            &[],
            &[],
            &partial.file_targets,
            &gemini,
            Some(&manifest_path),
            false,
        )
        .unwrap();
        execute_planned_transaction(&receipt, &plan.mutations).unwrap();
        let manifest = crate::skill_manifest::load_manifest(&manifest_path).unwrap();
        assert_eq!(
            manifest
                .installation(&codex[0].root)
                .unwrap()
                .discovery_clients,
            vec!["codex"]
        );
        let last_codex = prepare_skill_discovery_uninstall(&codex, &layout, &manifest).unwrap();
        assert_eq!(last_codex.links_to_remove.len(), 1);
        let plan = preflight_uninstall(
            &[],
            &[],
            &last_codex.file_targets,
            &codex,
            Some(&manifest_path),
            false,
        )
        .unwrap();
        let links = last_codex
            .links_to_remove
            .into_iter()
            .map(|(path, expected)| SymlinkChange::remove(path, expected))
            .collect::<Vec<_>>();
        execute_planned_transaction_with_links(&receipt, &plan.mutations, &links, |_, change| {
            publish_symlink_change(change)
        })
        .unwrap();
        assert!(fs::symlink_metadata(&codex[0].discovery_links[0]).is_err());
        assert!(!codex[0].anchor().exists());
        // Reinstall Gemini to exercise removing both selectors in one transaction too.
        install_test_skills(&codex, &receipt);
        install_test_skills(&gemini, &receipt);
        let manifest = crate::skill_manifest::load_manifest(&manifest_path).unwrap();
        let mut both = gemini;
        both.extend(codex);
        dedupe_skill_targets(&mut both).unwrap();
        let last = prepare_skill_discovery_uninstall(&both, &layout, &manifest).unwrap();
        assert_eq!(last.links_to_remove.len(), 1);
        let plan = preflight_uninstall(
            &[],
            &[],
            &last.file_targets,
            &both,
            Some(&manifest_path),
            false,
        )
        .unwrap();
        let links = last
            .links_to_remove
            .into_iter()
            .map(|(path, expected)| SymlinkChange::remove(path, expected))
            .collect::<Vec<_>>();
        execute_planned_transaction_with_links(&receipt, &plan.mutations, &links, |_, change| {
            publish_symlink_change(change)
        })
        .unwrap();
        assert!(!layout
            .home
            .join(".agents/skills/ai-session-search")
            .exists());
        assert!(!both[0].anchor().exists());
    }

    #[cfg(unix)]
    #[test]
    fn gemini_upgrade_does_not_reinstall_a_deleted_canonical_skill() {
        let dir = tempdir().unwrap();
        let layout = ClientLayout::new(
            dir.path().join("home"),
            dir.path().join("config"),
            ClientPlatform::Linux,
        );
        let receipt = layout.home.join(".ai-session-search/transaction.json");
        let target = skill_targets_for_layout(McpClient::Gemini, &layout).remove(0);
        let old = &target.retired_discovery_links[0];
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        create_directory_symlink(&target.root, old).unwrap();
        let mut manifest = crate::skill_manifest::SkillInstallManifest::default();
        manifest.record(&target.root, &[]);
        let path = crate::skill_manifest::manifest_path(&receipt);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, manifest.to_json().unwrap()).unwrap();
        refresh_integrations_for_version(&receipt, "9.9.9", Some(&layout)).unwrap();
        assert!(!target.root.exists());
        assert!(fs::symlink_metadata(old).is_ok());
        assert!(fs::symlink_metadata(&target.discovery_links[0]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn gemini_package_refresh_preserves_shared_users_without_claiming_new_ones() {
        let dir = tempdir().unwrap();
        let layout = ClientLayout::new(
            dir.path().join("home"),
            dir.path().join("config"),
            ClientPlatform::Linux,
        );
        let receipt = layout.home.join(".ai-session-search/transaction.json");
        let targets = skill_targets_for_layout(McpClient::Gemini, &layout);
        install_test_skills(&targets, &receipt);
        write_owned_skills(&[targets[0].root.clone()], &receipt, false, false).unwrap();
        let manifest =
            crate::skill_manifest::load_manifest(&crate::skill_manifest::manifest_path(&receipt))
                .unwrap();
        assert_eq!(
            manifest
                .installation(&targets[0].root)
                .unwrap()
                .discovery_clients,
            vec!["gemini"]
        );
        let removal = prepare_skill_discovery_uninstall(&targets, &layout, &manifest).unwrap();
        assert_eq!(removal.links_to_remove.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn gemini_migration_retains_codex_from_a_manifest_without_client_records() {
        let dir = tempdir().unwrap();
        let layout = ClientLayout::new(
            dir.path().join("home"),
            dir.path().join("config"),
            ClientPlatform::Linux,
        );
        let receipt = layout.home.join(".ai-session-search/transaction.json");
        let targets = skill_targets_for_layout(McpClient::Gemini, &layout);
        let mut legacy = targets[0].clone();
        legacy.discovery_links.clear();
        legacy.retired_discovery_links.clear();
        legacy.discovery_clients.clear();
        install_test_skills(&[legacy], &receipt);
        for link in targets[0]
            .discovery_links
            .iter()
            .chain(&targets[0].retired_discovery_links)
        {
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            create_directory_symlink(&targets[0].root, link).unwrap();
        }
        refresh_integrations_for_version(&receipt, "9.9.9", Some(&layout)).unwrap();
        let manifest =
            crate::skill_manifest::load_manifest(&crate::skill_manifest::manifest_path(&receipt))
                .unwrap();
        assert_eq!(
            manifest
                .installation(&targets[0].root)
                .unwrap()
                .discovery_clients,
            vec!["codex", "gemini"]
        );
        let removal = prepare_skill_discovery_uninstall(&targets, &layout, &manifest).unwrap();
        assert!(removal.links_to_remove.is_empty());
        assert!(removal.file_targets.is_empty());
    }

    #[test]
    fn uninstall_includes_the_retired_package_and_matching_discovery_links() {
        let mut current = skill_target(
            "app",
            PathBuf::from("/home/test/.ai-session-search/skills/ai-session-search"),
            vec![],
            vec![],
        );
        current.discovery_links = vec![
            PathBuf::from("/home/test/.claude/skills/ai-session-search"),
            PathBuf::from("/home/test/.agents/skills/ai-session-search"),
        ];
        let mut targets = vec![current];

        append_retired_sibling_skill_targets(&mut targets).unwrap();

        assert_eq!(targets.len(), 2);
        assert_eq!(
            targets[1].root,
            PathBuf::from("/home/test/.ai-session-search/skills/corrections")
        );
        assert_eq!(targets[1].package.name, "corrections");
        assert_eq!(
            targets[1].discovery_links,
            vec![
                PathBuf::from("/home/test/.claude/skills/corrections"),
                PathBuf::from("/home/test/.agents/skills/corrections"),
            ]
        );
    }

    #[test]
    fn canonical_skill_retention_inventory_includes_antigravity_cli() {
        let layout = ClientLayout::new(
            PathBuf::from("/home/test"),
            PathBuf::from("/home/test/.config"),
            ClientPlatform::Linux,
        );

        let links = all_standard_discovery_links(&layout, &AI_SESSION_SEARCH_SKILL_PACKAGE);
        for expected in [
            "/home/test/.gemini/antigravity-cli/skills/ai-session-search",
            "/home/test/.pi/agent/skills/ai-session-search",
            "/home/test/.prime/agent/skills/ai-session-search",
        ] {
            assert!(
                links.contains(&PathBuf::from(expected)),
                "a surviving harness link must retain the canonical skill package: {links:?}"
            );
        }
    }

    #[test]
    fn canonical_skill_retention_fails_closed_when_a_harness_link_cannot_be_inspected() {
        let dir = tempdir().unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join(".gemini"), "not a directory").unwrap();
        let layout = ClientLayout::new(home, dir.path().join("config"), ClientPlatform::Linux);
        let canonical = dir.path().join("state/skills/ai-session-search");

        let error = another_owned_discovery_link(
            &layout,
            &AI_SESSION_SEARCH_SKILL_PACKAGE,
            &[],
            &canonical,
        )
        .expect_err("an unreadable inventory cannot authorize package removal")
        .to_string();

        assert!(error.contains("inspect skill discovery entry"), "{error}");
        let normalized_error = error.replace('\\', "/");
        assert!(
            normalized_error.contains(".gemini/skills/ai-session-search"),
            "{error}"
        );
    }

    #[test]
    fn selecting_multiple_harnesses_keeps_every_discovery_directory() {
        let layout = ClientLayout::new(
            PathBuf::from("/home/test"),
            PathBuf::from("/home/test/.config"),
            ClientPlatform::Linux,
        );
        let mut targets = [
            McpClient::Claude,
            McpClient::Codex,
            McpClient::Gemini,
            McpClient::Antigravity,
            McpClient::Pi,
            McpClient::PrimeAgent,
        ]
        .into_iter()
        .flat_map(|client| skill_targets_for_layout(client, &layout))
        .collect::<Vec<_>>();
        dedupe_skill_targets(&mut targets).unwrap();

        assert_eq!(targets.len(), MANAGED_SKILL_PACKAGES.len());
        for target in targets {
            assert_eq!(
                target.discovery_links.len(),
                6,
                "{} must remain discoverable from all selected harnesses",
                target.package.name
            );
        }
    }

    #[test]
    fn relocated_config_rehomes_only_automatic_canonical_skills() {
        let package = &AI_SESSION_SEARCH_SKILL_PACKAGE;
        let mut automatic =
            skill_target_for_package("app", PathBuf::from("/old"), package, vec![], vec![]);
        automatic
            .discovery_links
            .push(PathBuf::from("/home/test/.claude/skills/ai-session-search"));
        let custom = skill_target_for_package(
            CUSTOM_SKILL_TARGET_LABEL,
            PathBuf::from("/custom/ai-session-search"),
            package,
            vec![],
            vec![],
        );
        let mut targets = vec![automatic, custom];

        rebase_automatic_skill_roots(
            &mut targets,
            Path::new("/portable/aise/.ai-session-search-mcp-transaction.json"),
        );

        assert_eq!(
            targets[0].root,
            PathBuf::from("/portable/aise/skills/ai-session-search")
        );
        assert_eq!(targets[1].root, PathBuf::from("/custom/ai-session-search"));
    }

    #[cfg(unix)]
    #[test]
    fn canonical_install_creates_real_skill_and_harness_symlink() {
        let dir = tempdir().unwrap();
        let canonical = dir.path().join("config/skills/ai-session-search");
        let link = dir.path().join("harness/skills/ai-session-search");
        let mut target = skill_target_for_package(
            "app",
            canonical.clone(),
            &AI_SESSION_SEARCH_SKILL_PACKAGE,
            vec![],
            vec![],
        );
        target.discovery_links.push(link.clone());
        let manifest = dir.path().join("config/skill-install-manifest.json");
        let receipt = dir.path().join("config/transaction.json");

        let mutations = preflight_install(
            &[],
            &[],
            &[target.clone()],
            Path::new("aise"),
            Some(&manifest),
        )
        .unwrap();
        let links = plan_install_skill_discovery_links(std::slice::from_ref(&target)).unwrap();
        execute_planned_transaction_with_links(&receipt, &mutations, &links, |_, change| {
            publish_install_skill_discovery_link(change, std::slice::from_ref(&target))
        })
        .unwrap();

        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_link(&link).unwrap(), canonical);
        assert!(link.join("SKILL.md").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn install_retires_the_owned_sibling_package_and_its_discovery_symlink() {
        let dir = tempdir().unwrap();
        let canonical = dir.path().join("config/skills/ai-session-search");
        let retired = canonical.parent().unwrap().join(RETIRED_SKILL_PACKAGE_NAME);
        let link = dir.path().join("harness/skills/ai-session-search");
        let retired_link = link.parent().unwrap().join(RETIRED_SKILL_PACKAGE_NAME);
        let mut target = skill_target_for_package(
            "app",
            canonical.clone(),
            &AI_SESSION_SEARCH_SKILL_PACKAGE,
            vec![],
            vec![],
        );
        target.discovery_links.push(link.clone());

        let retired_skill = "---\nname: corrections\ndescription: retired\n---\n\
                             <!-- ai-session-search-managed-skill v1 -->\nbody\n";
        let retired_capability = "schema_version = 1\n";
        fs::create_dir_all(&retired).unwrap();
        fs::write(retired.join("SKILL.md"), retired_skill).unwrap();
        fs::write(retired.join("capability.toml"), retired_capability).unwrap();
        fs::create_dir_all(retired_link.parent().unwrap()).unwrap();
        create_directory_symlink(&retired, &retired_link).unwrap();

        let manifest = dir.path().join("config/skill-install-manifest.json");
        fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        let mut recorded = crate::skill_manifest::SkillInstallManifest::default();
        recorded.record(
            &retired,
            &[
                ("SKILL.md".to_string(), retired_skill),
                ("capability.toml".to_string(), retired_capability),
            ],
        );
        fs::write(&manifest, recorded.to_json().unwrap()).unwrap();
        let receipt = dir.path().join("config/transaction.json");

        let mutations = preflight_install(
            &[],
            &[],
            std::slice::from_ref(&target),
            Path::new("aise"),
            Some(&manifest),
        )
        .unwrap();
        let retired_links =
            plan_retire_sibling_discovery_links(std::slice::from_ref(&target)).unwrap();
        let mut links = retired_links
            .iter()
            .map(|(path, expected)| SymlinkChange::remove(path.clone(), expected.clone()))
            .collect::<Vec<_>>();
        links.extend(plan_install_skill_discovery_links(std::slice::from_ref(&target)).unwrap());
        execute_planned_transaction_with_links(&receipt, &mutations, &links, |_, change| {
            publish_install_skill_discovery_link(change, std::slice::from_ref(&target))
        })
        .unwrap();
        prune_retired_skill_directories(std::slice::from_ref(&retired));

        assert!(!retired_link.exists());
        assert!(!retired.join("SKILL.md").exists());
        assert!(!retired.exists());
        assert_eq!(fs::read_link(&link).unwrap(), canonical);
        assert!(canonical
            .join(crate::skill_catalog::CAPABILITY_FILE)
            .is_file());
    }

    #[cfg(unix)]
    #[test]
    fn second_discovery_link_install_failure_restores_text_and_first_link() {
        let dir = tempdir().unwrap();
        let config = dir.path().join("config.json");
        let receipt = dir.path().join("transaction.json");
        let canonical = dir.path().join("skills/ai-session-search");
        let first = dir.path().join("claude/skills/ai-session-search");
        let second = dir.path().join("codex/skills/ai-session-search");
        fs::write(&config, "before").unwrap();
        fs::create_dir_all(&canonical).unwrap();
        let mutations = [planned_write(
            &config,
            &Some("before".to_string()),
            "after".to_string(),
        )];
        let links = [
            SymlinkChange::install(first.clone(), canonical.clone()),
            SymlinkChange::install(second.clone(), canonical),
        ];

        let error = execute_planned_transaction_with_links(
            &receipt,
            &mutations,
            &links,
            |index, change| {
                if index == 1 {
                    bail!("injected second discovery-link install failure");
                }
                publish_symlink_change(change)
            },
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("restored 2 changed path"), "{error}");
        assert_eq!(fs::read_to_string(config).unwrap(), "before");
        assert!(!first.exists());
        assert!(!second.exists());
        assert!(!receipt.exists());
    }

    #[cfg(unix)]
    #[test]
    fn second_discovery_link_uninstall_failure_restores_text_and_first_link() {
        let dir = tempdir().unwrap();
        let config = dir.path().join("config.json");
        let receipt = dir.path().join("transaction.json");
        let canonical = dir.path().join("skills/ai-session-search");
        let first = dir.path().join("claude/skills/ai-session-search");
        let second = dir.path().join("codex/skills/ai-session-search");
        fs::write(&config, "before").unwrap();
        fs::create_dir_all(&canonical).unwrap();
        fs::create_dir_all(first.parent().unwrap()).unwrap();
        fs::create_dir_all(second.parent().unwrap()).unwrap();
        create_directory_symlink(&canonical, &first).unwrap();
        create_directory_symlink(&canonical, &second).unwrap();
        let mutations = [planned_write(
            &config,
            &Some("before".to_string()),
            "after".to_string(),
        )];
        let links = [
            SymlinkChange::remove(first.clone(), canonical.clone()),
            SymlinkChange::remove(second.clone(), canonical.clone()),
        ];

        let error = execute_planned_transaction_with_links(
            &receipt,
            &mutations,
            &links,
            |index, change| {
                if index == 1 {
                    bail!("injected second discovery-link uninstall failure");
                }
                publish_symlink_change(change)
            },
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("restored 2 changed path"), "{error}");
        assert_eq!(fs::read_to_string(config).unwrap(), "before");
        assert_eq!(fs::read_link(first).unwrap(), canonical);
        assert_eq!(fs::read_link(second).unwrap(), canonical);
        assert!(!receipt.exists());
    }

    #[test]
    fn migration_refuses_a_legacy_copy_with_user_files() {
        let dir = tempdir().unwrap();
        let canonical = dir.path().join("config/skills/ai-session-search");
        let legacy = dir.path().join("harness/skills/ai-session-search");
        let mut target = skill_target_for_package(
            "app",
            canonical,
            &AI_SESSION_SEARCH_SKILL_PACKAGE,
            vec![],
            vec![],
        );
        target.discovery_links.push(legacy.clone());
        for file in AI_SESSION_SEARCH_SKILL_PACKAGE.files {
            let path = legacy.join(file.relative_path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, file.content).unwrap();
        }
        fs::write(legacy.join("my-notes.md"), "keep me").unwrap();

        let error = preflight_install(
            &[],
            &[],
            &[target],
            Path::new("aise"),
            Some(&dir.path().join("manifest.json")),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("user changes"));
        assert_eq!(
            fs::read_to_string(legacy.join("my-notes.md")).unwrap(),
            "keep me"
        );
    }

    #[cfg(unix)]
    #[test]
    fn uninstall_transaction_removes_only_a_planned_owned_discovery_link() {
        let dir = tempdir().unwrap();
        let canonical = dir.path().join("config/skills/ai-session-search");
        let owned = dir.path().join("claude/skills/ai-session-search");
        let modified = dir.path().join("gemini/skills/ai-session-search");
        let other = dir.path().join("other-skill");
        fs::create_dir_all(&canonical).unwrap();
        fs::create_dir_all(&other).unwrap();
        fs::create_dir_all(owned.parent().unwrap()).unwrap();
        fs::create_dir_all(modified.parent().unwrap()).unwrap();
        create_directory_symlink(&canonical, &owned).unwrap();
        create_directory_symlink(&other, &modified).unwrap();

        execute_planned_transaction_with_links(
            &dir.path().join("transaction.json"),
            &[],
            &[SymlinkChange::remove(owned.clone(), canonical)],
            |_, change| publish_symlink_change(change),
        )
        .unwrap();

        assert!(!owned.exists());
        assert!(fs::symlink_metadata(&modified)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_link(modified).unwrap(), other);
    }

    #[test]
    fn pi_family_targets_install_native_skills_and_instructions() {
        use clap::ValueEnum;

        assert_eq!(McpClient::from_str("pi", false), Ok(McpClient::Pi));
        assert_eq!(
            McpClient::from_str("prime-agent", false),
            Ok(McpClient::PrimeAgent)
        );
        let layout = ClientLayout::new(
            PathBuf::from("/home/test"),
            PathBuf::from("/home/test/.config"),
            ClientPlatform::Linux,
        );
        for (client, agent_dir, binary) in [
            (McpClient::Pi, ".pi/agent", "pi"),
            (McpClient::PrimeAgent, ".prime/agent", "prime-agent"),
        ] {
            let instructions = instruction_targets_for_layout(client, &layout);
            assert_eq!(instructions.len(), 1);
            assert_eq!(
                instructions[0].path,
                PathBuf::from(format!("/home/test/{agent_dir}/AGENTS.md"))
            );
            assert_eq!(instructions[0].format, InstructionFormat::InlineBlock);
            assert!(instruction_block(instructions[0].format)
                .contains("`ai-session-search` MCP server"));
            assert_eq!(instructions[0].detect_binaries, vec![binary]);

            let skills = skill_targets_for_layout(client, &layout);
            assert_eq!(skills.len(), 1);
            assert_eq!(
                skills[0].discovery_links,
                vec![PathBuf::from(format!(
                    "/home/test/{agent_dir}/skills/ai-session-search"
                ))]
            );
        }
    }

    #[test]
    fn instruction_targets_cover_claude_codex_gemini_antigravity_pi_prime_and_opencode() {
        let targets = instruction_targets_for(McpClient::All).unwrap();
        let labels = targets
            .iter()
            .map(|target| target.label)
            .collect::<Vec<_>>();
        assert!(labels.contains(&"claude") || !dirs::home_dir().unwrap().join(".claude").exists());
        assert!(instruction_targets_for(McpClient::Claude)
            .unwrap()
            .iter()
            .any(|target| target.path.ends_with("CLAUDE.md")));
        assert!(instruction_targets_for(McpClient::Codex)
            .unwrap()
            .iter()
            .any(|target| target.path.ends_with("AGENTS.md")
                && matches!(target.format, InstructionFormat::InlineBlock)));
        for client in [McpClient::Gemini, McpClient::Antigravity] {
            let targets = instruction_targets_for(client).unwrap();
            assert_eq!(targets.len(), 1);
            assert!(targets[0].path.ends_with(".gemini/GEMINI.md"));
            assert!(matches!(targets[0].format, InstructionFormat::InlineBlock));
        }
        for (client, suffix) in [
            (McpClient::Pi, ".pi/agent/AGENTS.md"),
            (McpClient::PrimeAgent, ".prime/agent/AGENTS.md"),
        ] {
            let targets = instruction_targets_for(client).unwrap();
            assert_eq!(targets.len(), 1);
            assert!(targets[0].path.ends_with(suffix));
            assert_eq!(targets[0].format, InstructionFormat::InlineBlock);
        }
        assert!(instruction_targets_for(McpClient::Opencode)
            .unwrap()
            .iter()
            .any(|target| target.path.ends_with("AGENTS.md")
                && matches!(target.format, InstructionFormat::InlineBlock)));
    }

    #[test]
    fn custom_targets_cover_json_vscode_and_codex_shapes() {
        let targets = custom_targets(
            &[PathBuf::from("~/json.json")],
            &[PathBuf::from("~/vscode.json")],
            &[PathBuf::from("~/zed.json")],
            &[PathBuf::from("~/opencode.json")],
            &[PathBuf::from("~/codex.toml")],
        )
        .unwrap();
        assert_eq!(targets.len(), 5);
        assert!(matches!(targets[0].format, ConfigFormat::JsonMcpServers));
        assert!(matches!(targets[1].format, ConfigFormat::VscodeServers));
        assert!(matches!(targets[2].format, ConfigFormat::ZedContextServers));
        assert!(matches!(targets[3].format, ConfigFormat::OpenCode));
        assert!(matches!(targets[4].format, ConfigFormat::CodexToml));
    }

    #[test]
    fn custom_instruction_targets_cover_claude_and_agents() {
        let targets = custom_instruction_targets(
            &[PathBuf::from("~/CLAUDE.md")],
            &[PathBuf::from("~/GEMINI.md")],
            &[PathBuf::from("~/AGENTS.md")],
        )
        .unwrap();
        assert_eq!(targets.len(), 3);
        assert_eq!(targets[0].label, "custom-claude");
        assert_eq!(targets[1].label, "custom-gemini");
        assert_eq!(targets[2].label, "custom-agents");
    }

    #[test]
    fn shared_target_selection_combines_custom_paths_and_honors_instruction_opt_out() {
        let json = [PathBuf::from("~/json.json")];
        let vscode = [PathBuf::from("~/vscode.json")];
        let zed = [PathBuf::from("~/zed.json")];
        let opencode = [PathBuf::from("~/opencode.json")];
        let codex = [PathBuf::from("~/codex.toml")];
        let claude = [PathBuf::from("~/CLAUDE.md")];
        let gemini = [PathBuf::from("~/GEMINI.md")];
        let agents = [PathBuf::from("~/AGENTS.md")];
        let selection = McpTargetSelection {
            clients: &[McpClient::Cursor],
            excluded_clients: &[],
            no_instructions: false,
            json_mcp_configs: &json,
            vscode_configs: &vscode,
            zed_configs: &zed,
            opencode_configs: &opencode,
            codex_configs: &codex,
            claude_md_paths: &claude,
            gemini_md_paths: &gemini,
            agents_md_paths: &agents,
            no_skill: false,
            skill_roots: &[],
        };

        let (targets, instructions, skills) = assemble_selected_targets(selection).unwrap();
        assert_eq!(targets.len(), 6);
        assert_eq!(instructions.len(), 3);
        assert!(skills.is_empty());

        let (targets_without_instructions, instructions, skills) =
            assemble_selected_targets(McpTargetSelection {
                no_instructions: true,
                ..selection
            })
            .unwrap();
        assert_eq!(targets_without_instructions.len(), 6);
        assert!(instructions.is_empty());
        assert!(skills.is_empty());
    }

    #[test]
    fn client_selection_supports_repeated_includes_excludes_and_detected_default() {
        assert_eq!(
            resolve_client_selection(&[McpClient::Antigravity, McpClient::Opencode], &[]).unwrap(),
            (vec![McpClient::Antigravity, McpClient::Opencode], false)
        );
        let (selected, detected_only) =
            resolve_client_selection(&[McpClient::All], &[McpClient::Opencode]).unwrap();
        assert!(detected_only);
        assert!(selected.contains(&McpClient::Antigravity));
        assert!(!selected.contains(&McpClient::Opencode));
        assert!(
            resolve_client_selection(&[McpClient::All, McpClient::Claude], &[])
                .unwrap_err()
                .to_string()
                .contains("cannot be combined")
        );
        assert!(
            resolve_client_selection(&[McpClient::All], &[McpClient::All])
                .unwrap_err()
                .to_string()
                .contains("invalid")
        );
    }

    #[test]
    fn same_path_targets_dedupe_identical_formats_and_reject_conflicting_formats() {
        let path = PathBuf::from("/tmp/shared-client-config");
        let mut identical = vec![
            test_target(path.clone(), ConfigFormat::JsonMcpServers),
            test_target(path.clone(), ConfigFormat::JsonMcpServers),
        ];
        dedupe_config_targets(&mut identical).unwrap();
        assert_eq!(identical.len(), 1);

        let mut conflicting = vec![
            test_target(path.clone(), ConfigFormat::JsonMcpServers),
            test_target(path.clone(), ConfigFormat::OpenCode),
        ];
        let error = dedupe_config_targets(&mut conflicting).unwrap_err();
        assert!(error.to_string().contains("incompatible config formats"));
        assert!(error.to_string().contains(path.to_str().unwrap()));

        let instruction = |format| InstructionTarget {
            label: "test",
            path: path.clone(),
            format,
            detect_paths: Vec::new(),
            detect_binaries: Vec::new(),
        };
        let mut conflicting = vec![
            instruction(InstructionFormat::ClaudeImport),
            instruction(InstructionFormat::InlineBlock),
        ];
        assert!(dedupe_instruction_targets(&mut conflicting)
            .unwrap_err()
            .to_string()
            .contains("incompatible Markdown ownership formats"));
    }

    #[test]
    fn all_detection_does_not_treat_home_parent_as_installed_client() {
        let dir = tempdir().unwrap();
        let target = Target {
            label: "home-level",
            path: dir.path().join(".claude.json"),
            format: ConfigFormat::JsonMcpServers,
            detect_paths: Vec::new(),
            detect_binaries: Vec::new(),
            superseded_paths: Vec::new(),
            codex_marketplace: None,
        };

        assert!(!target_detected(&target));
    }

    #[test]
    fn all_detection_uses_explicit_client_dirs() {
        let dir = tempdir().unwrap();
        let client_dir = dir.path().join(".claude");
        fs::create_dir_all(&client_dir).unwrap();
        let target = Target {
            label: "detected",
            path: dir.path().join(".claude.json"),
            format: ConfigFormat::JsonMcpServers,
            detect_paths: vec![client_dir],
            detect_binaries: Vec::new(),
            superseded_paths: Vec::new(),
            codex_marketplace: None,
        };

        assert!(target_detected(&target));
    }

    #[test]
    fn claude_targets_include_code_and_desktop_configs() {
        let targets = targets_for(McpClient::Claude).unwrap();
        assert!(targets
            .iter()
            .any(|target| target.label == "claude code modern"));
        assert!(!targets
            .iter()
            .any(|target| target.label == "claude code legacy"));
        assert!(targets
            .iter()
            .any(|target| target.label == "claude desktop"));
    }

    #[test]
    fn codex_app_cli_and_ide_share_the_user_config() {
        let targets = targets_for(McpClient::Codex).unwrap();
        assert_eq!(targets.len(), 1);
        assert!(targets[0].path.ends_with(".codex/config.toml"));
        assert!(matches!(targets[0].format, ConfigFormat::CodexToml));
        assert!(targets[0]
            .detect_paths
            .iter()
            .any(|path| path.ends_with(".codex")));
    }

    #[test]
    fn antigravity_targets_include_shared_current_and_legacy_configs() {
        let targets = targets_for(McpClient::Antigravity).unwrap();
        assert!(targets.iter().any(|target| {
            target.label == "antigravity app/cli"
                && target.path.ends_with(".gemini/config/mcp_config.json")
                && matches!(target.format, ConfigFormat::JsonMcpServers)
        }));
        assert!(targets.iter().any(|target| {
            target.label == "antigravity cli"
                && target
                    .path
                    .ends_with(".gemini/antigravity-cli/settings.json")
                && matches!(target.format, ConfigFormat::JsonMcpServers)
        }));
        assert!(targets.iter().any(|target| {
            target.label == "antigravity legacy"
                && target.path.ends_with(".gemini/antigravity/mcp_config.json")
                && matches!(target.format, ConfigFormat::JsonMcpServers)
        }));
    }

    #[test]
    fn antigravity_and_opencode_integrations_preserve_unowned_content() {
        let dir = tempdir().unwrap();
        let binary = Path::new("/bin/aise");
        let targets = [
            Target {
                label: "antigravity app/cli",
                path: dir.path().join("antigravity-current-mcp.json"),
                format: ConfigFormat::JsonMcpServers,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
                superseded_paths: Vec::new(),
                codex_marketplace: None,
            },
            Target {
                label: "antigravity cli",
                path: dir.path().join("antigravity-settings.json"),
                format: ConfigFormat::JsonMcpServers,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
                superseded_paths: Vec::new(),
                codex_marketplace: None,
            },
            Target {
                label: "antigravity legacy",
                path: dir.path().join("antigravity-mcp.json"),
                format: ConfigFormat::JsonMcpServers,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
                superseded_paths: Vec::new(),
                codex_marketplace: None,
            },
            Target {
                label: "opencode",
                path: dir.path().join("opencode.json"),
                format: ConfigFormat::OpenCode,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
                superseded_paths: Vec::new(),
                codex_marketplace: None,
            },
        ];

        for target in &targets {
            fs::write(&target.path, r#"{"keep":{"owner":"user"}}"#).unwrap();
            upsert_target(target, binary).unwrap();
            let installed = fs::read(&target.path).unwrap();
            assert_eq!(status_target(target).unwrap(), "configured");
            upsert_target(target, binary).unwrap();
            assert_eq!(fs::read(&target.path).unwrap(), installed);
            assert!(remove_target(target).unwrap());
            assert_eq!(status_target(target).unwrap(), "not configured");
            let remaining: Value =
                serde_json::from_str(&fs::read_to_string(&target.path).unwrap()).unwrap();
            assert_eq!(remaining["keep"]["owner"], "user");
        }

        for (label, file_name) in [
            ("gemini/antigravity", "GEMINI.md"),
            ("opencode", "AGENTS.md"),
        ] {
            let target = InstructionTarget {
                label,
                path: dir.path().join(file_name),
                format: InstructionFormat::InlineBlock,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
            };
            fs::write(&target.path, "# User instructions\n").unwrap();
            upsert_instruction_file(&target).unwrap();
            let installed = fs::read(&target.path).unwrap();
            assert_eq!(status_instruction_file(&target).unwrap(), "configured");
            upsert_instruction_file(&target).unwrap();
            assert_eq!(fs::read(&target.path).unwrap(), installed);
            assert!(remove_instruction_file(&target).unwrap());
            assert_eq!(
                fs::read_to_string(&target.path).unwrap(),
                "# User instructions\n"
            );
        }
    }

    fn test_target(path: PathBuf, format: ConfigFormat) -> Target {
        Target {
            label: "test",
            path,
            format,
            detect_paths: Vec::new(),
            detect_binaries: Vec::new(),
            superseded_paths: Vec::new(),
            codex_marketplace: None,
        }
    }

    #[test]
    fn upsert_rejects_non_utf8_regular_file_without_replacing_bytes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        let original = [0xff, 0xfe, 0xfd];
        fs::write(&path, original).unwrap();

        let error = upsert_json_mcp_server(&path, json!({"command": "aise"})).unwrap_err();

        assert!(error.to_string().contains("not valid UTF-8"));
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn upsert_rejects_non_regular_destination_without_replacing_it() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        fs::create_dir(&path).unwrap();

        let error = upsert_json_mcp_server(&path, json!({"command": "aise"})).unwrap_err();

        assert!(error.to_string().contains("not a regular file"));
        assert!(path.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn upsert_rejects_symbolic_link_without_replacing_target_or_link() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let target = dir.path().join("real.json");
        let link = dir.path().join("mcp.json");
        fs::write(&target, "{\"keep\":true}\n").unwrap();
        symlink(&target, &link).unwrap();

        let error = upsert_json_mcp_server(&link, json!({"command": "aise"})).unwrap_err();

        assert!(error.to_string().contains("not a regular file"));
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&target).unwrap(), "{\"keep\":true}\n");
    }

    #[test]
    fn install_preflight_error_on_later_target_leaves_every_target_unchanged() {
        let dir = tempdir().unwrap();
        let first = dir.path().join("first.json");
        let second = dir.path().join("second.json");
        let first_original = "{\"keep\":true}\n";
        let second_original = "{not-json\n";
        fs::write(&first, first_original).unwrap();
        fs::write(&second, second_original).unwrap();
        let targets = vec![
            test_target(first.clone(), ConfigFormat::JsonMcpServers),
            test_target(second.clone(), ConfigFormat::JsonMcpServers),
        ];

        let error = preflight_install(&targets, &[], &[], Path::new("aise"), None).unwrap_err();

        assert!(error.to_string().contains("failed to parse JSON"));
        assert_eq!(fs::read_to_string(first).unwrap(), first_original);
        assert_eq!(fs::read_to_string(second).unwrap(), second_original);
    }

    #[test]
    fn uninstall_preflight_error_on_later_target_leaves_every_target_unchanged() {
        let dir = tempdir().unwrap();
        let first = dir.path().join("first.json");
        let second = dir.path().join("second.json");
        let first_original = "{\"mcpServers\":{\"aise\":{\"command\":\"aise\"}},\"keep\":true}\n";
        let second_original = "[\"not-an-object\"]\n";
        fs::write(&first, first_original).unwrap();
        fs::write(&second, second_original).unwrap();
        let targets = vec![
            test_target(first.clone(), ConfigFormat::JsonMcpServers),
            test_target(second.clone(), ConfigFormat::JsonMcpServers),
        ];

        let error = preflight_uninstall(&targets, &[], &[], &[], None, false).unwrap_err();

        assert!(error.to_string().contains("must contain a JSON object"));
        assert_eq!(fs::read_to_string(first).unwrap(), first_original);
        assert_eq!(fs::read_to_string(second).unwrap(), second_original);
    }

    #[test]
    fn instruction_preflight_error_does_not_publish_valid_config_change() {
        let dir = tempdir().unwrap();
        let config = dir.path().join("mcp.json");
        let instructions = dir.path().join("AGENTS.md");
        let config_original = "{\"keep\":true}\n";
        let instructions_original = "before\n<!-- aise-instructions v1 -->\npartial\n";
        fs::write(&config, config_original).unwrap();
        fs::write(&instructions, instructions_original).unwrap();
        let targets = vec![test_target(config.clone(), ConfigFormat::JsonMcpServers)];
        let instruction_targets = vec![InstructionTarget {
            label: "test",
            path: instructions.clone(),
            format: InstructionFormat::InlineBlock,
            detect_paths: Vec::new(),
            detect_binaries: Vec::new(),
        }];

        let error = preflight_install(&targets, &instruction_targets, &[], Path::new("aise"), None)
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("no matching `<!-- /aise-instructions -->`"));
        assert_eq!(fs::read_to_string(config).unwrap(), config_original);
        assert_eq!(
            fs::read_to_string(instructions).unwrap(),
            instructions_original
        );
    }
}
