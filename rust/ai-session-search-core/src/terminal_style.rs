// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

//! What the terminal on the other end can actually draw.
//!
//! The browser reaches terminals that are not the maintainer's: an `ssh` session with `LANG=C`,
//! a CI log, a serial console, `TERM=dumb`. Two capabilities decide whether the screen is
//! readable there — whether the encoding carries characters outside ASCII, and whether colour
//! survives — and neither is something a widget library can answer, because both are properties
//! of the far end rather than of the program.
//!
//! Configuration states the intent (`auto`, `on`, `off`) and this module resolves it against the
//! environment when the TUI starts, so nothing about a terminal is decided while a configuration
//! file is being parsed. Every symbol the browser draws comes from here, so a fallback is one
//! table rather than a search for stray glyphs.

use std::env;

use ratatui::symbols::border;
use serde::{Deserialize, Serialize};

/// Whether a capability is used, and how that is decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CapabilityMode {
    /// Read the environment, and prefer the richer rendering when it says nothing.
    #[default]
    Auto,
    On,
    Off,
}

/// An ASCII border, for a terminal whose encoding cannot carry box drawing. Ratatui ships no
/// ASCII set — every one of its border sets is line-drawing or block characters — but a set is
/// a plain struct of strings, so this supplies one rather than reimplementing the border.
const ASCII_BORDER: border::Set<'static> = border::Set {
    top_left: "+",
    top_right: "+",
    bottom_left: "+",
    bottom_right: "+",
    vertical_left: "|",
    vertical_right: "|",
    horizontal_top: "-",
    horizontal_bottom: "-",
};

/// The symbols and styling the browser draws with, resolved for one run.
///
/// Crate-internal, and deliberately so: [`Self::border_set`] returns a `ratatui` type this crate
/// does not re-export, so a caller outside it could not name the result. Nothing outside can draw
/// a frame either — `tui` is a private module — which leaves this useful only in here. Publishing
/// it would make a `ratatui` major bump a breaking change to *this* crate's public API for an
/// audience that has no use for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalStyle {
    unicode: bool,
    color: bool,
}

impl Default for TerminalStyle {
    fn default() -> Self {
        Self {
            unicode: true,
            color: true,
        }
    }
}

impl TerminalStyle {
    /// Resolve both capabilities from the configured intent and this process's environment.
    pub(crate) fn resolve(unicode: CapabilityMode, color: CapabilityMode) -> Self {
        Self {
            unicode: match unicode {
                CapabilityMode::On => true,
                CapabilityMode::Off => false,
                CapabilityMode::Auto => environment_carries_unicode(|name: &str| env::var_os(name)),
            },
            color: match color {
                CapabilityMode::On => true,
                CapabilityMode::Off => false,
                CapabilityMode::Auto => environment_shows_color(|name: &str| env::var_os(name)),
            },
        }
    }

    /// There is deliberately no `unicode()` beside this. Colour is asked about once, to clear it
    /// from the finished frame; every unicode decision is a symbol, so the renderer asks for the
    /// symbol and cannot forget the fallback by branching on a boolean of its own.
    pub(crate) fn color(&self) -> bool {
        self.color
    }

    /// The border a pane is drawn with.
    pub(crate) fn border_set(&self) -> border::Set<'static> {
        if self.unicode {
            border::PLAIN
        } else {
            ASCII_BORDER
        }
    }

    /// Marks text cut out of the middle of a line.
    pub(crate) fn ellipsis(&self) -> &'static str {
        if self.unicode {
            "…"
        } else {
            "..."
        }
    }

    /// Separates the status bar's hints.
    pub(crate) fn hint_separator(&self) -> &'static str {
        if self.unicode {
            " │ "
        } else {
            " | "
        }
    }

    /// Opens a preview section heading.
    pub(crate) fn section_rule(&self) -> &'static str {
        if self.unicode {
            "──"
        } else {
            "--"
        }
    }

    /// Opens the preview's "N more turns hidden" line.
    pub(crate) fn elision_marker(&self) -> &'static str {
        if self.unicode {
            "⋯"
        } else {
            "..."
        }
    }

    /// Separates the parts of a pane title.
    pub(crate) fn title_separator(&self) -> &'static str {
        if self.unicode {
            "·"
        } else {
            "-"
        }
    }

    /// Marks the selected row. It is drawn whatever the colour support, because a reader who
    /// cannot see the highlight colour still has to know which row Enter would resume.
    pub(crate) fn selection_symbol(&self) -> &'static str {
        if self.unicode {
            "❯ "
        } else {
            "> "
        }
    }
}

/// True unless the locale names an encoding that is not UTF-8.
///
/// Unset is treated as capable: the browser has always drawn box borders, and a terminal that
/// handles them is far more common than one that does not, so an absent `LANG` is not evidence
/// of anything. An explicit `C`, `POSIX`, or non-UTF-8 charset is evidence, and that is what
/// this looks for. `LC_ALL` outranks `LC_CTYPE`, which outranks `LANG`, as POSIX specifies.
fn environment_carries_unicode(read: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .find_map(&read)
        .and_then(|value| value.into_string().ok());
    let Some(locale) = locale.filter(|value| !value.is_empty()) else {
        return true;
    };
    let lowered = locale.to_ascii_lowercase();
    if lowered == "c" || lowered == "posix" || lowered.starts_with("c.") && !lowered.contains("utf")
    {
        return lowered.contains("utf");
    }
    match lowered.split_once('.') {
        // A charset is named: it decides, and only UTF-8 carries the symbols.
        Some((_, charset)) => charset.contains("utf-8") || charset.contains("utf8"),
        // No charset named and not the C locale: no evidence against.
        None => true,
    }
}

/// True unless the environment asks for no colour.
///
/// `NO_COLOR` is honoured as its specification states — set and non-empty means no colour,
/// whatever the value — and `TERM=dumb` names a terminal that renders no attributes at all.
fn environment_shows_color(read: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    if read("NO_COLOR").is_some_and(|value| !value.is_empty()) {
        return false;
    }
    !read("TERM").is_some_and(|value| value.eq_ignore_ascii_case("dumb"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::ffi::OsString;

    fn environment<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        let map: HashMap<&str, &str> = pairs.iter().copied().collect();
        move |name| map.get(name).map(OsString::from)
    }

    #[test]
    fn an_absent_locale_is_not_evidence_against_unicode() {
        // The browser has always drawn box borders; a terminal that cannot is the rare one, and
        // an unset LANG says nothing either way. Falling back here would change how the screen
        // looks for everyone whose environment simply does not set it.
        assert!(environment_carries_unicode(environment(&[])));
    }

    #[test]
    fn a_named_non_utf8_charset_falls_back() {
        for locale in ["C", "POSIX", "en_US.ISO8859-1", "ja_JP.eucJP", "c"] {
            assert!(
                !environment_carries_unicode(environment(&[("LANG", locale)])),
                "{locale} names an encoding that cannot carry box drawing"
            );
        }
        for locale in ["en_US.UTF-8", "C.UTF-8", "en_GB.utf8", "en_US"] {
            assert!(
                environment_carries_unicode(environment(&[("LANG", locale)])),
                "{locale} carries it"
            );
        }
    }

    #[test]
    fn lc_all_outranks_lc_ctype_which_outranks_lang() {
        assert!(!environment_carries_unicode(environment(&[
            ("LC_ALL", "C"),
            ("LC_CTYPE", "en_US.UTF-8"),
            ("LANG", "en_US.UTF-8"),
        ])));
        assert!(!environment_carries_unicode(environment(&[
            ("LC_CTYPE", "C"),
            ("LANG", "en_US.UTF-8"),
        ])));
        assert!(environment_carries_unicode(environment(&[
            ("LC_ALL", "en_US.UTF-8"),
            ("LANG", "C"),
        ])));
    }

    #[test]
    fn no_color_is_honoured_whatever_its_value() {
        // The specification is presence, not truthiness: `NO_COLOR=0` still means no colour.
        for value in ["1", "0", "false", "yes"] {
            assert!(!environment_shows_color(environment(&[(
                "NO_COLOR", value
            )])));
        }
        assert!(environment_shows_color(environment(&[("NO_COLOR", "")])));
        assert!(environment_shows_color(environment(&[])));
    }

    #[test]
    fn a_dumb_terminal_gets_no_color() {
        assert!(!environment_shows_color(environment(&[("TERM", "dumb")])));
        assert!(!environment_shows_color(environment(&[("TERM", "DUMB")])));
        assert!(environment_shows_color(environment(&[(
            "TERM",
            "xterm-256color"
        )])));
    }

    #[test]
    fn an_explicit_setting_outranks_the_environment() {
        let forced = TerminalStyle::resolve(CapabilityMode::On, CapabilityMode::On);
        assert!(forced.unicode && forced.color());
        let refused = TerminalStyle::resolve(CapabilityMode::Off, CapabilityMode::Off);
        assert!(!refused.unicode && !refused.color());
    }

    #[test]
    fn every_ascii_symbol_is_ascii_and_every_unicode_one_is_not_empty() {
        // A fallback that still emits a multi-byte character is not a fallback, and one that
        // emits nothing loses the distinction it was drawn for.
        let ascii = TerminalStyle {
            unicode: false,
            color: true,
        };
        let unicode = TerminalStyle::default();
        let border = ascii.border_set();
        for symbol in [
            ascii.ellipsis(),
            ascii.hint_separator(),
            ascii.section_rule(),
            ascii.elision_marker(),
            ascii.title_separator(),
            ascii.selection_symbol(),
            border.top_left,
            border.top_right,
            border.bottom_left,
            border.bottom_right,
            border.vertical_left,
            border.vertical_right,
            border.horizontal_top,
            border.horizontal_bottom,
        ] {
            assert!(symbol.is_ascii(), "{symbol:?} is not ASCII");
            assert!(!symbol.is_empty());
        }
        for symbol in [
            unicode.ellipsis(),
            unicode.hint_separator(),
            unicode.section_rule(),
            unicode.elision_marker(),
            unicode.title_separator(),
            unicode.selection_symbol(),
        ] {
            assert!(!symbol.is_empty());
        }
    }
}
