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
use crate::models::{Provider, SearchFilters, SessionKind, SessionRecord};
use crate::render::{render, OutputFormat, Row};
use crate::service::SessionSearch;
use crate::tui;
use crate::util::{
    current_repo, highlight_matches, prompt_confirm, relative_age, render_posix_shell_command,
    resume_plan, select_transcript_lines, truncate_for_display,
};
use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;

/// Help section the six root-level flags are grouped under.
///
/// Every one of them is `global = true`, so clap repeats them in EVERY subcommand's help, where
/// it interleaves them alphabetically with that command's own options: `aise corrections --help`
/// listed `--config`, `--session-id`, `--database`, `--provider`, `--cache-dir`, `--path`, ...
/// A reader could not tell which flags belong to the command they are reading about. A heading
/// separates them without changing what any flag does or where it may be passed.
const GLOBAL_OPTIONS_HEADING: &str = "Global options (accepted by every command)";

#[derive(Debug, Parser)]
#[command(
    name = "aise",
    version,
    about = "AI Session Search (aise): search local sessions from Claude Code, Claude Desktop local agent, Codex, Cursor, Antigravity, Pi coding agent, Google AI Studio, and Gemini CLI"
)]
struct Cli {
    /// Explicit configuration file. Overrides AI_SESSION_SEARCH_CONFIG and platform discovery.
    #[arg(long, global = true, help_heading = GLOBAL_OPTIONS_HEADING)]
    config: Option<PathBuf>,
    /// Explicit SQLite index. Overrides AI_SESSION_SEARCH_DATABASE and config.toml.
    #[arg(long, global = true, help_heading = GLOBAL_OPTIONS_HEADING)]
    database: Option<PathBuf>,
    /// Explicit cache directory. Overrides AI_SESSION_SEARCH_CACHE_DIR and config.toml.
    #[arg(long, global = true, help_heading = GLOBAL_OPTIONS_HEADING)]
    cache_dir: Option<PathBuf>,
    /// Worker threads, an integer 1 or greater. Overrides AI_SESSION_SEARCH_THREADS and
    /// config.toml.
    #[arg(long, global = true, help_heading = GLOBAL_OPTIONS_HEADING, value_parser = parse_positive_usize)]
    threads: Option<usize>,
    /// Index refresh policy for implicit read commands. Overrides
    /// AI_SESSION_SEARCH_INDEX_REFRESH and config.toml.
    #[arg(long, global = true, help_heading = GLOBAL_OPTIONS_HEADING, value_enum)]
    index_refresh: Option<IndexRefresh>,
    /// Skip the optional release notification and its network check for this invocation.
    /// Explicit `aise package check|update` commands remain enabled.
    #[arg(long, global = true, help_heading = GLOBAL_OPTIONS_HEADING)]
    skip_release_notification: bool,
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
        after_help = "For turn-level literal, regex, or fuzzy content search, use `aise messages search QUERY` or select `--query-mode regex|fuzzy`."
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
    /// Manage executable aliases, client registrations, instructions, and skills.
    #[command(subcommand)]
    Integrations(IntegrationsCmd),
    /// Inspect the skills that supply correction rules: list, explain, and validate.
    #[command(subcommand)]
    Skills(crate::skills::SkillsCmd),
    /// Inspect, check, or update the installed aise distribution.
    #[command(subcommand)]
    Package(PackageCmd),
    /// Serve MCP JSON-RPC over standard input/output.
    #[command(subcommand)]
    Mcp(crate::integrations::McpCmd),
    /// Expert read-only SQL over the local AI session-history index.
    #[command(subcommand)]
    Db(crate::sql_query::DbCmd),
    /// Safely migrate or verify a session index database.
    #[command(subcommand)]
    Migrate(MigrationCmd),
    /// Inspect effective configuration, its file, origins, and resolved filesystem paths.
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Show the supported --since/--until/--when date and EDTF formats.
    Dates,
    /// Check index health, provider discovery, and resume-tool availability.
    Doctor(DoctorArgs),
    /// Launch the interactive terminal UI for browsing and resuming sessions.
    Tui,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RootOption {
    ConfigFile,
    Database,
    CacheDirectory,
    WorkerThreads,
    IndexRefresh,
    ReleaseNotification,
}

impl RootOption {
    fn flag(self) -> &'static str {
        match self {
            Self::ConfigFile => "--config",
            Self::Database => "--database",
            Self::CacheDirectory => "--cache-dir",
            Self::WorkerThreads => "--threads",
            Self::IndexRefresh => "--index-refresh",
            Self::ReleaseNotification => "--skip-release-notification",
        }
    }

    fn is_present(self, cli: &Cli) -> bool {
        match self {
            Self::ConfigFile => cli.config.is_some(),
            Self::Database => cli.database.is_some(),
            Self::CacheDirectory => cli.cache_dir.is_some(),
            Self::WorkerThreads => cli.threads.is_some(),
            Self::IndexRefresh => cli.index_refresh.is_some(),
            Self::ReleaseNotification => cli.skip_release_notification,
        }
    }
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
struct PackageUpdateArgs {
    /// Invoke an evidence-backed package manager without confirmation when a newer release exists.
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct ReportArgs {
    /// Output format: table for people or JSON for scripts.
    #[arg(long, value_enum, default_value_t = ReportOutputFormat::Table)]
    format: ReportOutputFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "lowercase")]
pub(crate) enum ReportOutputFormat {
    Table,
    Json,
}

#[derive(Debug, Subcommand)]
enum PackageCmd {
    /// Inspect the running executable, PATH candidates, owner evidence, and manager command.
    Status(ReportArgs),
    /// Check GitHub for a newer release in this build's stable or prerelease channel.
    Check(ReportArgs),
    /// Check and, when newer, invoke the evidence-backed package manager after confirmation.
    Update(PackageUpdateArgs),
}

#[derive(Debug, Subcommand)]
enum IntegrationsCmd {
    /// Install executable aliases, client registrations, managed instructions, and skills.
    Install(crate::integrations::IntegrationInstallArgs),
    /// Inspect executable aliases, client registrations, managed instructions, and skills.
    Status(crate::integrations::IntegrationStatusArgs),
    /// Remove owned integrations while preserving the aise package, database, and cache.
    Uninstall(crate::integrations::IntegrationUninstallArgs),
    /// Recover or finalize an interrupted integration transaction.
    Recover(crate::integrations::IntegrationRecoverArgs),
}

#[derive(Debug, Args)]
struct DoctorArgs {
    /// Output format. JSON is the stable machine-readable status shared with MCP.
    #[arg(long, value_enum, default_value_t = DoctorFormat::Table)]
    format: DoctorFormat,
    /// For each discovered file that produced no session, report the session id its content
    /// resolves to and which indexed file already holds that id. Reads the files; it does not
    /// modify the index. Use when `unindexed_files` is non-zero and you need the reason rather
    /// than the count.
    #[arg(long)]
    explain_unindexed: bool,
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
    /// Restrict to one session class; one-value alias for --session-kinds.
    #[arg(long = "session-kind", value_enum)]
    session_kind: Option<SessionKind>,
    /// Session classes to return: user for sessions you started, subagent for runs those
    /// sessions spawned. Omit for both. Pass subagent to search only delegated work, or user
    /// to list conversations without the runs beneath them.
    #[arg(
        long = "session-kinds",
        value_enum,
        num_args = 1..,
        value_delimiter = ',',
        conflicts_with = "session_kind"
    )]
    session_kinds: Vec<SessionKind>,
    /// Restrict to runs spawned by this exact session id.
    #[arg(long = "parent-session")]
    parent_session: Option<String>,
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
    /// Print the selected config file path without reading or creating the file.
    File,
    /// Print the embedded commented example config.
    Example,
    /// Write the embedded commented example config to the default config path.
    Init(ConfigInitArgs),
    /// Print the effective config after defaults and config.toml are merged.
    Show(ConfigShowArgs),
    /// Print origins for config, database, cache, threads, refresh policy, and search scope.
    Origins,
    /// Print resolved config, state, search-scope, and session-source paths.
    Paths(ReportArgs),
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
    validate_root_options(&cli)?;
    if matches!(&cli.command, Commands::RefreshIndex) {
        return run_background_refresh_from_stdin();
    }
    let skip_release_notification = cli.skip_release_notification;
    let overrides = ConfigOverrides {
        config_path: cli.config,
        database_path: cli.database,
        cache_dir: cli.cache_dir,
        threads: cli.threads,
        index_refresh: cli.index_refresh,
    };
    let command = match cli.command {
        Commands::Integrations(IntegrationsCmd::Install(args)) => {
            let config_path = Config::selected_config_path(overrides.config_path.clone());
            let receipt = crate::integrations::default_transaction_receipt(&config_path);
            return crate::integrations::install_with_receipt(args, &receipt);
        }
        Commands::Integrations(IntegrationsCmd::Status(args)) => {
            let config_path = Config::selected_config_path(overrides.config_path.clone());
            let receipt = crate::integrations::default_transaction_receipt(&config_path);
            return crate::integrations::status_with_receipt(args, &receipt);
        }
        Commands::Integrations(IntegrationsCmd::Uninstall(args)) => {
            let config_path = Config::selected_config_path(overrides.config_path.clone());
            let receipt = crate::integrations::default_transaction_receipt(&config_path);
            return crate::integrations::uninstall_with_receipt(args, &receipt);
        }
        Commands::Integrations(IntegrationsCmd::Recover(args)) => {
            let config_path = Config::selected_config_path(overrides.config_path.clone());
            let receipt = crate::integrations::default_transaction_receipt(&config_path);
            return crate::integrations::recover_with_receipt(args, &receipt);
        }
        Commands::Package(PackageCmd::Status(args)) => {
            return crate::update::print_package_status(args.format);
        }
        command => command,
    };
    let command = match command {
        Commands::Mcp(crate::integrations::McpCmd::Serve) => {
            let resolved = Config::resolve(overrides.clone())?;
            return crate::mcp_server::serve_with_config(resolved.config);
        }
        command => command,
    };

    if let Commands::Config(cmd) = &command {
        match cmd {
            ConfigCmd::File => {
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
            ConfigCmd::Show(_) | ConfigCmd::Origins | ConfigCmd::Paths(_) => {}
        }
    }

    if let Commands::Migrate(cmd) = command {
        return run_migration(cmd);
    }

    let resolved = Config::resolve(overrides)?;
    let config = resolved.config.clone();
    if let Commands::Package(command) = &command {
        return match command {
            PackageCmd::Status(_) => {
                unreachable!("package status returns before configuration resolution")
            }
            PackageCmd::Check(args) => crate::update::run_package_check(&config, args.format),
            PackageCmd::Update(args) => crate::update::run_package_update(&config, args.yes),
        };
    }
    if let Commands::Db(cmd) = command {
        if matches!(&cmd, crate::sql_query::DbCmd::Query(_)) {
            crate::search_scope::ensure_raw_sql_allowed(&config.search.scope, "aise db query")?;
        }
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
    if let Commands::Skills(cmd) = command {
        // Config, never the index: `skills list` answers "which rules would run", which no
        // session data can change. Opening the database here would also trigger a refresh.
        //
        // The receipt path is the same one `integrations install` uses, so the writing verbs share
        // its recovery record and its manifest location rather than inventing a second pair.
        let receipt = crate::integrations::default_transaction_receipt(&resolved.config_path);
        return crate::skills::run(&config, cmd, &receipt);
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
                let dry_run = render_posix_shell_command(&[
                    "aise".to_string(),
                    "resume".to_string(),
                    args.id.clone(),
                    "--dry-run".to_string(),
                ])?;
                return Err(anyhow!(
                    "resume command `{rendered}` exited with status {status}; rerun \
                     `{dry_run}` to print the command without executing it, or check \
                     the tool's own error output above",
                ));
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
                    return Err(anyhow!(
                        "aise export {id} does not accept corpus filters, --limit, or \
                         --output-dir (those apply to multi-session export); drop --id to \
                         export a filtered corpus, or drop --limit/--output-dir/filters to \
                         export just {id}"
                    ));
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
        Commands::Messages(cmd) => crate::messages::run(db, &cmd, &config)?,
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
        Commands::Doctor(args) => print_doctor(&config, db, args.format, args.explain_unindexed)?,
        Commands::Tui => {
            schedule_auto_refresh_after_output(&config, db, implicit_read, &mut refresh_scheduled);
            tui::run(&config, db)?
        }
        Commands::Mcp(_) => unreachable!("MCP serving returns before opening the DB"),
        Commands::Integrations(_) => {
            unreachable!("integration lifecycle commands return before configuration")
        }
        Commands::Package(_) => unreachable!("package commands return before opening the DB"),
        Commands::Db(_) => unreachable!("DB query commands return before opening the write DB"),
        Commands::Migrate(_) => unreachable!("migration commands return before opening the DB"),
        Commands::Config(_) => unreachable!("Config commands return before opening the DB"),
        Commands::Skills(_) => unreachable!("skill commands return before opening the DB"),
        Commands::RefreshIndex => unreachable!("background refresh returns before configuration"),
    }

    schedule_auto_refresh_after_output(&config, db, implicit_read, &mut refresh_scheduled);
    // Close SQLite and the per-instance worker pool before an optional network check.
    drop(app);
    crate::update::notify_if_new_stable_release_available_after_cli_output(
        &config,
        skip_release_notification,
    );

    Ok(())
}

fn validate_root_options(cli: &Cli) -> Result<()> {
    const CONFIG_INPUTS: &[RootOption] = &[
        RootOption::ConfigFile,
        RootOption::Database,
        RootOption::CacheDirectory,
        RootOption::WorkerThreads,
        RootOption::IndexRefresh,
    ];
    const CONFIG_AND_CACHE: &[RootOption] = &[RootOption::ConfigFile, RootOption::CacheDirectory];
    const CONFIG_DATABASE_AND_CACHE: &[RootOption] = &[
        RootOption::ConfigFile,
        RootOption::Database,
        RootOption::CacheDirectory,
    ];
    const CONFIG_FILE_ONLY: &[RootOption] = &[RootOption::ConfigFile];
    const DATABASE_COMMAND_INPUTS: &[RootOption] = &[RootOption::ConfigFile, RootOption::Database];
    const ORDINARY_COMMAND_INPUTS: &[RootOption] = &[
        RootOption::ConfigFile,
        RootOption::Database,
        RootOption::CacheDirectory,
        RootOption::WorkerThreads,
        RootOption::IndexRefresh,
        RootOption::ReleaseNotification,
    ];
    const NO_ROOT_OPTIONS: &[RootOption] = &[];

    let (command_name, allowed) = match &cli.command {
        Commands::Integrations(IntegrationsCmd::Install(_)) => {
            ("aise integrations install", CONFIG_FILE_ONLY)
        }
        Commands::Integrations(IntegrationsCmd::Status(_)) => {
            ("aise integrations status", CONFIG_FILE_ONLY)
        }
        Commands::Integrations(IntegrationsCmd::Uninstall(_)) => {
            ("aise integrations uninstall", CONFIG_FILE_ONLY)
        }
        Commands::Integrations(IntegrationsCmd::Recover(_)) => {
            ("aise integrations recover", CONFIG_FILE_ONLY)
        }
        Commands::Package(PackageCmd::Status(_)) => ("aise package status", NO_ROOT_OPTIONS),
        Commands::Package(PackageCmd::Check(_)) => ("aise package check", CONFIG_AND_CACHE),
        Commands::Package(PackageCmd::Update(_)) => ("aise package update", CONFIG_AND_CACHE),
        Commands::Mcp(_) => ("aise mcp serve", CONFIG_INPUTS),
        Commands::Config(ConfigCmd::File) => ("aise config file", CONFIG_FILE_ONLY),
        Commands::Config(ConfigCmd::Example) => ("aise config example", NO_ROOT_OPTIONS),
        Commands::Config(ConfigCmd::Init(_)) => ("aise config init", CONFIG_FILE_ONLY),
        Commands::Config(ConfigCmd::Show(_)) => ("aise config show", CONFIG_INPUTS),
        Commands::Config(ConfigCmd::Origins) => ("aise config origins", CONFIG_INPUTS),
        Commands::Config(ConfigCmd::Paths(_)) => ("aise config paths", CONFIG_DATABASE_AND_CACHE),
        Commands::Db(_) => ("aise db", DATABASE_COMMAND_INPUTS),
        Commands::Migrate(_) => ("aise migrate", NO_ROOT_OPTIONS),
        Commands::Dates => ("aise dates", NO_ROOT_OPTIONS),
        Commands::RefreshIndex => ("aise __refresh-index", NO_ROOT_OPTIONS),
        _ => ("this command", ORDINARY_COMMAND_INPUTS),
    };

    for option in [
        RootOption::ConfigFile,
        RootOption::Database,
        RootOption::CacheDirectory,
        RootOption::WorkerThreads,
        RootOption::IndexRefresh,
        RootOption::ReleaseNotification,
    ] {
        if option.is_present(cli) && !allowed.contains(&option) {
            bail!(
                "{} does not apply to `{command_name}`; remove it",
                option.flag()
            );
        }
    }
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
        ConfigCmd::File => println!("{}", resolved.config_path.display()),
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
        ConfigCmd::Origins => println!("{}", serde_json::to_string_pretty(&resolved.origins)?),
        ConfigCmd::Paths(args) => {
            print_config_paths(&resolved.config, &resolved.config_path, args.format)?
        }
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
        && filters.session_kinds.is_none()
        && filters.parent_session_id.is_none()
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
        // Clap enforces that --session-kind and --session-kinds are not both given, so this
        // cannot silently drop one. Same resolution as messages.rs for --kind/--kinds.
        session_kinds: args
            .session_kind
            .map(|kind| vec![kind])
            .or_else(|| (!args.session_kinds.is_empty()).then(|| args.session_kinds.clone())),
        parent_session_id: args.parent_session.clone(),
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
    // Only a spawned run has these, so an ordinary session's rows are unchanged. Without this
    // line `agent_label` was stored and filterable but rendered nowhere, which is a field
    // documented "display and grouping only" that nothing displayed. Short keys match the
    // `cwd=`/`preview=` style above rather than the serialized field names.
    if session.parent_session_id.is_some() || session.agent_label.is_some() {
        let agent = session.agent_label.as_deref().unwrap_or("-");
        let parent = session.parent_session_id.as_deref().unwrap_or("-");
        println!("  agent={agent}  parent={parent}");
    }
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

/// Name each discovered file that produced no session, and what took its place.
///
/// This answers the question that previously required joining index state against a directory
/// listing by hand, which no SQL-only interface can express because half the question is the
/// filesystem. The reason is recomputed from the files rather than read from storage; see
/// `diagnostics::explain_unindexed`.
fn print_unindexed_explanation(unindexed: &[crate::diagnostics::UnindexedFile]) {
    if unindexed.is_empty() {
        println!("\nUnindexed files: none; every discovered file produced a session.");
        return;
    }
    println!("\nUnindexed files: {}", unindexed.len());
    for item in unindexed {
        println!("  {} [{}]", item.path, item.provider);
        match &item.id_already_held_by {
            Some(holder) => println!(
                "    resolves to session id {}, which is already held by {holder}",
                item.resolves_to
            ),
            None => println!(
                "    resolves to session id {}, which no indexed file holds, so this file was \
                 skipped for another reason",
                item.resolves_to
            ),
        }
    }
}

fn print_doctor(
    config: &Config,
    db: &Db,
    format: DoctorFormat,
    explain_unindexed: bool,
) -> Result<()> {
    let diagnostics = crate::diagnostics::collect(config, db)?;
    let unindexed = explain_unindexed
        .then(|| crate::diagnostics::explain_unindexed(config, db))
        .transpose()?;
    if format == DoctorFormat::Json {
        let mut document = serde_json::to_value(&diagnostics)?;
        if let Some(unindexed) = unindexed {
            document
                .as_object_mut()
                .expect("DiagnosticStatus serializes as an object")
                .insert(
                    "unindexed_file_explanations".to_string(),
                    serde_json::to_value(unindexed)?,
                );
        }
        println!("{}", serde_json::to_string_pretty(&document)?);
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
    // "N stale, 0 repairable" with no repair command reads as N sessions needing a reparse.
    // They are retained sessions whose transcripts were deleted from disk, which is a normal
    // resting state, so say that no action exists rather than leaving a bare "stale" count.
    if status.unavailable_stale_sessions > 0 {
        println!(
            "  of those, {} are retained: the transcript was deleted from disk, so the \
             indexed copy is all that remains and no repair applies",
            status.unavailable_stale_sessions
        );
    }
    // Stated as a consequence, not as a bare count: the reason this mattered is that a reader
    // seeing only "discovered 414 / indexed 349" concluded the index was healthy.
    if status.unindexed_files > 0 {
        println!(
            "Unindexed: {} discovered file(s) produced no session, so their content is \
             absent from every search result",
            status.unindexed_files
        );
    }
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
        if item.unindexed_files > 0 {
            println!(
                "  files not indexed: {} (discovered but absent from the index)",
                item.unindexed_files
            );
        }
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
    if let Some(unindexed) = unindexed {
        print_unindexed_explanation(&unindexed);
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

#[derive(Debug, Serialize)]
struct ConfigPathsReport {
    config_file: PathBuf,
    database: PathBuf,
    cache: PathBuf,
    search_scope: ConfigSearchScopeReport,
    background_refresh_status: PathBuf,
    provider_roots: Vec<ProviderRootsReport>,
    codex_metadata_home: PathBuf,
}

#[derive(Debug, Serialize)]
#[serde(tag = "mode", rename_all = "kebab-case")]
enum ConfigSearchScopeReport {
    All,
    AllowedRoots { roots: Vec<AllowedRootReport> },
}

#[derive(Debug, Serialize)]
struct AllowedRootReport {
    path: PathBuf,
    sources: Vec<AllowedRootSourceReport>,
}

#[derive(Debug, Serialize)]
struct AllowedRootSourceReport {
    origin: String,
    configured_path: PathBuf,
}

#[derive(Debug, Serialize)]
struct ProviderRootsReport {
    provider: Provider,
    enabled: bool,
    roots: Vec<PathBuf>,
}

fn config_paths_report(
    config: &Config,
    config_path: &std::path::Path,
) -> Result<ConfigPathsReport> {
    let access = crate::search_scope::EffectiveAccessScope::resolve(
        &config.search.scope,
        crate::search_scope::TrustedAccessInputs::capture(&config.search.scope, Vec::new())?,
    )?;
    let search_scope = match access {
        crate::search_scope::EffectiveAccessScope::All => ConfigSearchScopeReport::All,
        crate::search_scope::EffectiveAccessScope::AllowedRoots { roots } => {
            ConfigSearchScopeReport::AllowedRoots {
                roots: roots
                    .into_iter()
                    .map(|root| AllowedRootReport {
                        path: root.path().to_path_buf(),
                        sources: root
                            .sources()
                            .iter()
                            .map(|source| AllowedRootSourceReport {
                                origin: source.origin().as_str().to_owned(),
                                configured_path: source.configured_path().to_path_buf(),
                            })
                            .collect(),
                    })
                    .collect(),
            }
        }
    };
    let provider_roots = crate::source::PROVIDERS
        .into_iter()
        .map(|provider| ProviderRootsReport {
            provider,
            enabled: crate::source::provider_enabled(config, provider),
            roots: crate::source::provider_roots(config, provider),
        })
        .collect();
    Ok(ConfigPathsReport {
        config_file: config_path.to_path_buf(),
        database: config.db_path(),
        cache: config.cache_dir(),
        search_scope,
        background_refresh_status: crate::background_refresh::report_path(config),
        provider_roots,
        codex_metadata_home: config.codex_home(),
    })
}

fn print_config_paths(
    config: &Config,
    config_path: &std::path::Path,
    format: ReportOutputFormat,
) -> Result<()> {
    let report = config_paths_report(config, config_path)?;
    if format == ReportOutputFormat::Json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "Config: {}", report.config_file.display())?;
    writeln!(out, "DB: {}", report.database.display())?;
    writeln!(out, "Cache: {}", report.cache.display())?;
    match &report.search_scope {
        ConfigSearchScopeReport::All => {
            writeln!(out, "Search scope: all (unrestricted)")?;
        }
        ConfigSearchScopeReport::AllowedRoots { roots } => {
            writeln!(out, "Search scope: allowed-roots")?;
            for root in roots {
                let sources = root
                    .sources
                    .iter()
                    .map(|source| format!("{}:{}", source.origin, source.configured_path.display()))
                    .collect::<Vec<_>>()
                    .join(", ");
                writeln!(
                    out,
                    "Search allowed root: {} ({sources})",
                    root.path.display()
                )?;
            }
        }
    }
    writeln!(
        out,
        "Background refresh status: {}",
        report.background_refresh_status.display()
    )?;
    for provider in &report.provider_roots {
        writeln!(
            out,
            "{} roots: {}{}",
            provider.provider.display_name(),
            provider
                .roots
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            if provider.enabled { "" } else { " (disabled)" }
        )?;
    }
    writeln!(
        out,
        "Codex metadata home: {}",
        report.codex_metadata_home.display()
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

    fn assert_root_options_apply<const N: usize>(args: [&str; N]) {
        let cli = Cli::try_parse_from(args)
            .unwrap_or_else(|err| panic!("expected CLI args to parse: {args:?}: {err}"));
        validate_root_options(&cli)
            .unwrap_or_else(|err| panic!("expected root options to apply: {args:?}: {err}"));
    }

    fn assert_root_option_is_irrelevant<const N: usize>(args: [&str; N], flag: &str) {
        let cli = Cli::try_parse_from(args)
            .unwrap_or_else(|err| panic!("expected CLI args to parse: {args:?}: {err}"));
        let error = validate_root_options(&cli).unwrap_err().to_string();
        assert!(error.contains(flag), "{error}");
        assert!(error.contains("does not apply"), "{error}");
    }

    /// The session-class filter reaches every command that takes session filters, because it
    /// lives on the flattened `SessionFilterArgs` rather than on one subcommand.
    #[test]
    fn session_class_filters_reach_every_session_command() {
        // `search` takes a required QUERY, `list` takes none; both flatten SessionFilterArgs.
        for base in [vec!["aise", "search", "migrations"], vec!["aise", "list"]] {
            for accepted in [
                ["--session-kinds", "subagent"],
                ["--session-kinds", "user,subagent"],
                ["--session-kind", "user"],
                [
                    "--parent-session",
                    "claude:7e745098-c299-4cf5-bdbe-5cdb1fb5a62d",
                ],
            ] {
                let mut args = base.clone();
                args.extend_from_slice(&accepted);
                Cli::try_parse_from(&args)
                    .unwrap_or_else(|err| panic!("expected {args:?} to parse: {err}"));
            }

            // The spellings the provider survey ruled out must not be accepted here either;
            // clap validates against the same enum the parser and the MCP schema derive from.
            // The last case keeps one class filter, so the alias cannot disagree with the set.
            for rejected in [
                vec!["--session-kinds", "agent"],
                vec!["--session-kinds", "top-level"],
                vec!["--session-kind", "user", "--session-kinds", "subagent"],
            ] {
                let mut args = base.clone();
                args.extend_from_slice(&rejected);
                assert!(
                    Cli::try_parse_from(&args).is_err(),
                    "expected {args:?} to be rejected"
                );
            }
        }
    }

    /// A named set reaches `SearchFilters` intact, and naming none leaves the default in place
    /// rather than materializing a set that would then have to be kept in step with it.
    #[test]
    fn session_class_args_resolve_to_filters() {
        let filters = |args: &[&str]| {
            let cli = Cli::try_parse_from(
                std::iter::once("aise")
                    .chain(args.iter().copied())
                    .collect::<Vec<_>>(),
            )
            .expect("args parse");
            match cli.command {
                Commands::List(args) => build_filters(&args.filters, 10).expect("built"),
                other => panic!("expected list, got {other:?}"),
            }
        };

        assert_eq!(filters(&["list"]).session_kinds, None);
        assert_eq!(
            filters(&["list", "--session-kinds", "subagent"]).session_kinds,
            Some(vec![SessionKind::Subagent])
        );
        assert_eq!(
            filters(&["list", "--session-kinds", "user,subagent"]).session_kinds,
            Some(vec![SessionKind::User, SessionKind::Subagent])
        );
        assert_eq!(
            filters(&["list", "--session-kind", "user"]).session_kinds,
            Some(vec![SessionKind::User]),
            "the one-value alias resolves into the same set the list does"
        );
        assert_eq!(
            filters(&["list", "--parent-session", "claude:abc"]).parent_session_id,
            Some("claude:abc".to_string())
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
    fn messages_search_limit_help_explains_latest_match_window() {
        let help = Cli::try_parse_from(["aise", "messages", "search", "--help"])
            .unwrap_err()
            .to_string();
        assert!(
            help.contains("--match-window latest"),
            "limit help must name the latest-match selector: {help}"
        );
        assert!(
            help.contains("with one session"),
            "limit help must state the latest-window session scope: {help}"
        );
    }

    #[test]
    fn config_commands_parse() {
        assert_parses(["aise", "config", "file"]);
        assert_parses(["aise", "config", "paths"]);
        assert_parses(["aise", "config", "example"]);
        assert_parses(["aise", "config", "init", "--force"]);
        assert_parses(["aise", "config", "show"]);
        assert_parses(["aise", "config", "show", "--format", "json"]);
        assert_parses(["aise", "config", "origins"]);
        assert_parses([
            "aise",
            "--config",
            "/tmp/config.toml",
            "--database",
            "/tmp/index.db",
            "--cache-dir",
            "/tmp/cache",
            "config",
            "paths",
        ]);
        assert!(Cli::try_parse_from(["aise", "--threads", "0", "config", "paths"]).is_err());
        assert_parses(["aise", "package", "status"]);
        assert_rejects(["aise", "paths"]);
        assert_rejects(["aise", "config", "path"]);
        assert_rejects(["aise", "config", "explain"]);
        assert_rejects(["aise", "installation"]);
        assert_rejects(["aise", "package"]);
    }

    #[test]
    fn package_lifecycle_commands_have_distinct_effects() {
        assert_parses(["aise", "package", "status"]);
        assert_parses(["aise", "package", "status", "--format", "json"]);
        assert_parses(["aise", "package", "check"]);
        assert_parses(["aise", "package", "check", "--format", "json"]);
        assert_parses(["aise", "package", "update"]);
        assert_parses(["aise", "package", "update", "--yes"]);
        assert_rejects(["aise", "update"]);
        assert_rejects(["aise", "package", "status", "--yes"]);
        assert_rejects(["aise", "package", "check", "--yes"]);
        assert_rejects(["aise", "package", "update", "--check-only"]);
    }

    #[test]
    fn root_options_are_rejected_when_the_selected_command_ignores_them() {
        assert_root_options_apply([
            "aise",
            "package",
            "check",
            "--config",
            "/tmp/config.toml",
            "--cache-dir",
            "/tmp/cache",
        ]);
        assert_root_options_apply([
            "aise",
            "--config",
            "/tmp/config.toml",
            "--database",
            "/tmp/index.db",
            "--cache-dir",
            "/tmp/cache",
            "config",
            "paths",
        ]);
        assert_root_option_is_irrelevant(
            ["aise", "--database", "/tmp/index.db", "package", "status"],
            "--database",
        );
        assert_root_option_is_irrelevant(
            ["aise", "--threads", "2", "config", "paths"],
            "--threads",
        );
        assert_root_option_is_irrelevant(
            ["aise", "--skip-release-notification", "package", "check"],
            "--skip-release-notification",
        );
        assert_root_option_is_irrelevant(
            ["aise", "--config", "/tmp/config.toml", "config", "example"],
            "--config",
        );
        assert_root_option_is_irrelevant(
            ["aise", "--skip-release-notification", "mcp", "serve"],
            "--skip-release-notification",
        );
    }

    #[test]
    fn root_options_parse_before_or_after_subcommands_and_remain_semantically_checked() {
        let root_help = Cli::try_parse_from(["aise", "--help"])
            .unwrap_err()
            .to_string();
        assert!(root_help.contains("--database"));
        assert!(root_help.contains("--skip-release-notification"));

        assert_root_options_apply(["aise", "search", "needle", "--config", "/tmp/config.toml"]);
        assert_root_option_is_irrelevant(
            ["aise", "package", "status", "--database", "/tmp/index.db"],
            "--database",
        );
    }

    #[test]
    fn integration_commands_use_one_explicit_namespace() {
        let cli = Cli::try_parse_from([
            "aise",
            "integrations",
            "install",
            "--client",
            "antigravity",
            "--client",
            "opencode",
            "--exclude-client",
            "opencode",
        ])
        .unwrap();
        let Commands::Integrations(IntegrationsCmd::Install(args)) = cli.command else {
            panic!("expected integrations install command");
        };
        assert_eq!(
            args.targets.clients,
            [
                crate::integrations::McpClient::Antigravity,
                crate::integrations::McpClient::Opencode,
            ]
        );
        assert_eq!(
            args.targets.excluded_clients,
            [crate::integrations::McpClient::Opencode]
        );
        assert!(matches!(
            Cli::try_parse_from(["aise", "integrations", "install", "--no-mcp"])
                .unwrap()
                .command,
            Commands::Integrations(IntegrationsCmd::Install(args)) if args.no_mcp
        ));
        assert!(matches!(
            Cli::try_parse_from(["aise", "integrations", "status", "--no-mcp"])
                .unwrap()
                .command,
            Commands::Integrations(IntegrationsCmd::Status(args)) if args.no_mcp
        ));
        assert!(matches!(
            Cli::try_parse_from(["aise", "integrations", "status", "--client", "opencode"])
                .unwrap()
                .command,
            Commands::Integrations(IntegrationsCmd::Status(_))
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "aise",
                "integrations",
                "uninstall",
                "--client",
                "opencode",
                "--keep-instructions",
            ])
            .unwrap()
            .command,
            Commands::Integrations(IntegrationsCmd::Uninstall(_))
        ));
        assert!(matches!(
            Cli::try_parse_from(["aise", "integrations", "uninstall", "--keep-mcp"])
                .unwrap()
                .command,
            Commands::Integrations(IntegrationsCmd::Uninstall(args)) if args.keep_mcp
        ));
        assert_rejects(["aise", "integrations", "uninstall", "--no-instructions"]);
        assert_rejects(["aise", "install"]);
        assert_rejects(["aise", "status"]);
        assert_rejects(["aise", "uninstall"]);
        assert_rejects(["aise", "mcp", "install", "--client", "antigravity"]);
        assert_rejects(["aise", "mcp", "status"]);
        assert_rejects(["aise", "mcp", "uninstall"]);
        assert_parses(["aise", "integrations", "recover"]);
        assert_parses([
            "aise",
            "integrations",
            "recover",
            "--transaction-receipt",
            "/tmp/integrations.json",
        ]);
        assert_rejects(["aise", "mcp", "recover"]);
        assert_parses(["aise", "mcp", "serve"]);
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
            "--query-mode",
            "regex",
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
            "--query-mode",
            "fuzzy",
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

        assert!(help.contains("--query-mode regex|fuzzy"), "{help}");
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
    fn messages_search_query_mode_is_explicit_and_closed() {
        assert_parses([
            "aise",
            "messages",
            "search",
            "magic values",
            "--query-mode",
            "fuzzy",
        ]);
        assert_parses([
            "aise",
            "messages",
            "search",
            "-e",
            "--path",
            "--query-mode",
            "fuzzy",
        ]);
        assert_parses([
            "aise",
            "messages",
            "search",
            "magic.*values",
            "--query-mode",
            "regex",
        ]);
        assert_parses([
            "aise",
            "messages",
            "search",
            "-e",
            "--path",
            "--query-mode",
            "regex",
        ]);
        assert_rejects([
            "aise",
            "messages",
            "search",
            "magic values",
            "--query-mode",
            "fuzzy",
            "--rank",
        ]);
        assert_rejects([
            "aise",
            "messages",
            "search",
            "magic",
            "--query-mode",
            "fuzzy",
            "values",
        ]);
        assert_parses(["aise", "messages", "search", "--query-mode", "fuzzy"]);
        assert_rejects([
            "aise",
            "messages",
            "search",
            "magic.*values",
            "--query-mode",
            "unknown",
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

        assert!(help.contains("--all-results"), "{help}");
        assert!(help.contains("ranks every eligible match"), "{help}");
        assert!(help.contains("Fuzzy offsets apply after"), "{help}");
        assert!(help.contains("Fuzzy search is always bounded"), "{help}");
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
