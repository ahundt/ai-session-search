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
use sessiongrep::models::{Provider, ProviderHealth, SearchFilters, SessionRecord};
use sessiongrep::providers::{
    antigravity::AntigravityAdapter, claude::ClaudeAdapter, codex::CodexAdapter,
    cursor::CursorAdapter, pi::PiAdapter,
};
use sessiongrep::render::{render, OutputFormat, Row};
use sessiongrep::util::{
    current_repo, highlight_matches, normalize_path, prompt_confirm, relative_age, render_command,
    resume_plan, truncate_for_display, which,
};

#[derive(Debug, Parser)]
#[command(
    name = "sessiongrep",
    version,
    about = "Search and resume Claude, Codex, Cursor, Antigravity, and Pi session history"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Rebuild the index from session files (incremental; `--full` reparses everything).
    Reindex(ReindexArgs),
    /// List recent sessions (newest first), with optional provider/path/date filters.
    List(QueryArgs),
    /// Search sessions by keyword (FTS5 candidates, fuzzy-ranked) across providers.
    Search(SearchArgs),
    /// Print one session's full transcript and metadata (by id or id prefix).
    Show(ShowArgs),
    /// Resume a session in its native CLI — prints the command, or runs it with confirmation.
    Resume(ResumeArgs),
    /// Export a full session to a file or stdout (markdown/json/text).
    Export(ExportArgs),
    /// Search and read indexed messages (search|get|timeline).
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
    /// Find near-duplicate / repeated messages (MinHash + LSH).
    Repeats(sessiongrep::analytics::RepeatsArgs),
    /// Recover edited files: search/history/cross-ref/extract.
    #[command(subcommand)]
    Files(sessiongrep::files::FilesCmd),
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
    /// Restrict to one provider (claude, codex, cursor, antigravity, or pi).
    #[arg(long)]
    provider: Option<Provider>,
    /// Restrict to sessions whose cwd or repo root starts with this path prefix.
    #[arg(long)]
    path: Option<String>,
    #[command(flatten)]
    dates: DateRange,
    /// Maximum number of rows to return.
    #[arg(long, default_value_t = 25)]
    limit: usize,
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

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load()?;
    // Size the global thread pool for data-parallel scans from config/env/host (auto by default).
    sessiongrep::config::init_thread_pool(config.resolve_threads());
    fs::create_dir_all(config.cache_dir())?;
    let mut db = Db::open(&config.db_path())?;
    db.apply_performance_config(&config.performance);
    // Terminal frontend: report library progress (e.g. the one-time lazy index build) to stderr.
    db.set_progress_reporter(|message| eprintln!("sessiongrep: {message}"));

    // Auto-reindex before commands that read session data. After a schema upgrade
    // (new tables/columns that incremental indexing would skip), do a one-time FULL
    // reindex to backfill, then stamp the schema version so later runs stay fast.
    if !matches!(
        cli.command,
        Commands::Reindex(_) | Commands::Paths | Commands::Dates
    ) {
        if db.needs_backfill()? {
            eprintln!("sessiongrep: index schema changed — running a one-time full reindex to backfill...");
            reindex(&config, &db, true, false)?;
            // The full reindex re-parses live files with the current parser, but sessions whose
            // source file was deleted are never re-visited (durable archive), so any
            // harness-injected rows they indexed under an older parser persist — purge those.
            let purged = db.purge_injected_messages()?;
            if purged > 0 {
                eprintln!(
                    "sessiongrep: purged {purged} harness-injected rows from archived sessions"
                );
            }
            db.mark_schema_current()?;
        } else {
            reindex(&config, &db, false, true)?;
        }
    }

    match cli.command {
        Commands::Reindex(args) => {
            let (seen, updated) = reindex(&config, &db, args.full, false)?;
            // A manual reindex (especially `--full`) also clears the backfill flag.
            db.mark_schema_current()?;
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
            let session = db.resolve_session(&args.id)?;
            print_session_detail(&session.session);
            if args.raw {
                println!("\n{}", session.transcript_text);
            } else {
                println!("\nTranscript\n{}\n", session.transcript_text);
            }
        }
        Commands::Resume(args) => {
            let session = db.resolve_session(&args.id)?;
            let (cmd, cwd) = resume_plan(&session.session)?;
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
        Commands::Dates => println!("{}", sessiongrep::dates::format_reference()),
        Commands::Doctor => print_doctor(&config, &db)?,
        Commands::Paths => print_paths(&config),
        Commands::Tui => tui::run(&config, &db)?,
    }

    Ok(())
}

fn reindex(config: &Config, db: &Db, full: bool, quiet: bool) -> Result<(usize, usize)> {
    if quiet {
        return indexer::reindex(config, db, full, None);
    }

    // Render progress to stderr when the dataset is large enough to matter.
    // We don't know the total up front without re-running discovery here, so
    // we let the callback gate on `total` and update on every change.
    let mut progress = |index: usize, total: usize, updated: usize| {
        if total >= 20 && (updated.is_multiple_of(10) || index == total) {
            eprint!("\rindexing: {index}/{total} files ({updated} updated)");
        }
    };
    let (total, updated) = indexer::reindex(config, db, full, Some(&mut progress))?;
    if total >= 20 {
        eprintln!();
    }
    Ok((total, updated))
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
        path_prefix: args.path.clone().map(|path| {
            if path.starts_with('~') {
                normalize_path(&sessiongrep::util::expand_tilde(&path))
            } else {
                path
            }
        }),
        since,
        until,
        limit: if args.limit == 0 {
            config.search.default_limit
        } else {
            args.limit
        },
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
            discovered_files: claude_adapter.discover().len(),
            sample_resume: "claude --resume <session-id>".to_string(),
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
