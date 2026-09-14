# FSRS Weights Implementation Plan (Part B)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** make the 19 FSRS weights data rather than a compiled-in constant,
and let a user read and change them.

**Architecture:** `pub const W: [f64; 19]` becomes a validated `Weights`
newtype that every function in `fsrs.rs` takes as a parameter. `Scheduling`
carries it, so it reaches the scheduler by the route retention and the
maximum interval already take, and it resolves through the same three layers
(instance → user → collection) that Part A built.

**Tech Stack:** Rust 2024, maud, rusqlite, serde. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-09-13-fsrs-settings-design.md` (§6)

**Depends on:** Part A (`docs/superpowers/plans/2026-09-13-settings-surface.md`),
which is implemented and merged into this branch.

## Global Constraints

As Part A: no `unwrap()` in production code, `Fallible`/`?`/`fail()`,
newtypes for domain concepts, imports over qualified paths, small functions,
non-reentrant connection mutex (one lock per method, free functions under
it), failing test first, commit per task, `CHANGELOG.xml` at the end.

**The one constraint specific to this part:** the five golden sequences in
`src/fsrs.rs` (`test_3e`, `test_3g`, `test_2h`, `test_2f`, `test_gf`) must
keep their current expected values **unchanged** throughout. They are the
proof that parameterizing the weights moved nobody's schedule. If a change
requires editing one of those expectations, the change is wrong.

## Deviation from the spec, decided here

The spec says the page offers "the 19 values in a table ... each editable,
plus paste-a-whole-vector". Nineteen number inputs *and* a paste box are two
controls editing one value, and they disagree the moment someone uses both.

This plan ships **one textarea holding the 19 numbers, comma-separated**,
beside a read-only table that names what each index governs. That format is
what FSRS optimizers (Anki's included) already emit, so pasting a fitted
vector is the natural gesture rather than a second feature. Part C's
optimizer writes into the same box.

## File Structure

| File | Responsibility |
|---|---|
| `src/fsrs.rs` | `Weights` newtype, per-index bounds; every function takes `&Weights`. |
| `src/types/performance.rs` | `Scheduling` carries `Weights`. |
| `src/user_settings.rs` | `weights` field, stored as one `meta` row. |
| `src/cmd/serve/config.rs` | `[defaults].weights`, three-layer resolution. |
| `src/cmd/serve/cards.rs` | `weights` in `.hashcards.toml`. |
| `src/cmd/serve/settings.rs` | The weight editor and its explanatory table. |
| `src/cmd/serve/collection_settings.rs` | Per-collection weight override. |

---

### Task 1: `Weights`

**Files:**
- Modify: `src/fsrs.rs`

**Interfaces:**
- Produces: `Weights` (`Clone, Copy, Debug, PartialEq, Default`);
  `Weights::DEFAULT: [f64; 19]`; `Weights::new([f64; 19]) ->
  Fallible<Weights>`; `Weights::get(usize) -> f64`;
  `Weights::as_array() -> [f64; 19]`; `Weights::parse_list(&str) ->
  Fallible<Weights>`; `Weights::to_list(self) -> String`;
  `Weights::is_default(&self) -> bool`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/fsrs.rs`:

```rust
    #[test]
    fn the_default_weights_are_the_constant_that_was_compiled_in() {
        assert_eq!(Weights::default().as_array(), Weights::DEFAULT);
        assert!(Weights::default().is_default());
    }

    /// The formulas are not total. A negative `W[9]` inverts the stability
    /// exponent and a non-positive `W[11]` drives the failure branch
    /// negative, so the bounds are part of the type rather than a caller's
    /// responsibility.
    #[test]
    fn weights_outside_their_bounds_are_refused() {
        let mut w = Weights::DEFAULT;
        w[9] = -1.0;
        assert!(Weights::new(w).is_err());

        let mut w = Weights::DEFAULT;
        w[4] = 0.0;
        assert!(Weights::new(w).is_err(), "initial difficulty must be positive");

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
    fn the_rejection_names_the_weight() {
        let mut w = Weights::DEFAULT;
        w[9] = -1.0;
        let err = Weights::new(w).expect_err("out of bounds");
        assert!(err.to_string().contains("9"), "message was: {err}");
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
    fn a_list_of_the_wrong_length_is_refused() {
        let err = Weights::parse_list("0.4, 1.2").expect_err("two is not nineteen");
        assert!(err.to_string().contains("19"), "message was: {err}");
        assert!(Weights::parse_list("").is_err());
        assert!(Weights::parse_list("a, b, c").is_err());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --bin hashcards-web weight`
Expected: FAIL to compile — `Weights` does not exist.

- [ ] **Step 3: Implement the newtype**

Replace `pub const W: [f64; 19] = [...]` in `src/fsrs.rs` with:

```rust
/// Per-index bounds, `(low, high)` inclusive.
///
/// FSRS's own clamps. They exist because the formulas are not total: a
/// negative `W[9]` inverts the stability exponent, `W[4]` is a difficulty
/// and difficulty is bounded 1..10, and `W[7]` is a mixing fraction that
/// means nothing outside 0..1.
const BOUNDS: [(f64, f64); 19] = [
    (0.001, 100.0),   // 0  initial stability, Forgot
    (0.001, 100.0),   // 1  initial stability, Hard
    (0.001, 100.0),   // 2  initial stability, Good
    (0.001, 100.0),   // 3  initial stability, Easy
    (1.0, 10.0),      // 4  initial difficulty
    (0.001, 4.0),     // 5  initial difficulty, grade exponent
    (0.001, 4.0),     // 6  difficulty step per grade
    (0.0, 0.75),      // 7  difficulty mean-reversion fraction
    (0.0, 4.5),       // 8  stability growth, constant
    (-0.5, 0.0),      // 9  stability growth, stability exponent
    (0.0, 3.0),       // 10 stability growth, retrievability
    (0.001, 0.8),     // 11 failure, constant
    (-0.2, -0.01),    // 12 failure, difficulty exponent
    (0.01, 0.9),      // 13 failure, stability exponent
    (0.01, 3.0),      // 14 failure, retrievability
    (0.0, 1.0),       // 15 Hard penalty
    (1.0, 6.0),       // 16 Easy bonus
    (0.0, 2.0),       // 17 short-term, constant
    (0.0, 2.0),       // 18 short-term, grade
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
            .split(|c| c == ',' || c == '\n')
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
```

Note the `[f64; 19]` literal is the old `W`, copied verbatim. Compare it
against `git show HEAD:src/fsrs.rs` before moving on — a typo here is a
silently different schedule for everyone.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --bin hashcards-web weight`
Expected: PASS, 5 tests.

- [ ] **Step 5: Commit**

```bash
git add src/fsrs.rs
git commit -m "feat: Weights, the 19 FSRS parameters as data"
```

---

### Task 2: every FSRS function takes the weights

**Files:**
- Modify: `src/fsrs.rs`, `src/types/performance.rs`

**Interfaces:**
- `initial_stability(g, w)`, `initial_difficulty(g, w)`,
  `new_stability(d, s, r, g, w)`, `new_difficulty(d, g, w)` — each gaining a
  final `w: &Weights`. `retrievability` and `interval` are unchanged: they
  use `F` and `C`, which are not weights.
- `Scheduling` gains `pub weights: Weights`.

- [ ] **Step 1: Thread the parameter through `fsrs.rs`**

Replace every `W[n]` with `w.get(n)` and add `w: &Weights` as the last
parameter of the four functions above and the private helpers `s_success`,
`s_fail`, `dp`, `delta_d`.

- [ ] **Step 2: Update the golden tests without changing their expectations**

In `mod tests`, `sim()` passes `&Weights::default()` to each call. The
`Step` values in `test_3e`, `test_3g`, `test_2h`, `test_2f` and `test_gf`
are **not** edited. `test_initial_difficulty_of_forgetting` compares against
`Weights::default().get(4)` instead of `W[4]`.

- [ ] **Step 3: Run the golden tests**

Run: `cargo test --bin hashcards-web fsrs`
Expected: PASS with the original numbers. `test_3e` still expects
`s: 15.69`. If any expectation had to move, revert and find the typo.

- [ ] **Step 4: Carry the weights on `Scheduling`**

Add to `Scheduling` in `src/types/performance.rs`:

```rust
    /// The FSRS parameters this schedule is computed with.
    ///
    /// Per instance, per user and per collection, like the two numbers
    /// beside it: a fit is only as good as the history it came from, and
    /// one person's Spanish and their anatomy deck do not have to share
    /// one.
    pub weights: Weights,
```

`update_performance` passes `&scheduling.weights` to the four calls it
makes. `Scheduling::default()` is unchanged in behaviour, which the existing
`performance.rs` tests already assert.

- [ ] **Step 5: Run the whole suite**

Run: `cargo test`
Expected: PASS, all 670 plus the new ones. Fix any `Scheduling { .. }`
literal the compiler names by adding `weights: Weights::default()` or
`..Default::default()`.

- [ ] **Step 6: Commit**

```bash
git add src/fsrs.rs src/types/performance.rs
git commit -m "feat: the scheduler takes its weights as a parameter"
```

---

### Task 3: weights resolve through the three layers

**Files:**
- Modify: `src/user_settings.rs`, `src/cmd/serve/config.rs`,
  `src/cmd/serve/cards.rs`

**Interfaces:**
- `UserSettings` gains `weights: Option<Weights>`, stored under
  `KEY_WEIGHTS = "setting.weights"` as the comma-separated list.
- `SchedulingOverrides` gains `weights: Option<Weights>`.
- `DefaultsSection` gains `weights: Option<Vec<f64>>`, validated in
  `scheduling()`.
- `ResolvedCollection::scheduling` resolves `weights` collection → user →
  instance, exactly as it resolves retention.

- [ ] **Step 1: Write the failing tests**

In `src/user_settings.rs` tests, extend `settings_round_trip` with a
non-default `Weights` and add:

```rust
    /// A weight vector that will not parse costs the weights and nothing
    /// else, as every other unreadable setting does.
    #[test]
    fn junk_weights_inherit() -> Fallible<()> {
        let db = UserDatabase::memory()?;
        db.put_meta_for_test(KEY_WEIGHTS, "1, 2, 3")?;
        assert_eq!(db.user_settings().weights, None);
        Ok(())
    }
```

In `src/cmd/serve/config.rs` tests:

```rust
    #[test]
    fn weights_resolve_like_the_other_knobs() -> Fallible<()> {
        let mut mine = Weights::DEFAULT;
        mine[0] = 0.5;
        let mine = Weights::new(mine)?;
        let mut theirs = Weights::DEFAULT;
        theirs[0] = 0.6;
        let theirs = Weights::new(theirs)?;

        let user = UserSettings {
            weights: Some(mine),
            ..UserSettings::default()
        };
        let rc = test_collection(SchedulingOverrides {
            weights: Some(theirs),
            ..SchedulingOverrides::default()
        });
        assert_eq!(
            rc.scheduling(Scheduling::default(), &user).weights,
            theirs,
            "the collection's own fit wins"
        );

        let rc = test_collection(SchedulingOverrides::default());
        assert_eq!(
            rc.scheduling(Scheduling::default(), &user).weights,
            mine,
            "otherwise the user's"
        );
        assert_eq!(
            rc.scheduling(Scheduling::default(), &UserSettings::default()).weights,
            Weights::default(),
            "otherwise the instance's"
        );
        Ok(())
    }

    /// A `[defaults].weights` of the wrong length is a configuration error
    /// at startup, not a silent pad: an administrator is there to read it.
    #[test]
    fn a_short_weight_list_in_config_is_refused() {
        let toml = "[server]\ndata_dir = \"/tmp\"\n\n[defaults]\nweights = [0.4, 1.2]\n";
        let config: ServeConfig = toml::from_str(toml).expect("parses");
        assert!(config.defaults.scheduling().is_err());
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --bin hashcards-web weight`
Expected: FAIL — the fields do not exist.

- [ ] **Step 3: Implement the three layers**

Follow Part A's pattern exactly, field for field:

- `user_settings.rs`: `KEY_WEIGHTS`, read with `lenient(KEY_WEIGHTS,
  Weights::parse_list(&v))`, write `settings.weights.map(|w|
  w.to_list())`.
- `config.rs`: `#[serde(default)] pub weights: Option<Vec<f64>>` on
  `DefaultsSection`; `scheduling()` converts it with a length check and
  `Weights::new`; `ResolvedCollection::scheduling` resolves
  `self.overrides.weights.or(user.weights).unwrap_or(defaults.weights)`.
- `cards.rs`: `weights: Option<toml::Value>` on `CollectionMeta`, read
  leniently as an array of numbers, written back by
  `write_collection_overrides` — which gains a `weights` argument and must
  **preserve** it the way it preserves limits, so no caller erases a fit by
  touching something else. Update `set_scheduling_for` accordingly.

- [ ] **Step 4: Run the suite**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: resolve FSRS weights through instance, user and collection"
```

---

### Task 4: the weight editor

**Files:**
- Modify: `src/cmd/serve/settings.rs`,
  `src/cmd/serve/collection_settings.rs`

**Interfaces:**
- `SettingsForm` and `CollectionSettingsForm` gain `weights: String`.
- Blank means inherit, as every other field does.

- [ ] **Step 1: Write the failing tests**

In `src/cmd/serve/settings.rs` tests:

```rust
    /// The box holds the whole vector in the format an optimizer emits, so
    /// a fitted list can be pasted straight in.
    #[test]
    fn the_weights_box_shows_the_vector_in_force() -> Fallible<()> {
        let html = render_settings(
            &UserSettings::default(),
            Scheduling::default(),
            DailyLimits::default(),
            None,
        )
        .into_string();
        assert!(html.contains("name=\"weights\""), "{html}");
        // The default vector is shown as what it is inheriting.
        assert!(html.contains("0.40255"), "{html}");
        // And the table says what the numbers govern, so 19 bare floats are
        // not the whole of the explanation.
        assert!(html.contains("initial stability"), "{html}");
        Ok(())
    }

    #[test]
    fn a_pasted_weight_vector_is_accepted() -> Fallible<()> {
        let mut w = Weights::DEFAULT;
        w[0] = 0.5;
        let text = Weights::new(w)?.to_list();
        let parsed = settings_from_form(&SettingsForm {
            weights: text,
            ..SettingsForm::default()
        })?;
        assert_eq!(parsed.weights, Some(Weights::new(w)?));
        Ok(())
    }

    /// Blank clears the override, so "reset to default" is emptying the box
    /// rather than pasting the defaults back in -- which would freeze
    /// today's defaults into the user's settings forever.
    #[test]
    fn an_empty_weights_box_inherits() -> Fallible<()> {
        let parsed = settings_from_form(&SettingsForm::default())?;
        assert_eq!(parsed.weights, None);
        Ok(())
    }

    /// A bad paste is refused with a message that names the problem, and
    /// the count, because 19 numbers are hard to eyeball.
    #[test]
    fn a_bad_weight_paste_is_refused_with_a_count() {
        let err = settings_from_form(&SettingsForm {
            weights: "0.4, 1.2, 3.1".to_string(),
            ..SettingsForm::default()
        })
        .expect_err("three is not nineteen");
        assert!(err.to_string().contains("19"), "message was: {err}");
        assert!(err.to_string().contains('3'), "message was: {err}");
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --bin hashcards-web weight`
Expected: FAIL — `SettingsForm` has no field `weights`.

- [ ] **Step 3: Implement**

Add the field to both forms; parse with `Weights::parse_list` when
non-blank. In `render_settings`, add an "FSRS weights" block:

```rust
    h2 { "FSRS weights" }
    p.hint {
        "The 19 numbers the scheduling formulas are built from. The defaults \
         are fitted to a large population of other people's reviews; these \
         can be fitted to yours. Paste a list from an optimizer, or empty \
         the box to go back to the defaults."
    }
    textarea.input name="weights" rows="4"
        placeholder=(inherited_scheduling.weights.to_list()) {
        (user.weights.map(|w| w.to_list()).unwrap_or_default())
    }
    @if user.weights.is_none() {
        (inherited(inherited_scheduling.weights.to_list()))
    }
    table.weight-table {
        tbody {
            @for (range, what) in WEIGHT_GROUPS {
                tr { td { (range) } td { (what) } }
            }
        }
    }
```

with:

```rust
/// What each block of the vector governs, so the numbers are not 19
/// anonymous floats.
const WEIGHT_GROUPS: [(&str, &str); 5] = [
    ("0-3", "initial stability, one per grade"),
    ("4-5", "initial difficulty"),
    ("6-7", "how difficulty moves with each grade"),
    ("8-16", "how stability grows on success and collapses on a lapse"),
    ("17-18", "same-day reviews"),
];
```

The collection page gets the same box without the table, linking to
`/settings` for the explanation.

- [ ] **Step 4: Run the suite**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: all clean.

- [ ] **Step 5: Verify by hand**

Start the server, open `/settings`, paste a vector with `W[0]` changed,
save, reload and confirm it stuck; empty the box, save, and confirm it says
it is inheriting again.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat: read and edit the FSRS weights"
```

---

## Self-Review

**Spec coverage:** §6 (weights as data) → Tasks 1, 2; the three-layer
resolution the spec's table asks for → Task 3; the editor → Task 4. §8's
MCP extension is folded into Task 3, since `write_collection_overrides`
gains the parameter there and `set_scheduling_for` must preserve it.

**Type consistency:** `Weights::new`/`get`/`as_array`/`parse_list`/`to_list`
/`is_default`/`DEFAULT` (Tasks 1–4); `Scheduling.weights` (Tasks 2, 3);
`UserSettings.weights` and `KEY_WEIGHTS` (Task 3); `SettingsForm.weights`
(Task 4).

**Known risk:** the `BOUNDS` table is this plan's own judgement, derived
from what the formulas require rather than copied from a published FSRS
table. Too tight a bound rejects a legitimate fit; Task 1's tests pin the
cases that matter (the ones that make the formulas undefined), and Part C's
optimizer clamps to these same bounds, so the two cannot disagree.
