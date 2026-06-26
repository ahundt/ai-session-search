//! Flexible date / EDTF / duration / natural-language parsing for the `--since`,
//! `--until`, and `--when` filters. This mirrors aise's `parse_date_input`
//! (ai_session_tools/engine.py:48-211) but offloads the genuinely hard parts to
//! battle-tested libraries instead of hand-rolling a grammar:
//!
//!   * **EDTF** (`2026`, `2026-01`, `202X`, `19XX`, `2026-XX`, `2026-01-XX`,
//!     intervals `A/B`, full datetimes) → the [`edtf`] crate. Its [`edtf::level_1::Precision`]
//!     classifies the value (Century/Decade/Year/Month/Day + the `XX`-unspecified
//!     variants) and we expand that to instant bounds with `chrono`.
//!   * **Natural language** (`yesterday`, `3 days ago`, `last friday`) → the
//!     [`interim`] crate (a `chrono-english` fork), matching aise's `dateutil` fallback.
//!     Resolved NLP is widened to its whole UTC day (see below), so a time-of-day in the
//!     phrase (e.g. `8pm`) is intentionally ignored — use an ISO datetime for second
//!     precision.
//!   * **ISO datetimes** (full and partial-precision `…T14`, `…T14:30`) and the
//!     **aise duration shorthand** (`7d 2w 1m 24h 30min 1y`) → `chrono` arithmetic;
//!     these are not EDTF and not standard NLP.
//!
//! Every input resolves to a `(start, end)` pair of UTC instants:
//!   * a fuzzy period (date, month, year, decade, century, unspecified-digit day) →
//!     `[first instant, last instant]` of the period;
//!   * a duration (`7d`, `2w`, …) → the window `[now - delta, now]`, so `--when 7d`
//!     means "the last 7 days";
//!   * relative natural language (`yesterday`, `3 days ago`) → its whole UTC day, so
//!     `--when yesterday` matches the same span as `--when <that date>`;
//!   * a full second-precision datetime → `start == end` (an explicit instant);
//!   * an EDTF interval `A/B` → `(start-of-A, end-of-B)`.
//!
//! `--since` takes the start, `--until` takes the end, `--when` takes both.
//!
//! ## Why not SQLite's date functions?
//! rusqlite's bundled SQLite ships excellent `date()`/`datetime()`/`strftime()`
//! helpers with modifiers (`'start of month'`, `'-7 days'`) that cover the ISO and
//! relative subset and compute period bounds with correct leap-year handling. They
//! were evaluated and intentionally not used for *parsing* here because (a) they
//! cannot parse EDTF unspecified-digit values (`202X`, `2026-01-1X`) or
//! natural-language input, and (b) routing parse through SQL would couple this pure,
//! unit-testable module to a live `Connection`. `chrono` performs the identical
//! bound arithmetic in-process. The existing lexicographic `ts >= ?` comparison in
//! `db.rs` (uniform `+00:00` rfc3339) remains the storage-side filter.

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use clap::Args;
use edtf::level_1::{Date as EdtfDate, Edtf, Precision, Season};
use interim::{Dialect, parse_date_string};
use regex::Regex;
use std::sync::OnceLock;

/// Inclusive lower (`Start`) or upper (`End`) instant a fuzzy period collapses to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    Start,
    End,
}

/// Resolved `(since, until)` filter bounds; either side `None` means "unbounded".
pub type Bounds = (Option<DateTime<Utc>>, Option<DateTime<Utc>>);

/// Shared `--since/--until/--when` flags. Flattened into each command's args so the
/// date surface is defined once (DRY). `--when` is a single period used as *both*
/// bounds and is mutually exclusive with `--since`/`--until` (enforced by clap).
#[derive(Debug, Clone, Default, Args)]
pub struct DateRange {
    /// Lower time bound, inclusive. Accepts EDTF / ISO / duration / natural language
    /// (e.g. `2026-01-15`, `2026-01`, `202X`, `7d`, `yesterday`). See `sessiongrep dates`.
    #[arg(long)]
    pub since: Option<String>,
    /// Upper time bound, inclusive. Same formats as `--since`; periods resolve to
    /// their last instant (e.g. `--until 2026-01` ends at `2026-01-31T23:59:59`).
    #[arg(long)]
    pub until: Option<String>,
    /// A single period used as BOTH bounds (e.g. `2026-01`, `202X`, `2026-01/2026-03`).
    #[arg(long, conflicts_with_all = ["since", "until"])]
    pub when: Option<String>,
}

impl DateRange {
    /// Resolve the flags to `(since, until)` UTC instants relative to `now`.
    pub fn resolve(&self, now: DateTime<Utc>) -> Result<Bounds> {
        if let Some(when) = self.when.as_deref() {
            let (start, end) = parse_span(when, now).map_err(|err| anyhow!("invalid --when '{when}': {err}"))?;
            return Ok((Some(start), Some(end)));
        }
        let since = self
            .since
            .as_deref()
            .map(|raw| parse_bound(raw, Bound::Start, now).map_err(|err| anyhow!("invalid --since '{raw}': {err}")))
            .transpose()?;
        let until = self
            .until
            .as_deref()
            .map(|raw| parse_bound(raw, Bound::End, now).map_err(|err| anyhow!("invalid --until '{raw}': {err}")))
            .transpose()?;
        Ok((since, until))
    }

    /// Resolve relative to the wall clock. Used by command handlers.
    pub fn resolve_now(&self) -> Result<Bounds> {
        self.resolve(Utc::now())
    }
}

/// Parse one token to a single bound (start or end of its period).
pub fn parse_bound(input: &str, bound: Bound, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let (start, end) = parse_span(input, now)?;
    Ok(match bound {
        Bound::Start => start,
        Bound::End => end,
    })
}

/// Parse one token to its full `(start, end)` instant span.
pub fn parse_span(input: &str, now: DateTime<Utc>) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    let s = input.trim();
    if s.is_empty() {
        bail!("empty date (try 2026-01-15, 2026-01, 202X, 7d, yesterday — run `sessiongrep dates`)");
    }

    // 1. Duration shorthand (7d 2w 1m 24h 30min 1y) — aise-specific, not EDTF/NLP.
    //    Resolves to the window [now - delta, now]: `--since 7d` starts 7 days back,
    //    `--until 7d` ends now, and `--when 7d` covers the last 7 days. Built with
    //    checked arithmetic so an absurd magnitude errors cleanly instead of panicking
    //    chrono's TimeDelta.
    if let Some(caps) = duration_re().captures(s) {
        let n: i64 = caps[1].parse().map_err(|_| anyhow!("duration number too large in '{s}'"))?;
        let delta = match caps[2].to_ascii_lowercase().as_str() {
            "d" => Duration::try_days(n),
            "w" => Duration::try_weeks(n),
            "h" => Duration::try_hours(n),
            "min" => Duration::try_minutes(n),
            "m" => n.checked_mul(30).and_then(Duration::try_days), // 1m ≈ 30 days, matching aise
            "y" => n.checked_mul(365).and_then(Duration::try_days), // 1y ≈ 365 days, matching aise
            other => bail!("unsupported duration unit '{other}'"),
        }
        .ok_or_else(|| anyhow!("duration '{s}' is out of range"))?;
        let start = now
            .checked_sub_signed(delta)
            .ok_or_else(|| anyhow!("duration '{s}' is out of range"))?;
        return Ok((start, now));
    }

    // EDTF needs uppercase `X`; only uppercase purely date-shaped tokens so NLP
    // strings ("yesterday", "last friday") keep their original casing.
    let normalized = if datelike_re().is_match(s) { s.to_ascii_uppercase() } else { s.to_string() };
    let s = normalized.as_str();

    // 2. Partial-precision datetime (YYYY-MM-DDTHH or …THH:MM) — EDTF rejects these.
    if let Some(caps) = partial_dt_re().captures(s) {
        let date_hh = &caps[1]; // "2026-01-15T14"
        let (start_s, end_s) = match caps.get(2).map(|m| m.as_str()) {
            None => (format!("{date_hh}:00:00"), format!("{date_hh}:59:59")),
            Some(mm) => (format!("{date_hh}:{mm}:00"), format!("{date_hh}:{mm}:59")),
        };
        return Ok((parse_iso_dt(&start_s)?, parse_iso_dt(&end_s)?));
    }

    // 3. Single-digit-unspecified day (2026-01-1X, 2026-01-X5, 2026-01-XX) — the
    //    edtf crate parses whole-day `-XX` but rejects partial-digit days, so we
    //    expand these exactly like aise (engine.py:140-164).
    if let Some(caps) = day_x_re().captures(s) {
        let tens = caps[3].chars().next().unwrap_or('X');
        let units = caps[4].chars().next().unwrap_or('X');
        if tens == 'X' || units == 'X' {
            let year: i32 = caps[1].parse()?;
            let month: u32 = caps[2].parse()?;
            return day_x_span(year, month, tens, units);
        }
    }

    // 4. EDTF: dates, months, years, decades, centuries, XX-unspecified, intervals,
    //    full datetimes. The library does the parsing; Precision drives bound math.
    if let Ok(edtf) = Edtf::parse(s) {
        return match edtf {
            Edtf::Date(date) => date_bounds(&date),
            Edtf::Interval(a, b) => Ok((date_bounds(&a)?.0, date_bounds(&b)?.1)),
            Edtf::DateTime(dt) => {
                let instant = dt.to_chrono(&Utc);
                Ok((instant, instant))
            }
            // YYear (>4 digit years) and open intervals are out of scope here.
            _ => bail!("unsupported EDTF form '{s}'"),
        };
    }

    // 5. Natural language ("yesterday", "3 days ago", "last friday"). These are
    //    day-granular, so expand the resolved instant to its full UTC calendar day:
    //    `--when yesterday` then covers the whole day (consistent with `--when
    //    2026-06-24`) instead of a zero-width instant, while `--since`/`--until` take
    //    that day's start/end.
    if let Ok(instant) = parse_date_string(input.trim(), now, Dialect::Us) {
        return day_span_of(instant);
    }

    bail!(
        "unrecognised date/time '{input}' — try 2026-01-15, 2026-01, 2026, 202X, 2026-01-1X, \
         2026-01/2026-03, 7d, 2w, 1m, 24h, 1y, yesterday, '3 days ago' (run `sessiongrep dates`)"
    )
}

/// Expand an [`EdtfDate`] to inclusive `(start, end)` instants from its precision.
fn date_bounds(date: &EdtfDate) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    match date.precision() {
        Precision::Century(y) => Ok((start_of_year(y)?, end_of_year(y + 99)?)),
        Precision::Decade(y) => Ok((start_of_year(y)?, end_of_year(y + 9)?)),
        Precision::Year(y) | Precision::MonthOfYear(y) | Precision::DayOfYear(y) => {
            Ok((start_of_year(y)?, end_of_year(y)?))
        }
        Precision::Month(y, m) | Precision::DayOfMonth(y, m) => month_span(y, m),
        Precision::Day(y, m, d) => day_span(y, m, d),
        Precision::Season(y, season) => season_span(y, season),
    }
}

/// aise day-X expansion (engine.py:146-164): `XX`→whole month, `1X`→10..19,
/// `X5`→5..25, each clamped to the month's real length.
fn day_x_span(year: i32, month: u32, tens: char, units: char) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    let max_day = days_in_month(year, month)?;
    let (lo, hi) = if tens == 'X' && units == 'X' {
        (1, max_day)
    } else if units == 'X' {
        // tens fixed, units unspecified: "1X" = 10..19
        let t = tens.to_digit(10).unwrap_or(0);
        ((t * 10).max(1), (t * 10 + 9).min(max_day))
    } else {
        // tens unspecified, units fixed: span the first..last in-month day ending in that
        // digit, e.g. "X0" → 10..30 (or 10..20 in a 28/29-day month), "X1" → 1..31.
        let u = units.to_digit(10).unwrap_or(0);
        let lo = (1..=max_day).find(|d| d % 10 == u).unwrap_or(1);
        let hi = (1..=max_day).rev().find(|d| d % 10 == u).unwrap_or(lo);
        (lo, hi)
    };
    Ok((at(year, month, lo, 0, 0, 0)?, at(year, month, hi, 23, 59, 59)?))
}

fn season_span(year: i32, season: Season) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    // Meteorological-ish bounds; Winter spans into the next calendar year.
    let (s, e) = match season {
        Season::Spring => (month_span(year, 3)?.0, month_span(year, 5)?.1),
        Season::Summer => (month_span(year, 6)?.0, month_span(year, 8)?.1),
        Season::Autumn => (month_span(year, 9)?.0, month_span(year, 11)?.1),
        Season::Winter => (month_span(year, 12)?.0, month_span(year + 1, 2)?.1),
    };
    Ok((s, e))
}

// ── small chrono helpers ────────────────────────────────────────────────────

fn at(year: i32, month: u32, day: u32, h: u32, mi: u32, s: u32) -> Result<DateTime<Utc>> {
    Utc.with_ymd_and_hms(year, month, day, h, mi, s)
        .single()
        .ok_or_else(|| anyhow!("invalid date {year:04}-{month:02}-{day:02}T{h:02}:{mi:02}:{s:02}"))
}

fn day_span(year: i32, month: u32, day: u32) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    Ok((at(year, month, day, 0, 0, 0)?, at(year, month, day, 23, 59, 59)?))
}

/// Inclusive `(start, end)` of the UTC calendar day containing `dt`. Used to make
/// relative natural-language input day-granular.
fn day_span_of(dt: DateTime<Utc>) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    let date = dt.date_naive();
    day_span(date.year(), date.month(), date.day())
}

fn month_span(year: i32, month: u32) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    let last = days_in_month(year, month)?;
    Ok((at(year, month, 1, 0, 0, 0)?, at(year, month, last, 23, 59, 59)?))
}

fn start_of_year(year: i32) -> Result<DateTime<Utc>> {
    at(year, 1, 1, 0, 0, 0)
}

fn end_of_year(year: i32) -> Result<DateTime<Utc>> {
    at(year, 12, 31, 23, 59, 59)
}

fn days_in_month(year: i32, month: u32) -> Result<u32> {
    let (ny, nm) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    let first_next =
        NaiveDate::from_ymd_opt(ny, nm, 1).ok_or_else(|| anyhow!("invalid month {year}-{month:02}"))?;
    let last = first_next
        .pred_opt()
        .ok_or_else(|| anyhow!("date underflow computing month length"))?;
    Ok(last.day())
}

fn parse_iso_dt(s: &str) -> Result<DateTime<Utc>> {
    let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
        .map_err(|err| anyhow!("invalid datetime '{s}': {err}"))?;
    Ok(naive.and_utc())
}

// ── cached regexes (compiled once) ──────────────────────────────────────────

fn duration_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^(\d+)(min|[dwmhy])$").expect("valid duration regex"))
}

fn datelike_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[0-9XxTt:/.\-]+$").expect("valid datelike regex"))
}

fn partial_dt_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\d{4}-\d{2}-\d{2}T\d{2})(?::(\d{2}))?$").expect("valid partial regex"))
}

fn day_x_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\d{4})-(\d{2})-([0-3X])([0-9X])$").expect("valid day-x regex"))
}

/// Human-readable format reference for the `dates` command.
pub fn format_reference() -> String {
    [
        "sessiongrep date/time formats (--since, --until, --when)",
        "",
        "ISO 8601:",
        "  2026-01-15T14:30:25   exact second",
        "  2026-01-15T14:30      minute precision (expands to :00..:59)",
        "  2026-01-15T14         hour precision (expands to :00:00..:59:59)",
        "  2026-01-15            day        → 00:00:00 .. 23:59:59",
        "  2026-01               month      → first .. last day",
        "  2026                  year       → Jan 1 .. Dec 31",
        "",
        "EDTF (Extended Date/Time Format, via the `edtf` crate):",
        "  202X                  decade     → 2020-01-01 .. 2029-12-31",
        "  19XX                  century    → 1900-01-01 .. 1999-12-31",
        "  2026-XX               unspecified month → whole year",
        "  2026-01-XX            unspecified day   → whole month",
        "  2026-01-1X            partial day digit → 2026-01-10 .. 2026-01-19",
        "  2026-01/2026-03       interval   → start of A .. end of B",
        "",
        "Duration shorthand (the window [now - N, now]):",
        "  7d 2w 1m 24h 30min 1y   (m ≈ 30 days, y ≈ 365 days)",
        "",
        "Natural language (via the `interim` crate; day-granular):",
        "  today  yesterday  \"3 days ago\"  \"last friday\"",
        "",
        "--since uses a period's start, --until its end, --when uses both.",
        "A duration spans [now - N, now]; a relative word spans its whole day,",
        "so `--when 7d` = the last 7 days and `--when yesterday` = all of yesterday.",
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixed reference instant so duration/NLP cases are deterministic.
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 25, 12, 0, 0).unwrap()
    }

    fn span(s: &str) -> (DateTime<Utc>, DateTime<Utc>) {
        parse_span(s, now()).unwrap_or_else(|e| panic!("parse_span({s:?}) failed: {e}"))
    }

    fn iso(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    // ── Family 1: duration shorthand ────────────────────────────────────────
    #[test]
    fn durations_are_a_window_ending_now() {
        // A duration is the window [now - delta, now], so `--since 7d` starts 7 days
        // back and `--when 7d` covers the last 7 days (not a zero-width instant).
        assert_eq!(span("7d"), (now() - Duration::days(7), now()));
        assert_eq!(span("2w"), (now() - Duration::weeks(2), now()));
        assert_eq!(span("24h"), (now() - Duration::hours(24), now()));
        assert_eq!(span("30min"), (now() - Duration::minutes(30), now()));
        assert_eq!(span("1m"), (now() - Duration::days(30), now())); // m ≈ 30 days
        assert_eq!(span("1y"), (now() - Duration::days(365), now())); // y ≈ 365 days
        // The window is non-empty: start strictly before end.
        let (start, end) = span("7d");
        assert!(start < end);
        // Case-insensitive unit.
        assert_eq!(span("24H"), (now() - Duration::hours(24), now()));
    }

    #[test]
    fn oversized_duration_errors_without_panicking() {
        // 9.2e18 days would overflow chrono's TimeDelta — must be a clean error.
        assert!(parse_span("9999999999999999999d", now()).is_err());
        assert!(parse_span("999999999999y", now()).is_err());
    }

    // ── Family 2: full + partial ISO datetime ───────────────────────────────
    #[test]
    fn full_datetime_is_exact_second() {
        let (s, e) = span("2026-01-15T14:30:25");
        assert_eq!(s, iso("2026-01-15T14:30:25+00:00"));
        assert_eq!(s, e);
    }

    #[test]
    fn partial_datetime_expands_by_precision() {
        let (s, e) = span("2026-01-15T14");
        assert_eq!(s, iso("2026-01-15T14:00:00+00:00"));
        assert_eq!(e, iso("2026-01-15T14:59:59+00:00"));
        let (s, e) = span("2026-01-15T14:30");
        assert_eq!(s, iso("2026-01-15T14:30:00+00:00"));
        assert_eq!(e, iso("2026-01-15T14:30:59+00:00"));
    }

    // ── Family 3: calendar date / month / year ──────────────────────────────
    #[test]
    fn day_month_year_periods() {
        assert_eq!(span("2026-01-15"), (iso("2026-01-15T00:00:00+00:00"), iso("2026-01-15T23:59:59+00:00")));
        assert_eq!(span("2026-01"), (iso("2026-01-01T00:00:00+00:00"), iso("2026-01-31T23:59:59+00:00")));
        assert_eq!(span("2026"), (iso("2026-01-01T00:00:00+00:00"), iso("2026-12-31T23:59:59+00:00")));
    }

    // ── Family 4: EDTF unspecified digits ───────────────────────────────────
    #[test]
    fn edtf_decade_and_century() {
        assert_eq!(span("202X"), (iso("2020-01-01T00:00:00+00:00"), iso("2029-12-31T23:59:59+00:00")));
        assert_eq!(span("19XX"), (iso("1900-01-01T00:00:00+00:00"), iso("1999-12-31T23:59:59+00:00")));
        // Lowercase x is normalized to uppercase before EDTF parsing.
        assert_eq!(span("202x"), span("202X"));
    }

    #[test]
    fn edtf_unspecified_month_and_day() {
        // Whole-year and whole-month via -XX.
        assert_eq!(span("2026-XX"), span("2026"));
        assert_eq!(span("2026-01-XX"), span("2026-01"));
        // Partial-digit day (edtf rejects → aise workaround).
        assert_eq!(span("2026-01-1X"), (iso("2026-01-10T00:00:00+00:00"), iso("2026-01-19T23:59:59+00:00")));
        // "3X" clamps the upper day to the real month length (Jan → 31).
        assert_eq!(span("2026-01-3X"), (iso("2026-01-30T00:00:00+00:00"), iso("2026-01-31T23:59:59+00:00")));
    }

    #[test]
    fn edtf_units_fixed_day_reaches_last_matching_day() {
        // tens unspecified, units fixed: the span must reach the LAST in-month day with
        // that units digit, not stop ~20. "X0" → 10,20,30; "X1" → 1,11,21,31.
        assert_eq!(span("2026-01-X0"), (iso("2026-01-10T00:00:00+00:00"), iso("2026-01-30T23:59:59+00:00")));
        assert_eq!(span("2026-01-X1"), (iso("2026-01-01T00:00:00+00:00"), iso("2026-01-31T23:59:59+00:00")));
        // Unchanged middle digit (regression guard): "X5" → 5,15,25.
        assert_eq!(span("2026-01-X5"), (iso("2026-01-05T00:00:00+00:00"), iso("2026-01-25T23:59:59+00:00")));
        // Clamped to a short month: Feb 2026 has 28 days, so "X0" → 10,20 (no day 30).
        assert_eq!(span("2026-02-X0"), (iso("2026-02-10T00:00:00+00:00"), iso("2026-02-20T23:59:59+00:00")));
    }

    // ── Family 5: EDTF interval ─────────────────────────────────────────────
    #[test]
    fn edtf_interval_uses_start_of_a_end_of_b() {
        assert_eq!(span("2026-01/2026-03"), (iso("2026-01-01T00:00:00+00:00"), iso("2026-03-31T23:59:59+00:00")));
    }

    // ── Family 6: natural language ──────────────────────────────────────────
    /// Inclusive `[00:00:00, 23:59:59]` span of the UTC day containing `dt`.
    fn day_of(dt: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
        let d = dt.date_naive();
        (
            d.and_hms_opt(0, 0, 0).unwrap().and_utc(),
            d.and_hms_opt(23, 59, 59).unwrap().and_utc(),
        )
    }

    #[test]
    fn nlp_relative_dates_span_the_whole_day() {
        // Relative NLP is day-granular: it expands to the full day so `--when yesterday`
        // covers all of yesterday (consistent with `--when 2026-06-24`), not a
        // zero-width instant.
        assert_eq!(span("today"), day_of(now()));
        assert_eq!(span("yesterday"), day_of(now() - Duration::days(1)));
        assert_eq!(span("3 days ago"), day_of(now() - Duration::days(3)));
        // Non-empty day.
        let (s, e) = span("yesterday");
        assert!(s < e);
    }

    // ── Edge cases (#67) ────────────────────────────────────────────────────
    #[test]
    fn leap_year_february_bounds() {
        // 2024 is a leap year (29 days); 2026 is not (28).
        assert_eq!(span("2024-02").1, iso("2024-02-29T23:59:59+00:00"));
        assert_eq!(span("2026-02").1, iso("2026-02-28T23:59:59+00:00"));
        // -XX day on Feb respects the real length.
        assert_eq!(span("2024-02-XX").1, iso("2024-02-29T23:59:59+00:00"));
    }

    #[test]
    fn month_end_bounds() {
        assert_eq!(span("2026-04").1, iso("2026-04-30T23:59:59+00:00")); // 30-day month
        assert_eq!(span("2026-12").1, iso("2026-12-31T23:59:59+00:00")); // 31-day month
    }

    #[test]
    fn invalid_inputs_error() {
        assert!(parse_span("", now()).is_err());
        assert!(parse_span("2026-13", now()).is_err()); // month out of range
        assert!(parse_span("2026-02-30", now()).is_err()); // Feb 30 invalid
        assert!(parse_span("not-a-date-at-all-xyz", now()).is_err());
    }

    #[test]
    fn unspecified_tens_digit_day() {
        // "X5" = the days whose units digit is 5 → aise spans 5..25 (clamped to month).
        assert_eq!(
            span("2026-01-X5"),
            (iso("2026-01-05T00:00:00+00:00"), iso("2026-01-25T23:59:59+00:00"))
        );
        // "XX" = the whole month.
        assert_eq!(span("2026-01-XX"), span("2026-01"));
    }

    #[test]
    fn span_start_never_after_end_for_well_formed_periods() {
        // Ordering invariant across every well-formed form/family.
        for input in [
            "7d", "2w", "24h", "30min", "1m", "1y",
            "2026-01-15T14:30:25", "2026-01-15T14", "2026-01-15T14:30",
            "2026-01-15", "2026-01", "2026",
            "202X", "19XX", "2026-XX", "2026-01-XX", "2026-01-1X", "2026-01-X5",
            "2026-01/2026-03",
            "today", "yesterday", "3 days ago",
        ] {
            let (s, e) = span(input);
            assert!(s <= e, "{input}: start {s} must be <= end {e}");
        }
    }

    #[test]
    fn reversed_interval_yields_start_after_end_without_panicking() {
        // A backwards EDTF interval is accepted (no panic); it just yields an empty
        // downstream filter (start > end).
        let (s, e) = span("2026-03/2026-01");
        assert!(s > e);
    }

    #[test]
    fn bound_selects_start_or_end() {
        assert_eq!(parse_bound("2026-01", Bound::Start, now()).unwrap(), iso("2026-01-01T00:00:00+00:00"));
        assert_eq!(parse_bound("2026-01", Bound::End, now()).unwrap(), iso("2026-01-31T23:59:59+00:00"));
    }

    // ── DateRange resolution ────────────────────────────────────────────────
    #[test]
    fn daterange_since_until_when() {
        let r = DateRange { since: Some("2026-01".into()), until: Some("2026-03".into()), when: None };
        let (since, until) = r.resolve(now()).unwrap();
        assert_eq!(since.unwrap(), iso("2026-01-01T00:00:00+00:00"));
        assert_eq!(until.unwrap(), iso("2026-03-31T23:59:59+00:00"));

        // --when applies one period to both bounds.
        let r = DateRange { since: None, until: None, when: Some("2026".into()) };
        let (since, until) = r.resolve(now()).unwrap();
        assert_eq!(since.unwrap(), iso("2026-01-01T00:00:00+00:00"));
        assert_eq!(until.unwrap(), iso("2026-12-31T23:59:59+00:00"));

        // Empty range resolves to (None, None).
        let (since, until) = DateRange::default().resolve(now()).unwrap();
        assert!(since.is_none() && until.is_none());
    }

    #[test]
    fn daterange_when_interval() {
        let r = DateRange { since: None, until: None, when: Some("2026-01/2026-03".into()) };
        let (since, until) = r.resolve(now()).unwrap();
        assert_eq!(since.unwrap(), iso("2026-01-01T00:00:00+00:00"));
        assert_eq!(until.unwrap(), iso("2026-03-31T23:59:59+00:00"));
    }

    #[test]
    fn daterange_when_duration_is_the_last_n_window() {
        // Regression: `--when 7d` used to resolve to a zero-width instant and matched
        // nothing. It must now span the last 7 days [now-7d, now].
        let r = DateRange { since: None, until: None, when: Some("7d".into()) };
        let (since, until) = r.resolve(now()).unwrap();
        assert_eq!(since.unwrap(), now() - Duration::days(7));
        assert_eq!(until.unwrap(), now());
        assert!(since.unwrap() < until.unwrap());
    }

    #[test]
    fn daterange_when_relative_nlp_is_the_whole_day() {
        // Regression: `--when yesterday` used to be a zero-width instant. It must now
        // cover the full previous day, exactly like `--when <that date>`.
        let r = DateRange { since: None, until: None, when: Some("yesterday".into()) };
        let (since, until) = r.resolve(now()).unwrap();
        let y = (now() - Duration::days(1)).date_naive();
        assert_eq!(since.unwrap(), y.and_hms_opt(0, 0, 0).unwrap().and_utc());
        assert_eq!(until.unwrap(), y.and_hms_opt(23, 59, 59).unwrap().and_utc());
        // Equivalent to the explicit calendar form.
        let explicit = DateRange {
            since: None,
            until: None,
            when: Some(y.format("%Y-%m-%d").to_string()),
        };
        assert_eq!(r.resolve(now()).unwrap(), explicit.resolve(now()).unwrap());
    }

    #[test]
    fn daterange_since_until_duration_bound_selection() {
        // `--since 7d` = lower bound now-7d; `--until 7d` = upper bound now.
        let since_only = DateRange { since: Some("7d".into()), until: None, when: None };
        let (s, u) = since_only.resolve(now()).unwrap();
        assert_eq!(s.unwrap(), now() - Duration::days(7));
        assert!(u.is_none());

        let until_only = DateRange { since: None, until: Some("7d".into()), when: None };
        let (s, u) = until_only.resolve(now()).unwrap();
        assert!(s.is_none());
        assert_eq!(u.unwrap(), now());
    }

    #[test]
    fn daterange_inverted_range_resolves_both_bounds() {
        // since > until is a legal (if empty-yielding) query: both bounds set, no panic.
        let r = DateRange { since: Some("2026-03".into()), until: Some("2026-01".into()), when: None };
        let (since, until) = r.resolve(now()).unwrap();
        assert!(since.unwrap() > until.unwrap());
    }

    #[test]
    fn daterange_invalid_value_is_a_clean_error() {
        let r = DateRange { since: Some("notadate".into()), until: None, when: None };
        let err = r.resolve(now()).unwrap_err().to_string();
        assert!(err.contains("--since"), "error names the offending flag: {err}");
    }
}
