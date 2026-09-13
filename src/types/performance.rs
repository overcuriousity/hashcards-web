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

use chrono::Duration;
use chrono::NaiveDate;

use crate::error::Fallible;
use crate::error::fail;
use crate::fsrs::Difficulty;
use crate::fsrs::Grade;
use crate::fsrs::Interval;
use crate::fsrs::Recall;
use crate::fsrs::Stability;
use crate::fsrs::initial_difficulty;
use crate::fsrs::initial_stability;
use crate::fsrs::interval;
use crate::fsrs::new_difficulty;
use crate::fsrs::new_stability;
use crate::fsrs::retrievability;
use crate::rng::TinyRng;
use crate::types::date::Date;
use crate::types::free_days::FreeDays;
use crate::types::timestamp::Timestamp;

/// The minimum review interval in days.
const MIN_INTERVAL: f64 = 1.0;

/// The recall probability a schedule aims for: the chance that a card is
/// still remembered when it comes back.
///
/// It is the one FSRS number a person has an opinion about, because it names
/// the trade the algorithm exists to make. Raising it shortens every
/// interval — more reviews, more of them remembered; lowering it lengthens
/// them. Bounded at both ends because the interval formula degenerates
/// outside them: at 1.0 it asks for an interval of zero.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DesiredRetention(f64);

impl DesiredRetention {
    /// FSRS's own default, and what every schedule written before this was
    /// configurable used.
    pub const DEFAULT: f64 = 0.9;

    pub fn new(retention: f64) -> Fallible<DesiredRetention> {
        if !retention.is_finite() || !(0.7..=0.99).contains(&retention) {
            return fail(format!(
                "desired retention must be a number between 0.7 and 0.99, got: {retention}"
            ));
        }
        Ok(DesiredRetention(retention))
    }

    pub fn into_inner(self) -> f64 {
        self.0
    }
}

impl Default for DesiredRetention {
    fn default() -> Self {
        DesiredRetention(Self::DEFAULT)
    }
}

/// The ceiling on a review interval, in days.
///
/// FSRS will happily leave a well-known card for years. A ceiling says how
/// far ahead a schedule is willing to plan — which is a statement about the
/// person, not the card: material you intend to keep for a career tolerates
/// a longer one than material you need until an exam.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MaxInterval(f64);

impl MaxInterval {
    /// What the hardcoded ceiling was, kept as the default so that making it
    /// configurable moves nobody's existing schedule.
    pub const DEFAULT: f64 = 256.0;

    /// A hundred years. Past this the due date is fiction.
    pub const LIMIT: f64 = 36500.0;

    pub fn new(days: f64) -> Fallible<MaxInterval> {
        if !days.is_finite() || !(MIN_INTERVAL..=Self::LIMIT).contains(&days) {
            return fail(format!(
                "the maximum interval must be a number of days between {} and {}, got: {days}",
                MIN_INTERVAL,
                Self::LIMIT
            ));
        }
        Ok(MaxInterval(days))
    }

    pub fn into_inner(self) -> f64 {
        self.0
    }
}

impl Default for MaxInterval {
    fn default() -> Self {
        MaxInterval(Self::DEFAULT)
    }
}

/// Everything that shapes an interval besides the card's own history.
///
/// Carried as one value rather than as three arguments: each of these is
/// configurable per collection, and a card is scheduled by the collection it
/// belongs to however it was reached.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Scheduling {
    pub retention: DesiredRetention,
    pub max_interval: MaxInterval,
    pub jitter: Jitter,
    /// The weekdays that take no due dates.
    ///
    /// Per instance and per user, never per collection -- see the note on
    /// jitter in `ResolvedCollection::scheduling`, which applies here for
    /// the same reason: which days you are busy is a fact about your week,
    /// not about one collection.
    pub free_days: FreeDays,
}

/// Fractional random jitter applied to computed review intervals.
///
/// A value of 0.05 means each computed interval is scaled by a uniformly
/// random factor in [0.95, 1.05], to diffuse review peaks over time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Jitter(f64);

impl Default for Jitter {
    fn default() -> Self {
        Jitter(Self::DEFAULT_FRACTION)
    }
}

impl Jitter {
    /// The default jitter fraction: plus or minus 5%.
    pub const DEFAULT_FRACTION: f64 = 0.05;

    /// Construct a jitter fraction, validating its range.
    pub fn new(fraction: f64) -> Fallible<Jitter> {
        if !fraction.is_finite() || !(0.0..=0.5).contains(&fraction) {
            return fail(format!(
                "interval jitter must be a number between 0.0 and 0.5, got: {fraction}"
            ));
        }
        Ok(Jitter(fraction))
    }

    pub fn into_inner(self) -> f64 {
        self.0
    }

    /// No jitter: intervals are unchanged.
    #[cfg_attr(not(test), allow(dead_code))]
    pub const fn none() -> Jitter {
        Jitter(0.0)
    }

    /// Draw a random scale factor in [1 - fraction, 1 + fraction].
    pub fn factor(self, rng: &mut TinyRng) -> f64 {
        let unit: f64 = f64::from(rng.next_u32()) / f64::from(u32::MAX);
        1.0 + self.0 * (2.0 * unit - 1.0)
    }
}

/// Represents performance information for a card.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Performance {
    /// The card is new, and has never been reviewed.
    New,
    /// The card has been reviewed at least once.
    Reviewed(ReviewedPerformance),
}

impl Performance {
    pub fn is_new(&self) -> bool {
        matches!(self, Performance::New)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReviewedPerformance {
    /// The timestamp when the card was last reviewed.
    pub last_reviewed_at: Timestamp,
    /// The card's stability (an FSRS parameter).
    pub stability: Stability,
    /// The card's difficulty (an FSRS parameter).
    pub difficulty: Difficulty,
    /// The FSRS-calculated interval in days until the next review. This is
    /// the raw interval, before any rounding and clamping.
    pub interval_raw: Interval,
    /// The FSRS interval as an integer number of days.
    pub interval_days: i64,
    /// The card's next due date.
    pub due_date: Date,
    /// The number of times the card has been reviewed.
    pub review_count: usize,
}

pub fn update_performance(
    perf: Performance,
    grade: Grade,
    reviewed_at: Timestamp,
    scheduling: Scheduling,
    rng: &mut TinyRng,
) -> ReviewedPerformance {
    let today: NaiveDate = reviewed_at.date().into_inner();
    let (stability, difficulty, review_count): (Stability, Difficulty, usize) = match perf {
        Performance::New => (initial_stability(grade), initial_difficulty(grade), 0),
        Performance::Reviewed(ReviewedPerformance {
            last_reviewed_at,
            stability,
            difficulty,
            review_count,
            ..
        }) => {
            let last_reviewed_at: NaiveDate = last_reviewed_at.date().into_inner();
            // Clamp: a clock rollback can put `last_reviewed_at` in the
            // future; negative elapsed time makes retrievability exceed 1.0
            // or go NaN (BUG-28).
            let time: Interval = ((today - last_reviewed_at).num_days() as f64).max(0.0);
            let retr: Recall = retrievability(time, stability);
            let stability: Stability = new_stability(difficulty, stability, retr, grade);
            let difficulty: Difficulty = new_difficulty(difficulty, grade);
            (stability, difficulty, review_count)
        }
    };
    let interval_raw: Interval = interval(scheduling.retention.into_inner(), stability);
    // FEAT-05: scale the realized interval by a small random factor to
    // diffuse review peaks. `interval_raw` stays un-jittered — and so
    // uncapped, so that raising the ceiling later does not have to recover a
    // stability the ceiling threw away.
    let interval_jittered: Interval = interval_raw * scheduling.jitter.factor(rng);
    let interval_rounded: Interval = interval_jittered.round();
    let interval_clamped: Interval =
        interval_rounded.clamp(MIN_INTERVAL, scheduling.max_interval.into_inner());
    let interval_days: i64 = interval_clamped as i64;
    let ideal: NaiveDate = today + Duration::days(interval_days);
    // The ceiling outranks the free-day preference: a card already at the
    // maximum interval is pulled earlier rather than pushed past it.
    let not_after: NaiveDate = today + Duration::days(scheduling.max_interval.into_inner() as i64);
    let shifted: NaiveDate = scheduling.free_days.shift(ideal, not_after);
    // Recomputed rather than carried, so `interval_days` always describes
    // the date actually written. `interval_raw` stays unshifted for the
    // reason it stays un-jittered.
    let interval_days: i64 = (shifted - today).num_days();
    let due_date: Date = Date::new(shifted);
    ReviewedPerformance {
        last_reviewed_at: reviewed_at,
        stability,
        difficulty,
        interval_raw,
        interval_days,
        due_date,
        review_count: review_count + 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;
    use chrono::Weekday;

    use crate::types::free_days::FreeDays;

    fn scheduling_with_free(days: [bool; 7]) -> Fallible<Scheduling> {
        Ok(Scheduling {
            free_days: FreeDays::new(days)?,
            ..Scheduling::default()
        })
    }

    fn weekend_free() -> Fallible<Scheduling> {
        let mut days = [false; 7];
        days[5] = true;
        days[6] = true;
        scheduling_with_free(days)
    }

    fn at_nine(day: NaiveDate) -> Timestamp {
        Timestamp::new(day.and_hms_opt(9, 0, 0).expect("valid time"))
    }

    /// A weekend-free schedule never lands a card on Saturday or Sunday,
    /// over a long run of reviews at many different stabilities.
    #[test]
    fn free_days_keep_due_dates_off_those_days() -> Fallible<()> {
        let scheduling = weekend_free()?;
        let mut rng = TinyRng::from_seed(42);
        for day in 0..60 {
            let start =
                NaiveDate::from_ymd_opt(2026, 9, 14).expect("valid date") + Duration::days(day);
            let result = update_performance(
                Performance::New,
                Grade::Good,
                at_nine(start),
                scheduling,
                &mut rng,
            );
            let weekday = result.due_date.into_inner().weekday();
            assert!(
                weekday != Weekday::Sat && weekday != Weekday::Sun,
                "landed on {weekday:?}"
            );
        }
        Ok(())
    }

    /// `interval_days` describes the date actually written, so the two can
    /// never disagree; `interval_raw` stays the unshifted truth, exactly as
    /// it stays un-jittered.
    #[test]
    fn a_shifted_due_date_and_its_interval_agree() -> Fallible<()> {
        let scheduling = weekend_free()?;
        let mut rng = TinyRng::from_seed(7);
        for day in 0..30 {
            let start =
                NaiveDate::from_ymd_opt(2026, 9, 14).expect("valid date") + Duration::days(day);
            let result = update_performance(
                Performance::New,
                Grade::Good,
                at_nine(start),
                scheduling,
                &mut rng,
            );
            assert_eq!(
                result.due_date.into_inner() - start,
                Duration::days(result.interval_days),
                "interval_days must describe the date written"
            );
        }
        Ok(())
    }

    /// A schedule with no free days is the schedule this codebase had
    /// before free days existed.
    #[test]
    fn no_free_days_changes_no_due_date() {
        let mut a = TinyRng::from_seed(99);
        let mut b = TinyRng::from_seed(99);
        let at = at_nine(NaiveDate::from_ymd_opt(2026, 9, 19).expect("valid date"));
        let plain = update_performance(
            Performance::New,
            Grade::Good,
            at,
            Scheduling::default(),
            &mut a,
        );
        let explicit = Scheduling {
            free_days: FreeDays::none(),
            ..Scheduling::default()
        };
        let same = update_performance(Performance::New, Grade::Good, at, explicit, &mut b);
        assert_eq!(plain, same);
    }

    fn approx_eq(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-2
    }

    #[test]
    fn test_new() {
        assert!(Performance::New.is_new());
        let reviewed_at = Timestamp::now();
        let mut rng = TinyRng::from_seed(0);
        let reviewed_perf = update_performance(
            Performance::New,
            Grade::Good,
            reviewed_at,
            Scheduling {
                jitter: Jitter::none(),
                ..Scheduling::default()
            },
            &mut rng,
        );
        assert!(!Performance::Reviewed(reviewed_perf).is_new());
    }

    #[test]
    fn test_update_new_card() {
        let reviewed_at = Timestamp::now();
        let mut rng = TinyRng::from_seed(0);
        let result = update_performance(
            Performance::New,
            Grade::Good,
            reviewed_at,
            Scheduling {
                jitter: Jitter::none(),
                ..Scheduling::default()
            },
            &mut rng,
        );
        let ReviewedPerformance {
            last_reviewed_at,
            stability,
            difficulty,
            interval_raw,
            interval_days,
            due_date: _,
            review_count,
        } = result;
        assert_eq!(last_reviewed_at, reviewed_at);
        assert!(approx_eq(stability, 3.17));
        assert!(approx_eq(difficulty, 5.28));
        assert!(approx_eq(interval_raw, 3.17));
        assert_eq!(interval_days, 3);
        assert_eq!(review_count, 1);
    }

    #[test]
    fn test_update_already_reviewed_card() {
        let now = Timestamp::now();
        let today = now.date();
        let duration = Duration::days(3);
        let last_reviewed_at = Timestamp::new(now.into_inner() - duration);
        let initial_perf = ReviewedPerformance {
            last_reviewed_at,
            stability: 3.17,
            difficulty: 5.28,
            interval_raw: 3.17,
            interval_days: 3,
            due_date: Date::new(today.into_inner() + duration),
            review_count: 1,
        };
        let reviewed_at = now;
        let mut rng = TinyRng::from_seed(0);
        let result = update_performance(
            Performance::Reviewed(initial_perf),
            Grade::Easy,
            reviewed_at,
            Scheduling {
                jitter: Jitter::none(),
                ..Scheduling::default()
            },
            &mut rng,
        );
        let ReviewedPerformance {
            last_reviewed_at,
            stability,
            difficulty,
            interval_raw,
            interval_days,
            due_date: _,
            review_count,
        } = result;
        assert_eq!(last_reviewed_at, reviewed_at);
        assert!(approx_eq(stability, 25.80));
        assert!(approx_eq(difficulty, 4.50));
        assert!(approx_eq(interval_raw, 25.80));
        assert_eq!(interval_days, 26);
        assert_eq!(review_count, 2);
    }

    /// Regression test for BUG-28: a `last_reviewed_at` in the future (clock
    /// rollback) must not produce negative elapsed time. Elapsed days clamp
    /// to zero, so retrievability is exactly 1.0 and the update stays finite.
    #[test]
    fn test_future_last_reviewed_at_is_clamped() {
        let now = Timestamp::now();
        let today = now.date();

        // Case A: one day in the future. Without clamping, retrievability
        // exceeds 1.0 and stability shrinks on a Good grade.
        let one_day = Duration::days(1);
        let perf = ReviewedPerformance {
            last_reviewed_at: Timestamp::new(now.into_inner() + one_day),
            stability: 3.17,
            difficulty: 5.28,
            interval_raw: 3.17,
            interval_days: 3,
            due_date: Date::new(today.into_inner() + one_day),
            review_count: 1,
        };
        let mut rng = TinyRng::from_seed(0);
        let result = update_performance(
            Performance::Reviewed(perf),
            Grade::Good,
            now,
            Scheduling {
                jitter: Jitter::none(),
                ..Scheduling::default()
            },
            &mut rng,
        );
        assert!(result.stability.is_finite());
        assert!(result.interval_raw.is_finite());
        // With elapsed time clamped to 0, retrievability is 1.0 and a Good
        // grade leaves stability unchanged.
        assert!(approx_eq(result.stability, 3.17));
        assert_eq!(result.interval_days, 3);

        // Case B: five days in the future with low stability. Without
        // clamping, retrievability's base goes negative and powf() is NaN.
        let five_days = Duration::days(5);
        let perf = ReviewedPerformance {
            last_reviewed_at: Timestamp::new(now.into_inner() + five_days),
            stability: 1.0,
            difficulty: 5.0,
            interval_raw: 1.0,
            interval_days: 1,
            due_date: Date::new(today.into_inner() + five_days),
            review_count: 1,
        };
        let mut rng = TinyRng::from_seed(0);
        let result = update_performance(
            Performance::Reviewed(perf),
            Grade::Good,
            now,
            Scheduling {
                jitter: Jitter::none(),
                ..Scheduling::default()
            },
            &mut rng,
        );
        assert!(result.stability.is_finite());
        assert!(result.interval_raw.is_finite());
        assert!(result.interval_days >= 1);
    }

    #[test]
    fn test_desired_retention_validates_range() {
        assert!(DesiredRetention::new(0.7).is_ok());
        assert!(DesiredRetention::new(0.9).is_ok());
        assert!(DesiredRetention::new(0.99).is_ok());
        assert!(DesiredRetention::new(0.69).is_err());
        assert!(DesiredRetention::new(1.0).is_err());
        assert!(DesiredRetention::new(f64::NAN).is_err());
        let err = DesiredRetention::new(2.0).err().unwrap().to_string();
        assert!(err.contains("2"), "message was: {err}");
    }

    #[test]
    fn test_max_interval_validates_range() {
        assert!(MaxInterval::new(1.0).is_ok());
        assert!(MaxInterval::new(256.0).is_ok());
        assert!(MaxInterval::new(36500.0).is_ok());
        assert!(MaxInterval::new(0.9).is_err());
        assert!(MaxInterval::new(36501.0).is_err());
        assert!(MaxInterval::new(f64::NAN).is_err());
        let err = MaxInterval::new(0.0).err().unwrap().to_string();
        assert!(err.contains("0"), "message was: {err}");
    }

    /// The ceiling was 256 days, hardcoded and invisible. Making it
    /// configurable must not move it for anyone who does not configure it:
    /// every card already scheduled was scheduled under this number.
    #[test]
    fn test_defaults_preserve_the_previous_constants() {
        let scheduling = Scheduling::default();
        assert_eq!(scheduling.retention.into_inner(), 0.9);
        assert_eq!(scheduling.max_interval.into_inner(), 256.0);
    }

    /// Wanting to remember more of what you learned means seeing it sooner.
    #[test]
    fn test_higher_retention_shortens_the_interval() {
        let reviewed_at = Timestamp::now();
        let at = |retention: f64| {
            let mut rng = TinyRng::from_seed(0);
            update_performance(
                Performance::New,
                Grade::Easy,
                reviewed_at,
                Scheduling {
                    retention: DesiredRetention::new(retention).expect("in range"),
                    ..Scheduling::default()
                },
                &mut rng,
            )
            .interval_raw
        };
        assert!(
            at(0.95) < at(0.9) && at(0.9) < at(0.85),
            "raising retention did not shorten the interval: {} {} {}",
            at(0.95),
            at(0.9),
            at(0.85)
        );
    }

    #[test]
    fn test_max_interval_caps_the_interval() {
        let reviewed_at = Timestamp::now();
        let mut rng = TinyRng::from_seed(0);
        // A card known well enough that FSRS would leave it for years.
        let perf = Performance::Reviewed(ReviewedPerformance {
            last_reviewed_at: reviewed_at,
            stability: 10_000.0,
            difficulty: 1.0,
            interval_raw: 10_000.0,
            interval_days: 10_000,
            due_date: reviewed_at.date(),
            review_count: 40,
        });
        let result = update_performance(
            perf,
            Grade::Easy,
            reviewed_at,
            Scheduling {
                max_interval: MaxInterval::new(30.0).expect("in range"),
                jitter: Jitter::none(),
                ..Scheduling::default()
            },
            &mut rng,
        );
        assert_eq!(result.interval_days, 30);
        // The uncapped figure is kept, so the cap can be raised later
        // without the card's real stability having been thrown away.
        assert!(
            result.interval_raw > 30.0,
            "the raw interval was capped too: {}",
            result.interval_raw
        );
    }

    #[test]
    fn test_jitter_new_validates_fraction() {
        assert!(Jitter::new(0.0).is_ok());
        assert!(Jitter::new(0.05).is_ok());
        assert!(Jitter::new(0.5).is_ok());
        assert!(Jitter::new(-0.01).is_err());
        assert!(Jitter::new(0.51).is_err());
        assert!(Jitter::new(f64::NAN).is_err());
        let err = Jitter::new(2.0).err().unwrap().to_string();
        assert!(err.contains("2"), "message was: {err}");
    }

    #[test]
    fn test_jitter_factor_bounds_and_determinism() {
        let jitter = Jitter::new(0.05).unwrap();
        // Deterministic under a fixed seed.
        let mut rng_a = TinyRng::from_seed(42);
        let mut rng_b = TinyRng::from_seed(42);
        for _ in 0..100 {
            assert_eq!(jitter.factor(&mut rng_a), jitter.factor(&mut rng_b));
        }
        // Always within [0.95, 1.05].
        let mut rng = TinyRng::from_seed(7);
        for _ in 0..1000 {
            let f = jitter.factor(&mut rng);
            assert!((0.95..=1.05).contains(&f), "factor out of bounds: {f}");
        }
        // Zero jitter is always exactly 1.0.
        let mut rng = TinyRng::from_seed(7);
        for _ in 0..100 {
            assert_eq!(Jitter::none().factor(&mut rng), 1.0);
        }
    }

    /// FEAT-05: jitter scales the realized interval within bounds, is
    /// deterministic under a seeded RNG, and actually varies across seeds.
    #[test]
    fn test_update_performance_jitter() {
        let reviewed_at = Timestamp::now();
        let today = reviewed_at.date();
        let duration = Duration::days(35);
        let perf = ReviewedPerformance {
            last_reviewed_at: Timestamp::new(reviewed_at.into_inner() - duration),
            stability: 34.57,
            difficulty: 5.26,
            interval_raw: 34.57,
            interval_days: 35,
            due_date: Date::new(today.into_inner()),
            review_count: 3,
        };
        let scheduling = Scheduling {
            jitter: Jitter::new(0.05).unwrap(),
            ..Scheduling::default()
        };
        let run = |seed: u64| {
            let mut rng = TinyRng::from_seed(seed);
            update_performance(
                Performance::Reviewed(perf),
                Grade::Good,
                reviewed_at,
                scheduling,
                &mut rng,
            )
        };
        // Deterministic under a fixed seed.
        assert_eq!(run(42), run(42));
        // The realized interval stays within +/-5% of the raw interval
        // (plus rounding).
        for seed in 0..100 {
            let result = run(seed);
            let days = result.interval_days as f64;
            assert!(days >= (result.interval_raw * 0.95).floor());
            assert!(days <= (result.interval_raw * 1.05).ceil());
        }
        // Across 100 seeds at least two distinct interval lengths appear;
        // without jitter every seed would yield the same value.
        let distinct: std::collections::HashSet<i64> =
            (0..100).map(|seed| run(seed).interval_days).collect();
        assert!(distinct.len() > 1);
        // interval_raw itself stays un-jittered.
        let baseline = {
            let mut rng = TinyRng::from_seed(0);
            update_performance(
                Performance::Reviewed(perf),
                Grade::Good,
                reviewed_at,
                Scheduling {
                    jitter: Jitter::none(),
                    ..Scheduling::default()
                },
                &mut rng,
            )
        };
        assert_eq!(run(42).interval_raw, baseline.interval_raw);
    }
}
