use anyhow::Result;
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as NucleoConfig, Matcher as NucleoMatcher, Utf32Str};
use rusqlite::functions::FunctionFlags;
use rusqlite::Connection;
use std::sync::Mutex;

struct FuzzyState {
    pattern: Pattern,
    matcher: NucleoMatcher,
    utf32_buf: Vec<char>,
}

/// Register the deterministic scalar functions shared by every query connection.
pub(crate) fn register(conn: &Connection) -> Result<()> {
    conn.create_scalar_function(
        "unicode_lower_contains",
        2,
        FunctionFlags::SQLITE_DETERMINISTIC | FunctionFlags::SQLITE_INNOCUOUS,
        |context| {
            let lowercase_query = context.get::<String>(1)?;
            match context.get_raw(0) {
                rusqlite::types::ValueRef::Null => Ok(false),
                value => Ok(value.as_str()?.to_lowercase().contains(&lowercase_query)),
            }
        },
    )?;
    conn.create_scalar_function(
        "rust_regexp",
        2,
        FunctionFlags::SQLITE_DETERMINISTIC | FunctionFlags::SQLITE_INNOCUOUS,
        |context| {
            type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;
            let regex = context.get_or_create_aux(0, |value| -> Result<_, BoxError> {
                Ok(regex::Regex::new(value.as_str()?)?)
            })?;
            match context.get_raw(1) {
                rusqlite::types::ValueRef::Null => Ok(false),
                value => Ok(regex.is_match(value.as_str()?)),
            }
        },
    )?;
    conn.create_scalar_function(
        "rust_json_pointer",
        2,
        FunctionFlags::SQLITE_DETERMINISTIC | FunctionFlags::SQLITE_INNOCUOUS,
        |context| {
            let pointer = context.get::<String>(0)?;
            let content = context.get_raw(1).as_str()?;
            let Ok(envelope) = serde_json::from_str::<serde_json::Value>(content) else {
                return Ok(None::<String>);
            };
            let Some(arguments) = envelope.get("args") else {
                return Ok(None);
            };
            let Some(value) = (if pointer.is_empty() {
                Some(arguments)
            } else {
                arguments.pointer(&pointer)
            }) else {
                return Ok(None);
            };
            Ok(Some(match value {
                serde_json::Value::String(value) => value.clone(),
                other => serde_json::to_string(other)
                    .map_err(|error| rusqlite::Error::UserFunctionError(Box::new(error)))?,
            }))
        },
    )?;
    conn.create_scalar_function(
        "fuzzy_score",
        2,
        FunctionFlags::SQLITE_DETERMINISTIC | FunctionFlags::SQLITE_INNOCUOUS,
        |context| {
            type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;
            let state = context.get_or_create_aux(0, |value| -> Result<_, BoxError> {
                Ok(Mutex::new(FuzzyState {
                    pattern: Pattern::new(
                        value.as_str()?,
                        CaseMatching::Ignore,
                        Normalization::Smart,
                        AtomKind::Fuzzy,
                    ),
                    matcher: NucleoMatcher::new(NucleoConfig::DEFAULT),
                    utf32_buf: Vec::new(),
                }))
            })?;
            let content = context.get_raw(1).as_str()?;
            let mut state = state.lock().map_err(|_| {
                rusqlite::Error::UserFunctionError(
                    std::io::Error::other("fuzzy scorer mutex poisoned").into(),
                )
            })?;
            let FuzzyState {
                pattern,
                matcher,
                utf32_buf,
            } = &mut *state;
            utf32_buf.clear();
            Ok(pattern
                .score(Utf32Str::new(content, utf32_buf), matcher)
                .map(i64::from))
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_lower_contains_preserves_null_empty_and_unicode_behavior() {
        let conn = Connection::open_in_memory().unwrap();
        register(&conn).unwrap();

        let values: (bool, bool, bool, bool) = conn
            .query_row(
                "select unicode_lower_contains(null, ''),
                        unicode_lower_contains('', ''),
                        unicode_lower_contains('CAFÉ', 'café'),
                        unicode_lower_contains('Straße', 'strasse')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();

        assert_eq!(values, (false, true, true, false));
    }

    #[test]
    fn rust_regexp_preserves_regex_syntax_and_null_behavior() {
        let conn = Connection::open_in_memory().unwrap();
        register(&conn).unwrap();

        let values: (bool, bool, bool) = conn
            .query_row(
                "select rust_regexp('^h.llo$', 'hello'),
                        rust_regexp('^h.llo$', 'yellow'),
                        rust_regexp('x', null)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(values, (true, false, false));
        assert!(conn
            .query_row("select rust_regexp('[', 'text')", [], |row| {
                row.get::<_, bool>(0)
            })
            .is_err());
    }

    #[test]
    fn rust_json_pointer_preserves_tool_argument_projection_boundaries() {
        let conn = Connection::open_in_memory().unwrap();
        register(&conn).unwrap();
        let content = r#"{"args":{"cmd":"echo hi","request":{"path":"/tmp/a"},"n":2,"nil":null}}"#;

        let values: (
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "select rust_json_pointer('/cmd', ?1),
                        rust_json_pointer('/request', ?1),
                        rust_json_pointer('/n', ?1),
                        rust_json_pointer('/nil', ?1),
                        rust_json_pointer('/missing', ?1),
                        rust_json_pointer('/cmd', 'malformed')",
                [content],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            values,
            (
                "echo hi".into(),
                r#"{"path":"/tmp/a"}"#.into(),
                "2".into(),
                "null".into(),
                None,
                None,
            )
        );
    }

    #[test]
    fn fuzzy_score_reuses_query_state_and_returns_null_for_non_matches() {
        let conn = Connection::open_in_memory().unwrap();
        register(&conn).unwrap();

        let scores: (Option<i64>, Option<i64>, Option<i64>) = conn
            .query_row(
                "select fuzzy_score('magic config', 'avoid magic values; keep config'),
                        fuzzy_score('magic config', 'unrelated'),
                        fuzzy_score('magic config', 'magic config')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert!(scores.0.is_some());
        assert_eq!(scores.1, None);
        assert!(scores.2.is_some());
    }
}
