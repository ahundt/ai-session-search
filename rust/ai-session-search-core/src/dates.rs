// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

//! Flexible date / EDTF / duration / natural-language parsing for the `--since`,
//! `--until`, and `--when` filters. Rather than hand-roll a date grammar, it offloads the
//! genuinely hard parts to battle-tested libraries:
//!
//!   * **EDTF** (`2026`, `2026-01`, `202X`, `19XX`, `2026-XX`, `2026-01-XX`,
//!     intervals `A/B`, full datetimes) → the [`edtf`] crate. Its [`edtf::level_1::Precision`]
//!     classifies the value (Century/Decade/Year/Month/Day + the `XX`-unspecified
//!     variants) and we expand that to instant bounds with `chrono`.
//!   * **Natural language** (`yesterday`, `3 days ago`, `last friday`) → the
//!     [`interim`] crate (a `chrono-english` fork).
//!     Resolved NLP is widened to its whole UTC day (see below), so a time-of-day in the
//!     phrase (e.g. `8pm`) is intentionally ignored — use an ISO datetime for second
//!     precision.
//!   * **ISO datetimes** (full and partial-precision `…T14`, `…T14:30`) and the
//!     **duration shorthand** (`7d 2w 24h 30m 1M 1y`) → `chrono` arithmetic;
//!     these are not EDTF and not standard NLP. The units follow humantime and systemd:
//!     `m` is minutes and `M` months (ISO 8601 writes the same split as `PT1M` and `P1M`).
//!     Months and years are calendar months via `chrono`'s `checked_sub_months`, not fixed
//!     day counts. No dependency we carry parses this shorthand with calendar months
//!     (humantime and jiff use fixed or differently spelled months), so the token match
//!     lives here and every date computation is chrono's.
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

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Duration, Utc};
use clap::Args;
use edtf::level_1::{Date as EdtfDate, Edtf, Precision, Season};
use interim::{parse_date_string, Dialect};
use jiff::tz::TimeZone;
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

/// Help section every command that takes `--since/--until/--when` files them under, so the three
/// stay together and apart from that command's own filters.
pub const TIME_WINDOW_HEADING: &str = "Time window (formats: `aise dates`)";

// Shared `--since/--until/--when` flags. Flattened into each command's args so syntax and
// inclusive bound resolution are defined once (DRY). The resolved bounds are relation-neutral:
// session queries intersect them with a known session span, while message/file/analytics queries
// compare each event timestamp. `--when` supplies both bounds and conflicts with the one-sided
// forms (enforced by clap).
#[derive(Debug, Clone, Default, Args)]
#[non_exhaustive]
pub struct DateRange {
    /// Inclusive lower bound of the query period. Session commands (`list`, `search`, `export`,
    /// `analyze`) intersect it with each session's known span from created_at through updated_at;
    /// message, file, and analytics commands compare each event's own timestamp. Accepts EDTF /
    /// ISO / duration / natural language. Calendar and relative periods use UTC; exact RFC 3339
    /// timestamps honor `Z` or their explicit offset. Examples: `2026-01-15`, `2026-01`, `202X`,
    /// `7d`, `yesterday`. See `aise dates`. Omit for no lower bound.
    #[arg(long, help_heading = TIME_WINDOW_HEADING)]
    pub since: Option<String>,
    /// Inclusive upper bound of the query period. Same formats as `--since`; periods resolve to
    /// their last instant, while an RFC 3339 timestamp preserves its fractional-second precision.
    /// Omit for no upper bound.
    #[arg(long, help_heading = TIME_WINDOW_HEADING)]
    pub until: Option<String>,
    /// A single period used as BOTH bounds (e.g. `2026-01`, `202X`, `2026-01/2026-03`).
    /// Omit for no time filter; conflicts with `--since` and `--until`.
    #[arg(long, help_heading = TIME_WINDOW_HEADING, conflicts_with_all = ["since", "until"])]
    pub when: Option<String>,
}

impl DateRange {
    /// Resolve the flags to `(since, until)` UTC instants relative to `now`, reading dates and
    /// times that carry no offset in UTC. [`DateRange::resolve_now`] reads them in local time.
    pub fn resolve(&self, now: DateTime<Utc>) -> Result<Bounds> {
        self.resolve_in_zone(now, &TimeZone::UTC)
    }

    /// Resolve the flags relative to `now`, reading dates and times that carry no offset in the
    /// IANA time zone `time_zone` (for example `America/New_York` or `UTC`), as ISO 8601 and
    /// EDTF define them: `2026-11-07` is that day in that zone, while `2026-11-07T14:30:00Z` and
    /// durations such as `2h` name the same instants everywhere.
    pub fn resolve_in(&self, now: DateTime<Utc>, time_zone: &str) -> Result<Bounds> {
        self.resolve_in_zone(now, &named_time_zone(time_zone)?)
    }

    pub(crate) fn resolve_in_zone(&self, now: DateTime<Utc>, zone: &TimeZone) -> Result<Bounds> {
        if let Some(when) = self.when.as_deref() {
            let (start, end) = parse_span_in(when, now, zone)
                .map_err(|err| anyhow!("invalid --when '{when}': {err}"))?;
            if start > end {
                bail!(
                    "invalid --when '{when}': resolved period is reversed; put the earlier date first"
                );
            }
            return Ok((Some(start), Some(end)));
        }
        let since = self
            .since
            .as_deref()
            .map(|raw| {
                parse_bound_in(raw, Bound::Start, now, zone)
                    .map_err(|err| anyhow!("invalid --since '{raw}': {err}"))
            })
            .transpose()?;
        let until = self
            .until
            .as_deref()
            .map(|raw| {
                parse_bound_in(raw, Bound::End, now, zone)
                    .map_err(|err| anyhow!("invalid --until '{raw}': {err}"))
            })
            .transpose()?;
        if since.zip(until).is_some_and(|(start, end)| start > end) {
            bail!("resolved --since is later than --until; swap the bounds or move --until later");
        }
        Ok((since, until))
    }

    /// Resolve relative to the wall clock in the local time zone: the `TZ` environment variable
    /// when set, otherwise the system zone ([`local_time_zone`]). Used by command handlers.
    pub fn resolve_now(&self) -> Result<Bounds> {
        self.resolve_in_zone(Utc::now(), &local_time_zone())
    }
}

/// Parse one token to a single bound (start or end of its period), reading input without an
/// offset in UTC.
pub fn parse_bound(input: &str, bound: Bound, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    parse_bound_in(input, bound, now, &TimeZone::UTC)
}

/// [`parse_bound`], reading input without an offset in `zone`.
pub(crate) fn parse_bound_in(
    input: &str,
    bound: Bound,
    now: DateTime<Utc>,
    zone: &TimeZone,
) -> Result<DateTime<Utc>> {
    let (start, end) = parse_span_in(input, now, zone)?;
    Ok(match bound {
        Bound::Start => start,
        Bound::End => end,
    })
}

/// Parse one token to its full `(start, end)` instant span, reading input without an offset in
/// UTC.
pub fn parse_span(input: &str, now: DateTime<Utc>) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    parse_span_in(input, now, &TimeZone::UTC)
}

/// [`parse_span`], reading input without an offset in `zone`.
pub(crate) fn parse_span_in(
    input: &str,
    now_utc: DateTime<Utc>,
    zone: &TimeZone,
) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    let now = zoned(now_utc, zone)?;
    let s = input.trim();
    if s.is_empty() {
        bail!("empty date (try 2026-01-15, 2026-01, 202X, 7d, yesterday — run `aise dates`)");
    }

    // 1. Durations (7d 2w 1M 24h 30m 1y, 1d12h, P1M, PT2H) are the window [now - span, now].
    //    jiff applies the span in the zone: months and days are calendar units, so `1M` from
    //    March 31 starts on the last day of February and `1d` across a daylight-saving change
    //    keeps the wall-clock time; hours and smaller units are elapsed time.
    if let Some(span) = parse_duration(s)? {
        let start = now
            .checked_sub(span)
            .map_err(|err| anyhow!("duration '{s}' is out of range: {err}"))?;
        return Ok((from_timestamp(start.timestamp())?, now_utc));
    }

    // EDTF needs uppercase `X`; only uppercase purely date-shaped tokens so NLP
    // strings ("yesterday", "last friday") keep their original casing.
    let normalized = if datelike_re().is_match(s) {
        s.to_ascii_uppercase()
    } else {
        s.to_string()
    };
    let s = normalized.as_str();

    // 2. An exact instant with `Z` or an offset (RFC 3339 / RFC 9557), including fractional
    //    seconds. Parsed before EDTF/NLP: the EDTF crate rejects fractional seconds, and the NLP
    //    fallback could accept the calendar part and widen an exact instant to a whole day.
    if let Ok(timestamp) = s.parse::<jiff::Timestamp>() {
        let instant = from_timestamp(timestamp)?;
        return Ok((instant, instant));
    }

    // 3. A date and time without an offset is local time (ISO 8601, EDTF). Hour or minute
    //    precision names that whole hour or minute; seconds or finer name one instant.
    if let Some((_, time)) = s.split_once('T').filter(|(date, _)| date.len() == 10) {
        let colons = time.matches(':').count();
        let text = if colons == 0 {
            format!("{s}:00")
        } else {
            s.to_string()
        };
        if let Ok(start) = text.parse::<jiff::civil::DateTime>() {
            let length = match colons {
                0 => Some(jiff::Span::new().hours(1)),
                1 => Some(jiff::Span::new().minutes(1)),
                _ => None,
            };
            let end = match length {
                Some(length) => start
                    .checked_add(length)?
                    .checked_sub(jiff::Span::new().nanoseconds(1))?,
                None => start,
            };
            return Ok((instant_in(zone, start)?, instant_in(zone, end)?));
        }
    }

    // 4. Single-digit-unspecified day (2026-01-1X, 2026-01-X5, 2026-01-XX) — the
    //    edtf crate parses whole-day `-XX` but rejects partial-digit days, so we
    //    expand these per the EDTF unspecified-digit rules.
    if let Some(caps) = day_x_re().captures(s) {
        let tens = caps[3].chars().next().unwrap_or('X');
        let units = caps[4].chars().next().unwrap_or('X');
        if tens == 'X' || units == 'X' {
            return day_x_span(caps[1].parse()?, caps[2].parse()?, tens, units, zone);
        }
    }

    // 5. EDTF: dates, months, years, decades, centuries, XX-unspecified, intervals,
    //    full datetimes. The library does the parsing; Precision drives bound math.
    if let Ok(edtf) = Edtf::parse(s) {
        return match edtf {
            Edtf::Date(date) => date_bounds(&date, zone),
            Edtf::Interval(a, b) => Ok((date_bounds(&a, zone)?.0, date_bounds(&b, zone)?.1)),
            // Zone-less datetimes were read in step 3 and offset ones in step 2.
            Edtf::DateTime(dt) => {
                let instant = dt.to_chrono(&Utc);
                Ok((instant, instant))
            }
            // YYear (>4 digit years) and open intervals are out of scope here.
            _ => bail!("unsupported EDTF form '{s}'"),
        };
    }

    // 6. Natural language ("yesterday", "3 days ago", "last friday"). These are
    //    day-granular, so expand the resolved instant to its full calendar day in the zone:
    //    `--when yesterday` then covers the whole day (consistent with `--when
    //    2026-06-24`) instead of a zero-width instant, while `--since`/`--until` take
    //    that day's start/end.
    if let Ok(resolved) = parse_date_string(input.trim(), now.clone(), Dialect::Us) {
        return days_span(resolved.date(), resolved.date(), zone);
    }

    bail!(
        "unrecognized date/time '{input}' — try 2026-01-15, 2026-01, 2026, 202X, 2026-01-1X, \
         2026-01/2026-03, 7d, 2w, 1M, 24h, 1y, yesterday, '3 days ago' (run `aise dates`)"
    )
}

/// A duration token, parsed by jiff's friendly or ISO 8601 grammar. `M` alone means months, as
/// in humantime and systemd (jiff rejects it to avoid confusion with minutes); other units ignore
/// case. Phrases with spaces, such as `3 days ago`, are left to natural language.
fn parse_duration(s: &str) -> Result<Option<jiff::Span>> {
    if s.contains(char::is_whitespace) {
        return Ok(None);
    }
    let months = s
        .strip_suffix('M')
        .filter(|count| !count.is_empty() && count.bytes().all(|byte| byte.is_ascii_digit()));
    let span = match months {
        Some(count) => format!("{count}mo").parse::<jiff::Span>().ok(),
        None => s
            .parse::<jiff::Span>()
            .or_else(|_| s.to_ascii_lowercase().parse())
            .ok(),
    };
    match span {
        Some(span) if span.is_negative() => {
            bail!("duration '{s}' is negative; durations count back from now, so write it without '-'")
        }
        span => Ok(span),
    }
}

/// Expand an [`EdtfDate`] to inclusive `(start, end)` instants from its precision.
fn date_bounds(date: &EdtfDate, zone: &TimeZone) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    match date.precision() {
        Precision::Century(y) => years_span(y, y + 99, zone),
        Precision::Decade(y) => years_span(y, y + 9, zone),
        Precision::Year(y) | Precision::MonthOfYear(y) | Precision::DayOfYear(y) => {
            years_span(y, y, zone)
        }
        Precision::Month(y, m) | Precision::DayOfMonth(y, m) => months_span(y, m, y, m, zone),
        Precision::Day(y, m, d) => days_span(civil_date(y, m, d)?, civil_date(y, m, d)?, zone),
        // Meteorological seasons; winter runs into the next calendar year.
        Precision::Season(y, season) => match season {
            Season::Spring => months_span(y, 3, y, 5, zone),
            Season::Summer => months_span(y, 6, y, 8, zone),
            Season::Autumn => months_span(y, 9, y, 11, zone),
            Season::Winter => months_span(y, 12, y + 1, 2, zone),
        },
    }
}

/// Unspecified-day expansion: `XX`→whole month, `1X`→10..19,
/// `X5`→5..25, each clamped to the month's real length.
fn day_x_span(
    year: i32,
    month: u32,
    tens: char,
    units: char,
    zone: &TimeZone,
) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    let max_day = u32::try_from(civil_date(year, month, 1)?.days_in_month())?;
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
    days_span(
        civil_date(year, month, lo)?,
        civil_date(year, month, hi)?,
        zone,
    )
}

// ── time zones and calendar spans (jiff) ────────────────────────────────────

/// The local time zone: the `TZ` environment variable when set, otherwise the system zone
/// (`/etc/localtime` on Unix, the Windows zone mapped to IANA). Falls back to UTC when neither
/// can be determined.
pub fn local_time_zone() -> TimeZone {
    TimeZone::try_system().unwrap_or(TimeZone::UTC)
}

/// Look up an IANA time zone name such as `America/New_York` or `UTC`.
fn named_time_zone(name: &str) -> Result<TimeZone> {
    TimeZone::get(name).map_err(|err| {
        anyhow!(
            "unknown time zone '{name}': {err}; use an IANA name such as America/New_York, \
             Europe/Berlin or UTC"
        )
    })
}

fn zoned(now: DateTime<Utc>, zone: &TimeZone) -> Result<jiff::Zoned> {
    let nanos = i32::try_from(now.timestamp_subsec_nanos())?;
    Ok(jiff::Timestamp::new(now.timestamp(), nanos)?.to_zoned(zone.clone()))
}

fn from_timestamp(timestamp: jiff::Timestamp) -> Result<DateTime<Utc>> {
    let seconds = crate::util::parse_unix_seconds(timestamp.as_second())
        .ok_or_else(|| anyhow!("instant {timestamp} is out of range"))?;
    Ok(seconds + Duration::nanoseconds(i64::from(timestamp.subsec_nanosecond())))
}

/// The UTC instant a wall-clock time names in `zone`. Daylight-saving transitions resolve with
/// jiff's "compatible" rule from RFC 5545 and TC39 Temporal: a time that occurs twice takes its
/// first occurrence, and a time skipped when clocks go forward lands after the gap.
fn instant_in(zone: &TimeZone, civil: jiff::civil::DateTime) -> Result<DateTime<Utc>> {
    from_timestamp(zone.to_ambiguous_timestamp(civil).compatible()?)
}

fn civil_date(year: i32, month: u32, day: u32) -> Result<jiff::civil::Date> {
    Ok(jiff::civil::Date::new(
        i16::try_from(year)?,
        i8::try_from(month)?,
        i8::try_from(day)?,
    )?)
}

/// From the first instant of local day `first` to the last nanosecond of local day `last` in
/// `zone`. A day may be 23 or 25 hours long, or start after midnight where midnight is skipped.
fn days_span(
    first: jiff::civil::Date,
    last: jiff::civil::Date,
    zone: &TimeZone,
) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    let start = first.to_zoned(zone.clone())?.start_of_day()?;
    let end = last.to_zoned(zone.clone())?.end_of_day()?;
    Ok((
        from_timestamp(start.timestamp())?,
        from_timestamp(end.timestamp())?,
    ))
}

fn months_span(
    first_year: i32,
    first_month: u32,
    last_year: i32,
    last_month: u32,
    zone: &TimeZone,
) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    let first = civil_date(first_year, first_month, 1)?;
    let last = civil_date(last_year, last_month, 1)?.last_of_month();
    days_span(first, last, zone)
}

fn years_span(
    first_year: i32,
    last_year: i32,
    zone: &TimeZone,
) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    months_span(first_year, 1, last_year, 12, zone)
}

// ── cached regexes (compiled once) ──────────────────────────────────────────

fn datelike_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[0-9XxTt:/.\-]+$").expect("valid datelike regex"))
}

fn day_x_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\d{4})-(\d{2})-([0-3X])([0-9X])$").expect("valid day-x regex"))
}

/// Human-readable format reference for the `dates` command.
pub fn format_reference() -> String {
    [
        "aise date/time formats (--since, --until, --when)",
        "",
        "ISO 8601:",
        "  2026-01-15T14:30:25.123Z exact fractional instant (offsets normalize to UTC)",
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
        "  7d 2w 24h 30m 1M 1y     (m = minutes, M or mo = calendar months, y = calendar years)",
        "",
        "Natural language (via the `interim` crate; day-granular):",
        "  today  yesterday  \"3 days ago\"  \"last friday\"",
        "",
        "--since uses a period's start, --until its end, --when uses both.",
        "Calendar periods and relative days use UTC; RFC 3339 timestamps honor their offset.",
        "A duration spans [now - N, now]; a relative word spans its whole day,",
        "so `--when 7d` = the last 7 days and `--when yesterday` = all of yesterday.",
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    /// Fixed reference instant so duration/NLP cases are deterministic.
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 25, 12, 0, 0).unwrap()
    }

    fn span(s: &str) -> (DateTime<Utc>, DateTime<Utc>) {
        parse_span(s, now()).unwrap_or_else(|e| panic!("parse_span({s:?}) failed: {e}"))
    }

    fn iso(s: &str) -> DateTime<Utc> {
        crate::util::parse_datetime(s).unwrap()
    }

    // ── Time zones: input without an offset is local time (ISO 8601, EDTF) ──
    fn in_new_york(when: &str, now: &str) -> (DateTime<Utc>, DateTime<Utc>) {
        let range = DateRange {
            when: Some(when.to_string()),
            ..DateRange::default()
        };
        let (start, end) = range.resolve_in(iso(now), "America/New_York").unwrap();
        (start.unwrap(), end.unwrap())
    }

    #[test]
    fn input_without_an_offset_is_read_in_the_named_time_zone() {
        // 8 pm on Nov 7 in New York (EST, UTC-05:00) is 01:00Z on Nov 8.
        let evening = "2026-11-08T01:00:00Z";
        let nov_7 = (
            iso("2026-11-07T05:00:00Z"),
            iso("2026-11-08T04:59:59.999999999Z"),
        );
        assert_eq!(in_new_york("2026-11-07", evening), nov_7);
        assert_eq!(in_new_york("today", evening), nov_7);
        assert_eq!(
            in_new_york("2026-11-07T14:30", evening),
            (
                iso("2026-11-07T19:30:00Z"),
                iso("2026-11-07T19:30:59.999999999Z")
            )
        );
        // Seconds precision names one instant (start == end), read in the zone.
        let afternoon = iso("2026-11-07T19:30:00Z");
        assert_eq!(
            in_new_york("2026-11-07T14:30:00", evening),
            (afternoon, afternoon)
        );
        // An explicit offset or `Z` names one instant wherever the reader is.
        let exact = iso("2026-11-07T14:30:00Z");
        assert_eq!(in_new_york("2026-11-07T14:30:00Z", evening), (exact, exact));
        // A duration is elapsed time and does not depend on the zone.
        assert_eq!(in_new_york("2h", evening).0, iso("2026-11-07T23:00:00Z"));
        // The UTC entry points keep the reading existing callers rely on.
        assert_eq!(
            span("2026-01-15"),
            (
                iso("2026-01-15T00:00:00Z"),
                iso("2026-01-15T23:59:59.999999999Z")
            )
        );
        let unknown = DateRange::default().resolve_in(iso(evening), "Mars/Olympus_Mons");
        assert!(unknown.unwrap_err().to_string().contains("IANA name"));
    }

    #[test]
    fn daylight_saving_days_and_times_follow_the_zone_rules() {
        // US clocks went forward at 02:00 on 2026-03-08 and back at 02:00 on 2026-11-01.
        let now = "2026-12-01T00:00:00Z";
        let (start, end) = in_new_york("2026-03-08", now);
        assert_eq!(start, iso("2026-03-08T05:00:00Z"));
        assert_eq!(end, iso("2026-03-09T03:59:59.999999999Z"), "a 23-hour day");
        let (start, end) = in_new_york("2026-11-01", now);
        assert_eq!(start, iso("2026-11-01T04:00:00Z"));
        assert_eq!(end, iso("2026-11-02T04:59:59.999999999Z"), "a 25-hour day");
        // 02:30 never happened on 2026-03-08; it reads as the instant after the gap, 03:30 EDT.
        assert_eq!(
            in_new_york("2026-03-08T02:30:00", now).0,
            iso("2026-03-08T07:30:00Z")
        );
        // 01:30 happened twice on 2026-11-01; it reads as the first, 01:30 EDT.
        assert_eq!(
            in_new_york("2026-11-01T01:30:00", now).0,
            iso("2026-11-01T05:30:00Z")
        );
        // A calendar month back across the change keeps the wall-clock time.
        assert_eq!(
            in_new_york("1M", "2026-04-07T16:00:00Z").0,
            iso("2026-03-07T17:00:00Z"),
            "noon EDT back one month is noon EST"
        );
    }

    // ── Family 1: duration shorthand ────────────────────────────────────────
    #[test]
    fn durations_are_a_window_ending_now() {
        // A duration is the window [now - delta, now], so `--since 7d` starts 7 days
        // back and `--when 7d` covers the last 7 days (not a zero-width instant).
        assert_eq!(span("7d"), (now() - Duration::days(7), now()));
        assert_eq!(span("2w"), (now() - Duration::weeks(2), now()));
        assert_eq!(span("24h"), (now() - Duration::hours(24), now()));
        assert_eq!(span("2h"), (now() - Duration::hours(2), now()));
        assert_eq!(span("2d"), (now() - Duration::days(2), now()));
        assert_eq!(span("30min"), (now() - Duration::minutes(30), now()));
        // Lowercase m is minutes and uppercase M is calendar months, as in humantime and
        // systemd; ISO 8601 writes the same split as PT1M and P1M.
        assert_eq!(span("5m"), (now() - Duration::minutes(5), now()));
        assert_eq!(span("1M"), (iso("2026-05-25T12:00:00Z"), now()));
        assert_eq!(span("1mo"), span("1M"));
        assert_eq!(span("30min"), span("30m"));
        assert_eq!(span("1y"), (iso("2025-06-25T12:00:00Z"), now()));
        assert_eq!(span("2Y"), (iso("2024-06-25T12:00:00Z"), now()));
        // The window is non-empty: start strictly before end.
        let (start, end) = span("7d");
        assert!(start < end);
        // Case-insensitive unit.
        assert_eq!(span("24H"), (now() - Duration::hours(24), now()));
    }

    #[test]
    fn calendar_months_clamp_to_the_last_day_of_a_shorter_month() {
        // Fixed 30-day months started a "1M" window on 2026-03-01 from 2026-03-31; a calendar
        // month back from March 31 is the last day of February.
        let march_31 = iso("2026-03-31T08:00:00Z");
        assert_eq!(
            parse_span("1M", march_31).unwrap().0,
            iso("2026-02-28T08:00:00Z")
        );
        let leap_day = iso("2024-02-29T08:00:00Z");
        assert_eq!(
            parse_span("1y", leap_day).unwrap().0,
            iso("2023-02-28T08:00:00Z")
        );
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
    fn rfc3339_fractional_second_and_offset_are_exact_instants() {
        // Regression: EDTF rejects fractional seconds, then the NLP fallback interpreted the
        // calendar portion as a whole-day span. Equal --since/--until values consequently leaked
        // every later message from that day.
        for input in ["2026-07-07T02:53:22.358Z", "2026-07-06T22:53:22.358-04:00"] {
            let (start, end) = span(input);
            assert_eq!(start, iso("2026-07-07T02:53:22.358Z"));
            assert_eq!(start, end);
        }
    }

    #[test]
    fn partial_datetime_expands_by_precision() {
        let (s, e) = span("2026-01-15T14");
        assert_eq!(s, iso("2026-01-15T14:00:00+00:00"));
        assert_eq!(e, iso("2026-01-15T14:59:59.999999999+00:00"));
        let (s, e) = span("2026-01-15T14:30");
        assert_eq!(s, iso("2026-01-15T14:30:00+00:00"));
        assert_eq!(e, iso("2026-01-15T14:30:59.999999999+00:00"));
    }

    // ── Family 3: calendar date / month / year ──────────────────────────────
    #[test]
    fn day_month_year_periods() {
        assert_eq!(
            span("2026-01-15"),
            (
                iso("2026-01-15T00:00:00+00:00"),
                iso("2026-01-15T23:59:59.999999999+00:00")
            )
        );
        assert_eq!(
            span("2026-01"),
            (
                iso("2026-01-01T00:00:00+00:00"),
                iso("2026-01-31T23:59:59.999999999+00:00")
            )
        );
        assert_eq!(
            span("2026"),
            (
                iso("2026-01-01T00:00:00+00:00"),
                iso("2026-12-31T23:59:59.999999999+00:00")
            )
        );
    }

    // ── Family 4: EDTF unspecified digits ───────────────────────────────────
    #[test]
    fn edtf_decade_and_century() {
        assert_eq!(
            span("202X"),
            (
                iso("2020-01-01T00:00:00+00:00"),
                iso("2029-12-31T23:59:59.999999999+00:00")
            )
        );
        assert_eq!(
            span("19XX"),
            (
                iso("1900-01-01T00:00:00+00:00"),
                iso("1999-12-31T23:59:59.999999999+00:00")
            )
        );
        // Lowercase x is normalized to uppercase before EDTF parsing.
        assert_eq!(span("202x"), span("202X"));
    }

    #[test]
    fn edtf_unspecified_month_and_day() {
        // Whole-year and whole-month via -XX.
        assert_eq!(span("2026-XX"), span("2026"));
        assert_eq!(span("2026-01-XX"), span("2026-01"));
        // Partial-digit day (edtf rejects these → manual expansion).
        assert_eq!(
            span("2026-01-1X"),
            (
                iso("2026-01-10T00:00:00+00:00"),
                iso("2026-01-19T23:59:59.999999999+00:00")
            )
        );
        // "3X" clamps the upper day to the real month length (Jan → 31).
        assert_eq!(
            span("2026-01-3X"),
            (
                iso("2026-01-30T00:00:00+00:00"),
                iso("2026-01-31T23:59:59.999999999+00:00")
            )
        );
    }

    #[test]
    fn edtf_units_fixed_day_reaches_last_matching_day() {
        // tens unspecified, units fixed: the span must reach the LAST in-month day with
        // that units digit, not stop ~20. "X0" → 10,20,30; "X1" → 1,11,21,31.
        assert_eq!(
            span("2026-01-X0"),
            (
                iso("2026-01-10T00:00:00+00:00"),
                iso("2026-01-30T23:59:59.999999999+00:00")
            )
        );
        assert_eq!(
            span("2026-01-X1"),
            (
                iso("2026-01-01T00:00:00+00:00"),
                iso("2026-01-31T23:59:59.999999999+00:00")
            )
        );
        // Unchanged middle digit (regression guard): "X5" → 5,15,25.
        assert_eq!(
            span("2026-01-X5"),
            (
                iso("2026-01-05T00:00:00+00:00"),
                iso("2026-01-25T23:59:59.999999999+00:00")
            )
        );
        // Clamped to a short month: Feb 2026 has 28 days, so "X0" → 10,20 (no day 30).
        assert_eq!(
            span("2026-02-X0"),
            (
                iso("2026-02-10T00:00:00+00:00"),
                iso("2026-02-20T23:59:59.999999999+00:00")
            )
        );
    }

    // ── Family 5: EDTF interval ─────────────────────────────────────────────
    #[test]
    fn edtf_interval_uses_start_of_a_end_of_b() {
        assert_eq!(
            span("2026-01/2026-03"),
            (
                iso("2026-01-01T00:00:00+00:00"),
                iso("2026-03-31T23:59:59.999999999+00:00")
            )
        );
    }

    // ── Family 6: natural language ──────────────────────────────────────────
    /// Inclusive `[00:00:00, 23:59:59.999999999]` span of the UTC day containing `dt`.
    fn day_of(dt: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
        let d = dt.date_naive();
        (
            d.and_hms_opt(0, 0, 0).unwrap().and_utc(),
            d.and_hms_nano_opt(23, 59, 59, 999_999_999)
                .unwrap()
                .and_utc(),
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
        assert_eq!(
            span("2024-02").1,
            iso("2024-02-29T23:59:59.999999999+00:00")
        );
        assert_eq!(
            span("2026-02").1,
            iso("2026-02-28T23:59:59.999999999+00:00")
        );
        // -XX day on Feb respects the real length.
        assert_eq!(
            span("2024-02-XX").1,
            iso("2024-02-29T23:59:59.999999999+00:00")
        );
    }

    #[test]
    fn month_end_bounds() {
        assert_eq!(
            span("2026-04").1,
            iso("2026-04-30T23:59:59.999999999+00:00")
        ); // 30-day month
        assert_eq!(
            span("2026-12").1,
            iso("2026-12-31T23:59:59.999999999+00:00")
        ); // 31-day month
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
        // "X5" = the days whose units digit is 5 → spans 5..25 (clamped to month).
        assert_eq!(
            span("2026-01-X5"),
            (
                iso("2026-01-05T00:00:00+00:00"),
                iso("2026-01-25T23:59:59.999999999+00:00")
            )
        );
        // "XX" = the whole month.
        assert_eq!(span("2026-01-XX"), span("2026-01"));
    }

    #[test]
    fn span_start_never_after_end_for_well_formed_periods() {
        // Ordering invariant across every well-formed form/family.
        for input in [
            "7d",
            "2w",
            "24h",
            "30min",
            "5m",
            "1M",
            "1y",
            "2026-01-15T14:30:25",
            "2026-01-15T14",
            "2026-01-15T14:30",
            "2026-01-15",
            "2026-01",
            "2026",
            "202X",
            "19XX",
            "2026-XX",
            "2026-01-XX",
            "2026-01-1X",
            "2026-01-X5",
            "2026-01/2026-03",
            "today",
            "yesterday",
            "3 days ago",
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
        assert_eq!(
            parse_bound("2026-01", Bound::Start, now()).unwrap(),
            iso("2026-01-01T00:00:00+00:00")
        );
        assert_eq!(
            parse_bound("2026-01", Bound::End, now()).unwrap(),
            iso("2026-01-31T23:59:59.999999999+00:00")
        );
    }

    // ── DateRange resolution ────────────────────────────────────────────────
    #[test]
    fn daterange_since_until_when() {
        let r = DateRange {
            since: Some("2026-01".into()),
            until: Some("2026-03".into()),
            when: None,
        };
        let (since, until) = r.resolve(now()).unwrap();
        assert_eq!(since.unwrap(), iso("2026-01-01T00:00:00+00:00"));
        assert_eq!(until.unwrap(), iso("2026-03-31T23:59:59.999999999+00:00"));

        // --when applies one period to both bounds.
        let r = DateRange {
            since: None,
            until: None,
            when: Some("2026".into()),
        };
        let (since, until) = r.resolve(now()).unwrap();
        assert_eq!(since.unwrap(), iso("2026-01-01T00:00:00+00:00"));
        assert_eq!(until.unwrap(), iso("2026-12-31T23:59:59.999999999+00:00"));

        // Empty range resolves to (None, None).
        let (since, until) = DateRange::default().resolve(now()).unwrap();
        assert!(since.is_none() && until.is_none());
    }

    #[test]
    fn daterange_when_interval() {
        let r = DateRange {
            since: None,
            until: None,
            when: Some("2026-01/2026-03".into()),
        };
        let (since, until) = r.resolve(now()).unwrap();
        assert_eq!(since.unwrap(), iso("2026-01-01T00:00:00+00:00"));
        assert_eq!(until.unwrap(), iso("2026-03-31T23:59:59.999999999+00:00"));
    }

    #[test]
    fn daterange_when_duration_is_the_last_n_window() {
        // Regression: `--when 7d` used to resolve to a zero-width instant and matched
        // nothing. It must now span the last 7 days [now-7d, now].
        let r = DateRange {
            since: None,
            until: None,
            when: Some("7d".into()),
        };
        let (since, until) = r.resolve(now()).unwrap();
        assert_eq!(since.unwrap(), now() - Duration::days(7));
        assert_eq!(until.unwrap(), now());
        assert!(since.unwrap() < until.unwrap());
    }

    #[test]
    fn daterange_when_relative_nlp_is_the_whole_day() {
        // Regression: `--when yesterday` used to be a zero-width instant. It must now
        // cover the full previous day, exactly like `--when <that date>`.
        let r = DateRange {
            since: None,
            until: None,
            when: Some("yesterday".into()),
        };
        let (since, until) = r.resolve(now()).unwrap();
        let y = (now() - Duration::days(1)).date_naive();
        assert_eq!(since.unwrap(), y.and_hms_opt(0, 0, 0).unwrap().and_utc());
        assert_eq!(
            until.unwrap(),
            y.and_hms_nano_opt(23, 59, 59, 999_999_999)
                .unwrap()
                .and_utc()
        );
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
        let since_only = DateRange {
            since: Some("7d".into()),
            until: None,
            when: None,
        };
        let (s, u) = since_only.resolve(now()).unwrap();
        assert_eq!(s.unwrap(), now() - Duration::days(7));
        assert!(u.is_none());

        let until_only = DateRange {
            since: None,
            until: Some("7d".into()),
            when: None,
        };
        let (s, u) = until_only.resolve(now()).unwrap();
        assert!(s.is_none());
        assert_eq!(u.unwrap(), now());
    }

    #[test]
    fn daterange_inverted_range_is_rejected() {
        let r = DateRange {
            since: Some("2026-03".into()),
            until: Some("2026-01".into()),
            when: None,
        };
        let error = r.resolve(now()).unwrap_err().to_string();
        assert!(error.contains("--since is later than --until"), "{error}");
    }

    #[test]
    fn daterange_when_rejects_reversed_interval() {
        let when = DateRange {
            since: None,
            until: None,
            when: Some("2026-03/2026-01".into()),
        };
        assert!(when
            .resolve(now())
            .unwrap_err()
            .to_string()
            .contains("reversed"));
    }

    #[test]
    fn daterange_invalid_value_is_a_clean_error() {
        let r = DateRange {
            since: Some("notadate".into()),
            until: None,
            when: None,
        };
        let err = r.resolve(now()).unwrap_err().to_string();
        assert!(
            err.contains("--since"),
            "error names the offending flag: {err}"
        );
    }
}
