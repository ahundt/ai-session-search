use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::analysis_pipeline::AnalysisPolicySpec;
use crate::analysis_publication::{AnalysisPublicationFormat, AnalysisPublicationPlan};
use crate::config::{Config, ConfigOverrides, IndexRefresh, ResolvedConfig};
use crate::dates::DateRange;
use crate::db::Db;
use crate::durable_fs::{atomic_write_file, AtomicWriteMode};
use crate::indexer;
use crate::inspect::{inspection_rows, InspectionOptions};
use crate::migration::{
    import_legacy_config, load_receipt, migrate_database, publish_imported_config,
    recover_database_migration, verify_migration, ConfigPublishOptions, DatabaseMigrationOptions,
};
use crate::models::{Provider, SearchFilters, SessionRecord};
use crate::render::{render, OutputFormat, Row};
use crate::service::SessionSearch;
use crate::tui;
use crate::util::{
    current_repo, executable_candidates, highlight_matches, prompt_confirm, relative_age,
    render_posix_shell_command, resume_plan, select_transcript_lines, truncate_for_display,
};
use anyhow::{anyhow, Context, Result};
use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "aise",
    version,
    about = "AI Session Search (aise): search local sessions from Claude Code, Claude Desktop local agent, Codex, Cursor, Antigravity, Pi coding agent, Google AI Studio, and Gemini CLI"
)]
struct Cli {
    /// Explicit configuration file. Overrides AI_SESSION_SEARCH_CONFIG and platform discovery.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Explicit SQLite index. Overrides AI_SESSION_SEARCH_DATABASE and config.toml.
    #[arg(long, global = true)]
    database: Option<PathBuf>,
    /// Explicit cache directory. Overrides AI_SESSION_SEARCH_CACHE_DIR and config.toml.
    #[arg(long, global = true)]
    cache_dir: Option<PathBuf>,
    /// Worker threads, an integer 1 or greater. Overrides AI_SESSION_SEARCH_THREADS and
    /// config.toml.
    #[arg(long, global = true, value_parser = parse_positive_usize)]
    threads: Option<usize>,
    /// Index refresh policy for implicit read commands. Overrides
    /// AI_SESSION_SEARCH_INDEX_REFRESH and config.toml.
    #[arg(long, global = true, value_enum)]
    index_refresh: Option<IndexRefresh>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    #[command(name = "__refresh-index", hide = true)]
    RefreshIndex,
    /// Rebuild the index from session files (incremental; `--full` reparses everything).
    Reindex(ReindexArgs),
    /// Reclaim disk space: merge FTS segments, `VACUUM`, then truncate the WAL.
    Compact,
    /// List recent sessions (newest first), with optional provider/path/date filters.
    List(QueryArgs),
    /// Search indexed sessions by keyword, ranked by relevance; filter with `--provider`.
    #[command(
        after_help = "For turn-level literal, regex, or fuzzy content search, use `aise messages search QUERY`, `aise messages search --regex QUERY`, or `aise messages search --fuzzy QUERY`."
    )]
    Search(SearchArgs),
    /// Print one session's transcript and metadata (bounded by default).
    Show(ShowArgs),
    /// Resume a session in its native CLI: print the command, or run it with confirmation.
    Resume(ResumeArgs),
    /// Export one full session or an explicitly selected bounded session bundle.
    Export(ExportArgs),
    /// Search and read individual messages: conversation turns and tool evidence (search|get|timeline|evidence).
    #[command(subcommand)]
    Messages(crate::messages::MessagesCmd),
    /// Find user messages where corrections were given (categorized).
    Corrections(crate::analytics::CorrectionsArgs),
    /// Aggregate slash-command usage frequency.
    Planning(crate::analytics::PlanningArgs),
    /// Analyze indexed sessions with an optional validated JSON policy and publish one immutable bundle.
    Analyze(AnalyzeArgs),
    /// Message counts by role.
    Stats(crate::analytics::StatsArgs),
    /// Term-frequency vocabulary over the message index (fts5vocab).
    Vocab(crate::analytics::VocabArgs),
    /// Find recurring phrases in session messages.
    Repeats(crate::analytics::RepeatsArgs),
    /// Recover edited files: search/history/cross-ref/extract.
    #[command(subcommand)]
    Files(crate::files::FilesCmd),
    /// Install executable aliases, MCP registrations, and managed instructions.
    Install(crate::mcp_install::McpInstallArgs),
    /// Inspect executable aliases, MCP registrations, and managed instructions.
    Status(crate::mcp_install::McpStatusArgs),
    /// Remove owned aliases, MCP registrations, and instructions; preserve data and the `aise` CLI.
    Uninstall(crate::mcp_install::McpUninstallArgs),
    /// Serve MCP requests or recover an interrupted client-configuration transaction.
    #[command(subcommand)]
    Mcp(crate::mcp_install::McpCmd),
    /// Expert read-only SQL over the local AI session-history index.
    #[command(subcommand)]
    Db(crate::sql_query::DbCmd),
    /// Safely migrate or verify a session index database.
    #[command(subcommand)]
    Migrate(MigrationCmd),
    /// Print effective configuration or the config file path.
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Show the supported --since/--until/--when date and EDTF formats.
    Dates,
    /// Check index health, provider discovery, and resume-tool availability.
    Doctor(DoctorArgs),
    /// Print the paths aise reads and writes (database, cache, config, providers).
    Paths,
    /// Launch the interactive terminal UI for browsing and resuming sessions.
    Tui,
}

#[derive(Debug, Subcommand)]
enum MigrationCmd {
    /// Online-backup a live SQLite database and atomically publish a verified copy.
    Database(MigrationDatabaseArgs),
    /// Preview or atomically publish a legacy aise JSON configuration as Rust TOML.
    Config(MigrationConfigArgs),
    /// Verify source and destination against a published migration receipt.
    Verify(MigrationVerifyArgs),
    /// Safely resume or finalize a database migration from durable prepared evidence.
    Recover(MigrationVerifyArgs),
}

#[derive(Debug, Args)]
struct MigrationConfigArgs {
    #[arg(long)]
    source_json: PathBuf,
    #[arg(long)]
    destination: PathBuf,
    #[arg(long)]
    database_path: PathBuf,
    #[arg(long)]
    cache_dir: PathBuf,
    #[arg(long)]
    apply: bool,
    #[arg(long, requires = "apply")]
    replace: bool,
    #[arg(long, requires = "replace")]
    rollback_copy: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct MigrationDatabaseArgs {
    #[arg(long)]
    source: PathBuf,
    #[arg(long)]
    destination: PathBuf,
    #[arg(long)]
    receipt: PathBuf,
    #[arg(long, default_value_t = 256)]
    pages_per_step: i32,
    #[arg(long, default_value_t = 10)]
    pause_ms: u64,
}

#[derive(Debug, Args)]
struct MigrationVerifyArgs {
    #[arg(long)]
    receipt: PathBuf,
}

#[derive(Debug, Args)]
struct ReindexArgs {
    /// Reparse every session file, ignoring the mtime/size skip cache.
    #[arg(long)]
    full: bool,
}

#[derive(Debug, Args)]
struct DoctorArgs {
    /// Output format. JSON is the stable machine-readable status shared with MCP.
    #[arg(long, value_enum, default_value_t = DoctorFormat::Table)]
    format: DoctorFormat,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::ValueEnum)]
enum DoctorFormat {
    Table,
    Json,
}

#[derive(Debug, Args, Clone)]
struct QueryArgs {
    #[command(flatten)]
    filters: SessionFilterArgs,
    /// Maximum number of rows to return. Omit to use `[search].default_limit`; zero means all.
    #[arg(long)]
    limit: Option<usize>,
    /// Output format. `table` (default) keeps the rich human layout; json/jsonl/csv/plain
    /// emit machine-readable rows.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,
}

#[derive(Debug, Args, Clone)]
struct SessionFilterArgs {
    /// Restrict to one indexed session source; omit to include all eight.
    #[arg(long)]
    provider: Option<Provider>,
    /// Restrict to sessions whose cwd or repo root starts with this path prefix.
    #[arg(long)]
    path: Option<String>,
    /// Exclude sessions whose cwd, repo root, or transcript path starts with this path.
    /// Repeat to exclude multiple noisy worktrees or transcript roots.
    #[arg(long = "exclude-path")]
    exclude_paths: Vec<String>,
    /// Exclude one exact session id. Repeat to exclude multiple sessions.
    #[arg(long = "exclude-session")]
    exclude_sessions: Vec<String>,
    #[command(flatten)]
    dates: DateRange,
    /// Show only sessions that produced a parse warning.
    #[arg(long)]
    warnings_only: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::ValueEnum)]
enum AnalysisFormatArg {
    Json,
    Markdown,
}

impl From<AnalysisFormatArg> for AnalysisPublicationFormat {
    fn from(value: AnalysisFormatArg) -> Self {
        match value {
            AnalysisFormatArg::Json => Self::Json,
            AnalysisFormatArg::Markdown => Self::Markdown,
        }
    }
}

#[derive(Debug, Args)]
struct AnalyzeArgs {
    #[command(flatten)]
    filters: SessionFilterArgs,
    /// Maximum sessions to analyze. Omit or pass zero to analyze the full selected corpus.
    #[arg(long)]
    limit: Option<usize>,
    /// Destination for the new immutable bundle; a relative path resolves against the current
    /// directory. Give a fresh path: the bundle is created here, and an existing path is
    /// refused so a prior bundle stays intact.
    #[arg(long)]
    output: PathBuf,
    /// Optional UTF-8 JSON AnalysisPolicySpec. Omit for structural graph/taxonomy analysis.
    #[arg(long)]
    policy: Option<PathBuf>,
    /// Artifact representation to publish. Repeat to select both.
    #[arg(long = "publication-format", value_enum, default_values_t = [AnalysisFormatArg::Json, AnalysisFormatArg::Markdown])]
    publication_formats: Vec<AnalysisFormatArg>,
}

#[derive(Debug, Args)]
struct SearchArgs {
    /// Session-level keywords, phrase, code snippet, path fragment, or title text to search for.
    /// A query starting with `-` is parsed as a flag here; pass it after `--`, with every other
    /// flag before the `--`, e.g. `--limit 5 -- --path`.
    query: String,
    #[command(flatten)]
    filters: QueryArgs,
}

#[derive(Debug, Args)]
struct ShowArgs {
    /// Session id or unambiguous id prefix (e.g. `claude:79accec8` or `79accec8`).
    id: String,
    /// Transcript lines to print: positive=head, negative=tail, 0=entire transcript.
    ///
    /// Bounds output so long sessions stay skimmable: a negative tail such as `-40` shows how a
    /// session ended, a positive head shows how it started, and `0` prints the entire transcript,
    /// which may be very large. To pinpoint one turn instead of scanning transcript text, use
    /// `aise messages search` and expand the hit with `aise messages get --seq N --context 3`.
    /// Omit to use `[cli].show_transcript_lines` from config.
    #[arg(long, allow_hyphen_values = true)]
    transcript_lines: Option<i64>,
    /// Print a compact session summary: purpose, tool activity, refs, changed files, and follow-ups.
    #[arg(long, conflicts_with_all = ["transcript_lines", "raw"])]
    summary: bool,
    /// With --summary, select aggregate evidence records: positive=first, negative=last, 0=all.
    /// Omit to use [cli].summary_items from config.
    #[arg(long, allow_hyphen_values = true, requires = "summary")]
    summary_items: Option<i64>,
    /// With --summary, cap each evidence preview to this many characters (1 or greater).
    /// Omit to use [cli].evidence_preview_chars from config.
    #[arg(long, requires = "summary", value_parser = parse_positive_usize)]
    preview_chars: Option<usize>,
    /// Print the raw stored transcript text instead of the formatted view.
    #[arg(long)]
    raw: bool,
}

#[derive(Debug, Args)]
struct ResumeArgs {
    /// Session id or unambiguous id prefix to resume.
    id: String,
    /// Skip the confirmation prompt and run the resume command immediately.
    #[arg(long)]
    yes: bool,
    /// Print a POSIX-shell rendering of the resume arguments without running them.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct ExportArgs {
    /// Session id or unambiguous id prefix. Omit only with --output-dir for a filtered bundle.
    id: Option<String>,
    #[command(flatten)]
    filters: SessionFilterArgs,
    /// Maximum bundled sessions. Omit for `[search].default_limit`; zero explicitly means all.
    #[arg(long)]
    limit: Option<usize>,
    /// Export format: markdown, json, or text.
    #[arg(long, default_value = "markdown")]
    format: String,
    /// Write to this file instead of stdout.
    #[arg(short, long, conflicts_with = "output_dir")]
    output: Option<PathBuf>,
    /// Atomically publish filtered sessions as a new immutable directory.
    #[arg(long, conflicts_with = "output")]
    output_dir: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum ConfigCmd {
    /// Print the config file path.
    Path,
    /// Print the embedded commented example config.
    Example,
    /// Write the embedded commented example config to the default config path.
    Init(ConfigInitArgs),
    /// Print the effective config after defaults and config.toml are merged.
    Show(ConfigShowArgs),
    /// Explain where each effective path and thread setting came from.
    Explain,
}

#[derive(Debug, Args)]
struct ConfigInitArgs {
    /// Overwrite an existing config.toml.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct ConfigShowArgs {
    /// Output format.
    #[arg(long, value_enum, default_value_t = ConfigOutputFormat::Toml)]
    format: ConfigOutputFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "lowercase")]
enum ConfigOutputFormat {
    Toml,
    Json,
}

pub(crate) fn parse_positive_usize(value: &str) -> std::result::Result<usize, String> {
    match value.parse::<usize>() {
        Ok(parsed) if parsed > 0 => Ok(parsed),
        _ => Err(format!("expected a positive integer, got {value:?}")),
    }
}

/// Parse and execute the canonical CLI without terminating the embedding process.
///
/// Clap help and usage errors are printed to their normal stream and returned as an
/// exit code. Runtime failures remain structured [`anyhow::Error`] values. This lets
/// the native executable and PyO3 console entry point share one command dispatcher.
pub fn run_from<I, T>(args: I) -> Result<i32>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let exit_code = error.exit_code();
            error.print()?;
            return Ok(exit_code);
        }
    };
    execute(cli)?;
    Ok(0)
}

fn execute(cli: Cli) -> Result<()> {
    if matches!(&cli.command, Commands::RefreshIndex) {
        return run_background_refresh_from_stdin();
    }
    let overrides = ConfigOverrides {
        config_path: cli.config,
        database_path: cli.database,
        cache_dir: cli.cache_dir,
        threads: cli.threads,
        index_refresh: cli.index_refresh,
    };
    let command = match cli.command {
        Commands::Install(args) => {
            let config_path = Config::selected_config_path(overrides.config_path.clone());
            let receipt = crate::mcp_install::default_transaction_receipt(&config_path);
            return crate::mcp_install::install_with_receipt(args, &receipt);
        }
        Commands::Status(args) => {
            let config_path = Config::selected_config_path(overrides.config_path.clone());
            let receipt = crate::mcp_install::default_transaction_receipt(&config_path);
            return crate::mcp_install::status_with_receipt(args, &receipt);
        }
        Commands::Uninstall(args) => {
            let config_path = Config::selected_config_path(overrides.config_path.clone());
            let receipt = crate::mcp_install::default_transaction_receipt(&config_path);
            return crate::mcp_install::uninstall_with_receipt(args, &receipt);
        }
        command => command,
    };
    let command = match command {
        Commands::Mcp(crate::mcp_install::McpCmd::Serve) => {
            let resolved = Config::resolve(overrides.clone())?;
            report_config_diagnostics(&resolved);
            return crate::mcp_server::serve_with_config(resolved.config);
        }
        Commands::Mcp(cmd) => {
            let config_path = Config::selected_config_path(overrides.config_path.clone());
            let receipt = crate::mcp_install::default_transaction_receipt(&config_path);
            return crate::mcp_install::run_mcp_cmd_with_receipt(cmd, &receipt);
        }
        command => command,
    };

    if let Commands::Config(cmd) = &command {
        match cmd {
            ConfigCmd::Path => {
                println!(
                    "{}",
                    Config::selected_config_path(overrides.config_path.clone()).display()
                );
                return Ok(());
            }
            ConfigCmd::Example => {
                print!("{}", crate::config::CONFIG_EXAMPLE_TOML);
                return Ok(());
            }
            ConfigCmd::Init(args) => {
                write_config_example(
                    &Config::selected_config_path(overrides.config_path.clone()),
                    args.force,
                )?;
                return Ok(());
            }
            ConfigCmd::Show(_) | ConfigCmd::Explain => {}
        }
    }

    if let Commands::Migrate(cmd) = command {
        return run_migration(cmd);
    }

    let resolved = Config::resolve(overrides)?;
    report_config_diagnostics(&resolved);
    let config = resolved.config.clone();
    if let Commands::Db(cmd) = command {
        return crate::sql_query::run(
            &config.db_path(),
            config.index.busy_timeout_ms,
            &config.db,
            cmd,
        );
    }
    if let Commands::Config(cmd) = command {
        return run_config_cmd(&resolved, cmd);
    }
    if matches!(command, Commands::Paths) {
        print_paths(&config, &resolved.config_path)?;
        return Ok(());
    }
    if matches!(command, Commands::Dates) {
        println!("{}", crate::dates::format_reference());
        return Ok(());
    }
    let explicit_maintenance = matches!(&command, Commands::Reindex(_) | Commands::Compact);
    let mut app = if explicit_maintenance {
        SessionSearch::open_for_maintenance(config.clone())?
    } else {
        SessionSearch::open(config.clone())?
    };
    // Terminal frontend: report library progress (e.g. the one-time lazy index build) to stderr.
    app.set_progress_reporter(|message| eprintln!("aise: {message}"));
    let db = app.database();

    // Repair an unusable schema before a read. A usable index is served immediately; `auto`
    // schedules an incremental refresh after output, while deterministic policies complete here.
    let implicit_read = !matches!(
        command,
        Commands::Reindex(_) | Commands::Compact | Commands::Doctor(_)
    );
    if implicit_read {
        prepare_index_for_immediate_read(&config, db)?;
    }

    let mut refresh_scheduled = false;
    match command {
        Commands::Reindex(args) => {
            let outcome = reindex(&config, db, args.full)?;
            println!(
                "reindex complete: scanned {} files, updated {} sessions",
                outcome.files_seen, outcome.sessions_updated
            );
            if outcome.effective_full {
                let allocation = db.storage_allocation()?;
                if let Some(guidance) = storage_compaction_guidance(allocation) {
                    eprintln!("aise: {guidance}");
                }
            }
        }
        Commands::List(args) => {
            let format = args.format;
            let filters =
                build_filters(&args.filters, configured_search_limit(args.limit, &config))?;
            let sessions = app.catalog().list_sessions(&filters)?;
            match format {
                OutputFormat::Table => print_sessions(&sessions),
                other => render_rows(&sessions, other)?,
            }
        }
        Commands::Search(args) => {
            let format = args.filters.format;
            let filters = build_filters(
                &args.filters.filters,
                configured_search_limit(args.filters.limit, &config),
            )?;
            let current_repo = current_repo(&config);
            let hits = app.catalog().search_sessions(
                &args.query,
                &filters,
                current_repo.as_deref(),
                &config.search.scoring,
            )?;
            match format {
                OutputFormat::Table => {
                    if hits.is_empty() {
                        println!("no sessions matched");
                    } else {
                        for hit in hits {
                            print_search_hit(&hit, &args.query);
                        }
                    }
                }
                other => render_rows(&hits, other)?,
            }
        }
        Commands::Show(args) => {
            if args.summary {
                let options = InspectionOptions {
                    preview_chars: args
                        .preview_chars
                        .unwrap_or(config.cli.evidence_preview_chars)
                        .max(1),
                    evidence_window: crate::inspect::EvidenceWindow::from_signed_items(
                        args.summary_items.unwrap_or(config.cli.summary_items),
                    )?,
                    include_time_profile: false,
                };
                let inspection = app.catalog().inspect(&args.id, options)?;
                render_rows(&inspection_rows(&inspection, options), OutputFormat::Table)?;
                schedule_auto_refresh_after_output(
                    &config,
                    db,
                    implicit_read,
                    &mut refresh_scheduled,
                );
                return Ok(());
            }
            let session = db.resolve_session(&args.id)?;
            print_session_detail(&session.session);
            let transcript_lines = args
                .transcript_lines
                .unwrap_or(config.cli.show_transcript_lines);
            let (transcript, returned_lines) =
                select_transcript_lines(&session.transcript_text, transcript_lines);
            if args.raw {
                println!("\n{transcript}");
            } else {
                println!("\nTranscript lines returned: {returned_lines}");
                println!("\nTranscript\n{transcript}\n");
            }
        }
        Commands::Resume(args) => {
            let session = db.resolve_session_record(&args.id)?;
            let (cmd, cwd) = resume_plan(&session)?;
            let rendered = render_posix_shell_command(&cmd)?;
            println!("POSIX shell resume command: {rendered}");
            if let Some(cwd) = &cwd {
                println!("cwd: {cwd}");
            }
            schedule_auto_refresh_after_output(&config, db, implicit_read, &mut refresh_scheduled);
            if args.dry_run {
                return Ok(());
            }
            if !args.yes && !prompt_confirm("Execute resume command?")? {
                println!("resume cancelled");
                return Ok(());
            }

            let mut command = Command::new(&cmd[0]);
            command.args(&cmd[1..]);
            if let Some(cwd) = cwd {
                command.current_dir(cwd);
            }
            let status = command.status()?;
            if !status.success() {
                return Err(anyhow!("resume command failed with status {status}"));
            }
        }
        Commands::Export(args) => {
            let format = args.format.parse()?;
            let filters = build_filters(
                &args.filters,
                args.limit.unwrap_or(config.search.default_limit),
            )?;
            if let Some(id) = args.id {
                if args.output_dir.is_some()
                    || args.limit.is_some()
                    || !export_filters_are_empty(&filters)
                {
                    return Err(anyhow!("single-session export does not accept corpus filters, --limit, or --output-dir"));
                }
                let output = crate::service::ExportService::new(db)
                    .render_full(&id, format)?
                    .into_content();
                if let Some(path) = args.output {
                    fs::write(&path, output)?;
                    println!("wrote {}", path.display());
                } else {
                    print!("{output}");
                }
            } else {
                if args.output.is_some() {
                    return Err(anyhow!(
                        "multi-session export requires --output-dir, not --output"
                    ));
                }
                let destination = args
                    .output_dir
                    .context("export requires a session ID or --output-dir")?;
                let destination = if destination.is_absolute() {
                    destination
                } else {
                    std::env::current_dir()?.join(destination)
                };
                let plan = crate::export::ExportPublicationPlan::new(destination, format)?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&app.exports().publish_bundle(&filters, &plan)?)?
                );
            }
        }
        Commands::Messages(cmd) => crate::messages::run(db, &cmd, &config.cli)?,
        Commands::Corrections(args) => crate::analytics::run_corrections(db, &config, &args)?,
        Commands::Planning(args) => crate::analytics::run_planning(db, &config, &args)?,
        Commands::Analyze(args) => {
            let filters = build_filters(&args.filters, analysis_limit(args.limit))?;
            // Resolve relative destinations against the current directory, exactly as
            // multi-session export does; the publication plan itself still requires an
            // absolute path so library callers stay explicit.
            let output = if args.output.is_absolute() {
                args.output
            } else {
                std::env::current_dir()?.join(args.output)
            };
            let plan = AnalysisPublicationPlan::new(
                output,
                args.publication_formats
                    .into_iter()
                    .map(AnalysisPublicationFormat::from),
            )?;
            plan.preflight()?;
            let policy_spec = match args.policy {
                Some(path) => {
                    let bytes = fs::read(&path).with_context(|| {
                        format!("failed to read analysis policy {}", path.display())
                    })?;
                    serde_json::from_slice(&bytes).with_context(|| {
                        format!("failed to parse analysis policy {}", path.display())
                    })?
                }
                None => AnalysisPolicySpec::default(),
            };
            let policy = policy_spec.compile()?;
            let result = app.analysis().run(&filters, &policy)?;
            println!("{}", serde_json::to_string_pretty(&plan.publish(&result)?)?);
        }
        Commands::Stats(args) => crate::analytics::run_stats(db, &config, &args)?,
        Commands::Vocab(args) => crate::analytics::run_vocab(db, &config.analytics, &args)?,
        Commands::Repeats(args) => crate::analytics::run_repeats(db, &config.analytics, &args)?,
        Commands::Files(cmd) => crate::files::run(db, &cmd)?,
        Commands::Compact => {
            let before = db.storage_allocation()?.total_bytes;
            eprintln!(
                "aise: compacting index ({}) — optimize + vacuum + wal checkpoint…",
                mib(before)
            );
            let outcome = app.maintenance().compact()?;
            println!(
                "compact complete: {} → {} (reclaimed {})",
                mib(outcome.before_bytes),
                mib(outcome.after_bytes),
                mib(outcome.reclaimed_bytes())
            );
        }
        Commands::Dates => unreachable!("date reference returns before opening the DB"),
        Commands::Doctor(args) => print_doctor(&config, db, args.format)?,
        Commands::Paths => unreachable!("path inspection returns before opening the DB"),
        Commands::Tui => {
            schedule_auto_refresh_after_output(&config, db, implicit_read, &mut refresh_scheduled);
            tui::run(&config, db)?
        }
        Commands::Mcp(_) => unreachable!("MCP install commands return before opening the DB"),
        Commands::Install(_) | Commands::Status(_) | Commands::Uninstall(_) => {
            unreachable!("top-level integration aliases normalize before configuration")
        }
        Commands::Db(_) => unreachable!("DB query commands return before opening the write DB"),
        Commands::Migrate(_) => unreachable!("migration commands return before opening the DB"),
        Commands::Config(_) => unreachable!("Config commands return before opening the DB"),
        Commands::RefreshIndex => unreachable!("background refresh returns before configuration"),
    }

    schedule_auto_refresh_after_output(&config, db, implicit_read, &mut refresh_scheduled);

    Ok(())
}

fn run_migration(command: MigrationCmd) -> Result<()> {
    match command {
        MigrationCmd::Database(args) => {
            let mut options =
                DatabaseMigrationOptions::new(args.source, args.destination, args.receipt);
            options.pages_per_step = args.pages_per_step;
            options.pause_between_steps = std::time::Duration::from_millis(args.pause_ms);
            let receipt = migrate_database(&options)?;
            println!("{}", serde_json::to_string_pretty(&receipt)?);
        }
        MigrationCmd::Config(args) => {
            let import = import_legacy_config(
                &args.source_json,
                args.destination.clone(),
                args.database_path,
                args.cache_dir,
            )?;
            println!("{}", serde_json::to_string_pretty(&import.report)?);
            if args.apply {
                publish_imported_config(
                    &import,
                    &ConfigPublishOptions {
                        destination: args.destination,
                        replace_existing: args.replace,
                        rollback_copy: args.rollback_copy,
                    },
                )?;
            }
        }
        MigrationCmd::Verify(args) => {
            let receipt = load_receipt(&args.receipt)?;
            verify_migration(&receipt)?;
            println!("migration verified: {}", receipt.destination.display());
        }
        MigrationCmd::Recover(args) => {
            let receipt = recover_database_migration(&args.receipt)?;
            println!(
                "migration recovered and verified: {}",
                receipt.destination.display()
            );
        }
    }
    Ok(())
}

fn run_config_cmd(resolved: &ResolvedConfig, cmd: ConfigCmd) -> Result<()> {
    match cmd {
        ConfigCmd::Path => println!("{}", resolved.config_path.display()),
        ConfigCmd::Example => print!("{}", crate::config::CONFIG_EXAMPLE_TOML),
        ConfigCmd::Init(args) => write_config_example(&resolved.config_path, args.force)?,
        ConfigCmd::Show(args) => match args.format {
            ConfigOutputFormat::Toml => {
                print!("{}", toml::to_string_pretty(&resolved.config)?)
            }
            ConfigOutputFormat::Json => {
                println!("{}", serde_json::to_string_pretty(&resolved.config)?)
            }
        },
        ConfigCmd::Explain => println!("{}", serde_json::to_string_pretty(&resolved.origins)?),
    }
    Ok(())
}

fn write_config_example(path: &std::path::Path, force: bool) -> Result<()> {
    let mode = if force {
        AtomicWriteMode::Replace
    } else {
        AtomicWriteMode::CreateNew
    };
    atomic_write_file(path, crate::config::CONFIG_EXAMPLE_TOML.as_bytes(), mode).with_context(
        || {
            if force {
                format!("failed to initialize config file {}", path.display())
            } else {
                format!(
                    "failed to initialize config file {}; use `aise config init --force` only to replace an existing regular file",
                    path.display()
                )
            }
        },
    )?;
    println!("wrote {}", path.display());
    Ok(())
}

fn report_config_diagnostics(resolved: &ResolvedConfig) {
    for diagnostic in &resolved.diagnostics {
        eprintln!("aise: {diagnostic}");
    }
}

/// Human-readable mebibytes for size reporting.
fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / 1_048_576.0)
}

fn reindex(config: &Config, db: &Db, full: bool) -> Result<indexer::ExplicitReindexOutcome> {
    // Render progress to stderr when the dataset is large enough to matter.
    // We don't know the total up front without re-running discovery here, so
    // we let the callback gate on `total` and update on every change.
    let mut progress = |index: usize, total: usize, updated: usize| {
        if total >= 20 && (updated.is_multiple_of(10) || index == total) {
            eprint!("\rindexing: {index}/{total} files ({updated} updated)");
        }
    };
    let outcome = indexer::explicit_reindex_and_migrate(config, db, full, Some(&mut progress))?;
    if outcome.files_seen >= 20 {
        eprintln!();
    }
    Ok(outcome)
}

fn prepare_index_for_immediate_read(config: &Config, db: &Db) -> Result<()> {
    match indexer::prepare_index_for_read_now(config, db)? {
        None
        | Some(indexer::AutoReindexOutcome::Updated { .. })
        | Some(indexer::AutoReindexOutcome::SkippedFresh) => Ok(()),
        Some(indexer::AutoReindexOutcome::SkippedBusy) => {
            eprintln!(
                "aise: auto-reindex skipped because another process is writing; serving existing index"
            );
            Ok(())
        }
        Some(indexer::AutoReindexOutcome::SkippedLockUnavailable { reason }) => {
            eprintln!(
                "aise: auto-reindex skipped because the update lock is unavailable; serving existing index ({reason})"
            );
            Ok(())
        }
    }
}

fn schedule_auto_refresh_after_output(
    config: &Config,
    db: &Db,
    implicit_read: bool,
    scheduled: &mut bool,
) {
    if *scheduled || !implicit_read || config.index.refresh != IndexRefresh::Auto {
        return;
    }
    *scheduled = true;
    if let Err(error) = io::stdout().flush() {
        eprintln!(
            "aise: background index refresh not started because stdout flush failed: {error}"
        );
        return;
    }
    match indexer::auto_refresh_is_due(db, config.index.auto_reindex_interval_ms) {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => {
            eprintln!("aise: background index refresh not started because refresh state could not be checked: {error:#}");
            return;
        }
    }
    if let Err(error) = spawn_background_refresh(config) {
        eprintln!("aise: background index refresh could not start: {error:#}");
    }
}

fn spawn_background_refresh(config: &Config) -> Result<()> {
    let executable =
        std::env::current_exe().context("could not resolve the running aise executable")?;
    let mut child = Command::new(&executable)
        .arg("__refresh-index")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        // A detached child must not retain a harness's captured output pipes: doing so makes the
        // foreground command appear to run until refresh finishes. Parent-side spawn and config
        // errors remain visible; persistent index health is reported by `aise doctor`.
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to start {} __refresh-index", executable.display()))?;
    let mut stdin = child
        .stdin
        .take()
        .context("background refresh process did not expose its configuration pipe")?;
    serde_json::to_writer(&mut stdin, config)
        .context("failed to send resolved configuration to background refresh process")?;
    stdin
        .flush()
        .context("failed to flush background refresh configuration")?;
    Ok(())
}

fn run_background_refresh_from_stdin() -> Result<()> {
    let config: Config = serde_json::from_reader(io::stdin().lock())
        .context("failed to read resolved background refresh configuration from stdin")?;
    crate::background_refresh::run(
        &config,
        crate::background_refresh::BackgroundRefreshOrigin::Cli,
        &|| false,
    )?;
    Ok(())
}

/// Render rows to stdout in a non-table machine format (json/jsonl/csv/plain).
fn render_rows<T: serde::Serialize + Row>(rows: &[T], format: OutputFormat) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    render(rows, format, &mut out)?;
    out.flush()?;
    Ok(())
}

fn configured_search_limit(limit: Option<usize>, config: &Config) -> usize {
    limit.unwrap_or(config.search.default_limit)
}

fn analysis_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(0)
}

fn export_filters_are_empty(filters: &SearchFilters) -> bool {
    filters.provider.is_none()
        && filters.path_prefix.is_none()
        && filters.exclude_path_prefixes.is_empty()
        && filters.exclude_session_ids.is_empty()
        && filters.since.is_none()
        && filters.until.is_none()
        && !filters.warnings_only
}

fn build_filters(args: &SessionFilterArgs, limit: usize) -> Result<SearchFilters> {
    let (since, until) = args.dates.resolve_now()?;
    Ok(SearchFilters {
        provider: args.provider,
        path_prefix: args.path.as_deref().map(crate::util::normalize_path_prefix),
        exclude_path_prefixes: args
            .exclude_paths
            .iter()
            .map(|path| crate::util::normalize_path_prefix(path))
            .collect(),
        exclude_session_ids: args.exclude_sessions.clone(),
        since,
        until,
        limit,
        warnings_only: args.warnings_only,
    })
}

fn print_sessions(sessions: &[SessionRecord]) {
    if sessions.is_empty() {
        println!("no sessions found");
        return;
    }
    for session in sessions {
        print_session_row(session, None, None);
    }
}

fn print_session_row(session: &SessionRecord, match_source: Option<&str>, score: Option<i64>) {
    let title = session
        .title
        .as_deref()
        .map(|value| truncate_for_display(value, 72))
        .unwrap_or_else(|| session.preview_text.clone());
    let cwd = session.cwd.as_deref().unwrap_or("-");
    let mut suffix = String::new();
    if let Some(source) = match_source {
        suffix.push_str(&format!(" match={source}"));
    }
    if let Some(score) = score {
        suffix.push_str(&format!(" score={score}"));
    }
    println!(
        "{}  {:<6}  {:<38}  {:<72}{}",
        relative_age(session.updated_at),
        session.provider,
        session.provider_session_id,
        title,
        suffix
    );
    println!("  cwd={}  preview={}", cwd, session.preview_text);
    if let Some(warning) = &session.parse_warning {
        println!("  warning={warning}");
    }
}

fn print_search_hit(hit: &crate::models::SearchHit, query: &str) {
    let title = hit
        .session
        .title
        .as_deref()
        .map(|value| truncate_for_display(value, 72))
        .unwrap_or_else(|| hit.session.preview_text.clone());
    let title = highlight_matches(&title, query);
    let cwd = hit.session.cwd.as_deref().unwrap_or("-");
    println!(
        "{}  {:<6}  {:<38}  {} match={} score={}",
        relative_age(hit.session.updated_at),
        hit.session.provider,
        hit.session.provider_session_id,
        title,
        hit.match_source,
        hit.score
    );
    println!(
        "  cwd={}  preview={}",
        cwd,
        highlight_matches(&hit.session.preview_text, query)
    );
    println!(
        "  hit[{}]: {}",
        hit.match_source,
        highlight_matches(&hit.match_snippet, query)
    );
    if let Some(warning) = &hit.session.parse_warning {
        println!("  warning={warning}");
    }
}

fn print_session_detail(session: &SessionRecord) {
    println!("ID: {}", session.id);
    println!("Provider: {}", session.provider);
    println!("Provider Session ID: {}", session.provider_session_id);
    println!("Title: {}", session.title.as_deref().unwrap_or("-"));
    println!("Summary: {}", session.summary.as_deref().unwrap_or("-"));
    println!("CWD: {}", session.cwd.as_deref().unwrap_or("-"));
    println!("Repo Root: {}", session.repo_root.as_deref().unwrap_or("-"));
    println!(
        "Created: {}",
        session
            .created_at
            .map(|value| value.to_rfc3339())
            .unwrap_or_else(|| "-".to_string())
    );
    println!(
        "Updated: {}",
        session
            .updated_at
            .map(|value| value.to_rfc3339())
            .unwrap_or_else(|| "-".to_string())
    );
    println!("Messages: {}", session.message_count.unwrap_or_default());
    println!("Source Path: {}", session.source_path);
    println!("Discovery: {}", session.discovery_source);
    if let Some(warning) = &session.parse_warning {
        println!("Parse Warning: {warning}");
    }
}

fn print_doctor(config: &Config, db: &Db, format: DoctorFormat) -> Result<()> {
    let diagnostics = crate::diagnostics::collect(config, db)?;
    if format == DoctorFormat::Json {
        println!("{}", serde_json::to_string_pretty(&diagnostics)?);
        return Ok(());
    }
    let status = &diagnostics.index_status;
    let health = &diagnostics.providers;
    let warnings = status.parser_health.parse_warnings;
    println!("DB: {}", config.db_path().display());
    let allocation = db.storage_allocation()?;
    println!(
        "Storage: {} bytes total; {} bytes reclaimable",
        allocation.total_bytes, allocation.reclaimable_bytes
    );
    if let Some(guidance) = storage_compaction_guidance(allocation) {
        println!("Maintenance: {guidance}");
    }
    println!(
        "Parser health: {} current, {} stale ({} repairable, {} unavailable); schema {}/{}",
        status.parser_health.current_sessions,
        status.parser_health.stale_sessions,
        status.repairable_stale_sessions,
        status.unavailable_stale_sessions,
        status.parser_health.schema_version,
        status.parser_health.expected_schema_version
    );
    for command in &status.repair_commands {
        println!("Repair: {command}");
    }
    print_auto_reindex_status(config, db)?;
    if let Some(update) = &status.index_update {
        println!(
            "Index update: {} since {}: {}",
            update.state.as_str(),
            update.started_at.to_rfc3339(),
            update.message
        );
        if let Some(command) = &update.next_command {
            println!("Index update next command: {command}");
        }
    }
    println!("Parse warnings indexed: {warnings}");
    for item in health {
        println!("\nProvider: {}", item.provider);
        println!(
            "  binary: {}",
            if item.cli_available {
                "present"
            } else {
                "missing"
            }
        );
        println!("  roots: {}", item.roots.join(", "));
        println!("  files discovered: {}", item.discovered_files);
        println!("  sessions indexed: {}", item.indexed_sessions);
        println!(
            "  parser: {} current, {} stale ({} repairable, {} unavailable; expected {})",
            item.current_sessions,
            item.stale_sessions,
            item.repairable_stale_sessions,
            item.unavailable_stale_sessions,
            item.expected_parse_version
        );
        println!(
            "  resume: {}",
            item.resume_command.as_deref().unwrap_or("not supported")
        );
    }
    Ok(())
}

fn storage_compaction_guidance(allocation: crate::db::StorageAllocation) -> Option<String> {
    (allocation.reclaimable_bytes > 0).then(|| {
        format!(
            "{} bytes ({}) of {} are reclaimable; run `aise compact` when an exclusive database lock and temporary disk space are available",
            allocation.reclaimable_bytes,
            mib(allocation.reclaimable_bytes),
            mib(allocation.total_bytes)
        )
    })
}

fn print_auto_reindex_status(config: &Config, db: &Db) -> Result<()> {
    let completed_at = db.auto_reindex_completed_at()?;
    let fresh = db.auto_reindex_is_fresh(config.index.auto_reindex_interval_ms)?;
    let window = if config.index.auto_reindex_interval_ms == 0 {
        "free-read window disabled".to_string()
    } else {
        format!(
            "free-read window {} ms",
            config.index.auto_reindex_interval_ms
        )
    };
    match completed_at {
        Some(value) => println!(
            "Auto-reindex last completed: {} ({}, {}, {})",
            value.to_rfc3339(),
            relative_age(Some(value)),
            if fresh { "fresh" } else { "stale" },
            window
        ),
        None => println!("Auto-reindex last completed: never ({window})"),
    }
    Ok(())
}

fn print_paths(config: &Config, config_path: &std::path::Path) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "Executable: {}", std::env::current_exe()?.display())?;
    let candidates = executable_candidates("aise");
    writeln!(
        out,
        "PATH aise candidates: {}",
        if candidates.is_empty() {
            "not found".to_string()
        } else {
            candidates
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        }
    )?;
    writeln!(out, "Config: {}", config_path.display())?;
    writeln!(out, "DB: {}", config.db_path().display())?;
    writeln!(out, "Cache: {}", config.cache_dir().display())?;
    writeln!(
        out,
        "Background refresh status: {}",
        crate::background_refresh::report_path(config).display()
    )?;
    writeln!(
        out,
        "Claude roots: {}",
        config
            .claude_paths()
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )?;
    writeln!(
        out,
        "Claude Desktop roots: {}",
        config
            .claude_desktop_paths()
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )?;
    writeln!(
        out,
        "Codex roots: {}",
        config
            .codex_paths()
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )?;
    writeln!(
        out,
        "Cursor roots: {}",
        config
            .cursor_paths()
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )?;
    writeln!(
        out,
        "Antigravity roots: {}",
        config
            .antigravity_paths()
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )?;
    writeln!(
        out,
        "Pi roots: {}",
        config
            .pi_paths()
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )?;
    writeln!(
        out,
        "Codex metadata home: {}",
        config.codex_home().display()
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn assert_parses<const N: usize>(args: [&str; N]) {
        Cli::try_parse_from(args)
            .unwrap_or_else(|err| panic!("expected CLI args to parse: {args:?}: {err}"));
    }

    fn assert_rejects<const N: usize>(args: [&str; N]) {
        assert!(
            Cli::try_parse_from(args).is_err(),
            "expected CLI args to be rejected: {args:?}"
        );
    }

    #[test]
    fn analyze_accepts_shared_scope_policy_and_publication_controls() {
        let cli = Cli::try_parse_from([
            "aise",
            "analyze",
            "--provider",
            "codex",
            "--limit",
            "2",
            "--output",
            "/tmp/analysis-bundle",
            "--policy",
            "/tmp/policy.json",
            "--publication-format",
            "json",
        ])
        .unwrap();
        let Commands::Analyze(args) = cli.command else {
            panic!("expected analyze command");
        };
        assert_eq!(args.filters.provider, Some(Provider::Codex));
        assert_eq!(args.limit, Some(2));
        assert_eq!(args.publication_formats, [AnalysisFormatArg::Json]);

        let cli = Cli::try_parse_from(["aise", "analyze", "--output", "/tmp/full-analysis-bundle"])
            .unwrap();
        let Commands::Analyze(args) = cli.command else {
            panic!("expected analyze command");
        };
        assert_eq!(analysis_limit(args.limit), 0);
        assert_eq!(
            args.publication_formats,
            [AnalysisFormatArg::Json, AnalysisFormatArg::Markdown]
        );
    }

    #[test]
    fn analyze_help_does_not_claim_the_search_default_limit() {
        let help = Cli::try_parse_from(["aise", "analyze", "--help"])
            .unwrap_err()
            .to_string();
        assert!(help.contains("Omit or pass zero to analyze the full selected corpus"));
        assert!(!help.contains("page-size"));
        assert!(!help.contains("use `[search].default_limit`"));
    }

    #[test]
    fn session_query_limit_distinguishes_omitted_unlimited_and_bounded() {
        let mut config = Config::default();
        config.search.default_limit = 37;

        assert_eq!(configured_search_limit(None, &config), 37);
        assert_eq!(configured_search_limit(Some(0), &config), 0);
        assert_eq!(configured_search_limit(Some(9), &config), 9);

        let help = Cli::try_parse_from(["aise", "search", "--help"])
            .unwrap_err()
            .to_string();
        assert!(help.contains("Omit to use `[search].default_limit`; zero means all"));
    }

    #[test]
    fn root_help_names_every_session_source() {
        let help = Cli::try_parse_from(["aise", "--help"])
            .unwrap_err()
            .to_string();
        for provider in crate::source::PROVIDERS {
            assert!(
                help.contains(provider.display_name()),
                "root help must name {}: {help}",
                provider.display_name()
            );
        }
        assert!(!help.contains("supported agents"));
        assert!(!help.contains("all agents"));
        assert!(!help.contains("__refresh-index"));
        assert!(help.contains("Overrides AI_SESSION_SEARCH_INDEX_REFRESH and config.toml"));
    }

    #[test]
    fn provider_filters_use_one_concrete_session_source_term() {
        for args in [
            ["aise", "list", "--help"],
            ["aise", "corrections", "--help"],
            ["aise", "planning", "--help"],
            ["aise", "stats", "--help"],
            ["aise", "repeats", "--help"],
        ] {
            let help = Cli::try_parse_from(args).unwrap_err().to_string();
            assert!(
                help.contains("Restrict to one indexed session source"),
                "provider help is not concrete: {help}"
            );
            assert!(!help.contains("Restrict to one harness"), "{help}");
        }
    }

    #[test]
    fn messages_summary_names_every_subcommand() {
        // Derive the subcommand set from the parser itself so the one-line summary
        // cannot silently drop a command the way "(search|get|timeline)" omitted
        // `evidence`. A future fifth subcommand fails this until the summary lists it.
        let cli = Cli::command();
        let messages = cli
            .get_subcommands()
            .find(|c| c.get_name() == "messages")
            .expect("messages subcommand exists");
        let about = messages
            .get_about()
            .expect("messages command has an about summary")
            .to_string();
        let subnames: Vec<&str> = messages
            .get_subcommands()
            .map(|c| c.get_name())
            .filter(|name| *name != "help") // clap's built-in help subcommand is not user content
            .collect();
        assert!(
            subnames.len() >= 4,
            "expected at least search/get/timeline/evidence, got {subnames:?}"
        );
        for name in &subnames {
            assert!(
                about.contains(name),
                "messages summary must name subcommand `{name}`: {about}"
            );
        }
        // Regression-lock: the evidence-omitting three-command list must not return.
        assert!(
            !about.contains("(search|get|timeline)"),
            "messages summary regressed to the evidence-omitting list: {about}"
        );
    }

    #[test]
    fn messages_search_limit_help_points_to_real_newest_commands() {
        let help = Cli::try_parse_from(["aise", "messages", "search", "--help"])
            .unwrap_err()
            .to_string();
        // Newest-N reads must route to real subcommands. `aise timeline` is not a
        // command (it is `aise messages timeline`), so a bare `timeline` pointer
        // sends callers to an "unrecognized subcommand 'timeline'" error. The old
        // text "`messages get`/`timeline --order newest`" had both defects: `get`
        // carried no `--order`, and `timeline` looked top-level.
        assert!(
            help.contains("get --order newest"),
            "limit help must point `get` at `--order newest`: {help}"
        );
        assert!(
            help.contains("messages timeline"),
            "limit help must qualify timeline as `messages timeline`: {help}"
        );
        // Regression-lock: the ambiguous backtick-prefixed bare `timeline` pointer.
        assert!(
            !help.contains("`timeline"),
            "limit help regressed to a bare top-level `timeline` pointer: {help}"
        );
    }

    #[test]
    fn config_commands_parse() {
        assert_parses(["aise", "config", "path"]);
        assert_parses(["aise", "config", "example"]);
        assert_parses(["aise", "config", "init", "--force"]);
        assert_parses(["aise", "config", "show"]);
        assert_parses(["aise", "config", "show", "--format", "json"]);
        assert_parses(["aise", "config", "explain"]);
        assert_parses([
            "aise",
            "--config",
            "/tmp/config.toml",
            "--database",
            "/tmp/index.db",
            "--cache-dir",
            "/tmp/cache",
            "--threads",
            "2",
            "paths",
        ]);
        assert!(Cli::try_parse_from(["aise", "--threads", "0", "paths"]).is_err());
    }

    #[test]
    fn top_level_integration_commands_share_mcp_arguments() {
        let cli = Cli::try_parse_from([
            "aise",
            "install",
            "--client",
            "antigravity",
            "--client",
            "opencode",
            "--exclude-client",
            "opencode",
        ])
        .unwrap();
        let Commands::Install(args) = cli.command else {
            panic!("expected install command");
        };
        assert_eq!(
            args.targets.clients,
            [
                crate::mcp_install::McpClient::Antigravity,
                crate::mcp_install::McpClient::Opencode,
            ]
        );
        assert_eq!(
            args.targets.excluded_clients,
            [crate::mcp_install::McpClient::Opencode]
        );
        assert!(matches!(
            Cli::try_parse_from(["aise", "install", "--no-mcp"])
                .unwrap()
                .command,
            Commands::Install(args) if args.no_mcp
        ));
        assert!(matches!(
            Cli::try_parse_from(["aise", "status", "--no-mcp"])
                .unwrap()
                .command,
            Commands::Status(args) if args.no_mcp
        ));
        assert!(matches!(
            Cli::try_parse_from(["aise", "status", "--client", "opencode"])
                .unwrap()
                .command,
            Commands::Status(_)
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "aise",
                "uninstall",
                "--client",
                "opencode",
                "--keep-instructions",
            ])
            .unwrap()
            .command,
            Commands::Uninstall(_)
        ));
        assert!(matches!(
            Cli::try_parse_from(["aise", "uninstall", "--keep-mcp"])
                .unwrap()
                .command,
            Commands::Uninstall(args) if args.keep_mcp
        ));
        assert_rejects(["aise", "uninstall", "--no-instructions"]);
        assert_rejects(["aise", "mcp", "install", "--client", "antigravity"]);
        assert_rejects(["aise", "mcp", "status"]);
        assert_rejects(["aise", "mcp", "uninstall"]);
    }

    #[test]
    fn config_init_requires_force_and_atomically_replaces_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        write_config_example(&path, false).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            crate::config::CONFIG_EXAMPLE_TOML
        );

        fs::write(&path, "preserve until publication").unwrap();
        assert!(write_config_example(&path, false).is_err());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "preserve until publication"
        );

        write_config_example(&path, true).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            crate::config::CONFIG_EXAMPLE_TOML
        );
    }

    #[test]
    fn show_accepts_bounded_head_tail_and_all_transcript_modes() {
        let cli = Cli::try_parse_from(["aise", "show", "abc"]).unwrap();
        let Commands::Show(args) = cli.command else {
            panic!("expected show command");
        };
        assert_eq!(args.transcript_lines, None);
        assert!(!args.summary);
        assert_eq!(args.summary_items, None);
        assert_eq!(args.preview_chars, None);

        assert_parses(["aise", "show", "abc", "--summary"]);
        assert_parses([
            "aise",
            "show",
            "abc",
            "--summary",
            "--summary-items",
            "-12",
            "--preview-chars",
            "180",
        ]);
        assert_rejects(["aise", "show", "abc", "--summary", "--raw"]);
        assert_rejects([
            "aise",
            "show",
            "abc",
            "--summary",
            "--transcript-lines",
            "20",
        ]);
        assert_parses(["aise", "show", "abc", "--transcript-lines", "20"]);
        assert_parses(["aise", "show", "abc", "--transcript-lines", "-20"]);
        assert_parses(["aise", "show", "abc", "--transcript-lines", "0"]);
    }

    #[test]
    fn doctor_accepts_machine_readable_json_format() {
        let cli = Cli::try_parse_from(["aise", "doctor", "--format", "json"]).unwrap();
        let Commands::Doctor(args) = cli.command else {
            panic!("expected doctor command");
        };
        assert_eq!(args.format, DoctorFormat::Json);
        assert!(Cli::try_parse_from(["aise", "doctor", "--format", "csv"]).is_err());
    }

    #[test]
    fn storage_guidance_is_exact_and_requires_reclaimable_bytes() {
        assert!(storage_compaction_guidance(crate::db::StorageAllocation {
            total_bytes: 4096,
            reclaimable_bytes: 0,
        })
        .is_none());
        assert_eq!(
            storage_compaction_guidance(crate::db::StorageAllocation {
                total_bytes: 8 * 1024 * 1024,
                reclaimable_bytes: 4 * 1024 * 1024,
            })
            .unwrap(),
            "4194304 bytes (4.0 MiB) of 8.0 MiB are reclaimable; run `aise compact` when an exclusive database lock and temporary disk space are available"
        );
    }

    #[test]
    fn messages_evidence_accepts_time_profile_include() {
        let cli = Cli::try_parse_from([
            "aise",
            "messages",
            "evidence",
            "abc",
            "--include",
            "time-profile",
            "--format",
            "json",
        ])
        .unwrap();
        let Commands::Messages(crate::messages::MessagesCmd::Evidence(args)) = cli.command else {
            panic!("expected messages evidence command");
        };
        assert_eq!(
            args.include,
            vec![crate::messages::EvidenceInclude::TimeProfile]
        );
    }

    #[test]
    fn messages_search_accepts_general_tool_argument_pointer_and_offset() {
        let cli = Cli::try_parse_from([
            "aise",
            "messages",
            "search",
            "cargo test",
            "--field",
            "tool-argument",
            "--argument-path",
            "/cmd",
            "--offset",
            "2",
        ])
        .unwrap();
        let Commands::Messages(crate::messages::MessagesCmd::Search(args)) = cli.command else {
            panic!("expected messages search command");
        };
        assert_eq!(args.field, crate::models::SearchField::ToolArgument);
        assert_eq!(args.argument_path.as_deref(), Some("/cmd"));
        assert_eq!(args.offset, 2);
    }

    #[test]
    fn messages_commands_accept_signed_lines_per_message() {
        let cli = Cli::try_parse_from([
            "aise",
            "messages",
            "search",
            "exit status",
            "--lines-per-message",
            "-3",
        ])
        .unwrap();
        let Commands::Messages(crate::messages::MessagesCmd::Search(args)) = cli.command else {
            panic!("expected messages search command");
        };
        assert_eq!(args.lines_per_message, Some(-3));

        assert_parses(["aise", "messages", "get", "abc", "--lines-per-message", "5"]);
        assert_parses([
            "aise",
            "messages",
            "timeline",
            "abc",
            "--lines-per-message",
            "-1",
        ]);

        let help = Cli::try_parse_from(["aise", "messages", "search", "--help"])
            .unwrap_err()
            .to_string();
        for required in [
            "does not change matches, ranking, result count, pagination, context membership, or reference extraction",
            "keep many search hits or long tool outputs skimmable without discarding hits",
            "0 keeps its complete content",
        ] {
            assert!(help.contains(required), "missing {required:?} in help: {help}");
        }
    }

    #[test]
    fn messages_search_accepts_leading_dash_literals() {
        let cli = Cli::try_parse_from(["aise", "messages", "search", "-e", "--path"]).unwrap();
        let Commands::Messages(crate::messages::MessagesCmd::Search(args)) = cli.command else {
            panic!("expected messages search command");
        };
        assert_eq!(args.query_arg.as_deref(), Some("--path"));

        assert_parses(["aise", "messages", "search", "--", "--path"]);
        assert_parses([
            "aise",
            "messages",
            "search",
            "--regex",
            "--",
            "^/[^[:space:]]+",
        ]);
        assert_parses([
            "aise",
            "migrate",
            "config",
            "--source-json",
            "old/config.json",
            "--destination",
            "new/config.toml",
            "--database-path",
            "new/index.db",
            "--cache-dir",
            "new/cache",
        ]);
        assert_rejects([
            "aise",
            "migrate",
            "config",
            "--source-json",
            "old/config.json",
            "--destination",
            "new/config.toml",
            "--database-path",
            "new/index.db",
            "--cache-dir",
            "new/cache",
            "--replace",
        ]);
        assert_parses([
            "aise",
            "messages",
            "search",
            "--fuzzy",
            "--",
            "--hyphenated query",
        ]);
        assert_parses(["aise", "repeats", "--regex", "--", "magic|config"]);
        assert_parses(["aise", "search", "--limit", "1", "--", "--path"]);
    }

    #[test]
    fn message_role_filter_has_one_canonical_cli_name() {
        assert_parses(["aise", "messages", "search", "query", "--role", "user"]);
        assert_parses(["aise", "messages", "get", "session", "--role", "user"]);
        assert!(
            Cli::try_parse_from(["aise", "messages", "search", "query", "--type", "user"]).is_err()
        );
        assert!(
            Cli::try_parse_from(["aise", "messages", "get", "session", "--type", "user"]).is_err()
        );
    }

    #[test]
    fn top_level_search_help_points_to_message_search_modes() {
        let mut cmd = Cli::command();
        let search = cmd
            .find_subcommand_mut("search")
            .expect("search subcommand");
        let mut help = Vec::new();
        search.write_long_help(&mut help).unwrap();
        let help = String::from_utf8(help).unwrap();

        assert!(help.contains("messages search --regex"), "{help}");
        assert!(help.contains("messages search --fuzzy"), "{help}");
        assert!(
            help.contains(
                "Session-level keywords, phrase, code snippet, path fragment, or title text"
            ),
            "{help}"
        );
        for provider in crate::source::PROVIDERS {
            assert!(
                help.contains(provider.as_str()),
                "missing {provider} in {help}"
            );
        }
    }

    #[test]
    fn messages_search_fuzzy_is_explicit_and_exclusive() {
        assert_parses(["aise", "messages", "search", "magic values", "--fuzzy"]);
        assert_parses(["aise", "messages", "search", "-e", "--path", "--fuzzy"]);
        assert_parses(["aise", "messages", "search", "magic.*values", "--regex"]);
        assert_parses(["aise", "messages", "search", "-e", "--path", "--regex"]);
        assert_rejects([
            "aise",
            "messages",
            "search",
            "magic values",
            "--fuzzy",
            "--rank",
        ]);
        assert_rejects(["aise", "messages", "search", "magic", "--fuzzy", "values"]);
        assert_parses(["aise", "messages", "search", "--fuzzy"]);
        assert_rejects([
            "aise",
            "messages",
            "search",
            "magic.*values",
            "--regex",
            "--fuzzy",
        ]);
    }

    #[test]
    fn messages_search_help_distinguishes_unlimited_exact_from_bounded_fuzzy() {
        let mut cmd = Cli::command();
        let messages = cmd
            .find_subcommand_mut("messages")
            .expect("messages subcommand");
        let search = messages
            .find_subcommand_mut("search")
            .expect("messages search subcommand");
        let mut help = Vec::new();
        search.write_long_help(&mut help).unwrap();
        let help = String::from_utf8(help).unwrap();

        assert!(help.contains("0 = unlimited for exact/regex"), "{help}");
        // Stated as the accepted ceiling rather than "must not exceed": a reader can inverse a
        // negated instruction, and every bound in this help reads as what to pass.
        assert!(help.contains("offset + limit at most 10,000"), "{help}");
        assert!(help.contains("minimum 3 characters"), "{help}");
    }

    #[test]
    fn planning_accepts_command_token_regex_filters() {
        assert_parses([
            "aise",
            "planning",
            "--commands",
            "^/cmd-a",
            "--commands",
            "^/cmd-b$",
        ]);
        assert_parses(["aise", "planning", "--command", "^/cmd-c$"]);
    }

    #[test]
    fn analytics_commands_accept_exact_session_id_scope() {
        for command in ["corrections", "planning", "stats", "repeats"] {
            assert_parses(["aise", command, "--session-id", "claude:abc"]);
            assert_rejects(["aise", command, "--session", "abc"]);
        }
    }

    #[test]
    fn repeats_command_parses() {
        assert_parses(["aise", "repeats", "--role", "user"]);
        assert_parses(["aise", "repeats", "magic values", "--role", "user"]);
        assert_parses(["aise", "repeats", "magic|config", "--regex"]);
        assert_parses([
            "aise",
            "repeats",
            "--min-matches",
            "3",
            "--phrase-min-words",
            "2",
            "--phrase-max-words",
            "4",
            "--max-groups",
            "20",
        ]);
        assert_rejects(["aise", "repeats", "you forgot", "--similarity"]);
        assert_rejects(["aise", "repeats", "you forgot", "--groups"]);
        assert_rejects(["aise", "repeats", "--context", "-1"]);
        assert_rejects(["aise", "similar", "--type", "user"]);
    }

    #[test]
    fn migration_commands_require_explicit_portable_paths() {
        assert_parses([
            "aise",
            "migrate",
            "database",
            "--source",
            "old/index.db",
            "--destination",
            "new/index.db",
            "--receipt",
            "new/migration.json",
            "--pages-per-step",
            "64",
            "--pause-ms",
            "1",
        ]);
        assert_parses([
            "aise",
            "migrate",
            "verify",
            "--receipt",
            "new/migration.json",
        ]);
        assert_parses([
            "aise",
            "migrate",
            "recover",
            "--receipt",
            "new/migration.json",
        ]);
        assert_rejects(["aise", "migrate", "database"]);
    }

    #[test]
    fn export_keeps_direct_single_session_and_adds_filtered_bundle_mode() {
        assert_parses(["aise", "export", "claude:abc"]);
        assert_parses([
            "aise",
            "export",
            "--provider",
            "codex",
            "--since",
            "7d",
            "--limit",
            "0",
            "--output-dir",
            "history",
        ]);
        assert_rejects([
            "aise",
            "export",
            "claude:abc",
            "--output",
            "one.md",
            "--output-dir",
            "many",
        ]);
    }
}
