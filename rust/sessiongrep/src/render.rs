//! Reusable output rendering shared by every command that emits row data
//! (`messages`, `corrections`, `planning`, `stats`, …).
//!
//! Design: any row type that is `Serialize` (for `json`/`jsonl`) and implements
//! [`Row`] (for `table`/`csv`/`plain`) can be rendered uniformly via [`render`],
//! so each command only describes its columns once.

use std::io::Write;

use anyhow::Result;
use serde::Serialize;

/// Output formats (table, json, jsonl, csv, plain).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum OutputFormat {
    #[default]
    Table,
    Json,
    Jsonl,
    Csv,
    Plain,
}

impl std::str::FromStr for OutputFormat {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "table" => Ok(Self::Table),
            "json" => Ok(Self::Json),
            "jsonl" => Ok(Self::Jsonl),
            "csv" => Ok(Self::Csv),
            "plain" => Ok(Self::Plain),
            other => Err(format!(
                "unknown format: {other} (table|json|jsonl|csv|plain)"
            )),
        }
    }
}

/// A row that can present itself as fixed columns for tabular formats.
pub trait Row {
    /// Column headers, in order.
    fn headers() -> &'static [&'static str];
    /// Cell values for this row, in the same order as [`Row::headers`].
    fn cells(&self) -> Vec<String>;
}

/// RFC 4180 field escaping plus spreadsheet formula-injection defense: a field beginning
/// with `=`, `+`, `-`, `@`, tab or CR is prefixed with a `'` so Excel/Sheets treat it as
/// text rather than executing it as a formula. Then quote when the (guarded) value
/// contains a comma, quote, CR or LF.
fn csv_escape(field: &str) -> String {
    let guarded = if field
        .chars()
        .next()
        .is_some_and(|c| matches!(c, '=' | '+' | '-' | '@' | '\t' | '\r'))
    {
        format!("'{field}")
    } else {
        field.to_string()
    };
    if guarded.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", guarded.replace('"', "\"\""))
    } else {
        guarded
    }
}

/// Render `rows` in `fmt` to `out`. Generic over any `Serialize + Row` row type so
/// every command reuses one rendering path.
pub fn render<T, W>(rows: &[T], fmt: OutputFormat, out: &mut W) -> Result<()>
where
    T: Serialize + Row,
    W: Write,
{
    match fmt {
        OutputFormat::Json => writeln!(out, "{}", serde_json::to_string_pretty(rows)?)?,
        OutputFormat::Jsonl => {
            for row in rows {
                writeln!(out, "{}", serde_json::to_string(row)?)?;
            }
        }
        OutputFormat::Csv => {
            writeln!(
                out,
                "{}",
                T::headers()
                    .iter()
                    .map(|h| csv_escape(h))
                    .collect::<Vec<_>>()
                    .join(",")
            )?;
            for row in rows {
                writeln!(
                    out,
                    "{}",
                    row.cells()
                        .iter()
                        .map(|c| csv_escape(c))
                        .collect::<Vec<_>>()
                        .join(",")
                )?;
            }
        }
        OutputFormat::Plain => {
            for row in rows {
                writeln!(out, "{}", row.cells().join("\t"))?;
            }
        }
        OutputFormat::Table => {
            let headers = T::headers();
            let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
            let body: Vec<Vec<String>> = rows
                .iter()
                .map(|r| {
                    let cells = r.cells();
                    for (i, cell) in cells.iter().enumerate() {
                        if i < widths.len() {
                            widths[i] = widths[i].max(cell.chars().count());
                        }
                    }
                    cells
                })
                .collect();
            let fmt_row = |cells: &[String]| -> String {
                cells
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        format!("{:width$}", c, width = widths.get(i).copied().unwrap_or(0))
                    })
                    .collect::<Vec<_>>()
                    .join("  ")
                    .trim_end()
                    .to_string()
            };
            let header_cells: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
            writeln!(out, "{}", fmt_row(&header_cells))?;
            for cells in &body {
                writeln!(out, "{}", fmt_row(cells))?;
            }
        }
    }
    Ok(())
}

// Session-level Row impls live here (the lib) so the binary's `list`/`search`
// handlers can render them via `--format` without tripping the orphan rule.
impl Row for crate::models::SessionRecord {
    fn headers() -> &'static [&'static str] {
        &["updated", "provider", "session", "title", "cwd"]
    }
    fn cells(&self) -> Vec<String> {
        vec![
            crate::util::relative_age(self.updated_at),
            self.provider.as_str().to_string(),
            self.provider_session_id.clone(),
            self.title
                .clone()
                .unwrap_or_else(|| self.preview_text.clone()),
            self.cwd.clone().unwrap_or_default(),
        ]
    }
}

impl Row for crate::models::SearchHit {
    fn headers() -> &'static [&'static str] {
        &["updated", "provider", "session", "score", "match", "title"]
    }
    fn cells(&self) -> Vec<String> {
        vec![
            crate::util::relative_age(self.session.updated_at),
            self.session.provider.as_str().to_string(),
            self.session.provider_session_id.clone(),
            self.score.to_string(),
            self.match_source.clone(),
            self.session
                .title
                .clone()
                .unwrap_or_else(|| self.session.preview_text.clone()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Serialize)]
    struct Demo {
        name: String,
        note: String,
    }
    impl Row for Demo {
        fn headers() -> &'static [&'static str] {
            &["name", "note"]
        }
        fn cells(&self) -> Vec<String> {
            vec![self.name.clone(), self.note.clone()]
        }
    }

    fn demo() -> Vec<Demo> {
        vec![Demo {
            name: "a".into(),
            note: "has, comma".into(),
        }]
    }

    fn rendered(fmt: OutputFormat) -> String {
        let mut buf = Vec::new();
        render(&demo(), fmt, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn csv_quotes_fields_with_commas() {
        let out = rendered(OutputFormat::Csv);
        assert_eq!(out, "name,note\na,\"has, comma\"\n");
    }

    #[test]
    fn csv_escape_guards_formula_injection() {
        // Leading formula characters are neutralized with a `'` prefix.
        assert_eq!(csv_escape("=cmd"), "'=cmd");
        assert_eq!(csv_escape("+1"), "'+1");
        assert_eq!(csv_escape("-cmd"), "'-cmd");
        assert_eq!(csv_escape("@SUM(A1)"), "'@SUM(A1)");
        // Guard composes with RFC-4180 quoting when the field also needs it.
        assert_eq!(csv_escape("=a,b"), "\"'=a,b\"");
        // Ordinary fields are untouched (no false guarding mid-string).
        assert_eq!(csv_escape("hello"), "hello");
        assert_eq!(csv_escape("a-b"), "a-b");
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
    }

    #[test]
    fn jsonl_is_one_object_per_line() {
        let out = rendered(OutputFormat::Jsonl);
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("\"name\":\"a\""));
    }

    #[test]
    fn json_is_an_array() {
        let out = rendered(OutputFormat::Json);
        assert!(out.trim_start().starts_with('['));
    }

    #[test]
    fn plain_is_tab_separated() {
        assert_eq!(rendered(OutputFormat::Plain), "a\thas, comma\n");
    }

    #[test]
    fn table_has_aligned_header() {
        let out = rendered(OutputFormat::Table);
        assert!(out.starts_with("name"));
        assert_eq!(out.lines().count(), 2);
    }

    #[test]
    fn format_parses_case_insensitively() {
        assert_eq!("JSON".parse::<OutputFormat>().unwrap(), OutputFormat::Json);
        assert!("bogus".parse::<OutputFormat>().is_err());
    }
}
