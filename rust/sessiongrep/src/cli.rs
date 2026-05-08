use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Result, anyhow};
use clap::{Args, Parser, Subcommand};
use serde_json::json;

use sessiongrep::config::Config;
use sessiongrep::db::Db;
use sessiongrep::models::{Provider, ProviderHealth, SearchFilters, SessionRecord};
use sessiongrep::providers::{claude::ClaudeAdapter, codex::CodexAdapter};
use sessiongrep::util::{
    current_repo, highlight_matches, normalize_path, parse_datetime, prompt_confirm, relative_age,
    render_command, resume_plan, truncate_for_display, which,
};
use crate::tui;

#[derive(Debug, Parser)]
#[command(
    name = "sessiongrep",
    version,
    about = "Search and resume Claude and Codex session history"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Reindex(ReindexArgs),
    List(QueryArgs),
    Search(SearchArgs),
    Show(ShowArgs),
    Resume(ResumeArgs),
    Export(ExportArgs),
    Doctor,
    Paths,
    Tui,
}

#[derive(Debug, Args)]
struct ReindexArgs {
    #[arg(long)]
    full: bool,
}

#[derive(Debug, Args, Clone)]
struct QueryArgs {
    #[arg(long)]
    provider: Option<Provider>,
    #[arg(long)]
    path: Option<String>,
    #[arg(long)]
    since: Option<String>,
    #[arg(long, default_value_t = 25)]
    limit: usize,
    #[arg(long)]
    warnings_only: bool,
}

#[derive(Debug, Args)]
struct SearchArgs {
    query: String,
    #[command(flatten)]
    filters: QueryArgs,
}

#[derive(Debug, Args)]
struct ShowArgs {
    id: String,
    #[arg(long)]
    raw: bool,
}

#[derive(Debug, Args)]
struct ResumeArgs {
    id: String,
    #[arg(long)]
    yes: bool,
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct ExportArgs {
    id: String,
    #[arg(long, default_value = "markdown")]
    format: String,
    #[arg(short, long)]
    output: Option<PathBuf>,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load()?;
    fs::create_dir_all(config.cache_dir())?;
    let db = Db::open(&config.db_path())?;

    // Auto-reindex (incremental) before commands that read session data
    if !matches!(cli.command, Commands::Reindex(_) | Commands::Paths) {
        reindex(&config, &db, false, true)?;
    }

    match cli.command {
        Commands::Reindex(args) => {
            let (seen, updated) = reindex(&config, &db, args.full, false)?;
            println!("reindex complete: scanned {seen} files, updated {updated} sessions");
        }
        Commands::List(args) => {
            let filters = build_filters(&args, &config)?;
            let sessions = db.list_recent(&filters)?;
            print_sessions(&sessions);
        }
        Commands::Search(args) => {
            let filters = build_filters(&args.filters, &config)?;
            let current_repo = current_repo(&config);
            let hits = db.search(&args.query, &filters, current_repo.as_deref())?;
            if hits.is_empty() {
                println!("no sessions matched");
            } else {
                for hit in hits {
                    print_search_hit(&hit, &args.query);
                }
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
        Commands::Doctor => print_doctor(&config, &db)?,
        Commands::Paths => print_paths(&config),
        Commands::Tui => tui::run(&config, &db)?,
    }

    Ok(())
}

fn reindex(config: &Config, db: &Db, full: bool, quiet: bool) -> Result<(usize, usize)> {
    if full {
        db.clear_all()?;
    }

    let claude = ClaudeAdapter::new(config.claude_paths());
    let codex = CodexAdapter::new(config.codex_paths(), config.codex_home());
    let mut sources = Vec::new();
    if config.providers.claude.enabled {
        sources.extend(claude.discover());
    }
    if config.providers.codex.enabled {
        sources.extend(codex.discover());
    }

    let total = sources.len();
    let mut updated = 0usize;
    for (i, source) in sources.iter().enumerate() {
        let source_path = normalize_path(&source.path);
        if !full
            && db.is_file_current(
                source.provider,
                &source_path,
                source.mtime_ns,
                source.size_bytes,
            )?
        {
            continue;
        }
        let parsed = match source.provider {
            Provider::Claude => claude.parse(source),
            Provider::Codex => codex.parse(source),
        };
        db.upsert_session(&parsed, source.mtime_ns, source.size_bytes)?;
        updated += 1;
        if !quiet && total >= 20 && (updated.is_multiple_of(10) || i + 1 == total) {
            eprint!("\rindexing: {}/{} files ({} updated)", i + 1, total, updated);
        }
    }
    if !quiet && total >= 20 {
        eprintln!();
    }

    Ok((total, updated))
}

fn build_filters(args: &QueryArgs, config: &Config) -> Result<SearchFilters> {
    Ok(SearchFilters {
        provider: args.provider,
        path_prefix: args.path.clone().map(|path| {
            if path.starts_with('~') {
                normalize_path(&sessiongrep::util::expand_tilde(&path))
            } else {
                path
            }
        }),
        since: args.since.as_deref().and_then(parse_datetime),
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


fn export_session(session: &sessiongrep::models::SessionWithTranscript, format: &str) -> Result<String> {
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
    println!("Codex metadata home: {}", config.codex_home().display());
}

