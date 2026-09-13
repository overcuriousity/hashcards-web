// Copyright 2025 Fernando Borretti
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use chrono::Datelike;
use chrono::Duration;
use chrono::NaiveDate;
use chrono::Weekday;

use crate::error::Fallible;
use crate::error::fail;

/// The three-letter names, Monday first, as they appear in `hashcards.toml`
/// and in a `meta` row.
const NAMES: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// How far the shift ever has to look. With at least one open day, some day
/// within three is open in one direction or the other.
const MAX_SHIFT: i64 = 3;

/// The weekdays on which no card is scheduled to come due.
///
/// A directional cousin of `Jitter`: where jitter spreads review peaks
/// symmetrically and blindly, this moves a due date off a day you have said
/// you are not available. Like jitter it is a statement about one person's
/// week rather than about any collection, so it is never set per collection.
///
/// A bitmask, Monday in bit 0.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct FreeDays(u8);

impl FreeDays {
    /// Every day takes cards. What every schedule written before this was
    /// configurable did, and so the default.
    pub const fn none() -> FreeDays {
        FreeDays(0)
    }

    /// Refuses all seven: a week with no open day has no due date to offer,
    /// and silently ignoring it would leave a user believing their cards
    /// were scheduled somewhere.
    pub fn new(days: [bool; 7]) -> Fallible<FreeDays> {
        if days.iter().all(|d| *d) {
            return fail("at least one weekday must stay open: cards have to come due somewhere");
        }
        let mut bits = 0u8;
        for (i, free) in days.iter().enumerate() {
            if *free {
                bits |= 1 << i;
            }
        }
        Ok(FreeDays(bits))
    }

    pub fn is_free(&self, day: Weekday) -> bool {
        self.0 & (1 << day.num_days_from_monday()) != 0
    }

    /// The nearest day to `due` that takes cards.
    ///
    /// Ties resolve later: landing early means reviewing a card before the
    /// scheduler wanted it, which is the more harmful of the two errors.
    /// `not_after` outranks that preference, so a card whose interval is at
    /// the ceiling is pulled earlier rather than pushed past it.
    pub fn shift(&self, due: NaiveDate, not_after: NaiveDate) -> NaiveDate {
        if !self.is_free(due.weekday()) {
            return due;
        }
        for delta in 1..=MAX_SHIFT {
            let later = due + Duration::days(delta);
            if !self.is_free(later.weekday()) && later <= not_after {
                return later;
            }
            let earlier = due - Duration::days(delta);
            if !self.is_free(earlier.weekday()) {
                return earlier;
            }
        }
        // Unreachable while `new` refuses all seven: within three days of
        // any date, both directions have covered the whole week. Returning
        // the unshifted date is the harmless answer if it ever is reached.
        due
    }

    /// `"sat,sun"`, as written in config and in a `meta` row. The empty
    /// string is no free days.
    pub fn parse_list(s: &str) -> Fallible<FreeDays> {
        let mut days = [false; 7];
        for token in s.split(',') {
            let token = token.trim().to_lowercase();
            if token.is_empty() {
                continue;
            }
            match NAMES.iter().position(|n| *n == token) {
                Some(i) => days[i] = true,
                None => {
                    return fail(format!(
                        "{token} is not a weekday: use {}",
                        NAMES.join(", ")
                    ));
                }
            }
        }
        FreeDays::new(days)
    }

    pub fn to_list(&self) -> Vec<&'static str> {
        NAMES
            .iter()
            .enumerate()
            .filter(|(i, _)| self.0 & (1 << i) != 0)
            .map(|(_, n)| *n)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Fallible;

    /// Days after Sunday 2026-09-13, so `d(1)` is Monday and `d(7)` is the
    /// following Sunday. Added rather than substituted into the day field:
    /// `d(365)`, the far-future cap, is not September 378th.
    fn d(day: i64) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, 13).expect("valid date") + Duration::days(day)
    }

    fn free(days: &[usize]) -> FreeDays {
        let mut a = [false; 7];
        for &i in days {
            a[i] = true;
        }
        FreeDays::new(a).expect("not all seven")
    }

    #[test]
    fn no_free_days_moves_nothing() {
        let f = FreeDays::none();
        for day in 1..=7 {
            assert_eq!(f.shift(d(day), d(365)), d(day));
        }
    }

    /// Saturday and Sunday free: Saturday pulls back to Friday (one day)
    /// and Sunday pushes to Monday (one day).
    #[test]
    fn a_weekend_resolves_to_the_nearer_side() {
        let f = free(&[5, 6]);
        assert_eq!(f.shift(d(6), d(365)), d(5), "Saturday → Friday");
        assert_eq!(f.shift(d(7), d(365)), d(8), "Sunday → Monday");
    }

    /// Equidistant: Wednesday free, Tuesday and Thursday both one day
    /// away. Later wins, because landing early means reviewing a card
    /// before the scheduler wanted it.
    #[test]
    fn a_tie_resolves_later() {
        let f = free(&[2]);
        assert_eq!(f.shift(d(3), d(365)), d(4), "Wednesday → Thursday");
    }

    /// The cap outranks the tie-break: if the later day is past
    /// `not_after`, the earlier one is taken even though it is a tie.
    #[test]
    fn the_cap_wins_over_the_tie_break() {
        let f = free(&[2]);
        assert_eq!(f.shift(d(3), d(3)), d(2), "Wednesday → Tuesday");
    }

    /// Six free days: the search still terminates on the one open day.
    #[test]
    fn one_open_day_is_always_found() {
        let f = free(&[0, 1, 2, 4, 5, 6]);
        for day in 1..=7 {
            assert_eq!(f.shift(d(day), d(365)).weekday(), Weekday::Thu);
        }
    }

    #[test]
    fn all_seven_free_is_refused() {
        assert!(FreeDays::new([true; 7]).is_err());
    }

    #[test]
    fn a_list_round_trips() -> Fallible<()> {
        let f = FreeDays::parse_list("sat,sun")?;
        assert_eq!(f, free(&[5, 6]));
        assert_eq!(f.to_list(), vec!["sat", "sun"]);
        assert_eq!(FreeDays::parse_list("")?, FreeDays::none());
        assert!(FreeDays::parse_list("caturday").is_err());
        Ok(())
    }
}
