//! Foundry's instant-in-time type and its persisted text format.
//!
//! Foundry wrote months of state with `chrono` before it moved to `jiff`:
//! the event JSONL files, the work-item ledger, the registry, campaigns,
//! sentinels, pacing state, traces, findings and initiatives. Other readers
//! (ops-visualizer, scripts) parse those files. [`UtcDateTime`] keeps that
//! text format exactly:
//!
//! - serde writes the chrono `Serialize` form, `2026-11-20T07:00:01.798036936Z`;
//! - [`UtcDateTime::to_rfc3339`] writes the chrono `to_rfc3339` form,
//!   `2026-11-20T07:00:01.798036936+00:00`;
//! - [`fmt::Display`] writes the chrono `Display` form,
//!   `2026-11-20 07:00:01.798036936 UTC`.
//!
//! All three use chrono's "auto SI" fraction: no fraction for whole seconds,
//! otherwise 3, 6 or 9 digits, the fewest that hold the value without loss.
//! (jiff's own formatting trims trailing zeros instead, so `.120` would become
//! `.12`.) Parsing accepts every RFC 3339 form chrono accepted, with any
//! offset, and normalises to UTC.
//!
//! The supported range is jiff's: years 0000 to 9999 for text (jiff's
//! timestamp range ends a few hours before 10000-01-01). Foundry has never
//! written a timestamp outside it.
//!
//! Durations are [`jiff::SignedDuration`], re-exported as [`SignedDuration`].

use std::fmt;
use std::ops::{Add, AddAssign, Sub, SubAssign};
use std::str::FromStr;

use jiff::Timestamp;
use jiff::civil::{Date, DateTime};
use jiff::tz::{Offset, TimeZone};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub use jiff::SignedDuration;

const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// An instant in time, held in UTC.
///
/// Ordering, equality and hashing compare instants, so two values parsed from
/// different offsets that name the same instant are equal.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UtcDateTime(Timestamp);

/// A timestamp string that is not a valid RFC 3339 instant.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid timestamp '{input}': {reason}")]
pub struct ParseTimestampError {
    input: String,
    reason: String,
}

impl UtcDateTime {
    /// The current system time.
    #[must_use]
    pub fn now() -> Self {
        Self(Timestamp::now())
    }

    /// The instant `seconds` whole seconds plus `nanoseconds` after the Unix
    /// epoch, as chrono's `DateTime::from_timestamp` defines it: `seconds`
    /// may be negative, `nanoseconds` must be below one second, and `None`
    /// means the value is out of range.
    #[must_use]
    pub fn from_timestamp(seconds: i64, nanoseconds: u32) -> Option<Self> {
        if i64::from(nanoseconds) >= NANOS_PER_SECOND {
            return None;
        }
        let total = i128::from(seconds) * i128::from(NANOS_PER_SECOND) + i128::from(nanoseconds);
        Timestamp::from_nanosecond(total).ok().map(Self)
    }

    /// The UTC instant at the given calendar date and wall-clock time, or
    /// `None` when any part is out of range (chrono's
    /// `Utc.with_ymd_and_hms`).
    #[must_use]
    pub fn from_ymd_hms(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
        second: u32,
    ) -> Option<Self> {
        let civil = DateTime::new(
            i16::try_from(year).ok()?,
            i8::try_from(month).ok()?,
            i8::try_from(day).ok()?,
            i8::try_from(hour).ok()?,
            i8::try_from(minute).ok()?,
            i8::try_from(second).ok()?,
            0,
        )
        .ok()?;
        Offset::UTC.to_timestamp(civil).ok().map(Self)
    }

    /// Whole seconds since the Unix epoch, rounded toward negative infinity
    /// (chrono's `timestamp()`). `1969-12-31T23:59:59.5Z` gives `-1`, not `0`
    /// as jiff's own `as_second()` would.
    #[must_use]
    pub fn timestamp(&self) -> i64 {
        self.floor_seconds_and_nanos().0
    }

    /// Whole microseconds since the Unix epoch, rounded toward negative
    /// infinity (chrono's `timestamp_micros()`).
    #[must_use]
    pub fn timestamp_micros(&self) -> i64 {
        let (seconds, nanos) = self.floor_seconds_and_nanos();
        seconds * 1_000_000 + i64::from(nanos / 1_000)
    }

    /// Nanoseconds since the Unix epoch, or `None` when the value does not
    /// fit in an `i64` (chrono's `timestamp_nanos_opt()`).
    #[must_use]
    pub fn timestamp_nanos_opt(&self) -> Option<i64> {
        i64::try_from(self.0.as_nanosecond()).ok()
    }

    /// Seconds rounded toward negative infinity, and the non-negative
    /// nanoseconds past them. jiff gives both parts the sign of the instant.
    fn floor_seconds_and_nanos(&self) -> (i64, i32) {
        let (seconds, nanos) = (self.0.as_second(), self.0.subsec_nanosecond());
        if nanos < 0 {
            (seconds - 1, nanos + 1_000_000_000)
        } else {
            (seconds, nanos)
        }
    }

    /// Parse an RFC 3339 timestamp with any offset (`Z`, `+00:00`, `-05:00`).
    ///
    /// # Errors
    ///
    /// Returns [`ParseTimestampError`] when `input` is not a timestamp with an
    /// offset.
    pub fn parse_from_rfc3339(input: &str) -> Result<Self, ParseTimestampError> {
        chrono_relaxed(input)
            .parse::<Timestamp>()
            .map(Self)
            .map_err(|error| ParseTimestampError {
                input: input.to_string(),
                reason: error.to_string(),
            })
    }

    /// The chrono `to_rfc3339()` form: `2026-11-20T07:02:23.405665746+00:00`.
    #[must_use]
    pub fn to_rfc3339(&self) -> String {
        Iso {
            at: *self,
            separator: 'T',
            suffix: "+00:00",
        }
        .to_string()
    }

    /// Format with a `strftime` pattern, in UTC.
    #[must_use]
    pub fn format(&self, pattern: &str) -> String {
        self.0.strftime(pattern).to_string()
    }

    /// The UTC calendar date of this instant.
    #[must_use]
    pub fn date(&self) -> Date {
        self.civil().date()
    }

    /// This instant plus `duration`, or `None` when the result is out of
    /// range.
    #[must_use]
    pub fn checked_add(&self, duration: SignedDuration) -> Option<Self> {
        self.0.checked_add(duration).ok().map(Self)
    }

    /// The duration from `earlier` to `self`; negative when `earlier` is
    /// later.
    #[must_use]
    pub fn signed_duration_since(&self, earlier: Self) -> SignedDuration {
        self.0.duration_since(earlier.0)
    }

    fn civil(&self) -> DateTime {
        Offset::UTC.to_datetime(self.0)
    }
}

/// `date{separator}time{fraction}{suffix}` with chrono's year and auto-SI
/// fraction rules: the one writer behind serde, `to_rfc3339`, `Debug` and
/// `Display`.
struct Iso {
    at: UtcDateTime,
    separator: char,
    suffix: &'static str,
}

impl fmt::Display for Iso {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let civil = self.at.civil();
        write_date(f, civil.date())?;
        write!(
            f,
            "{}{:02}:{:02}:{:02}",
            self.separator,
            civil.hour(),
            civil.minute(),
            civil.second()
        )?;
        write_auto_si_fraction(f, civil.subsec_nanosecond())?;
        f.write_str(self.suffix)
    }
}

/// Rewrite the relaxed forms chrono's parser accepted into ones jiff accepts:
/// surrounding whitespace, a space before the offset (`07:02:23 +00:00`),
/// chrono's own `Display` suffix (`07:02:23 UTC`), and fractions longer than
/// nine digits, which chrono truncated.
fn chrono_relaxed(input: &str) -> std::borrow::Cow<'_, str> {
    let trimmed = input.trim();
    let mut text = std::borrow::Cow::Borrowed(trimmed);
    if let Some(stem) = trimmed.strip_suffix(" UTC") {
        text = format!("{stem}Z").into();
    }
    if let Some(at) = text.rfind(' ')
        && text[at + 1..].starts_with(['+', '-'])
    {
        text = format!("{}{}", &text[..at], &text[at + 1..]).into();
    }
    if let Some(dot) = text.find('.') {
        let digits = text[dot + 1..].bytes().take_while(u8::is_ascii_digit).count();
        if digits > 9 {
            text = format!("{}{}", &text[..dot + 10], &text[dot + 1 + digits..]).into();
        }
    }
    text
}

/// chrono writes years `0..=9999` as four digits and any other year with an
/// explicit sign.
fn write_date(out: &mut impl fmt::Write, date: Date) -> fmt::Result {
    let year = date.year();
    if (0..=9999).contains(&year) {
        write!(out, "{year:04}")?;
    } else {
        write!(out, "{year:+05}")?;
    }
    write!(out, "-{:02}-{:02}", date.month(), date.day())
}

/// chrono's `SecondsFormat::AutoSi`: nothing for a whole second, otherwise
/// milli-, micro- or nanosecond precision, whichever is the shortest exact
/// form.
fn write_auto_si_fraction(out: &mut impl fmt::Write, nanos: i32) -> fmt::Result {
    if nanos == 0 {
        Ok(())
    } else if nanos % 1_000_000 == 0 {
        write!(out, ".{:03}", nanos / 1_000_000)
    } else if nanos % 1_000 == 0 {
        write!(out, ".{:06}", nanos / 1_000)
    } else {
        write!(out, ".{nanos:09}")
    }
}

impl fmt::Debug for UtcDateTime {
    /// chrono's `Debug`: `2026-11-20T07:00:01.798036936Z`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(
            &Iso {
                at: *self,
                separator: 'T',
                suffix: "Z",
            },
            f,
        )
    }
}

impl fmt::Display for UtcDateTime {
    /// chrono's `Display`: `2026-11-20 07:00:01.798036936 UTC`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(
            &Iso {
                at: *self,
                separator: ' ',
                suffix: " UTC",
            },
            f,
        )
    }
}

impl FromStr for UtcDateTime {
    type Err = ParseTimestampError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        Self::parse_from_rfc3339(input)
    }
}

impl Serialize for UtcDateTime {
    /// chrono's `Serialize`: `2026-11-20T07:00:01.798036936Z`.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&format_args!("{self:?}"))
    }
}

impl<'de> Deserialize<'de> for UtcDateTime {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = UtcDateTime;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an RFC 3339 timestamp string")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<UtcDateTime, E> {
                UtcDateTime::parse_from_rfc3339(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(Visitor)
    }
}

impl Add<SignedDuration> for UtcDateTime {
    type Output = Self;

    /// # Panics
    ///
    /// Panics when the result is out of range, as chrono's `+` did.
    fn add(self, duration: SignedDuration) -> Self {
        Self(self.0 + duration)
    }
}

impl Sub<SignedDuration> for UtcDateTime {
    type Output = Self;

    /// # Panics
    ///
    /// Panics when the result is out of range, as chrono's `-` did.
    fn sub(self, duration: SignedDuration) -> Self {
        Self(self.0 - duration)
    }
}

impl AddAssign<SignedDuration> for UtcDateTime {
    fn add_assign(&mut self, duration: SignedDuration) {
        *self = *self + duration;
    }
}

impl SubAssign<SignedDuration> for UtcDateTime {
    fn sub_assign(&mut self, duration: SignedDuration) {
        *self = *self - duration;
    }
}

impl Sub for UtcDateTime {
    type Output = SignedDuration;

    fn sub(self, earlier: Self) -> SignedDuration {
        self.signed_duration_since(earlier)
    }
}

/// A duration of `days` 24-hour days (chrono's `Duration::days`).
///
/// # Panics
///
/// Panics when the duration does not fit, as chrono's `Duration::days` did.
#[must_use]
pub fn days(days: i64) -> SignedDuration {
    SignedDuration::from_hours(days.saturating_mul(24))
}

/// Today's date in the system time zone (chrono's
/// `Local::now().date_naive()`). The system zone comes from `TZ` when set,
/// otherwise from the host configuration.
#[must_use]
pub fn local_today() -> Date {
    Timestamp::now().to_zoned_datetime(TimeZone::system()).date()
}

#[cfg(test)]
mod tests;
