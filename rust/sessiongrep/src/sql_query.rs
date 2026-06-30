use std::collections::{BTreeMap, HashMap};
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use rusqlite::hooks::{AuthAction, Authorization};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Number, Value};

use crate::render::{csv_escape, OutputFormat};

const DEFAULT_LIMIT: usize = 100;
const DEFAULT_TIMEOUT_MS: u64 = 1_000;

#[derive(Debug, Subcommand)]
pub enum DbCmd {
    /// Run one read-only SQL query against the sessiongrep index.
    Query(DbQueryArgs),
}

#[derive(Debug, Args, Clone)]
pub struct DbQueryArgs {
    /// One read-only SQL statement. Use --limit 0 only when you really want all rows.
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

pub fn run(path: &Path, busy_timeout_ms: u64, cmd: DbCmd) -> Result<()> {
    match cmd {
        DbCmd::Query(args) => {
            let result = query_path(path, busy_timeout_ms, &args)?;
            let stdout = io::stdout();
            let mut out = stdout.lock();
            render_query_result(&result, args.format, &mut out)?;
            out.flush()?;
        }
    }
    Ok(())
}

pub fn query_path(path: &Path, busy_timeout_ms: u64, args: &DbQueryArgs) -> Result<QueryResult> {
    ensure_single_statement(&args.sql)?;
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    let conn = Connection::open_with_flags(path, flags)
        .with_context(|| format!("failed to open {} read-only", path.display()))?;
    conn.busy_timeout(Duration::from_millis(busy_timeout_ms))?;
    query_connection(&conn, args)
}

fn query_connection(conn: &Connection, args: &DbQueryArgs) -> Result<QueryResult> {
    conn.execute_batch("pragma query_only = on")?;
    conn.authorizer(Some(read_only_authorizer));

    if args.timeout_ms > 0 {
        let deadline = Instant::now() + Duration::from_millis(args.timeout_ms);
        conn.progress_handler(10_000, Some(move || Instant::now() >= deadline));
    }

    let result = collect_query_rows(conn, args);

    conn.progress_handler(0, None::<fn() -> bool>);
    conn.authorizer(None::<fn(rusqlite::hooks::AuthContext<'_>) -> Authorization>);
    result
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
        AuthAction::Select | AuthAction::Read { .. } | AuthAction::Function { .. } => {
            Authorization::Allow
        }
        AuthAction::Pragma {
            pragma_value: None, ..
        } => Authorization::Allow,
        _ => Authorization::Deny,
    }
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

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "create table demo(id integer primary key, name text, note text);
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
}
