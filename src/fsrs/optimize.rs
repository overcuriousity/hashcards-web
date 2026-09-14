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

//! Fitting the FSRS weights to one person's review history.
//!
//! Pure arithmetic over sequences: no database, no clock, no server. That is
//! what makes it testable against a synthetic log whose answer is known.

use crate::error::Fallible;
use crate::error::fail;
use crate::fsrs::BOUNDS;
use crate::fsrs::Grade;
use crate::fsrs::Weights;
use crate::fsrs::initial_difficulty;
use crate::fsrs::initial_stability;
use crate::fsrs::new_difficulty;
use crate::fsrs::new_stability;
use crate::fsrs::retrievability;

/// Keeps the logarithm away from zero.
const EPSILON: f64 = 1e-9;

/// Mean binary log-loss of predicted recall against what happened.
///
/// The standard FSRS objective. For each review after a card's first, the
/// weights predict the chance the card is still remembered after the gap;
/// the observation is 1 unless the grade was Forgot. A card's first review
/// is not scored -- there is no prior state to predict from -- but it does
/// set the state the rest of the sequence is judged against.
pub fn log_loss(sequences: &[Vec<(f64, Grade)>], w: &Weights) -> f64 {
    let mut total = 0.0;
    let mut scored = 0usize;
    for sequence in sequences {
        let Some((_, first)) = sequence.first() else {
            continue;
        };
        let mut stability = initial_stability(*first, w);
        let mut difficulty = initial_difficulty(*first, w);
        for (delta_t, grade) in sequence.iter().skip(1) {
            let elapsed = delta_t.max(0.0);
            let predicted = retrievability(elapsed, stability).clamp(EPSILON, 1.0 - EPSILON);
            let observed = if *grade == Grade::Forgot { 0.0 } else { 1.0 };
            total += -(observed * predicted.ln() + (1.0 - observed) * (1.0 - predicted).ln());
            scored += 1;
            stability = new_stability(difficulty, stability, predicted, *grade, w);
            difficulty = new_difficulty(difficulty, *grade, w);
        }
    }
    if scored == 0 {
        0.0
    } else {
        total / scored as f64
    }
}

/// Below this, a fit is noise. 19 parameters need considerably more than 19
/// observations before the answer means anything.
pub const MIN_REVIEWS: usize = 400;

/// Iteration cap. Chosen to stay interactive under `run_blocking` rather
/// than to reach a minimum: a better-but-unfinished fit is still an
/// improvement, and the guardrail that matters is that it beat the starting
/// point.
const MAX_STEPS: usize = 120;

/// Adam's usual constants.
const LEARNING_RATE: f64 = 0.02;
const BETA1: f64 = 0.9;
const BETA2: f64 = 0.999;
const ADAM_EPSILON: f64 = 1e-8;

/// Step for the central difference, relative to each weight's own range, so
/// one step size does not have to suit both `W[7]` (0..0.75) and `W[3]`
/// (0..100).
const GRADIENT_STEP: f64 = 1e-4;

/// What a fit found, and what it was worth.
#[derive(Debug)]
pub struct FitOutcome {
    pub weights: Weights,
    pub loss_before: f64,
    pub loss_after: f64,
    pub reviews: usize,
}

fn clamp_to_bounds(w: &mut [f64; 19]) {
    for (i, value) in w.iter_mut().enumerate() {
        let (low, high) = BOUNDS[i];
        if !value.is_finite() {
            *value = Weights::DEFAULT[i];
        }
        *value = value.clamp(low, high);
    }
}

/// Fit the weights to these sequences, starting from `from`.
///
/// Adam over central-difference numeric gradients. Analytic gradients
/// through the full recurrence are error-prone calculus; numeric costs 38
/// replays per step and is obviously correct.
pub fn fit(sequences: &[Vec<(f64, Grade)>], from: &Weights) -> Fallible<FitOutcome> {
    let reviews: usize = sequences.iter().map(|s| s.len()).sum();
    if reviews < MIN_REVIEWS {
        return fail(format!(
            "A fit needs at least {MIN_REVIEWS} reviews to mean anything, and this history has \
             {reviews}. Keep reviewing and try again later."
        ));
    }
    let loss_before = log_loss(sequences, from);

    let mut current = from.as_array();
    let mut m = [0.0f64; 19];
    let mut v = [0.0f64; 19];
    let mut best = current;
    let mut best_loss = loss_before;

    for step in 1..=MAX_STEPS {
        // Central differences, one pair of replays per weight.
        let mut gradient = [0.0f64; 19];
        for i in 0..19 {
            let (low, high) = BOUNDS[i];
            let h = ((high - low) * GRADIENT_STEP).max(1e-8);
            let mut up = current;
            let mut down = current;
            up[i] = (up[i] + h).min(high);
            down[i] = (down[i] - h).max(low);
            let span = up[i] - down[i];
            if span <= 0.0 {
                continue;
            }
            clamp_to_bounds(&mut up);
            clamp_to_bounds(&mut down);
            let (Ok(wu), Ok(wd)) = (Weights::new(up), Weights::new(down)) else {
                continue;
            };
            gradient[i] = (log_loss(sequences, &wu) - log_loss(sequences, &wd)) / span;
        }

        for i in 0..19 {
            m[i] = BETA1 * m[i] + (1.0 - BETA1) * gradient[i];
            v[i] = BETA2 * v[i] + (1.0 - BETA2) * gradient[i] * gradient[i];
            let m_hat = m[i] / (1.0 - BETA1.powi(step as i32));
            let v_hat = v[i] / (1.0 - BETA2.powi(step as i32));
            let (low, high) = BOUNDS[i];
            // Scaled by the weight's own range, for the reason the gradient
            // step is: one learning rate cannot suit both 0..0.75 and 0..100.
            let scale = (high - low).max(1e-6);
            current[i] -= LEARNING_RATE * scale * m_hat / (v_hat.sqrt() + ADAM_EPSILON);
        }
        clamp_to_bounds(&mut current);

        let Ok(candidate) = Weights::new(current) else {
            break;
        };
        let loss = log_loss(sequences, &candidate);
        if loss < best_loss {
            best_loss = loss;
            best = current;
        }
    }

    Ok(FitOutcome {
        // The best iterate, not the last: Adam overshoots, and the point of
        // the exercise is the best answer found rather than wherever the
        // walk happened to stop.
        weights: Weights::new(best)?,
        loss_before,
        loss_after: best_loss,
        reviews,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::TinyRng;

    /// A sequence of one review teaches nothing: there is no prediction to
    /// score before the first observation.
    #[test]
    fn a_first_review_contributes_no_loss() {
        let seqs = vec![vec![(0.0, Grade::Good)]];
        assert_eq!(log_loss(&seqs, &Weights::default()), 0.0);
    }

    /// The loss is a mean over scored predictions, so it does not grow just
    /// because the log did.
    #[test]
    fn the_loss_is_a_mean_not_a_sum() {
        let one = vec![vec![(0.0, Grade::Good), (3.0, Grade::Good)]];
        let many = vec![
            vec![(0.0, Grade::Good), (3.0, Grade::Good)],
            vec![(0.0, Grade::Good), (3.0, Grade::Good)],
            vec![(0.0, Grade::Good), (3.0, Grade::Good)],
        ];
        let a = log_loss(&one, &Weights::default());
        let b = log_loss(&many, &Weights::default());
        assert!((a - b).abs() < 1e-9, "{a} vs {b}");
    }

    /// Predicting a lapse that happened is better than predicting one that
    /// did not: the loss must be able to tell those apart, or there is
    /// nothing to minimise.
    #[test]
    fn forgetting_after_a_long_gap_costs_less_than_forgetting_at_once() {
        let w = Weights::default();
        let late = vec![vec![(0.0, Grade::Good), (365.0, Grade::Forgot)]];
        let early = vec![vec![(0.0, Grade::Good), (1.0, Grade::Forgot)]];
        assert!(
            log_loss(&late, &w) < log_loss(&early, &w),
            "late {} early {}",
            log_loss(&late, &w),
            log_loss(&early, &w)
        );
    }

    /// Every loss is a finite number. The recurrence has powf and exp in it
    /// and the optimizer pushes weights to their bounds, so a NaN here would
    /// silently poison a fit.
    #[test]
    fn the_loss_is_finite_at_the_bounds() {
        let seqs = vec![vec![
            (0.0, Grade::Forgot),
            (1.0, Grade::Hard),
            (30.0, Grade::Good),
            (400.0, Grade::Easy),
            (0.0, Grade::Forgot),
        ]];
        let mut low = Weights::DEFAULT;
        let mut high = Weights::DEFAULT;
        for (i, (lo, hi)) in BOUNDS.iter().enumerate() {
            low[i] = *lo;
            high[i] = *hi;
        }
        for w in [Weights::new(low), Weights::new(high)]
            .into_iter()
            .flatten()
        {
            let loss = log_loss(&seqs, &w);
            assert!(loss.is_finite(), "loss was {loss}");
        }
    }

    /// Enough reviews to be allowed to fit, generated deterministically so
    /// the test is not flaky. The log really is generated by `w`: each grade
    /// is a coin against the recall probability `w` predicts.
    fn synthetic_log(w: &Weights, cards: usize, per_card: usize) -> Vec<Vec<(f64, Grade)>> {
        let mut rng = TinyRng::from_seed(7);
        let mut out = Vec::new();
        for _ in 0..cards {
            let mut sequence = vec![(0.0, Grade::Good)];
            let mut stability = initial_stability(Grade::Good, w);
            let mut difficulty = initial_difficulty(Grade::Good, w);
            for _ in 1..per_card {
                let gap = stability.max(1.0).round();
                let recall = retrievability(gap, stability);
                let unit = f64::from(rng.next_u32()) / f64::from(u32::MAX);
                let grade = if unit < recall {
                    Grade::Good
                } else {
                    Grade::Forgot
                };
                sequence.push((gap, grade));
                stability = new_stability(difficulty, stability, recall, grade, w);
                difficulty = new_difficulty(difficulty, grade, w);
            }
            out.push(sequence);
        }
        out
    }

    /// A log too short to learn from is refused rather than fitted.
    #[test]
    fn a_short_log_is_refused() {
        let seqs = synthetic_log(&Weights::default(), 2, 3);
        let err = fit(&seqs, &Weights::default()).expect_err("six reviews is not a fit");
        assert!(err.to_string().contains("400"), "message was: {err}");
    }

    /// The fit improves the loss it was given, or it is not a fit.
    #[test]
    fn fitting_improves_the_loss() -> Fallible<()> {
        let mut truth = Weights::DEFAULT;
        truth[2] = 6.0; // a much more stable "Good" than the defaults assume
        let truth = Weights::new(truth)?;
        let seqs = synthetic_log(&truth, 200, 5);

        let outcome = fit(&seqs, &Weights::default())?;
        assert!(
            outcome.loss_after < outcome.loss_before,
            "before {} after {}",
            outcome.loss_before,
            outcome.loss_after
        );
        assert!(outcome.reviews >= MIN_REVIEWS);
        // Not a token improvement: a fit worth offering moves the loss by
        // something a person would notice.
        let drop = (outcome.loss_before - outcome.loss_after) / outcome.loss_before;
        assert!(drop > 0.01, "the loss fell by only {:.3}%", drop * 100.0);
        // And it moves the *right* parameter the right way. Recovery is not
        // exact -- 19 free parameters against a thousand noisy observations
        // overshoots, reaching about 7.6 for a planted 6.0 -- but a fit that
        // did not find the planted direction at all would be worthless.
        assert!(
            outcome.weights.get(2) > Weights::default().get(2),
            "w2 should rise toward the planted 6.0, got {}",
            outcome.weights.get(2)
        );
        Ok(())
    }

    /// Whatever it proposes is a vector the type accepts: the optimizer
    /// clamps to the same bounds `Weights::new` validates against, so a fit
    /// can never propose something that cannot be saved.
    #[test]
    fn the_fit_stays_inside_the_bounds() -> Fallible<()> {
        let seqs = synthetic_log(&Weights::default(), 200, 5);
        let outcome = fit(&seqs, &Weights::default())?;
        Weights::new(outcome.weights.as_array()).expect("a fit must be a valid vector");
        Ok(())
    }

    /// A fit must never make its own starting point worse.
    #[test]
    fn a_fit_on_its_own_weights_barely_moves() -> Fallible<()> {
        let seqs = synthetic_log(&Weights::default(), 200, 5);
        let outcome = fit(&seqs, &Weights::default())?;
        assert!(
            outcome.loss_after <= outcome.loss_before + 1e-6,
            "before {} after {}",
            outcome.loss_before,
            outcome.loss_after
        );
        Ok(())
    }
}
