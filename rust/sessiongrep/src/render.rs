//! Reusable output rendering shared by every command that emits row data
//! (`messages`, `corrections`, `planning`, `stats`, …).
//!
//! Design: any row type that is `Serialize` (for `json`/`jsonl`) and implements
//! [`Row`] (for `table`/`csv`/`plain`) can be rendered uniformly via [`render`],
//! so each command only describes its columns once.

use std::io::Write;

use anyhow::Result;
use serde::Serialize;

/// Output formats, mirroring aise's `--format` set.
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
            other => Err(format!("unknown format: {other} (table|json|jsonl|csv|plain)")),
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

/// RFC 4180 field escaping: quote when the value contains a comma, quote, CR or LF.
fn csv_escape(field: &str) -> String {
    if field.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
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
                T::headers().iter().map(|h| csv_escape(h)).collect::<Vec<_>>().join(",")
            )?;
            for row in rows {
                writeln!(
                    out,
                    "{}",
                    row.cells().iter().map(|c| csv_escape(c)).collect::<Vec<_>>().join(",")
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
                    .map(|(i, c)| format!("{:width$}", c, width = widths.get(i).copied().unwrap_or(0)))
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
