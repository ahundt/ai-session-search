// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

//! The TUI's keyboard vocabulary: what a key press means, spelled the way `[ui.keys]` spells it.
//!
//! This module owns the vocabulary and nothing else. `config` stores a [`KeyBindings`] and
//! validates it with everything else; `tui` asks it what a [`KeyEvent`] means and never parses a
//! key name. Keeping the parsing here is what lets the configuration file, the rendered help, and
//! the error messages all be derived from one table instead of three lists that drift.
//!
//! [`KeyBindings`] is the only public item here, because it is the type of a `[ui]` field and a
//! caller therefore has to be able to name it. Everything else is crate-internal on purpose:
//! `KeyChord` and `KeyBindings::action_for` speak in `crossterm`'s key types, which this crate
//! does not re-export, so an outside caller could not construct an argument or read a result — and
//! could not run the browser either, since `tui` is a private module. Publishing them would make a
//! `crossterm` major bump a breaking change to *this* crate's public API in exchange for nothing.
//! Widening any of this later is not a breaking change; narrowing it after 1.0.0 would be.
//!
//! Those two are written in plain backticks rather than as doc links, because a public module's
//! documentation linking a private item is what `cargo doc -D warnings` refuses.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use anyhow::{bail, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// One thing the reader can ask the browser to do.
///
/// The variant list is the source of truth for the accepted `[ui.keys]` names, the duplicate
/// check, and the rendered help, so adding a command means adding it here once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TuiAction {
    /// Ask to leave. The first press arms, the second quits — see [`TuiAction::Interrupt`].
    Interrupt,
    Quit,
    EnterSearch,
    /// Leave the search box, keeping the query and searching it at once.
    LeaveSearch,
    MoveDown,
    MoveUp,
    PageDown,
    PageUp,
    Top,
    Bottom,
    CycleProvider,
    CycleSessionKind,
    CycleTimeWindow,
    ToggleWarningsOnly,
    PreviewScrollDown,
    PreviewScrollUp,
    PreviewPageDown,
    PreviewPageUp,
    Resume,
    /// Show every command and the keys bound to it.
    Help,
    // The search box's own editing. These are commands a reader presses, so they are bound here
    // rather than hard-coded; typing a character is not, because it is the text itself.
    ClearQuery,
    DeleteBackward,
    DeleteForward,
    DeleteWordBackward,
    CursorLeft,
    CursorRight,
    CursorStart,
    CursorEnd,
}

/// Which mode an action is reachable from. Two actions may share a chord when their modes do
/// not overlap, which is why Esc can both leave the search box and quit the browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActionMode {
    Browse,
    Search,
    Both,
}

impl TuiAction {
    /// Every action, in the order `[ui.keys]` and the defaults list them.
    ///
    /// A plain constant, so unlike [`Self::mode`] and [`Self::name`] the compiler cannot ask for
    /// an entry when a variant is added. An action left out of it has no help entry and no
    /// shipped binding, and no test can discover that, because every way of enumerating the
    /// variants is this list. Add the action here first.
    pub(crate) const ALL: [Self; 28] = [
        Self::Interrupt,
        Self::Quit,
        Self::EnterSearch,
        Self::LeaveSearch,
        Self::MoveDown,
        Self::MoveUp,
        Self::PageDown,
        Self::PageUp,
        Self::Top,
        Self::Bottom,
        Self::CycleProvider,
        Self::CycleSessionKind,
        Self::CycleTimeWindow,
        Self::ToggleWarningsOnly,
        Self::PreviewScrollDown,
        Self::PreviewScrollUp,
        Self::PreviewPageDown,
        Self::PreviewPageUp,
        Self::Resume,
        Self::Help,
        Self::ClearQuery,
        Self::DeleteBackward,
        Self::DeleteForward,
        Self::DeleteWordBackward,
        Self::CursorLeft,
        Self::CursorRight,
        Self::CursorStart,
        Self::CursorEnd,
    ];

    /// Every arm is named rather than falling through a wildcard. A `_ => Browse` here read as a
    /// sensible default and behaved as a trap: an editing command added later would compile,
    /// become browse-only, and be unreachable from the search box it was written for, with
    /// nothing failing. Spelling the browse list out makes the compiler ask.
    pub(crate) fn mode(self) -> ActionMode {
        match self {
            Self::Interrupt => ActionMode::Both,
            Self::LeaveSearch
            | Self::ClearQuery
            | Self::DeleteBackward
            | Self::DeleteForward
            | Self::DeleteWordBackward
            | Self::CursorLeft
            | Self::CursorRight
            | Self::CursorStart
            | Self::CursorEnd => ActionMode::Search,
            Self::Quit
            | Self::EnterSearch
            | Self::MoveDown
            | Self::MoveUp
            | Self::PageDown
            | Self::PageUp
            | Self::Top
            | Self::Bottom
            | Self::CycleProvider
            | Self::CycleSessionKind
            | Self::CycleTimeWindow
            | Self::ToggleWarningsOnly
            | Self::PreviewScrollDown
            | Self::PreviewScrollUp
            | Self::PreviewPageDown
            | Self::PreviewPageUp
            | Self::Resume
            | Self::Help => ActionMode::Browse,
        }
    }

    /// The `[ui.keys]` name, from the same serde renaming the file is parsed with.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Interrupt => "interrupt",
            Self::Quit => "quit",
            Self::EnterSearch => "enter_search",
            Self::LeaveSearch => "leave_search",
            Self::MoveDown => "move_down",
            Self::MoveUp => "move_up",
            Self::PageDown => "page_down",
            Self::PageUp => "page_up",
            Self::Top => "top",
            Self::Bottom => "bottom",
            Self::CycleProvider => "cycle_provider",
            Self::CycleSessionKind => "cycle_session_kind",
            Self::CycleTimeWindow => "cycle_time_window",
            Self::ToggleWarningsOnly => "toggle_warnings_only",
            Self::PreviewScrollDown => "preview_scroll_down",
            Self::PreviewScrollUp => "preview_scroll_up",
            Self::PreviewPageDown => "preview_page_down",
            Self::PreviewPageUp => "preview_page_up",
            Self::Resume => "resume",
            Self::Help => "help",
            Self::ClearQuery => "clear_query",
            Self::DeleteBackward => "delete_backward",
            Self::DeleteForward => "delete_forward",
            Self::DeleteWordBackward => "delete_word_backward",
            Self::CursorLeft => "cursor_left",
            Self::CursorRight => "cursor_right",
            Self::CursorStart => "cursor_start",
            Self::CursorEnd => "cursor_end",
        }
    }
}

impl fmt::Display for TuiAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// The named keys `[ui.keys]` accepts beside a single character, paired with the crossterm code
/// they select. One table serves parsing, rendering, and the error message that lists them.
///
/// Function keys are deliberately absent: they are a numbered range, and listing four of them
/// here made `f7` a key this module would print but refuse to read back. [`function_key`] owns
/// the whole range for both directions instead.
const NAMED_KEYS: [(&str, KeyCode); 16] = [
    ("enter", KeyCode::Enter),
    ("esc", KeyCode::Esc),
    ("tab", KeyCode::Tab),
    ("backtab", KeyCode::BackTab),
    ("backspace", KeyCode::Backspace),
    ("delete", KeyCode::Delete),
    ("insert", KeyCode::Insert),
    ("space", KeyCode::Char(' ')),
    ("up", KeyCode::Up),
    ("down", KeyCode::Down),
    ("left", KeyCode::Left),
    ("right", KeyCode::Right),
    ("home", KeyCode::Home),
    ("end", KeyCode::End),
    ("pageup", KeyCode::PageUp),
    ("pagedown", KeyCode::PageDown),
];

/// The highest function key a terminal reports. Twelve is what a keyboard carries; the kitty
/// keyboard protocol numbers them to thirty-five, so accepting the whole range costs nothing and
/// keeps a chord this module prints readable back in.
const HIGHEST_FUNCTION_KEY: u8 = 35;

/// `f7` as [`KeyCode::F`], for the range [`fmt::Display`] writes out.
fn function_key(name: &str) -> Option<KeyCode> {
    let number = name.strip_prefix('f')?.parse::<u8>().ok()?;
    (1..=HIGHEST_FUNCTION_KEY)
        .contains(&number)
        .then_some(KeyCode::F(number))
}

/// The modifiers a chord may name, in the order [`KeyChord`] writes them back out.
const NAMED_MODIFIERS: [(&str, KeyModifiers); 4] = [
    ("ctrl", KeyModifiers::CONTROL),
    ("alt", KeyModifiers::ALT),
    ("shift", KeyModifiers::SHIFT),
    ("super", KeyModifiers::SUPER),
];

/// One key press: `q`, `/`, `ctrl+d`, `shift+tab`.
///
/// Shift is deliberately not part of matching a character chord. A terminal reports `G` as the
/// character `G`, with or without a shift flag depending on its keyboard protocol, so requiring
/// the flag would make `bottom = ["G"]` work on one terminal and not another. For a named key
/// there is no character to carry the distinction, so shift stays significant there.
///
/// Once another modifier is present the character's *case* becomes the same kind of protocol
/// difference. crossterm swaps in the shifted character and clears the shift flag when the
/// terminal reports kitty alternate keys, so Ctrl+Shift+C arrives as `Char('C')` with Control
/// there and as `Char('c')` with Control and Shift on a legacy terminal. A chorded letter is
/// therefore folded to lower case, which makes `ctrl+c` and `ctrl+C` one binding that answers on
/// both. A bare letter keeps its case, because there the case is the whole of what distinguishes
/// the shipped `g` from `G`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyChord {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl KeyChord {
    pub(crate) fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        let (code, modifiers) = Self::significant(code, modifiers);
        Self { code, modifiers }
    }

    /// The key code and modifiers that decide whether two presses are the same chord.
    fn significant(code: KeyCode, modifiers: KeyModifiers) -> (KeyCode, KeyModifiers) {
        let chorded = KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER;
        let KeyCode::Char(character) = code else {
            return (code, modifiers & (chorded | KeyModifiers::SHIFT));
        };
        let kept = modifiers & chorded;
        if kept.is_empty() {
            return (code, kept);
        }
        (KeyCode::Char(fold_case(character)), kept)
    }

    /// True when `event` is this chord. Key repeat and release events are the caller's business.
    pub(crate) fn matches(&self, event: &KeyEvent) -> bool {
        (self.code, self.modifiers) == Self::significant(event.code, event.modifiers)
    }
}

/// `character` in lower case, when lowering it yields one character. `İ` lowers to two, and a
/// chord is one key press, so such a character is left as the reader wrote it.
fn fold_case(character: char) -> char {
    let mut lowered = character.to_lowercase();
    match (lowered.next(), lowered.next()) {
        (Some(single), None) => single,
        _ => character,
    }
}

impl fmt::Display for KeyChord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (name, flag) in NAMED_MODIFIERS {
            if self.modifiers.contains(flag) {
                write!(formatter, "{name}+")?;
            }
        }
        match NAMED_KEYS.iter().find(|(_, code)| *code == self.code) {
            Some((name, _)) => formatter.write_str(name),
            None => match self.code {
                KeyCode::Char(character) => write!(formatter, "{character}"),
                KeyCode::F(number) => write!(formatter, "f{number}"),
                other => write!(formatter, "{other:?}"),
            },
        }
    }
}

/// The accepted key names, for an error message that tells the reader what to write instead.
fn accepted_key_names() -> String {
    let named = NAMED_KEYS
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ");
    format!("a single character, f1 through f{HIGHEST_FUNCTION_KEY}, or one of: {named}")
}

impl FromStr for KeyChord {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            bail!(
                "a key binding cannot be empty; write {}",
                accepted_key_names()
            );
        }
        let mut modifiers = KeyModifiers::NONE;
        let mut rest = trimmed;
        // `+` separates modifiers from the key, and the key itself may be `+`, so only split
        // while what remains still has a key after the separator.
        while let Some((head, tail)) = rest.split_once('+') {
            if tail.is_empty() {
                break;
            }
            let lowered = head.to_ascii_lowercase();
            match NAMED_MODIFIERS
                .iter()
                .find(|(name, _)| *name == lowered.as_str())
            {
                Some((_, flag)) => modifiers |= *flag,
                None => bail!(
                    "unknown key modifier {head:?} in {trimmed:?}; write one of ctrl, alt, \
                     shift, super, joined to the key with +"
                ),
            }
            rest = tail;
        }
        let lowered = rest.to_ascii_lowercase();
        if let Some((_, code)) = NAMED_KEYS
            .iter()
            .find(|(name, _)| *name == lowered.as_str())
        {
            return Ok(Self::new(*code, modifiers));
        }
        if let Some(code) = function_key(&lowered) {
            return Ok(Self::new(code, modifiers));
        }
        let mut characters = rest.chars();
        match (characters.next(), characters.next()) {
            (Some(character), None) => Ok(Self::new(KeyCode::Char(character), modifiers)),
            _ => bail!(
                "unknown key {rest:?} in {trimmed:?}; write {}",
                accepted_key_names()
            ),
        }
    }
}

impl<'de> Deserialize<'de> for KeyChord {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

impl Serialize for KeyChord {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

/// Every action's chords.
///
/// A `[ui.keys]` table names only the actions it changes: the rest keep their defaults, the same
/// way a `[providers.<name>]` table overrides only the fields it names. An action set to `[]` is
/// deliberately unbound.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct KeyBindings(BTreeMap<TuiAction, Vec<KeyChord>>);

impl KeyBindings {
    /// The chords bound to `action`.
    pub(crate) fn chords(&self, action: TuiAction) -> &[KeyChord] {
        self.0.get(&action).map_or(&[], Vec::as_slice)
    }

    /// Bind `chords` to `action`, keeping the first spelling of a chord the action already lists.
    ///
    /// Every binding is stored through here, so [`Self::validate`]'s conflict check only ever
    /// sees one chord per action and its message always names two different commands. Naming a
    /// key twice for one command is a repeat, not a conflict — it still means one thing — and it
    /// happens the moment two spellings fold together, as `ctrl+c` and `ctrl+C` now do.
    fn bind(&mut self, action: TuiAction, chords: impl IntoIterator<Item = KeyChord>) {
        let mut kept: Vec<KeyChord> = Vec::new();
        for chord in chords {
            if !kept.contains(&chord) {
                kept.push(chord);
            }
        }
        self.0.insert(action, kept);
    }

    /// The action `event` selects in `mode`, if any.
    pub(crate) fn action_for(&self, event: &KeyEvent, mode: ActionMode) -> Option<TuiAction> {
        self.0
            .iter()
            .filter(|(action, _)| reachable(action.mode(), mode))
            .find(|(_, chords)| chords.iter().any(|chord| chord.matches(event)))
            .map(|(action, _)| *action)
    }

    /// Reject a table that cannot be operated: an unknown spelling is already refused while
    /// parsing, so what is left is one chord meaning two things in one mode, and no way out.
    pub(crate) fn validate(&self) -> Result<()> {
        // A list rather than a map: crossterm's key types are not ordered, the table holds
        // tens of chords, and the scan keeps the message deterministic in action order.
        for mode in [ActionMode::Browse, ActionMode::Search] {
            let mut seen: Vec<(KeyChord, TuiAction)> = Vec::new();
            for (action, chords) in &self.0 {
                if !reachable(action.mode(), mode) {
                    continue;
                }
                for chord in chords {
                    if let Some((_, existing)) =
                        seen.iter().find(|(seen_chord, _)| seen_chord == chord)
                    {
                        bail!(
                            "ui.keys binds {chord} to both {existing} and {action}, which are \
                             both reachable from the same mode; give one of them another key"
                        );
                    }
                    seen.push((*chord, *action));
                }
            }
        }
        if self.chords(TuiAction::Quit).is_empty() && self.chords(TuiAction::Interrupt).is_empty() {
            bail!(
                "ui.keys leaves both quit and interrupt unbound, so the terminal UI could not \
                 be exited; bind at least one of them"
            );
        }
        Ok(())
    }
}

fn reachable(action: ActionMode, mode: ActionMode) -> bool {
    action == ActionMode::Both || mode == ActionMode::Both || action == mode
}

impl Default for KeyBindings {
    fn default() -> Self {
        let chord = |text: &str| text.parse::<KeyChord>().expect("a shipped default parses");
        let mut bindings = Self(BTreeMap::new());
        let mut bind = |action: TuiAction, keys: &[&str]| {
            bindings.bind(action, keys.iter().map(|key| chord(key)));
        };
        bind(TuiAction::Interrupt, &["ctrl+c"]);
        bind(TuiAction::Quit, &["q", "esc"]);
        bind(TuiAction::EnterSearch, &["/"]);
        bind(TuiAction::LeaveSearch, &["enter", "esc"]);
        bind(TuiAction::MoveDown, &["j", "down"]);
        bind(TuiAction::MoveUp, &["k", "up"]);
        bind(TuiAction::PageDown, &["pagedown"]);
        bind(TuiAction::PageUp, &["pageup"]);
        bind(TuiAction::Top, &["g"]);
        bind(TuiAction::Bottom, &["G"]);
        bind(TuiAction::CycleProvider, &["p"]);
        bind(TuiAction::CycleSessionKind, &["f"]);
        bind(TuiAction::CycleTimeWindow, &["s"]);
        bind(TuiAction::ToggleWarningsOnly, &["w"]);
        // Shifted vertical keys, not h/l: the preview is scrolled on the same axis as the list,
        // with shift saying "the preview rather than the list". `J`/`K` first because the status
        // bar shows an action's first chord and because shift+arrow does not survive every
        // terminal; see `the_preview_scrolls_on_the_vertical_axis_rather_than_on_h_and_l`.
        bind(TuiAction::PreviewScrollDown, &["J", "shift+down"]);
        bind(TuiAction::PreviewScrollUp, &["K", "shift+up"]);
        bind(TuiAction::PreviewPageDown, &["ctrl+d"]);
        bind(TuiAction::PreviewPageUp, &["ctrl+u"]);
        bind(TuiAction::Resume, &["enter", "r"]);
        bind(TuiAction::Help, &["?"]);
        bind(TuiAction::ClearQuery, &["ctrl+u"]);
        bind(TuiAction::DeleteBackward, &["backspace"]);
        bind(TuiAction::DeleteForward, &["delete"]);
        bind(TuiAction::DeleteWordBackward, &["ctrl+w"]);
        bind(TuiAction::CursorLeft, &["left"]);
        bind(TuiAction::CursorRight, &["right"]);
        bind(TuiAction::CursorStart, &["home", "ctrl+a"]);
        bind(TuiAction::CursorEnd, &["end", "ctrl+e"]);
        bindings
    }
}

impl<'de> Deserialize<'de> for KeyBindings {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        // Start from the defaults so a table naming one action does not silently unbind the
        // other eighteen. `TuiAction` deserializes from a closed set, so an action nobody
        // implements is refused here rather than accepted and ignored.
        let overrides = BTreeMap::<TuiAction, Vec<KeyChord>>::deserialize(deserializer)?;
        let mut bindings = Self::default();
        for (action, chords) in overrides {
            bindings.bind(action, chords);
        }
        Ok(bindings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shipped_default_round_trips_through_its_written_form() {
        // The file writes what `aise config show` prints, so a chord that parses one way and
        // prints another would make an edited configuration silently differ from the one shown.
        let bindings = KeyBindings::default();
        for action in TuiAction::ALL {
            for chord in bindings.chords(action) {
                let written = chord.to_string();
                let reparsed: KeyChord = written.parse().expect("a printed chord parses");
                assert_eq!(reparsed, *chord, "{action} wrote {written}");
            }
        }
    }

    #[test]
    fn every_action_has_a_default_binding() {
        let bindings = KeyBindings::default();
        for action in TuiAction::ALL {
            assert!(
                !bindings.chords(action).is_empty(),
                "{action} ships unbound, so nothing documents how to reach it"
            );
        }
    }

    #[test]
    fn a_character_chord_ignores_the_shift_a_terminal_may_or_may_not_report() {
        // `bottom = ["G"]` has to work whether the terminal reports SHIFT alongside the capital
        // or only the capital, which is the difference between keyboard protocols rather than
        // between key presses.
        let chord: KeyChord = "G".parse().unwrap();
        assert!(chord.matches(&KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE)));
        assert!(chord.matches(&KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT)));
        assert!(!chord.matches(&KeyEvent::new(KeyCode::Char('G'), KeyModifiers::CONTROL)));
        // A named key has no character to carry the distinction, so shift stays significant.
        let tab: KeyChord = "shift+tab".parse().unwrap();
        assert!(tab.matches(&KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT)));
        assert!(!tab.matches(&KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
    }

    #[test]
    fn the_preview_scrolls_on_the_vertical_axis_rather_than_on_h_and_l() {
        // `l` scrolled the preview down and `h` scrolled it up, so the key that means "right"
        // everywhere else moved a pane downwards. Two conventions say otherwise: in the
        // two-pane browsers h and l came from, they walk the hierarchy -- ranger.1 defines
        // "h, j, k, l  Move left, down, up or right" -- and in vim they are horizontal cursor
        // motion. fzf, the closest analogue to this screen, binds `preview-down` to shift-down
        // and `preview-up` to shift-up (fzf.1), so a shifted vertical key is the established
        // idiom for "the preview, not the list".
        //
        // The letters carry that idiom where the arrows cannot. macOS Terminal.app emits no
        // parameterized `CSI 1;2A` for shift+arrow and ignores `modifyOtherKeys`, so shift+up
        // alone would leave the preview unscrollable there; `J` and `K` are ordinary printable
        // characters every terminal transmits. They are also the same width in the status bar
        // as the pair they replace, which is one row and sheds hints at eighty columns.
        let bindings = KeyBindings::default();
        let press = |code, modifiers| {
            bindings.action_for(&KeyEvent::new(code, modifiers), ActionMode::Browse)
        };

        assert_eq!(
            press(KeyCode::Char('J'), KeyModifiers::NONE),
            Some(TuiAction::PreviewScrollDown)
        );
        assert_eq!(
            press(KeyCode::Char('K'), KeyModifiers::NONE),
            Some(TuiAction::PreviewScrollUp)
        );
        assert_eq!(
            press(KeyCode::Down, KeyModifiers::SHIFT),
            Some(TuiAction::PreviewScrollDown)
        );
        assert_eq!(
            press(KeyCode::Up, KeyModifiers::SHIFT),
            Some(TuiAction::PreviewScrollUp)
        );

        // The unshifted letters still move the list, so the capitals are a second binding rather
        // than a collision. A terminal reports a capital as the shifted character itself, which
        // is what makes this safe -- the same property `bottom = ["G"]` already relies on.
        assert_eq!(
            press(KeyCode::Char('j'), KeyModifiers::NONE),
            Some(TuiAction::MoveDown)
        );
        assert_eq!(
            press(KeyCode::Char('k'), KeyModifiers::NONE),
            Some(TuiAction::MoveUp)
        );

        // And the horizontal keys no longer move anything vertically.
        for code in [
            KeyCode::Char('h'),
            KeyCode::Char('l'),
            KeyCode::Left,
            KeyCode::Right,
        ] {
            assert_eq!(
                press(code, KeyModifiers::NONE),
                None,
                "{code:?} still reaches a command in browse mode"
            );
        }
    }

    #[test]
    fn a_chorded_letter_is_not_the_bare_letter() {
        // Every browse binding used to match on the character alone, so Ctrl+Q quit and Ctrl+S
        // moved the time window. Matching includes the modifiers, so the chords are distinct.
        let bindings = KeyBindings::default();
        let quit = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        let chorded_quit = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        assert_eq!(
            bindings.action_for(&quit, ActionMode::Browse),
            Some(TuiAction::Quit)
        );
        assert_eq!(bindings.action_for(&chorded_quit, ActionMode::Browse), None);
    }

    #[test]
    fn esc_and_enter_mean_different_things_in_the_two_modes() {
        let bindings = KeyBindings::default();
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            bindings.action_for(&esc, ActionMode::Browse),
            Some(TuiAction::Quit)
        );
        assert_eq!(
            bindings.action_for(&esc, ActionMode::Search),
            Some(TuiAction::LeaveSearch)
        );
        assert_eq!(
            bindings.action_for(&enter, ActionMode::Browse),
            Some(TuiAction::Resume)
        );
        assert_eq!(
            bindings.action_for(&enter, ActionMode::Search),
            Some(TuiAction::LeaveSearch)
        );
        bindings.validate().expect("the shipped table is operable");
    }

    #[test]
    fn interrupt_is_reachable_from_both_modes() {
        let bindings = KeyBindings::default();
        let interrupt = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        for mode in [ActionMode::Browse, ActionMode::Search] {
            assert_eq!(
                bindings.action_for(&interrupt, mode),
                Some(TuiAction::Interrupt)
            );
        }
    }

    #[test]
    fn every_function_key_reads_back_the_way_it_is_written() {
        // Four of them used to be listed by name while `Display` wrote out the whole range, so
        // `f7` was a chord this module printed and then refused to parse.
        for number in 1..=HIGHEST_FUNCTION_KEY {
            let written = format!("f{number}");
            let chord: KeyChord = written.parse().unwrap_or_else(|error| {
                panic!("{written} did not parse: {error}");
            });
            assert!(chord.matches(&KeyEvent::new(KeyCode::F(number), KeyModifiers::NONE)));
            assert_eq!(chord.to_string(), written);
        }
        assert!("f0".parse::<KeyChord>().is_err());
        assert!(format!("f{}", u16::from(HIGHEST_FUNCTION_KEY) + 1)
            .parse::<KeyChord>()
            .is_err());
    }

    #[test]
    fn an_unknown_key_name_says_what_to_write_instead() {
        let error = "ctrl+nope".parse::<KeyChord>().unwrap_err().to_string();
        assert!(error.contains("unknown key"), "{error}");
        assert!(error.contains("single character"), "{error}");
        assert!(error.contains("pagedown"), "{error}");

        let modifier = "hyper+c".parse::<KeyChord>().unwrap_err().to_string();
        assert!(modifier.contains("unknown key modifier"), "{modifier}");
        assert!(modifier.contains("ctrl"), "{modifier}");
    }

    #[test]
    fn a_chord_may_itself_be_the_plus_key() {
        let plus: KeyChord = "+".parse().unwrap();
        assert!(plus.matches(&KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE)));
        let chorded: KeyChord = "ctrl++".parse().unwrap();
        assert!(chorded.matches(&KeyEvent::new(KeyCode::Char('+'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn one_chord_bound_to_two_actions_in_one_mode_is_refused() {
        let mut bindings = KeyBindings::default();
        bindings.bind(TuiAction::Top, ["q".parse().unwrap()]);
        let error = bindings.validate().unwrap_err().to_string();
        assert!(error.contains("ui.keys binds q"), "{error}");
        assert!(error.contains("quit"), "{error}");
        assert!(error.contains("top"), "{error}");
    }

    #[test]
    fn a_table_with_no_way_out_is_refused() {
        let mut bindings = KeyBindings::default();
        bindings.bind(TuiAction::Quit, []);
        bindings.bind(TuiAction::Interrupt, []);
        let error = bindings.validate().unwrap_err().to_string();
        assert!(error.contains("could not be exited"), "{error}");
    }

    #[test]
    fn a_table_naming_one_action_keeps_the_other_defaults() {
        // Replacing the whole table on a partial override is the container failure this exists
        // to avoid: someone rebinding quit would otherwise lose every other key.
        let bindings: KeyBindings = toml::from_str("quit = [\"x\"]").unwrap();
        assert_eq!(
            bindings.chords(TuiAction::Quit),
            &["x".parse::<KeyChord>().unwrap()]
        );
        assert_eq!(
            bindings.chords(TuiAction::MoveDown),
            KeyBindings::default().chords(TuiAction::MoveDown)
        );
    }

    #[test]
    fn an_action_nobody_implements_is_refused_rather_than_ignored() {
        let error = toml::from_str::<KeyBindings>("teleport = [\"t\"]")
            .unwrap_err()
            .to_string();
        assert!(error.contains("teleport"), "{error}");
    }

    #[test]
    fn an_action_may_be_reached_by_as_many_keys_as_the_reader_writes() {
        // The point of a list per action: a reader who wants their own key does not have to give
        // up the shipped one, and several aliases for one command are ordinary rather than a
        // conflict. Only two chords per action ship, so without this nothing tested more.
        let bindings: KeyBindings =
            toml::from_str("quit = [\"q\", \"esc\", \"ctrl+q\", \"f1\", \"x\"]").unwrap();
        bindings.validate().expect("aliases are not a conflict");
        for (code, modifiers) in [
            (KeyCode::Char('q'), KeyModifiers::NONE),
            (KeyCode::Esc, KeyModifiers::NONE),
            (KeyCode::Char('q'), KeyModifiers::CONTROL),
            (KeyCode::F(1), KeyModifiers::NONE),
            (KeyCode::Char('x'), KeyModifiers::NONE),
        ] {
            assert_eq!(
                bindings.action_for(&KeyEvent::new(code, modifiers), ActionMode::Browse),
                Some(TuiAction::Quit),
                "{code:?} with {modifiers:?} did not reach quit"
            );
        }
    }

    #[test]
    fn naming_one_key_twice_for_one_action_is_kept_once_rather_than_refused() {
        // Listing a key an action already has is a duplicate, not a conflict: it still means one
        // thing. The check reported `binds q to both quit and quit`, naming one action twice and
        // asking the reader to give one of them another key.
        let bindings: KeyBindings = toml::from_str("quit = [\"q\", \"esc\", \"q\"]").unwrap();
        bindings
            .validate()
            .expect("one action listing a key twice still means one thing");
        assert_eq!(
            bindings.chords(TuiAction::Quit),
            &[
                "q".parse::<KeyChord>().unwrap(),
                "esc".parse::<KeyChord>().unwrap()
            ],
            "the repeat should be dropped, so the help does not print `q, esc, q`"
        );
    }

    #[test]
    fn a_modified_letter_is_the_same_chord_whichever_case_the_terminal_reports() {
        // crossterm replaces the key code with the shifted character and clears SHIFT when the
        // terminal reports kitty alternate keys (crossterm 0.29 parse.rs:598-606), so Ctrl+Shift+C
        // arrives as Char('C')+CONTROL there and as Char('c')+CONTROL|SHIFT on a legacy terminal.
        // Keeping the case significant would make one binding work on one terminal only, which is
        // the failure the shift rule above already refuses for a bare character.
        let lower: KeyChord = "ctrl+c".parse().unwrap();
        let upper: KeyChord = "ctrl+C".parse().unwrap();
        assert_eq!(lower, upper, "ctrl+c and ctrl+C name one key press");
        for chord in [lower, upper] {
            assert!(chord.matches(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
            assert!(chord.matches(&KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )));
            assert!(chord.matches(&KeyEvent::new(KeyCode::Char('C'), KeyModifiers::CONTROL)));
        }
        // Unmodified letters keep their case: `g` and `G` are the shipped top and bottom keys.
        let top: KeyChord = "g".parse().unwrap();
        assert!(!top.matches(&KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE)));
    }

    #[test]
    fn an_explicitly_empty_action_is_unbound() {
        let bindings: KeyBindings = toml::from_str("cycle_provider = []").unwrap();
        assert!(bindings.chords(TuiAction::CycleProvider).is_empty());
        let provider = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE);
        assert_eq!(bindings.action_for(&provider, ActionMode::Browse), None);
        bindings
            .validate()
            .expect("unbinding one action is allowed");
    }
}
