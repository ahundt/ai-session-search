// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-FileCopyrightText: 2026 Nisarg Patel
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::num::NonZeroUsize;
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
use crate::models::{
    AnalysisRequest, AnalysisSessionSelection, Provider, SearchFilters, SessionKind, SessionRecord,
};
use crate::render::{render, OutputFormat, Row};
use crate::service::SessionSearch;
use crate::tui;
use crate::util::{
    current_repo, highlight_matches, prompt_confirm, relative_age, render_posix_shell_command,
    resume_plan, select_transcript_lines, truncate_for_display,
};
use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, CommandFactory, Parser, Subcommand};
use serde::Serialize;

/// Help section the six root-level flags are grouped under.
///
/// Every one of them is `global = true`, so clap repeats them in EVERY subcommand's help, where
/// it interleaves them alphabetically with that command's own options:
/// `aise skills corrections --help` listed `--config`, `--session-id`, `--database`,
/// `--provider`, `--cache-dir`, `--path`, ...
/// A reader could not tell which flags belong to the command they are reading about. A heading
/// separates them without changing what any flag does or where it may be passed.
const GLOBAL_OPTIONS_HEADING: &str =
    "Shared options (parsed globally; applicability depends on the selected command)";

/// The command groups the root help names, in the order the commands are listed. Every visible
/// subcommand appears in exactly one group (`root_help_names_every_visible_command_once` pins
/// that), so a reader sees the six everyday commands first and can tell them from maintenance,
/// analytics, and expert tools without reading twenty-five one-line summaries.
const ROOT_COMMAND_GROUPS: &str = "\
Start here (everyday):   search, messages, show, list, resume, files
Export and maintenance:  export, reindex, doctor, integrations, package, config, dates
Analytics:               stats, repeats, vocab, planning, analyze, skills
Expert:                  db, migrate, compact, mcp, tui

Typical path: `aise search \"<topic>\" --when 30d` or `aise list --path <dir> --when 7d` to find a
session, `aise messages search \"<phrase>\" --context 2` to find the exact turn, then
`aise show <id>` or `aise messages get <id> --seq <N> --context 3` to read it.";

#[derive(Debug, Parser)]
#[command(
    name = "aise",
    version,
    about = "AI Session Search (aise): search local sessions from Claude Code, Claude Desktop local agent, Codex, Cursor, Antigravity, Pi coding agent, Prime Agent, Google AI Studio, and Gemini CLI",
    after_help = ROOT_COMMAND_GROUPS
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
    ///
    /// Use `aise doctor` to see what is already indexed or diagnose indexing failures.
    #[command(display_order = 21)]
    Reindex(ReindexArgs),
    /// Reclaim disk space: merge FTS segments, `VACUUM`, then truncate the WAL.
    ///
    /// Use `aise doctor` first to see whether the index is large enough to be worth compacting.
    #[command(display_order = 42)]
    Compact,
    /// List recent sessions, optionally intersecting their known spans with a date period.
    ///
    /// Date bounds intersect the known indexed session span from created_at through updated_at.
    /// The span can contain gaps and is not continuous runtime.
    #[command(display_order = 13)]
    List(QueryArgs),
    /// Search indexed sessions by keyword and date-span/metadata filters, ranked by relevance.
    ///
    /// Date bounds intersect the known indexed session span from created_at through updated_at.
    /// The span can contain gaps and is not continuous runtime.
    #[command(
        display_order = 10,
        after_help = "For turn-level literal, regex, or fuzzy content search, use `aise messages search QUERY` or select `--query-mode regex|fuzzy`."
    )]
    Search(SearchArgs),
    /// Print one session's transcript and metadata (bounded by default).
    ///
    /// Use `aise list` or `aise search` to find the session id, or `aise messages search` to find
    /// one turn inside it.
    #[command(display_order = 12)]
    Show(ShowArgs),
    /// Resume a session in its native CLI: print the command, or run it with confirmation.
    ///
    /// Use `aise list` or `aise search` to find the session id first.
    #[command(display_order = 14)]
    Resume(ResumeArgs),
    /// Export one full session or an explicitly selected bounded session bundle.
    #[command(display_order = 20)]
    Export(ExportArgs),
    /// Search and read individual messages: conversation turns and tool evidence (search|get|timeline|evidence).
    #[command(display_order = 11, subcommand)]
    Messages(crate::messages::MessagesCmd),
    /// Aggregate slash-command usage frequency.
    #[command(display_order = 33)]
    Planning(crate::analytics::PlanningArgs),
    /// Analyze indexed sessions with an optional validated JSON policy and publish one immutable bundle.
    #[command(display_order = 34)]
    Analyze(AnalyzeArgs),
    /// Message counts by role.
    ///
    /// Every harness notice is left out: what the harness told the agent, not what a person or a
    /// model wrote. A raw `group by role` over the messages table counts those too and reports
    /// more; `aise messages search --kind harness-notice` returns them on their own.
    #[command(display_order = 30)]
    Stats(crate::analytics::StatsArgs),
    /// How often a term appears across every indexed message, and in how many of them.
    ///
    /// `--prefix cargo` looks one term up. Without it the report is the whole vocabulary ordered
    /// by frequency, which ordinary words head. Two columns: `docs` counts messages containing
    /// the term and `count` counts occurrences, so a term repeated inside one message raises only
    /// the second. `--trigram` reads the substring index instead, whose terms are 3 characters
    /// including spaces and punctuation; it is built `detail=none` and holds no occurrence counts,
    /// so there `count` repeats `docs` rather than reporting anything further.
    ///
    /// This counts the index itself, so unlike `aise stats` it counts every indexed message,
    /// harness notices included: hook and tool wording ranks here beside what people and models
    /// wrote. It reports how often, never which messages — `aise messages search` returns those.
    #[command(display_order = 32)]
    Vocab(crate::analytics::VocabArgs),
    /// Find recurring phrases in what people wrote.
    ///
    /// User-role messages unless `--role` names another, so an assistant or tool phrase repeated
    /// across sessions is not reported by default.
    #[command(display_order = 31)]
    Repeats(crate::analytics::RepeatsArgs),
    /// Recover edited files: search/history/cross-ref/extract.
    #[command(display_order = 15, subcommand)]
    Files(crate::files::FilesCmd),
    /// Manage executable aliases, client registrations, instructions, and skills.
    #[command(display_order = 23, subcommand)]
    Integrations(IntegrationsCmd),
    /// List, inspect, validate, create, or execute Agent Skill packages.
    #[command(display_order = 35, subcommand)]
    Skills(crate::skills::SkillsCmd),
    /// Inspect, check, or update the installed aise distribution.
    #[command(display_order = 24, subcommand)]
    Package(PackageCmd),
    /// Serve MCP JSON-RPC over standard input/output.
    #[command(display_order = 43, subcommand)]
    Mcp(crate::integrations::McpCmd),
    /// Expert read-only SQL over the local AI session-history index.
    #[command(display_order = 40, subcommand)]
    Db(crate::sql_query::DbCmd),
    /// Safely migrate or verify a session index database.
    #[command(display_order = 41, subcommand)]
    Migrate(MigrationCmd),
    /// Inspect effective configuration, its file, origins, and resolved filesystem paths.
    #[command(display_order = 25, subcommand)]
    Config(ConfigCmd),
    /// Show the supported --since/--until/--when date and EDTF formats.
    ///
    /// Referenced by every `--since`, `--until`, and `--when` flag; `aise list --since` is the usual caller.
    #[command(display_order = 26)]
    Dates,
    /// Check index health, provider discovery, and resume-tool availability.
    ///
    /// Pass `--format json` for the machine-readable summary, and use `aise config paths` to see where files are read from.
    #[command(display_order = 22)]
    Doctor(DoctorArgs),
    /// Launch the interactive terminal UI for browsing and resuming sessions.
    ///
    /// Use `aise search` or `aise messages search` for the same queries without the interface.
    #[command(display_order = 44)]
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
    ///
    /// Use `aise migrate verify` afterwards to confirm the result, or `aise migrate recover`
    /// if it was interrupted.
    Database(MigrationDatabaseArgs),
    /// Preview or atomically publish a legacy aise JSON configuration as Rust TOML.
    ///
    /// Use `aise config show` to read the result, or `aise migrate verify` to confirm it.
    Config(MigrationConfigArgs),
    /// Verify source and destination against a published migration receipt.
    ///
    /// Use `aise migrate database` or `aise migrate config` to perform the move this checks.
    Verify(MigrationVerifyArgs),
    /// Safely resume or finalize a database migration from durable prepared evidence.
    /// Use `aise migrate verify` afterwards, or `aise doctor` when recovery cannot complete.
    Recover(MigrationVerifyArgs),
}

#[derive(Debug, Args)]
struct MigrationConfigArgs {
    /// Legacy aise JSON configuration to read. Required; nothing is discovered, because a
    /// migration that guessed its own input could publish the wrong file's settings.
    #[arg(long)]
    source_json: PathBuf,
    /// Path the converted TOML configuration is published to. Required.
    #[arg(long)]
    destination: PathBuf,
    /// Index database path to record in the converted configuration. Required: the legacy format
    /// does not carry one, so it cannot be inferred from the source.
    #[arg(long)]
    database_path: PathBuf,
    /// Cache directory to record in the converted configuration. Required, for the same reason as
    /// --database-path.
    #[arg(long)]
    cache_dir: PathBuf,
    /// Publish the result. Omit for a preview that reports what would be written and writes
    /// nothing.
    #[arg(long)]
    apply: bool,
    /// Overwrite an existing destination. Omit to fail rather than replace a configuration that is
    /// already there; requires --apply.
    #[arg(long, requires = "apply")]
    replace: bool,
    /// Where the replaced configuration is copied before it is overwritten. Omit to overwrite
    /// without keeping a copy; requires --replace.
    #[arg(long, requires = "replace")]
    rollback_copy: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct MigrationDatabaseArgs {
    /// Existing index database to migrate from. Required; nothing is discovered.
    #[arg(long)]
    source: PathBuf,
    /// Path the migrated database is published to. Required; must not already hold a database
    /// this migration did not stage.
    #[arg(long)]
    destination: PathBuf,
    /// Where the durable migration receipt is written. Required, because recovery after an
    /// interruption reads it to decide what already happened.
    #[arg(long)]
    receipt: PathBuf,
    #[arg(long, default_value_t = 256)]
    pages_per_step: i32,
    #[arg(long, default_value_t = 10)]
    pause_ms: u64,
}

#[derive(Debug, Args)]
struct MigrationVerifyArgs {
    /// The durable receipt the migration wrote. Required: it records what the migration already
    /// did, and both verification and recovery read it rather than inspecting the files and
    /// guessing how far the move got.
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
    ///
    /// Use `aise package check` to look for a newer release, or `aise doctor` for index health.
    Status(ReportArgs),
    /// Check GitHub for a newer release in this build's stable or prerelease channel.
    ///
    /// Use `aise package update` to install what this finds, or `aise package status` for the
    /// build in use.
    Check(ReportArgs),
    /// Check and, when newer, invoke the evidence-backed package manager after confirmation.
    ///
    /// Use `aise package check` first to see what is available, or `aise package status`
    /// afterwards to confirm the build in use.
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
    ///
    /// Use `aise integrations status` afterwards to confirm what is registered, or
    /// `aise integrations install` to redo the transaction.
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

/// Help sections for `aise list`, `aise search`, and `aise analyze`, which flatten the same session
/// filters: which sessions, when (the shared time-window section), and how the rows come back.
const SESSION_FILTER_HEADING: &str = "Filters (which sessions)";
const SESSION_OUTPUT_HEADING: &str = "Result window and output";

#[derive(Debug, Args, Clone)]
struct QueryArgs {
    #[command(flatten)]
    filters: SessionFilterArgs,
    /// Maximum number of rows to return. Omit to use `[search].default_limit`; zero means all.
    #[arg(help_heading = SESSION_OUTPUT_HEADING, long)]
    limit: Option<usize>,
    /// Output format. `table` (default) keeps the rich human layout; json/jsonl/csv/plain
    /// emit machine-readable rows.
    #[arg(help_heading = SESSION_OUTPUT_HEADING, long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,
    /// Add optional fields to structured JSON/JSONL session rows. Repeat or comma-separate
    /// values. `raw-metadata` restores the provider metadata blob, which is omitted by default
    /// because it can be large. Table, CSV, and plain output keep their established columns.
    #[arg(help_heading = SESSION_OUTPUT_HEADING, long, value_enum, value_delimiter = ',')]
    include: Vec<SessionInclude>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::ValueEnum)]
#[clap(rename_all = "kebab-case")]
enum SessionInclude {
    RawMetadata,
}

#[derive(Debug, Args, Clone)]
struct SessionFilterArgs {
    /// Restrict to one indexed session source; omit to include all nine.
    #[arg(help_heading = SESSION_FILTER_HEADING, long)]
    provider: Option<Provider>,
    /// Restrict to sessions whose cwd or repo root is this directory or a descendant of it
    /// (a component boundary: `project` matches `project/src`, never `project-other`).
    /// Omit to search every allowed root.
    #[arg(help_heading = SESSION_FILTER_HEADING, long)]
    path: Option<String>,
    /// Exclude sessions whose cwd, repo root, or transcript path is this directory or a
    /// descendant of it (component boundary). Repeat to exclude multiple noisy worktrees or
    /// transcript roots. Omit to exclude none.
    #[arg(help_heading = SESSION_FILTER_HEADING, long = "exclude-path")]
    exclude_paths: Vec<String>,
    /// Exclude one exact session id. Repeat to exclude multiple sessions. Omit to exclude none.
    #[arg(help_heading = SESSION_FILTER_HEADING, long = "exclude-session")]
    exclude_sessions: Vec<String>,
    /// Restrict to one session class; one-value alias for --session-kinds. Omit for both classes.
    #[arg(help_heading = SESSION_FILTER_HEADING, long = "session-kind", value_enum)]
    session_kind: Option<SessionKind>,
    /// Session classes to return: user for sessions you started, subagent for runs those
    /// sessions spawned. Omit for both. With --parent-session, use subagent or omit this option;
    /// user cannot match a spawned run.
    #[arg(
        help_heading = SESSION_FILTER_HEADING,
        long = "session-kinds",
        value_enum,
        num_args = 1..,
        value_delimiter = ',',
        conflicts_with = "session_kind"
    )]
    session_kinds: Vec<SessionKind>,
    /// Restrict to runs spawned by this exact session id. Omit to include root and spawned runs
    /// alike. If a session class is also supplied, it must include subagent.
    #[arg(help_heading = SESSION_FILTER_HEADING, long = "parent-session")]
    parent_session: Option<String>,
    #[command(flatten)]
    dates: DateRange,
    /// Show only sessions that produced a parse warning.
    #[arg(help_heading = SESSION_FILTER_HEADING, long)]
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
    /// Analyze only the first N eligible sessions in canonical session-ID order.
    ///
    /// This is a reproducible prefix, not the newest or a representative sample and not a message
    /// limit. Omit it to analyze every eligible session.
    #[arg(long, value_parser = parse_first_canonical_sessions)]
    first_canonical_sessions: Option<NonZeroUsize>,
    /// Destination for the new immutable bundle; a relative path resolves against the current
    /// directory. Required, and it must be a fresh path: the bundle is created here, and an
    /// existing path is refused so a prior bundle stays intact.
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
    /// Session-level keywords, phrase, code snippet, path fragment, or title text to search for
    /// and rank by. Plain text: the whole query and each whitespace-separated word match as
    /// case-insensitive substrings of the title, summary, cwd, repo, preview, and transcript,
    /// plus a fuzzy match on title and paths; sessions matching every word rank first; there
    /// are no quote or boolean operators. Required: to list sessions by their metadata alone
    /// (newest first), use `aise list` with the same filters. A query starting with `-` is
    /// parsed as a flag here; pass it after `--`, with every other flag before the `--`, e.g.
    /// `--limit 5 -- --path`.
    query: Option<String>,
    #[command(flatten)]
    filters: QueryArgs,
}

impl SearchArgs {
    /// The text to rank by, or the exact `aise list` invocation that answers a query-less request.
    ///
    /// clap's own "required arguments were not provided: <QUERY>" told a caller what was missing
    /// and not what to run instead; agents in session history answered it by retrying with `""`,
    /// which ranks every session against an empty needle. Both cases now name the command that
    /// lists sessions by filters alone, with the filters the caller already typed.
    fn ranking_query(&self) -> Result<&str> {
        match self.query.as_deref().map(str::trim) {
            Some(query) if !query.is_empty() => Ok(query),
            _ => bail!(
                "aise search ranks sessions by a query and none was given; to list sessions by \
                 their metadata alone (newest first) run `{}`",
                self.equivalent_list_command()
            ),
        }
    }

    /// `aise list` with every session filter, result window, and output option this search carried.
    fn equivalent_list_command(&self) -> String {
        self.filters.as_list_command()
    }
}

/// The token clap accepts for `value`, taken from clap's own variant table so a rendered command
/// cannot drift from the parser that has to read it back.
fn value_enum_token<T: clap::ValueEnum>(value: &T) -> String {
    value
        .to_possible_value()
        .expect("a parsed filter value is always a selectable variant")
        .get_name()
        .to_string()
}

impl QueryArgs {
    /// The `aise list` invocation that selects the same sessions and emits the same fields.
    ///
    /// `aise list` takes this exact type, so every option a caller typed is renderable and the
    /// suggestion can be an equivalent command rather than an approximation of one. The fields are
    /// destructured rather than read through `self`, so a later option added to `QueryArgs`,
    /// `SessionFilterArgs`, or `DateRange` fails to compile here instead of silently narrowing the
    /// suggested command.
    fn as_list_command(&self) -> String {
        let Self {
            filters,
            limit,
            format,
            include,
        } = self;
        let SessionFilterArgs {
            provider,
            path,
            exclude_paths,
            exclude_sessions,
            session_kind,
            session_kinds,
            parent_session,
            dates,
            warnings_only,
        } = filters;
        let DateRange { since, until, when } = dates;

        fn push(command: &mut Vec<String>, flag: &str, value: &str) {
            command.push(flag.to_string());
            command.push(
                render_posix_shell_command(&[value.to_string()])
                    .unwrap_or_else(|_| format!("{value:?}")),
            );
        }

        let mut command = vec!["aise".to_string(), "list".to_string()];
        if let Some(provider) = provider {
            push(&mut command, "--provider", provider.as_str());
        }
        if let Some(path) = path {
            push(&mut command, "--path", path);
        }
        for path in exclude_paths {
            push(&mut command, "--exclude-path", path);
        }
        for session in exclude_sessions {
            push(&mut command, "--exclude-session", session);
        }
        if let Some(kind) = session_kind {
            push(&mut command, "--session-kind", &value_enum_token(kind));
        }
        // One comma-joined token, which the `value_delimiter = ','` parser splits back into the
        // same list. Separate tokens would let `num_args = 1..` swallow a following bare value.
        if !session_kinds.is_empty() {
            let kinds = session_kinds
                .iter()
                .map(value_enum_token)
                .collect::<Vec<_>>()
                .join(",");
            push(&mut command, "--session-kinds", &kinds);
        }
        if let Some(parent) = parent_session {
            push(&mut command, "--parent-session", parent);
        }
        if let Some(when) = when {
            push(&mut command, "--when", when);
        }
        if let Some(since) = since {
            push(&mut command, "--since", since);
        }
        if let Some(until) = until {
            push(&mut command, "--until", until);
        }
        if *warnings_only {
            command.push("--warnings-only".to_string());
        }
        if let Some(limit) = limit {
            push(&mut command, "--limit", &limit.to_string());
        }
        if *format != OutputFormat::Table {
            push(&mut command, "--format", format.as_str());
        }
        if !include.is_empty() {
            let fields = include
                .iter()
                .map(value_enum_token)
                .collect::<Vec<_>>()
                .join(",");
            push(&mut command, "--include", &fields);
        }
        command.join(" ")
    }
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
    /// Write to this file instead of stdout. Omit to write to stdout, so the export can be piped.
    #[arg(short, long, conflicts_with = "output_dir")]
    output: Option<PathBuf>,
    /// Atomically publish filtered sessions as a new immutable directory. Omit to write one
    /// stream to stdout or to --output instead of a directory per session.
    #[arg(long, conflicts_with = "output")]
    output_dir: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum ConfigCmd {
    /// Print the selected config file path without reading or creating the file.
    ///
    /// Use `aise config show` for the resolved values, or `aise config paths` for every path consulted.
    File,
    /// Print the embedded commented example config.
    ///
    /// Use `aise config init` to write this to disk, or `aise config file` for the path in use.
    Example,
    /// Write the embedded commented example config to the default config path.
    ///
    /// Use `aise config example` to preview the contents first, or `aise config show` afterwards.
    Init(ConfigInitArgs),
    /// Print the effective config after defaults and config.toml are merged.
    ///
    /// Use `aise config origins` to see where each value came from, or `aise config file` for the
    /// path it was read from.
    Show(ConfigShowArgs),
    /// Print origins for config, database, cache, threads, refresh policy, and search scope.
    ///
    /// Use `aise config show` for the values themselves, or `aise config paths` for the files consulted.
    Origins,
    /// Print resolved config, state, search-scope, and session-source paths.
    ///
    /// Use `aise config file` for the active configuration path, or `aise doctor` when one is unreadable.
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
    let cli = match parse_cli_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let exit_code = error.exit_code();
            error.print()?;
            return Ok(exit_code);
        }
    };
    if let Commands::Skills(command) = &cli.command {
        if command.print_execution_help_if_requested()? {
            return Ok(0);
        }
    }
    execute(cli)?;
    Ok(0)
}

/// Preserve root-global options after an external skill selector.
///
/// Clap cannot recognize globals after an `external_subcommand`; it hands the complete tail to
/// that subcommand. Build the global-option vocabulary from Clap's own command metadata, then move
/// only those tokens ahead of parsing. This stays O(argument count) and avoids a second hard-coded
/// option list that could drift from `Cli`.
fn parse_cli_from<I, T>(args: I) -> std::result::Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let mut args = args.into_iter().map(Into::into);
    let Some(program) = args.next() else {
        return Cli::try_parse_from(std::iter::empty::<OsString>());
    };
    let globals = Cli::command()
        .get_arguments()
        .filter(|argument| argument.is_global_set())
        .filter_map(|argument| {
            argument
                .get_long()
                .map(|name| (name.to_string(), argument.get_action().takes_values()))
        })
        .collect::<HashMap<_, _>>();
    let remaining = args.collect::<Vec<_>>();
    let mut global_tokens = Vec::new();
    let mut command_tokens = Vec::with_capacity(remaining.len());
    let mut index = 0;
    while index < remaining.len() {
        let token = &remaining[index];
        let global = token.to_str().and_then(|token| {
            token
                .strip_prefix("--")
                .map(|long| long.split_once('=').map_or(long, |(name, _)| name))
                .and_then(|name| globals.get(name).map(|takes_value| (name, *takes_value)))
        });
        match global {
            Some((_name, takes_value)) => {
                let has_inline_value = token
                    .to_str()
                    .is_some_and(|token| token.starts_with("--") && token.contains('='));
                global_tokens.push(token.clone());
                if takes_value && !has_inline_value && index + 1 < remaining.len() {
                    index += 1;
                    global_tokens.push(remaining[index].clone());
                }
            }
            None => command_tokens.push(token.clone()),
        }
        index += 1;
    }

    let normalized = std::iter::once(program)
        .chain(global_tokens)
        .chain(command_tokens.iter().cloned())
        .collect::<Vec<_>>();
    Cli::try_parse_from(&normalized)
        .map_err(|error| clarify_stale_message_argument(error, &command_tokens))
}

/// Replace Clap's edit-distance guess only for removed message-search arguments whose meaning is
/// known. The stale names remain rejected: accepting aliases would silently preserve old scope
/// semantics. `command_tokens` are the arguments after the hoisted globals, so a leading
/// `--index-refresh` does not hide the subcommand; the offending flag comes from the error's own
/// `InvalidArg` context, so `--project=/x` is recognized too, and the usage line is replaced with
/// the subcommand's real usage rather than the one clap built around its guess. O(A) over the
/// argument vector with O(1) extra state.
fn clarify_stale_message_argument(
    mut error: clap::Error,
    command_tokens: &[OsString],
) -> clap::Error {
    use clap::error::{ContextKind, ContextValue, ErrorKind};

    if error.kind() != ErrorKind::UnknownArgument
        || command_tokens.first().and_then(|value| value.to_str()) != Some("messages")
        || command_tokens.get(1).and_then(|value| value.to_str()) != Some("search")
    {
        return error;
    }
    let invalid = match error.get(ContextKind::InvalidArg) {
        Some(ContextValue::String(value)) => value.clone(),
        _ => return error,
    };
    let stale = invalid
        .split_once('=')
        .map_or(invalid.as_str(), |(name, _)| name);
    // Two shapes of rename, neither reachable through clap's edit distance. A scope rename maps name
    // to name, where clap either guesses a near-miss with the wrong meaning — `--project` guesses
    // `--role`, the defect this table was built for — or, as with `--type`, guesses nothing. A mode
    // rename maps a bare flag to a flag AND a value, so no single argument name is close enough and
    // clap offers nothing at all. Removing any row below restores that build's behavior, which the
    // tests cover in both directions.
    let replacement = match stale {
        "--project" => "--workspace-path",
        "--type" => "--role",
        "--regex" => "--query-mode regex",
        "--fuzzy" => "--query-mode fuzzy",
        "--explain" => "--receipt-level summary",
        _ => return error,
    };
    error.insert(
        ContextKind::SuggestedArg,
        ContextValue::String(replacement.to_string()),
    );
    // clap adds `to pass '--type' as a value, use '-- --type'` whenever it has no suggestion of its
    // own, which is every mode rename plus `--type`. Searching for the literal text `--type` is
    // never what someone who typed a retired flag meant, so that tip competes with the answer this
    // table just supplied. Clearing it is also what makes all five renames render alike: `--project`
    // never showed the tip, because clap did have a guess there — the wrong one.
    //
    // This clears a whole context slot, and for an unknown argument clap fills that slot with one of
    // exactly two things: the pass-as-value tip above, or `'<subcommand> <flag>' exists` when the
    // flag belongs to a child command. Only the first is reachable here, because clap looks for that
    // second case among the current command's children and `messages search` has none — an invariant
    // `message_search_has_no_subcommands_so_clearing_the_suggestion_slot_drops_only_the_value_tip`
    // holds, so adding a child command fails there rather than silently deleting its suggestion.
    error.insert(ContextKind::Suggested, ContextValue::None);
    // `build()` propagates the binary-name chain, so the usage reads `aise messages search ...`
    // rather than the bare `search ...` an unbuilt subcommand renders.
    let mut root = Cli::command();
    root.build();
    if let Some(usage) = root
        .find_subcommand_mut("messages")
        .and_then(|messages| messages.find_subcommand_mut("search"))
        .map(clap::Command::render_usage)
    {
        error.insert(ContextKind::Usage, ContextValue::StyledStr(usage));
    }
    error
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
            let outcome = crate::integrations::install_with_receipt(args, &receipt)?;
            start_initial_indexing_after_integration_install(outcome, overrides, &config_path);
            return Ok(());
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
        Commands::Mcp(crate::integrations::McpCmd::SchemaBudget(args)) => {
            // Builds the catalogue and measures it; it never opens the index, so it runs on a
            // machine with no sessions and cannot be perturbed by whatever happens to be indexed.
            let resolved = Config::resolve(overrides.clone())?;
            return crate::mcp_schema_budget::run(&args, &resolved.config);
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
    if matches!(&command, Commands::Skills(cmd) if cmd.is_management()) {
        // Config, never the index: `skills list` answers "which rules would run", which no
        // session data can change. Opening the database here would also trigger a refresh.
        //
        // The receipt path is the same one `integrations install` uses, so the writing verbs share
        // its recovery record and its manifest location rather than inventing a second pair.
        let receipt = crate::integrations::default_transaction_receipt(&resolved.config_path);
        let Commands::Skills(cmd) = command else {
            unreachable!("the match guard above established a skill management command")
        };
        return crate::skills::run(&config, cmd, &receipt);
    }
    if matches!(command, Commands::Dates) {
        println!("{}", crate::dates::format_reference());
        return Ok(());
    }
    if let Commands::Messages(cmd) = &command {
        if crate::messages::run_index_independent(cmd, &config)? {
            return Ok(());
        }
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
            for warning in &outcome.discovery_warnings {
                eprintln!(
                    "aise: discovery warning: {} {} {}: {} {}",
                    warning.provider,
                    warning.operation,
                    warning.path,
                    warning.message,
                    warning.guidance
                );
            }
            if outcome.effective_full {
                let allocation = db.storage_allocation()?;
                if let Some(guidance) = storage_compaction_guidance(allocation) {
                    eprintln!("aise: {guidance}");
                }
            }
        }
        Commands::List(args) => {
            let format = args.format;
            let include = args.include;
            let filters =
                build_filters(&args.filters, configured_search_limit(args.limit, &config))?;
            let sessions = app.catalog().list_sessions(&filters)?;
            match format {
                OutputFormat::Table => print_sessions(&sessions),
                other => render_session_rows(&sessions, other, &include)?,
            }
        }
        Commands::Search(args) => {
            let query = args.ranking_query()?;
            let format = args.filters.format;
            let include = args.filters.include.clone();
            let filters = build_filters(
                &args.filters.filters,
                configured_search_limit(args.filters.limit, &config),
            )?;
            let current_repo = current_repo(&config);
            let hits = app.catalog().search_sessions(
                query,
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
                            print_search_hit(&hit, query);
                        }
                    }
                }
                other => render_session_rows(&hits, other, &include)?,
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
            println!("{}", crate::util::RESUME_COMMAND_POLICY_NOTE);
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
        Commands::Planning(args) => crate::analytics::run_planning(db, &config, &args)?,
        Commands::Analyze(args) => {
            let filters = build_filters(&args.filters, 0)?;
            let selection = args
                .first_canonical_sessions
                .map_or(AnalysisSessionSelection::AllEligible, |max_sessions| {
                    AnalysisSessionSelection::FirstCanonicalSessions { max_sessions }
                });
            let request = AnalysisRequest::new(filters, selection)?;
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
            let analysis = app.analysis().run(&request, &policy)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&plan.publish(&analysis)?)?
            );
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
        Commands::Skills(cmd) => {
            let execution = cmd.into_execution()?;
            let args = execution.args;
            let definition = args
                .definition_json
                .as_deref()
                .map(serde_json::from_str::<crate::skill_run::MessageClassificationDefinition>)
                .transpose()
                .context(
                    "--definition-json must be a JSON object with a nonempty categories array; \
                     each category requires name and patterns",
                )?;
            let additional_skills = args
                .additional_skills
                .iter()
                .cloned()
                .map(crate::skills::parse_skill_selector)
                .collect::<Result<Vec<_>>>()?;
            let filters = crate::analytics::message_classification_filters(db, &config, &args)?;
            let report = app.analysis().run_skill(&crate::skill_run::SkillRunQuery {
                skill: execution.selector,
                definition,
                input: crate::skill_run::SkillCapabilityInput::MessageClassification(
                    crate::skill_run::MessageClassificationQuery {
                        filters,
                        additional_skills,
                    },
                ),
            })?;
            crate::analytics::render_skill_run_report(&report, &config, &args)?;
        }
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
    let defaults = Config::default();
    let db_path =
        toml::Value::String(defaults.db_path().to_string_lossy().into_owned()).to_string();
    let cache_dir =
        toml::Value::String(defaults.cache_dir().to_string_lossy().into_owned()).to_string();
    let initialized = crate::config::CONFIG_EXAMPLE_TOML
        .replace(
            "# db_path = \"/absolute/path/to/index.db\"",
            &format!("db_path = {db_path}"),
        )
        .replace(
            "# cache_dir = \"/absolute/path/to/cache\"",
            &format!("cache_dir = {cache_dir}"),
        );
    atomic_write_file(path, initialized.as_bytes(), mode).with_context(
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
    if let Err(error) = spawn_background_refresh(
        config,
        crate::background_refresh::BackgroundRefreshOrigin::Cli,
    ) {
        eprintln!("aise: background index refresh could not start: {error:#}");
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BackgroundRefreshRequest {
    config: Config,
    origin: crate::background_refresh::BackgroundRefreshOrigin,
}

fn spawn_background_refresh(
    config: &Config,
    origin: crate::background_refresh::BackgroundRefreshOrigin,
) -> Result<()> {
    let executable = crate::update::background_child_executable()?;
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
    serde_json::to_writer(
        &mut stdin,
        &BackgroundRefreshRequest {
            config: config.clone(),
            origin,
        },
    )
    .context("failed to send resolved configuration to background refresh process")?;
    stdin
        .flush()
        .context("failed to flush background refresh configuration")?;
    Ok(())
}

fn start_initial_indexing_after_integration_install(
    outcome: crate::integrations::IntegrationInstallOutcome,
    overrides: ConfigOverrides,
    config_path: &std::path::Path,
) {
    if !outcome.should_start_initial_indexing() {
        return;
    }
    let resolved = match Config::resolve(overrides) {
        Ok(resolved) => resolved,
        Err(error) => {
            eprintln!(
                "aise: integration installation succeeded, but session index preparation could not start because configuration {} could not be resolved: {error:#}. \
                 The installed integration files were preserved. Fix the configuration and run `aise reindex`; verify readiness with `aise doctor`.",
                config_path.display()
            );
            return;
        }
    };
    if let Err(error) = spawn_background_refresh(
        &resolved.config,
        crate::background_refresh::BackgroundRefreshOrigin::IntegrationInstall,
    ) {
        eprintln!(
            "aise: integration installation succeeded, but session index preparation could not start because the background refresh process failed to launch: {error:#}. \
             The installed integration files were preserved. Run `aise reindex`; verify readiness with `aise doctor`."
        );
        return;
    }
    println!(
        "started session index preparation in the background; run `aise doctor` to check readiness and freshness"
    );
}

fn run_background_refresh_from_stdin() -> Result<()> {
    let request: BackgroundRefreshRequest = serde_json::from_reader(io::stdin().lock())
        .context("failed to read resolved background refresh configuration from stdin")?;
    crate::background_refresh::run(&request.config, request.origin, &|| false)?;
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

struct SessionRecordOutput<'a> {
    session: &'a SessionRecord,
    include_raw_metadata: bool,
}

impl<'a> SessionRecordOutput<'a> {
    fn new(session: &'a SessionRecord, include_raw_metadata: bool) -> Self {
        Self {
            session,
            include_raw_metadata,
        }
    }
}

impl serde::Serialize for SessionRecordOutput<'_> {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        let session = self.session;
        let mut row = serializer.serialize_struct(
            "SessionRecord",
            if self.include_raw_metadata { 19 } else { 18 },
        )?;
        row.serialize_field("id", &session.id)?;
        row.serialize_field("provider", &session.provider)?;
        row.serialize_field("provider_session_id", &session.provider_session_id)?;
        row.serialize_field("title", &session.title)?;
        row.serialize_field("summary", &session.summary)?;
        row.serialize_field("cwd", &session.cwd)?;
        row.serialize_field("repo_root", &session.repo_root)?;
        row.serialize_field("created_at", &session.created_at)?;
        row.serialize_field("updated_at", &session.updated_at)?;
        row.serialize_field("last_message_at", &session.last_message_at)?;
        row.serialize_field("preview_text", &session.preview_text)?;
        row.serialize_field("source_path", &session.source_path)?;
        row.serialize_field("message_count", &session.message_count)?;
        row.serialize_field("parse_version", &session.parse_version)?;
        if self.include_raw_metadata {
            row.serialize_field("raw_metadata_json", &session.raw_metadata_json)?;
        }
        row.serialize_field("parse_warning", &session.parse_warning)?;
        row.serialize_field("discovery_source", &session.discovery_source)?;
        row.serialize_field("parent_session_id", &session.parent_session_id)?;
        row.serialize_field("agent_label", &session.agent_label)?;
        row.end()
    }
}

#[derive(Serialize)]
struct SearchHitOutput<'a> {
    #[serde(flatten)]
    session: SessionRecordOutput<'a>,
    score: i64,
    match_source: &'a str,
    match_snippet: &'a str,
}

trait SessionMachineOutput: serde::Serialize + Row {
    type Output<'a>: serde::Serialize
    where
        Self: 'a;

    fn machine_output(&self, include_raw_metadata: bool) -> Self::Output<'_>;
}

impl SessionMachineOutput for SessionRecord {
    type Output<'a> = SessionRecordOutput<'a>;

    fn machine_output(&self, include_raw_metadata: bool) -> Self::Output<'_> {
        SessionRecordOutput::new(self, include_raw_metadata)
    }
}

impl SessionMachineOutput for crate::models::SearchHit {
    type Output<'a> = SearchHitOutput<'a>;

    fn machine_output(&self, include_raw_metadata: bool) -> Self::Output<'_> {
        SearchHitOutput {
            session: SessionRecordOutput::new(&self.session, include_raw_metadata),
            score: self.score,
            match_source: &self.match_source,
            match_snippet: &self.match_snippet,
        }
    }
}

fn render_session_rows<T: SessionMachineOutput>(
    rows: &[T],
    format: OutputFormat,
    include: &[SessionInclude],
) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    render_session_rows_to(rows, format, include, &mut out)?;
    out.flush()?;
    Ok(())
}

fn render_session_rows_to<T: SessionMachineOutput, W: Write>(
    rows: &[T],
    format: OutputFormat,
    include: &[SessionInclude],
    out: &mut W,
) -> Result<()> {
    let include_raw_metadata = include.contains(&SessionInclude::RawMetadata);
    match format {
        OutputFormat::Json => {
            let projected = rows
                .iter()
                .map(|row| row.machine_output(include_raw_metadata))
                .collect::<Vec<_>>();
            writeln!(out, "{}", serde_json::to_string_pretty(&projected)?)?;
        }
        OutputFormat::Jsonl => {
            for row in rows {
                writeln!(
                    out,
                    "{}",
                    serde_json::to_string(&row.machine_output(include_raw_metadata))?
                )?;
            }
        }
        OutputFormat::Table | OutputFormat::Csv | OutputFormat::Plain => {
            render(rows, format, out)?;
        }
    }
    Ok(())
}

fn configured_search_limit(limit: Option<usize>, config: &Config) -> usize {
    limit.unwrap_or(config.search.default_limit)
}

fn parse_first_canonical_sessions(value: &str) -> std::result::Result<NonZeroUsize, String> {
    value
        .parse::<usize>()
        .ok()
        .and_then(NonZeroUsize::new)
        .ok_or_else(|| {
            format!(
                "first-canonical-sessions must be a positive integer; omit it to analyze every eligible session; got {value:?}"
            )
        })
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
    let filters = SearchFilters {
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
    };
    filters.validate()?;
    Ok(filters)
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
    println!(
        "Index snapshot: {}{}",
        status.readiness.snapshot.availability.as_str(),
        status
            .readiness
            .snapshot
            .last_successful_refresh_at
            .map(|value| format!("; last successful refresh {}", value.to_rfc3339()))
            .unwrap_or_default()
    );
    let refresh = &status.readiness.refresh;
    println!("Index refresh: {}", refresh.state.as_str());
    if let (Some(processed), Some(discovered)) = (refresh.files_processed, refresh.files_discovered)
    {
        println!(
            "Index refresh progress: {processed}/{discovered} files; {} session(s) updated",
            refresh.sessions_updated.unwrap_or_default()
        );
    }
    if let Some(message) = &refresh.message {
        println!("Index refresh detail: {message}");
    }
    if let Some(command) = &refresh.next_command {
        println!("Index refresh next command: {command}");
    }
    println!("Parse warnings indexed: {warnings}");
    for warning in &diagnostics.discovery_warnings {
        println!(
            "Discovery warning: {} {} {}: {} {}",
            warning.provider, warning.operation, warning.path, warning.message, warning.guidance
        );
    }
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
    codex_metadata_homes: Vec<PathBuf>,
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
        codex_metadata_homes: crate::source::provider_roots(config, Provider::Codex)
            .iter()
            .map(|root| crate::providers::codex::codex_home_from_session_root(root))
            .collect(),
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
    for line in codex_metadata_home_lines(&report) {
        writeln!(out, "{line}")?;
    }
    Ok(())
}

fn codex_metadata_home_lines(report: &ConfigPathsReport) -> Vec<String> {
    report
        .codex_metadata_homes
        .iter()
        .map(|home| format!("Codex metadata home: {}", home.display()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn config_paths_reports_every_root_derived_codex_metadata_home() {
        let temp = tempfile::tempdir().unwrap();
        let home_a = temp.path().join("home-a");
        let home_b = temp.path().join("home-b");
        std::fs::create_dir_all(home_a.join("sessions")).unwrap();
        std::fs::create_dir_all(home_b.join("sessions")).unwrap();
        let home_a = std::fs::canonicalize(home_a).unwrap();
        let home_b = std::fs::canonicalize(home_b).unwrap();
        let mut config = Config::default();
        config.providers.codex.paths = vec![
            home_a.join("sessions").display().to_string(),
            home_b.join("sessions").display().to_string(),
        ];

        let report = config_paths_report(&config, &temp.path().join("config.toml")).unwrap();

        assert_eq!(
            report.codex_metadata_homes,
            vec![home_a.clone(), home_b.clone()]
        );
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(
            json["codex_metadata_homes"],
            serde_json::json!([home_a, home_b])
        );
        assert!(json.get("codex_metadata_home").is_none());
        assert_eq!(codex_metadata_home_lines(&report).len(), 2);
    }

    fn session_with_raw_metadata() -> SessionRecord {
        SessionRecord {
            id: "codex:test-session".into(),
            provider: Provider::Codex,
            provider_session_id: "test-session".into(),
            title: Some("metadata fixture".into()),
            summary: None,
            cwd: Some("/tmp/project".into()),
            repo_root: Some("/tmp/project".into()),
            created_at: None,
            updated_at: None,
            last_message_at: None,
            preview_text: "fixture preview".into(),
            source_path: "/tmp/session.jsonl".into(),
            message_count: Some(1),
            parse_version: "test-v1".into(),
            raw_metadata_json: Some(r#"{"large":"provider payload"}"#.into()),
            parse_warning: None,
            discovery_source: "test".into(),
            parent_session_id: None,
            agent_label: None,
        }
    }

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

    /// Every `aise …` example line inside a shell code block, with the sample values a reader
    /// would substitute. A line ends at the first unquoted `|`, `;`, `&&`, or redirection, so an
    /// example that pipes into another program still contributes its `aise` part.
    fn documented_aise_invocations(document: &str) -> Vec<(usize, Vec<String>)> {
        let mut invocations = Vec::new();
        let mut in_shell_block = false;
        for (index, raw) in document.lines().enumerate() {
            let line = raw.trim();
            if let Some(fence) = line.strip_prefix("```") {
                in_shell_block = !in_shell_block
                    && matches!(fence.trim(), "sh" | "bash" | "shell" | "zsh" | "console");
                continue;
            }
            if !in_shell_block || line.starts_with('#') {
                continue;
            }
            let Some(command) = line.strip_prefix("$ ").or(Some(line)) else {
                continue;
            };
            let Some(rest) = command.strip_prefix("aise ") else {
                continue;
            };
            let words = shlex::split(&format!("aise {rest}"))
                .unwrap_or_else(|| panic!("line {}: unbalanced quoting: {raw}", index + 1));
            let mut invocation = Vec::new();
            for word in words {
                if matches!(word.as_str(), "|" | ";" | "&&" | "||" | ">" | ">>" | "2>&1") {
                    break;
                }
                invocation.push(match word.as_str() {
                    "SESSION_ID" => "claude:00000000-0000-4000-8000-000000000000".to_owned(),
                    other => other.replace("SESSION_ID", "00000000-0000-4000-8000-000000000000"),
                });
            }
            invocations.push((index + 1, invocation));
        }
        invocations
    }

    /// `aise search` without a query used to end in clap's bare "required arguments were not
    /// provided: <QUERY>", and session history shows agents retrying with `""`, which ranks every
    /// session against an empty needle. Both now name the `aise list` invocation that lists
    /// sessions by the filters the caller already typed.
    #[test]
    fn session_search_without_a_query_names_the_equivalent_list_command() {
        let cli = Cli::try_parse_from([
            "aise",
            "search",
            "--path",
            "/work/repo",
            "--when",
            "7d",
            "--limit",
            "10",
        ])
        .expect("a query-less search parses so the guidance can be rendered");
        let Commands::Search(args) = cli.command else {
            panic!("expected search command")
        };
        let error = args.ranking_query().unwrap_err().to_string();
        assert!(
            error.contains("aise list --path /work/repo --when 7d --limit 10"),
            "{error}"
        );

        let cli = Cli::try_parse_from(["aise", "search", "   ", "--provider", "codex"]).unwrap();
        let Commands::Search(args) = cli.command else {
            panic!("expected search command")
        };
        let error = args.ranking_query().unwrap_err().to_string();
        assert!(error.contains("aise list --provider codex"), "{error}");

        let cli = Cli::try_parse_from(["aise", "search", "receipt corpus"]).unwrap();
        let Commands::Search(args) = cli.command else {
            panic!("expected search command")
        };
        assert_eq!(args.ranking_query().unwrap(), "receipt corpus");
    }

    /// The suggested `aise list` has to select the same sessions and emit the same fields as the
    /// search that produced it, or the error teaches a retry that answers a different question.
    ///
    /// `aise list` takes the very `QueryArgs` `aise search` flattens, so every option the caller
    /// typed is renderable. Parsing the suggestion back and comparing it structurally to the
    /// request proves that without pinning flag spelling or order, and it fails for any option a
    /// later revision adds to `QueryArgs` and forgets to render.
    #[test]
    fn the_suggested_list_command_round_trips_every_session_filter_and_output_option() {
        let typed = [
            "aise",
            "search",
            "--provider",
            "claude",
            "--path",
            "/work/repo",
            "--exclude-path",
            "/work/repo/vendor",
            "--exclude-path",
            "/work/repo/target",
            "--exclude-session",
            "claude:79accec8",
            "--exclude-session",
            "codex:1f2e3d4c",
            "--session-kinds",
            "user,subagent",
            "--parent-session",
            "claude:aabbccdd",
            "--warnings-only",
            "--since",
            "2026-01-15",
            "--until",
            "2026-02",
            "--limit",
            "25",
            "--format",
            "json",
            "--include",
            "raw-metadata",
        ];
        let cli = Cli::try_parse_from(typed).expect("a query-less search parses");
        let Commands::Search(args) = cli.command else {
            panic!("expected search command")
        };

        let suggested = args.equivalent_list_command();
        let parts = shlex::split(&suggested)
            .unwrap_or_else(|| panic!("the suggested command is one POSIX line: {suggested}"));
        let parsed = Cli::try_parse_from(&parts)
            .unwrap_or_else(|error| panic!("the suggested command parses: {suggested}\n{error}"));
        let Commands::List(listed) = parsed.command else {
            panic!("the suggestion runs `aise list`: {suggested}")
        };

        assert_eq!(
            format!("{listed:?}"),
            format!("{:?}", args.filters),
            "`{suggested}` selects a different session set or emits different fields than the \
             search that suggested it"
        );

        // `--session-kind` is the one-value alias for `--session-kinds` and conflicts with it, so
        // it needs its own request rather than another option on the one above.
        let cli = Cli::try_parse_from(["aise", "search", "--session-kind", "subagent"]).unwrap();
        let Commands::Search(args) = cli.command else {
            panic!("expected search command")
        };
        let suggested = args.equivalent_list_command();
        let parts = shlex::split(&suggested).expect("one POSIX line");
        let Commands::List(listed) = Cli::try_parse_from(&parts)
            .unwrap_or_else(|error| panic!("{suggested}\n{error}"))
            .command
        else {
            panic!("the suggestion runs `aise list`: {suggested}")
        };
        assert_eq!(
            format!("{listed:?}"),
            format!("{:?}", args.filters),
            "{suggested}"
        );
    }

    /// The root help lists twenty-four commands. `ROOT_COMMAND_GROUPS` sorts them into everyday,
    /// maintenance, analytics, and expert groups and `display_order` lists them in that order, so
    /// a new command must be classified before it ships: every visible subcommand appears in the
    /// grouping text exactly once, no group names a command that does not exist, and the rendered
    /// help lists the everyday group first.
    #[test]
    fn root_help_names_every_visible_command_once_and_lists_everyday_commands_first() {
        let command = Cli::command();
        let visible: Vec<&str> = command
            .get_subcommands()
            .filter(|sub| !sub.is_hide_set())
            .map(|sub| sub.get_name())
            .collect();
        let grouped: Vec<&str> = ROOT_COMMAND_GROUPS
            .lines()
            .take_while(|line| !line.is_empty())
            .filter_map(|line| line.split_once(':').map(|(_, names)| names))
            .flat_map(|names| names.split(','))
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect();
        for name in &visible {
            assert_eq!(
                grouped.iter().filter(|g| g == &name).count(),
                1,
                "visible command `{name}` must appear exactly once in ROOT_COMMAND_GROUPS"
            );
        }
        for name in &grouped {
            assert!(
                visible.contains(name),
                "ROOT_COMMAND_GROUPS names `{name}`, which is not a visible command"
            );
        }
        let rendered = Cli::command().render_help().to_string();
        let commands_section = rendered
            .split("Commands:")
            .nth(1)
            .and_then(|rest| rest.split("Options:").next())
            .expect("root help has a Commands section");
        let listed: Vec<&str> = commands_section
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .collect();
        let everyday = ["search", "messages", "show", "list", "resume", "files"];
        assert_eq!(
            &listed[..everyday.len()],
            &everyday,
            "the everyday commands lead the root help: {listed:?}"
        );
        assert!(
            rendered.contains("Start here (everyday):"),
            "the group legend is printed after the options"
        );
    }

    /// `aise messages search` has thirty-nine options; every one is filed under a section so the
    /// six a caller needs first are not buried in one alphabetical block. A new option must pick
    /// a section: only `--help` and the positional query stay in the unnamed default section.
    /// Both path scopes match the exact directory and its component-boundary descendants
    /// (`db::path_prefix_patterns`: exact, or `prefix/`-anchored `LIKE`), so `project` never
    /// admits `project-other`; the help must say the same thing on both commands.
    #[test]
    fn path_scope_help_states_component_boundary_matching_on_both_commands() {
        let root = Cli::command();
        let mut search = root.find_subcommand("search").expect("search").clone();
        let session_help = search.render_long_help().to_string();
        let mut messages_search = root
            .find_subcommand("messages")
            .expect("messages")
            .find_subcommand("search")
            .expect("messages search")
            .clone();
        let message_help = messages_search.render_long_help().to_string();
        for help in [&session_help, &message_help] {
            assert!(
                help.contains("component boundary"),
                "path scope help must state component-boundary matching: {help}"
            );
            assert!(
                !help.contains("sibling sharing the leading path"),
                "a lexical sibling never matches: {help}"
            );
        }
        assert!(
            message_help.contains("last N matching messages of that session"),
            "--match-window latest selects which messages, not which occurrence: {message_help}"
        );
    }

    #[test]
    fn messages_search_help_files_every_option_under_a_named_section() {
        let root = Cli::command();
        let messages = root
            .find_subcommand("messages")
            .expect("messages subcommand");
        let search = messages
            .find_subcommand("search")
            .expect("messages search subcommand");
        let unsectioned: Vec<String> = search
            .get_arguments()
            .filter(|arg| arg.get_help_heading().is_none())
            .filter(|arg| !arg.is_positional() && arg.get_id() != "help")
            .map(|arg| arg.get_id().to_string())
            .collect();
        assert!(
            unsectioned.is_empty(),
            "every messages-search option needs a help_heading; missing: {unsectioned:?}"
        );
        let rendered = search.clone().render_long_help().to_string();
        let sections = [
            "Query (what to find):",
            "Filters (which messages):",
            "Time window (formats: `aise dates`):",
            "Result window and context (how many, from where):",
            "Presentation and output (how matches are shown; matching stays the same):",
            "Advanced (purpose bundles, receipts, self-description):",
        ];
        let mut last = 0;
        for section in sections {
            let position = rendered.find(section).unwrap_or_else(|| {
                panic!("section `{section}` missing from messages search --help")
            });
            assert!(
                position > last,
                "sections must render in the documented order"
            );
            last = position;
        }
    }

    /// The shipped skill and README are what an agent reads instead of `--help`, so an example
    /// that names a flag this build no longer accepts teaches the wrong command with authority.
    /// That happened: `messages search --regex/--fuzzy/--explain` were renamed to `--query-mode`
    /// and `--receipt-level` on 2026-07-22 and the skill kept the old spellings through rc1;
    /// session history shows agents on Pi and Claude Code paying for it with `--help` probes and
    /// failed calls. Every documented invocation must parse against the current CLI, including
    /// the capability arguments after `skills <name>`, which the root parser passes through.
    #[test]
    fn every_documented_aise_example_parses_against_the_current_cli() {
        let documents = [
            (
                "skills/ai-session-search/SKILL.md",
                include_str!("../skills/ai-session-search/SKILL.md"),
            ),
            ("README.md", include_str!("../../../README.md")),
        ];
        let mut failures = Vec::new();
        let mut checked = 0usize;
        for (name, document) in documents {
            for (line, invocation) in documented_aise_invocations(document) {
                checked += 1;
                let rendered = invocation.join(" ");
                let cli = match Cli::try_parse_from(&invocation) {
                    Ok(cli) => cli,
                    // `--help` and `--version` are successful parses that clap reports as errors.
                    Err(error)
                        if matches!(
                            error.kind(),
                            clap::error::ErrorKind::DisplayHelp
                                | clap::error::ErrorKind::DisplayVersion
                        ) =>
                    {
                        continue;
                    }
                    Err(error) => {
                        let first_line = error.to_string();
                        let first_line = first_line.lines().next().unwrap_or_default().to_owned();
                        failures.push(format!("{name}:{line}: {rendered}\n    {first_line}"));
                        continue;
                    }
                };
                if let Commands::Skills(command) = cli.command {
                    if !command.is_management() {
                        if let Err(error) = command.into_execution() {
                            failures.push(format!("{name}:{line}: {rendered}\n    {error}"));
                        }
                    }
                }
            }
        }
        assert!(
            checked >= 40,
            "expected the documents to carry examples; found {checked}"
        );
        assert!(
            failures.is_empty(),
            "{} documented aise invocation(s) do not parse against this build:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    /// Read-only deterministic capabilities live under the skill that defines them.
    ///
    /// The inferred form is the normal human path. `run` is only the collision escape for a
    /// skill whose name is also a management verb; it must not become a second execution model.
    #[test]
    fn skills_accept_inferred_and_collision_escape_execution() {
        assert_parses(["aise", "skills", "corrections", "--when", "7d"]);
        assert_parses(["aise", "skills", "./my-review/SKILL.md", "--when", "7d"]);
        assert_parses(["aise", "skills", "run", "list", "--when", "7d"]);
    }

    #[test]
    fn skill_execution_parses_ordered_additional_name_and_path_selectors() {
        let cli = Cli::try_parse_from([
            "aise",
            "skills",
            "corrections",
            "--skill",
            "my-review",
            "--skill",
            "./other-review/SKILL.md",
        ])
        .unwrap();
        let Commands::Skills(command) = cli.command else {
            panic!("expected skills command")
        };
        let execution = command.into_execution().unwrap();
        assert_eq!(
            execution.args.additional_skills,
            vec![
                OsString::from("my-review"),
                OsString::from("./other-review/SKILL.md")
            ]
        );
    }

    #[test]
    fn global_options_after_an_inferred_skill_remain_global() {
        let cli = parse_cli_from([
            "aise",
            "skills",
            "corrections",
            "--when",
            "7d",
            "--config",
            "/tmp/aise-config.toml",
        ])
        .unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("/tmp/aise-config.toml")));
        let Commands::Skills(command) = cli.command else {
            panic!("expected skills command")
        };
        let execution = command.into_execution().unwrap();
        assert_eq!(execution.args.dates.when.as_deref(), Some("7d"));
    }

    /// This project has not released 1.0 yet, so keeping the correction-specific root command
    /// would create two public spellings before either one has compatibility value.
    #[test]
    fn correction_specific_root_command_is_not_part_of_the_release_cli() {
        assert_rejects(["aise", "corrections", "--when", "7d"]);
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

        let cli = Cli::try_parse_from([
            "aise",
            "list",
            "--session-kinds",
            "user",
            "--parent-session",
            "claude:abc",
        ])
        .expect("arguments parse before shared semantic validation");
        let Commands::List(args) = cli.command else {
            panic!("expected list")
        };
        let error = build_filters(&args.filters, 10)
            .expect_err("a parent cannot have a user-started child session")
            .to_string();
        assert!(error.contains("session_kinds"), "{error}");
        assert!(error.contains("parent_session_id"), "{error}");
        assert!(error.contains("subagent"), "{error}");
    }

    #[test]
    fn session_machine_output_omits_raw_metadata_unless_explicitly_included() {
        let sessions = [session_with_raw_metadata()];

        for format in [OutputFormat::Json, OutputFormat::Jsonl] {
            let mut default_output = Vec::new();
            render_session_rows_to(&sessions, format, &[], &mut default_output).unwrap();
            let default_output = String::from_utf8(default_output).unwrap();
            assert!(
                !default_output.contains("raw_metadata_json"),
                "{format:?} must omit the field rather than serialize null: {default_output}"
            );
            assert!(
                !default_output.contains("provider payload"),
                "{format:?} leaked the provider metadata payload: {default_output}"
            );

            let mut included_output = Vec::new();
            render_session_rows_to(
                &sessions,
                format,
                &[SessionInclude::RawMetadata],
                &mut included_output,
            )
            .unwrap();
            let included_output = String::from_utf8(included_output).unwrap();
            assert!(included_output.contains("raw_metadata_json"));
            assert!(included_output.contains("provider payload"));
        }
    }

    #[test]
    fn ranked_search_machine_output_applies_the_same_metadata_policy() {
        let hits = [crate::models::SearchHit {
            session: session_with_raw_metadata(),
            score: 42,
            match_source: "preview".into(),
            match_snippet: "matched needle".into(),
        }];

        let mut default_output = Vec::new();
        render_session_rows_to(&hits, OutputFormat::Json, &[], &mut default_output).unwrap();
        let default_value: serde_json::Value = serde_json::from_slice(&default_output).unwrap();
        let hit = &default_value.as_array().unwrap()[0];
        assert!(hit.get("raw_metadata_json").is_none());
        assert_eq!(hit["score"], 42);
        assert_eq!(hit["match_source"], "preview");
        assert_eq!(hit["match_snippet"], "matched needle");

        let mut included_output = Vec::new();
        render_session_rows_to(
            &hits,
            OutputFormat::Jsonl,
            &[SessionInclude::RawMetadata],
            &mut included_output,
        )
        .unwrap();
        let included_value: serde_json::Value = serde_json::from_slice(&included_output).unwrap();
        assert_eq!(
            included_value["raw_metadata_json"],
            r#"{"large":"provider payload"}"#
        );
        assert_eq!(included_value["score"], 42);
    }

    #[test]
    fn session_include_does_not_change_established_tabular_rows() {
        let sessions = [session_with_raw_metadata()];
        for format in [OutputFormat::Table, OutputFormat::Csv, OutputFormat::Plain] {
            let mut baseline = Vec::new();
            render(&sessions, format, &mut baseline).unwrap();

            let mut with_include = Vec::new();
            render_session_rows_to(
                &sessions,
                format,
                &[SessionInclude::RawMetadata],
                &mut with_include,
            )
            .unwrap();
            assert_eq!(with_include, baseline, "{format:?} output changed");
        }
    }

    #[test]
    fn list_and_search_share_the_explicit_raw_metadata_include_control() {
        for args in [
            vec![
                "aise",
                "list",
                "--format",
                "json",
                "--include",
                "raw-metadata",
            ],
            vec![
                "aise",
                "search",
                "needle",
                "--format",
                "jsonl",
                "--include",
                "raw-metadata",
            ],
        ] {
            let cli = Cli::try_parse_from(&args)
                .unwrap_or_else(|error| panic!("expected {args:?} to parse: {error}"));
            let includes = match cli.command {
                Commands::List(args) => args.include,
                Commands::Search(args) => args.filters.include,
                other => panic!("expected list or search, got {other:?}"),
            };
            assert_eq!(includes, vec![SessionInclude::RawMetadata]);
        }

        assert_rejects(["aise", "list", "--format", "json", "--include", "unknown"]);

        for subcommand in ["list", "search"] {
            let args = if subcommand == "search" {
                vec!["aise", "search", "needle", "--help"]
            } else {
                vec!["aise", "list", "--help"]
            };
            let help = Cli::try_parse_from(args).unwrap_err().to_string();
            assert!(help.contains("--include"), "{subcommand}: {help}");
            assert!(help.contains("raw-metadata"), "{subcommand}: {help}");
            assert!(help.contains("omitted by default"), "{subcommand}: {help}");
        }
    }

    #[test]
    fn analyze_accepts_shared_scope_policy_and_publication_controls() {
        let cli = Cli::try_parse_from([
            "aise",
            "analyze",
            "--provider",
            "codex",
            "--first-canonical-sessions",
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
        assert_eq!(args.first_canonical_sessions, NonZeroUsize::new(2));
        assert_eq!(args.publication_formats, [AnalysisFormatArg::Json]);

        let cli = Cli::try_parse_from(["aise", "analyze", "--output", "/tmp/full-analysis-bundle"])
            .unwrap();
        let Commands::Analyze(args) = cli.command else {
            panic!("expected analyze command");
        };
        assert_eq!(args.first_canonical_sessions, None);
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
        assert!(help.contains("canonical session-ID order"));
        assert!(help.contains("Omit it to analyze every eligible session"));
        assert!(!help.contains("--limit"));
        assert!(!help.contains("page-size"));
        assert!(!help.contains("use `[search].default_limit`"));
    }

    #[test]
    fn analyze_rejects_zero_canonical_prefix_with_the_all_sessions_replacement() {
        let error = Cli::try_parse_from([
            "aise",
            "analyze",
            "--first-canonical-sessions",
            "0",
            "--output",
            "/tmp/analysis-bundle",
        ])
        .unwrap_err()
        .to_string();
        assert!(error.contains("must be a positive integer"), "{error}");
        assert!(
            error.contains("omit it to analyze every eligible session"),
            "{error}"
        );
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
    fn help_never_recommends_removed_root_status_command() {
        for args in [
            &["aise", "reindex", "--help"][..],
            &["aise", "compact", "--help"],
            &["aise", "doctor", "--help"],
            &["aise", "package", "status", "--help"],
        ] {
            let help = Cli::try_parse_from(args).unwrap_err().to_string();
            assert!(!help.contains("`aise status`"), "{help}");
        }
    }

    #[test]
    fn provider_filters_use_one_concrete_session_source_term() {
        for args in [
            vec!["aise", "list", "--help"],
            vec!["aise", "planning", "--help"],
            vec!["aise", "stats", "--help"],
            vec!["aise", "repeats", "--help"],
        ] {
            let help = Cli::try_parse_from(args).unwrap_err().to_string();
            assert!(
                help.contains("Restrict to one indexed session source"),
                "provider help is not concrete: {help}"
            );
            assert!(!help.contains("Restrict to one harness"), "{help}");
        }
        let cli = Cli::try_parse_from(["aise", "skills", "corrections", "--help"]).unwrap();
        let Commands::Skills(command) = cli.command else {
            panic!("expected skills command")
        };
        let help = command.into_execution().unwrap_err().to_string();
        assert!(
            help.contains("Restrict to one indexed session source"),
            "provider help is not concrete: {help}"
        );
        assert!(!help.contains("Restrict to one harness"), "{help}");
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
    fn messages_search_exposes_only_the_final_presentation_and_include_controls() {
        let cli = Cli::command();
        let search = cli
            .get_subcommands()
            .find(|command| command.get_name() == "messages")
            .unwrap()
            .get_subcommands()
            .find(|command| command.get_name() == "search")
            .unwrap();
        let long_names = search
            .get_arguments()
            .filter_map(clap::Arg::get_long)
            .collect::<std::collections::BTreeSet<_>>();

        for required in ["detail", "field-view-chars", "match-view-chars", "include"] {
            assert!(
                long_names.contains(required),
                "messages search must expose final --{required} syntax"
            );
        }
        for removed in ["include-refs", "match-evidence-max-chars"] {
            assert!(
                !long_names.contains(removed),
                "messages search must not retain provisional --{removed} syntax"
            );
        }
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
        assert!(
            !root_help.contains("accepted by every command"),
            "root options are shared syntactically but command applicability is validated"
        );

        assert_root_options_apply(["aise", "search", "needle", "--config", "/tmp/config.toml"]);
        assert_root_option_is_irrelevant(
            ["aise", "package", "status", "--database", "/tmp/index.db"],
            "--database",
        );
    }

    #[test]
    fn integration_commands_use_one_explicit_namespace() {
        let install_help = Cli::try_parse_from(["aise", "integrations", "install", "--help"])
            .unwrap_err()
            .to_string();
        assert!(install_help.contains("starts best-effort session index preparation"));
        assert!(install_help.contains("run `aise doctor` to check readiness and freshness"));
        assert!(install_help.contains("pi, prime-agent"), "{install_help}");
        assert!(
            install_help.contains("Pi and Prime Agent instead receive their native skill"),
            "{install_help}"
        );

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
        let initialized = fs::read_to_string(&path).unwrap();
        let parsed: crate::config::Config = toml::from_str(&initialized).unwrap();
        assert_eq!(parsed.db_path(), crate::config::Config::default().db_path());
        assert_eq!(
            parsed.cache_dir(),
            crate::config::Config::default().cache_dir()
        );
        assert!(initialized.contains("\ndb_path = "));
        assert!(initialized.contains("\ncache_dir = "));

        fs::write(&path, "preserve until publication").unwrap();
        assert!(write_config_example(&path, false).is_err());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "preserve until publication"
        );

        write_config_example(&path, true).unwrap();
        let replaced = fs::read_to_string(&path).unwrap();
        assert!(replaced.contains("\ndb_path = "));
        assert!(replaced.contains("\ncache_dir = "));
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
    fn stale_message_scope_aliases_name_the_canonical_replacements() {
        let project = parse_cli_from([
            "aise",
            "messages",
            "search",
            "--project",
            "/tmp/project",
            "workflow",
        ])
        .unwrap_err()
        .to_string();
        assert!(project.contains("--workspace-path"), "{project}");
        assert!(
            !project.contains("similar argument exists: '--role'"),
            "{project}"
        );

        let kind = parse_cli_from(["aise", "messages", "search", "workflow", "--type", "user"])
            .unwrap_err()
            .to_string();
        assert!(kind.contains("--role"), "{kind}");

        // The tip keys on the argument clap rejected, so a leading global option, the `=` form,
        // and the usage line all agree; before this the usage line still read
        // `aise messages search --role <ROLE> [QUERY]` under a `--workspace-path` tip.
        for invocation in [
            vec![
                "aise",
                "--index-refresh",
                "existing-only",
                "messages",
                "search",
                "--project",
                "/tmp/project",
                "workflow",
            ],
            vec![
                "aise",
                "messages",
                "search",
                "--project=/tmp/project",
                "workflow",
            ],
        ] {
            let text = parse_cli_from(invocation.clone()).unwrap_err().to_string();
            assert!(text.contains("--workspace-path"), "{invocation:?}: {text}");
            assert!(!text.contains("--role"), "{invocation:?}: {text}");
        }
        assert!(
            !project.contains("--role"),
            "usage line must not name --role: {project}"
        );
        assert!(
            project.contains("Usage: aise messages search [OPTIONS] [QUERY]"),
            "the usage line is the subcommand's real usage: {project}"
        );
    }

    /// The mode renames need the same treatment as the scope renames, and cannot get it from clap.
    ///
    /// A mode rename maps a bare flag to a flag AND a value, so no single argument name is within
    /// clap's edit distance and clap offers nothing of its own. `--regex`/`--fuzzy`/`--explain` are
    /// the spellings `every_documented_aise_example_parses_against_the_current_cli` removed from the
    /// shipped documents; an agent carrying them in memory or in third-party notes still reaches the
    /// CLI with them, so the CLI is where the answer has to be.
    #[test]
    fn stale_message_mode_flags_name_the_canonical_replacement_and_its_value() {
        for (stale, replacement) in [
            ("--regex", "--query-mode regex"),
            ("--fuzzy", "--query-mode fuzzy"),
            ("--explain", "--receipt-level summary"),
        ] {
            let text = parse_cli_from(["aise", "messages", "search", "workflow", stale])
                .unwrap_err()
                .to_string();
            assert!(
                text.contains(replacement),
                "`{stale}` must name `{replacement}`: {text}"
            );
            assert!(
                text.contains("Usage: aise messages search [OPTIONS] [QUERY]"),
                "`{stale}` keeps the subcommand's real usage line: {text}"
            );
        }
    }

    /// Every rename answers with the replacement alone, with no competing tip.
    ///
    /// clap adds `to pass '--regex' as a value, use '-- --regex'` whenever it has no suggestion of
    /// its own, which is true for every mode rename and for `--type`. Searching for the literal text
    /// `--regex` is never what a caller who typed a retired flag meant, and once the table knows the
    /// exact replacement that second tip competes with the answer. `--project` never showed it,
    /// because clap had its own (wrong) guess there, so suppressing it is also what makes all five
    /// renames render the same way.
    ///
    /// The assertion names the tip by its quote-independent phrase. Spelling it with the wrong
    /// quote characters is how the first version of this check passed while the tip was still
    /// printed.
    #[test]
    fn stale_message_flags_answer_with_the_replacement_and_no_pass_as_value_tip() {
        for stale in ["--project", "--type", "--regex", "--fuzzy", "--explain"] {
            let text = parse_cli_from(["aise", "messages", "search", "workflow", stale])
                .unwrap_err()
                .to_string();
            assert!(
                text.contains("a similar argument exists"),
                "`{stale}` names its replacement: {text}"
            );
            assert!(
                !text.contains("as a value"),
                "`{stale}` must not also offer clap's pass-as-value tip: {text}"
            );
        }
    }

    /// Clearing clap's suggestion slot for a renamed flag can only drop the pass-as-value tip.
    ///
    /// `clarify_stale_message_argument` clears `ContextKind::Suggested` whole. For an unknown
    /// argument clap fills that slot with one of two things: the pass-as-value tip, or
    /// `'<subcommand> <flag>' exists` when the flag belongs to a child of the command that rejected
    /// it. The second is what makes clearing the slot lossy, and it is unreachable only while
    /// `messages search` has no children — `--regex`, for one, is a real flag on `aise repeats`, so
    /// the case is not hypothetical, only out of reach from this command. Giving `messages search`
    /// a subcommand would put it in reach; this fails first.
    #[test]
    fn message_search_has_no_subcommands_so_clearing_the_suggestion_slot_drops_only_the_value_tip()
    {
        let mut root = Cli::command();
        root.build();
        let search = root
            .find_subcommand_mut("messages")
            .and_then(|messages| messages.find_subcommand_mut("search"))
            .expect("messages search exists");
        let children: Vec<_> = search
            .get_subcommands()
            .map(clap::Command::get_name)
            .collect();
        assert!(
            children.is_empty(),
            "clearing the suggestion slot would now delete clap's `'<subcommand> <flag>' exists` \
             tip: {children:?}"
        );
    }

    /// The renames stay rejections, never working aliases: accepting `--regex` would keep two
    /// vocabularies alive indefinitely, while a rejection that teaches the current spelling
    /// converges on one.
    #[test]
    fn stale_message_mode_flags_remain_rejected() {
        for stale in ["--regex", "--fuzzy", "--explain"] {
            assert!(
                parse_cli_from(["aise", "messages", "search", "workflow", stale]).is_err(),
                "`{stale}` stays rejected"
            );
        }
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
    fn session_help_defines_span_overlap_without_redefining_event_dates() {
        let mut cli = Cli::command();
        for name in ["list", "search"] {
            let mut help = Vec::new();
            cli.find_subcommand_mut(name)
                .unwrap_or_else(|| panic!("{name} subcommand"))
                .write_long_help(&mut help)
                .unwrap();
            let help = String::from_utf8(help).unwrap();
            for required in [
                "known indexed session span",
                "created_at through updated_at",
                "can contain gaps",
                "not continuous runtime",
                "Inclusive lower bound of the query period",
                "Inclusive upper bound of the query period",
            ] {
                assert!(
                    help.contains(required),
                    "{name} help omits {required}: {help}"
                );
            }
        }
    }

    #[test]
    fn vocab_help_answers_what_it_is_for_before_naming_the_view_it_reads() {
        let mut cli = Cli::command();
        let mut help = Vec::new();
        cli.find_subcommand_mut("vocab")
            .expect("vocab subcommand")
            .write_long_help(&mut help)
            .unwrap();
        let help = String::from_utf8(help).unwrap();

        // A reader who cannot tell what question the command answers cannot tell whether to run
        // it, so the help has to carry the question, the lookup that answers it, and what each
        // reported number counts.
        for required in [
            "--prefix",
            // Two columns of numbers are unreadable until each says what it counts.
            "messages containing",
            "occurrences",
            // In trigram mode the index stores no occurrence counts, so the second column repeats
            // the first. Unstated, it reads as a real and much smaller occurrence count.
            "detail=none",
            // The command a reader actually wants when they want matching messages, not counts.
            "aise messages search",
            // The count asymmetry against the other counting command over the same index.
            "aise stats",
        ] {
            assert!(
                help.contains(required),
                "vocab help omits {required}: {help}"
            );
        }
    }

    /// Ceiling for the one-line summary `aise --help` prints beside each command name, measured
    /// against the list itself: 71 characters median over 25 commands, 104 for the longest
    /// (`messages`). A summary that runs past this wraps and pushes the neighbouring commands off
    /// the screen a reader is scanning to choose between them.
    const COMMAND_SUMMARY_CHARS: usize = 110;

    #[test]
    fn every_command_summary_fits_the_list_and_detail_goes_to_long_help() {
        let cli = Cli::command();
        for command in cli.get_subcommands() {
            let name = command.get_name();
            // A hidden command never prints in the list the ceiling protects.
            if command.is_hide_set() {
                continue;
            }
            let about = command
                .get_about()
                .unwrap_or_else(|| panic!("{name} has no summary"))
                .to_string();
            assert!(
                about.chars().count() <= COMMAND_SUMMARY_CHARS,
                "`aise {name}` summarises itself in {} characters, over the {COMMAND_SUMMARY_CHARS} \
                 the command list holds. Put the first sentence first, then a blank line, and clap \
                 keeps the rest for `--help`: {about}",
                about.chars().count()
            );
        }
    }

    #[test]
    fn counting_commands_keep_their_caveat_where_the_reader_asks_for_detail() {
        use clap::ValueEnum;
        let mut cli = Cli::command();

        let excluded: Vec<String> = crate::models::MessageKind::value_variants()
            .iter()
            .filter(|kind| !crate::models::MessageKind::default_search_set().contains(kind))
            .filter_map(|kind| Some(kind.to_possible_value()?.get_name().replace('-', " ")))
            .collect();
        assert!(!excluded.is_empty(), "the default set excludes nothing");

        // The caveat is too long for the command list, so it lives in `--help`. Asserting on the
        // rendered long help keeps it findable wherever clap decides to put it.
        for name in ["stats", "vocab"] {
            let mut help = Vec::new();
            cli.find_subcommand_mut(name)
                .unwrap_or_else(|| panic!("{name} subcommand"))
                .write_long_help(&mut help)
                .unwrap();
            let help = String::from_utf8(help).unwrap();
            for class in &excluded {
                assert!(
                    help.contains(class),
                    "`aise {name} --help` never names {class}, the class its counts and a raw \
                     `group by role` disagree over: {help}"
                );
            }
        }
    }

    #[test]
    fn db_query_long_help_carries_the_tables_the_notes_and_the_indexed_commands() {
        let mut cmd = Cli::command();
        let query = cmd
            .find_subcommand_mut("db")
            .expect("db subcommand")
            .find_subcommand_mut("query")
            .expect("query subcommand");
        let mut help = Vec::new();
        query.write_long_help(&mut help).unwrap();
        let help = String::from_utf8(help).unwrap();

        // A reader reaches for SQL because they do not know an indexed command answers the same
        // question with ranking and cross-provider matching, so the alternatives lead.
        for command in [
            "aise messages search",
            "aise files search",
            "aise search",
            "aise list",
            "aise stats",
            "aise vocab",
        ] {
            assert!(help.contains(command), "missing {command} in {help}");
        }
        for table in ["sessions", "messages", "file_edits", "transcripts"] {
            assert!(help.contains(table), "missing {table} in {help}");
        }
        assert!(help.contains("aise db schema --table"), "{help}");
        // Same text as `aise db schema`, not a second wording that can drift from it.
        for (table, column, note) in crate::sql_query::SCHEMA_COLUMN_NOTES {
            assert!(
                help.contains(*note),
                "missing the {table}.{column} note in {help}"
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
        let parsed = Cli::try_parse_from([
            "aise",
            "skills",
            "corrections",
            "--session-id",
            "claude:abc",
        ])
        .unwrap();
        let Commands::Skills(command) = parsed.command else {
            panic!("expected skills command")
        };
        assert_eq!(
            command.into_execution().unwrap().args.session_id.as_deref(),
            Some("claude:abc")
        );
        let rejected =
            Cli::try_parse_from(["aise", "skills", "corrections", "--session", "abc"]).unwrap();
        let Commands::Skills(command) = rejected.command else {
            panic!("expected skills command")
        };
        assert!(command.into_execution().is_err());
        for command in ["planning", "stats", "repeats"] {
            assert_parses(["aise", command, "--session-id", "claude:abc"]);
            assert_rejects(["aise", command, "--session", "abc"]);
        }
    }

    #[test]
    fn skills_accepts_a_direct_typed_definition_as_json() {
        let document = r#"{"categories":[{"name":"accuracy","patterns":["\\bwrong\\b"]}]}"#;
        let parsed = Cli::try_parse_from([
            "aise",
            "skills",
            "corrections",
            "--definition-json",
            document,
        ])
        .unwrap();
        let Commands::Skills(command) = parsed.command else {
            panic!("expected skills command")
        };
        let execution = command.into_execution().unwrap();
        let definition = serde_json::from_str::<crate::skill_run::MessageClassificationDefinition>(
            execution.args.definition_json.as_deref().unwrap(),
        )
        .unwrap();
        assert_eq!(definition.categories[0].name, "accuracy");
        assert_eq!(definition.categories[0].patterns, [r"\bwrong\b"]);
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
