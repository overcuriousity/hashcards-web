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

use crate::error::Fallible;
use crate::error::fail;

/// How many cards a collection will hand out in one day.
///
/// `None` is no limit, which is what every session did before this existed.
/// `Some(0)` is a limit of zero. They are different answers and nothing in
/// this type or the form that feeds it may collapse them: someone who wants
/// no new cards today says zero, and someone who wants every new card says
/// nothing at all.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct DailyLimits {
    /// Cards seen at least once before.
    pub reviews: Option<u32>,
    /// Cards never reviewed.
    pub new: Option<u32>,
}

impl DailyLimits {
    /// One field as typed into the form or written in config. Blank is no
    /// limit.
    pub fn parse(s: &str) -> Fallible<Option<u32>> {
        let s = s.trim();
        if s.is_empty() {
            return Ok(None);
        }
        match s.parse::<u32>() {
            Ok(n) => Ok(Some(n)),
            Err(_) => fail(format!(
                "a daily limit must be a whole number of cards, or blank for no limit, got: {s}"
            )),
        }
    }

    /// Whichever of `self` and `fallback` has an answer, field by field.
    pub fn or(self, fallback: DailyLimits) -> DailyLimits {
        DailyLimits {
            reviews: self.reviews.or(fallback.reviews),
            new: self.new.or(fallback.new),
        }
    }
}

/// What is left of a collection's limits today.
///
/// Built once per collection per session — and identically wherever that
/// session's size is *counted*, since a count that does not match the
/// session it describes is the easiest bug to introduce here.
pub struct DailyBudget {
    reviews_left: Option<u32>,
    new_left: Option<u32>,
}

impl DailyBudget {
    pub fn new(limits: DailyLimits, reviews_done: usize, new_done: usize) -> DailyBudget {
        // Saturating: a limit lowered after the day's work was done leaves
        // nothing, rather than wrapping into a very large allowance.
        let left = |limit: Option<u32>, done: usize| {
            limit.map(|l| l.saturating_sub(done.min(u32::MAX as usize) as u32))
        };
        DailyBudget {
            reviews_left: left(limits.reviews, reviews_done),
            new_left: left(limits.new, new_done),
        }
    }

    /// Whether one more card of this kind fits, spending the budget if so.
    pub fn admits(&mut self, is_new: bool) -> bool {
        let slot = if is_new {
            &mut self.new_left
        } else {
            &mut self.reviews_left
        };
        match slot {
            None => true,
            Some(0) => false,
            Some(n) => {
                *n -= 1;
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Fallible;

    fn limits(reviews: Option<u32>, new: Option<u32>) -> DailyLimits {
        DailyLimits { reviews, new }
    }

    /// The default is today's behaviour: everything due is offered.
    #[test]
    fn no_limits_admit_everything() {
        let mut b = DailyBudget::new(DailyLimits::default(), 0, 0);
        for _ in 0..1000 {
            assert!(b.admits(false));
            assert!(b.admits(true));
        }
    }

    #[test]
    fn a_review_limit_stops_at_its_count() {
        let mut b = DailyBudget::new(limits(Some(3), None), 0, 0);
        assert_eq!((0..10).filter(|_| b.admits(false)).count(), 3);
    }

    /// A day already half spent: twenty reviews done against a limit of
    /// thirty leaves ten.
    #[test]
    fn work_already_done_today_counts_against_the_limit() {
        let mut b = DailyBudget::new(limits(Some(30), None), 20, 0);
        assert_eq!((0..50).filter(|_| b.admits(false)).count(), 10);
    }

    /// More done than the limit allows — the limit was lowered mid-day —
    /// is nothing left, not a panic on unsigned subtraction.
    #[test]
    fn overshooting_the_limit_leaves_nothing() {
        let mut b = DailyBudget::new(limits(Some(5), None), 40, 0);
        assert!(!b.admits(false));
    }

    /// The two budgets are separate: a new card spends the new budget, a
    /// card seen before spends the review budget.
    #[test]
    fn new_and_review_budgets_are_separate() {
        let mut b = DailyBudget::new(limits(Some(2), Some(1)), 0, 0);
        assert!(b.admits(true), "first new card");
        assert!(!b.admits(true), "new budget spent");
        assert!(b.admits(false), "review budget untouched");
        assert!(b.admits(false));
        assert!(!b.admits(false));
    }

    /// Falling back is per field, not per value: a layer that sets only the
    /// new-card limit must not drop the review limit it inherits.
    #[test]
    fn falling_back_is_field_by_field() {
        let mine = limits(None, Some(3));
        let theirs = limits(Some(100), Some(10));
        assert_eq!(mine.or(theirs), limits(Some(100), Some(3)));
    }

    /// A zero set here must survive the fallback, rather than reading as
    /// "no answer" and inheriting.
    #[test]
    fn a_zero_is_an_answer_when_falling_back() {
        assert_eq!(
            limits(Some(0), None).or(limits(Some(100), None)),
            limits(Some(0), None)
        );
    }

    /// Zero is a real answer and must not read as "no limit". Someone who
    /// wants no new cards today says zero.
    #[test]
    fn zero_is_a_limit_and_blank_is_not() -> Fallible<()> {
        let mut b = DailyBudget::new(limits(None, Some(0)), 0, 0);
        assert!(!b.admits(true));
        assert!(b.admits(false));

        assert_eq!(DailyLimits::parse("0")?, Some(0));
        assert_eq!(DailyLimits::parse("")?, None);
        assert_eq!(DailyLimits::parse("  ")?, None);
        assert_eq!(DailyLimits::parse("40")?, Some(40));
        assert!(DailyLimits::parse("-1").is_err());
        assert!(DailyLimits::parse("lots").is_err());
        Ok(())
    }
}
