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

use rusqlite::ToSql;
use rusqlite::types::FromSql;
use rusqlite::types::FromSqlError;
use rusqlite::types::FromSqlResult;
use rusqlite::types::ToSqlOutput;
use rusqlite::types::ValueRef;
use serde::Serialize;

pub mod optimize;

use crate::error::ErrorReport;
use crate::error::Fallible;
use crate::error::fail;

/// Per-index bounds, `(low, high)` inclusive.
///
/// They exist because the formulas are not total: `W[4]` is a difficulty and
/// difficulty is bounded 1..10, `W[7]` is a mixing fraction that means
/// nothing outside 0..1, and `W[11]` multiplies the whole failure branch, so
/// zero would collapse every lapse to no stability at all.
///
/// Signs follow *this* codebase, not FSRS's published table. The exponents
/// at 9 and 12 are negated where they are used -- `s.powf(-W[9])`,
/// `d.powf(-W[12])` -- so they are stored positive here where the published
/// table writes them negative.
pub(crate) const BOUNDS: [(f64, f64); 19] = [
    (0.001, 100.0), // 0  initial stability, Forgot
    (0.001, 100.0), // 1  initial stability, Hard
    (0.001, 100.0), // 2  initial stability, Good
    (0.001, 100.0), // 3  initial stability, Easy
    (1.0, 10.0),    // 4  initial difficulty
    (0.001, 4.0),   // 5  initial difficulty, grade exponent
    (0.001, 4.0),   // 6  difficulty step per grade
    (0.0, 0.75),    // 7  difficulty mean-reversion fraction
    (0.0, 4.5),     // 8  stability growth, constant
    (0.0, 0.8),     // 9  stability growth, stability exponent (negated in use)
    (0.0, 3.0),     // 10 stability growth, retrievability
    (0.001, 5.0),   // 11 failure, constant
    (0.0, 0.8),     // 12 failure, difficulty exponent (negated in use)
    (0.01, 0.9),    // 13 failure, stability exponent
    (0.01, 3.0),    // 14 failure, retrievability
    (0.0, 1.0),     // 15 Hard penalty
    (1.0, 6.0),     // 16 Easy bonus
    (0.0, 2.0),     // 17 short-term, constant
    (0.0, 2.0),     // 18 short-term, grade
];

/// The 19 FSRS parameters.
///
/// Data rather than a constant, so that a schedule can be fitted to one
/// person's review history instead of to the population the published
/// defaults came from. The defaults are exactly the constant that used to be
/// compiled in, so a user who changes nothing is scheduled as they always
/// were.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Weights([f64; 19]);

impl Default for Weights {
    fn default() -> Self {
        Weights(Self::DEFAULT)
    }
}

impl Weights {
    pub const DEFAULT: [f64; 19] = [
        0.40255, 1.18385, 3.173, 15.69105, 7.1949, 0.5345, 1.4604, 0.0046, 1.54575, 0.1192,
        1.01925, 1.9395, 0.11, 0.29605, 2.2698, 0.2315, 2.9898, 0.51655, 0.6621,
    ];

    pub fn new(w: [f64; 19]) -> Fallible<Weights> {
        for (i, value) in w.iter().enumerate() {
            let (low, high) = BOUNDS[i];
            if !value.is_finite() || *value < low || *value > high {
                return fail(format!(
                    "FSRS weight {i} must be a number between {low} and {high}, got: {value}"
                ));
            }
        }
        Ok(Weights(w))
    }

    pub fn get(&self, i: usize) -> f64 {
        self.0[i]
    }

    pub fn as_array(&self) -> [f64; 19] {
        self.0
    }

    pub fn is_default(&self) -> bool {
        self.0 == Self::DEFAULT
    }

    /// The comma-separated form an optimizer emits and the page shows.
    pub fn parse_list(s: &str) -> Fallible<Weights> {
        let parts: Vec<&str> = s
            .split([',', '\n'])
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .collect();
        if parts.len() != 19 {
            return fail(format!(
                "FSRS needs exactly 19 weights, found {}. Paste the whole list.",
                parts.len()
            ));
        }
        let mut w = [0.0f64; 19];
        for (i, part) in parts.iter().enumerate() {
            match part.parse::<f64>() {
                Ok(v) => w[i] = v,
                Err(_) => return fail(format!("FSRS weight {i} is not a number: {part}")),
            }
        }
        Weights::new(w)
    }

    pub fn to_list(self) -> String {
        self.0
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub type Recall = f64;
pub type Stability = f64;
pub type Difficulty = f64;

#[derive(Clone, Copy, PartialEq, Debug, Serialize)]
pub enum Grade {
    Forgot,
    Hard,
    Good,
    Easy,
}

impl From<Grade> for f64 {
    fn from(g: Grade) -> f64 {
        match g {
            Grade::Forgot => 1.0,
            Grade::Hard => 2.0,
            Grade::Good => 3.0,
            Grade::Easy => 4.0,
        }
    }
}

impl Grade {
    pub fn as_str(&self) -> &str {
        match self {
            Grade::Forgot => "forgot",
            Grade::Hard => "hard",
            Grade::Good => "good",
            Grade::Easy => "easy",
        }
    }
}

impl TryFrom<String> for Grade {
    type Error = ErrorReport;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "forgot" => Ok(Grade::Forgot),
            "hard" => Ok(Grade::Hard),
            "good" => Ok(Grade::Good),
            "easy" => Ok(Grade::Easy),
            _ => fail(format!("invalid grade string: {value}")),
        }
    }
}

impl ToSql for Grade {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for Grade {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let string: String = FromSql::column_result(value)?;
        Grade::try_from(string).map_err(|e| FromSqlError::Other(Box::new(e)))
    }
}

pub type Interval = f64;

const F: f64 = 19.0 / 81.0;
const C: f64 = -0.5;

pub fn retrievability(t: Interval, s: Stability) -> Recall {
    (1.0 + F * (t / s)).powf(C)
}

pub fn interval(r_d: Recall, s: Stability) -> Interval {
    (s / F) * (r_d.powf(1.0 / C) - 1.0)
}

pub fn initial_stability(g: Grade, w: &Weights) -> Stability {
    match g {
        Grade::Forgot => w.get(0),
        Grade::Hard => w.get(1),
        Grade::Good => w.get(2),
        Grade::Easy => w.get(3),
    }
}

fn s_success(d: Difficulty, s: Stability, r: Recall, g: Grade, w: &Weights) -> Stability {
    let t_d = 11.0 - d;
    let t_s = s.powf(-w.get(9));
    let t_r = f64::exp(w.get(10) * (1.0 - r)) - 1.0;
    let h = if g == Grade::Hard { w.get(15) } else { 1.0 };
    let b = if g == Grade::Easy { w.get(16) } else { 1.0 };
    let c = f64::exp(w.get(8));
    let alpha = 1.0 + t_d * t_s * t_r * h * b * c;
    s * alpha
}

fn s_fail(d: Difficulty, s: Stability, r: Recall, w: &Weights) -> Stability {
    let d_f = d.powf(-w.get(12));
    let s_f = (s + 1.0).powf(w.get(13)) - 1.0;
    let r_f = f64::exp(w.get(14) * (1.0 - r));
    let c_f = w.get(11);
    let s_f = d_f * s_f * r_f * c_f;
    f64::min(s_f, s)
}

pub fn new_stability(d: Difficulty, s: Stability, r: Recall, g: Grade, w: &Weights) -> Stability {
    if g == Grade::Forgot {
        s_fail(d, s, r, w)
    } else {
        s_success(d, s, r, g, w)
    }
}

fn clamp_d(d: Difficulty) -> Difficulty {
    d.clamp(1.0, 10.0)
}

pub fn initial_difficulty(g: Grade, w: &Weights) -> Difficulty {
    let g: f64 = g.into();
    clamp_d(w.get(4) - f64::exp(w.get(5) * (g - 1.0)) + 1.0)
}

pub fn new_difficulty(d: Difficulty, g: Grade, w: &Weights) -> Difficulty {
    clamp_d(w.get(7) * initial_difficulty(Grade::Easy, w) + (1.0 - w.get(7)) * dp(d, g, w))
}

fn dp(d: Difficulty, g: Grade, w: &Weights) -> f64 {
    d + delta_d(g, w) * ((10.0 - d) / 9.0)
}

fn delta_d(g: Grade, w: &Weights) -> f64 {
    let g: f64 = g.into();
    -w.get(6) * (g - 3.0)
}

#[cfg(test)]
mod tests {
    use std::iter::zip;

    use super::*;
    use crate::error::Fallible;

    #[test]
    fn the_default_weights_are_the_constant_that_was_compiled_in() {
        assert_eq!(Weights::default().as_array(), Weights::DEFAULT);
        assert!(Weights::default().is_default());
    }

    /// The bounds must admit the defaults. Obvious, and worth a test: the
    /// first draft of `BOUNDS` took the signs from FSRS's own published
    /// table, where `w9` and `w12` are negative. This codebase negates them
    /// inline -- `s.powf(-W[9])` -- so it stores them positive, and three
    /// bounds rejected the very vector they were written around.
    #[test]
    fn the_default_weights_are_inside_their_own_bounds() {
        Weights::new(Weights::DEFAULT).expect("the defaults must be a valid weight vector");
    }

    /// The formulas are not total. A negative `W[9]` inverts the stability
    /// exponent and a difficulty of zero is not a difficulty, so the bounds
    /// are part of the type rather than a caller's responsibility.
    #[test]
    fn weights_outside_their_bounds_are_refused() {
        let mut w = Weights::DEFAULT;
        w[9] = -1.0;
        assert!(Weights::new(w).is_err());

        let mut w = Weights::DEFAULT;
        w[4] = 0.0;
        assert!(
            Weights::new(w).is_err(),
            "initial difficulty must be a difficulty"
        );

        let mut w = Weights::DEFAULT;
        w[0] = f64::NAN;
        assert!(Weights::new(w).is_err(), "NaN is not a weight");

        let mut w = Weights::DEFAULT;
        w[0] = f64::INFINITY;
        assert!(Weights::new(w).is_err());
    }

    /// The error names the index, because a rejected paste of 19 numbers is
    /// unactionable otherwise.
    #[test]
    fn the_weight_rejection_names_the_weight() {
        let mut w = Weights::DEFAULT;
        w[9] = -1.0;
        let err = Weights::new(w).expect_err("out of bounds");
        assert!(err.to_string().contains('9'), "message was: {err}");
    }

    /// The list format is what an optimizer emits and what the page shows.
    #[test]
    fn a_weight_list_round_trips() -> Fallible<()> {
        let text = Weights::default().to_list();
        assert_eq!(Weights::parse_list(&text)?, Weights::default());
        // Whitespace and newlines as a paste would carry them.
        let spaced = text.replace(", ", ",\n  ");
        assert_eq!(Weights::parse_list(&spaced)?, Weights::default());
        Ok(())
    }

    #[test]
    fn a_weight_list_of_the_wrong_length_is_refused() {
        let err = Weights::parse_list("0.4, 1.2").expect_err("two is not nineteen");
        assert!(err.to_string().contains("19"), "message was: {err}");
        assert!(Weights::parse_list("").is_err());
        assert!(Weights::parse_list("a, b, c").is_err());
    }

    /// Approximate equality.
    fn feq(a: f64, b: f64) -> bool {
        f64::abs(a - b) < 0.01
    }

    /// R_d = 0.9, I(S) = S.
    #[test]
    fn test_interval_equals_stability() {
        let samples = 100;
        let start = 0.1;
        let end = 5.0;
        let step = (end - start) / (samples as f64 - 1.0);
        for i in 0..samples {
            let s = start + (i as f64) * step;
            assert!(feq(interval(0.9, s), s))
        }
    }

    /// D_0(1) = w_4
    #[test]
    fn test_initial_difficulty_of_forgetting() {
        assert_eq!(
            initial_difficulty(Grade::Forgot, &Weights::default()),
            Weights::default().get(4)
        )
    }

    /// A simulation step.
    #[derive(Clone, Copy, Debug)]
    struct Step {
        /// The time when the review took place.
        t: Interval,
        /// New stability.
        s: Stability,
        /// New difficulty.
        d: Difficulty,
        /// Next interval.
        i: Interval,
    }

    impl PartialEq for Step {
        fn eq(&self, other: &Self) -> bool {
            feq(self.t, other.t)
                && feq(self.s, other.s)
                && feq(self.d, other.d)
                && feq(self.i, other.i)
        }
    }

    /// Simulate a series of reviews.
    fn sim(grades: Vec<Grade>) -> Vec<Step> {
        let mut t: Interval = 0.0;
        let r_d: f64 = 0.9;
        let mut steps = vec![];

        // Initial review.
        assert!(!grades.is_empty());
        let mut grades = grades.clone();
        let g: Grade = grades.remove(0);
        let w = Weights::default();
        let mut s: Stability = initial_stability(g, &w);
        let mut d: Difficulty = initial_difficulty(g, &w);
        let mut i: Interval = f64::max(interval(r_d, s).round(), 1.0);
        steps.push(Step { t, s, d, i });

        // n-th review
        for g in grades {
            t += i;
            let r: Recall = retrievability(i, s);
            s = new_stability(d, s, r, g, &w);
            d = new_difficulty(d, g, &w);
            i = f64::max(interval(r_d, s).round(), 1.0);
            steps.push(Step { t, s, d, i });
        }

        steps
    }

    /// Test a sequence of three easies.
    #[test]
    fn test_3e() {
        let g = Grade::Easy;
        let grades = vec![g, g, g];
        let expected = vec![
            Step {
                t: 0.0,
                s: 15.69,
                d: 3.22,
                i: 16.0,
            },
            Step {
                t: 16.0,
                s: 150.28,
                d: 2.13,
                i: 150.0,
            },
            Step {
                t: 166.0,
                s: 1252.22,
                d: 1.0,
                i: 1252.0,
            },
        ];
        let actual = sim(grades);
        assert_eq!(expected.len(), actual.len());
        for (expected, actual) in zip(expected, actual) {
            assert_eq!(actual, expected);
        }
    }

    /// Test a sequence of three goods.
    #[test]
    fn test_3g() {
        let g = Grade::Good;
        let grades = vec![g, g, g];
        let expected = vec![
            Step {
                t: 0.0,
                s: 3.17,
                d: 5.28,
                i: 3.0,
            },
            Step {
                t: 3.0,
                s: 10.73,
                d: 5.27,
                i: 11.0,
            },
            Step {
                t: 14.0,
                s: 34.57,
                d: 5.26,
                i: 35.0,
            },
        ];
        let actual = sim(grades);
        assert_eq!(expected.len(), actual.len());
        for (expected, actual) in zip(expected, actual) {
            assert_eq!(actual, expected);
        }
    }

    /// Test a sequence of two hards.
    #[test]
    fn test_2h() {
        let g = Grade::Hard;
        let grades = vec![g, g];
        let expected = vec![
            Step {
                t: 0.0,
                s: 1.18,
                d: 6.48,
                i: 1.0,
            },
            Step {
                t: 1.0,
                s: 1.70,
                d: 7.04,
                i: 2.0,
            },
        ];
        let actual = sim(grades);
        assert_eq!(expected.len(), actual.len());
        for (expected, actual) in zip(expected, actual) {
            assert_eq!(actual, expected);
        }
    }

    /// Test a sequence of two forgots.
    #[test]
    fn test_2f() {
        let g = Grade::Forgot;
        let grades = vec![g, g];
        let expected = vec![
            Step {
                t: 0.0,
                s: 0.40,
                d: 7.19,
                i: 1.0,
            },
            Step {
                t: 1.0,
                s: 0.26,
                d: 8.08,
                i: 1.0,
            },
        ];
        let actual = sim(grades);
        assert_eq!(expected.len(), actual.len());
        for (expected, actual) in zip(expected, actual) {
            assert_eq!(actual, expected);
        }
    }

    /// Test a sequence of good then forgot.
    #[test]
    fn test_gf() {
        let grades = vec![Grade::Good, Grade::Forgot];
        let expected = vec![
            Step {
                t: 0.0,
                s: 3.17,
                d: 5.28,
                i: 3.0,
            },
            Step {
                t: 3.0,
                s: 1.06,
                d: 6.8,
                i: 1.0,
            },
        ];
        let actual = sim(grades);
        assert_eq!(expected.len(), actual.len());
        for (expected, actual) in zip(expected, actual) {
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn test_grade_serialization_roundtrip() -> Fallible<()> {
        let grades = [Grade::Forgot, Grade::Hard, Grade::Good, Grade::Easy];
        for grade in grades {
            assert_eq!(grade, Grade::try_from(grade.as_str().to_string())?);
        }
        Ok(())
    }

    /// Test the serialization format of Grade.
    #[test]
    fn test_grade_serialization_format() -> Fallible<()> {
        let grades = [Grade::Forgot, Grade::Hard, Grade::Good, Grade::Easy];
        let expected = ["Forgot", "Hard", "Good", "Easy"];
        for (grade, expected) in zip(grades, expected) {
            let serialized = serde_json::to_string(&grade)?;
            let expected = format!("\"{}\"", expected);
            assert_eq!(serialized, expected);
        }

        Ok(())
    }

    #[test]
    fn test_invalid_grade_string() {
        let invalid_strings = ["", "invalid"];
        for s in invalid_strings {
            assert!(Grade::try_from(s.to_string()).is_err());
        }
    }

    /// Regression test for BUG-29: the error message must contain the actual
    /// offending value, not the literal string "{value}".
    #[test]
    fn test_invalid_grade_error_contains_value() {
        let result = Grade::try_from("bogus".to_string());
        let err = result.err().unwrap();
        let msg = err.to_string();
        assert!(msg.contains("bogus"), "message was: {msg}");
        assert!(!msg.contains("{value}"), "message was: {msg}");
    }
}
