// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::io::{self, Write};
use std::num::NonZeroU64;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use rusqlite::hooks::{AuthAction, Authorization};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Number, Value};

use crate::render::{csv_escape, OutputFormat};

pub const DEFAULT_LIMIT: usize = 100;
/// Native CLI/Rust raw-SQL default. Zero means valid read-only work is not interrupted merely
/// because the caller omitted a timer; MCP owns a separate, initially-zero availability guard.
pub const DEFAULT_TIMEOUT_MS: u64 = 0;
pub const DEFAULT_MCP_MAX_CELL_CHARS: usize = 1_000;
const SESSION_INDEX_NOUN: &str = "local AI session-history tables";

/// What to do about a raw-SQL query that ran out of time, printed at the point of failure.
///
/// Narrowing the SQL and raising the bound both keep the reader scanning, so the indexed surface
/// leads. `aise db query --help` already names it, but a reader only reaches this text after
/// passing that help, and the query that times out is usually the one FTS already answers. One
/// string serves the CLI and the MCP tool, so both spellings appear, matching how the sentence
/// below carries the canonical `timeout_ms` beside its CLI flag.
const QUERY_TIMEOUT_RECOVERY: &str = "message content and tool names are indexed, so `aise messages search` (MCP: search_messages) answers those without scanning, with --field tool-name for tool calls. Otherwise narrow the SQL or increase timeout_ms (CLI: --timeout-ms). Set timeout_ms to 0 only when an unbounded query is intentional";

/// Per-column semantics for values that a correct-looking predicate silently misreads.
///
/// `aise db query --help` sends a SQL writer to `aise db schema`, which otherwise reports storage
/// types only. Every entry below yields a wrong answer with no error, which is worse than a
/// failure, so the disclosure belongs beside the column being read rather than in prose the writer
/// passed earlier. Entries are `(table, column, note)`; a test checks each against the live schema
/// so a rename cannot leave a trap silently undocumented. Lookup is a linear scan of this
/// fixed-size table, so schema inspection stays `O(columns)`.
pub(crate) const SCHEMA_COLUMN_NOTES: &[(&str, &str, &str)] = &[
    (
        "messages",
        "tool_name",
        "Provider spelling, not a cross-provider identifier: Claude stores an MCP tool namespaced (mcp__codebase-memory-mcp__search_graph) where Codex stores the leaf name (search_graph). Matching one spelling drops the other provider silently. `aise messages search --field tool-name` (MCP: search_messages with tool_name_contains) matches both.",
    ),
    (
        "messages",
        "tool_call_id",
        "Not unique, in either direction: the same id appears on more than one row of a session, and a resumed session replays the earlier calls under its new session_id keeping their original ids. One row is therefore not one call; use count(distinct tool_call_id).",
    ),
    (
        "messages",
        "kind",
        "Stored with underscores: conversation, compaction, tool_call, tool_result, harness_notice, unknown. MCP's kinds and this SQL surface use those spellings; the CLI hyphenates three of them, as `--kind tool-call`, `tool-result`, and `harness-notice`. No hyphenated spelling appears in storage, so a hyphen in SQL matches no row and reports no error.",
    ),
    (
        "sessions",
        "raw_metadata_json",
        "Provider-shaped, with no key common to all: Codex records $.model where Claude records none, so grouping on json_extract(raw_metadata_json, '$.model') reports a Claude session as having no model rather than as unmeasured. A Claude session can also switch model partway through, so no session-level value would be correct for it.",
    ),
];

/// What each queryable table holds, one row per table, in the order a reader meets them.
///
/// Names the row, not the table, because "the messages table" tells a reader nothing about what
/// counting its rows would mean. A test checks each name against the live schema.
const SCHEMA_TABLE_ROWS: &[(&str, &str)] = &[
    ("sessions", "one indexed session"),
    (
        "messages",
        "one conversation turn, tool call, tool result, compaction, or harness notice",
    ),
    (
        "file_edits",
        "one file-editing tool call, with its path, tool, and resulting content",
    ),
    ("transcripts", "one session's full text, needing no join"),
];

/// The indexed command that answers each question a reader would otherwise write SQL for.
///
/// These lead the long help because reaching for SQL is usually the first mistake, not the
/// predicate: each of these returns ranking, context, and cross-provider matching that a
/// hand-written predicate over one column does not.
const INDEXED_COMMAND_ALTERNATIVES: &[(&str, &str)] = &[
    (
        "aise messages search",
        "message text, tool names, and tool arguments, with surrounding turns",
    ),
    (
        "aise files search",
        "files an edit tool wrote, by path or name, with edit and session counts",
    ),
    ("aise search", "sessions by keyword, ranked by relevance"),
    ("aise list", "sessions by recency, provider, or path"),
    ("aise stats", "message counts by role"),
    (
        // The fts5vocab views this reads are internal, so this line is the whole route to term
        // frequency for a reader who arrived here to write SQL for it.
        "aise vocab",
        "how often a term appears and in how many messages (--prefix looks one up)",
    ),
];

/// Long help for `aise db query`, built from the same constants the schema surface reports.
///
/// A new reader runs `--help` before an index exists, so this reads no database: the table layout
/// comes from the crate's migrations rather than from user data, and tests check both the table
/// names and the notes against a live schema. Restating the notes here rather than referring to
/// `aise db schema` is deliberate: the reader writing the predicate is holding this text and not
/// that output, and the string is shared, so the two cannot disagree.
fn db_query_long_help() -> String {
    let width = INDEXED_COMMAND_ALTERNATIVES
        .iter()
        .map(|(command, _)| command.len())
        .max()
        .unwrap_or_default();
    let mut help = String::from(
        "Run one read-only SQL statement over the local AI session-history index.\n\nMost questions do not need SQL. These read the same index and return ranking, context, and cross-provider matching that a predicate over one column does not:\n\n",
    );
    for (command, answers) in INDEXED_COMMAND_ALTERNATIVES {
        help.push_str(&format!("  {command:width$}  {answers}\n"));
    }
    help.push_str("\nTables, one row per:\n\n");
    let width = SCHEMA_TABLE_ROWS
        .iter()
        .map(|(table, _)| table.len())
        .max()
        .unwrap_or_default();
    for (table, row) in SCHEMA_TABLE_ROWS {
        help.push_str(&format!("  {table:width$}  {row}\n"));
    }
    help.push_str(
        "\nRun `aise db schema` for every table and `aise db schema --table NAME` for one table's columns, types, and notes.\n\nThese values a correct-looking predicate misreads, returning a wrong answer and no error:\n",
    );
    for (table, column, note) in SCHEMA_COLUMN_NOTES {
        help.push_str(&format!("\n  {table}.{column}\n      {note}\n"));
    }
    help
}

#[derive(Debug, Subcommand)]
pub enum DbCmd {
    /// Print the AI session-history SQLite schema, or columns for one table.
    ///
    /// Use `aise db query` to read these tables, or `aise messages search` for indexed search instead of SQL.
    Schema(DbSchemaArgs),
    /// Run one read-only SQL query against the AI session-history index.
    #[command(long_about = db_query_long_help())]
    Query(DbQueryArgs),
}

#[derive(Debug, Args, Clone)]
pub struct DbSchemaArgs {
    /// Show columns for one table or virtual table, using SQLite table_xinfo. The `note` column
    /// carries the reading traps: values a correct-looking predicate misreads without erroring.
    #[arg(long)]
    pub table: Option<String>,
    /// Include SQLite/FTS shadow tables and internal indexes.
    #[arg(long)]
    pub include_internal: bool,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Args, Clone)]
pub struct DbQueryArgs {
    /// One read-only SQL statement. Use `aise db schema` first to inspect tables and
    /// columns. For indexed content or regex search, prefer `aise messages search`.
    /// Use --limit 0 only when you really want all rows.
    pub sql: String,
    /// Maximum rows to return. Omit to use `[db].query_limit` from config. 0 = unlimited.
    #[arg(long)]
    pub limit: Option<usize>,
    /// Skip this many rows after the SQL statement runs. Prefer SQL LIMIT/OFFSET for expensive
    /// queries; this is a CLI pagination convenience.
    #[arg(long, default_value_t = 0)]
    pub offset: usize,
    /// Interrupt the query after this many milliseconds. Omit to use the native
    /// `[db].query_timeout_ms` default, which is 0 (no timeout). MCP resolves its separate
    /// `mcp.query_timeout_ms` before reaching this typed query.
    #[arg(long)]
    pub timeout_ms: Option<u64>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<BTreeMap<String, Value>>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueryResultPayload {
    pub value: Value,
}

#[derive(Debug, Clone)]
pub struct ResolvedDbQueryArgs {
    pub sql: String,
    pub limit: usize,
    pub offset: usize,
    pub timeout_ms: u64,
    pub format: OutputFormat,
}

impl DbQueryArgs {
    pub fn resolve(&self, defaults: &crate::config::DbConfig) -> ResolvedDbQueryArgs {
        ResolvedDbQueryArgs {
            sql: self.sql.clone(),
            limit: self.limit.unwrap_or(defaults.query_limit),
            offset: self.offset,
            timeout_ms: self.timeout_ms.unwrap_or(defaults.query_timeout_ms),
            format: self.format,
        }
    }
}

pub fn run(
    path: &Path,
    busy_timeout_ms: u64,
    defaults: &crate::config::DbConfig,
    cmd: DbCmd,
) -> Result<()> {
    match cmd {
        DbCmd::Schema(args) => {
            let result = schema_path(path, busy_timeout_ms, &args)?;
            let stdout = io::stdout();
            let mut out = stdout.lock();
            render_query_result(&result, args.format, &mut out)?;
            out.flush()?;
        }
        DbCmd::Query(args) => {
            let resolved = args.resolve(defaults);
            let result =
                query_path(path, busy_timeout_ms, &resolved).map_err(format_cli_query_error)?;
            let stdout = io::stdout();
            let mut out = stdout.lock();
            render_query_result(&result, resolved.format, &mut out)?;
            out.flush()?;
        }
    }
    Ok(())
}

pub fn format_cli_query_error(err: anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!(format_query_error(
        err,
        "aise db query",
        "run `aise db schema` to list tables, then `aise db schema --table NAME` to inspect columns",
    ))
}

pub fn format_query_error(err: anyhow::Error, caller: &str, schema_help: &str) -> String {
    let detail = err.to_string();
    let chain = format!("{err:#}");
    if chain.contains("Authorization denied") || chain.contains("not authorized") {
        format!(
            "{caller} rejected this SQL because it is not read-only or uses a blocked SQLite operation. Use exactly one SELECT-style statement over the {SESSION_INDEX_NOUN}, or {schema_help}. Details: {detail}"
        )
    } else if detail.contains("provide exactly one SQL statement") {
        format!(
            "{caller} accepts exactly one SQL statement. Remove extra semicolon-separated statements, or run one query per call."
        )
    } else if detail.contains("query must return rows") {
        format!(
            "{caller} only returns row-producing read-only queries. Use SELECT, WITH ... SELECT, or {schema_help}."
        )
    } else if detail.contains("no table or view named") {
        format!("{detail}. {schema_help}, then retry with one listed table or view name.")
    } else {
        format!("{caller} failed: {chain}")
    }
}

pub fn schema_path(path: &Path, busy_timeout_ms: u64, args: &DbSchemaArgs) -> Result<QueryResult> {
    let conn = open_read_only(path, busy_timeout_ms)?;
    schema_connection(&conn, args)
}

pub(crate) fn schema_path_cancellable(
    path: &Path,
    busy_timeout_ms: u64,
    args: &DbSchemaArgs,
    cancellation: &crate::db::QueryCancellation,
) -> Result<QueryResult> {
    let conn = open_read_only(path, busy_timeout_ms)?;
    cancellation.register(&conn);
    crate::db::with_sqlite_query_control(
        &conn,
        None,
        Some(cancellation.flag_arc()),
        "schema inspection",
        "retry the request after indexing or database contention subsides",
        || schema_connection(&conn, args),
    )
}

pub fn schema_summary_path(
    path: &Path,
    busy_timeout_ms: u64,
    max_tables: usize,
    max_columns: usize,
) -> Result<String> {
    let conn = open_read_only(path, busy_timeout_ms)?;
    schema_summary_connection(&conn, max_tables, max_columns)
}

pub fn query_path(
    path: &Path,
    busy_timeout_ms: u64,
    args: &ResolvedDbQueryArgs,
) -> Result<QueryResult> {
    validate_sql(&args.sql)?;
    let conn = open_read_only(path, busy_timeout_ms)?;
    query_connection(&conn, args)
}

pub(crate) fn query_path_cancellable(
    path: &Path,
    busy_timeout_ms: u64,
    args: &ResolvedDbQueryArgs,
    cancellation: &crate::db::QueryCancellation,
) -> Result<QueryResult> {
    validate_sql(&args.sql)?;
    let conn = open_read_only(path, busy_timeout_ms)?;
    cancellation.register(&conn);
    query_connection_control(&conn, args, Some(cancellation.flag_arc()))
}

fn open_read_only(path: &Path, busy_timeout_ms: u64) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    let conn = Connection::open_with_flags(path, flags)
        .with_context(|| format!("failed to open {} read-only", path.display()))?;
    conn.busy_timeout(Duration::from_millis(busy_timeout_ms))?;
    Ok(conn)
}

fn schema_connection(conn: &Connection, args: &DbSchemaArgs) -> Result<QueryResult> {
    with_read_only_authorizer(conn, || {
        if let Some(table) = args.table.as_deref() {
            table_columns(conn, table)
        } else {
            schema_objects(conn, args.include_internal)
        }
    })
}

fn schema_summary_connection(
    conn: &Connection,
    max_tables: usize,
    max_columns: usize,
) -> Result<String> {
    with_read_only_authorizer(conn, || {
        let schema = load_schema_objects(conn, false)?;
        let mut parts = Vec::new();
        for name in prioritized_schema_table_names(&schema)
            .into_iter()
            .take(max_tables)
        {
            let columns = table_column_names(conn, &name, max_columns)?;
            let suffix = if columns.truncated { ", ..." } else { "" };
            parts.push(format!("{name}({}{suffix})", columns.names.join(", ")));
        }
        if parts.is_empty() {
            Ok(
                "No queryable tables found; call query_session_index with no sql to inspect schema objects."
                    .to_string(),
            )
        } else {
            Ok(parts.join("; "))
        }
    })
}

fn with_read_only_authorizer<T>(conn: &Connection, f: impl FnOnce() -> Result<T>) -> Result<T> {
    conn.execute_batch("pragma query_only = on")?;
    conn.authorizer(Some(read_only_authorizer));
    let result = f();
    conn.authorizer(None::<fn(rusqlite::hooks::AuthContext<'_>) -> Authorization>);
    result
}

struct ColumnNames {
    names: Vec<String>,
    truncated: bool,
}

fn table_column_names(conn: &Connection, table: &str, max_columns: usize) -> Result<ColumnNames> {
    let columns = table_columns(conn, table)?;
    let mut names = columns
        .rows
        .iter()
        .filter_map(|row| row.get("name").map(value_to_cell))
        .collect::<Vec<_>>();
    let truncated = max_columns > 0 && names.len() > max_columns;
    if truncated {
        names.truncate(max_columns);
    }
    Ok(ColumnNames { names, truncated })
}

fn query_connection(conn: &Connection, args: &ResolvedDbQueryArgs) -> Result<QueryResult> {
    query_connection_control(conn, args, None)
}

fn query_connection_control(
    conn: &Connection,
    args: &ResolvedDbQueryArgs,
    cancellation: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<QueryResult> {
    with_read_only_authorizer(conn, || {
        crate::db::with_sqlite_query_control(
            conn,
            NonZeroU64::new(args.timeout_ms),
            cancellation,
            "query",
            QUERY_TIMEOUT_RECOVERY,
            || collect_query_rows(conn, args),
        )
    })
}

const PRIMARY_SCHEMA_TABLES: &[&str] = &["sessions", "messages", "file_edits", "transcripts"];

#[derive(Debug, Clone, Eq, PartialEq)]
struct SchemaObject {
    object_type: String,
    name: String,
    table_name: String,
    sql: String,
}

impl SchemaObject {
    fn to_query_row(&self) -> BTreeMap<String, Value> {
        BTreeMap::from([
            ("type".to_string(), Value::String(self.object_type.clone())),
            ("name".to_string(), Value::String(self.name.clone())),
            (
                "table_name".to_string(),
                Value::String(self.table_name.clone()),
            ),
            ("sql".to_string(), Value::String(self.sql.clone())),
        ])
    }
}

fn prioritized_schema_table_names(schema: &[SchemaObject]) -> Vec<String> {
    let mut names = Vec::new();
    for priority_name in PRIMARY_SCHEMA_TABLES {
        if schema
            .iter()
            .any(|object| object.object_type == "table" && object.name == *priority_name)
        {
            names.push((*priority_name).to_string());
        }
    }
    names.extend(
        schema
            .iter()
            .filter(|object| {
                object.object_type == "table"
                    && !PRIMARY_SCHEMA_TABLES.contains(&object.name.as_str())
            })
            .map(|object| object.name.clone()),
    );
    names
}

fn schema_objects(conn: &Connection, include_internal: bool) -> Result<QueryResult> {
    let mut objects = load_schema_objects(conn, include_internal)?;
    if !include_internal {
        objects.sort_by(compare_schema_objects_for_users);
    }
    let rows = objects
        .into_iter()
        .map(|object| object.to_query_row())
        .collect();
    Ok(QueryResult {
        columns: vec![
            "type".to_string(),
            "name".to_string(),
            "table_name".to_string(),
            "sql".to_string(),
        ],
        rows,
        truncated: false,
    })
}

fn load_schema_objects(conn: &Connection, include_internal: bool) -> Result<Vec<SchemaObject>> {
    let mut stmt = conn.prepare(
        "select type, name, tbl_name as table_name, sql
         from sqlite_schema
         where sql is not null
           and (?1 or (
             type in ('table', 'view')
             and
             name not like 'sqlite_%'
             and not exists (
               select 1
               from sqlite_schema as fts
               where fts.type = 'table'
                 and lower(ltrim(fts.sql)) like 'create virtual table%using fts5%'
                 and sqlite_schema.name in (
                   fts.name || '_content',
                   fts.name || '_data',
                   fts.name || '_idx',
                   fts.name || '_docsize',
                   fts.name || '_config'
                 )
             )
             -- A full-text index and the fts5vocab views over it are machinery behind the
             -- documented tables, so they are internal whatever they are named. Reading the
             -- declaration rather than the name keeps `messages_vocab` and
             -- `messages_trigram_terms` on the same side of the line.
             and lower(ltrim(sql)) not like 'create virtual table%using fts5%'
             and name not glob '*_fts_content'
             and name not glob '*_fts_data'
             and name not glob '*_fts_idx'
             and name not glob '*_fts_docsize'
             and name not glob '*_fts_config'
             and name not glob 'trigram_*'
             and name not in ('files_seen', 'index_metadata')
           ))
         order by
           case type when 'table' then 0 when 'view' then 1 when 'index' then 2 when 'trigger' then 3 else 4 end,
           name",
    )?;
    let mut rows = Vec::new();
    let mapped = stmt.query_map([include_internal], |row| {
        Ok(SchemaObject {
            object_type: row.get(0)?,
            name: row.get(1)?,
            table_name: row.get(2)?,
            sql: row.get(3)?,
        })
    })?;
    for row in mapped {
        rows.push(row?);
    }
    Ok(rows)
}

fn compare_schema_objects_for_users(left: &SchemaObject, right: &SchemaObject) -> Ordering {
    schema_object_priority(left)
        .cmp(&schema_object_priority(right))
        .then_with(|| left.name.cmp(&right.name))
}

fn schema_object_priority(object: &SchemaObject) -> usize {
    PRIMARY_SCHEMA_TABLES
        .iter()
        .position(|name| object.object_type == "table" && object.name == *name)
        .unwrap_or_else(|| match object.object_type.as_str() {
            "table" => PRIMARY_SCHEMA_TABLES.len(),
            "view" => PRIMARY_SCHEMA_TABLES.len() + 1,
            _ => PRIMARY_SCHEMA_TABLES.len() + 2,
        })
}

fn table_columns(conn: &Connection, table: &str) -> Result<QueryResult> {
    let exists: bool = conn.query_row(
        "select exists(
            select 1 from sqlite_schema
            where name = ?1 and type in ('table', 'view')
        )",
        [table],
        |row| row.get(0),
    )?;
    if !exists {
        bail!("no table or view named {table:?}; inspect schema objects to list valid names");
    }

    let mut stmt = conn.prepare(
        "select name, type, \"notnull\", dflt_value, pk, hidden
         from pragma_table_xinfo(?1)
         order by cid",
    )?;
    let mapped = stmt.query_map([table], |row| {
        let mut out = BTreeMap::new();
        let name = row.get::<_, String>(0)?;
        out.insert(
            "note".to_string(),
            schema_column_note(table, &name).map_or(Value::Null, |note| Value::String(note.into())),
        );
        out.insert("name".to_string(), Value::String(name));
        out.insert("type".to_string(), Value::String(row.get::<_, String>(1)?));
        out.insert(
            "not_null".to_string(),
            Value::Bool(row.get::<_, i64>(2)? != 0),
        );
        out.insert("default".to_string(), value_ref_to_json(row.get_ref(3)?));
        out.insert(
            "primary_key".to_string(),
            Number::from(row.get::<_, i64>(4)?).into(),
        );
        out.insert(
            "hidden".to_string(),
            Number::from(row.get::<_, i64>(5)?).into(),
        );
        Ok(out)
    })?;
    let mut rows = Vec::new();
    for row in mapped {
        rows.push(row?);
    }
    Ok(QueryResult {
        columns: vec![
            "name".to_string(),
            "type".to_string(),
            "not_null".to_string(),
            "default".to_string(),
            "primary_key".to_string(),
            "hidden".to_string(),
            "note".to_string(),
        ],
        rows,
        truncated: false,
    })
}

/// The [`SCHEMA_COLUMN_NOTES`] entry for one column, or `None` when reading it holds no trap.
fn schema_column_note(table: &str, column: &str) -> Option<&'static str> {
    SCHEMA_COLUMN_NOTES
        .iter()
        .find(|(note_table, note_column, _)| *note_table == table && *note_column == column)
        .map(|(_, _, note)| *note)
}

fn collect_query_rows(conn: &Connection, args: &ResolvedDbQueryArgs) -> Result<QueryResult> {
    let mut stmt = conn.prepare(&args.sql)?;
    let column_count = stmt.column_count();
    if column_count == 0 {
        bail!("query must return rows; writes and maintenance commands are not supported");
    }
    let columns = unique_column_names(stmt.column_names());
    let mut query = stmt.query([])?;
    let mut skipped = 0usize;
    let mut rows = Vec::new();
    let mut truncated = false;
    while let Some(row) = query.next()? {
        if skipped < args.offset {
            skipped += 1;
            continue;
        }
        if args.limit > 0 && rows.len() >= args.limit {
            truncated = true;
            break;
        }
        let mut out = BTreeMap::new();
        for (idx, name) in columns.iter().enumerate().take(column_count) {
            out.insert(name.clone(), value_ref_to_json(row.get_ref(idx)?));
        }
        rows.push(out);
    }

    Ok(QueryResult {
        columns,
        rows,
        truncated,
    })
}

fn read_only_authorizer(ctx: rusqlite::hooks::AuthContext<'_>) -> Authorization {
    match ctx.action {
        AuthAction::Select | AuthAction::Read { .. } => Authorization::Allow,
        AuthAction::Function { function_name } if allowed_read_only_function(function_name) => {
            Authorization::Allow
        }
        AuthAction::Pragma {
            pragma_name,
            pragma_value,
        } if allowed_read_only_pragma(pragma_name, pragma_value) => Authorization::Allow,
        _ => Authorization::Deny,
    }
}

fn allowed_read_only_function(name: &str) -> bool {
    !name.eq_ignore_ascii_case("load_extension")
}

fn allowed_read_only_pragma(name: &str, value: Option<&str>) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        (name.as_str(), value),
        ("table_info", Some(_))
            | ("table_xinfo", Some(_))
            | ("index_info", Some(_))
            | ("index_xinfo", Some(_))
            | ("database_list", None)
            | ("user_version", None)
            | ("application_id", None)
            | ("data_version", None)
    )
}

/// Every check a statement must pass before it reaches SQLite.
///
/// One entry point so both the CLI and the MCP path apply the same set; adding a check to one
/// caller and not the other is how a surface quietly keeps a defect the other one fixed.
fn validate_sql(sql: &str) -> Result<()> {
    ensure_single_statement(sql)?;
    ensure_vocabulary_predicate_can_match(sql)
}

/// Every column whose stored values come from a vocabulary the CLI and MCP also spell, paired with
/// the flag those surfaces take.
///
/// Each entry maps a typed spelling to the stored one and yields `None` when the two agree, so a
/// vocabulary that matches costs nothing and a vocabulary that diverges is caught the day it does.
/// `kind` is the one that diverges today; `role` and `provider` are listed because being listed is
/// what makes a future rename safe rather than silent. A stored vocabulary added later belongs
/// here, and a test checks each named column against the live schema.
const STORED_VOCABULARIES: &[StoredVocabulary] = &[
    StoredVocabulary {
        column: "kind",
        flag: "--kind",
        stored_for_typed: stored_kind_for_typed_spelling,
    },
    StoredVocabulary {
        column: "role",
        flag: "--role",
        stored_for_typed: stored_role_for_typed_spelling,
    },
    StoredVocabulary {
        column: "provider",
        flag: "--provider",
        stored_for_typed: stored_provider_for_typed_spelling,
    },
];

struct StoredVocabulary {
    /// The index column holding the stored spelling.
    column: &'static str,
    /// The `aise messages search` flag that takes the typed spelling of the same value.
    flag: &'static str,
    /// The stored spelling for a typed one, or `None` when the two agree for that value.
    stored_for_typed: fn(&str) -> Option<&'static str>,
}

/// Reject `kind = 'harness-notice'`, which returns zero rows because the index stores
/// `harness_notice`.
///
/// This is the trap that cannot be disclosed away: an empty table is indistinguishable from "no
/// such data", so a reader who learned the spelling from `aise messages search --kind` has no
/// signal that the vocabulary changed at the SQL boundary. Rewriting their statement would be
/// worse, because a read-only SQL surface has to run what was written, so the statement is refused
/// and the stored spelling is named instead.
///
/// A literal is only refused where it is compared against one of the [`STORED_VOCABULARIES`]
/// columns, so searching content that happens to contain the same text stays valid. Scanning is
/// one pass over the statement's tokens against a fixed-size table.
fn ensure_vocabulary_predicate_can_match(sql: &str) -> Result<()> {
    let mut comparing: Option<&StoredVocabulary> = None;
    for (token, text) in SqlTokens::new(sql) {
        match token {
            SqlToken::WhitespaceOrComment => {}
            SqlToken::Semicolon => comparing = None,
            SqlToken::StringLiteral => {
                let value = unquote_sql_string(text);
                let Some(vocabulary) = comparing else {
                    continue;
                };
                let Some(stored) = (vocabulary.stored_for_typed)(&value) else {
                    continue;
                };
                let (column, flag) = (vocabulary.column, vocabulary.flag);
                bail!(
                    "{column} = '{value}' matches no row: this index stores {column} as \
                     '{stored}'. The spelling you used is the CLI's; MCP and this SQL surface use \
                     the stored one. Retry with '{stored}', or run \
                     `aise messages search {flag} {value}`, which takes the spelling you wrote and \
                     searches the index directly."
                );
            }
            SqlToken::Other => {
                let lowered = text.to_ascii_lowercase();
                comparing = compared_vocabulary_column(&lowered)
                    .or_else(|| comparing.filter(|_| continues_a_comparison(&lowered)));
            }
        }
    }
    Ok(())
}

/// The [`STORED_VOCABULARIES`] entry this token references, allowing for a table alias and for a
/// comparison operator that the tokenizer ran together with the name, as in `m.kind=`.
fn compared_vocabulary_column(token: &str) -> Option<&'static StoredVocabulary> {
    let name = token.trim_end_matches(['=', '!', '<', '>', '(', ',']);
    let name = name.rsplit('.').next().unwrap_or(name);
    STORED_VOCABULARIES
        .iter()
        .find(|vocabulary| vocabulary.column == name)
}

/// Whether this token keeps an already-open `kind` comparison open, covering the operators and
/// punctuation that separate `kind` from its values in `kind in ('a', 'b')`.
fn continues_a_comparison(token: &str) -> bool {
    matches!(
        token.trim_matches(['(', ',']),
        "" | "in" | "like" | "=" | "==" | "!=" | "<>"
    )
}

/// The stored spelling for a `kind` value written the way the CLI and MCP spell it, when the two
/// differ.
///
/// Derived from [`crate::models::MessageKind`] so a variant added later is covered without an edit
/// here, and so a variant whose spellings converge stops being reported. Its two siblings below
/// read their own enums the same way.
pub(crate) fn stored_kind_for_typed_spelling(value: &str) -> Option<&'static str> {
    use clap::ValueEnum;
    crate::models::MessageKind::value_variants()
        .iter()
        .find_map(|kind| diverging_stored_spelling(kind, kind.as_str(), value))
}

/// The stored spelling for a `role` value written the way the CLI and MCP spell it, when the two
/// differ. They agree today, so this reports nothing until a variant is renamed.
fn stored_role_for_typed_spelling(value: &str) -> Option<&'static str> {
    use clap::ValueEnum;
    crate::models::Role::value_variants()
        .iter()
        .find_map(|role| diverging_stored_spelling(role, role.as_str(), value))
}

/// The stored spelling for a `provider` value written the way the CLI and MCP spell it, when the
/// two differ. They agree today, so this reports nothing until a variant is renamed.
fn stored_provider_for_typed_spelling(value: &str) -> Option<&'static str> {
    use clap::ValueEnum;
    crate::models::Provider::value_variants()
        .iter()
        .find_map(|provider| diverging_stored_spelling(provider, provider.as_str(), value))
}

/// `stored` when this variant's typed spelling differs from it and `value` is that typed spelling.
fn diverging_stored_spelling<T: clap::ValueEnum>(
    variant: &T,
    stored: &'static str,
    value: &str,
) -> Option<&'static str> {
    let typed = variant.to_possible_value()?;
    (typed.get_name() != stored && typed.get_name().eq_ignore_ascii_case(value)).then_some(stored)
}

/// The text of a single-quoted SQL string, with the doubled quotes that escape one collapsed.
fn unquote_sql_string(token: &str) -> String {
    token
        .strip_prefix('\'')
        .map_or(token, |rest| rest.strip_suffix('\'').unwrap_or(rest))
        .replace("''", "'")
}

fn ensure_single_statement(sql: &str) -> Result<()> {
    if sql.trim().is_empty() {
        bail!("SQL query cannot be empty");
    }
    let mut semicolon_seen = false;
    for (token, _) in SqlTokens::new(sql) {
        if semicolon_seen && !matches!(token, SqlToken::WhitespaceOrComment) {
            bail!("provide exactly one SQL statement");
        }
        if matches!(token, SqlToken::Semicolon) {
            semicolon_seen = true;
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SqlToken {
    Semicolon,
    WhitespaceOrComment,
    /// A single-quoted SQL string, the only token whose text is a value rather than a name.
    StringLiteral,
    Other,
}

struct SqlTokens<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> SqlTokens<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }
}

impl<'a> Iterator for SqlTokens<'a> {
    type Item = (SqlToken, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        let bytes = self.input.as_bytes();
        if self.pos >= bytes.len() {
            return None;
        }
        let start = self.pos;
        let kind = self.next_kind(bytes, start)?;
        Some((kind, &self.input[start..self.pos]))
    }
}

impl SqlTokens<'_> {
    fn next_kind(&mut self, bytes: &[u8], start: usize) -> Option<SqlToken> {
        match bytes[self.pos] {
            b';' => {
                self.pos += 1;
                Some(SqlToken::Semicolon)
            }
            b'\'' => {
                skip_quoted(bytes, &mut self.pos, b'\'');
                Some(SqlToken::StringLiteral)
            }
            b'"' | b'`' => {
                let quote = bytes[self.pos];
                skip_quoted(bytes, &mut self.pos, quote);
                Some(SqlToken::Other)
            }
            b'[' => {
                skip_bracket_quoted(bytes, &mut self.pos);
                Some(SqlToken::Other)
            }
            b'-' if bytes.get(self.pos + 1) == Some(&b'-') => {
                self.pos += 2;
                while self.pos < bytes.len() && !matches!(bytes[self.pos], b'\n' | b'\r') {
                    self.pos += 1;
                }
                Some(SqlToken::WhitespaceOrComment)
            }
            b'/' if bytes.get(self.pos + 1) == Some(&b'*') => {
                self.pos += 2;
                while self.pos + 1 < bytes.len()
                    && !(bytes[self.pos] == b'*' && bytes[self.pos + 1] == b'/')
                {
                    self.pos += 1;
                }
                self.pos = (self.pos + 2).min(bytes.len());
                Some(SqlToken::WhitespaceOrComment)
            }
            b if b.is_ascii_whitespace() => {
                while self.pos < bytes.len() && bytes[self.pos].is_ascii_whitespace() {
                    self.pos += 1;
                }
                Some(SqlToken::WhitespaceOrComment)
            }
            _ => {
                while !is_sql_token_boundary(bytes, self.pos) {
                    self.pos += 1;
                }
                if self.pos == start {
                    self.pos += 1;
                }
                Some(SqlToken::Other)
            }
        }
    }
}

fn is_sql_token_boundary(bytes: &[u8], pos: usize) -> bool {
    pos >= bytes.len()
        || matches!(bytes[pos], b';' | b'\'' | b'"' | b'`' | b'[')
        || bytes[pos].is_ascii_whitespace()
        || (bytes[pos] == b'-' && bytes.get(pos + 1) == Some(&b'-'))
        || (bytes[pos] == b'/' && bytes.get(pos + 1) == Some(&b'*'))
}

fn skip_quoted(bytes: &[u8], pos: &mut usize, quote: u8) {
    *pos += 1;
    while *pos < bytes.len() {
        if bytes[*pos] == quote {
            *pos += 1;
            if bytes.get(*pos) == Some(&quote) {
                *pos += 1;
                continue;
            }
            break;
        }
        *pos += 1;
    }
}

fn skip_bracket_quoted(bytes: &[u8], pos: &mut usize) {
    *pos += 1;
    while *pos < bytes.len() {
        if bytes[*pos] == b']' {
            *pos += 1;
            break;
        }
        *pos += 1;
    }
}

fn unique_column_names(names: Vec<&str>) -> Vec<String> {
    let mut seen = HashMap::<String, usize>::new();
    names
        .into_iter()
        .enumerate()
        .map(|(idx, raw)| {
            let base = if raw.is_empty() {
                format!("column_{}", idx + 1)
            } else {
                raw.to_string()
            };
            let count = seen.entry(base.clone()).or_insert(0);
            *count += 1;
            if *count == 1 {
                base
            } else {
                format!("{base}_{count}")
            }
        })
        .collect()
}

fn value_ref_to_json(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(value) => Value::Number(Number::from(value)),
        ValueRef::Real(value) => Number::from_f64(value).map_or(Value::Null, Value::Number),
        ValueRef::Text(value) => Value::String(String::from_utf8_lossy(value).into_owned()),
        ValueRef::Blob(value) => Value::String(format!("<blob {} bytes>", value.len())),
    }
}

pub fn render_query_result<W: Write>(
    result: &QueryResult,
    format: OutputFormat,
    out: &mut W,
) -> Result<()> {
    match format {
        OutputFormat::Json => writeln!(out, "{}", serde_json::to_string_pretty(&result.rows)?)?,
        OutputFormat::Jsonl => {
            for row in &result.rows {
                writeln!(out, "{}", serde_json::to_string(row)?)?;
            }
        }
        OutputFormat::Csv => {
            writeln!(
                out,
                "{}",
                result
                    .columns
                    .iter()
                    .map(|h| csv_escape(h))
                    .collect::<Vec<_>>()
                    .join(",")
            )?;
            for row in &result.rows {
                writeln!(out, "{}", csv_cells(result, row).join(","))?;
            }
        }
        OutputFormat::Plain => {
            for row in &result.rows {
                writeln!(out, "{}", plain_cells(result, row).join("\t"))?;
            }
        }
        OutputFormat::Table => render_table(result, out)?,
    }
    if result.truncated {
        writeln!(
            out,
            "# truncated at {} rows; rerun with --limit 0 for all rows",
            result.rows.len()
        )?;
    }
    Ok(())
}

pub fn query_result_payload(
    result: &QueryResult,
    offset: usize,
    max_cell_chars: usize,
) -> QueryResultPayload {
    let mut cells_truncated = false;
    let rows: Vec<Value> = result
        .rows
        .iter()
        .map(|row| {
            let mut out = serde_json::Map::new();
            for column in &result.columns {
                let value = row
                    .get(column)
                    .cloned()
                    .map(|value| truncate_json_value(value, max_cell_chars, &mut cells_truncated))
                    .unwrap_or(Value::Null);
                out.insert(column.clone(), value);
            }
            Value::Object(out)
        })
        .collect();
    QueryResultPayload {
        value: json!({
            "columns": result.columns,
            "rows": rows,
            "next_offset": result
                .truncated
                .then(|| offset.saturating_add(result.rows.len())),
            "truncated_cell_char_limit": cells_truncated.then_some(max_cell_chars),
        }),
    }
}

fn truncate_json_value(value: Value, max_chars: usize, truncated: &mut bool) -> Value {
    if max_chars == 0 {
        return value;
    }
    match value {
        Value::String(value) if value.chars().count() > max_chars => {
            *truncated = true;
            Value::String(format!(
                "{}... [truncated]",
                value.chars().take(max_chars).collect::<String>()
            ))
        }
        other => other,
    }
}

fn csv_cells(result: &QueryResult, row: &BTreeMap<String, Value>) -> Vec<String> {
    plain_cells(result, row)
        .iter()
        .map(|cell| csv_escape(cell))
        .collect()
}

fn plain_cells(result: &QueryResult, row: &BTreeMap<String, Value>) -> Vec<String> {
    result
        .columns
        .iter()
        .map(|column| row.get(column).map(value_to_cell).unwrap_or_default())
        .collect()
}

fn value_to_cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(value) => value.clone(),
        other => other.to_string(),
    }
}

fn render_table<W: Write>(result: &QueryResult, out: &mut W) -> Result<()> {
    let mut widths: Vec<usize> = result.columns.iter().map(|h| h.chars().count()).collect();
    let body: Vec<Vec<String>> = result
        .rows
        .iter()
        .map(|row| {
            let cells = plain_cells(result, row);
            for (idx, cell) in cells.iter().enumerate() {
                if idx < widths.len() {
                    widths[idx] = widths[idx].max(cell.chars().count());
                }
            }
            cells
        })
        .collect();
    let fmt_row = |cells: &[String]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(idx, cell)| format!("{:width$}", cell, width = widths[idx]))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    writeln!(out, "{}", fmt_row(&result.columns))?;
    for row in body {
        writeln!(out, "{}", fmt_row(&row))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "create table demo(id integer primary key, name text, note text);
             create index demo_name_idx on demo(name);
             create virtual table demo_fts using fts5(name, note);
             insert into demo(name, note) values ('alpha', '=formula');
             insert into demo(name, note) values ('beta', 'plain');",
        )
        .unwrap();
        (dir, path)
    }

    fn args(sql: &str) -> ResolvedDbQueryArgs {
        ResolvedDbQueryArgs {
            sql: sql.to_string(),
            limit: 100,
            offset: 0,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            format: OutputFormat::Json,
        }
    }

    fn schema_args() -> DbSchemaArgs {
        DbSchemaArgs {
            table: None,
            include_internal: false,
            format: OutputFormat::Json,
        }
    }

    fn schema_object(name: &str) -> SchemaObject {
        SchemaObject {
            object_type: "table".to_string(),
            name: name.to_string(),
            table_name: name.to_string(),
            sql: format!("create table {name}(id integer)"),
        }
    }

    #[test]
    fn db_query_args_resolve_config_defaults_and_explicit_zero_overrides() {
        let defaults = crate::config::DbConfig {
            query_limit: 17,
            query_timeout_ms: 2500,
        };
        let args = DbQueryArgs {
            sql: "select 1".to_string(),
            limit: None,
            offset: 2,
            timeout_ms: None,
            format: OutputFormat::Json,
        };
        let resolved = args.resolve(&defaults);
        assert_eq!(resolved.limit, 17);
        assert_eq!(resolved.timeout_ms, 2500);
        assert_eq!(resolved.offset, 2);

        let args = DbQueryArgs {
            sql: "select 1".to_string(),
            limit: Some(0),
            offset: 0,
            timeout_ms: Some(0),
            format: OutputFormat::Json,
        };
        let resolved = args.resolve(&defaults);
        assert_eq!(resolved.limit, 0, "explicit 0 keeps unlimited rows");
        assert_eq!(resolved.timeout_ms, 0, "explicit 0 keeps no timeout");
    }

    #[test]
    fn native_default_does_not_interrupt_valid_read_only_sql() {
        assert_eq!(
            DEFAULT_TIMEOUT_MS, 0,
            "native CLI/Rust raw SQL stays unlimited unless the caller configures a timeout"
        );
    }

    #[test]
    fn read_only_query_returns_typed_values() {
        let (_dir, path) = fixture();
        let result =
            query_path(&path, 100, &args("select id, name from demo order by id")).unwrap();
        assert_eq!(result.columns, vec!["id", "name"]);
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0]["id"], Value::Number(Number::from(1)));
        assert_eq!(result.rows[0]["name"], Value::String("alpha".into()));
    }

    #[test]
    fn query_timeout_reports_effective_bound_and_recovery_options() {
        let (_dir, path) = fixture();
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "create table workload(value integer);
             with recursive n(value) as (values(1) union all select value + 1 from n where value < 10000)
             insert into workload select value from n;",
        )
        .unwrap();
        drop(conn);
        let mut query = args("select sum(a.value * b.value) from workload a cross join workload b");
        query.timeout_ms = 1;

        let error = query_path(&path, 100, &query).unwrap_err().to_string();

        assert!(error.contains("timed out after 1 ms"), "{error}");
        assert!(error.contains("--timeout-ms"), "{error}");
        assert!(error.contains("timeout_ms to 0"), "{error}");
        // Narrowing the SQL and raising the timeout both keep the reader scanning. A reader at a
        // timeout has already passed `aise db query --help`, where the indexed alternative is
        // named, so naming it only there leaves it undiscovered exactly when it is needed. One
        // recovery string serves the CLI and MCP paths, so both spellings must appear.
        assert!(error.contains("aise messages search"), "{error}");
        assert!(error.contains("search_messages"), "{error}");
        assert!(error.contains("tool-name"), "{error}");
    }

    #[test]
    fn a_kind_predicate_that_can_never_match_is_rejected_with_the_stored_spelling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let _db = Db::open(&path).unwrap();

        for sql in [
            "select * from messages where kind = 'harness-notice'",
            "select * from messages where kind='harness-notice'",
            "select * from messages m where m.kind in ('tool-call', 'conversation')",
        ] {
            let error = query_path(&path, 100, &args(sql)).unwrap_err().to_string();
            // Zero rows reads as "no such data", which is the one answer a reader cannot debug,
            // so the corrected spelling has to arrive instead of an empty table.
            assert!(
                error.contains("harness_notice") || error.contains("tool_call"),
                "{sql}: {error}"
            );
            assert!(error.contains("aise messages search"), "{sql}: {error}");
        }

        // The hyphenated text is legitimate content, and naming the kind column in the select
        // list is not comparing against it, so neither of these may be rejected.
        for sql in [
            "select kind from messages where content like '%tool-call%'",
            "select * from messages where content = 'harness-notice'",
            "select * from messages where kind = 'harness_notice'",
        ] {
            query_path(&path, 100, &args(sql))
                .unwrap_or_else(|error| panic!("rejected a valid query {sql}: {error}"));
        }
    }

    #[test]
    fn schema_table_discloses_columns_a_correct_looking_predicate_misreads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let _db = Db::open(&path).unwrap();
        let mut args = schema_args();
        args.table = Some("messages".to_string());

        let result = schema_path(&path, 100, &args).unwrap();

        assert!(result.columns.contains(&"note".to_string()), "{result:?}");
        let note = |column: &str| {
            result
                .rows
                .iter()
                .find(|row| value_to_cell(&row["name"]) == column)
                .unwrap_or_else(|| panic!("no {column} column in {result:?}"))["note"]
                .clone()
        };
        // A biased tool_name predicate returns a per-provider answer and no error at all, so the
        // disclosure has to reach the writer while they are reading the column, not afterwards.
        let tool_name = note("tool_name");
        let tool_name = tool_name.as_str().unwrap_or_default();
        assert!(tool_name.contains("mcp__"), "{tool_name}");
        assert!(tool_name.contains("aise messages search"), "{tool_name}");
        let tool_call_id = note("tool_call_id");
        let tool_call_id = tool_call_id.as_str().unwrap_or_default();
        assert!(tool_call_id.contains("distinct"), "{tool_call_id}");
        // The typed surfaces teach `--kind harness-notice`, which is the one spelling that never
        // appears in storage, so a reader who learned the vocabulary from help gets zero rows.
        let kind = note("kind");
        let kind = kind.as_str().unwrap_or_default();
        assert!(kind.contains("harness_notice"), "{kind}");
        assert!(kind.contains("harness-notice"), "{kind}");
        // A note on every column would be noise that hides the few that matter.
        assert_eq!(note("seq"), Value::Null);

        args.table = Some("sessions".to_string());
        let sessions = schema_path(&path, 100, &args).unwrap();
        let metadata = sessions
            .rows
            .iter()
            .find(|row| value_to_cell(&row["name"]) == "raw_metadata_json")
            .expect("raw_metadata_json column")["note"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(metadata.contains("$.model"), "{metadata}");
    }

    #[test]
    fn every_stored_vocabulary_either_matches_its_typed_spelling_or_is_refused_and_disclosed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let _db = Db::open(&path).unwrap();
        let messages = schema_path(
            &path,
            100,
            &DbSchemaArgs {
                table: Some("messages".to_string()),
                ..schema_args()
            },
        )
        .unwrap();

        for vocabulary in STORED_VOCABULARIES {
            let column = vocabulary.column;
            let row = messages
                .rows
                .iter()
                .find(|row| value_to_cell(&row["name"]) == column)
                .unwrap_or_else(|| panic!("declared {column} is absent from messages"));
            let note = row["note"].as_str().unwrap_or_default();

            for typed in typed_spellings_of(column) {
                let Some(stored) = (vocabulary.stored_for_typed)(&typed) else {
                    // The two spellings agree, so SQL written from the typed vocabulary works and
                    // there is nothing to disclose. This is the state every column should be in.
                    continue;
                };
                // They diverge, so SQL written from the typed vocabulary silently matches nothing.
                // Both halves of the treatment are required: the schema note has to name each
                // spelling, and the query has to be refused rather than answered with zero rows.
                assert!(
                    note.contains(stored),
                    "{column}: {stored} missing from {note}"
                );
                assert!(
                    note.contains(&typed),
                    "{column}: {typed} missing from {note}"
                );
                let sql = format!("select * from messages where {column} = '{typed}'");
                let error = query_path(&path, 100, &args(&sql))
                    .expect_err(&format!("{sql} was answered instead of refused"))
                    .to_string();
                assert!(error.contains(stored), "{error}");
                assert!(error.contains(vocabulary.flag), "{error}");
            }
        }
    }

    /// Every spelling the CLI and MCP accept for one stored vocabulary column.
    fn typed_spellings_of(column: &str) -> Vec<String> {
        use clap::ValueEnum;
        fn names<T: ValueEnum>() -> Vec<String> {
            T::value_variants()
                .iter()
                .filter_map(|variant| Some(variant.to_possible_value()?.get_name().to_string()))
                .collect()
        }
        match column {
            "kind" => names::<crate::models::MessageKind>(),
            "role" => names::<crate::models::Role>(),
            "provider" => names::<crate::models::Provider>(),
            other => panic!("no typed vocabulary is declared for {other}"),
        }
    }

    #[test]
    fn the_kind_note_lists_every_stored_spelling_a_predicate_can_match() {
        use clap::ValueEnum;
        let note = schema_column_note("messages", "kind").expect("kind note");

        for kind in crate::models::MessageKind::value_variants() {
            // The enum is the authority for what a SQL writer can match; a variant added later
            // must reach this note rather than leaving one class silently unmatchable. The typed
            // spellings are covered by the stored-vocabulary contract, which also refuses them.
            let stored = kind.as_str();
            assert!(note.contains(stored), "{stored} is missing from: {note}");
        }
    }

    #[test]
    fn every_documented_schema_column_exists_in_the_live_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let _db = Db::open(&path).unwrap();

        let listed = schema_path(&path, 100, &schema_args()).unwrap();
        let names = listed
            .rows
            .iter()
            .map(|row| value_to_cell(&row["name"]))
            .collect::<Vec<_>>();
        for (table, _) in SCHEMA_TABLE_ROWS {
            // `aise db query --help` describes what one row of each table means. A table renamed
            // or dropped by a migration would leave that description describing nothing.
            assert!(
                names.contains(&(*table).to_string()),
                "documented {table} is absent from the live schema: {names:?}"
            );
        }

        for (table, column, _) in SCHEMA_COLUMN_NOTES {
            let mut args = schema_args();
            args.table = Some((*table).to_string());
            let result = schema_path(&path, 100, &args).unwrap();
            // A renamed column silently orphans its note, which puts the trap back undocumented
            // while the constant still reads as if it were covered.
            assert!(
                result
                    .rows
                    .iter()
                    .any(|row| value_to_cell(&row["name"]) == *column),
                "documented {table}.{column} is absent from the live schema: {result:?}"
            );
        }
    }

    #[test]
    fn schema_lists_queryable_objects_without_shadow_tables_by_default() {
        let (_dir, path) = fixture();
        let result = schema_path(&path, 100, &schema_args()).unwrap();
        let names = result
            .rows
            .iter()
            .map(|row| value_to_cell(&row["name"]))
            .collect::<Vec<_>>();

        assert!(names.contains(&"demo".to_string()));
        assert!(!names.contains(&"demo_fts".to_string()));
        assert!(!names.contains(&"demo_fts_data".to_string()));
        assert!(!names.contains(&"demo_name_idx".to_string()));
    }

    #[test]
    fn schema_hides_production_named_trigram_shadow_tables_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "create table messages(id integer primary key, content text);
             create virtual table messages_trigram using fts5(
                 content,
                 content='messages',
                 content_rowid='id',
                 tokenize='trigram',
                 detail=none,
                 columnsize=0
             );
             create virtual table messages_trigram_terms
                 using fts5vocab(messages_trigram, row);",
        )
        .unwrap();

        let result = schema_path(&path, 100, &schema_args()).unwrap();
        let names = result
            .rows
            .iter()
            .map(|row| value_to_cell(&row["name"]))
            .collect::<Vec<_>>();

        assert!(names.contains(&"messages".to_string()));
        // The index and its vocabulary are internal alongside their shadow tables. They were once
        // listed only because the name globs that hid `messages_vocab` did not match these two.
        assert!(!names.contains(&"messages_trigram".to_string()));
        assert!(!names.contains(&"messages_trigram_terms".to_string()));
        assert!(!names.contains(&"messages_trigram_config".to_string()));
        assert!(!names.contains(&"messages_trigram_data".to_string()));
        assert!(!names.contains(&"messages_trigram_idx".to_string()));
    }

    #[test]
    fn schema_summary_prioritizes_core_session_tables() {
        let names = prioritized_schema_table_names(&[
            schema_object("z_extra"),
            schema_object("messages"),
            schema_object("sessions"),
            schema_object("file_edits"),
            schema_object("transcripts"),
        ]);
        assert_eq!(
            names,
            vec![
                "sessions",
                "messages",
                "file_edits",
                "transcripts",
                "z_extra"
            ]
        );
    }

    #[test]
    fn schema_listing_prioritizes_core_session_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "create table z_extra(id integer);
             create table messages(id integer);
             create table sessions(id integer);
             create table file_edits(id integer);
             create table transcripts(id integer);",
        )
        .unwrap();

        let result = schema_path(&path, 100, &schema_args()).unwrap();
        let names = result
            .rows
            .iter()
            .map(|row| value_to_cell(&row["name"]))
            .collect::<Vec<_>>();
        assert_eq!(
            &names[..5],
            [
                "sessions",
                "messages",
                "file_edits",
                "transcripts",
                "z_extra"
            ]
        );
    }

    #[test]
    fn schema_summary_uses_actual_index_schema_columns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let _db = Db::open(&path).unwrap();

        let summary = schema_summary_path(&path, 100, 4, 20).unwrap();
        assert!(summary.contains(
            "sessions(id, provider, provider_session_id, title, summary, cwd, repo_root, created_at, updated_at, last_message_at, preview_text, source_path, message_count, parse_version, raw_metadata_json, parse_warning, discovery_source, parent_session_id, agent_label)"
        ));
        assert!(summary.contains(
            "messages(id, session_id, provider, seq, role, ts, tool_name, kind, tool_call_id, is_compaction, content, authorship, correlation_authority, correlation_scope, correlation_id, record_relation)"
        ));
        assert!(summary.contains(
            "file_edits(id, session_id, provider, seq, ts, tool, file_path, file_name, new_content, edits_json)"
        ));
        assert!(summary.contains("transcripts(session_id, transcript_text)"));
        assert!(!summary.contains("messages_fts("));
        assert!(!summary.contains("files_seen("));
        assert!(!summary.contains("index_metadata("));
    }

    #[test]
    fn schema_can_include_internal_shadow_tables() {
        let (_dir, path) = fixture();
        let mut args = schema_args();
        args.include_internal = true;
        let result = schema_path(&path, 100, &args).unwrap();
        let names = result
            .rows
            .iter()
            .map(|row| value_to_cell(&row["name"]))
            .collect::<Vec<_>>();

        assert!(names.contains(&"demo_fts_data".to_string()));
        assert!(names.contains(&"demo_fts".to_string()));
        assert!(names.contains(&"demo_name_idx".to_string()));
    }

    #[test]
    fn full_text_indexes_and_their_vocabularies_are_internal_whatever_they_are_named() {
        // The live index names its two fts5vocab views `messages_vocab` and
        // `messages_trigram_terms`. A name-shaped rule hid the first and listed the second, so one
        // term-frequency view read as part of the documented schema and the other did not exist.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "create table demo(id integer primary key, name text);
             create virtual table demo_fts using fts5(name);
             create virtual table demo_vocab using fts5vocab('demo_fts', 'row');
             create virtual table demo_terms using fts5vocab('demo_fts', 'row');",
        )
        .unwrap();
        drop(conn);

        let names = |include_internal: bool| -> Vec<String> {
            schema_path(
                &path,
                100,
                &DbSchemaArgs {
                    include_internal,
                    ..schema_args()
                },
            )
            .unwrap()
            .rows
            .iter()
            .map(|row| value_to_cell(&row["name"]))
            .collect()
        };

        let listed = names(false);
        assert!(listed.contains(&"demo".to_string()), "{listed:?}");
        for internal in ["demo_fts", "demo_vocab", "demo_terms"] {
            assert!(
                !listed.contains(&internal.to_string()),
                "{internal} is a full-text index or its vocabulary, not part of the queryable \
                 schema: {listed:?}"
            );
        }

        // Hidden by default, still reachable: the escape hatch is what makes hiding them honest.
        let internal = names(true);
        for name in ["demo_fts", "demo_vocab", "demo_terms"] {
            assert!(
                internal.contains(&name.to_string()),
                "--include-internal must still reach {name}: {internal:?}"
            );
        }
    }

    #[test]
    fn schema_table_prints_columns_using_table_xinfo() {
        let (_dir, path) = fixture();
        let mut args = schema_args();
        args.table = Some("demo".to_string());
        let result = schema_path(&path, 100, &args).unwrap();
        assert_eq!(
            result
                .rows
                .iter()
                .map(|row| value_to_cell(&row["name"]))
                .collect::<Vec<_>>(),
            vec!["id", "name", "note"]
        );
        assert!(schema_path(
            &path,
            100,
            &DbSchemaArgs {
                table: Some("missing".to_string()),
                ..schema_args()
            }
        )
        .is_err());
    }

    #[test]
    fn query_limit_truncates_without_cap() {
        let (_dir, path) = fixture();
        let mut query = args("select id from demo order by id");
        query.limit = 1;
        let result = query_path(&path, 100, &query).unwrap();
        assert!(result.truncated);
        assert_eq!(result.rows.len(), 1);

        query.limit = 0;
        let result = query_path(&path, 100, &query).unwrap();
        assert!(!result.truncated);
        assert_eq!(result.rows.len(), 2);
    }

    #[test]
    fn query_offset_paginates_after_sql_results() {
        let (_dir, path) = fixture();
        let mut query = args("select id from demo order by id");
        query.limit = 1;
        query.offset = 1;
        let result = query_path(&path, 100, &query).unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0]["id"], Value::Number(Number::from(2)));
        assert!(!result.truncated);
    }

    #[test]
    fn rejects_write_and_multi_statement_sql() {
        let (_dir, path) = fixture();
        assert!(query_path(&path, 100, &args("delete from demo")).is_err());
        assert!(query_path(&path, 100, &args("select 1; select 2")).is_err());
        assert!(query_path(&path, 100, &args("pragma wal_checkpoint")).is_err());
        assert!(query_path(&path, 100, &args("attach database '/tmp/x.db' as x")).is_err());
        assert!(query_path(
            &path,
            100,
            &args("select load_extension('/tmp/not-real-extension')")
        )
        .is_err());
        assert!(query_path(
            &path,
            100,
            &args("select * from pragma_table_xinfo('demo')")
        )
        .is_ok());
    }

    #[test]
    fn single_statement_validation_ignores_semicolons_in_strings_and_comments() {
        ensure_single_statement("select ';' as semi -- ;\n").unwrap();
        ensure_single_statement("select 'x'; /* trailing ; comment */").unwrap();
        assert!(ensure_single_statement("select 1; select 2").is_err());
    }

    #[test]
    fn dynamic_csv_uses_existing_formula_guard() {
        let (_dir, path) = fixture();
        let result = query_path(&path, 100, &args("select note from demo where id = 1")).unwrap();
        let mut out = Vec::new();
        render_query_result(&result, OutputFormat::Csv, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "note\n'=formula\n");
    }

    #[test]
    fn duplicate_column_names_are_disambiguated_for_json() {
        let (_dir, path) = fixture();
        let result = query_path(&path, 100, &args("select id, id from demo limit 1")).unwrap();
        assert_eq!(result.columns, vec!["id", "id_2"]);
        assert!(result.rows[0].contains_key("id"));
        assert!(result.rows[0].contains_key("id_2"));
    }

    #[test]
    fn mcp_payload_shape_includes_columns_and_cell_truncation() {
        let (_dir, path) = fixture();
        let result = query_path(
            &path,
            100,
            &args("select id, 'abcdef' as long_text from demo limit 1"),
        )
        .unwrap();
        let payload = query_result_payload(&result, 0, 3);

        assert_eq!(payload.value["columns"], json!(["id", "long_text"]));
        assert_eq!(payload.value["next_offset"], Value::Null);
        assert_eq!(payload.value["truncated_cell_char_limit"], 3);
        assert_eq!(payload.value["rows"][0]["long_text"], "abc... [truncated]");

        let mut paged = result.clone();
        paged.truncated = true;
        let payload = query_result_payload(&paged, 7, 0);
        assert_eq!(payload.value["next_offset"], 7 + paged.rows.len());
        assert_eq!(payload.value["truncated_cell_char_limit"], Value::Null);
    }
}
