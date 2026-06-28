use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{json, Map, Value};

const SERVER_NAME: &str = "sessiongrep";
const INSTRUCTIONS_FILE: &str = "SESSIONGREP.md";
const INSTRUCTIONS_REFERENCE: &str = "@SESSIONGREP.md";
const INSTRUCTIONS_LINE: &str = "Before guessing about prior AI work, use sessiongrep MCP or run `sessiongrep messages search --help` to recover Claude/Codex/Cursor/etc session history by query, repo/path/file, message context, and time range.";
const INSTRUCTIONS_START: &str = "<!-- sessiongrep-instructions";
const INSTRUCTIONS_END: &str = "<!-- /sessiongrep-instructions -->";

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
#[command(
    after_help = "Default install updates every detected client config: Claude Code/Desktop, Codex, Gemini, Antigravity, Cursor, Windsurf, VS Code, Zed, OpenCode, OpenClaw, and KiloCode. Config shapes are mcpServers.sessiongrep, [mcp_servers.sessiongrep], VS Code servers.sessiongrep, Zed context_servers.sessiongrep, or OpenCode mcp.sessiongrep as appropriate. Use --client to create/update one client, --dry-run to preview writes, and custom config flags for arbitrary compatible locations. Claude Code gets SESSIONGREP.md plus @SESSIONGREP.md; Codex/OpenCode get a managed AGENTS.md block because their AGENTS.md loaders read literal text."
)]
pub struct McpInstallArgs {
    /// Client config to update.
    #[arg(long, value_enum, default_value_t = McpClient::All)]
    pub client: McpClient,
    /// Print planned changes without writing files.
    #[arg(long)]
    pub dry_run: bool,
    /// Path to the sessiongrep-mcp binary. Defaults to the current executable
    /// when run as sessiongrep-mcp, otherwise the first sessiongrep-mcp on PATH.
    #[arg(long)]
    pub binary: Option<PathBuf>,
    /// Extra JSON config path using the common { "mcpServers": ... } shape.
    #[arg(long = "json-mcp-config")]
    pub json_mcp_configs: Vec<PathBuf>,
    /// Extra VS Code-style JSON config path using { "servers": ... }.
    #[arg(long = "vscode-config")]
    pub vscode_configs: Vec<PathBuf>,
    /// Extra Codex-style TOML config path using [mcp_servers.sessiongrep].
    #[arg(long = "codex-config")]
    pub codex_configs: Vec<PathBuf>,
    /// Do not add sessiongrep guidance to CLAUDE.md or AGENTS.md.
    #[arg(long)]
    pub no_instructions: bool,
    /// Extra CLAUDE.md path where @SESSIONGREP.md should be upserted.
    #[arg(long = "claude-md")]
    pub claude_md_paths: Vec<PathBuf>,
    /// Extra AGENTS.md path where the managed sessiongrep note should be upserted.
    #[arg(long = "agents-md")]
    pub agents_md_paths: Vec<PathBuf>,
}

#[derive(Debug, Args)]
#[command(
    after_help = "Status checks detected or explicit MCP config files plus managed sessiongrep instruction entries unless --no-instructions is set."
)]
pub struct McpStatusArgs {
    /// Client config to inspect.
    #[arg(long, value_enum, default_value_t = McpClient::All)]
    pub client: McpClient,
    /// Extra JSON config path using the common { "mcpServers": ... } shape.
    #[arg(long = "json-mcp-config")]
    pub json_mcp_configs: Vec<PathBuf>,
    /// Extra VS Code-style JSON config path using { "servers": ... }.
    #[arg(long = "vscode-config")]
    pub vscode_configs: Vec<PathBuf>,
    /// Extra Codex-style TOML config path using [mcp_servers.sessiongrep].
    #[arg(long = "codex-config")]
    pub codex_configs: Vec<PathBuf>,
    /// Do not inspect CLAUDE.md or AGENTS.md instruction files.
    #[arg(long)]
    pub no_instructions: bool,
    /// Extra CLAUDE.md path to inspect.
    #[arg(long = "claude-md")]
    pub claude_md_paths: Vec<PathBuf>,
    /// Extra AGENTS.md path to inspect.
    #[arg(long = "agents-md")]
    pub agents_md_paths: Vec<PathBuf>,
}

#[derive(Debug, Args)]
#[command(
    after_help = "Uninstall removes only the sessiongrep MCP entry and the managed sessiongrep instruction reference/block. Other client config and user instructions are preserved."
)]
pub struct McpUninstallArgs {
    /// Client config to update.
    #[arg(long, value_enum, default_value_t = McpClient::All)]
    pub client: McpClient,
    /// Print planned changes without writing files.
    #[arg(long)]
    pub dry_run: bool,
    /// Extra JSON config path using the common { "mcpServers": ... } shape.
    #[arg(long = "json-mcp-config")]
    pub json_mcp_configs: Vec<PathBuf>,
    /// Extra VS Code-style JSON config path using { "servers": ... }.
    #[arg(long = "vscode-config")]
    pub vscode_configs: Vec<PathBuf>,
    /// Extra Codex-style TOML config path using [mcp_servers.sessiongrep].
    #[arg(long = "codex-config")]
    pub codex_configs: Vec<PathBuf>,
    /// Do not remove sessiongrep guidance from CLAUDE.md or AGENTS.md.
    #[arg(long)]
    pub no_instructions: bool,
    /// Extra CLAUDE.md path where @SESSIONGREP.md should be removed.
    #[arg(long = "claude-md")]
    pub claude_md_paths: Vec<PathBuf>,
    /// Extra AGENTS.md path where the managed sessiongrep note should be removed.
    #[arg(long = "agents-md")]
    pub agents_md_paths: Vec<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum McpCmd {
    /// Register sessiongrep-mcp with supported MCP clients.
    Install(McpInstallArgs),
    /// Show whether supported MCP clients are configured.
    Status(McpStatusArgs),
    /// Remove sessiongrep-mcp from supported MCP clients.
    Uninstall(McpUninstallArgs),
}

#[derive(Debug, Clone, Copy)]
enum ConfigFormat {
    JsonMcpServers,
    CodexToml,
    VscodeServers,
    ZedContextServers,
    OpenCode,
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

#[derive(Debug, Clone, Copy)]
enum InstructionFormat {
    ClaudeImport,
    InlineBlock,
}

pub fn run_mcp_cmd(cmd: McpCmd) -> Result<()> {
    match cmd {
        McpCmd::Install(args) => install(args),
        McpCmd::Status(args) => status(args),
        McpCmd::Uninstall(args) => uninstall(args),
    }
}

pub fn install(args: McpInstallArgs) -> Result<()> {
    let binary = resolve_mcp_binary(args.binary.as_deref())?;
    let client = args.client;
    let targets = targets_for(args.client)
        .into_iter()
        .chain(custom_targets(
            &args.json_mcp_configs,
            &args.vscode_configs,
            &args.codex_configs,
        ))
        .collect::<Vec<_>>();
    let instruction_targets = if args.no_instructions {
        Vec::new()
    } else {
        instruction_targets_for(client)
            .into_iter()
            .chain(custom_instruction_targets(
                &args.claude_md_paths,
                &args.agents_md_paths,
            ))
            .collect::<Vec<_>>()
    };
    if targets.is_empty() && instruction_targets.is_empty() {
        println!(
            "No supported MCP client config was detected. Use --client or a custom config path to create one."
        );
        return Ok(());
    }
    for target in targets {
        if args.dry_run {
            println!(
                "dry-run: would upsert {} MCP server in {}",
                target.label,
                target.path.display()
            );
        } else {
            upsert_target(&target, &binary)?;
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
            upsert_instruction_file(&target)?;
            println!(
                "configured {} instruction guidance in {}",
                target.label,
                target.path.display()
            );
        }
    }
    if args.dry_run {
        println!("dry-run: no files were modified");
    } else {
        println!("Restart your MCP client to load sessiongrep.");
    }
    Ok(())
}

pub fn status(args: McpStatusArgs) -> Result<()> {
    let client = args.client;
    let targets = targets_for(args.client)
        .into_iter()
        .chain(custom_targets(
            &args.json_mcp_configs,
            &args.vscode_configs,
            &args.codex_configs,
        ))
        .collect::<Vec<_>>();
    let instruction_targets = if args.no_instructions {
        Vec::new()
    } else {
        instruction_targets_for(client)
            .into_iter()
            .chain(custom_instruction_targets(
                &args.claude_md_paths,
                &args.agents_md_paths,
            ))
            .collect::<Vec<_>>()
    };
    if targets.is_empty() && instruction_targets.is_empty() {
        println!("No supported MCP client config was detected.");
        return Ok(());
    }
    for target in targets {
        println!(
            "{} {}: {}",
            target.label,
            target.path.display(),
            status_target(&target)?
        );
    }
    for target in instruction_targets {
        println!(
            "{} {}: {}",
            target.label,
            target.path.display(),
            status_instruction_file(&target)?
        );
    }
    Ok(())
}

pub fn uninstall(args: McpUninstallArgs) -> Result<()> {
    let client = args.client;
    let targets = targets_for(args.client)
        .into_iter()
        .chain(custom_targets(
            &args.json_mcp_configs,
            &args.vscode_configs,
            &args.codex_configs,
        ))
        .collect::<Vec<_>>();
    let instruction_targets = if args.no_instructions {
        Vec::new()
    } else {
        instruction_targets_for(client)
            .into_iter()
            .chain(custom_instruction_targets(
                &args.claude_md_paths,
                &args.agents_md_paths,
            ))
            .collect::<Vec<_>>()
    };
    if targets.is_empty() && instruction_targets.is_empty() {
        println!("No supported MCP client config was detected.");
        return Ok(());
    }
    for target in targets {
        if args.dry_run {
            println!(
                "dry-run: would remove {} MCP server from {}",
                target.label,
                target.path.display()
            );
        } else if remove_target(&target)? {
            println!(
                "removed {} MCP server from {}",
                target.label,
                target.path.display()
            );
        }
    }
    for target in instruction_targets {
        if args.dry_run {
            println!(
                "dry-run: would remove {} instruction guidance from {}",
                target.label,
                target.path.display()
            );
        } else if remove_instruction_file(&target)? {
            println!(
                "removed {} instruction guidance from {}",
                target.label,
                target.path.display()
            );
        }
    }
    if args.dry_run {
        println!("dry-run: no files were modified");
    }
    Ok(())
}

fn targets_for(client: McpClient) -> Vec<Target> {
    match client {
        McpClient::All => [
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
        ]
        .into_iter()
        .flat_map(targets_for)
        .filter(target_detected)
        .collect(),
        McpClient::Claude => vec![
            json_target_with_detect(
                "claude code modern",
                home_dir().join(".claude.json"),
                vec![home_dir().join(".claude")],
                vec!["claude"],
            ),
            json_target_with_detect(
                "claude code legacy",
                home_dir().join(".claude").join(".mcp.json"),
                vec![home_dir().join(".claude")],
                vec!["claude"],
            ),
            json_target_with_detect(
                "claude desktop",
                claude_desktop_config_path(),
                vec![claude_desktop_config_dir()],
                Vec::new(),
            ),
        ],
        McpClient::Codex => vec![Target {
            label: "codex",
            path: home_dir().join(".codex").join("config.toml"),
            format: ConfigFormat::CodexToml,
            detect_paths: vec![home_dir().join(".codex")],
            detect_binaries: vec!["codex"],
        }],
        McpClient::Gemini => vec![json_target_with_detect(
            "gemini",
            home_dir().join(".gemini").join("settings.json"),
            vec![home_dir().join(".gemini")],
            vec!["gemini"],
        )],
        McpClient::Antigravity => vec![json_target_with_detect(
            "antigravity",
            home_dir()
                .join(".gemini")
                .join("antigravity")
                .join("mcp_config.json"),
            vec![home_dir().join(".gemini").join("antigravity")],
            Vec::new(),
        )],
        McpClient::Cursor => vec![json_target(
            "cursor",
            home_dir().join(".cursor").join("mcp.json"),
        )],
        McpClient::Windsurf => vec![json_target(
            "windsurf",
            home_dir()
                .join(".codeium")
                .join("windsurf")
                .join("mcp_config.json"),
        )],
        McpClient::Vscode => vec![Target {
            label: "vscode",
            path: vscode_config_path(),
            format: ConfigFormat::VscodeServers,
            detect_paths: vec![vscode_config_dir()],
            detect_binaries: vec!["code"],
        }],
        McpClient::Zed => vec![Target {
            label: "zed",
            path: zed_config_path(),
            format: ConfigFormat::ZedContextServers,
            detect_paths: vec![zed_config_dir()],
            detect_binaries: vec!["zed"],
        }],
        McpClient::Opencode => vec![Target {
            label: "opencode",
            path: home_dir()
                .join(".config")
                .join("opencode")
                .join("opencode.json"),
            format: ConfigFormat::OpenCode,
            detect_paths: vec![home_dir().join(".config").join("opencode")],
            detect_binaries: vec!["opencode"],
        }],
        McpClient::Openclaw => vec![json_target(
            "openclaw",
            home_dir().join(".openclaw").join("openclaw.json"),
        )],
        McpClient::Kilocode => vec![json_target(
            "kilocode",
            home_dir()
                .join(".config")
                .join("Code")
                .join("User")
                .join("globalStorage")
                .join("kilocode.kilo-code")
                .join("settings")
                .join("mcp_settings.json"),
        )],
    }
}

fn instruction_targets_for(client: McpClient) -> Vec<InstructionTarget> {
    let targets = match client {
        McpClient::All => vec![
            InstructionTarget {
                label: "claude",
                path: home_dir().join(".claude").join("CLAUDE.md"),
                format: InstructionFormat::ClaudeImport,
                detect_paths: vec![home_dir().join(".claude")],
                detect_binaries: vec!["claude"],
            },
            InstructionTarget {
                label: "codex",
                path: home_dir().join(".codex").join("AGENTS.md"),
                format: InstructionFormat::InlineBlock,
                detect_paths: vec![home_dir().join(".codex")],
                detect_binaries: vec!["codex"],
            },
            InstructionTarget {
                label: "opencode",
                path: home_dir()
                    .join(".config")
                    .join("opencode")
                    .join("AGENTS.md"),
                format: InstructionFormat::InlineBlock,
                detect_paths: vec![home_dir().join(".config").join("opencode")],
                detect_binaries: vec!["opencode"],
            },
        ],
        McpClient::Claude => vec![InstructionTarget {
            label: "claude",
            path: home_dir().join(".claude").join("CLAUDE.md"),
            format: InstructionFormat::ClaudeImport,
            detect_paths: vec![home_dir().join(".claude")],
            detect_binaries: vec!["claude"],
        }],
        McpClient::Codex => vec![InstructionTarget {
            label: "codex",
            path: home_dir().join(".codex").join("AGENTS.md"),
            format: InstructionFormat::InlineBlock,
            detect_paths: vec![home_dir().join(".codex")],
            detect_binaries: vec!["codex"],
        }],
        McpClient::Opencode => vec![InstructionTarget {
            label: "opencode",
            path: home_dir()
                .join(".config")
                .join("opencode")
                .join("AGENTS.md"),
            format: InstructionFormat::InlineBlock,
            detect_paths: vec![home_dir().join(".config").join("opencode")],
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
    agents_md_paths: &[PathBuf],
) -> Vec<InstructionTarget> {
    claude_md_paths
        .iter()
        .map(|path| InstructionTarget {
            label: "custom-claude",
            path: expand_tilde(path),
            format: InstructionFormat::ClaudeImport,
            detect_paths: Vec::new(),
            detect_binaries: Vec::new(),
        })
        .chain(agents_md_paths.iter().map(|path| InstructionTarget {
            label: "custom-agents",
            path: expand_tilde(path),
            format: InstructionFormat::InlineBlock,
            detect_paths: Vec::new(),
            detect_binaries: Vec::new(),
        }))
        .collect()
}

fn custom_targets(
    json_mcp_configs: &[PathBuf],
    vscode_configs: &[PathBuf],
    codex_configs: &[PathBuf],
) -> Vec<Target> {
    json_mcp_configs
        .iter()
        .map(|path| Target {
            label: "custom-json-mcp",
            path: expand_tilde(path),
            format: ConfigFormat::JsonMcpServers,
            detect_paths: Vec::new(),
            detect_binaries: Vec::new(),
        })
        .chain(vscode_configs.iter().map(|path| Target {
            label: "custom-vscode",
            path: expand_tilde(path),
            format: ConfigFormat::VscodeServers,
            detect_paths: Vec::new(),
            detect_binaries: Vec::new(),
        }))
        .chain(codex_configs.iter().map(|path| Target {
            label: "custom-codex",
            path: expand_tilde(path),
            format: ConfigFormat::CodexToml,
            detect_paths: Vec::new(),
            detect_binaries: Vec::new(),
        }))
        .collect()
}

fn json_target(label: &'static str, path: PathBuf) -> Target {
    let home = home_dir();
    let detect_paths = path
        .parent()
        .filter(|parent| *parent != home.as_path())
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

fn upsert_target(target: &Target, binary: &Path) -> Result<()> {
    match target.format {
        ConfigFormat::JsonMcpServers => upsert_json_mcp_server(
            &target.path,
            json!({
                "command": binary.display().to_string(),
                "args": []
            }),
        ),
        ConfigFormat::CodexToml => upsert_codex_mcp_server(&target.path, binary),
        ConfigFormat::VscodeServers => upsert_keyed_json_server(
            &target.path,
            "servers",
            json!({
                "type": "stdio",
                "command": binary.display().to_string(),
                "args": []
            }),
        ),
        ConfigFormat::ZedContextServers => upsert_keyed_json_server(
            &target.path,
            "context_servers",
            json!({
                "command": binary.display().to_string(),
                "args": []
            }),
        ),
        ConfigFormat::OpenCode => upsert_keyed_json_server(
            &target.path,
            "mcp",
            json!({
                "command": [binary.display().to_string()],
                "enabled": true
            }),
        ),
    }
}

fn remove_target(target: &Target) -> Result<bool> {
    match target.format {
        ConfigFormat::JsonMcpServers => remove_json_mcp_server(&target.path),
        ConfigFormat::CodexToml => remove_codex_mcp_server(&target.path),
        ConfigFormat::VscodeServers => remove_keyed_json_server(&target.path, "servers"),
        ConfigFormat::ZedContextServers => {
            remove_keyed_json_server(&target.path, "context_servers")
        }
        ConfigFormat::OpenCode => remove_keyed_json_server(&target.path, "mcp"),
    }
}

fn status_target(target: &Target) -> Result<&'static str> {
    match target.format {
        ConfigFormat::JsonMcpServers => status_json_keyed_server(&target.path, "mcpServers"),
        ConfigFormat::CodexToml => status_codex_mcp_server(&target.path),
        ConfigFormat::VscodeServers => status_json_keyed_server(&target.path, "servers"),
        ConfigFormat::ZedContextServers => {
            status_json_keyed_server(&target.path, "context_servers")
        }
        ConfigFormat::OpenCode => status_json_keyed_server(&target.path, "mcp"),
    }
}

pub fn upsert_json_mcp_server(path: &Path, entry: Value) -> Result<()> {
    upsert_keyed_json_server(path, "mcpServers", entry)
}

pub fn remove_json_mcp_server(path: &Path) -> Result<bool> {
    remove_keyed_json_server(path, "mcpServers")
}

fn upsert_keyed_json_server(path: &Path, container_key: &str, entry: Value) -> Result<()> {
    let mut root = read_json_object_or_empty(path)?;
    let servers = root
        .entry(container_key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(servers) = servers.as_object_mut() else {
        return Err(anyhow!("{} has non-object {container_key}", path.display()));
    };
    servers.insert(SERVER_NAME.to_string(), entry);
    write_json(path, &Value::Object(root))
}

fn remove_keyed_json_server(path: &Path, container_key: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let mut root = read_json_object_or_empty(path)?;
    let removed = root
        .get_mut(container_key)
        .and_then(Value::as_object_mut)
        .and_then(|servers| servers.remove(SERVER_NAME))
        .is_some();
    if removed {
        write_json(path, &Value::Object(root))?;
    }
    Ok(removed)
}

pub fn upsert_codex_mcp_server(path: &Path, binary: &Path) -> Result<()> {
    let original = fs::read_to_string(path).unwrap_or_default();
    let without_old = remove_codex_section_text(&original);
    let section = format!(
        "[mcp_servers.{SERVER_NAME}]\ncommand = \"{}\"\nstartup_timeout_sec = 30.0\n",
        escape_toml_string(&binary.display().to_string())
    );
    let mut next = without_old.trim_end().to_string();
    if !next.is_empty() {
        next.push_str("\n\n");
    }
    next.push_str(&section);
    write_text(path, &(next + "\n"))
}

pub fn remove_codex_mcp_server(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let original =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let next = remove_codex_section_text(&original);
    let removed = next != original;
    if removed {
        write_text(path, &next)?;
    }
    Ok(removed)
}

fn read_json_object_or_empty(path: &Path) -> Result<Map<String, Value>> {
    if !path.exists() {
        return Ok(Map::new());
    }
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(&text)
        .with_context(|| format!("failed to parse JSON in {}", path.display()))?
    {
        Value::Object(map) => Ok(map),
        _ => Err(anyhow!("{} must contain a JSON object", path.display())),
    }
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    let text = serde_json::to_string_pretty(value)? + "\n";
    write_text(path, &text)
}

fn write_text(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))
}

fn status_json_keyed_server(path: &Path, container_key: &str) -> Result<&'static str> {
    if !path.exists() {
        return Ok("missing");
    }
    let root = read_json_object_or_empty(path)?;
    Ok(
        if root
            .get(container_key)
            .and_then(Value::as_object)
            .is_some_and(|servers| servers.contains_key(SERVER_NAME))
        {
            "configured"
        } else {
            "not configured"
        },
    )
}

fn status_codex_mcp_server(path: &Path) -> Result<&'static str> {
    if !path.exists() {
        return Ok("missing");
    }
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(if text.contains(&format!("[mcp_servers.{SERVER_NAME}]")) {
        "configured"
    } else {
        "not configured"
    })
}

fn upsert_instruction_file(target: &InstructionTarget) -> Result<()> {
    match target.format {
        InstructionFormat::ClaudeImport => upsert_claude_instruction_file(&target.path),
        InstructionFormat::InlineBlock => upsert_inline_instruction_file(&target.path),
    }
}

fn remove_instruction_file(target: &InstructionTarget) -> Result<bool> {
    match target.format {
        InstructionFormat::ClaudeImport => remove_claude_instruction_file(&target.path),
        InstructionFormat::InlineBlock => remove_inline_instruction_file(&target.path),
    }
}

fn status_instruction_file(target: &InstructionTarget) -> Result<&'static str> {
    match target.format {
        InstructionFormat::ClaudeImport => status_claude_instruction_file(&target.path),
        InstructionFormat::InlineBlock => status_inline_instruction_file(&target.path),
    }
}

fn upsert_claude_instruction_file(path: &Path) -> Result<()> {
    let original = fs::read_to_string(path).unwrap_or_default();
    let next = upsert_claude_instruction_text(&original)?;
    write_sessiongrep_instruction_file(path)?;
    write_text(path, &next)
}

fn remove_claude_instruction_file(path: &Path) -> Result<bool> {
    if !path.exists() {
        return remove_sessiongrep_instruction_file(path);
    }
    let original =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let reference_removed = if let Some(next) = remove_claude_instruction_text(&original)? {
        write_text(path, &next)?;
        true
    } else {
        false
    };
    let file_removed = remove_sessiongrep_instruction_file(path)?;
    Ok(reference_removed || file_removed)
}

fn status_claude_instruction_file(path: &Path) -> Result<&'static str> {
    if !path.exists() {
        return Ok("missing");
    }
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let instruction_file = sessiongrep_instruction_path(path);
    Ok(
        if text.lines().any(is_instruction_reference_line) && instruction_file.exists() {
            "configured"
        } else {
            "not configured"
        },
    )
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

fn upsert_inline_instruction_file(path: &Path) -> Result<()> {
    let original = fs::read_to_string(path).unwrap_or_default();
    let next = upsert_inline_instruction_text(&original)?;
    remove_sessiongrep_instruction_file(path)?;
    write_text(path, &next)
}

fn remove_inline_instruction_file(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let original =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let removed_inline = remove_inline_instruction_block(&original)?;
    let base = removed_inline.as_deref().unwrap_or(&original);
    let removed_reference = remove_instruction_reference(base)?;
    let Some(next) = removed_reference.or(removed_inline) else {
        return remove_sessiongrep_instruction_file(path);
    };
    write_text(path, &next)?;
    remove_sessiongrep_instruction_file(path)?;
    Ok(true)
}

fn status_inline_instruction_file(path: &Path) -> Result<&'static str> {
    if !path.exists() {
        return Ok("missing");
    }
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(
        if text.contains(INSTRUCTIONS_START) && text.contains(INSTRUCTIONS_END) {
            "configured"
        } else {
            "not configured"
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
    let Some(start) = text.find(INSTRUCTIONS_START) else {
        return Ok(None);
    };
    let end_relative = text[start..]
        .find(INSTRUCTIONS_END)
        .ok_or_else(|| anyhow!("found sessiongrep instruction start marker without end marker"))?;
    let mut end = start + end_relative + INSTRUCTIONS_END.len();
    if text[end..].starts_with('\n') {
        end += 1;
    }
    let mut next = String::with_capacity(text.len() - (end - start));
    next.push_str(&text[..start]);
    next.push_str(&text[end..]);
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

fn write_sessiongrep_instruction_file(instruction_ref_path: &Path) -> Result<()> {
    write_text(
        &sessiongrep_instruction_path(instruction_ref_path),
        &instruction_file_content(),
    )
}

fn remove_sessiongrep_instruction_file(instruction_ref_path: &Path) -> Result<bool> {
    let path = sessiongrep_instruction_path(instruction_ref_path);
    if !path.exists() {
        return Ok(false);
    }
    let text =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    if text.trim_end() != instruction_file_content().trim_end() {
        return Ok(false);
    }
    fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
    Ok(true)
}

fn sessiongrep_instruction_path(instruction_ref_path: &Path) -> PathBuf {
    instruction_ref_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(INSTRUCTIONS_FILE)
}

fn instruction_file_content() -> String {
    format!("# sessiongrep\n\n{INSTRUCTIONS_LINE}\n")
}

fn instruction_block() -> String {
    format!("<!-- sessiongrep-instructions v1 -->\n{INSTRUCTIONS_LINE}\n{INSTRUCTIONS_END}\n")
}

fn remove_codex_section_text(text: &str) -> String {
    let header = format!("[mcp_servers.{SERVER_NAME}]");
    let mut out = Vec::new();
    let mut skipping = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == header {
            skipping = true;
            continue;
        }
        if skipping && trimmed.starts_with('[') && trimmed.ends_with(']') {
            skipping = false;
        }
        if !skipping {
            out.push(line);
        }
    }
    let mut result = out.join("\n");
    if text.ends_with('\n') && !result.is_empty() {
        result.push('\n');
    }
    result
}

fn resolve_mcp_binary(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return absolutize(&expand_tilde(path));
    }
    let current = env::current_exe().context("failed to get current executable")?;
    if current
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("sessiongrep-mcp"))
    {
        return Ok(current);
    }
    which("sessiongrep-mcp").ok_or_else(|| {
        anyhow!("sessiongrep-mcp is not on PATH; pass --binary /path/to/sessiongrep-mcp")
    })
}

fn which(binary: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    let names = executable_names(binary);
    env::split_paths(&paths)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

fn executable_names(binary: &str) -> Vec<String> {
    let mut names = vec![binary.to_string()];
    if cfg!(windows) && !binary.contains('.') {
        names.extend(["exe", "cmd", "bat"].map(|ext| format!("{binary}.{ext}")));
    }
    names
}

fn absolutize(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

fn expand_tilde(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        home_dir()
    } else if let Some(rest) = text.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        path.to_path_buf()
    }
}

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| home_dir().join(".config"))
}

fn claude_desktop_config_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        home_dir()
            .join("Library")
            .join("Application Support")
            .join("Claude")
    } else {
        config_dir().join("Claude")
    }
}

fn claude_desktop_config_path() -> PathBuf {
    claude_desktop_config_dir().join("claude_desktop_config.json")
}

fn vscode_config_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        home_dir()
            .join("Library")
            .join("Application Support")
            .join("Code")
            .join("User")
    } else {
        config_dir().join("Code").join("User")
    }
}

fn vscode_config_path() -> PathBuf {
    vscode_config_dir().join("mcp.json")
}

fn zed_config_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        home_dir()
            .join("Library")
            .join("Application Support")
            .join("Zed")
    } else {
        config_dir().join("zed")
    }
}

fn zed_config_path() -> PathBuf {
    zed_config_dir().join("settings.json")
}

fn escape_toml_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn upsert_json_preserves_existing_servers() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        fs::write(
            &path,
            r#"{"mcpServers":{"other":{"command":"other"}},"keep":true}"#,
        )
        .unwrap();

        upsert_json_mcp_server(&path, json!({"command": "/bin/sessiongrep-mcp"})).unwrap();
        let data: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let servers = data["mcpServers"].as_object().unwrap();
        assert!(servers.contains_key("other"));
        assert_eq!(servers["sessiongrep"]["command"], "/bin/sessiongrep-mcp");
        assert_eq!(data["keep"], true);
    }

    #[test]
    fn uninstall_json_preserves_other_servers_and_root_keys() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        fs::write(
            &path,
            r#"{"mcpServers":{"sessiongrep":{"command":"/old"},"other":{"command":"other"}},"keep":true}"#,
        )
        .unwrap();

        assert!(remove_json_mcp_server(&path).unwrap());
        let data: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let servers = data["mcpServers"].as_object().unwrap();
        assert!(!servers.contains_key("sessiongrep"));
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
            Path::new("/bin/sessiongrep-mcp"),
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
            Path::new("/bin/sessiongrep-mcp"),
        )
        .unwrap();

        let vscode_data: Value =
            serde_json::from_str(&fs::read_to_string(vscode).unwrap()).unwrap();
        assert_eq!(vscode_data["servers"]["sessiongrep"]["type"], "stdio");
        assert_eq!(
            vscode_data["servers"]["sessiongrep"]["command"],
            "/bin/sessiongrep-mcp"
        );
        let zed_data: Value = serde_json::from_str(&fs::read_to_string(zed).unwrap()).unwrap();
        assert_eq!(
            zed_data["context_servers"]["sessiongrep"]["command"],
            "/bin/sessiongrep-mcp"
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
            Path::new("/bin/sessiongrep-mcp"),
        )
        .unwrap();
        let data: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(
            data["mcp"]["sessiongrep"]["command"][0],
            "/bin/sessiongrep-mcp"
        );
        assert_eq!(data["mcp"]["sessiongrep"]["enabled"], true);
    }

    #[test]
    fn remove_target_matches_each_config_shape() {
        let dir = tempdir().unwrap();
        let binary = Path::new("/bin/sessiongrep-mcp");
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

        upsert_codex_mcp_server(&path, Path::new("/bin/sessiongrep-mcp")).unwrap();
        upsert_codex_mcp_server(&path, Path::new("/bin/sessiongrep-mcp")).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[existing]\nvalue = true"));
        assert_eq!(text.matches("[mcp_servers.sessiongrep]").count(), 1);
        assert!(text.contains("command = \"/bin/sessiongrep-mcp\""));
        assert!(text.contains("startup_timeout_sec = 30.0"));
    }

    #[test]
    fn remove_codex_section_preserves_following_sections() {
        let input = "[a]\nx = 1\n\n[mcp_servers.sessiongrep]\ncommand = \"/old\"\n\n[b]\ny = 2\n";
        let output = remove_codex_section_text(input);
        assert!(output.contains("[a]\nx = 1"));
        assert!(output.contains("[b]\ny = 2"));
        assert!(!output.contains("mcp_servers.sessiongrep"));
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
        let err = remove_inline_instruction_block(
            "before\n<!-- sessiongrep-instructions v1 -->\npartial",
        )
        .unwrap_err();
        assert!(err.to_string().contains("without end marker"));
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
        fs::write(&agents_md, "# Team rules\n\n@SESSIONGREP.md\n").unwrap();
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
    fn instruction_targets_cover_claude_codex_and_opencode() {
        let targets = instruction_targets_for(McpClient::All);
        let labels = targets
            .iter()
            .map(|target| target.label)
            .collect::<Vec<_>>();
        assert!(labels.contains(&"claude") || !home_dir().join(".claude").exists());
        assert!(instruction_targets_for(McpClient::Claude)
            .iter()
            .any(|target| target.path.ends_with("CLAUDE.md")));
        assert!(instruction_targets_for(McpClient::Codex)
            .iter()
            .any(|target| target.path.ends_with("AGENTS.md")
                && matches!(target.format, InstructionFormat::InlineBlock)));
        assert!(instruction_targets_for(McpClient::Opencode)
            .iter()
            .any(|target| target.path.ends_with("AGENTS.md")
                && matches!(target.format, InstructionFormat::InlineBlock)));
    }

    #[test]
    fn custom_targets_cover_json_vscode_and_codex_shapes() {
        let targets = custom_targets(
            &[PathBuf::from("~/json.json")],
            &[PathBuf::from("~/vscode.json")],
            &[PathBuf::from("~/codex.toml")],
        );
        assert_eq!(targets.len(), 3);
        assert!(matches!(targets[0].format, ConfigFormat::JsonMcpServers));
        assert!(matches!(targets[1].format, ConfigFormat::VscodeServers));
        assert!(matches!(targets[2].format, ConfigFormat::CodexToml));
    }

    #[test]
    fn custom_instruction_targets_cover_claude_and_agents() {
        let targets = custom_instruction_targets(
            &[PathBuf::from("~/CLAUDE.md")],
            &[PathBuf::from("~/AGENTS.md")],
        );
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].label, "custom-claude");
        assert_eq!(targets[1].label, "custom-agents");
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
        let targets = targets_for(McpClient::Claude);
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
}
