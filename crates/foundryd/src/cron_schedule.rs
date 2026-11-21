//! Cron expressions and their next firing, in a caller-chosen time zone.
//!
//! This module replaces the `cron` 0.17 crate (which depends on chrono) with a
//! small parser and evaluator built on jiff. It reproduces the behaviour of
//! `cron` 0.17 as the sentinel scheduler used it, including the parts that
//! differ from Vixie cron:
//!
//! - Five fields (`min hour dom month dow`, seconds implied `0`) or six fields
//!   (`sec min hour dom month dow`), separated by whitespace.
//! - Day of week runs `1..=7` with **1 = Sunday** and 7 = Saturday (Quartz
//!   numbering). `0` is out of range. `"0 9 * * 1"` fires on Sundays.
//! - Day of month and day of week combine with AND: a day matches only when it
//!   is in both sets. Vixie cron uses OR when both are restricted.
//! - `a/n` means "from `a` to the field maximum, every `n`". A step larger than
//!   the field maximum is rejected (so `*/60` is invalid for minutes).
//! - `?` is accepted in the day-of-month and day-of-week fields only, and means
//!   the same as `*`.
//! - Month and weekday names are case-insensitive. Months accept `jan` and
//!   `january`; weekdays accept `sun`/`sunday`, `tue`/`tues`/`tuesday` and
//!   `thu`/`thurs`/`thursday`. A name cannot start a step (`mon/2` is invalid,
//!   `mon-fri/2` is valid).
//!
//! Not carried over from `cron` 0.17: the optional seventh (year) field and the
//! `@daily`-style shorthands. No sentinel uses them.

use std::fmt;
use std::str::FromStr;

use jiff::civil::{Date, DateTime};
use jiff::tz::{AmbiguousOffset, TimeZone};
use jiff::{SignedDuration, ZonedDateTime};

/// The last year searched for a firing. `cron` 0.17 stopped at 2100 too, so an
/// expression that cannot fire (such as `"0 0 31 2 *"`) yields `None` after a
/// bounded day-by-day scan rather than looping forever.
const LAST_SEARCHED_YEAR: i16 = 2100;

/// A parsed cron expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronSchedule {
    seconds: FieldSet,
    minutes: FieldSet,
    hours: FieldSet,
    days_of_month: FieldSet,
    months: FieldSet,
    days_of_week: FieldSet,
}

/// Why a cron expression could not be parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronParseError(String);

impl fmt::Display for CronParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CronParseError {}

impl FromStr for CronSchedule {
    type Err = CronParseError;

    fn from_str(expression: &str) -> Result<Self, Self::Err> {
        let fields: Vec<&str> = expression.split_whitespace().collect();
        let [seconds, minutes, hours, days_of_month, months, days_of_week] = match fields[..] {
            [min, hour, dom, month, dow] => ["0", min, hour, dom, month, dow],
            [sec, min, hour, dom, month, dow] => [sec, min, hour, dom, month, dow],
            _ => {
                return Err(CronParseError(format!(
                    "expected 5 fields (min hour dom month dow) or 6 fields \
                     (sec min hour dom month dow), found {}",
                    fields.len()
                )));
            }
        };
        Ok(Self {
            seconds: SECONDS.parse(seconds)?,
            minutes: MINUTES.parse(minutes)?,
            hours: HOURS.parse(hours)?,
            days_of_month: DAYS_OF_MONTH.parse(days_of_month)?,
            months: MONTHS.parse(months)?,
            days_of_week: DAYS_OF_WEEK.parse(days_of_week)?,
        })
    }
}

impl CronSchedule {
    /// The first firing strictly after `now`, in `now`'s time zone, or `None`
    /// if the expression never fires on or before 31 December 2100.
    ///
    /// Candidates are wall-clock times in `now`'s zone, searched in order from
    /// the whole second after `now` (any sub-second part of `now` is dropped
    /// once that second is added, as in `cron` 0.17).
    ///
    /// Daylight saving time follows what `cron` 0.17 did with chrono's `Local`
    /// zone, which is how the sentinel scheduler ran before:
    ///
    /// - In a spring-forward gap, the first wall-clock second of the gap maps
    ///   to the transition instant (chrono resolved it with the old offset),
    ///   and every later second of the gap is skipped. So in America/Toronto a
    ///   02:00 nightly fires at 03:00 EDT on the spring-forward day, while a
    ///   02:30 schedule does not fire that day.
    /// - In a fall-back fold, a repeated wall-clock time resolves to its later
    ///   (standard-time) instant, because chrono listed the standard-time
    ///   reading first and `cron` 0.17 took the first.
    #[must_use]
    pub fn next_after(&self, now: &ZonedDateTime) -> Option<ZonedDateTime> {
        let time_zone = now.time_zone();
        let search_start = now
            .timestamp()
            .checked_add(SignedDuration::from_secs(1))
            .ok()?
            .to_zoned_datetime(time_zone.clone())
            .datetime();

        let mut day = search_start.date();
        let mut earliest = TimeOfDay {
            hour: search_start.hour(),
            minute: search_start.minute(),
            second: search_start.second(),
        };
        while day.year() <= LAST_SEARCHED_YEAR {
            if self.matches_day(day) {
                let firing = self
                    .times_from(earliest)
                    .find_map(|t| resolve(time_zone, day.at(t.hour, t.minute, t.second, 0), now));
                if firing.is_some() {
                    return firing;
                }
            }
            earliest = TimeOfDay::MIDNIGHT;
            day = day.tomorrow().ok()?;
        }
        None
    }

    fn matches_day(&self, day: Date) -> bool {
        self.months.contains(day.month())
            && self.days_of_month.contains(day.day())
            && self.days_of_week.contains(day.weekday().to_sunday_one_offset())
    }

    /// Every time of day in the schedule at or after `earliest`, in order.
    fn times_from(&self, earliest: TimeOfDay) -> impl Iterator<Item = TimeOfDay> + '_ {
        self.hours.values_from(earliest.hour).flat_map(move |hour| {
            let first_minute = if hour == earliest.hour {
                earliest.minute
            } else {
                0
            };
            self.minutes.values_from(first_minute).flat_map(move |minute| {
                let first_second = if hour == earliest.hour && minute == earliest.minute {
                    earliest.second
                } else {
                    0
                };
                self.seconds.values_from(first_second).map(move |second| TimeOfDay {
                    hour,
                    minute,
                    second,
                })
            })
        })
    }
}

/// The instant for a candidate wall-clock time, or `None` if it cannot fire.
/// See [`CronSchedule::next_after`] for the gap and fold rules.
fn resolve(time_zone: &TimeZone, civil: DateTime, now: &ZonedDateTime) -> Option<ZonedDateTime> {
    let ambiguous = time_zone.to_ambiguous_zoned_datetime(civil);
    let resolved = match ambiguous.offset() {
        AmbiguousOffset::Unambiguous { .. } => ambiguous.unambiguous().ok()?,
        AmbiguousOffset::Fold { .. } => ambiguous.later().ok()?,
        AmbiguousOffset::Gap { before, .. } => {
            let previous_second = civil.checked_sub(SignedDuration::from_secs(1)).ok()?;
            let gap_starts_here = !matches!(
                time_zone.to_ambiguous_zoned_datetime(previous_second).offset(),
                AmbiguousOffset::Gap { .. }
            );
            if !gap_starts_here {
                return None;
            }
            before.to_timestamp(civil).ok()?.to_zoned_datetime(time_zone.clone())
        }
    };
    (resolved > *now).then_some(resolved)
}

#[derive(Clone, Copy, Debug)]
struct TimeOfDay {
    hour: i8,
    minute: i8,
    second: i8,
}

impl TimeOfDay {
    const MIDNIGHT: Self = Self {
        hour: 0,
        minute: 0,
        second: 0,
    };
}

/// The values a field matches. Bit `n` is set when value `n` matches; every
/// field's values fit in `0..=59`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FieldSet(u64);

impl FieldSet {
    const LARGEST_VALUE: i8 = 59;

    fn contains(self, value: i8) -> bool {
        u32::try_from(value).is_ok_and(|bit| bit < 64 && self.0 & (1 << bit) != 0)
    }

    fn insert(&mut self, value: i8) {
        if let Ok(bit) = u32::try_from(value) {
            self.0 |= 1 << bit;
        }
    }

    fn values_from(self, start: i8) -> impl Iterator<Item = i8> {
        (start..=Self::LARGEST_VALUE).filter(move |&value| self.contains(value))
    }
}

/// The rules for one field of an expression.
struct FieldSpec {
    name: &'static str,
    min: i8,
    max: i8,
    names: &'static [(&'static str, i8)],
    accepts_question_mark: bool,
}

const SECONDS: FieldSpec = FieldSpec {
    name: "seconds",
    min: 0,
    max: 59,
    names: &[],
    accepts_question_mark: false,
};

const MINUTES: FieldSpec = FieldSpec {
    name: "minutes",
    min: 0,
    max: 59,
    names: &[],
    accepts_question_mark: false,
};

const HOURS: FieldSpec = FieldSpec {
    name: "hours",
    min: 0,
    max: 23,
    names: &[],
    accepts_question_mark: false,
};

const DAYS_OF_MONTH: FieldSpec = FieldSpec {
    name: "day of month",
    min: 1,
    max: 31,
    names: &[],
    accepts_question_mark: true,
};

const MONTHS: FieldSpec = FieldSpec {
    name: "month",
    min: 1,
    max: 12,
    names: &[
        ("jan", 1),
        ("january", 1),
        ("feb", 2),
        ("february", 2),
        ("mar", 3),
        ("march", 3),
        ("apr", 4),
        ("april", 4),
        ("may", 5),
        ("jun", 6),
        ("june", 6),
        ("jul", 7),
        ("july", 7),
        ("aug", 8),
        ("august", 8),
        ("sep", 9),
        ("september", 9),
        ("oct", 10),
        ("october", 10),
        ("nov", 11),
        ("november", 11),
        ("dec", 12),
        ("december", 12),
    ],
    accepts_question_mark: false,
};

const DAYS_OF_WEEK: FieldSpec = FieldSpec {
    name: "day of week",
    min: 1,
    max: 7,
    names: &[
        ("sun", 1),
        ("sunday", 1),
        ("mon", 2),
        ("monday", 2),
        ("tue", 3),
        ("tues", 3),
        ("tuesday", 3),
        ("wed", 4),
        ("wednesday", 4),
        ("thu", 5),
        ("thurs", 5),
        ("thursday", 5),
        ("fri", 6),
        ("friday", 6),
        ("sat", 7),
        ("saturday", 7),
    ],
    accepts_question_mark: true,
};

impl FieldSpec {
    /// Parse a comma-separated list of items into the set of values it matches.
    fn parse(&self, field: &str) -> Result<FieldSet, CronParseError> {
        let mut set = FieldSet(0);
        for item in field.split(',') {
            let (first, last, step) = self.parse_item(item)?;
            for value in (first..=last).step_by(step) {
                set.insert(value);
            }
        }
        Ok(set)
    }

    /// One list item as an inclusive range and a step.
    fn parse_item(&self, item: &str) -> Result<(i8, i8, usize), CronParseError> {
        let Some((base, step)) = item.split_once('/') else {
            return self.parse_base(item, false).map(|(first, last)| (first, last, 1));
        };
        let step = self.parse_step(step)?;
        let (first, last) = self.parse_base(base, true)?;
        Ok((first, last, step))
    }

    /// `*`, `?`, a range, a number or a name. Before a step, a lone number
    /// runs to the field maximum and a lone name is rejected.
    fn parse_base(&self, base: &str, before_step: bool) -> Result<(i8, i8), CronParseError> {
        if base == "*" || (base == "?" && self.accepts_question_mark) {
            return Ok((self.min, self.max));
        }
        if let Some((first, last)) = base.split_once('-') {
            return self.parse_range(first, last);
        }
        if is_number(base) {
            let value = self.parse_number(base)?;
            return Ok((value, if before_step { self.max } else { value }));
        }
        if is_name(base) && !before_step {
            let value = self.parse_name(base)?;
            return Ok((value, value));
        }
        Err(self.error(format!("'{base}' is not a valid value")))
    }

    fn parse_range(&self, first: &str, last: &str) -> Result<(i8, i8), CronParseError> {
        let (start, end) = if is_number(first) && is_number(last) {
            (self.parse_number(first)?, self.parse_number(last)?)
        } else if is_name(first) && is_name(last) {
            (self.parse_name(first)?, self.parse_name(last)?)
        } else {
            return Err(self.error(format!("'{first}-{last}' is not a valid range")));
        };
        if start > end {
            return Err(self.error(format!(
                "range '{first}-{last}' runs backwards; the start must not exceed the end"
            )));
        }
        Ok((start, end))
    }

    fn parse_step(&self, step: &str) -> Result<usize, CronParseError> {
        if !is_number(step) {
            return Err(self.error(format!("step '{step}' is not a number")));
        }
        let step: u32 =
            step.parse().map_err(|_| self.error(format!("step '{step}' is too large")))?;
        if step == 0 {
            return Err(self.error("step cannot be zero"));
        }
        if step > u32::from(self.max.unsigned_abs()) {
            return Err(
                self.error(format!("step must be between 1 and {}, found {step}", self.max))
            );
        }
        usize::try_from(step).map_err(|_| self.error(format!("step '{step}' is too large")))
    }

    fn parse_number(&self, text: &str) -> Result<i8, CronParseError> {
        text.parse::<i8>()
            .ok()
            .filter(|value| (self.min..=self.max).contains(value))
            .ok_or_else(|| {
                self.error(format!(
                    "{text} is out of range; values must be {}-{}",
                    self.min, self.max
                ))
            })
    }

    fn parse_name(&self, name: &str) -> Result<i8, CronParseError> {
        if self.names.is_empty() {
            return Err(self.error(format!("names are not allowed here, found '{name}'")));
        }
        let lowercase = name.to_ascii_lowercase();
        self.names
            .iter()
            .find(|(known, _)| *known == lowercase)
            .map(|&(_, value)| value)
            .ok_or_else(|| self.error(format!("'{name}' is not a recognised name")))
    }

    fn error(&self, reason: impl fmt::Display) -> CronParseError {
        CronParseError(format!("{} field: {reason}", self.name))
    }
}

fn is_number(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

fn is_name(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use jiff::civil::Weekday;

    use super::*;

    const TORONTO: &str = "America/Toronto";

    /// A Toronto wall-clock time. Ambiguous times resolve to the earlier
    /// instant; write the offset explicitly with [`exact`] when it matters.
    fn toronto(y: i16, mo: i8, d: i8, h: i8, mi: i8, s: i8) -> ZonedDateTime {
        jiff::civil::date(y, mo, d)
            .at(h, mi, s, 0)
            .to_zoned_datetime(TimeZone::get(TORONTO).unwrap())
            .unwrap()
    }

    /// A zoned time from its RFC 9557 form, offset included.
    fn exact(text: &str) -> ZonedDateTime {
        text.parse().unwrap()
    }

    fn next(expression: &str, now: &ZonedDateTime) -> ZonedDateTime {
        schedule(expression).next_after(now).unwrap()
    }

    fn schedule(expression: &str) -> CronSchedule {
        expression.parse().unwrap()
    }

    fn parse_error(expression: &str) -> String {
        expression.parse::<CronSchedule>().unwrap_err().to_string()
    }

    // ---------------------------------------------------------------------
    // Expressions in use by sentinels today
    // ---------------------------------------------------------------------

    #[test]
    fn nightly_at_two_fires_today_when_before_two() {
        let now = toronto(2026, 11, 20, 1, 0, 0);
        assert_eq!(next("0 2 * * *", &now), toronto(2026, 11, 20, 2, 0, 0));
    }

    #[test]
    fn nightly_at_two_rolls_to_tomorrow_when_after_two() {
        let now = toronto(2026, 11, 20, 3, 0, 0);
        assert_eq!(next("0 2 * * *", &now), toronto(2026, 11, 21, 2, 0, 0));
    }

    #[test]
    fn evening_at_seventeen() {
        let now = toronto(2026, 11, 20, 17, 0, 1);
        assert_eq!(next("0 17 * * *", &now), toronto(2026, 11, 21, 17, 0, 0));
    }

    #[test]
    fn every_three_hours_on_the_hour() {
        let now = toronto(2026, 11, 20, 10, 15, 0);
        assert_eq!(next("0 */3 * * *", &now), toronto(2026, 11, 20, 12, 0, 0));
        let late = toronto(2026, 11, 20, 21, 0, 0);
        assert_eq!(next("0 */3 * * *", &late), toronto(2026, 11, 21, 0, 0, 0));
    }

    #[test]
    fn morning_at_six() {
        let now = toronto(2026, 11, 20, 5, 59, 59);
        assert_eq!(next("0 6 * * *", &now), toronto(2026, 11, 20, 6, 0, 0));
    }

    #[test]
    fn every_three_hours_at_half_past() {
        let now = toronto(2026, 11, 20, 3, 30, 0);
        assert_eq!(next("30 */3 * * *", &now), toronto(2026, 11, 20, 6, 30, 0));
    }

    #[test]
    fn weekly_at_four_past_seven_on_sunday() {
        // 2026-11-20 is a Friday; dow 1 is Sunday.
        let now = toronto(2026, 11, 20, 12, 0, 0);
        let fire = next("7 4 * * 1", &now);
        assert_eq!(fire, toronto(2026, 11, 22, 4, 7, 0));
        assert_eq!(fire.weekday(), Weekday::Sunday);
    }

    #[test]
    fn every_six_hours_at_eleven_past() {
        let now = toronto(2026, 11, 20, 6, 11, 0);
        assert_eq!(next("11 */6 * * *", &now), toronto(2026, 11, 20, 12, 11, 0));
    }

    #[test]
    fn every_five_minutes() {
        let now = toronto(2026, 11, 20, 23, 57, 3);
        assert_eq!(next("*/5 * * * *", &now), toronto(2026, 11, 21, 0, 0, 0));
    }

    #[test]
    fn nine_on_dow_one_fires_on_sunday_not_monday() {
        let now = toronto(2026, 11, 20, 12, 0, 0);
        let fire = next("0 9 * * 1", &now);
        assert_eq!(fire, toronto(2026, 11, 22, 9, 0, 0));
        assert_eq!(fire.weekday(), Weekday::Sunday);
    }

    #[test]
    fn six_field_form_is_accepted_directly() {
        let now = toronto(2026, 11, 20, 1, 0, 0);
        assert_eq!(next("0 0 2 * * *", &now), toronto(2026, 11, 20, 2, 0, 0));
        assert_eq!(schedule("0 0 2 * * *"), schedule("0 2 * * *"));
    }

    #[test]
    fn surrounding_and_repeated_whitespace_is_ignored() {
        assert_eq!(schedule("  0   2 *\t* *  "), schedule("0 2 * * *"));
    }

    #[test]
    fn six_field_seconds_are_honoured() {
        let now = toronto(2026, 11, 20, 1, 0, 0);
        assert_eq!(next("*/20 * * * * *", &now), toronto(2026, 11, 20, 1, 0, 20));
    }

    // ---------------------------------------------------------------------
    // Strictly after now
    // ---------------------------------------------------------------------

    #[test]
    fn now_exactly_on_a_firing_returns_the_following_firing() {
        let now = toronto(2026, 11, 20, 2, 0, 0);
        assert_eq!(next("0 2 * * *", &now), toronto(2026, 11, 21, 2, 0, 0));
    }

    #[test]
    fn now_a_fraction_past_a_firing_returns_the_following_firing() {
        let now = exact("2026-11-20T02:00:00.4-05:00[America/Toronto]");
        assert_eq!(next("0 2 * * *", &now), toronto(2026, 11, 21, 2, 0, 0));
    }

    #[test]
    fn now_a_fraction_before_a_firing_returns_that_firing() {
        let now = exact("2026-11-20T01:59:59.999-05:00[America/Toronto]");
        assert_eq!(next("0 2 * * *", &now), toronto(2026, 11, 20, 2, 0, 0));
    }

    #[test]
    fn every_second_moves_forward_one_whole_second() {
        let now = exact("2026-11-20T10:00:00.5-05:00[America/Toronto]");
        assert_eq!(next("* * * * * *", &now), toronto(2026, 11, 20, 10, 0, 1));
    }

    #[test]
    fn result_is_in_the_time_zone_of_now() {
        let now = toronto(2026, 11, 20, 1, 0, 0);
        let fire = next("0 2 * * *", &now);
        assert_eq!(fire.time_zone(), now.time_zone());
    }

    // ---------------------------------------------------------------------
    // Field semantics
    // ---------------------------------------------------------------------

    #[test]
    fn day_of_week_seven_is_saturday() {
        let now = toronto(2026, 11, 20, 12, 0, 0);
        assert_eq!(next("0 9 * * 7", &now).weekday(), Weekday::Saturday);
    }

    #[test]
    fn day_of_month_and_day_of_week_must_both_match() {
        // The 13th that is also a Friday (dow 6): 2027-08-13.
        let now = toronto(2026, 11, 20, 12, 0, 0);
        let fire = next("0 0 13 * 6", &now);
        assert_eq!(fire, toronto(2027, 8, 13, 0, 0, 0));
        assert_eq!(fire.weekday(), Weekday::Friday);
    }

    #[test]
    fn question_mark_means_any_day() {
        assert_eq!(schedule("0 9 ? * 2"), schedule("0 9 * * 2"));
        assert_eq!(schedule("0 9 1 * ?"), schedule("0 9 1 * *"));
    }

    #[test]
    fn month_and_day_names_are_case_insensitive_and_match_numbers() {
        assert_eq!(schedule("0 9 * JAN,Feb mon"), schedule("0 9 * 1,2 2"));
        assert_eq!(schedule("0 9 * december Tues"), schedule("0 9 * 12 3"));
        assert_eq!(schedule("0 9 * * THURS"), schedule("0 9 * * 5"));
        assert_eq!(schedule("0 9 * * sun-sat"), schedule("0 9 * * *"));
    }

    #[test]
    fn named_range_with_a_step() {
        // mon-fri/2 is Monday, Wednesday, Friday.
        assert_eq!(schedule("0 9 * * mon-fri/2"), schedule("0 9 * * 2,4,6"));
    }

    #[test]
    fn number_before_step_runs_to_the_field_maximum() {
        assert_eq!(schedule("10/20 * * * *"), schedule("10,30,50 * * * *"));
    }

    #[test]
    fn range_with_a_step() {
        assert_eq!(schedule("0 1-10/4 * * *"), schedule("0 1,5,9 * * *"));
    }

    #[test]
    fn star_step_starts_at_the_field_minimum() {
        assert_eq!(schedule("0 0 */10 * *"), schedule("0 0 1,11,21,31 * *"));
        assert_eq!(schedule("0 0 * * */3"), schedule("0 0 * * 1,4,7"));
    }

    #[test]
    fn lists_mix_points_ranges_and_steps() {
        assert_eq!(schedule("0,15-17,50/5 * * * *"), schedule("0,15,16,17,50,55 * * * *"));
    }

    #[test]
    fn short_months_skip_missing_days() {
        let now = toronto(2027, 1, 31, 12, 0, 0);
        assert_eq!(next("0 0 31 * *", &now), toronto(2027, 3, 31, 0, 0, 0));
    }

    #[test]
    fn leap_day_waits_for_a_leap_year() {
        let now = toronto(2026, 11, 20, 12, 0, 0);
        assert_eq!(next("0 0 29 2 *", &now), toronto(2028, 2, 29, 0, 0, 0));
    }

    #[test]
    fn impossible_expression_never_fires() {
        let now = toronto(2026, 11, 20, 12, 0, 0);
        assert_eq!(schedule("0 0 31 2 *").next_after(&now), None);
        assert_eq!(schedule("0 0 30 2 *").next_after(&now), None);
    }

    #[test]
    fn search_stops_after_2100() {
        let now = toronto(2100, 12, 31, 23, 59, 0);
        assert_eq!(schedule("0 0 * * *").next_after(&now), None);
    }

    // ---------------------------------------------------------------------
    // Parse errors
    // ---------------------------------------------------------------------

    #[test]
    fn wrong_field_count_is_rejected() {
        assert!(parse_error("").contains("found 0"));
        assert!(parse_error("0 2 * *").contains("found 4"));
        assert!(parse_error("0 0 2 * * * 2027").contains("found 7"));
        assert!(parse_error("definitely not cron").contains("found 3"));
    }

    #[test]
    fn out_of_range_values_are_rejected() {
        assert!(parse_error("60 * * * *").contains("minutes field"));
        assert!(parse_error("0 24 * * *").contains("hours field"));
        assert!(parse_error("0 0 0 * *").contains("day of month field"));
        assert!(parse_error("0 0 32 * *").contains("day of month field"));
        assert!(parse_error("0 0 * 13 *").contains("month field"));
        assert!(parse_error("0 0 * 0 *").contains("month field"));
        assert!(parse_error("60 0 0 * * *").contains("seconds field"));
        assert!(parse_error("0 0 * * 8").contains("day of week field"));
        assert!(parse_error("0 0 * * 99999999999").contains("out of range"));
    }

    #[test]
    fn day_of_week_zero_is_rejected() {
        let error = parse_error("0 9 * * 0");
        assert!(error.contains("day of week field"), "{error}");
        assert!(error.contains("1-7"), "{error}");
    }

    #[test]
    fn zero_step_is_rejected() {
        assert!(parse_error("*/0 * * * *").contains("step cannot be zero"));
    }

    #[test]
    fn step_larger_than_field_maximum_is_rejected() {
        assert!(parse_error("*/60 * * * *").contains("step must be between 1 and 59"));
        assert!(parse_error("0 */24 * * *").contains("step must be between 1 and 23"));
        assert!(parse_error("*/x * * * *").contains("not a number"));
    }

    #[test]
    fn backwards_range_is_rejected() {
        assert!(parse_error("0 5-1 * * *").contains("runs backwards"));
        assert!(parse_error("0 9 * * fri-mon").contains("runs backwards"));
    }

    #[test]
    fn malformed_items_are_rejected() {
        assert!(parse_error("0 -5 * * *").contains("not a valid range"));
        assert!(parse_error("0 1-2-3 * * *").contains("not a valid range"));
        assert!(parse_error("0 1-mon * * *").contains("not a valid range"));
        assert!(parse_error("0 1, * * *").contains("not a valid value"));
        assert!(parse_error("0 5/ * * *").contains("not a number"));
        assert!(parse_error("0 9 * * mon/2").contains("not a valid value"));
        assert!(parse_error("0 9 * * 1.5").contains("not a valid value"));
    }

    #[test]
    fn question_mark_outside_day_fields_is_rejected() {
        assert!(parse_error("? * * * *").contains("minutes field"));
        assert!(parse_error("0 0 * ? *").contains("month field"));
    }

    #[test]
    fn names_are_rejected_where_unsupported_or_unknown() {
        assert!(parse_error("0 noon * * *").contains("names are not allowed"));
        assert!(parse_error("0 9 * * funday").contains("not a recognised name"));
        assert!(parse_error("0 9 * smarch *").contains("not a recognised name"));
    }

    // ---------------------------------------------------------------------
    // Daylight saving time in America/Toronto
    // ---------------------------------------------------------------------

    #[test]
    fn nightly_fires_at_the_transition_instant_on_the_spring_forward_day() {
        // 2027-03-14 02:00 EST jumps to 03:00 EDT. chrono's Local zone read
        // 02:00:00 with the old offset, so the cron 0.17 nightly fired at the
        // transition instant, 03:00 EDT, and this keeps that.
        let now = toronto(2027, 3, 14, 1, 0, 0);
        assert_eq!(next("0 2 * * *", &now), exact("2027-03-14T03:00:00-04:00[America/Toronto]"));
        // The day after is an ordinary 02:00 EDT.
        let fired = next("0 2 * * *", &now);
        assert_eq!(next("0 2 * * *", &fired), exact("2027-03-15T02:00:00-04:00[America/Toronto]"));
    }

    #[test]
    fn a_time_inside_the_spring_forward_gap_is_skipped_that_day() {
        let now = toronto(2027, 3, 14, 1, 0, 0);
        assert_eq!(next("30 2 * * *", &now), exact("2027-03-15T02:30:00-04:00[America/Toronto]"));
    }

    #[test]
    fn half_hourly_crosses_the_spring_forward_gap() {
        // 02:00 maps to the transition instant (03:00 EDT); 02:30 is skipped;
        // the next 03:00 EDT is the same instant, so it does not fire twice.
        let now = exact("2027-03-14T01:30:00-05:00[America/Toronto]");
        let first = next("*/30 * * * *", &now);
        assert_eq!(first, exact("2027-03-14T03:00:00-04:00[America/Toronto]"));
        assert_eq!(
            next("*/30 * * * *", &first),
            exact("2027-03-14T03:30:00-04:00[America/Toronto]")
        );
    }

    #[test]
    fn fall_back_fires_at_the_repeated_hours_standard_time_reading() {
        // 2026-11-01 01:00-01:59 happens twice: first EDT (-04), then EST
        // (-05). cron 0.17 on chrono's Local zone took the EST reading.
        let before = toronto(2026, 11, 1, 0, 0, 0);
        let first = next("30 1 * * *", &before);
        assert_eq!(first, exact("2026-11-01T01:30:00-05:00[America/Toronto]"));
        assert_eq!(next("30 1 * * *", &first), exact("2026-11-02T01:30:00-05:00[America/Toronto]"));
    }

    #[test]
    fn fall_back_from_inside_the_repeated_hour_follows_the_wall_clock() {
        let now = exact("2026-11-01T01:10:00-05:00[America/Toronto]");
        assert_eq!(next("30 1 * * *", &now), exact("2026-11-01T01:30:00-05:00[America/Toronto]"));
        // From 01:40 EDT the wall clock has passed 01:30, so the search moves
        // to the next day even though 01:30 EST is still ahead (cron 0.17's
        // wall-clock order).
        let now = exact("2026-11-01T01:40:00-04:00[America/Toronto]");
        assert_eq!(next("30 1 * * *", &now), exact("2026-11-02T01:30:00-05:00[America/Toronto]"));
    }

    #[test]
    fn nightly_on_the_fall_back_day_fires_once_at_two_standard_time() {
        let now = toronto(2026, 11, 1, 0, 30, 0);
        assert_eq!(next("0 2 * * *", &now), exact("2026-11-01T02:00:00-05:00[America/Toronto]"));
    }

    #[test]
    fn half_hourly_through_fall_back_follows_wall_clock_order() {
        // From 00:30 EDT the 01:00 and 01:30 readings resolve to EST, so the
        // EDT pass of the repeated hour does not fire.
        let now = exact("2026-11-01T00:30:00-04:00[America/Toronto]");
        let first = next("*/30 * * * *", &now);
        assert_eq!(first, exact("2026-11-01T01:00:00-05:00[America/Toronto]"));
        let second = next("*/30 * * * *", &first);
        assert_eq!(second, exact("2026-11-01T01:30:00-05:00[America/Toronto]"));
        assert_eq!(
            next("*/30 * * * *", &second),
            exact("2026-11-01T02:00:00-05:00[America/Toronto]")
        );
    }

    #[test]
    fn utc_has_no_gap() {
        let now = exact("2027-03-14T01:00:00+00:00[UTC]");
        assert_eq!(next("0 2 * * *", &now), exact("2027-03-14T02:00:00+00:00[UTC]"));
    }

    // Every expression here was also rejected by cron 0.17 when this module
    // replaced it (checked against the crate before it left the tree).
    #[test]
    fn rejects_what_cron_0_17_rejected() {
        let rejected = [
            "0 2 * *",
            "60 * * * *",
            "0 24 * * *",
            "0 0 0 * *",
            "0 0 32 * *",
            "0 0 * 13 *",
            "0 0 * 0 *",
            "0 0 * * 8",
            "0 9 * * 0",
            "*/0 * * * *",
            "*/60 * * * *",
            "0 */24 * * *",
            "0 5-1 * * *",
            "0 9 * * fri-mon",
            "0 -5 * * *",
            "0 1-2-3 * * *",
            "0 1-mon * * *",
            "0 1, * * *",
            "0 5/ * * *",
            "0 9 * * mon/2",
            "0 9 * * 1.5",
            "? * * * *",
            "0 0 * ? *",
            "0 noon * * *",
            "0 9 * * funday",
        ];
        for expression in rejected {
            assert!(expression.parse::<CronSchedule>().is_err(), "accepted '{expression}'");
        }
    }
}
