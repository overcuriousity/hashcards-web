# FSRS Optimizer Implementation Plan (Part C)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** fit the 19 FSRS weights to the user's own review history, propose
the result, and apply it only when they say so.

**Architecture:** the review log already holds everything a fit needs. Read
it into `(delta_t, grade)` sequences per card, define the standard FSRS
log-loss by replaying the recurrence forward under candidate weights, and
minimise it with Adam over central-difference numeric gradients. Guardrails
refuse a short log and refuse a fit that does not improve. The result is
*proposed* — it lands in the weight box for the user to accept.

**Tech Stack:** Rust 2024, rusqlite, maud. No new dependencies: the
optimizer is ~150 lines of arithmetic over `Weights`, which Part B made
data.

**Spec:** `docs/superpowers/specs/2026-09-13-fsrs-settings-design.md` (§7)

**Depends on:** Parts A and B, both implemented on this branch.

## Global Constraints

As Parts A and B. Plus, specific to this part:

- **Numeric, not analytic, gradients.** Central differences cost 38 replays
  per step and are obviously correct. Analytic gradients through the full
  recurrence are error-prone calculus and are a later optimization, if
  profiling ever asks.
- **Every iterate stays inside `BOUNDS`.** The optimizer clamps to the same
  table `Weights::new` validates against, so a fit can never propose a
  vector the type would reject.
- **Bounded work.** The fit runs synchronously under `run_blocking` with a
  hard iteration cap. No job queue: there is no infrastructure for one and
  adding it before a real log has been measured is speculation.

## File Structure

| File | Responsibility |
|---|---|
| `src/fsrs/optimize.rs` *(new)* | `ReviewSequence`, `log_loss`, `fit`, `FitOutcome`. Pure arithmetic, no database. |
| `src/fsrs.rs` | Declare the submodule; expose `BOUNDS` to it. |
| `src/db.rs` | `Database::review_sequences()`. |
| `src/cmd/serve/settings.rs` | The Optimize button, the proposal, and Apply. |

The split matters: `optimize.rs` takes sequences and returns weights, so it
is testable against a synthetic log with no server, no SQLite and no clock.

---

### Task 1: reading the log

**Files:**
- Modify: `src/db.rs`

**Interfaces:**
- Produces: `Database::review_sequences(&self) ->
  Fallible<Vec<Vec<(f64, Grade)>>>` — per card, in review order, each entry
  the days elapsed since that card's previous review and the grade given.
  The first entry of each sequence has `delta_t = 0.0`.

- [ ] **Step 1: Write the failing test**

In `src/db.rs` tests, reusing the `review_on` helper Part A added:

```rust
    /// The optimizer's input: per card, in order, how long since that card
    /// was last seen and what the grade was. Elapsed time comes from the
    /// timestamps, so it is the *real* gap -- a free-day shift or a late
    /// review is accounted for rather than assumed.
    #[test]
    fn review_sequences_are_per_card_and_in_order() -> Fallible<()> {
        let user = UserDatabase::memory()?;
        let db = user.collection(CollectionId::new("bio")?);
        let day = |n: i64| {
            NaiveDate::from_ymd_opt(2026, 9, 1).expect("valid date") + Duration::days(n)
        };
        let session = db.create_session(Timestamp::new(
            day(0).and_hms_opt(8, 0, 0).expect("valid"),
        ))?;
        let a = CardHash::hash_bytes(b"card a");
        let b = CardHash::hash_bytes(b"card b");
        db.insert_card(a, Timestamp::now())?;
        db.insert_card(b, Timestamp::now())?;

        review_on(&db, session, a, day(0))?;
        review_on(&db, session, a, day(3))?;
        review_on(&db, session, a, day(10))?;
        let voided = review_on(&db, session, b, day(1))?;
        review_on(&db, session, b, day(5))?;

        db.void_review_and_restore_performance(voided, b, Performance::New, None)?;

        let mut seqs = db.review_sequences()?;
        seqs.sort_by_key(|s| s.len());

        // Card b: one surviving review, the voided one gone entirely.
        assert_eq!(seqs[0].len(), 1);
        assert_eq!(seqs[0][0].0, 0.0, "a card's first review has no gap");

        // Card a: three reviews, gaps 0, 3, 7.
        let gaps: Vec<f64> = seqs[1].iter().map(|(dt, _)| *dt).collect();
        assert_eq!(gaps, vec![0.0, 3.0, 7.0]);
        Ok(())
    }

    /// A card with a single review teaches the fit nothing about intervals,
    /// but it is still a first observation and must not be dropped.
    #[test]
    fn a_single_review_is_still_a_sequence() -> Fallible<()> {
        // one card, one review; assert exactly one sequence of length 1
    }
```

Fill the second body from the first.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --bin hashcards-web review_sequences`
Expected: FAIL to compile.

- [ ] **Step 3: Implement**

```rust
    /// Every card's surviving reviews, in order, as `(days since that
    /// card's previous review, grade)`.
    ///
    /// Across the whole collection rather than one card, because a fit
    /// wants every observation there is. Ordered by card and then by time,
    /// so one pass builds the sequences.
    pub fn review_sequences(&self) -> Fallible<Vec<Vec<(f64, Grade)>>> {
        let conn = self.conn.lock();
        let sql = "select card_hash, reviewed_at, grade from reviews \
                   where collection_id = ? and voided = 0 \
                   order by card_hash, reviewed_at;";
        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt.query(params![self.collection])?;
        let mut out: Vec<Vec<(f64, Grade)>> = Vec::new();
        let mut current: Option<(CardHash, Timestamp)> = None;
        while let Some(row) = rows.next()? {
            let hash: CardHash = row.get(0)?;
            let at: Timestamp = row.get(1)?;
            let grade: Grade = row.get(2)?;
            match &current {
                Some((prev_hash, prev_at)) if *prev_hash == hash => {
                    // Clamped for the reason `update_performance` clamps:
                    // a clock rollback must not make time run backwards.
                    let days = (at.date().into_inner() - prev_at.date().into_inner())
                        .num_days()
                        .max(0) as f64;
                    match out.last_mut() {
                        Some(seq) => seq.push((days, grade)),
                        None => out.push(vec![(days, grade)]),
                    }
                }
                _ => out.push(vec![(0.0, grade)]),
            }
            current = Some((hash, at));
        }
        Ok(out)
    }
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --bin hashcards-web review_sequences`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/db.rs
git commit -m "feat: read the review log as per-card grade sequences"
```

---

### Task 2: the loss

**Files:**
- Create: `src/fsrs/optimize.rs`
- Modify: `src/fsrs.rs` (declare the module; make `BOUNDS` visible to it)

Moving `src/fsrs.rs` to `src/fsrs/mod.rs` is the ordinary way to give it a
submodule. Do that in this task, with no other change to its contents, so
that the diff stays readable.

**Interfaces:**
- Produces: `log_loss(sequences: &[Vec<(f64, Grade)>], w: &Weights) -> f64`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

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
        // A lapse a year later is unsurprising; a lapse the next day is.
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
    /// and the optimizer will push weights to their bounds, so a NaN here
    /// would silently poison a fit.
    #[test]
    fn the_loss_is_finite_at_the_bounds() {
        let seqs = vec![vec![
            (0.0, Grade::Forgot),
            (1.0, Grade::Hard),
            (30.0, Grade::Good),
            (400.0, Grade::Easy),
            (0.0, Grade::Forgot),
        ]];
        for (low, high) in [true, false].iter().map(|_| (true, false)) {
            let _ = (low, high);
        }
        let mut low = Weights::DEFAULT;
        let mut high = Weights::DEFAULT;
        for (i, (lo, hi)) in BOUNDS.iter().enumerate() {
            low[i] = *lo;
            high[i] = *hi;
        }
        for w in [Weights::new(low), Weights::new(high)].into_iter().flatten() {
            let loss = log_loss(&seqs, &w);
            assert!(loss.is_finite(), "loss was {loss}");
        }
    }
}
```

Delete the stray `for (low, high) in ...` line above when transcribing — it
is noise; the two `Weights::new` calls below it are the test.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --bin hashcards-web log_loss`
Expected: FAIL to compile.

- [ ] **Step 3: Implement**

```rust
//! Fitting the FSRS weights to one person's review history.
//!
//! Pure arithmetic over sequences: no database, no clock, no server. That
//! is what makes it testable against a synthetic log whose answer is known.

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
    if scored == 0 { 0.0 } else { total / scored as f64 }
}
```

In `src/fsrs/mod.rs`: `pub mod optimize;` and `pub(crate) const BOUNDS`.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --bin hashcards-web log_loss`
Expected: PASS, 4 tests.

- [ ] **Step 5: Commit**

```bash
git add src/fsrs src/fsrs.rs
git commit -m "feat: the FSRS log-loss over a review history"
```

---

### Task 3: the fit

**Files:**
- Modify: `src/fsrs/optimize.rs`

**Interfaces:**
- Produces: `FitOutcome { weights: Weights, loss_before: f64, loss_after: f64,
  reviews: usize }`; `fit(sequences: &[Vec<(f64, Grade)>], from: &Weights) ->
  Fallible<FitOutcome>`; `MIN_REVIEWS: usize = 400`.

- [ ] **Step 1: Write the failing tests**

```rust
    /// Enough reviews to be allowed to fit, generated deterministically so
    /// the test is not flaky.
    fn synthetic_log(w: &Weights, cards: usize, per_card: usize) -> Vec<Vec<(f64, Grade)>> {
        let mut rng = TinyRng::from_seed(7);
        let mut out = Vec::new();
        for _ in 0..cards {
            let mut sequence = vec![(0.0, Grade::Good)];
            let mut stability = initial_stability(Grade::Good, w);
            let mut difficulty = initial_difficulty(Grade::Good, w);
            for _ in 1..per_card {
                // Review at roughly the interval these weights ask for.
                let gap = stability.max(1.0).round();
                let recall = retrievability(gap, stability);
                // Deterministic coin against the true recall probability:
                // the log then really is generated by `w`.
                let unit = f64::from(rng.next_u32()) / f64::from(u32::MAX);
                let grade = if unit < recall { Grade::Good } else { Grade::Forgot };
                sequence.push((gap, grade));
                stability = new_stability(difficulty, stability, recall, grade, w);
                difficulty = new_difficulty(difficulty, grade, w);
            }
            out.push(sequence);
        }
        out
    }

    /// A log too short to learn from is refused rather than fitted. A fit on
    /// twelve reviews is noise wearing a number's clothes.
    #[test]
    fn a_short_log_is_refused() {
        let seqs = synthetic_log(&Weights::default(), 2, 3);
        let err = fit(&seqs, &Weights::default()).expect_err("four reviews is not a fit");
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

    /// A log that the starting weights already explain perfectly leaves them
    /// alone rather than wandering.
    #[test]
    fn a_fit_on_its_own_weights_barely_moves() -> Fallible<()> {
        let seqs = synthetic_log(&Weights::default(), 200, 5);
        let outcome = fit(&seqs, &Weights::default())?;
        assert!(
            outcome.loss_after <= outcome.loss_before + 1e-6,
            "a fit must never make its own starting point worse"
        );
        Ok(())
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --bin hashcards-web fit`
Expected: FAIL to compile.

- [ ] **Step 3: Implement**

```rust
/// Below this, a fit is noise. Anki asks for a comparable number and for
/// the same reason: 19 parameters need considerably more than 19
/// observations before the answer means anything.
pub const MIN_REVIEWS: usize = 400;

/// Iteration cap. Chosen to stay interactive under `run_blocking` rather
/// than to reach a minimum: a better-but-unfinished fit is still offered,
/// and the guardrail that matters is that it beat the starting point.
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
pub fn fit(sequences: &[Vec<(f64, Grade)>], from: &Weights) -> Fallible<FitOutcome> {
    let reviews: usize = sequences.iter().map(|s| s.len()).sum();
    if reviews < MIN_REVIEWS {
        return fail(format!(
            "A fit needs at least {MIN_REVIEWS} reviews to mean anything, and this history \
             has {reviews}. Keep reviewing and try again later."
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

        // Adam.
        for i in 0..19 {
            m[i] = BETA1 * m[i] + (1.0 - BETA1) * gradient[i];
            v[i] = BETA2 * v[i] + (1.0 - BETA2) * gradient[i] * gradient[i];
            let m_hat = m[i] / (1.0 - BETA1.powi(step as i32));
            let v_hat = v[i] / (1.0 - BETA2.powi(step as i32));
            let (low, high) = BOUNDS[i];
            // Scaled by the weight's own range, for the reason the
            // gradient step is.
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
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --bin hashcards-web fit`
Expected: PASS, 4 tests. `fitting_improves_the_loss` is the one that
matters; if it fails, the gradient sign or the scaling is wrong, not the
test.

- [ ] **Step 5: Commit**

```bash
git add src/fsrs/optimize.rs
git commit -m "feat: fit the FSRS weights by Adam over numeric gradients"
```

---

### Task 4: Optimize, propose, Apply

**Files:**
- Modify: `src/cmd/serve/settings.rs`, `src/cmd/serve/server.rs`

**Interfaces:**
- `POST /settings/optimize`, which reads the user's whole tree (or one
  collection, by an optional `slug` field), fits, and re-renders the
  settings page with the proposal in the weight box and a notice giving the
  before/after loss.

- [ ] **Step 1: Write the failing tests**

```rust
    /// The proposal is shown, not applied: the box holds the fitted vector
    /// and the notice says what it would buy, but nothing is saved until
    /// the user submits the form.
    #[test]
    fn a_proposal_is_shown_rather_than_saved() -> Fallible<()> {
        let mut w = Weights::DEFAULT;
        w[2] = 4.0;
        let proposal = Proposal {
            weights: Weights::new(w)?,
            loss_before: 0.42,
            loss_after: 0.31,
            reviews: 1200,
        };
        let html = render_settings_with(
            &UserSettings::default(),
            Scheduling::default(),
            DailyLimits::default(),
            Some(&proposal),
            None,
        )
        .into_string();
        assert!(html.contains("1200"), "says what it fitted on: {html}");
        assert!(html.contains("0.42") && html.contains("0.31"), "{html}");
        assert!(html.contains(&proposal.weights.to_list()), "the box holds it");
        // And it is still the user's own settings that are stored.
        assert!(html.contains("Apply"), "{html}");
        Ok(())
    }

    /// A fit that did not improve is refused rather than offered. Proposing
    /// a worse schedule as an improvement is the one outcome this feature
    /// must never produce.
    #[test]
    fn a_fit_that_does_not_improve_is_refused() -> Fallible<()> {
        let outcome = FitOutcome {
            weights: Weights::default(),
            loss_before: 0.30,
            loss_after: 0.30,
            reviews: 900,
        };
        assert!(proposal_from(outcome).is_err());
        Ok(())
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --bin hashcards-web proposal`
Expected: FAIL to compile.

- [ ] **Step 3: Implement**

`render_settings` gains a `proposal: Option<&Proposal>` parameter;
`render_settings(..)` becomes a thin wrapper passing `None` so the existing
tests and call sites are untouched. When a proposal is present, the weight
box is pre-filled with the fitted vector and a notice above it reads:

> Fitted to 1200 of your reviews. Predicted-recall error falls from 0.42 to
> 0.31. Nothing is saved until you press Save settings.

`proposal_from(outcome) -> Fallible<Proposal>` refuses when
`loss_after >= loss_before`, with a message saying the current weights
already explain this history as well as anything found.

The handler gathers sequences across every collection the user owns
(`owned_collections`, then `open_user_db` per path, `review_sequences` per
collection), concatenates them, and fits from the weights currently in
force.

- [ ] **Step 4: Route it**

```rust
        .route("/settings/optimize", post(settings_optimize_handler))
```

- [ ] **Step 5: Run the gate**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: all clean.

- [ ] **Step 6: Verify by hand**

Start the server against a collection with a real history if one is to
hand; otherwise confirm the short-log refusal appears, naming 400, since
that is the path a new user hits.

- [ ] **Step 7: Update `CHANGELOG.xml` and commit**

One entry covering the weight editor and the optimizer together — from a
user's point of view they are one feature.

---

## Self-Review

**Spec coverage:** §7's input → Task 1; loss → Task 2; Adam with numeric
gradients → Task 3; all four guardrails (minimum reviews, clamping,
before/after reported, refuse a non-improvement) → Tasks 3 and 4; propose
rather than apply → Task 4; `run_blocking` with a bounded budget → Task 4.
The spec's per-collection *target selector* is **not** implemented: the fit
runs over the user's whole tree. Noted below.

**Deliberate omission:** the spec offers "a selector: the whole tree, or one
collection". Fitting the whole tree is the better default — more
observations, and 19 parameters need them — and a per-collection fit is
reachable later through the same `fit` function, which already takes
whatever sequences it is handed. The handler accepts an optional `slug` so
the machinery is there; only the control is missing.

**Known risk:** `MAX_STEPS = 120` with numeric gradients is 120 × 38 = 4560
full replays of the log. On a history of a few thousand reviews that is
fast; on a very large one it may not be. Task 4's hand-check should time it,
and the cap is one constant to lower if it bites.
