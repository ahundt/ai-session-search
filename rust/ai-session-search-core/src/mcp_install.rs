use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::{json, Map, Value};

use crate::text_file_transaction::{
    execute_text_file_transaction, publish_text_change, recover_text_file_transaction,
    recovery_guidance, snapshot_utf8_regular_file, transaction_recovery_required,
    with_text_file_transaction_read_lock, RecoveryOutcome, TextFileChange, TextFileImage,
};
use crate::util::which;

const SERVER_NAME: &str = "ai_session_search";
const LEGACY_SERVER_NAME: &str = "aise";
const INSTRUCTIONS_FILE: &str = "AI_SESSION_SEARCH.md";
const INSTRUCTIONS_REFERENCE: &str = "@AI_SESSION_SEARCH.md";
const LEGACY_INSTRUCTIONS_LINE: &str = "Before guessing about prior AI work, use aise MCP or run `aise messages search --help` to recover session history from Claude Code, Claude Desktop local agent, Codex, Cursor, Antigravity, Pi coding agent, Google AI Studio, and Gemini CLI by query, repo/path/file, message context, and time range.";
const INSTRUCTIONS_LINE: &str = "Before guessing about prior AI work, use AI Session Search (`aise`): call the `ai_session_search` MCP `search_sessions` tool to find relevant sessions or `search_messages` for message-level matches, then pass a returned session ID to `get_session`. It searches Claude Code, Claude Desktop local agent, Codex, Cursor, Antigravity, Pi coding agent, Google AI Studio, and Gemini CLI by query, repo/path/file, message context, and time range. If MCP is unavailable, run `aise messages search --help`.";
const INSTRUCTIONS_START: &str = "<!-- aise-instructions";
const INSTRUCTIONS_END: &str = "<!-- /aise-instructions -->";
const INSTRUCTIONS_FILE_START: &str = "<!-- ai-session-search-managed-file v1 -->";
const INSTRUCTIONS_FILE_END: &str = "<!-- /ai-session-search-managed-file -->";
const SKILL_MANAGED_MARKER: &str = "<!-- ai-session-search-managed-skill v1 -->";
const SKILL_CONTENT: &str = include_str!("../skills/ai-session-search/SKILL.md");

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum McpClient {
    All,
    Claude,
    Codex,
    Gemini,
    Antigravity,
    Cursor,
    Windsurf,
    Vscode,
    Zed,
    Opencode,
    Openclaw,
    Kilocode,
}

#[derive(Debug, Args)]
pub struct McpTargetsArgs {
    /// Client config to include. Repeat for multiple clients; omit for all detected clients.
    #[arg(long = "client", value_enum, default_value = "all")]
    pub clients: Vec<McpClient>,
    /// Client config to exclude from the selected set. Repeat for multiple clients.
    #[arg(long = "exclude-client", value_enum)]
    pub excluded_clients: Vec<McpClient>,
    /// Extra JSON config path using the common { "mcpServers": ... } shape.
    #[arg(long = "json-mcp-config")]
    pub json_mcp_configs: Vec<PathBuf>,
    /// Extra VS Code-style JSON config path using { "servers": ... }.
    #[arg(long = "vscode-config")]
    pub vscode_configs: Vec<PathBuf>,
    /// Extra Zed JSON config path using { "context_servers": ... }.
    #[arg(long = "zed-config")]
    pub zed_configs: Vec<PathBuf>,
    /// Extra OpenCode JSON config path using { "mcp": ... }.
    #[arg(long = "opencode-config")]
    pub opencode_configs: Vec<PathBuf>,
    /// Extra Codex-style TOML config path using [mcp_servers.ai_session_search].
    #[arg(long = "codex-config")]
    pub codex_configs: Vec<PathBuf>,
    /// Extra CLAUDE.md path where @AI_SESSION_SEARCH.md is managed.
    #[arg(long = "claude-md")]
    pub claude_md_paths: Vec<PathBuf>,
    /// Extra GEMINI.md path where the managed AI Session Search (`aise`) note is managed.
    #[arg(long = "gemini-md")]
    pub gemini_md_paths: Vec<PathBuf>,
    /// Extra AGENTS.md path where the managed AI Session Search (`aise`) note is managed.
    #[arg(long = "agents-md")]
    pub agents_md_paths: Vec<PathBuf>,
    /// Extra exact SKILL.md destination. Repeat to install the skill in custom harness roots.
    #[arg(long = "skill-path", value_name = "PATH")]
    pub skill_paths: Vec<PathBuf>,
}

impl McpTargetsArgs {
    fn resolve(
        &self,
        no_instructions: bool,
        no_skill: bool,
    ) -> Result<(Vec<Target>, Vec<InstructionTarget>, Vec<SkillTarget>)> {
        assemble_selected_targets(McpTargetSelection {
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
            skill_paths: &self.skill_paths,
        })
    }
}

#[derive(Debug, Args)]
#[command(
    after_help = "Default install configures MCP, executable aliases, managed instructions, and the AI Session Search skill for every detected client in one step. Supported MCP clients: Claude Code/Desktop, Codex, Gemini, Antigravity, Cursor, Windsurf, VS Code, Zed, OpenCode, OpenClaw, and KiloCode. Config shapes use the `ai_session_search` server key: mcpServers.ai_session_search, [mcp_servers.ai_session_search], VS Code servers.ai_session_search, Zed context_servers.ai_session_search, or OpenCode mcp.ai_session_search as appropriate. Use --no-mcp, --no-aliases, --no-instructions, or --no-skill to omit one component; --client selects specific clients; --dry-run previews every write. Claude Code gets AI_SESSION_SEARCH.md plus @AI_SESSION_SEARCH.md and ~/.claude/skills/ai-session-search/SKILL.md; Codex gets a managed AGENTS.md block and ~/.agents/skills/ai-session-search/SKILL.md; Gemini/Antigravity share managed ~/.gemini/GEMINI.md and ~/.gemini/skills/ai-session-search/SKILL.md files."
)]
pub struct McpInstallArgs {
    #[command(flatten)]
    pub targets: McpTargetsArgs,
    /// Print planned changes without writing files.
    #[arg(long)]
    pub dry_run: bool,
    /// Executable path to store in client configs. Omit to store portable `aise` after verifying
    /// the installer's PATH. GUI clients may inherit a different PATH; pass an explicit path only
    /// when that client cannot resolve `aise`.
    #[arg(long)]
    pub binary: Option<PathBuf>,
    /// Do not add AI Session Search MCP registrations to client configuration files.
    #[arg(long)]
    pub no_mcp: bool,
    /// Do not add AI Session Search (`aise`) guidance to CLAUDE.md, AGENTS.md, or GEMINI.md.
    #[arg(long)]
    pub no_instructions: bool,
    /// Do not install the AI Session Search skill for Claude, Codex, or Gemini/Antigravity.
    #[arg(long)]
    pub no_skill: bool,
    /// Do not create the `aisearch` and `ai_session_search` executable aliases beside `aise`.
    #[arg(long)]
    pub no_aliases: bool,
    #[command(flatten)]
    pub transaction: McpTransactionArgs,
}

#[derive(Debug, Args)]
#[command(
    after_help = "Status checks MCP registrations, executable aliases, managed instructions, and installed AI Session Search skills by default. Use --no-mcp, --no-aliases, --no-instructions, or --no-skill to omit one component."
)]
pub struct McpStatusArgs {
    #[command(flatten)]
    pub targets: McpTargetsArgs,
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
    #[command(flatten)]
    pub transaction: McpTransactionArgs,
}

#[derive(Debug, Args)]
#[command(
    after_help = "Uninstall removes owned MCP registrations, executable aliases, managed instructions, and AI Session Search skills by default while preserving the `aise` executable, database, cache, other client configuration, and user-authored files. Use --keep-mcp, --keep-aliases, --keep-instructions, or --keep-skill to preserve one component."
)]
pub struct McpUninstallArgs {
    #[command(flatten)]
    pub targets: McpTargetsArgs,
    /// Print planned changes without writing files.
    #[arg(long)]
    pub dry_run: bool,
    /// Preserve MCP registrations while removing other selected owned components.
    #[arg(long)]
    pub keep_mcp: bool,
    /// Preserve AI Session Search (`aise`) guidance while removing MCP registrations.
    #[arg(long = "keep-instructions")]
    pub no_instructions: bool,
    /// Preserve installed AI Session Search skills while removing other owned components.
    #[arg(long)]
    pub keep_skill: bool,
    /// Preserve executable aliases while removing MCP registrations and managed instructions.
    #[arg(long)]
    pub keep_aliases: bool,
    #[command(flatten)]
    pub transaction: McpTransactionArgs,
}

#[derive(Debug, Clone, Default, Args)]
pub struct McpTransactionArgs {
    /// Durable recovery receipt. Defaults beside the selected ai-session-search config file.
    #[arg(long, value_name = "PATH")]
    pub transaction_receipt: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct McpRecoverArgs {
    #[command(flatten)]
    pub transaction: McpTransactionArgs,
}

#[derive(Debug, Subcommand)]
pub enum McpCmd {
    /// Serve MCP JSON-RPC over standard input/output.
    Serve,
    /// Recover or finalize an interrupted MCP client configuration transaction.
    Recover(McpRecoverArgs),
}

#[derive(Debug, Parser)]
#[command(
    name = "aise mcp",
    about = "Serve and configure AI Session Search (`aise`) MCP integration"
)]
struct McpCli {
    #[command(subcommand)]
    command: McpCmd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigFormat {
    JsonMcpServers,
    CodexToml,
    VscodeServers,
    ZedContextServers,
    OpenCode,
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
    config: PathBuf,
    platform: ClientPlatform,
}

impl ClientLayout {
    fn new(home: PathBuf, config: PathBuf, platform: ClientPlatform) -> Self {
        Self {
            home,
            config,
            platform,
        }
    }

    fn from_discovered_dirs(
        home: Option<PathBuf>,
        config: Option<PathBuf>,
        platform: ClientPlatform,
    ) -> Result<Self> {
        let home = home.ok_or_else(missing_home_error)?;
        let config = config.unwrap_or_else(|| match platform {
            ClientPlatform::Macos => home.join("Library").join("Application Support"),
            ClientPlatform::Linux => home.join(".config"),
            ClientPlatform::Windows => home.join("AppData").join("Roaming"),
        });
        Ok(Self::new(home, config, platform))
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

    fn claude_desktop_config_dir(&self) -> PathBuf {
        if self.platform == ClientPlatform::Macos {
            self.home
                .join("Library")
                .join("Application Support")
                .join("Claude")
        } else {
            self.config.join("Claude")
        }
    }

    fn vscode_config_dir(&self) -> PathBuf {
        if self.platform == ClientPlatform::Macos {
            self.home
                .join("Library")
                .join("Application Support")
                .join("Code")
                .join("User")
        } else {
            self.config.join("Code").join("User")
        }
    }

    fn zed_config_dir(&self) -> PathBuf {
        if self.platform == ClientPlatform::Macos {
            self.home
                .join("Library")
                .join("Application Support")
                .join("Zed")
        } else {
            self.config.join("zed")
        }
    }
}

#[derive(Debug, Clone)]
struct Target {
    label: &'static str,
    path: PathBuf,
    format: ConfigFormat,
    detect_paths: Vec<PathBuf>,
    detect_binaries: Vec<&'static str>,
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
    path: PathBuf,
    detect_paths: Vec<PathBuf>,
    detect_binaries: Vec<&'static str>,
}

#[derive(Debug)]
struct UninstallPlan {
    mutations: Vec<PlannedFileMutation>,
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
                    "multiple MCP transformations produce conflicting changes for {}; pass each destination once",
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

fn publish_planned_mutations(mutations: &[PlannedFileMutation]) -> Result<()> {
    for mutation in mutations {
        publish_text_change(&mutation.as_change())?;
    }
    Ok(())
}

fn execute_planned_transaction(receipt: &Path, mutations: &[PlannedFileMutation]) -> Result<()> {
    let changes = mutations
        .iter()
        .map(PlannedFileMutation::as_change)
        .collect::<Vec<_>>();
    execute_text_file_transaction(receipt, &changes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstructionFormat {
    ClaudeImport,
    InlineBlock,
}

pub fn run_mcp_cmd(cmd: McpCmd) -> Result<()> {
    run_mcp_cmd_with_receipt(
        cmd,
        &default_transaction_receipt(&crate::config::Config::config_path()),
    )
}

pub(crate) fn run_mcp_cmd_with_receipt(cmd: McpCmd, default_receipt: &Path) -> Result<()> {
    match cmd {
        McpCmd::Serve => crate::mcp_server::serve(),
        McpCmd::Recover(args) => recover_with_receipt(args, default_receipt),
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
    skill_paths: &'a [PathBuf],
}

fn assemble_selected_targets(
    selection: McpTargetSelection<'_>,
) -> Result<(Vec<Target>, Vec<InstructionTarget>, Vec<SkillTarget>)> {
    let layout = ClientLayout::discover()?;
    let (clients, detected_only) =
        resolve_client_selection(selection.clients, selection.excluded_clients)?;
    let mut targets = clients
        .iter()
        .copied()
        .flat_map(|client| targets_for_layout(client, &layout))
        .filter(|target| !detected_only || target_detected(target))
        .chain(custom_targets(
            selection.json_mcp_configs,
            selection.vscode_configs,
            selection.zed_configs,
            selection.opencode_configs,
            selection.codex_configs,
        )?)
        .collect::<Vec<_>>();
    dedupe_config_targets(&mut targets)?;
    let mut instruction_targets = if selection.no_instructions {
        Vec::new()
    } else {
        clients
            .iter()
            .copied()
            .flat_map(|client| instruction_targets_for_layout(client, &layout))
            .filter(|target| !detected_only || instruction_detected(target))
            .chain(custom_instruction_targets(
                selection.claude_md_paths,
                selection.gemini_md_paths,
                selection.agents_md_paths,
            )?)
            .collect::<Vec<_>>()
    };
    dedupe_instruction_targets(&mut instruction_targets)?;
    let mut skill_targets = if selection.no_skill {
        Vec::new()
    } else {
        clients
            .iter()
            .copied()
            .flat_map(|client| skill_targets_for_layout(client, &layout))
            .filter(|target| !detected_only || skill_target_detected(target))
            .chain(custom_skill_targets(selection.skill_paths)?)
            .collect::<Vec<_>>()
    };
    dedupe_skill_targets(&mut skill_targets);
    Ok((targets, instruction_targets, skill_targets))
}

const CONCRETE_CLIENTS: [McpClient; 11] = [
    McpClient::Claude,
    McpClient::Codex,
    McpClient::Gemini,
    McpClient::Antigravity,
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
        Some(_) => {
            conflict = Some(target.path.clone());
            false
        }
    });
    if let Some(path) = conflict {
        bail!(
            "MCP destination {} was selected with incompatible config formats; pass it through exactly one format-specific option",
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

fn dedupe_skill_targets(targets: &mut Vec<SkillTarget>) {
    let mut seen = std::collections::HashSet::new();
    targets.retain(|target| seen.insert(target.path.clone()));
}

/// Parse the canonical MCP command surface for an embedded executable.
///
/// The standalone Rust CLI and Python console entrypoint share [`McpCmd`], so option names,
/// defaults, validation, and help text cannot drift between installation pathways.
pub fn parse_mcp_cmd(args: impl IntoIterator<Item = String>) -> clap::error::Result<McpCmd> {
    McpCli::try_parse_from(std::iter::once("aise mcp".to_string()).chain(args))
        .map(|cli| cli.command)
}

pub fn install(args: McpInstallArgs) -> Result<()> {
    let receipt = default_transaction_receipt(&crate::config::Config::config_path());
    install_with_receipt(args, &receipt)
}

pub(crate) fn install_with_receipt(args: McpInstallArgs, default_receipt: &Path) -> Result<()> {
    let binary = resolve_mcp_binary(args.binary.as_deref())?;
    let (mut targets, instruction_targets, skill_targets) =
        args.targets.resolve(args.no_instructions, args.no_skill)?;
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
        return Ok(());
    }
    let mutations = preflight_install(&targets, &instruction_targets, &skill_targets, &binary)?;
    let alias_guard = if args.dry_run {
        None
    } else {
        aliases.as_ref().map(|value| value.install()).transpose()?
    };
    if !args.dry_run {
        let receipt = selected_transaction_receipt(&args.transaction, default_receipt)?;
        execute_planned_transaction(&receipt, &mutations)?;
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
    for target in skill_targets {
        if args.dry_run {
            println!(
                "dry-run: would install {} skill at {}",
                target.label,
                target.path.display()
            );
        } else {
            println!(
                "installed {} skill at {}",
                target.label,
                target.path.display()
            );
        }
    }
    if args.dry_run {
        println!("dry-run: no files were modified");
    } else if has_mcp_targets {
        println!("Restart your MCP client to load AI Session Search (`aise`).");
    }
    Ok(())
}

pub fn status(args: McpStatusArgs) -> Result<()> {
    let receipt = default_transaction_receipt(&crate::config::Config::config_path());
    status_with_receipt(args, &receipt)
}

pub(crate) fn status_with_receipt(args: McpStatusArgs, default_receipt: &Path) -> Result<()> {
    let receipt = selected_transaction_receipt(&args.transaction, default_receipt)?;
    let (mut targets, instruction_targets, skill_targets) =
        args.targets.resolve(args.no_instructions, args.no_skill)?;
    if args.no_mcp {
        targets.clear();
    }
    let aliases = if args.no_aliases {
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
    let lines = with_text_file_transaction_read_lock(&receipt, || {
        ensure_no_pending_transaction(&receipt)?;
        let mut lines =
            Vec::with_capacity(targets.len() + instruction_targets.len() + skill_targets.len());
        for target in &targets {
            lines.push(format!(
                "{} {}: {}",
                target.label,
                target.path.display(),
                status_target(target)?
            ));
        }
        for target in &instruction_targets {
            lines.push(format!(
                "{} {}: {}",
                target.label,
                target.path.display(),
                status_instruction_file(target)?
            ));
        }
        for target in &skill_targets {
            lines.push(format!(
                "{} {}: {}",
                target.label,
                target.path.display(),
                status_skill_file(target)?
            ));
        }
        Ok(lines)
    })?;
    for line in lines {
        println!("{line}");
    }
    if let Some(aliases) = aliases {
        for line in aliases.status_lines()? {
            println!("{line}");
        }
    }
    Ok(())
}

fn ensure_no_pending_transaction(receipt: &Path) -> Result<()> {
    if transaction_recovery_required(receipt)? {
        bail!(
            "MCP configuration status is not authoritative while recovery receipt {} exists; {} first",
            receipt.display(),
            recovery_guidance(receipt)
        );
    }
    Ok(())
}

pub fn uninstall(args: McpUninstallArgs) -> Result<()> {
    let receipt = default_transaction_receipt(&crate::config::Config::config_path());
    uninstall_with_receipt(args, &receipt)
}

pub(crate) fn uninstall_with_receipt(args: McpUninstallArgs, default_receipt: &Path) -> Result<()> {
    let (mut targets, instruction_targets, mut skill_targets) = args
        .targets
        .resolve(args.no_instructions, args.keep_skill)?;
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
    let UninstallPlan {
        mutations,
        changed_targets,
        changed_instructions,
        changed_skills,
    } = preflight_uninstall(&targets, &instruction_targets, &skill_targets)?;
    if !args.dry_run {
        let receipt = selected_transaction_receipt(&args.transaction, default_receipt)?;
        execute_planned_transaction(&receipt, &mutations)?;
    }
    for (target, changed) in targets.into_iter().zip(changed_targets) {
        if args.dry_run {
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
        if args.dry_run {
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
    for (target, changed) in skill_targets.into_iter().zip(changed_skills) {
        if args.dry_run {
            println!(
                "dry-run: would remove {} skill from {}",
                target.label,
                target.path.display()
            );
        } else if changed {
            println!(
                "removed {} skill from {}",
                target.label,
                target.path.display()
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

pub fn recover(args: McpRecoverArgs) -> Result<()> {
    let receipt = default_transaction_receipt(&crate::config::Config::config_path());
    recover_with_receipt(args, &receipt)
}

fn recover_with_receipt(args: McpRecoverArgs, default_receipt: &Path) -> Result<()> {
    let receipt = selected_transaction_receipt(&args.transaction, default_receipt)?;
    match recover_text_file_transaction(&receipt)? {
        RecoveryOutcome::RolledBack { paths } => println!(
            "restored {paths} path(s) from interrupted MCP configuration; removed {}",
            receipt.display()
        ),
        RecoveryOutcome::Finalized { paths } => println!(
            "verified {paths} published MCP path(s); removed stale receipt {}",
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
    args: &McpTransactionArgs,
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
    match client {
        McpClient::All => CONCRETE_CLIENTS
            .into_iter()
            .flat_map(|client| targets_for_layout(client, layout))
            .filter(target_detected)
            .collect(),
        McpClient::Claude => vec![
            json_target_with_detect(
                "claude code modern",
                layout.home.join(".claude.json"),
                vec![layout.home.join(".claude")],
                vec!["claude"],
            ),
            json_target_with_detect(
                "claude code legacy",
                layout.home.join(".claude").join(".mcp.json"),
                vec![layout.home.join(".claude")],
                vec!["claude"],
            ),
            json_target_with_detect(
                "claude desktop",
                layout
                    .claude_desktop_config_dir()
                    .join("claude_desktop_config.json"),
                vec![layout.claude_desktop_config_dir()],
                Vec::new(),
            ),
        ],
        McpClient::Codex => vec![Target {
            label: "codex",
            path: layout.home.join(".codex").join("config.toml"),
            format: ConfigFormat::CodexToml,
            detect_paths: vec![layout.home.join(".codex")],
            detect_binaries: vec!["codex"],
        }],
        McpClient::Gemini => vec![json_target_with_detect(
            "gemini",
            layout.home.join(".gemini").join("settings.json"),
            vec![layout.home.join(".gemini")],
            vec!["gemini"],
        )],
        McpClient::Antigravity => vec![
            json_target_with_detect(
                "antigravity cli",
                layout
                    .home
                    .join(".gemini")
                    .join("antigravity-cli")
                    .join("settings.json"),
                vec![layout.home.join(".gemini").join("antigravity-cli")],
                vec!["agy"],
            ),
            json_target_with_detect(
                "antigravity legacy",
                layout
                    .home
                    .join(".gemini")
                    .join("antigravity")
                    .join("mcp_config.json"),
                vec![layout.home.join(".gemini").join("antigravity")],
                Vec::new(),
            ),
        ],
        McpClient::Cursor => vec![json_target(
            layout,
            "cursor",
            layout.home.join(".cursor").join("mcp.json"),
        )],
        McpClient::Windsurf => vec![json_target(
            layout,
            "windsurf",
            layout
                .home
                .join(".codeium")
                .join("windsurf")
                .join("mcp_config.json"),
        )],
        McpClient::Vscode => vec![Target {
            label: "vscode",
            path: layout.vscode_config_dir().join("mcp.json"),
            format: ConfigFormat::VscodeServers,
            detect_paths: vec![layout.vscode_config_dir()],
            detect_binaries: vec!["code"],
        }],
        McpClient::Zed => vec![Target {
            label: "zed",
            path: layout.zed_config_dir().join("settings.json"),
            format: ConfigFormat::ZedContextServers,
            detect_paths: vec![layout.zed_config_dir()],
            detect_binaries: vec!["zed"],
        }],
        McpClient::Opencode => vec![Target {
            label: "opencode",
            // OpenCode intentionally uses this XDG-style location on every supported OS.
            path: layout
                .home
                .join(".config")
                .join("opencode")
                .join("opencode.json"),
            format: ConfigFormat::OpenCode,
            detect_paths: vec![layout.home.join(".config").join("opencode")],
            detect_binaries: vec!["opencode"],
        }],
        McpClient::Openclaw => vec![json_target(
            layout,
            "openclaw",
            layout.home.join(".openclaw").join("openclaw.json"),
        )],
        McpClient::Kilocode => vec![json_target(
            layout,
            "kilocode legacy vscode extension",
            layout
                .vscode_config_dir()
                .join("globalStorage")
                .join("kilocode.kilo-code")
                .join("settings")
                .join("mcp_settings.json"),
        )],
    }
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
    let targets = match client {
        McpClient::All => vec![
            InstructionTarget {
                label: "claude",
                path: layout.home.join(".claude").join("CLAUDE.md"),
                format: InstructionFormat::ClaudeImport,
                detect_paths: vec![layout.home.join(".claude")],
                detect_binaries: vec!["claude"],
            },
            InstructionTarget {
                label: "codex",
                path: layout.home.join(".codex").join("AGENTS.md"),
                format: InstructionFormat::InlineBlock,
                detect_paths: vec![layout.home.join(".codex")],
                detect_binaries: vec!["codex"],
            },
            gemini_instruction_target(layout),
            InstructionTarget {
                label: "opencode",
                path: layout
                    .home
                    .join(".config")
                    .join("opencode")
                    .join("AGENTS.md"),
                format: InstructionFormat::InlineBlock,
                detect_paths: vec![layout.home.join(".config").join("opencode")],
                detect_binaries: vec!["opencode"],
            },
        ],
        McpClient::Claude => vec![InstructionTarget {
            label: "claude",
            path: layout.home.join(".claude").join("CLAUDE.md"),
            format: InstructionFormat::ClaudeImport,
            detect_paths: vec![layout.home.join(".claude")],
            detect_binaries: vec!["claude"],
        }],
        McpClient::Codex => vec![InstructionTarget {
            label: "codex",
            path: layout.home.join(".codex").join("AGENTS.md"),
            format: InstructionFormat::InlineBlock,
            detect_paths: vec![layout.home.join(".codex")],
            detect_binaries: vec!["codex"],
        }],
        McpClient::Gemini | McpClient::Antigravity => {
            vec![gemini_instruction_target(layout)]
        }
        McpClient::Opencode => vec![InstructionTarget {
            label: "opencode",
            path: layout
                .home
                .join(".config")
                .join("opencode")
                .join("AGENTS.md"),
            format: InstructionFormat::InlineBlock,
            detect_paths: vec![layout.home.join(".config").join("opencode")],
            detect_binaries: vec!["opencode"],
        }],
        _ => Vec::new(),
    };

    if client == McpClient::All {
        targets.into_iter().filter(instruction_detected).collect()
    } else {
        targets
    }
}

fn gemini_instruction_target(layout: &ClientLayout) -> InstructionTarget {
    InstructionTarget {
        label: "gemini/antigravity",
        path: layout.home.join(".gemini").join("GEMINI.md"),
        format: InstructionFormat::InlineBlock,
        detect_paths: vec![layout.home.join(".gemini")],
        detect_binaries: vec!["gemini", "agy"],
    }
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
    match client {
        McpClient::Claude => vec![skill_target(
            "claude",
            layout
                .home
                .join(".claude")
                .join("skills")
                .join("ai-session-search")
                .join("SKILL.md"),
            vec![layout.home.join(".claude")],
            vec!["claude"],
        )],
        McpClient::Codex => vec![skill_target(
            "codex",
            layout
                .home
                .join(".agents")
                .join("skills")
                .join("ai-session-search")
                .join("SKILL.md"),
            vec![layout.home.join(".codex"), layout.home.join(".agents")],
            vec!["codex"],
        )],
        McpClient::Gemini | McpClient::Antigravity => vec![skill_target(
            "gemini/antigravity",
            layout
                .home
                .join(".gemini")
                .join("skills")
                .join("ai-session-search")
                .join("SKILL.md"),
            vec![layout.home.join(".gemini")],
            vec!["gemini", "agy"],
        )],
        _ => Vec::new(),
    }
}

fn skill_target(
    label: &'static str,
    path: PathBuf,
    detect_paths: Vec<PathBuf>,
    detect_binaries: Vec<&'static str>,
) -> SkillTarget {
    SkillTarget {
        label,
        path,
        detect_paths,
        detect_binaries,
    }
}

fn skill_target_detected(target: &SkillTarget) -> bool {
    target.path.exists()
        || target.detect_paths.iter().any(|path| path.is_dir())
        || target
            .detect_binaries
            .iter()
            .any(|binary| which(binary).is_some())
}

fn custom_skill_targets(paths: &[PathBuf]) -> Result<Vec<SkillTarget>> {
    paths
        .iter()
        .map(|path| {
            Ok(skill_target(
                "custom",
                expand_tilde(path)?,
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
            })
        })
        .chain(vscode_configs.iter().map(|path| {
            Ok(Target {
                label: "custom-vscode",
                path: expand_tilde(path)?,
                format: ConfigFormat::VscodeServers,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
            })
        }))
        .chain(zed_configs.iter().map(|path| {
            Ok(Target {
                label: "custom-zed",
                path: expand_tilde(path)?,
                format: ConfigFormat::ZedContextServers,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
            })
        }))
        .chain(opencode_configs.iter().map(|path| {
            Ok(Target {
                label: "custom-opencode",
                path: expand_tilde(path)?,
                format: ConfigFormat::OpenCode,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
            })
        }))
        .chain(codex_configs.iter().map(|path| {
            Ok(Target {
                label: "custom-codex",
                path: expand_tilde(path)?,
                format: ConfigFormat::CodexToml,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
            })
        }))
        .collect()
}

fn json_target(layout: &ClientLayout, label: &'static str, path: PathBuf) -> Target {
    let detect_paths = path
        .parent()
        .filter(|parent| *parent != layout.home.as_path())
        .map(|parent| vec![parent.to_path_buf()])
        .unwrap_or_default();
    json_target_with_detect(label, path, detect_paths, Vec::new())
}

fn json_target_with_detect(
    label: &'static str,
    path: PathBuf,
    detect_paths: Vec<PathBuf>,
    detect_binaries: Vec<&'static str>,
) -> Target {
    Target {
        label,
        path,
        format: ConfigFormat::JsonMcpServers,
        detect_paths,
        detect_binaries,
    }
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
) -> Result<Vec<PlannedFileMutation>> {
    let mut mutations = Vec::new();
    for target in targets {
        mutations.extend(plan_upsert_target(target, binary)?);
    }
    for target in instruction_targets {
        mutations.extend(plan_upsert_instruction_file(target)?);
    }
    for target in skill_targets {
        mutations.extend(plan_upsert_skill_file(target)?);
    }
    normalize_planned_mutations(mutations)
}

fn preflight_uninstall(
    targets: &[Target],
    instruction_targets: &[InstructionTarget],
    skill_targets: &[SkillTarget],
) -> Result<UninstallPlan> {
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
    for target in skill_targets {
        let planned = plan_remove_skill_file(target)?;
        changed_skills.push(!planned.is_empty());
        mutations.extend(planned);
    }
    Ok(UninstallPlan {
        mutations: normalize_planned_mutations(mutations)?,
        changed_targets,
        changed_instructions,
        changed_skills,
    })
}

fn plan_upsert_skill_file(target: &SkillTarget) -> Result<Vec<PlannedFileMutation>> {
    let original = read_optional_utf8_regular_file(&target.path)?;
    if let Some(text) = original.as_deref() {
        if !text.contains(SKILL_MANAGED_MARKER) {
            bail!(
                "refusing to replace unmanaged AI Session Search skill {}; move it or choose another --skill-path",
                target.path.display()
            );
        }
    }
    Ok(vec![planned_write(
        &target.path,
        &original,
        SKILL_CONTENT.to_string(),
    )])
}

fn plan_remove_skill_file(target: &SkillTarget) -> Result<Vec<PlannedFileMutation>> {
    let Some(original) = read_optional_utf8_regular_file(&target.path)? else {
        return Ok(Vec::new());
    };
    if !original.contains(SKILL_MANAGED_MARKER) {
        bail!(
            "refusing to remove unmanaged AI Session Search skill {}",
            target.path.display()
        );
    }
    Ok(vec![PlannedFileMutation::Remove {
        path: target.path.clone(),
        original,
    }])
}

fn status_skill_file(target: &SkillTarget) -> Result<&'static str> {
    Ok(
        match read_optional_utf8_regular_file(&target.path)?.as_deref() {
            None => "missing",
            Some(text) if text == SKILL_CONTENT => "configured",
            Some(text) if text.contains(SKILL_MANAGED_MARKER) => "outdated",
            Some(_) => "modified or unmanaged",
        },
    )
}

fn plan_upsert_target(target: &Target, binary: &Path) -> Result<Vec<PlannedFileMutation>> {
    let binary = binary_config_value(binary)?;
    match target.format {
        ConfigFormat::JsonMcpServers => plan_upsert_keyed_json_server(
            &target.path,
            "mcpServers",
            json!({
                "command": binary,
                "args": ["mcp", "serve"]
            }),
        ),
        ConfigFormat::CodexToml => plan_upsert_codex_mcp_server(&target.path, binary),
        ConfigFormat::VscodeServers => plan_upsert_keyed_json_server(
            &target.path,
            "servers",
            json!({
                "type": "stdio",
                "command": binary,
                "args": ["mcp", "serve"]
            }),
        ),
        ConfigFormat::ZedContextServers => plan_upsert_keyed_json_server(
            &target.path,
            "context_servers",
            json!({
                "command": binary,
                "args": ["mcp", "serve"]
            }),
        ),
        ConfigFormat::OpenCode => plan_upsert_keyed_json_server(
            &target.path,
            "mcp",
            json!({
                "command": [binary, "mcp", "serve"],
                "enabled": true
            }),
        ),
    }
}

#[cfg(test)]
fn remove_target(target: &Target) -> Result<bool> {
    let mutations = normalize_planned_mutations(plan_remove_target(target)?)?;
    let changed = !mutations.is_empty();
    publish_planned_mutations(&mutations)?;
    Ok(changed)
}

fn plan_remove_target(target: &Target) -> Result<Vec<PlannedFileMutation>> {
    match target.format {
        ConfigFormat::JsonMcpServers => plan_remove_keyed_json_server(&target.path, "mcpServers"),
        ConfigFormat::CodexToml => plan_remove_codex_mcp_server(&target.path),
        ConfigFormat::VscodeServers => plan_remove_keyed_json_server(&target.path, "servers"),
        ConfigFormat::ZedContextServers => {
            plan_remove_keyed_json_server(&target.path, "context_servers")
        }
        ConfigFormat::OpenCode => plan_remove_keyed_json_server(&target.path, "mcp"),
    }
}

fn status_target(target: &Target) -> Result<&'static str> {
    match target.format {
        ConfigFormat::JsonMcpServers => {
            status_json_keyed_server(&target.path, "mcpServers", target.format)
        }
        ConfigFormat::CodexToml => status_codex_mcp_server(&target.path),
        ConfigFormat::VscodeServers => {
            status_json_keyed_server(&target.path, "servers", target.format)
        }
        ConfigFormat::ZedContextServers => {
            status_json_keyed_server(&target.path, "context_servers", target.format)
        }
        ConfigFormat::OpenCode => status_json_keyed_server(&target.path, "mcp", target.format),
    }
}

pub fn upsert_json_mcp_server(path: &Path, entry: Value) -> Result<()> {
    upsert_keyed_json_server(path, "mcpServers", entry)
}

pub fn remove_json_mcp_server(path: &Path) -> Result<bool> {
    remove_keyed_json_server(path, "mcpServers")
}

fn upsert_keyed_json_server(path: &Path, container_key: &str, entry: Value) -> Result<()> {
    let mutations =
        normalize_planned_mutations(plan_upsert_keyed_json_server(path, container_key, entry)?)?;
    publish_planned_mutations(&mutations)
}

fn plan_upsert_keyed_json_server(
    path: &Path,
    container_key: &str,
    entry: Value,
) -> Result<Vec<PlannedFileMutation>> {
    let original = read_optional_utf8_regular_file(path)?;
    let mut root = parse_json_object_or_empty(path, original.as_deref())?;
    let servers = root
        .entry(container_key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(servers) = servers.as_object_mut() else {
        return Err(anyhow!("{} has non-object {container_key}", path.display()));
    };
    servers.remove(LEGACY_SERVER_NAME);
    servers.insert(SERVER_NAME.to_string(), entry);
    let content = serde_json::to_string_pretty(&Value::Object(root))? + "\n";
    Ok(vec![planned_write(path, &original, content)])
}

fn remove_keyed_json_server(path: &Path, container_key: &str) -> Result<bool> {
    let mutations =
        normalize_planned_mutations(plan_remove_keyed_json_server(path, container_key)?)?;
    let changed = !mutations.is_empty();
    publish_planned_mutations(&mutations)?;
    Ok(changed)
}

fn plan_remove_keyed_json_server(
    path: &Path,
    container_key: &str,
) -> Result<Vec<PlannedFileMutation>> {
    let original = read_optional_utf8_regular_file(path)?;
    let Some(text) = original.as_deref() else {
        return Ok(Vec::new());
    };
    let mut root = parse_json_object_or_empty(path, Some(text))?;
    let removed = root
        .get_mut(container_key)
        .and_then(Value::as_object_mut)
        .and_then(|servers| {
            let current = servers.remove(SERVER_NAME);
            let legacy = servers.remove(LEGACY_SERVER_NAME);
            current.or(legacy)
        })
        .is_some();
    if removed {
        let content = serde_json::to_string_pretty(&Value::Object(root))? + "\n";
        Ok(vec![planned_write(path, &original, content)])
    } else {
        Ok(Vec::new())
    }
}

pub fn upsert_codex_mcp_server(path: &Path, binary: &Path) -> Result<()> {
    let binary = binary_config_value(binary)?;
    publish_planned_mutations(&normalize_planned_mutations(plan_upsert_codex_mcp_server(
        path, binary,
    )?)?)
}

fn plan_upsert_codex_mcp_server(path: &Path, binary: &str) -> Result<Vec<PlannedFileMutation>> {
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
    servers.remove(LEGACY_SERVER_NAME);
    servers.insert(SERVER_NAME, toml_edit::Item::Table(server));

    let content = document.to_string();
    parse_codex_document(path, &content).with_context(|| {
        format!(
            "refusing to publish generated Codex TOML for {}",
            path.display()
        )
    })?;
    Ok(vec![planned_write(path, &original, content)])
}

pub fn remove_codex_mcp_server(path: &Path) -> Result<bool> {
    let mutations = normalize_planned_mutations(plan_remove_codex_mcp_server(path)?)?;
    let changed = !mutations.is_empty();
    publish_planned_mutations(&mutations)?;
    Ok(changed)
}

fn plan_remove_codex_mcp_server(path: &Path) -> Result<Vec<PlannedFileMutation>> {
    let original = read_optional_utf8_regular_file(path)?;
    let Some(text) = original.as_deref() else {
        return Ok(Vec::new());
    };
    let mut document = parse_codex_document(path, text)?;
    let removed = document
        .get_mut("mcp_servers")
        .and_then(toml_edit::Item::as_table_mut)
        .and_then(|servers| {
            let current = servers.remove(SERVER_NAME);
            let legacy = servers.remove(LEGACY_SERVER_NAME);
            current.or(legacy)
        })
        .is_some();
    if !removed {
        Ok(Vec::new())
    } else {
        let content = document.to_string();
        parse_codex_document(path, &content).with_context(|| {
            format!(
                "refusing to publish generated Codex TOML for {}",
                path.display()
            )
        })?;
        Ok(vec![planned_write(path, &original, content)])
    }
}

fn parse_codex_document(path: &Path, text: &str) -> Result<toml_edit::DocumentMut> {
    text.parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("failed to parse TOML in {}", path.display()))
}

fn binary_config_value(binary: &Path) -> Result<&str> {
    binary.to_str().ok_or_else(|| {
        anyhow!(
            "MCP executable path {} is not valid UTF-8 and cannot be represented in JSON or TOML client configuration; install aise at a UTF-8 path or omit --binary to store the portable `aise` command",
            binary.display()
        )
    })
}

fn parse_json_object_or_empty(path: &Path, text: Option<&str>) -> Result<Map<String, Value>> {
    let text = text.unwrap_or_default();
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(text)
        .with_context(|| format!("failed to parse JSON in {}", path.display()))?
    {
        Value::Object(map) => Ok(map),
        _ => Err(anyhow!("{} must contain a JSON object", path.display())),
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
        ConfigFormat::OpenCode => {
            entry.get("enabled").and_then(Value::as_bool) == Some(true)
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
        | ConfigFormat::VscodeServers
        | ConfigFormat::ZedContextServers => {
            let command_is_set = entry
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|command| !command.is_empty());
            let type_is_valid = !matches!(format, ConfigFormat::VscodeServers)
                || entry.get("type").and_then(Value::as_str) == Some("stdio");
            command_is_set
                && type_is_valid
                && json_array_is_strings(entry.get("args"), &["mcp", "serve"])
        }
        ConfigFormat::CodexToml => false,
    }
}

fn status_json_keyed_server(
    path: &Path,
    container_key: &str,
    format: ConfigFormat,
) -> Result<&'static str> {
    let text = read_optional_utf8_regular_file(path)?;
    if text.is_none() {
        return Ok("missing");
    }
    let root = parse_json_object_or_empty(path, text.as_deref())?;
    let servers = root.get(container_key).and_then(Value::as_object);
    let entry = servers.and_then(|servers| servers.get(SERVER_NAME));
    Ok(match entry {
        Some(entry) if json_entry_is_current(entry, format) => "configured",
        Some(_) => "stale",
        None if servers.is_some_and(|servers| servers.contains_key(LEGACY_SERVER_NAME)) => "stale",
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
        None if servers.is_some_and(|servers| servers.contains_key(LEGACY_SERVER_NAME)) => "stale",
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
        InstructionFormat::InlineBlock => plan_upsert_inline_instruction_file(&target.path),
    }
}

fn plan_remove_instruction_file(target: &InstructionTarget) -> Result<Vec<PlannedFileMutation>> {
    match target.format {
        InstructionFormat::ClaudeImport => plan_remove_claude_instruction_file(&target.path),
        InstructionFormat::InlineBlock => plan_remove_inline_instruction_file(&target.path),
    }
}

fn status_instruction_file(target: &InstructionTarget) -> Result<&'static str> {
    match target.format {
        InstructionFormat::ClaudeImport => status_claude_instruction_file(&target.path),
        InstructionFormat::InlineBlock => status_inline_instruction_file(&target.path),
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

fn plan_upsert_inline_instruction_file(path: &Path) -> Result<Vec<PlannedFileMutation>> {
    let original = read_optional_utf8_regular_file(path)?;
    let next = upsert_inline_instruction_text(original.as_deref().unwrap_or_default())?;
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

fn status_inline_instruction_file(path: &Path) -> Result<&'static str> {
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
        if starts == 1 && ends == 1 && text[start..end].trim_end() == instruction_block().trim_end()
        {
            "configured"
        } else {
            "outdated"
        },
    )
}

fn upsert_inline_instruction_text(text: &str) -> Result<String> {
    let without_inline = remove_inline_instruction_block(text)?.unwrap_or_else(|| text.to_string());
    let removed = remove_instruction_reference(&without_inline)?.unwrap_or(without_inline);
    let mut next = removed.trim_end().to_string();
    if !next.is_empty() {
        next.push_str("\n\n");
    }
    next.push_str(&instruction_block());
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
                    "found aise instruction end marker without start marker"
                ));
            }
            break;
        };
        if end.is_some_and(|end| end < start) {
            return Err(anyhow!(
                "found aise instruction end marker before start marker"
            ));
        }
        let end_relative = next[start..]
            .find(INSTRUCTIONS_END)
            .ok_or_else(|| anyhow!("found aise instruction start marker without end marker"))?;
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
            "refusing to replace unmanaged instruction file {}",
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

fn is_current_instruction_file(text: &str) -> bool {
    text.trim_end() == instruction_file_content().trim_end()
}

fn is_managed_instruction_file(text: &str) -> bool {
    if text.trim_end() == legacy_instruction_file_content().trim_end() {
        return true;
    }
    let trimmed = text.trim_end();
    let Some(managed) = trimmed.strip_prefix("# AI Session Search (`aise`)\n\n") else {
        return false;
    };
    managed.starts_with(INSTRUCTIONS_FILE_START) && managed.ends_with(INSTRUCTIONS_FILE_END)
}

fn instruction_block() -> String {
    format!("<!-- aise-instructions v1 -->\n{INSTRUCTIONS_LINE}\n{INSTRUCTIONS_END}\n")
}

fn resolve_mcp_binary(explicit: Option<&Path>) -> Result<PathBuf> {
    let resolved = if let Some(path) = explicit {
        return validate_mcp_binary(absolutize(&expand_tilde(path)?)?);
    } else {
        which("aise").ok_or_else(|| anyhow!("aise is not on PATH; pass --binary /path/to/aise"))?;
        PathBuf::from("aise")
    };
    Ok(resolved)
}

fn validate_mcp_binary(path: PathBuf) -> Result<PathBuf> {
    let metadata = fs::metadata(&path).with_context(|| {
        format!(
            "MCP binary {} does not exist or is not accessible",
            path.display()
        )
    })?;
    if !metadata.is_file() {
        return Err(anyhow!("MCP binary {} is not a file", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(anyhow!("MCP binary {} is not executable", path.display()));
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

fn expand_tilde(path: &Path) -> Result<PathBuf> {
    if path == Path::new("~") {
        home_dir()
    } else if let Ok(rest) = path.strip_prefix(Path::new("~")) {
        Ok(home_dir()?.join(rest))
    } else {
        Ok(path.to_path_buf())
    }
}

fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(missing_home_error)
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
            assert_eq!(kilocode.len(), 1);
            assert_eq!(kilocode[0].path, PathBuf::from(expected_kilocode));
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
            r#"{"mcpServers":{"aise":{"command":"legacy"},"other":{"command":"other"}},"keep":true}"#,
        )
        .unwrap();

        upsert_json_mcp_server(&path, json!({"command": "/bin/aise"})).unwrap();
        let data: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let servers = data["mcpServers"].as_object().unwrap();
        assert!(servers.contains_key("other"));
        assert!(!servers.contains_key(LEGACY_SERVER_NAME));
        assert_eq!(servers[SERVER_NAME]["command"], "/bin/aise");
        assert_eq!(data["keep"], true);
    }

    #[test]
    fn uninstall_json_preserves_other_servers_and_root_keys() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        fs::write(
            &path,
            r#"{"mcpServers":{"aise":{"command":"/old"},"other":{"command":"other"}},"keep":true}"#,
        )
        .unwrap();

        assert!(remove_json_mcp_server(&path).unwrap());
        let data: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let servers = data["mcpServers"].as_object().unwrap();
        assert!(!servers.contains_key("aise"));
        assert!(servers.contains_key("other"));
        assert_eq!(data["keep"], true);
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

    #[test]
    fn opencode_uses_command_array() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("opencode.json");
        upsert_target(
            &Target {
                label: "opencode",
                path: path.clone(),
                format: ConfigFormat::OpenCode,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
            },
            Path::new("/bin/aise"),
        )
        .unwrap();
        let data: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(data["mcp"][SERVER_NAME]["command"][0], "/bin/aise");
        assert_eq!(data["mcp"][SERVER_NAME]["command"][1], "mcp");
        assert_eq!(data["mcp"][SERVER_NAME]["command"][2], "serve");
        assert_eq!(data["mcp"][SERVER_NAME]["enabled"], true);
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
            },
            Target {
                label: "vscode",
                path: dir.path().join("vscode.json"),
                format: ConfigFormat::VscodeServers,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
            },
            Target {
                label: "zed",
                path: dir.path().join("zed.json"),
                format: ConfigFormat::ZedContextServers,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
            },
            Target {
                label: "opencode",
                path: dir.path().join("opencode.json"),
                format: ConfigFormat::OpenCode,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
            },
            Target {
                label: "codex",
                path: dir.path().join("config.toml"),
                format: ConfigFormat::CodexToml,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
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

    #[test]
    fn codex_upsert_is_idempotent_and_preserves_other_sections() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[existing]\nvalue = true\n").unwrap();

        upsert_codex_mcp_server(&path, Path::new("/bin/aise")).unwrap();
        upsert_codex_mcp_server(&path, Path::new("/bin/aise")).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[existing]\nvalue = true"));
        assert_eq!(text.matches("[mcp_servers.ai_session_search]").count(), 1);
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
            status_json_keyed_server(&json_path, "mcpServers", ConfigFormat::JsonMcpServers)
                .unwrap(),
            "stale"
        );

        let opencode_path = dir.path().join("opencode.json");
        fs::write(
            &opencode_path,
            r#"{"mcp":{"aise":{"command":["aise-mcp"],"enabled":true}}}"#,
        )
        .unwrap();
        assert_eq!(
            status_json_keyed_server(&opencode_path, "mcp", ConfigFormat::OpenCode).unwrap(),
            "stale"
        );

        let codex_path = dir.path().join("config.toml");
        fs::write(&codex_path, "[mcp_servers.aise]\ncommand = \"aise-mcp\"\n").unwrap();
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
        let binary = dir.path().join("aise");
        fs::write(&binary, "not executable").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o644)).unwrap();

        let error = resolve_mcp_binary(Some(&binary)).unwrap_err();

        assert!(error.to_string().contains("not executable"));
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
        let first = upsert_inline_instruction_text(original).unwrap();
        assert!(first.contains(instruction_block().trim_end()));
        assert!(first.contains("# Team rules"));

        let stale = first.replace(INSTRUCTIONS_LINE, "old wording");
        let updated = upsert_inline_instruction_text(&stale).unwrap();
        assert!(updated.contains(INSTRUCTIONS_LINE));
        assert!(!updated.contains("old wording"));
        assert_eq!(updated.matches(INSTRUCTIONS_START).count(), 1);
    }

    #[test]
    fn inline_instruction_upsert_collapses_duplicates_and_uninstall_removes_every_block() {
        let duplicated = format!(
            "# Team rules\n\n{}Keep this.\n\n{}",
            instruction_block(),
            instruction_block()
        );

        let updated = upsert_inline_instruction_text(&duplicated).unwrap();
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
        let input = format!("# Team rules\n\n{}Keep this.\n", instruction_block());
        let output = remove_inline_instruction_block(&input).unwrap().unwrap();
        assert!(output.contains("# Team rules"));
        assert!(output.contains("Keep this."));
        assert!(output.contains("# Team rules\n\nKeep this."));
        assert!(!output.contains(INSTRUCTIONS_START));
    }

    #[test]
    fn inline_instruction_remove_rejects_malformed_block() {
        let err = remove_inline_instruction_block("before\n<!-- aise-instructions v1 -->\npartial")
            .unwrap_err();
        assert!(err.to_string().contains("without end marker"));

        let err = remove_inline_instruction_block("before\n<!-- /aise-instructions -->\nafter\n")
            .unwrap_err();
        assert!(err.to_string().contains("without start marker"));
    }

    #[test]
    fn instruction_content_names_product_and_gives_an_exact_mcp_workflow() {
        let content = instruction_file_content();
        assert!(content.contains("AI Session Search (`aise`)"));
        for tool in ["`search_sessions`", "`search_messages`", "`get_session`"] {
            assert!(content.contains(tool), "missing {tool}: {content}");
        }
        for provider in [
            "Claude Code",
            "Claude Desktop local agent",
            "Codex",
            "Cursor",
            "Antigravity",
            "Pi coding agent",
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
            format!("# Team rules\n\n{}Keep this.\n", instruction_block()),
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
        fs::write(&agents_md, instruction_block()).unwrap();
        assert_eq!(status_instruction_file(&target).unwrap(), "configured");
        fs::write(
            &agents_md,
            instruction_block().replace(INSTRUCTIONS_LINE, LEGACY_INSTRUCTIONS_LINE),
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

    #[test]
    fn managed_skill_content_uses_current_product_surface() {
        assert!(SKILL_CONTENT.starts_with("---\nname: ai-session-search\n"));
        assert!(SKILL_CONTENT.contains(SKILL_MANAGED_MARKER));
        assert!(SKILL_CONTENT.contains("aise messages search"));
        assert!(SKILL_CONTENT.contains("query_session_index"));
        assert!(!SKILL_CONTENT.contains("aise messages inspect"));
        assert!(!SKILL_CONTENT.contains("aise tools search"));
    }

    #[test]
    fn managed_skill_lifecycle_refuses_unowned_files() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("skills/ai-session-search/SKILL.md");
        let target = skill_target("test", path.clone(), Vec::new(), Vec::new());

        publish_planned_mutations(
            &normalize_planned_mutations(plan_upsert_skill_file(&target).unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), SKILL_CONTENT);
        assert_eq!(status_skill_file(&target).unwrap(), "configured");

        fs::write(&path, format!("{SKILL_MANAGED_MARKER}\nold\n")).unwrap();
        assert_eq!(status_skill_file(&target).unwrap(), "outdated");
        publish_planned_mutations(
            &normalize_planned_mutations(plan_upsert_skill_file(&target).unwrap()).unwrap(),
        )
        .unwrap();
        publish_planned_mutations(
            &normalize_planned_mutations(plan_remove_skill_file(&target).unwrap()).unwrap(),
        )
        .unwrap();
        assert!(!path.exists());

        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "---\nname: user-skill\n---\n").unwrap();
        assert!(plan_upsert_skill_file(&target)
            .unwrap_err()
            .to_string()
            .contains("unmanaged"));
        assert!(plan_remove_skill_file(&target)
            .unwrap_err()
            .to_string()
            .contains("unmanaged"));
        assert_eq!(status_skill_file(&target).unwrap(), "modified or unmanaged");
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
        assert_eq!(
            claude[0].path,
            PathBuf::from("/home/test/.claude/skills/ai-session-search/SKILL.md")
        );
        assert_eq!(
            codex[0].path,
            PathBuf::from("/home/test/.agents/skills/ai-session-search/SKILL.md")
        );
        assert_eq!(gemini[0].path, antigravity[0].path);
        assert_eq!(
            gemini[0].path,
            PathBuf::from("/home/test/.gemini/skills/ai-session-search/SKILL.md")
        );
        assert!(skill_targets_for_layout(McpClient::Opencode, &layout).is_empty());
    }

    #[test]
    fn instruction_targets_cover_claude_codex_gemini_antigravity_and_opencode() {
        let targets = instruction_targets_for(McpClient::All).unwrap();
        let labels = targets
            .iter()
            .map(|target| target.label)
            .collect::<Vec<_>>();
        assert!(labels.contains(&"claude") || !home_dir().unwrap().join(".claude").exists());
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
            skill_paths: &[],
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
    fn recover_command_accepts_explicit_transaction_receipt() {
        let command = parse_mcp_cmd([
            "recover".to_string(),
            "--transaction-receipt".to_string(),
            "/tmp/mcp-receipt.json".to_string(),
        ])
        .unwrap();

        let McpCmd::Recover(args) = command else {
            panic!("expected recover command");
        };
        assert_eq!(
            args.transaction.transaction_receipt,
            Some(PathBuf::from("/tmp/mcp-receipt.json"))
        );
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
        };

        assert!(target_detected(&target));
    }

    #[test]
    fn claude_targets_include_code_and_desktop_configs() {
        let targets = targets_for(McpClient::Claude).unwrap();
        assert!(targets
            .iter()
            .any(|target| target.label == "claude code modern"));
        assert!(targets
            .iter()
            .any(|target| target.label == "claude code legacy"));
        assert!(targets
            .iter()
            .any(|target| target.label == "claude desktop"));
    }

    #[test]
    fn antigravity_targets_include_cli_settings_and_legacy_config() {
        let targets = targets_for(McpClient::Antigravity).unwrap();
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
                label: "antigravity cli",
                path: dir.path().join("antigravity-settings.json"),
                format: ConfigFormat::JsonMcpServers,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
            },
            Target {
                label: "antigravity legacy",
                path: dir.path().join("antigravity-mcp.json"),
                format: ConfigFormat::JsonMcpServers,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
            },
            Target {
                label: "opencode",
                path: dir.path().join("opencode.json"),
                format: ConfigFormat::OpenCode,
                detect_paths: Vec::new(),
                detect_binaries: Vec::new(),
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

        let error = preflight_install(&targets, &[], &[], Path::new("aise")).unwrap_err();

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

        let error = preflight_uninstall(&targets, &[], &[]).unwrap_err();

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

        let error =
            preflight_install(&targets, &instruction_targets, &[], Path::new("aise")).unwrap_err();

        assert!(error.to_string().contains("without end marker"));
        assert_eq!(fs::read_to_string(config).unwrap(), config_original);
        assert_eq!(
            fs::read_to_string(instructions).unwrap(),
            instructions_original
        );
    }
}
