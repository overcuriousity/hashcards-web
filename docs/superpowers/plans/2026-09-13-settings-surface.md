# Settings Surface Implementation Plan (Part A)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** give the web interface a settings page, backed by a per-user
settings layer, adding daily review limits and free weekdays.

**Architecture:** settings resolve in three layers — instance `[defaults]` →
user → collection. The user layer is stored as rows in the `meta` table of
the user's existing review database and read leniently; the collection layer
is the existing `.hashcards.toml`. Two new validated newtypes (`FreeDays`,
`DailyLimits`) carry the new concepts, and the free-day shift is one
function called at the end of `update_performance`.

**Tech Stack:** Rust 2024, axum 0.8, maud 0.27, rusqlite 0.39 (bundled),
chrono 0.4, serde, toml 1.1. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-09-13-fsrs-settings-design.md`

## Global Constraints

Copied from `CLAUDE.md` and the spec. Every task's requirements include
these:

- No `unwrap()` in production code. Tests may use it.
- `Fallible` and `?` for error handling; `fail()` for custom errors. All
  error messages are user-facing and must read as such.
- Newtypes for domain concepts.
- Prefer `use foo::bar;` over fully qualified `foo::bar()`.
- Keep functions small; module files re-export what is needed and hide the
  rest.
- **Dates are naive.** No timezone type appears anywhere in this plan.
- **The connection mutex is not reentrant.** A `UserDatabase`/`Database`
  method takes the lock once and calls only free functions under it. Never
  call another locking method while holding the lock.
- Write failing test first, watch it fail, then implement.
- Commit after every task.
- `CHANGELOG.xml` is updated in the final task.
- Part A introduces **no** FSRS weight configuration. `fsrs.rs` is not
  touched. Weights are Part B.

## File Structure

| File | Responsibility |
|---|---|
| `src/types/free_days.rs` *(new)* | `FreeDays` newtype: which weekdays take no due dates, and the shift. |
| `src/types/limits.rs` *(new)* | `DailyLimits` and `DailyBudget`: how many cards a collection hands out today. |
| `src/user_settings.rs` *(new)* | `UserSettings` and its `meta`-table read/write free functions. |
| `src/types/mod.rs` | Declare the two new type modules. |
| `src/types/performance.rs` | `Scheduling` gains `free_days`; `update_performance` applies the shift. |
| `src/user_db.rs` | `UserDatabase::user_settings()` / `save_user_settings()`. |
| `src/db.rs` | `Database::reviews_today_count`, `new_cards_today_count`, `new_cards`. |
| `src/cmd/serve/config.rs` | `[defaults]` gains four keys; three-layer resolution on `ResolvedCollection`. |
| `src/cmd/serve/settings.rs` *(new)* | `/settings` page: GET, POST, rendering. |
| `src/cmd/serve/collection_settings.rs` *(new)* | `/collection/{slug}/settings` page. |
| `src/cmd/serve/cards.rs` | `CollectionMeta` gains the two limit keys. |
| `src/cmd/serve/handlers.rs` | Session creation applies the budget. |
| `src/cmd/serve/counts.rs` | `Burial` takes resolved settings; deck counts apply the budget. |
| `src/cmd/serve/landing.rs` | Nav link to `/settings`. |
| `src/cmd/serve/server.rs`, `mod.rs` | Routes and module declarations. |

---

### Task 1: `FreeDays`

**Files:**
- Create: `src/types/free_days.rs`
- Modify: `src/types/mod.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `FreeDays::none() -> FreeDays`, `FreeDays::new(days: [bool; 7])
  -> Fallible<FreeDays>` (Monday first), `FreeDays::is_free(&self, day:
  Weekday) -> bool`, `FreeDays::as_array(&self) -> [bool; 7]`,
  `FreeDays::shift(&self, due: NaiveDate, not_after: NaiveDate) ->
  NaiveDate`, `FreeDays::parse_list(s: &str) -> Fallible<FreeDays>`,
  `FreeDays::to_list(&self) -> Vec<&'static str>`. Derives `Clone, Copy,
  Debug, PartialEq, Default` (default = none free).

- [ ] **Step 1: Write the failing tests**

Create `src/types/free_days.rs` with the Apache header copied verbatim from
the top of `src/types/performance.rs`, then this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Monday 2026-09-14 .. Sunday 2026-09-20.
    fn d(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, 13 + day).expect("valid date")
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib free_days`
Expected: FAIL to compile — `FreeDays` does not exist.

- [ ] **Step 3: Implement `FreeDays`**

Above the test module:

```rust
use chrono::Datelike;
use chrono::Duration;
use chrono::NaiveDate;
use chrono::Weekday;

use crate::error::Fallible;
use crate::error::fail;

/// The three-letter names, Monday first, as they appear in `hashcards.toml`
/// and in a `meta` row.
const NAMES: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// How far the shift will ever have to look. With at least one open day,
/// some day within three is open in one direction or the other.
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
            return fail(
                "at least one weekday must stay open: cards have to come due somewhere",
            );
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

    pub fn as_array(&self) -> [bool; 7] {
        let mut out = [false; 7];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = self.0 & (1 << i) != 0;
        }
        out
    }

    /// The nearest day that takes cards, at or after `due`'s own distance
    /// from itself.
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
```

Add to the test module's imports: `use crate::error::Fallible;`.

- [ ] **Step 4: Declare the module**

In `src/types/mod.rs`, add `pub mod free_days;` in alphabetical position
among the existing `pub mod` lines.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib free_days`
Expected: PASS, 7 tests.

- [ ] **Step 6: Commit**

```bash
git add src/types/free_days.rs src/types/mod.rs
git commit -m "feat: FreeDays, the weekdays that take no due dates"
```

---

### Task 2: `DailyLimits` and `DailyBudget`

**Files:**
- Create: `src/types/limits.rs`
- Modify: `src/types/mod.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `DailyLimits { reviews: Option<u32>, new: Option<u32> }`
  (`Clone, Copy, Debug, PartialEq, Default`; default both `None`);
  `DailyLimits::parse(s: &str) -> Fallible<Option<u32>>` for one field;
  `DailyBudget::new(limits: DailyLimits, reviews_done: usize, new_done:
  usize) -> DailyBudget`; `DailyBudget::admits(&mut self, is_new: bool) ->
  bool`; `DailyBudget::is_limited(&self) -> bool`.

- [ ] **Step 1: Write the failing tests**

Create `src/types/limits.rs` with the Apache header, then:

```rust
#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!b.is_limited());
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib limits`
Expected: FAIL to compile — `DailyLimits` does not exist.

- [ ] **Step 3: Implement**

```rust
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
                "a daily limit must be a whole number of cards, or blank for no limit, \
                 got: {s}"
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
/// session it describes is the bug this type is easiest to introduce.
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

    /// Whether any limit is in force, so a page can say a count was capped
    /// rather than silently showing a smaller number.
    pub fn is_limited(&self) -> bool {
        self.reviews_left.is_some() || self.new_left.is_some()
    }
}
```

Add `use crate::error::Fallible;` to the test module.

- [ ] **Step 4: Declare the module**

In `src/types/mod.rs`, add `pub mod limits;`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib limits`
Expected: PASS, 6 tests.

- [ ] **Step 6: Commit**

```bash
git add src/types/limits.rs src/types/mod.rs
git commit -m "feat: DailyLimits and DailyBudget"
```

---

### Task 3: apply the free-day shift when scheduling

**Files:**
- Modify: `src/types/performance.rs` (`Scheduling` at :119,
  `update_performance` at :199, due date at :238)

**Interfaces:**
- Consumes: `FreeDays` from Task 1.
- Produces: `Scheduling` gains `pub free_days: FreeDays`. `Scheduling`
  keeps `Default`, so `Scheduling::default()` is unchanged behaviour and
  the existing `Scheduling { retention, max_interval, jitter }` literals in
  `config.rs` need `..Default::default()` or the new field.

- [ ] **Step 1: Write the failing tests**

Add to the existing `mod tests` in `src/types/performance.rs`:

```rust
use crate::types::free_days::FreeDays;
use chrono::Datelike;
use chrono::Weekday;

fn scheduling_with_free(days: [bool; 7]) -> Fallible<Scheduling> {
    Ok(Scheduling {
        free_days: FreeDays::new(days)?,
        ..Scheduling::default()
    })
}

/// A weekend-free schedule never lands a card on Saturday or Sunday, over
/// a long run of reviews at many different stabilities.
#[test]
fn free_days_keep_due_dates_off_those_days() -> Fallible<()> {
    let mut days = [false; 7];
    days[5] = true;
    days[6] = true;
    let scheduling = scheduling_with_free(days)?;
    let mut rng = TinyRng::from_seed(42);
    for day in 0..60 {
        let at = Timestamp::new(
            NaiveDate::from_ymd_opt(2026, 9, 14)
                .expect("valid")
                .checked_add_signed(Duration::days(day))
                .expect("in range")
                .and_hms_opt(9, 0, 0)
                .expect("valid"),
        );
        let result = update_performance(Performance::New, Grade::Good, at, scheduling, &mut rng);
        let weekday = result.due_date.into_inner().weekday();
        assert!(
            weekday != Weekday::Sat && weekday != Weekday::Sun,
            "landed on {weekday:?}"
        );
    }
    Ok(())
}

/// `interval_days` describes the date actually written, so the two can
/// never disagree; `interval_raw` stays the unshifted truth, exactly as it
/// stays un-jittered.
#[test]
fn a_shifted_due_date_and_its_interval_agree() -> Fallible<()> {
    let mut days = [false; 7];
    days[5] = true;
    days[6] = true;
    let scheduling = scheduling_with_free(days)?;
    let mut rng = TinyRng::from_seed(7);
    for day in 0..30 {
        let start = NaiveDate::from_ymd_opt(2026, 9, 14)
            .expect("valid")
            .checked_add_signed(Duration::days(day))
            .expect("in range");
        let at = Timestamp::new(start.and_hms_opt(9, 0, 0).expect("valid"));
        let result = update_performance(Performance::New, Grade::Good, at, scheduling, &mut rng);
        assert_eq!(
            result.due_date.into_inner() - start,
            Duration::days(result.interval_days),
            "interval_days must describe the date written"
        );
    }
    Ok(())
}

/// A schedule with no free days is the schedule this codebase had before
/// free days existed.
#[test]
fn no_free_days_changes_no_due_date() {
    let mut a = TinyRng::from_seed(99);
    let mut b = TinyRng::from_seed(99);
    let at = Timestamp::new(
        NaiveDate::from_ymd_opt(2026, 9, 19)
            .expect("valid")
            .and_hms_opt(9, 0, 0)
            .expect("valid"),
    );
    let plain = update_performance(Performance::New, Grade::Good, at, Scheduling::default(), &mut a);
    let explicit = Scheduling {
        free_days: FreeDays::none(),
        ..Scheduling::default()
    };
    let same = update_performance(Performance::New, Grade::Good, at, explicit, &mut b);
    assert_eq!(plain, same);
}
```

If `Timestamp::new` is not the constructor in this codebase, use whichever
one `src/types/timestamp.rs` exposes; check it before writing the test.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib performance`
Expected: FAIL to compile — `Scheduling` has no field `free_days`.

- [ ] **Step 3: Add the field**

In the `Scheduling` struct at `src/types/performance.rs:119`:

```rust
    /// The weekdays that take no due dates.
    ///
    /// Per instance and per user, never per collection — see the note on
    /// `jitter` in `ResolvedCollection::scheduling`, which applies here for
    /// the same reason: which days you are busy is a fact about your week,
    /// not about one collection.
    pub free_days: FreeDays,
```

with `use crate::types::free_days::FreeDays;` at the top.

- [ ] **Step 4: Apply the shift**

Replace the due-date computation at `src/types/performance.rs:236-238`:

```rust
    let interval_duration: Duration = Duration::days(interval_days);
    let ideal: NaiveDate = today + interval_duration;
    // The ceiling outranks the free-day preference: a card already at the
    // maximum interval is pulled earlier rather than pushed past it.
    let not_after: NaiveDate =
        today + Duration::days(scheduling.max_interval.into_inner() as i64);
    let shifted: NaiveDate = scheduling.free_days.shift(ideal, not_after);
    // Recomputed rather than carried, so `interval_days` always describes
    // the date actually written. `interval_raw` stays unshifted for the
    // reason it stays un-jittered.
    let interval_days: i64 = (shifted - today).num_days();
    let due_date: Date = Date::new(shifted);
```

Delete the now-dead `let interval_duration` binding if the borrow checker
or clippy complains about it being unused after the rewrite.

- [ ] **Step 5: Fix the `Scheduling` literals**

Run `cargo build` and add `free_days: FreeDays::none(),` — or
`..Default::default()` — to every `Scheduling { .. }` literal the compiler
names. Expect one in `src/cmd/serve/config.rs:200`.

- [ ] **Step 6: Run the whole suite**

Run: `cargo test`
Expected: PASS, including every pre-existing test. The five golden FSRS
sequences in `src/fsrs.rs` must be untouched and still passing.

- [ ] **Step 7: Commit**

```bash
git add src/types/performance.rs src/cmd/serve/config.rs
git commit -m "feat: shift due dates off free weekdays"
```

---

### Task 4: `UserSettings` and its storage

**Files:**
- Create: `src/user_settings.rs`
- Modify: `src/main.rs` (module declaration), `src/user_db.rs`

**Interfaces:**
- Consumes: `FreeDays`, `DailyLimits`.
- Produces: `UserSettings { retention: Option<DesiredRetention>,
  max_interval: Option<MaxInterval>, jitter: Option<Jitter>, bury_siblings:
  Option<bool>, limits: DailyLimits, free_days: Option<FreeDays> }`
  (`Clone, Copy, Debug, PartialEq, Default`);
  `UserDatabase::user_settings(&self) -> UserSettings` (never fails);
  `UserDatabase::save_user_settings(&self, s: &UserSettings) ->
  Fallible<()>`.

- [ ] **Step 1: Write the failing tests**

Create `src/user_settings.rs` with the Apache header, then:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_db::UserDatabase;

    #[test]
    fn a_fresh_database_inherits_everything() -> Fallible<()> {
        let db = UserDatabase::memory()?;
        assert_eq!(db.user_settings(), UserSettings::default());
        Ok(())
    }

    #[test]
    fn settings_round_trip() -> Fallible<()> {
        let db = UserDatabase::memory()?;
        let settings = UserSettings {
            retention: Some(DesiredRetention::new(0.85)?),
            max_interval: Some(MaxInterval::new(365.0)?),
            jitter: Some(Jitter::new(0.1)?),
            bury_siblings: Some(false),
            limits: DailyLimits {
                reviews: Some(40),
                new: Some(0),
            },
            free_days: Some(FreeDays::parse_list("sat,sun")?),
        };
        db.save_user_settings(&settings)?;
        assert_eq!(db.user_settings(), settings);
        Ok(())
    }

    /// Saving `None` clears the row, so the value inherits again rather
    /// than keeping whatever was there before.
    #[test]
    fn clearing_a_setting_removes_it() -> Fallible<()> {
        let db = UserDatabase::memory()?;
        db.save_user_settings(&UserSettings {
            retention: Some(DesiredRetention::new(0.85)?),
            ..UserSettings::default()
        })?;
        db.save_user_settings(&UserSettings::default())?;
        assert_eq!(db.user_settings(), UserSettings::default());
        Ok(())
    }

    /// A `meta` row nothing can parse costs that one setting and nothing
    /// else. This is the same leniency `collection_overrides` applies, for
    /// the same reason: a preference is not worth failing a page over.
    #[test]
    fn junk_costs_only_its_own_setting() -> Fallible<()> {
        let db = UserDatabase::memory()?;
        db.save_user_settings(&UserSettings {
            max_interval: Some(MaxInterval::new(365.0)?),
            ..UserSettings::default()
        })?;
        db.put_meta_for_test(KEY_RETENTION, "banana")?;
        db.put_meta_for_test(KEY_FREE_DAYS, "caturday")?;

        let loaded = db.user_settings();
        assert_eq!(loaded.retention, None, "junk inherits");
        assert_eq!(loaded.free_days, None, "junk inherits");
        assert_eq!(
            loaded.max_interval,
            Some(MaxInterval::new(365.0)?),
            "its neighbour survives"
        );
        Ok(())
    }

    /// Out of range is junk too: the newtype refuses it on the way in, and
    /// a value that got there another way must not get past on the way out.
    #[test]
    fn an_out_of_range_value_inherits() -> Fallible<()> {
        let db = UserDatabase::memory()?;
        db.put_meta_for_test(KEY_RETENTION, "2.5")?;
        assert_eq!(db.user_settings().retention, None);
        Ok(())
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib user_settings`
Expected: FAIL to compile — `UserSettings` does not exist.

- [ ] **Step 3: Implement `UserSettings` and the free functions**

```rust
//! One user's settings: the layer between the instance's `[defaults]` and
//! a collection's own `.hashcards.toml`.
//!
//! Stored as rows in the `meta` table of the user's review database, one
//! row per setting rather than one blob, so that a single unreadable value
//! cannot cost the others.

use rusqlite::Connection;
use rusqlite::OptionalExtension;
use rusqlite::params;

use crate::error::Fallible;
use crate::types::free_days::FreeDays;
use crate::types::limits::DailyLimits;
use crate::types::performance::DesiredRetention;
use crate::types::performance::Jitter;
use crate::types::performance::MaxInterval;

pub const KEY_RETENTION: &str = "setting.desired_retention";
pub const KEY_MAX_INTERVAL: &str = "setting.max_interval_days";
pub const KEY_JITTER: &str = "setting.jitter";
pub const KEY_BURY_SIBLINGS: &str = "setting.bury_siblings";
pub const KEY_MAX_REVIEWS: &str = "setting.max_reviews_per_day";
pub const KEY_MAX_NEW: &str = "setting.max_new_per_day";
pub const KEY_FREE_DAYS: &str = "setting.free_days";

/// What one user asks for, in place of the instance's settings.
///
/// Every field optional, `None` meaning "inherit". A user who has never
/// opened the settings page is scheduled exactly as they were before this
/// layer existed.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct UserSettings {
    pub retention: Option<DesiredRetention>,
    pub max_interval: Option<MaxInterval>,
    pub jitter: Option<Jitter>,
    pub bury_siblings: Option<bool>,
    pub limits: DailyLimits,
    pub free_days: Option<FreeDays>,
}

/// Read every setting, forgiving every one of them.
///
/// A free function taking a `&Connection`: the caller holds the lock, which
/// is not reentrant.
pub fn read_settings(conn: &Connection) -> UserSettings {
    let get = |key: &str| -> Option<String> {
        conn.query_row(
            "select value from meta where key = ?;",
            params![key],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .unwrap_or(None)
        .flatten()
    };
    // One warning per unreadable value, naming the key, so that a setting
    // silently inheriting is explicable from the log.
    let lenient = |key: &str, parsed: Fallible<_>| match parsed {
        Ok(v) => Some(v),
        Err(e) => {
            log::warn!("Ignoring {key}: {e}");
            None
        }
    };
    let number = |key: &str| -> Option<f64> {
        let raw = get(key)?;
        match raw.trim().parse::<f64>() {
            Ok(n) => Some(n),
            Err(e) => {
                log::warn!("Ignoring {key}: {raw} is not a number ({e})");
                None
            }
        }
    };

    UserSettings {
        retention: number(KEY_RETENTION)
            .and_then(|n| lenient(KEY_RETENTION, DesiredRetention::new(n))),
        max_interval: number(KEY_MAX_INTERVAL)
            .and_then(|n| lenient(KEY_MAX_INTERVAL, MaxInterval::new(n))),
        jitter: number(KEY_JITTER).and_then(|n| lenient(KEY_JITTER, Jitter::new(n))),
        bury_siblings: get(KEY_BURY_SIBLINGS).and_then(|v| match v.trim() {
            "true" => Some(true),
            "false" => Some(false),
            other => {
                log::warn!("Ignoring {KEY_BURY_SIBLINGS}: {other} is not true or false");
                None
            }
        }),
        limits: DailyLimits {
            reviews: get(KEY_MAX_REVIEWS)
                .and_then(|v| lenient(KEY_MAX_REVIEWS, DailyLimits::parse(&v)))
                .flatten(),
            new: get(KEY_MAX_NEW)
                .and_then(|v| lenient(KEY_MAX_NEW, DailyLimits::parse(&v)))
                .flatten(),
        },
        free_days: get(KEY_FREE_DAYS)
            .and_then(|v| lenient(KEY_FREE_DAYS, FreeDays::parse_list(&v))),
    }
}

/// Write every setting, deleting the ones that are `None` so that clearing
/// a field really does return it to the inherited value.
///
/// A free function taking a `&Connection`, for the same reason as above.
/// One transaction, so a half-saved settings page is not a state anyone can
/// observe.
pub fn write_settings(conn: &mut Connection, settings: &UserSettings) -> Fallible<()> {
    let tx = conn.transaction()?;
    let mut put = |key: &str, value: Option<String>| -> Fallible<()> {
        match value {
            Some(v) => {
                tx.execute(
                    "insert into meta (key, value) values (?, ?) \
                     on conflict (key) do update set value = excluded.value;",
                    params![key, v],
                )?;
            }
            None => {
                tx.execute("delete from meta where key = ?;", params![key])?;
            }
        }
        Ok(())
    };
    put(KEY_RETENTION, settings.retention.map(|v| v.into_inner().to_string()))?;
    put(KEY_MAX_INTERVAL, settings.max_interval.map(|v| v.into_inner().to_string()))?;
    put(KEY_JITTER, settings.jitter.map(|v| v.into_inner().to_string()))?;
    put(KEY_BURY_SIBLINGS, settings.bury_siblings.map(|v| v.to_string()))?;
    put(KEY_MAX_REVIEWS, settings.limits.reviews.map(|v| v.to_string()))?;
    put(KEY_MAX_NEW, settings.limits.new.map(|v| v.to_string()))?;
    put(KEY_FREE_DAYS, settings.free_days.map(|v| v.to_list().join(",")))?;
    drop(put);
    tx.commit()?;
    Ok(())
}
```

`Jitter` needs an `into_inner()`; check `src/types/performance.rs` and add
one beside `DesiredRetention::into_inner` if it is missing.

- [ ] **Step 4: Add the `UserDatabase` methods**

In `src/user_db.rs`, beside `collection()`:

```rust
    /// This user's settings, forgiving anything unreadable.
    ///
    /// Takes the lock once and calls a free function under it: the mutex is
    /// not reentrant.
    pub fn user_settings(&self) -> UserSettings {
        let conn = self.conn.lock();
        read_settings(&conn)
    }

    /// Replace this user's settings wholesale, clearing the ones they no
    /// longer set.
    pub fn save_user_settings(&self, settings: &UserSettings) -> Fallible<()> {
        let mut conn = self.conn.lock();
        write_settings(&mut conn, settings)
    }

    /// Plant a raw `meta` value, to test that an unreadable one is
    /// forgiven.
    #[cfg(test)]
    pub fn put_meta_for_test(&self, key: &str, value: &str) -> Fallible<()> {
        let conn = self.conn.lock();
        conn.execute(
            "insert into meta (key, value) values (?, ?) \
             on conflict (key) do update set value = excluded.value;",
            params![key, value],
        )?;
        Ok(())
    }
```

with the matching `use crate::user_settings::...` imports.

- [ ] **Step 5: Declare the module**

In `src/main.rs`, add `mod user_settings;` beside `mod user_db;`.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --lib user_settings`
Expected: PASS, 5 tests.

- [ ] **Step 7: Commit**

```bash
git add src/user_settings.rs src/user_db.rs src/main.rs src/types/performance.rs
git commit -m "feat: per-user settings, stored in the review database's meta table"
```

---

### Task 5: three-layer resolution

**Files:**
- Modify: `src/cmd/serve/config.rs` (`DefaultsSection` at :181,
  `SchedulingOverrides` at :281, `ResolvedCollection::scheduling` at :309)
- Modify: `src/cmd/serve/cards.rs` (`CollectionMeta` at :186,
  `collection_overrides` at :212, `write_collection_overrides` at :253)

**Interfaces:**
- Consumes: `UserSettings`, `DailyLimits`, `FreeDays`.
- Produces: `DefaultsSection` gains `max_reviews_per_day: Option<u32>`,
  `max_new_per_day: Option<u32>`, `free_days: String`;
  `DefaultsSection::scheduling()` fills `free_days`;
  `DefaultsSection::limits() -> DailyLimits`; `SchedulingOverrides` gains
  `limits: DailyLimits`; `ResolvedCollection::scheduling(defaults:
  Scheduling, user: &UserSettings) -> Scheduling`;
  `ResolvedCollection::limits(defaults: DailyLimits, user: &UserSettings)
  -> DailyLimits`.

- [ ] **Step 1: Write the failing tests**

Add to the existing `mod tests` in `src/cmd/serve/config.rs`:

```rust
/// Collection beats user beats instance, field by field.
#[test]
fn the_three_layers_resolve_in_order() -> Fallible<()> {
    let instance = Scheduling {
        retention: DesiredRetention::new(0.90)?,
        max_interval: MaxInterval::new(100.0)?,
        jitter: Jitter::new(0.05)?,
        free_days: FreeDays::none(),
    };
    let user = UserSettings {
        retention: Some(DesiredRetention::new(0.85)?),
        max_interval: Some(MaxInterval::new(200.0)?),
        jitter: Some(Jitter::new(0.2)?),
        free_days: Some(FreeDays::parse_list("sun")?),
        ..UserSettings::default()
    };
    let rc = test_collection(SchedulingOverrides {
        retention: Some(DesiredRetention::new(0.80)?),
        ..SchedulingOverrides::default()
    });

    let resolved = rc.scheduling(instance, &user);
    assert_eq!(resolved.retention, DesiredRetention::new(0.80)?, "collection wins");
    assert_eq!(resolved.max_interval, MaxInterval::new(200.0)?, "user wins");
    assert_eq!(resolved.jitter, Jitter::new(0.2)?, "jitter is the user's");
    assert_eq!(resolved.free_days, FreeDays::parse_list("sun")?);
    Ok(())
}

/// An untouched user layer schedules exactly as the instance does. This is
/// the property that says adding the layer moved nobody's cards.
#[test]
fn an_empty_user_layer_changes_nothing() -> Fallible<()> {
    let instance = Scheduling {
        retention: DesiredRetention::new(0.93)?,
        max_interval: MaxInterval::new(512.0)?,
        jitter: Jitter::new(0.07)?,
        free_days: FreeDays::none(),
    };
    let rc = test_collection(SchedulingOverrides::default());
    assert_eq!(rc.scheduling(instance, &UserSettings::default()), instance);
    Ok(())
}

/// Jitter and free days have no collection layer at all: they exist to
/// spread one person's peaks across every collection they own.
#[test]
fn a_collection_cannot_override_jitter_or_free_days() -> Fallible<()> {
    let instance = Scheduling {
        jitter: Jitter::new(0.01)?,
        free_days: FreeDays::parse_list("sat")?,
        ..Scheduling::default()
    };
    let rc = test_collection(SchedulingOverrides::default());
    let resolved = rc.scheduling(instance, &UserSettings::default());
    assert_eq!(resolved.jitter, Jitter::new(0.01)?);
    assert_eq!(resolved.free_days, FreeDays::parse_list("sat")?);
    Ok(())
}

#[test]
fn limits_resolve_in_the_same_order() -> Fallible<()> {
    let instance = DailyLimits { reviews: Some(100), new: Some(10) };
    let user = UserSettings {
        limits: DailyLimits { reviews: Some(50), new: None },
        ..UserSettings::default()
    };
    let rc = test_collection(SchedulingOverrides {
        limits: DailyLimits { reviews: None, new: Some(3) },
        ..SchedulingOverrides::default()
    });
    let resolved = rc.limits(instance, &user);
    assert_eq!(resolved.reviews, Some(50), "user over instance");
    assert_eq!(resolved.new, Some(3), "collection over both");
    Ok(())
}

/// `free_days` in `[defaults]` is validated at startup rather than
/// forgiven: an administrator is there to read the message.
#[test]
fn a_bad_free_days_list_is_a_configuration_error() {
    let toml = "[server]\ndata_dir = \"/var/lib/hashcards\"\n\n\
                [defaults]\nfree_days = \"caturday\"\n";
    let config: ServeConfig = toml::from_str(toml).expect("parses");
    assert!(config.defaults.scheduling().is_err());
}
```

Write the `test_collection(overrides) -> ResolvedCollection` helper in the
same test module, building a `ResolvedCollection` with placeholder paths
and the given overrides.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib config`
Expected: FAIL to compile — `scheduling` takes one argument.

- [ ] **Step 3: Extend `DefaultsSection`**

Add the fields and their `serde` defaults, following the existing pattern
at `src/cmd/serve/config.rs:181`:

```rust
    #[serde(default)]
    pub max_reviews_per_day: Option<u32>,
    #[serde(default)]
    pub max_new_per_day: Option<u32>,
    /// `"sat,sun"`. Validated in `scheduling()`, not here, so that the
    /// error names the file rather than a serde path.
    #[serde(default)]
    pub free_days: String,
```

`scheduling()` gains `free_days: FreeDays::parse_list(&self.free_days)?`,
and a new method:

```rust
    /// The instance-wide daily limits.
    pub fn limits(&self) -> DailyLimits {
        DailyLimits {
            reviews: self.max_reviews_per_day,
            new: self.max_new_per_day,
        }
    }
```

Update the manual `impl Default for DefaultsSection` at `config.rs:219`
with `max_reviews_per_day: None, max_new_per_day: None, free_days:
String::new(),`.

- [ ] **Step 4: Extend the resolution**

`SchedulingOverrides` at `config.rs:281` gains `pub limits: DailyLimits,`.
Then rewrite `ResolvedCollection::scheduling` at `config.rs:309`, keeping
its existing doc comment about jitter and extending it to free days:

```rust
    pub fn scheduling(&self, defaults: Scheduling, user: &UserSettings) -> Scheduling {
        Scheduling {
            retention: self
                .overrides
                .retention
                .or(user.retention)
                .unwrap_or(defaults.retention),
            max_interval: self
                .overrides
                .max_interval
                .or(user.max_interval)
                .unwrap_or(defaults.max_interval),
            jitter: user.jitter.unwrap_or(defaults.jitter),
            free_days: user.free_days.unwrap_or(defaults.free_days),
        }
    }

    /// How many cards this collection hands out today: its own answer, then
    /// the user's, then the instance's.
    ///
    /// Per collection rather than per user on purpose, as Anki's are per
    /// deck: the user's value is the default each collection inherits, not
    /// a shared pool, so drilling one collection never silently spends
    /// another's allowance.
    pub fn limits(&self, defaults: DailyLimits, user: &UserSettings) -> DailyLimits {
        self.overrides.limits.or(user.limits).or(defaults)
    }
```

- [ ] **Step 5: Extend the collection file**

In `src/cmd/serve/cards.rs`, add to `CollectionMeta` (:186), keeping the
deliberately-untyped `toml::Value` pattern and its comment's reasoning:

```rust
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_reviews_per_day: Option<toml::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_new_per_day: Option<toml::Value>,
```

`collection_overrides` (:212) reads both into `SchedulingOverrides.limits`
through the existing `override_number` helper, forgiving anything
unreadable exactly as it forgives the other two. A negative or fractional
value is not a count of cards and inherits.
`write_collection_overrides` (:253) takes a `DailyLimits` argument and
writes both, keeping its refusal to rewrite a file the parser rejects.

- [ ] **Step 6: Fix every call site**

Run `cargo build` and thread the new arguments through. Expect
`handlers.rs:601`, `cards.rs:748`, `cards.rs:776`, `cards.rs:804`,
`cards.rs:827`, and `mcp/tools/collections.rs`. In the MCP tool pass
`&UserSettings::default()` only where the call is genuinely instance-level;
anywhere it has a user, pass theirs.

- [ ] **Step 7: Run the suite**

Run: `cargo test`
Expected: PASS, including the five new tests.

- [ ] **Step 8: Commit**

```bash
git add src/cmd/serve/config.rs src/cmd/serve/cards.rs src/cmd/serve/handlers.rs src/cmd/serve/mcp
git commit -m "feat: resolve scheduling through instance, user and collection layers"
```

---

### Task 6: today's new-card count, and which cards are new

**Do not add a review counter.** `Database::count_reviews_in_date(date)`
already exists at `src/db.rs:748`, already filters `voided = 0`, and is
exactly the count a review limit needs. Reuse it. This task adds only the
two things that genuinely do not exist.

**Files:**
- Modify: `src/db.rs`

**Interfaces:**
- Consumes: `Date`, `CardHash` (binds as a parameter directly — see
  `count_reviews_in_date`, which passes a `Date` straight into `params!`).
- Produces: `Database::new_cards_today_count(&self, today: Date) ->
  Fallible<usize>`, `Database::new_cards(&self) ->
  Fallible<HashSet<CardHash>>`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/db.rs` (it already imports `UserDatabase`,
`CollectionId`, `Grade`, `ReviewedPerformance`):

```rust
    /// A review row on `day`, written straight in. `insert_review_immediately`
    /// is the test-only writer the surrounding tests use.
    fn review_on(
        db: &Database,
        session_id: i64,
        hash: CardHash,
        day: NaiveDate,
    ) -> Fallible<i64> {
        let at = Timestamp::new(day.and_hms_opt(9, 0, 0).expect("valid time"));
        db.insert_review_immediately(
            session_id,
            &ReviewRecord {
                card_hash: hash,
                reviewed_at: at,
                grade: Grade::Good,
                stability: 2.0,
                difficulty: 5.0,
                interval_raw: 3.0,
                interval_days: 3,
                due_date: Date::new(day),
                duration_ms: None,
            },
        )
    }

    /// A card counts as introduced today when its *first* surviving review
    /// is today, however many times it has been seen since. A card first
    /// seen yesterday is not new today, even though it was reviewed today.
    #[test]
    fn todays_new_count_is_cards_first_seen_today() -> Fallible<()> {
        let user = UserDatabase::memory()?;
        let db = user.collection(CollectionId::new("bio")?);
        let today = NaiveDate::from_ymd_opt(2026, 9, 13).expect("valid date");
        let yesterday = NaiveDate::from_ymd_opt(2026, 9, 12).expect("valid date");
        let session = db.create_session(Timestamp::new(
            today.and_hms_opt(8, 0, 0).expect("valid time"),
        ))?;

        let old = CardHash::hash_bytes(b"seen yesterday");
        let fresh = CardHash::hash_bytes(b"seen today");
        db.insert_card(old, Timestamp::now())?;
        db.insert_card(fresh, Timestamp::now())?;

        review_on(&db, session, old, yesterday)?;
        review_on(&db, session, old, today)?;
        review_on(&db, session, fresh, today)?;
        review_on(&db, session, fresh, today)?;

        assert_eq!(
            db.new_cards_today_count(Date::new(today))?,
            1,
            "only the card first seen today, counted once"
        );
        Ok(())
    }

    /// An undone review is work that did not happen. Voiding the only
    /// review a card has makes it new again, exactly as it makes the
    /// review vanish from every other read path.
    #[test]
    fn a_voided_first_review_does_not_introduce_a_card() -> Fallible<()> {
        let user = UserDatabase::memory()?;
        let db = user.collection(CollectionId::new("bio")?);
        let today = NaiveDate::from_ymd_opt(2026, 9, 13).expect("valid date");
        let session = db.create_session(Timestamp::new(
            today.and_hms_opt(8, 0, 0).expect("valid time"),
        ))?;
        let hash = CardHash::hash_bytes(b"undone");
        db.insert_card(hash, Timestamp::now())?;
        let review_id = review_on(&db, session, hash, today)?;

        assert_eq!(db.new_cards_today_count(Date::new(today))?, 1);
        db.void_review_and_restore_performance(review_id, Performance::New)?;
        assert_eq!(db.new_cards_today_count(Date::new(today))?, 0);
        Ok(())
    }

    /// The set a session filters against: cards with no reviews at all.
    #[test]
    fn new_cards_are_the_ones_never_reviewed() -> Fallible<()> {
        let user = UserDatabase::memory()?;
        let db = user.collection(CollectionId::new("bio")?);
        let untouched = CardHash::hash_bytes(b"never seen");
        let seen = CardHash::hash_bytes(b"seen once");
        let now = Timestamp::now();
        db.insert_card(untouched, now)?;
        db.insert_card(seen, now)?;
        db.update_card_performance(
            seen,
            Performance::Reviewed(ReviewedPerformance {
                last_reviewed_at: now,
                stability: 2.0,
                difficulty: 5.0,
                interval_raw: 3.0,
                interval_days: 3,
                due_date: now.date(),
                review_count: 1,
            }),
        )?;

        let new = db.new_cards()?;
        assert!(new.contains(&untouched));
        assert!(!new.contains(&seen));
        Ok(())
    }
```

Check `void_review_and_restore_performance`'s signature at `src/db.rs:386`
before writing the second test and match it; if it takes the previous
performance by another name or shape, adapt the call rather than the
assertion.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib db::tests`
Expected: FAIL to compile — `new_cards_today_count` and `new_cards` do not
exist.

- [ ] **Step 3: Implement**

Beside `count_reviews_in_date` at `src/db.rs:748`:

```rust
    /// How many cards this collection introduced today: those whose first
    /// surviving review is today, counted once however often they have
    /// been seen since.
    ///
    /// `voided = 0` inside the grouping rather than outside it, so that
    /// undoing a card's only review makes it new again rather than leaving
    /// it introduced by a review that no longer exists.
    pub fn new_cards_today_count(&self, today: Date) -> Fallible<usize> {
        let conn = self.conn.lock();
        let sql = "select count(*) from ( \
                     select card_hash, min(reviewed_at) as first_at from reviews \
                     where collection_id = ? and voided = 0 group by card_hash \
                   ) where substr(first_at, 1, 10) = ?;";
        let count: i64 = conn.query_row(sql, params![self.collection, today], |r| r.get(0))?;
        Ok(count as usize)
    }

    /// The cards in this collection that have never been reviewed.
    ///
    /// A set rather than a count: the session builder needs to know which
    /// card it is holding, not how many there are.
    pub fn new_cards(&self) -> Fallible<HashSet<CardHash>> {
        let conn = self.conn.lock();
        let sql = "select card_hash from cards where collection_id = ? and review_count = 0;";
        let mut stmt = conn.prepare(sql)?;
        let mut out = HashSet::new();
        let mut rows = stmt.query(params![self.collection])?;
        while let Some(row) = rows.next()? {
            out.insert(row.get::<_, CardHash>(0)?);
        }
        Ok(out)
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib db::tests`
Expected: PASS, 3 new tests.

- [ ] **Step 5: Commit**

```bash
git add src/db.rs
git commit -m "feat: count today's new cards, and list the never-reviewed ones"
```

---

### Task 7: apply the budget to sessions and to the counts that describe them

**Files:**
- Modify: `src/cmd/serve/handlers.rs` (`create_session_from_sources` :496,
  `deck_card_counts` :670)
- Modify: `src/cmd/serve/counts.rs` (`Burial::new` :45,
  `refresh_collection_info` :74)

**Interfaces:**
- Consumes: `DailyBudget`, `DailyLimits`, `UserSettings`, the Task 6 counts.
- Produces: `Burial::new(bury_siblings: bool)`; a `load_user_settings(state:
  &AppState, owner: Option<&str>) -> UserSettings` helper.

**The invariant this task exists to protect:** `deck_card_counts` counts
what `create_session_from_sources` will queue, and its own comment says so
— the due count is filtered exactly as the queue is. The existing test
`the_due_count_on_the_page_matches_the_session_it_starts` holds the line.
Every filter added here goes in **both** places.

**Which budget a card spends:** a card that has never been reviewed is
*new*, and spends the new budget; a card seen before spends the review
budget. This matters more than it sounds. A collection nobody has drilled
yet contains nothing but new cards, so `max_reviews_per_day` on it caps
nothing at all — `max_new_per_day` is the control that bites. The test
below therefore limits new cards, because it drills a fresh collection.

- [ ] **Step 1: Write the failing test**

In `src/cmd/serve/handlers.rs` tests, directly beside
`the_due_count_on_the_page_matches_the_session_it_starts` (:1489), reusing
its helpers — it is a plain `#[test]`, not an async one:

```rust
    /// The count a page shows is the size of the session its Drill button
    /// starts — with a daily limit in force exactly as without one. A count
    /// that overstates the session is the bug this arrangement exists to
    /// prevent: "Start (10 due)" leading to "0 of 4".
    #[test]
    fn a_limited_collections_count_matches_the_session_it_starts() -> Fallible<()> {
        use crate::cmd::serve::browse::build_deck_tree;
        use crate::cmd::serve::cards::write_collection_overrides;
        use crate::cmd::serve::handlers::find_collection;
        use crate::types::limits::DailyLimits;

        let dir = tempfile::tempdir()?;
        let data_dir = dir.path().canonicalize()?;
        // Ten plain cards, no cloze families, so burying cannot be what
        // trims the count below.
        let mut markdown = String::new();
        for i in 0..10 {
            markdown.push_str(&format!("Q: Question {i}\nA: Answer {i}\n\n"));
        }
        card_collection(&data_dir, None, "Deck", &markdown)?;
        let state = crate::cmd::serve::state::test_support::state_with_data_dir(data_dir.clone());
        let rc = find_collection(&state, "Deck", None)
            .ok_or_else(|| ErrorReport::new("the collection was not discovered"))?;

        // Every card here is new -- nothing has been reviewed -- so the new
        // limit is the one that bites. A review limit would cap nothing.
        write_collection_overrides(
            &rc.coll_dir,
            None,
            None,
            DailyLimits {
                reviews: None,
                new: Some(4),
            },
        )?;
        // Re-resolve so the collection carries the overrides just written.
        let rc = find_collection(&state, "Deck", None)
            .ok_or_else(|| ErrorReport::new("the collection was not discovered"))?;

        let db = open_collection_db(&state, &rc)?;
        crate::cmd::serve::counts::compute_collection_counts(
            &rc.coll_dir,
            db,
            &state.config.defaults,
        )?;

        let browse = build_deck_tree(
            &rc.coll_dir,
            open_collection_db(&state, &rc)?,
            &state.config.defaults,
        )?;
        let shown = browse.tree.due_today_recursive();
        assert_eq!(shown, 4, "the page counts what the cap allows");

        let session = create_session_from_sources(
            &state,
            vec![SessionSourceSpec {
                collection: rc,
                decks: Vec::new(),
            }],
            None,
        )?
        .ok_or_else(|| ErrorReport::new("the session held no cards"))?;

        assert_eq!(
            shown, session.total_cards,
            "the page promised {shown} cards and the session holds {}",
            session.total_cards
        );
        Ok(())
    }
```

`build_deck_tree` and `compute_collection_counts` take
`&state.config.defaults` today. If applying the budget means they need the
resolved settings instead, change their signatures in step 5 and update
this test with them — but keep the assertion identical.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib a_limited_collections_count_matches`
Expected: FAIL — the count and the session both ignore limits, reporting 10.

- [ ] **Step 3: Load the user's settings once per request**

Add to `src/cmd/serve/reviewdb.rs`:

```rust
/// The settings of the user who owns this database, or the inherit-
/// everything default if their database cannot be opened.
///
/// Never an error: a settings layer that cannot be read must not take down
/// the page it decorates, and the layer below it is a complete answer.
pub fn user_settings_for(state: &AppState, db_path: &Path) -> UserSettings {
    match UserDatabase::open(db_path) {
        Ok(db) => db.user_settings(),
        Err(e) => {
            log::warn!("Could not read settings from {}: {e}", db_path.display());
            UserSettings::default()
        }
    }
}
```

In `create_session_from_sources` and `deck_card_counts`, the user database
is already open per path — call `user_db.user_settings()` there instead of
reopening, and keep it beside the `UserDatabase` in the `opened` map.

- [ ] **Step 4: Apply the budget in the session builder**

In `create_session_from_sources`, the per-source loop currently appends
straight into `due_cards`. Collect each source's cards into a local `Vec`
first, then:

```rust
        let limits = rc.limits(default_limits, &user_settings);
        let mut budget = DailyBudget::new(
            limits,
            // The counter that already existed (`db.rs:748`), not a new one.
            collection.db.count_reviews_in_date(today)?,
            collection.db.new_cards_today_count(today)?,
        );
        let new_cards = collection.db.new_cards()?;
        source_cards.retain(|card| budget.admits(new_cards.contains(&card.hash())));
        due_cards.extend(source_cards);
```

Use whatever the card's hash accessor is called in this codebase. The
budget is per source, so a deck spanning two collections spends each
collection's allowance separately — which is what "limits are per
collection" means.

- [ ] **Step 5: Apply the same budget in the counter**

Make the same change in `deck_card_counts`, in the same order relative to
`Burial`. Extract the shared part into one function taking the cards, the
budget and the burial, and call it from both, so the two can no longer
drift.

- [ ] **Step 6: Move `bury_siblings` to the user layer**

`Burial::new` takes a `bool` rather than `&DefaultsSection`. Every caller
passes `user.bury_siblings.unwrap_or(state.config.defaults.bury_siblings)`.

- [ ] **Step 7: Run the suite**

Run: `cargo test`
Expected: PASS, including
`the_due_count_on_the_page_matches_the_session_it_starts` unchanged.

- [ ] **Step 8: Commit**

```bash
git add src/cmd/serve/handlers.rs src/cmd/serve/counts.rs src/cmd/serve/reviewdb.rs
git commit -m "feat: daily limits cap the session and the count that describes it"
```

---

### Task 8: the `/settings` page

**Files:**
- Create: `src/cmd/serve/settings.rs`
- Modify: `src/cmd/serve/mod.rs`, `src/cmd/serve/server.rs`

**Interfaces:**
- Consumes: `UserSettings`, `FreeDays`, `DailyLimits`.
- Produces: `settings_get_handler`, `settings_post_handler`.

**Pattern to follow:** `src/cmd/serve/tokens.rs` is the closest existing
page — same auth shape, same `run_blocking` wrapper, same
`page_template` + `maud` rendering, same flash handling. Read it first.

- [ ] **Step 1: Write the failing tests**

```rust
/// Every control the spec promises is on the page, and each shows the
/// value in force rather than an empty box.
#[test]
fn the_page_shows_the_settings_in_force() -> Fallible<()> {
    let settings = UserSettings {
        retention: Some(DesiredRetention::new(0.85)?),
        free_days: Some(FreeDays::parse_list("sat")?),
        limits: DailyLimits { reviews: Some(40), new: None },
        ..UserSettings::default()
    };
    let html = render_settings(&settings, Scheduling::default(), DailyLimits::default(), None)
        .into_string();
    assert!(html.contains("name=\"desired_retention\""));
    assert!(html.contains("name=\"max_interval_days\""));
    assert!(html.contains("name=\"jitter\""));
    assert!(html.contains("name=\"max_reviews_per_day\""));
    assert!(html.contains("name=\"max_new_per_day\""));
    assert!(html.contains("name=\"free_sat\""));
    assert!(html.contains("0.85"));
    assert!(html.contains("40"));
    Ok(())
}

/// A blank limit and a zero limit are different answers, and the form must
/// carry the difference intact.
#[test]
fn blank_and_zero_limits_survive_the_form() -> Fallible<()> {
    let blank = settings_from_form(&SettingsForm {
        max_new_per_day: String::new(),
        ..SettingsForm::empty()
    })?;
    assert_eq!(blank.limits.new, None);
    let zero = settings_from_form(&SettingsForm {
        max_new_per_day: "0".to_string(),
        ..SettingsForm::empty()
    })?;
    assert_eq!(zero.limits.new, Some(0));
    Ok(())
}

/// A value out of range is refused with a message naming the range,
/// because a form has an author who can be told.
#[test]
fn an_out_of_range_value_is_refused_with_a_message() {
    let err = settings_from_form(&SettingsForm {
        desired_retention: "2.0".to_string(),
        ..SettingsForm::empty()
    })
    .expect_err("2.0 is out of range");
    assert!(err.to_string().contains("0.7"), "message was: {err}");
}

/// All seven days ticked is refused rather than saved: a week with no
/// open day has no due date to offer.
#[test]
fn a_week_with_no_open_day_is_refused() {
    let ticked = || Some("on".to_string());
    let form = SettingsForm {
        free_mon: ticked(),
        free_tue: ticked(),
        free_wed: ticked(),
        free_thu: ticked(),
        free_fri: ticked(),
        free_sat: ticked(),
        free_sun: ticked(),
        ..SettingsForm::default()
    };
    let err = settings_from_form(&form).expect_err("seven free days is not a week");
    assert!(err.to_string().contains("open"), "message was: {err}");
}
```

Everywhere above, `SettingsForm::empty()` is `SettingsForm::default()` —
the struct derives `Default`, and every field's default is the blank string
or `None` that means "inherit".

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib settings`
Expected: FAIL to compile.

- [ ] **Step 3: Implement the form type and its conversion**

```rust
/// Every control on the page, as the browser submits it. Strings
/// throughout: the parse and its error message belong to
/// `settings_from_form`, which can say what the range is, not to serde,
/// which cannot.
///
/// A checkbox absent from the submission is an unticked box, so each is
/// `Option<String>` with `None` meaning not free.
#[derive(Deserialize, Default)]
pub struct SettingsForm {
    #[serde(default)]
    pub desired_retention: String,
    #[serde(default)]
    pub max_interval_days: String,
    #[serde(default)]
    pub jitter: String,
    #[serde(default)]
    pub bury_siblings: Option<String>,
    #[serde(default)]
    pub max_reviews_per_day: String,
    #[serde(default)]
    pub max_new_per_day: String,
    #[serde(default)]
    pub free_mon: Option<String>,
    #[serde(default)]
    pub free_tue: Option<String>,
    #[serde(default)]
    pub free_wed: Option<String>,
    #[serde(default)]
    pub free_thu: Option<String>,
    #[serde(default)]
    pub free_fri: Option<String>,
    #[serde(default)]
    pub free_sat: Option<String>,
    #[serde(default)]
    pub free_sun: Option<String>,
}

/// A blank box is "inherit"; anything else is validated by the newtype
/// that owns the range. Those error messages already name their bounds and
/// are written to be read, so they are returned as they are rather than
/// wrapped in a second sentence.
fn optional_number<T>(
    raw: &str,
    what: &str,
    make: impl Fn(f64) -> Fallible<T>,
) -> Fallible<Option<T>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    match raw.parse::<f64>() {
        Ok(n) => make(n).map(Some),
        Err(_) => fail(format!("{what} must be a number, got: {raw}")),
    }
}

fn settings_from_form(form: &SettingsForm) -> Fallible<UserSettings> {
    let free = |ticked: &Option<String>| ticked.is_some();
    let free_days = [
        free(&form.free_mon),
        free(&form.free_tue),
        free(&form.free_wed),
        free(&form.free_thu),
        free(&form.free_fri),
        free(&form.free_sat),
        free(&form.free_sun),
    ];
    Ok(UserSettings {
        retention: optional_number(
            &form.desired_retention,
            "desired retention",
            DesiredRetention::new,
        )?,
        max_interval: optional_number(
            &form.max_interval_days,
            "the maximum interval",
            MaxInterval::new,
        )?,
        jitter: optional_number(&form.jitter, "interval jitter", Jitter::new)?,
        bury_siblings: Some(form.bury_siblings.is_some()),
        limits: DailyLimits {
            reviews: DailyLimits::parse(&form.max_reviews_per_day)?,
            new: DailyLimits::parse(&form.max_new_per_day)?,
        },
        // No day ticked is no free days, which is a real answer and not an
        // absent one: unticking the last box must clear the setting rather
        // than leave the instance's free days in force.
        free_days: Some(FreeDays::new(free_days)?),
    })
}
```

Note what `bury_siblings` and `free_days` have in common above: a checkbox
absent from a submission is an unticked box, not a missing answer, so
neither can ever be `None` from this form. The `Option` in `UserSettings`
exists for settings never touched at all — a user who has not opened this
page — not for a box left unticked.

- [ ] **Step 4: Implement rendering**

`render_settings(user: &UserSettings, inherited: Scheduling, inherited_limits: DailyLimits, flash: Option<Flash>) -> Markup`, four blocks per the spec. Each slider is an `<input type="range">` with a `<output>` showing its value, and each carries the consequence line the spec gives, e.g.:

```rust
    p.hint {
        "The chance a card is still remembered when it comes back. At 0.95, \
         a card you would have seen in 30 days comes back in about 18."
    }
```

Where a field is blank, the label says what it inherits: *"inheriting 0.90
from the server."* The readout is wired by the existing `script.js` with a
small `input` listener — add it there rather than inlining a `<script>`.

- [ ] **Step 5: Implement the handlers**

Copy the shape of `tokens_get_handler` and `tokens_mint_handler` exactly.
Opening the database:

```rust
/// The caller's review database. On GET a missing file is *not* created:
/// serving a page must not materialize a database for someone who has
/// never drilled. They see the inherited values, which is the truth.
fn open_settings_db(state: &AppState, owner: Option<&str>, create: bool)
    -> Fallible<Option<UserDatabase>>
```

using `CardRoot::open`, `user_db_path`, then `refuse_if_unconsolidated`
before `UserDatabase::open` — the same gate every other write path takes,
and `CLAUDE.md` is explicit that everything opening a review database goes
through `reviewdb.rs`.

POST validates, saves, and re-renders with a success flash; on error it
re-renders with the submitted values still in the boxes and an error flash,
so nothing typed is lost.

- [ ] **Step 6: Route it**

`mod settings;` in `src/cmd/serve/mod.rs`; in `server.rs`, beside the
`/tokens` routes:

```rust
        .route("/settings", get(settings_get_handler))
        .route("/settings", post(settings_post_handler))
```

Confirm it sits inside the same `require_auth` layer the other per-user
pages use.

- [ ] **Step 7: Run the suite**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src/cmd/serve/settings.rs src/cmd/serve/mod.rs src/cmd/serve/server.rs src/cmd/drill/static
git commit -m "feat: a settings page"
```

---

### Task 9: the per-collection settings page

**Files:**
- Create: `src/cmd/serve/collection_settings.rs`
- Modify: `src/cmd/serve/mod.rs`, `src/cmd/serve/server.rs`,
  `src/cmd/serve/handlers.rs` (link from the collection page)

**Interfaces:**
- Consumes: `SchedulingOverrides`, `write_collection_overrides`,
  `find_drill_target`.
- Produces: `collection_settings_get_handler`,
  `collection_settings_post_handler`.

- [ ] **Step 1: Write the failing tests**

```rust
/// A field the collection does not override shows what it inherits, so the
/// page never implies a collection has an opinion it does not have.
#[test]
fn inherited_values_are_shown_as_inherited() -> Fallible<()> {
    let html = render_collection_settings(
        "Spanish",
        SchedulingOverrides::default(),
        Scheduling::default(),
        DailyLimits::default(),
        None,
    )
    .into_string();
    assert!(html.contains("inheriting"));
    Ok(())
}

/// Jitter and free days have no collection layer, so the page must not
/// offer them here.
#[test]
fn the_collection_page_offers_no_jitter_or_free_days() -> Fallible<()> {
    let html = render_collection_settings(
        "Spanish",
        SchedulingOverrides::default(),
        Scheduling::default(),
        DailyLimits::default(),
        None,
    )
    .into_string();
    assert!(!html.contains("name=\"jitter\""));
    assert!(!html.contains("name=\"free_sat\""));
    Ok(())
}

/// Clearing a field removes the override rather than freezing the value
/// it happened to be inheriting into the file. Otherwise raising the
/// server's default would silently skip every collection ever edited.
#[test]
fn clearing_a_field_removes_the_override() -> Fallible<()> {
    let dir = tempfile::tempdir()?;
    let folder = dir.path().join("Spanish");
    std::fs::create_dir(&folder)?;
    write_collection_overrides(
        &folder,
        Some(DesiredRetention::new(0.85)?),
        Some(MaxInterval::new(365.0)?),
        DailyLimits::default(),
    )?;
    assert!(collection_overrides(&folder).retention.is_some());

    // The form as submitted with the retention box emptied.
    let form = CollectionSettingsForm {
        desired_retention: String::new(),
        max_interval_days: "365".to_string(),
        max_reviews_per_day: String::new(),
        max_new_per_day: String::new(),
    };
    apply_collection_form(&folder, &form)?;

    let after = collection_overrides(&folder);
    assert_eq!(after.retention, None, "the override is gone, not frozen");
    assert_eq!(
        after.max_interval,
        Some(MaxInterval::new(365.0)?),
        "its neighbour is untouched"
    );
    Ok(())
}

/// This page is a new way to reach `write_collection_overrides`, not a way
/// around it. Its refusal to rewrite a file the parser rejects is
/// load-bearing: a file salvaged by eye would be rewritten as
/// id-plus-settings over whatever the user was in the middle of.
#[test]
fn a_broken_collection_file_is_refused_not_rewritten() -> Fallible<()> {
    let dir = tempfile::tempdir()?;
    let folder = dir.path().join("Spanish");
    std::fs::create_dir(&folder)?;
    let meta = folder.join(COLLECTION_META_FILE);
    let broken = "id = \"abc\"\ndesired retention = 0.95\n";
    std::fs::write(&meta, broken)?;

    let form = CollectionSettingsForm {
        desired_retention: "0.85".to_string(),
        max_interval_days: String::new(),
        max_reviews_per_day: String::new(),
        max_new_per_day: String::new(),
    };
    let err = apply_collection_form(&folder, &form).expect_err("a broken file is refused");
    assert!(err.to_string().contains(COLLECTION_META_FILE), "message was: {err}");
    assert_eq!(
        std::fs::read_to_string(&meta)?,
        broken,
        "the file the user was editing is left exactly as it was"
    );
    Ok(())
}
```

`apply_collection_form(folder, form) -> Fallible<()>` is this module's own
parse-then-write function, so both tests exercise the real path rather
than a handler. `collection_overrides`, `write_collection_overrides` and
`COLLECTION_META_FILE` come from `src/cmd/serve/cards.rs`; check their
visibility and widen it to `pub(super)` if needed.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib collection_settings`
Expected: FAIL to compile.

- [ ] **Step 3: Implement**

Four fields only — retention, max interval, and the two limits. Each is a
blank-means-inherit text or range input, exactly as on `/settings`. The
POST calls `write_collection_overrides`, whose refusal on an unparseable
file is surfaced verbatim: its message already tells the user what to do.

Ownership is checked the way `collection_get_handler` checks it
(`handlers.rs:85`): a slug that is not the caller's 404s before anything
else happens.

- [ ] **Step 4: Route and link it**

```rust
        .route("/collection/{slug}/settings", get(collection_settings_get_handler))
        .route("/collection/{slug}/settings", post(collection_settings_post_handler))
```

Add a link on the collection page beside the existing Stats and Export
links.

- [ ] **Step 5: Run the suite**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/cmd/serve/collection_settings.rs src/cmd/serve/mod.rs src/cmd/serve/server.rs src/cmd/serve/handlers.rs
git commit -m "feat: per-collection scheduling settings page"
```

---

### Task 10: the way in, and the backlog it must not hide

**Files:**
- Modify: `src/cmd/serve/landing.rs` (nav at :261, `row_meta` at :199)
- Modify: `CHANGELOG.xml`

- [ ] **Step 1: Write the failing tests**

In `src/cmd/serve/landing.rs` tests, modelled on
`the_landing_page_links_to_the_tokens_page` at :338:

```rust
    /// A page nobody can reach is a page nobody has -- the lesson the
    /// tokens page taught, whose whole first release shipped with nothing
    /// linking to it. The nav link is this feature's only entrance.
    #[test]
    fn the_landing_page_links_to_the_settings_page() {
        let status = LandingStatus {
            config_available: true,
            tokens_available: true,
            signed_in_as: None,
        };
        let html = render_landing_page(&[], &HashMap::new(), &status, None).into_string();
        assert!(
            html.contains("href=\"/settings\""),
            "the landing page must link to the settings page: {html}"
        );
    }

    /// A daily limit must never make a backlog look like a finished day.
    /// The row says what it will hand out *and* what is really waiting, so
    /// a pile growing behind a cap stays visible.
    #[test]
    fn a_capped_row_says_it_was_capped() {
        let counts = Some(RowCounts {
            due_today: 40,
            due_uncapped: 312,
            total_cards: 900,
        });
        let html = row_meta(&counts).into_string();
        assert!(html.contains("40"), "the session size: {html}");
        assert!(html.contains("312"), "the real backlog stays visible: {html}");
        assert!(html.contains("daily cap"), "and says why: {html}");
    }

    /// With no cap in force the row reads exactly as it always has, rather
    /// than growing a redundant "40 of 40".
    #[test]
    fn an_uncapped_row_is_unchanged() {
        let counts = Some(RowCounts {
            due_today: 36,
            due_uncapped: 36,
            total_cards: 900,
        });
        let html = row_meta(&counts).into_string();
        assert!(html.contains("36 due"), "{html}");
        assert!(!html.contains("daily cap"), "{html}");
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib landing`
Expected: FAIL.

- [ ] **Step 3: Implement**

Add the nav link beside the tokens one at `landing.rs:261`. `RowCounts`
gains the uncapped due total alongside the capped one, and `row_meta`
(:199) renders "40 of 312 due — limited by your daily cap" when they
differ.

- [ ] **Step 4: Update `CHANGELOG.xml`**

Follow the existing entry format in that file. One entry covering: a
settings page; per-user scheduling settings; daily review and new-card
limits; free weekdays.

- [ ] **Step 5: Run the full suite and the linter**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: all three clean.

- [ ] **Step 6: Verify by hand**

Run the server against a scratch `hashcards.toml`, then: open `/settings`,
set retention to 0.85, tick Saturday and Sunday, set a review limit of 5,
save. Reload and confirm the values stuck. Drill a collection with more
than five cards due and confirm the session stops at five and the landing
row says so. Grade a card and confirm its due date is not a weekend.

- [ ] **Step 7: Commit**

```bash
git add src/cmd/serve/landing.rs CHANGELOG.xml
git commit -m "feat: link the settings page, and show when a daily cap trimmed a row"
```

---

## Self-Review

**Spec coverage:** §1 user layer → Tasks 4, 5. §2 storage → Task 4. §3 page
→ Tasks 8, 9, 10. §4 free days → Tasks 1, 3. §5 limits → Tasks 2, 6, 7.
§6 weights, §7 optimizer → **deliberately not in this plan** (Parts B and
C). §8 config → Task 5; §8's MCP extension is Part B's, since the MCP
setter changes again there — noted below. §9 testing → distributed. §10
plans → this is plan A.

**Known gap, deliberate:** the spec's §8 says MCP's `set_scheduling_for`
gains the new fields. Task 5 step 6 only keeps it compiling. Extending the
MCP surface is folded into Part B, so that the tool's signature changes
once rather than twice.

**Type consistency checked:** `FreeDays::parse_list`/`to_list` (Tasks 1, 4,
5, 8); `DailyLimits { reviews, new }` and `DailyLimits::or` (Tasks 2, 5);
`DailyBudget::new(limits, reviews_done, new_done)` / `admits(is_new)`
(Tasks 2, 7); `UserSettings` field names (Tasks 4, 5, 7, 8);
`scheduling(defaults, user)` and `limits(defaults, user)` (Tasks 5, 7);
`reviews_today_count` / `new_cards_today_count` / `new_cards` (Tasks 6, 7).
