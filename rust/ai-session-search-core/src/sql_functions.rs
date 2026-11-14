// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

use anyhow::Result;
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as NucleoConfig, Matcher as NucleoMatcher, Utf32Str};
use rusqlite::functions::{Context, FunctionFlags};
use rusqlite::Connection;
use std::sync::Mutex;

use crate::util::UnicodeLowerNeedle;

struct FuzzyState {
    pattern: Pattern,
    matcher: NucleoMatcher,
    utf32_buf: Vec<char>,
}

fn unicode_lower_contains_value(needle: &UnicodeLowerNeedle, value: &str) -> bool {
    needle.contains(value)
}

/// Verify whole content or user-provided regions without SQLite's NUL-terminated substr.
/// For B content bytes and P parts, range traversal is O(B + P), with O(P) temporary metadata
/// and borrowed slices. No content copies or repeated prefix scans; the matcher adds its own cost.
fn matches_content(
    context: &Context<'_>,
    content_arg: usize,
    mut matches: impl FnMut(&str) -> bool,
) -> rusqlite::Result<bool> {
    let value = context.get_raw(content_arg);
    if value == rusqlite::types::ValueRef::Null {
        return Ok(false);
    }
    let content = value.as_str()?;
    if context.len() == 2 || context.get_raw(2) == rusqlite::types::ValueRef::Null {
        return Ok(matches(content));
    }
    let Ok(parts): Result<Vec<crate::models::MessageContentPart>, _> =
        serde_json::from_str(context.get_raw(2).as_str()?)
    else {
        return Ok(false);
    };
    // Invalid regions cannot supply evidence, matching the fuzzy/evidence path. Full-content
    // search remains available without inventing or clamping a user-provided region.
    let Ok(regions) = crate::util::content_part_regions(
        content,
        parts.iter().filter(|part| part.is_user_input()),
    ) else {
        return Ok(false);
    };
    Ok(regions.into_iter().any(|(_, region)| matches(region)))
}

/// Register the deterministic scalar functions shared by every query connection.
pub(crate) fn register(conn: &Connection) -> Result<()> {
    // The optional third argument selects mixed-message regions; ordinary searches keep the
    // existing two-argument path and never load or serialize part metadata.
    for arity in [2, 3] {
        conn.create_scalar_function(
            "unicode_lower_contains",
            arity,
            FunctionFlags::SQLITE_DETERMINISTIC | FunctionFlags::SQLITE_INNOCUOUS,
            |context| {
                type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;
                // The query is constant across a statement; cache its streaming matcher once. This
                // preserves `fold_caseless(haystack).contains(...)` semantics, expansions included,
                // without allocating a folded copy of every candidate row. The bound argument is
                // already folded by the caller, so both sides share one rule.
                let lowercase_query =
                    context.get_or_create_aux(1, |value| -> Result<_, BoxError> {
                        Ok(UnicodeLowerNeedle::from_lowered(value.as_str()?))
                    })?;
                matches_content(context, 0, |content| {
                    unicode_lower_contains_value(&lowercase_query, content)
                })
            },
        )?;
        conn.create_scalar_function(
            "rust_regexp",
            arity,
            FunctionFlags::SQLITE_DETERMINISTIC | FunctionFlags::SQLITE_INNOCUOUS,
            |context| {
                type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;
                let regex = context.get_or_create_aux(0, |value| -> Result<_, BoxError> {
                    Ok(regex::Regex::new(value.as_str()?)?)
                })?;
                matches_content(context, 1, |content| regex.is_match(content))
            },
        )?;
    }
    conn.create_scalar_function(
        "rust_json_pointer",
        2,
        FunctionFlags::SQLITE_DETERMINISTIC | FunctionFlags::SQLITE_INNOCUOUS,
        |context| {
            type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;
            // The pointer is constant across a statement; cache it once per scan.
            let pointer = context.get_or_create_aux(0, |value| -> Result<_, BoxError> {
                Ok(String::from(value.as_str()?))
            })?;
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
    fn mixed_content_matching_keeps_nul_unicode_and_separate_user_regions() {
        use crate::models::{ContentPartAuthorship, ContentPartOrigin, MessageContentPart};

        let conn = Connection::open_in_memory().unwrap();
        register(&conn).unwrap();
        let content = "\0é|generated|🦀";
        let parts = [
            MessageContentPart {
                ordinal: 0,
                start_char: 0,
                end_char: 2,
                authorship: ContentPartAuthorship::Unknown,
                origin: ContentPartOrigin::DirectInput,
            },
            MessageContentPart {
                ordinal: 1,
                start_char: 2,
                end_char: 13,
                authorship: ContentPartAuthorship::Generated,
                origin: ContentPartOrigin::ToolPayload,
            },
            MessageContentPart {
                ordinal: 2,
                start_char: 13,
                end_char: 14,
                authorship: ContentPartAuthorship::Human,
                origin: ContentPartOrigin::DirectInput,
            },
        ];
        let encoded = serde_json::to_string(&parts).unwrap();
        for (query, expected) in [
            ("\0é", true),
            ("🦀", true),
            ("generated", false),
            ("é🦀", false),
        ] {
            let actual: bool = conn
                .query_row(
                    "select unicode_lower_contains(?1, ?2, ?3)",
                    rusqlite::params![content, query, encoded],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(actual, expected, "{query:?}");
        }
        for (query, expected) in [
            ("^🦀$", true),
            ("^\\x00é$", true),
            ("generated", false),
            ("é.*🦀", false),
        ] {
            let actual: bool = conn
                .query_row(
                    "select rust_regexp(?1, ?2, ?3)",
                    rusqlite::params![query, content, encoded],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(actual, expected, "{query:?}");
        }
        let unrestricted: bool = conn
            .query_row(
                "select unicode_lower_contains(?1, 'generated', null)",
                [content],
                |row| row.get(0),
            )
            .unwrap();
        assert!(unrestricted);
        let no_regions: bool = conn
            .query_row("select rust_regexp('', ?1, '[]')", [content], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(!no_regions);
        let invalid = serde_json::to_string(&[MessageContentPart {
            end_char: 100,
            ..parts[0].clone()
        }])
        .unwrap();
        assert!(!conn
            .query_row(
                "select unicode_lower_contains(?1, 'é', ?2)",
                rusqlite::params![content, invalid],
                |row| row.get::<_, bool>(0)
            )
            .unwrap());
    }

    #[test]
    fn unicode_lower_contains_streams_lowercase_expansions_without_a_haystack_copy() {
        let needle = crate::util::UnicodeLowerNeedle::from_lowered("i\u{307}st");

        assert!(unicode_lower_contains_value(&needle, "İstanbul"));
        assert!(!unicode_lower_contains_value(&needle, "unrelated"));
    }

    #[test]
    fn unicode_lower_contains_preserves_null_behavior() {
        let conn = Connection::open_in_memory().unwrap();
        register(&conn).unwrap();

        // A null column is not text and contains nothing, which is what keeps a `tool_name is
        // null` row out of a tool-name search without a second predicate saying so.
        let value: bool = conn
            .query_row("select unicode_lower_contains(null, '')", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(!value);
    }

    /// The scalar SQLite calls answers the shared cases the same way the matcher does.
    ///
    /// This is the layer that decides which rows a literal search returns, and it reaches the
    /// matcher by a different route than ranking or snippets do: the needle arrives as a bound
    /// argument and is cached per statement. Driving it from the same table is what keeps a
    /// case added for one layer from passing there while failing here.
    #[test]
    fn the_sql_scalar_answers_every_shared_caseless_case() {
        let conn = Connection::open_in_memory().unwrap();
        register(&conn).unwrap();

        for case in crate::util::CASELESS_CASES {
            let folded = crate::util::fold_caseless(case.needle);
            let value: bool = conn
                .query_row(
                    "select unicode_lower_contains(?1, ?2)",
                    rusqlite::params![case.haystack, folded],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                value, case.contains,
                "{}: haystack={:?}, needle={:?}",
                case.why, case.haystack, case.needle
            );
        }
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
