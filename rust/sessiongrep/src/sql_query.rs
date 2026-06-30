use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use rusqlite::hooks::{AuthAction, Authorization};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Number, Value};

use crate::render::{csv_escape, OutputFormat};

pub const DEFAULT_LIMIT: usize = 100;
pub const DEFAULT_TIMEOUT_MS: u64 = 1_000;
pub const DEFAULT_MCP_MAX_CELL_CHARS: usize = 1_000;
const QUERY_PROGRESS_HANDLER_OPCODES: i32 = 10_000;
const SESSION_INDEX_NOUN: &str = "local AI session-history tables";

#[derive(Debug, Subcommand)]
pub enum DbCmd {
    /// Print the AI session-history SQLite schema, or columns for one table.
    Schema(DbSchemaArgs),
    /// Run one read-only SQL query against the AI session-history index.
    Query(DbQueryArgs),
}

#[derive(Debug, Args, Clone)]
pub struct DbSchemaArgs {
    /// Show columns for one table or virtual table, using SQLite table_xinfo.
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
    /// One read-only SQL statement. Use `sessiongrep db schema` first to inspect tables and
    /// columns. For indexed content or regex search, prefer `sessiongrep messages search`.
    /// Use --limit 0 only when you really want all rows.
    pub sql: String,
    /// Maximum rows to return. 0 = unlimited.
    #[arg(long, default_value_t = DEFAULT_LIMIT)]
    pub limit: usize,
    /// Skip this many rows after the SQL statement runs. Prefer SQL LIMIT/OFFSET for expensive
    /// queries; this is a CLI pagination convenience.
    #[arg(long, default_value_t = 0)]
    pub offset: usize,
    /// Interrupt the query after this many milliseconds. 0 = no timeout.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_MS)]
    pub timeout_ms: u64,
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
    pub cells_truncated: bool,
}

pub fn run(path: &Path, busy_timeout_ms: u64, cmd: DbCmd) -> Result<()> {
    match cmd {
        DbCmd::Schema(args) => {
            let result = schema_path(path, busy_timeout_ms, &args)?;
            let stdout = io::stdout();
            let mut out = stdout.lock();
            render_query_result(&result, args.format, &mut out)?;
            out.flush()?;
        }
        DbCmd::Query(args) => {
            let result =
                query_path(path, busy_timeout_ms, &args).map_err(format_cli_query_error)?;
            let stdout = io::stdout();
            let mut out = stdout.lock();
            render_query_result(&result, args.format, &mut out)?;
            out.flush()?;
        }
    }
    Ok(())
}

pub fn format_cli_query_error(err: anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!(format_query_error(
        err,
        "sessiongrep db query",
        "run `sessiongrep db schema` to list tables, then `sessiongrep db schema --table NAME` to inspect columns",
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

pub fn schema_summary_path(
    path: &Path,
    busy_timeout_ms: u64,
    max_tables: usize,
    max_columns: usize,
) -> Result<String> {
    let conn = open_read_only(path, busy_timeout_ms)?;
    schema_summary_connection(&conn, max_tables, max_columns)
}

pub fn query_path(path: &Path, busy_timeout_ms: u64, args: &DbQueryArgs) -> Result<QueryResult> {
    ensure_single_statement(&args.sql)?;
    let conn = open_read_only(path, busy_timeout_ms)?;
    query_connection(&conn, args)
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

fn query_connection(conn: &Connection, args: &DbQueryArgs) -> Result<QueryResult> {
    with_read_only_authorizer(conn, || {
        if args.timeout_ms > 0 {
            let deadline = Instant::now() + Duration::from_millis(args.timeout_ms);
            conn.progress_handler(
                QUERY_PROGRESS_HANDLER_OPCODES,
                Some(move || Instant::now() >= deadline),
            );
        }

        let result = collect_query_rows(conn, args);
        conn.progress_handler(0, None::<fn() -> bool>);
        result
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
             and name not glob '*_fts'
             and name not glob '*_fts_content'
             and name not glob '*_fts_data'
             and name not glob '*_fts_idx'
             and name not glob '*_fts_docsize'
             and name not glob '*_fts_config'
             and name not glob '*_vocab'
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
        out.insert("name".to_string(), Value::String(row.get::<_, String>(0)?));
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
        ],
        rows,
        truncated: false,
    })
}

fn collect_query_rows(conn: &Connection, args: &DbQueryArgs) -> Result<QueryResult> {
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

fn ensure_single_statement(sql: &str) -> Result<()> {
    if sql.trim().is_empty() {
        bail!("SQL query cannot be empty");
    }
    let mut semicolon_seen = false;
    for token in SqlTokens::new(sql) {
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

impl Iterator for SqlTokens<'_> {
    type Item = SqlToken;

    fn next(&mut self) -> Option<Self::Item> {
        let bytes = self.input.as_bytes();
        if self.pos >= bytes.len() {
            return None;
        }
        let start = self.pos;
        match bytes[self.pos] {
            b';' => {
                self.pos += 1;
                Some(SqlToken::Semicolon)
            }
            b'\'' | b'"' | b'`' => {
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

pub fn query_result_payload(result: &QueryResult, max_cell_chars: usize) -> QueryResultPayload {
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
            "row_truncated": result.truncated,
            "cells_truncated": cells_truncated,
        }),
        cells_truncated,
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

    fn args(sql: &str) -> DbQueryArgs {
        DbQueryArgs {
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
            "sessions(id, provider, provider_session_id, title, summary, cwd, repo_root, created_at, updated_at, last_message_at, preview_text, source_path, message_count, parse_version, raw_metadata_json, parse_warning, discovery_source)"
        ));
        assert!(summary.contains(
            "messages(id, session_id, provider, seq, role, ts, tool_name, is_compaction, content)"
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
        let payload = query_result_payload(&result, 3);

        assert!(payload.cells_truncated);
        assert_eq!(payload.value["columns"], json!(["id", "long_text"]));
        assert_eq!(payload.value["row_truncated"], false);
        assert_eq!(payload.value["cells_truncated"], true);
        assert_eq!(payload.value["rows"][0]["long_text"], "abc... [truncated]");
    }
}
