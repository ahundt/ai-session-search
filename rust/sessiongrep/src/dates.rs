//! Flexible date / EDTF / duration / natural-language parsing for the `--since`,
//! `--until`, and `--when` filters. This mirrors aise's `parse_date_input`
//! (ai_session_tools/engine.py:48-211) but offloads the genuinely hard parts to
//! battle-tested libraries instead of hand-rolling a grammar:
//!
//!   * **EDTF** (`2026`, `2026-01`, `202X`, `19XX`, `2026-XX`, `2026-01-XX`,
//!     intervals `A/B`, full datetimes) → the [`edtf`] crate. Its [`edtf::level_1::Precision`]
//!     classifies the value (Century/Decade/Year/Month/Day + the `XX`-unspecified
//!     variants) and we expand that to instant bounds with `chrono`.
//!   * **Natural language** (`yesterday`, `3 days ago`, `last friday 8pm`) → the
//!     [`interim`] crate (a `chrono-english` fork), matching aise's `dateutil` fallback.
//!   * **ISO datetimes** (full and partial-precision `…T14`, `…T14:30`) and the
//!     **aise duration shorthand** (`7d 2w 1m 24h 30min 1y`) → `chrono` arithmetic;
//!     these are not EDTF and not standard NLP.
//!
//! Every input resolves to a `(start, end)` pair of UTC instants:
//!   * a precise instant (duration, full datetime, relative NLP) → `start == end`;
//!   * a fuzzy period (date, month, year, decade, century, unspecified-digit day) →
//!     `[first instant, last instant]` of the period;
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
    if let Some(caps) = duration_re().captures(s) {
        let n: i64 = caps[1].parse().map_err(|_| anyhow!("duration number too large in '{s}'"))?;
        let delta = match caps[2].to_ascii_lowercase().as_str() {
            "d" => Duration::days(n),
            "w" => Duration::weeks(n),
            "h" => Duration::hours(n),
            "min" => Duration::minutes(n),
            "m" => Duration::days(n * 30),  // 1m ≈ 30 days, matching aise
            "y" => Duration::days(n * 365), // 1y ≈ 365 days, matching aise
            other => bail!("unsupported duration unit '{other}'"),
        };
        let dt = now - delta;
        return Ok((dt, dt));
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

    // 5. Natural language ("yesterday", "3 days ago", "last friday 8pm").
    if let Ok(instant) = parse_date_string(input.trim(), now, Dialect::Us) {
        return Ok((instant, instant));
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
        // tens unspecified, units fixed: "X5" spans the days that digit could be
        let u = units.to_digit(10).unwrap_or(0);
        (if u >= 1 { u } else { 10 }, (u + 20).min(max_day))
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
        "Duration shorthand (relative to now):",
        "  7d 2w 1m 24h 30min 1y   (m ≈ 30 days, y ≈ 365 days)",
        "",
        "Natural language (via the `interim` crate):",
        "  today  yesterday  \"3 days ago\"  \"last friday 8pm\"",
        "",
        "--since uses a period's start, --until its end, --when uses both.",
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
    fn durations_subtract_from_now() {
        assert_eq!(span("7d").0, now() - Duration::days(7));
        assert_eq!(span("2w").0, now() - Duration::weeks(2));
        assert_eq!(span("24h").0, now() - Duration::hours(24));
        assert_eq!(span("30min").0, now() - Duration::minutes(30));
        assert_eq!(span("1m").0, now() - Duration::days(30));
        assert_eq!(span("1y").0, now() - Duration::days(365));
        // Single instant: start == end.
        let s = span("7d");
        assert_eq!(s.0, s.1);
        // Case-insensitive.
        assert_eq!(span("24H").0, now() - Duration::hours(24));
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

    // ── Family 5: EDTF interval ─────────────────────────────────────────────
    #[test]
    fn edtf_interval_uses_start_of_a_end_of_b() {
        assert_eq!(span("2026-01/2026-03"), (iso("2026-01-01T00:00:00+00:00"), iso("2026-03-31T23:59:59+00:00")));
    }

    // ── Family 6: natural language ──────────────────────────────────────────
    #[test]
    fn nlp_today_and_relative() {
        // "today" → the calendar day of `now`.
        let (s, e) = span("today");
        assert_eq!(s.date_naive(), now().date_naive());
        assert_eq!(e.date_naive(), now().date_naive());
        // "3 days ago" resolves to a real instant before now.
        let (s, _) = span("3 days ago");
        assert!(s < now());
        assert_eq!(s.date_naive(), (now() - Duration::days(3)).date_naive());
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
}
