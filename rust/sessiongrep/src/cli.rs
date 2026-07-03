use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{anyhow, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::json;

use crate::tui;
use sessiongrep::config::Config;
use sessiongrep::dates::DateRange;
use sessiongrep::db::Db;
use sessiongrep::indexer;
use sessiongrep::inspect::{inspect_session, inspection_rows, InspectionOptions};
use sessiongrep::models::{Provider, ProviderHealth, SearchFilters, SessionRecord};
use sessiongrep::providers::{
    antigravity::AntigravityAdapter, claude::ClaudeAdapter, codex::CodexAdapter,
    cursor::CursorAdapter, pi::PiAdapter,
};
use sessiongrep::render::{render, OutputFormat, Row};
use sessiongrep::util::{
    current_repo, highlight_matches, normalize_path, prompt_confirm, relative_age, render_command,
    resume_plan, select_transcript_lines, truncate_for_display, which,
};

#[derive(Debug, Parser)]
#[command(
    name = "sessiongrep",
    version,
    about = "Search, read, and resume your AI coding-agent session history (Claude Code, Claude Desktop, Codex, Cursor, Antigravity, Pi)"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Rebuild the index from session files (incremental; `--full` reparses everything).
    Reindex(ReindexArgs),
    /// Reclaim disk space: merge FTS segments, `VACUUM`, then truncate the WAL.
    Compact,
    /// List recent sessions (newest first), with optional provider/path/date filters.
    List(QueryArgs),
    /// Search sessions by keyword, ranked by relevance, across all agents.
    Search(SearchArgs),
    /// Print one session's transcript and metadata (bounded by default).
    Show(ShowArgs),
    /// Resume a session in its native CLI: print the command, or run it with confirmation.
    Resume(ResumeArgs),
    /// Export a full session to a file or stdout (markdown/json/text).
    Export(ExportArgs),
    /// Search and read individual messages, i.e. conversation turns (search|get|timeline).
    #[command(subcommand)]
    Messages(sessiongrep::messages::MessagesCmd),
    /// Find user messages where corrections were given (categorized).
    Corrections(sessiongrep::analytics::CorrectionsArgs),
    /// Aggregate slash-command usage frequency.
    Planning(sessiongrep::analytics::PlanningArgs),
    /// Message counts by role.
    Stats(sessiongrep::analytics::StatsArgs),
    /// Term-frequency vocabulary over the message index (fts5vocab).
    Vocab(sessiongrep::analytics::VocabArgs),
    /// Find recurring phrases in session messages.
    Repeats(sessiongrep::analytics::RepeatsArgs),
    /// Recover edited files: search/history/cross-ref/extract.
    #[command(subcommand)]
    Files(sessiongrep::files::FilesCmd),
    /// Install, inspect, or remove sessiongrep-mcp client configuration.
    #[command(subcommand)]
    Mcp(sessiongrep::mcp_install::McpCmd),
    /// Expert read-only SQL over the local AI session-history index.
    #[command(subcommand)]
    Db(sessiongrep::sql_query::DbCmd),
    /// Print effective configuration or the config file path.
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Show the supported --since/--until/--when date and EDTF formats.
    Dates,
    /// Check index health, provider discovery, and resume-tool availability.
    Doctor,
    /// Print the paths sessiongrep reads and writes (database, cache, config, providers).
    Paths,
    /// Launch the interactive terminal UI for browsing and resuming sessions.
    Tui,
}

#[derive(Debug, Args)]
struct ReindexArgs {
    /// Reparse every session file, ignoring the mtime/size skip cache.
    #[arg(long)]
    full: bool,
}

#[derive(Debug, Args, Clone)]
struct QueryArgs {
    /// Restrict to one provider (claude, claude-desktop, codex, cursor, antigravity, or pi).
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
    /// Maximum number of rows to return. Omit to use [search].default_limit from config.
    #[arg(long)]
    limit: Option<usize>,
    /// Show only sessions that produced a parse warning.
    #[arg(long)]
    warnings_only: bool,
    /// Output format. `table` (default) keeps the rich human layout; json/jsonl/csv/plain
    /// emit machine-readable rows.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,
}

#[derive(Debug, Args)]
struct SearchArgs {
    query: String,
    #[command(flatten)]
    filters: QueryArgs,
}

#[derive(Debug, Args)]
struct ShowArgs {
    /// Session id or unambiguous id prefix (e.g. `claude:79accec8` or `79accec8`).
    id: String,
    /// Transcript lines to print: positive=head, negative=tail, 0=all and may be very large.
    /// Omit to use [cli].show_max_lines from config.
    #[arg(long, allow_hyphen_values = true)]
    max_lines: Option<i64>,
    /// Print a compact session summary: purpose, tool activity, refs, changed files, and follow-ups.
    #[arg(long, conflicts_with_all = ["max_lines", "raw"])]
    summary: bool,
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
    /// Print the resume command without running it.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct ExportArgs {
    /// Session id or unambiguous id prefix to export.
    id: String,
    /// Export format: markdown, json, or text.
    #[arg(long, default_value = "markdown")]
    format: String,
    /// Write to this file instead of stdout.
    #[arg(short, long)]
    output: Option<PathBuf>,
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

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let command = match cli.command {
        Commands::Mcp(cmd) => return sessiongrep::mcp_install::run_mcp_cmd(cmd),
        command => command,
    };

    if let Commands::Config(cmd) = &command {
        match cmd {
            ConfigCmd::Path => {
                println!("{}", Config::config_path().display());
                return Ok(());
            }
            ConfigCmd::Example => {
                print!("{}", sessiongrep::config::CONFIG_EXAMPLE_TOML);
                return Ok(());
            }
            ConfigCmd::Init(args) => {
                write_config_example(args.force)?;
                return Ok(());
            }
            ConfigCmd::Show(_) => {}
        }
    }

    let config = Config::load()?;
    if let Commands::Db(cmd) = command {
        return sessiongrep::sql_query::run(
            &config.db_path(),
            config.index.busy_timeout_ms,
            &config.db,
            cmd,
        );
    }
    if let Commands::Config(cmd) = command {
        return run_config_cmd(&config, cmd);
    }
    // Size the global thread pool for data-parallel scans from config/env/host (auto by default).
    // Non-fatal: Rayon falls back to its default pool. The CLI reports to stderr (its user channel).
    if let Err(err) = sessiongrep::config::init_thread_pool(config.resolve_threads()) {
        eprintln!("sessiongrep: using default thread pool ({err})");
    }
    fs::create_dir_all(config.cache_dir())?;
    let mut db = Db::open_with_busy_timeout(&config.db_path(), config.index.busy_timeout_ms)?;
    db.apply_performance_config(&config.performance);
    // Terminal frontend: report library progress (e.g. the one-time lazy index build) to stderr.
    db.set_progress_reporter(|message| eprintln!("sessiongrep: {message}"));

    // Auto-reindex before commands that read session data. After a schema upgrade
    // (new tables/columns that incremental indexing would skip), do a one-time FULL
    // reindex to backfill, then stamp the schema version so later runs stay fast.
    if !matches!(
        command,
        Commands::Reindex(_) | Commands::Compact | Commands::Paths | Commands::Dates
    ) {
        if db.needs_backfill()? {
            eprintln!("sessiongrep: index schema changed — running a one-time full reindex to backfill...");
            indexer::ensure_schema_backfilled(&config, &db, None)?;
        } else {
            auto_reindex(&config, &db)?;
        }
    }

    match command {
        Commands::Reindex(args) => {
            let (seen, updated) = indexer::with_index_update_lock(&config, || {
                let result = reindex(&config, &db, args.full, false)?;
                if args.full {
                    db.purge_injected_messages()?;
                    db.mark_schema_current()?;
                    db.mark_auto_reindex_complete()?;
                } else if db.needs_backfill()? {
                    eprintln!(
                        "sessiongrep: schema backfill still pending; run `sessiongrep reindex --full` to stamp the current schema"
                    );
                } else {
                    db.mark_auto_reindex_complete()?;
                }
                Ok(result)
            })?;
            println!("reindex complete: scanned {seen} files, updated {updated} sessions");
        }
        Commands::List(args) => {
            let format = args.format;
            let filters = build_filters(&args, &config)?;
            let sessions = db.list_recent(&filters)?;
            match format {
                OutputFormat::Table => print_sessions(&sessions),
                other => render_rows(&sessions, other)?,
            }
        }
        Commands::Search(args) => {
            let format = args.filters.format;
            let filters = build_filters(&args.filters, &config)?;
            let current_repo = current_repo(&config);
            let hits = db.search(
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
                let inspection = inspect_session(&db, &args.id, InspectionOptions::default())?;
                render_rows(
                    &inspection_rows(&inspection, InspectionOptions::default()),
                    OutputFormat::Table,
                )?;
                return Ok(());
            }
            let session = db.resolve_session(&args.id)?;
            print_session_detail(&session.session);
            let max_lines = args.max_lines.unwrap_or(config.cli.show_max_lines);
            let (transcript, returned_lines) =
                select_transcript_lines(&session.transcript_text, max_lines);
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
            let rendered = render_command(&cmd);
            println!("resume command: {rendered}");
            if let Some(cwd) = &cwd {
                println!("cwd: {cwd}");
            }
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
            let session = db.resolve_session(&args.id)?;
            let output = export_session(&session, &args.format)?;
            if let Some(path) = args.output {
                fs::write(&path, output)?;
                println!("wrote {}", path.display());
            } else {
                print!("{output}");
            }
        }
        Commands::Messages(cmd) => sessiongrep::messages::run(&db, &cmd)?,
        Commands::Corrections(args) => {
            sessiongrep::analytics::run_corrections(&db, &config, &args)?
        }
        Commands::Planning(args) => sessiongrep::analytics::run_planning(&db, &config, &args)?,
        Commands::Stats(args) => sessiongrep::analytics::run_stats(&db, &args)?,
        Commands::Vocab(args) => sessiongrep::analytics::run_vocab(&db, &args)?,
        Commands::Repeats(args) => sessiongrep::analytics::run_repeats(&db, &args)?,
        Commands::Files(cmd) => sessiongrep::files::run(&db, &cmd)?,
        Commands::Compact => compact(&config, &db)?,
        Commands::Dates => println!("{}", sessiongrep::dates::format_reference()),
        Commands::Doctor => print_doctor(&config, &db)?,
        Commands::Paths => print_paths(&config),
        Commands::Tui => tui::run(&config, &db)?,
        Commands::Mcp(_) => unreachable!("MCP install commands return before opening the DB"),
        Commands::Db(_) => unreachable!("DB query commands return before opening the write DB"),
        Commands::Config(_) => unreachable!("Config commands return before opening the DB"),
    }

    Ok(())
}

fn run_config_cmd(config: &Config, cmd: ConfigCmd) -> Result<()> {
    match cmd {
        ConfigCmd::Path => println!("{}", Config::config_path().display()),
        ConfigCmd::Example => print!("{}", sessiongrep::config::CONFIG_EXAMPLE_TOML),
        ConfigCmd::Init(args) => write_config_example(args.force)?,
        ConfigCmd::Show(args) => match args.format {
            ConfigOutputFormat::Toml => print!("{}", toml::to_string_pretty(config)?),
            ConfigOutputFormat::Json => println!("{}", serde_json::to_string_pretty(config)?),
        },
    }
    Ok(())
}

fn write_config_example(force: bool) -> Result<()> {
    let path = Config::config_path();
    if path.exists() && !force {
        return Err(anyhow!(
            "{} already exists; use `sessiongrep config init --force` to overwrite it",
            path.display()
        ));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, sessiongrep::config::CONFIG_EXAMPLE_TOML)?;
    println!("wrote {}", path.display());
    Ok(())
}

/// `compact`: merge FTS5 index segments (`optimize`), `VACUUM`, then checkpoint/truncate the WAL.
/// This is the documented OPTIMIZE → VACUUM order (VACUUM alone does not merge FTS5 segments).
/// The final checkpoint is needed in WAL mode so the rewritten pages are not left in `index.db-wal`.
fn compact(config: &Config, db: &Db) -> Result<()> {
    let path = config.db_path();
    let size = |p: &std::path::Path| fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let before = size(&path);
    eprintln!(
        "sessiongrep: compacting index ({}) — optimize + vacuum + wal checkpoint…",
        mib(before)
    );
    db.optimize_fts()?;
    db.vacuum()?;
    db.checkpoint_truncate()?;
    let after = size(&path);
    println!(
        "compact complete: {} → {} (reclaimed {})",
        mib(before),
        mib(after),
        mib(before.saturating_sub(after))
    );
    Ok(())
}

/// Human-readable mebibytes for size reporting.
fn mib(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
}

fn reindex(config: &Config, db: &Db, full: bool, quiet: bool) -> Result<(usize, usize)> {
    if quiet {
        return match indexer::reindex_with_mode(
            config,
            db,
            full,
            None,
            indexer::ReindexMode::Strict,
        )? {
            indexer::ReindexOutcome::Updated {
                files_seen,
                sessions_updated,
            } => Ok((files_seen, sessions_updated)),
            indexer::ReindexOutcome::SkippedBusy => unreachable!("strict reindex never skips"),
        };
    }

    // Render progress to stderr when the dataset is large enough to matter.
    // We don't know the total up front without re-running discovery here, so
    // we let the callback gate on `total` and update on every change.
    let mut progress = |index: usize, total: usize, updated: usize| {
        if total >= 20 && (updated.is_multiple_of(10) || index == total) {
            eprint!("\rindexing: {index}/{total} files ({updated} updated)");
        }
    };
    let (total, updated) = match indexer::reindex_with_mode(
        config,
        db,
        full,
        Some(&mut progress),
        indexer::ReindexMode::Strict,
    )? {
        indexer::ReindexOutcome::Updated {
            files_seen,
            sessions_updated,
        } => (files_seen, sessions_updated),
        indexer::ReindexOutcome::SkippedBusy => unreachable!("strict reindex never skips"),
    };
    if total >= 20 {
        eprintln!();
    }
    Ok((total, updated))
}

fn auto_reindex(config: &Config, db: &Db) -> Result<()> {
    match indexer::auto_reindex(config, db, None)? {
        indexer::AutoReindexOutcome::Updated { .. } | indexer::AutoReindexOutcome::SkippedFresh => {
            Ok(())
        }
        indexer::AutoReindexOutcome::SkippedBusy => {
            eprintln!(
                "sessiongrep: auto-reindex skipped because another process is writing; serving existing index"
            );
            Ok(())
        }
    }
}

/// Render rows to stdout in a non-table machine format (json/jsonl/csv/plain).
fn render_rows<T: serde::Serialize + Row>(rows: &[T], format: OutputFormat) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    render(rows, format, &mut out)?;
    out.flush()?;
    Ok(())
}

fn build_filters(args: &QueryArgs, config: &Config) -> Result<SearchFilters> {
    let (since, until) = args.dates.resolve_now()?;
    Ok(SearchFilters {
        provider: args.provider,
        path_prefix: args
            .path
            .as_deref()
            .map(sessiongrep::util::normalize_path_prefix),
        exclude_path_prefixes: args
            .exclude_paths
            .iter()
            .map(|path| sessiongrep::util::normalize_path_prefix(path))
            .collect(),
        exclude_session_ids: args.exclude_sessions.clone(),
        since,
        until,
        limit: args
            .limit
            .filter(|limit| *limit > 0)
            .unwrap_or(config.search.default_limit),
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

fn print_search_hit(hit: &sessiongrep::models::SearchHit, query: &str) {
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

fn export_session(
    session: &sessiongrep::models::SessionWithTranscript,
    format: &str,
) -> Result<String> {
    match format {
        "text" => Ok(format!(
            "{}\n\n{}\n",
            session
                .session
                .title
                .clone()
                .unwrap_or_else(|| session.session.id.clone()),
            session.transcript_text
        )),
        "markdown" | "md" => Ok(format!(
            "# {}\n\n- Provider: {}\n- Session ID: {}\n- CWD: {}\n- Updated At: {}\n\n## Preview\n\n{}\n\n## Transcript\n\n```\n{}\n```\n",
            session
                .session
                .title
                .clone()
                .unwrap_or_else(|| session.session.id.clone()),
            session.session.provider,
            session.session.provider_session_id,
            session.session.cwd.as_deref().unwrap_or("-"),
            session
                .session
                .updated_at
                .map(|value: chrono::DateTime<chrono::Utc>| value.to_rfc3339())
                .unwrap_or_else(|| "-".to_string()),
            session.session.preview_text,
            session.transcript_text
        )),
        "json" => Ok(serde_json::to_string_pretty(&json!(session))?),
        other => Err(anyhow!("unsupported export format: {other}")),
    }
}

fn print_doctor(config: &Config, db: &Db) -> Result<()> {
    let claude_adapter = ClaudeAdapter::new(config.claude_paths());
    let claude_sources = claude_adapter.discover();
    let claude_desktop_adapter = ClaudeAdapter::new(config.claude_desktop_paths());
    let claude_desktop_sources = claude_desktop_adapter.discover();
    let codex_adapter = CodexAdapter::new(config.codex_paths(), config.codex_home());
    let cursor_adapter = CursorAdapter::new(config.cursor_paths());
    let antigravity_adapter = AntigravityAdapter::new(config.antigravity_paths());
    let pi_adapter = PiAdapter::new(config.pi_paths());
    let health = vec![
        ProviderHealth {
            provider: Provider::Claude,
            binary_found: which("claude").is_some(),
            roots: config
                .claude_paths()
                .into_iter()
                .map(|path| normalize_path(&path))
                .collect(),
            discovered_files: claude_sources
                .iter()
                .filter(|source| source.provider == Provider::Claude)
                .count(),
            sample_resume: "claude --resume <session-id>".to_string(),
        },
        ProviderHealth {
            provider: Provider::ClaudeDesktop,
            binary_found: false,
            roots: config
                .claude_desktop_paths()
                .into_iter()
                .map(|path| normalize_path(&path))
                .collect(),
            discovered_files: claude_desktop_sources
                .iter()
                .filter(|source| source.provider == Provider::ClaudeDesktop)
                .count(),
            sample_resume: "not supported".to_string(),
        },
        ProviderHealth {
            provider: Provider::Codex,
            binary_found: which("codex").is_some(),
            roots: config
                .codex_paths()
                .into_iter()
                .map(|path| normalize_path(&path))
                .collect(),
            discovered_files: codex_adapter.discover().len(),
            sample_resume: "codex resume <session-id>".to_string(),
        },
        ProviderHealth {
            provider: Provider::Cursor,
            binary_found: which("cursor").is_some(),
            roots: config
                .cursor_paths()
                .into_iter()
                .map(|path| normalize_path(&path))
                .collect(),
            discovered_files: cursor_adapter.discover().len(),
            sample_resume: "not supported".to_string(),
        },
        ProviderHealth {
            provider: Provider::Antigravity,
            binary_found: false,
            roots: config
                .antigravity_paths()
                .into_iter()
                .map(|path| normalize_path(&path))
                .collect(),
            discovered_files: antigravity_adapter.discover().len(),
            sample_resume: "N/A".to_string(),
        },
        ProviderHealth {
            provider: Provider::Pi,
            binary_found: which("pi").is_some(),
            roots: config
                .pi_paths()
                .into_iter()
                .map(|path| normalize_path(&path))
                .collect(),
            discovered_files: pi_adapter.discover().len(),
            sample_resume: "pi --session <session-id>".to_string(),
        },
    ];
    let counts = db.counts_by_provider()?;
    let warnings = db.count_parse_warnings()?;
    println!("DB: {}", config.db_path().display());
    print_auto_reindex_status(config, db)?;
    println!("Parse warnings indexed: {warnings}");
    for item in health {
        println!("\nProvider: {}", item.provider);
        println!(
            "  binary: {}",
            if item.binary_found {
                "present"
            } else {
                "missing"
            }
        );
        println!("  roots: {}", item.roots.join(", "));
        println!("  files discovered: {}", item.discovered_files);
        println!(
            "  sessions indexed: {}",
            counts
                .get(item.provider.as_str())
                .copied()
                .unwrap_or_default()
        );
        println!("  sample resume: {}", item.sample_resume);
    }
    Ok(())
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

fn print_paths(config: &Config) {
    println!("Config: {}", Config::config_path().display());
    println!("DB: {}", config.db_path().display());
    println!("Cache: {}", config.cache_dir().display());
    println!(
        "Claude roots: {}",
        config
            .claude_paths()
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "Claude Desktop roots: {}",
        config
            .claude_desktop_paths()
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "Codex roots: {}",
        config
            .codex_paths()
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "Cursor roots: {}",
        config
            .cursor_paths()
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "Antigravity roots: {}",
        config
            .antigravity_paths()
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "Pi roots: {}",
        config
            .pi_paths()
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("Codex metadata home: {}", config.codex_home().display());
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn config_commands_parse() {
        assert_parses(["sessiongrep", "config", "path"]);
        assert_parses(["sessiongrep", "config", "example"]);
        assert_parses(["sessiongrep", "config", "init", "--force"]);
        assert_parses(["sessiongrep", "config", "show"]);
        assert_parses(["sessiongrep", "config", "show", "--format", "json"]);
    }

    #[test]
    fn show_accepts_bounded_head_tail_and_all_transcript_modes() {
        let cli = Cli::try_parse_from(["sessiongrep", "show", "abc"]).unwrap();
        let Commands::Show(args) = cli.command else {
            panic!("expected show command");
        };
        assert_eq!(args.max_lines, None);
        assert!(!args.summary);

        assert_parses(["sessiongrep", "show", "abc", "--summary"]);
        assert_rejects(["sessiongrep", "show", "abc", "--summary", "--raw"]);
        assert_rejects([
            "sessiongrep",
            "show",
            "abc",
            "--summary",
            "--max-lines",
            "20",
        ]);
        assert_parses(["sessiongrep", "show", "abc", "--max-lines", "20"]);
        assert_parses(["sessiongrep", "show", "abc", "--max-lines", "-20"]);
        assert_parses(["sessiongrep", "show", "abc", "--max-lines", "0"]);
    }

    #[test]
    fn messages_search_accepts_leading_dash_literals() {
        let cli =
            Cli::try_parse_from(["sessiongrep", "messages", "search", "-e", "--path"]).unwrap();
        let Commands::Messages(sessiongrep::messages::MessagesCmd::Search(args)) = cli.command
        else {
            panic!("expected messages search command");
        };
        assert_eq!(args.query_arg.as_deref(), Some("--path"));

        assert_parses(["sessiongrep", "messages", "search", "--", "--path"]);
    }

    #[test]
    fn messages_search_fuzzy_is_explicit_and_exclusive() {
        assert_parses([
            "sessiongrep",
            "messages",
            "search",
            "magic values",
            "--fuzzy",
        ]);
        assert_parses([
            "sessiongrep",
            "messages",
            "search",
            "-e",
            "--path",
            "--fuzzy",
        ]);
        assert_parses([
            "sessiongrep",
            "messages",
            "search",
            "magic.*values",
            "--regex",
        ]);
        assert_parses([
            "sessiongrep",
            "messages",
            "search",
            "-e",
            "--path",
            "--regex",
        ]);
        assert_rejects([
            "sessiongrep",
            "messages",
            "search",
            "magic values",
            "--fuzzy",
            "--rank",
        ]);
        assert_rejects([
            "sessiongrep",
            "messages",
            "search",
            "magic",
            "--fuzzy",
            "values",
        ]);
        assert_parses(["sessiongrep", "messages", "search", "--fuzzy"]);
        assert_rejects([
            "sessiongrep",
            "messages",
            "search",
            "magic.*values",
            "--regex",
            "--fuzzy",
        ]);
    }

    #[test]
    fn repeats_command_parses() {
        assert_parses(["sessiongrep", "repeats", "--type", "user"]);
        assert_parses(["sessiongrep", "repeats", "magic values", "--type", "user"]);
        assert_parses(["sessiongrep", "repeats", "magic|config", "--regex"]);
        assert_parses([
            "sessiongrep",
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
        assert_rejects(["sessiongrep", "repeats", "you forgot", "--similarity"]);
        assert_rejects(["sessiongrep", "repeats", "you forgot", "--groups"]);
        assert_rejects(["sessiongrep", "repeats", "--context", "-1"]);
        assert_rejects(["sessiongrep", "similar", "--type", "user"]);
    }
}
