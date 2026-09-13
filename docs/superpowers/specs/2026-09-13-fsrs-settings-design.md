# Settings, FSRS Weights and Easy Days — Design

Status: draft 2026-09-13.

**Scope:** give the browser a settings surface. Today the schedule is
configurable only by editing `hashcards.toml`, hand-editing a collection's
`.hashcards.toml`, or asking a model over MCP. A person sitting in front of
the web interface cannot change anything about how their cards are scheduled.

Three things, in dependency order:

- **(A)** a settings page, a per-user settings layer to back it, daily
  review limits, and free weekdays.
- **(B)** the 19 FSRS weights as data rather than a constant, and an editor
  for them.
- **(C)** an optimizer that fits those weights to the review log.

B and C are independent of each other. Both need A. Each gets its own
implementation plan.

## What is already there

More than it looks like. `Scheduling` (`src/types/performance.rs:119`) is
already the one value that carries everything shaping an interval besides
the card's own history, and it already travels to the scheduler:

```
[defaults] in hashcards.toml   DefaultsSection::scheduling()   config.rs:200
        ↓
ResolvedCollection::scheduling(defaults)                       config.rs:309
        ↓
SessionDb { scheduling }                                       handlers.rs:601
        ↓
update_performance(perf, grade, at, scheduling, rng)           performance.rs:199
```

Each knob is a validated newtype — `DesiredRetention` (0.7–0.99),
`MaxInterval` (1–36500 days), `Jitter` — and a collection may override the
first two through `collection_overrides` (`src/cmd/serve/cards.rs:212`),
which MCP's `set_scheduling_for` (`src/cmd/serve/mcp/tools/collections.rs:73`)
writes.

So the plumbing exists and is sound. What is missing is a *user* layer, a
*page*, and the settings themselves.

Two things are conspicuously absent, and both will matter below: the 19 FSRS
weights are `pub const W: [f64; 19]` (`src/fsrs.rs:26`), reachable only by
recompiling; and nothing anywhere knows what a weekday is.

## 1. The user layer

Settings resolve in three layers: **instance `[defaults]` → user →
collection**. `ResolvedCollection::scheduling` is already the single
resolution point and stays so, gaining the user's settings as a second
argument.

Which layers a setting appears at is not uniform, and should not be. It
follows from what the setting is *about*:

| Setting | Instance | User | Collection |
|---|---|---|---|
| `desired_retention` | ✓ | ✓ | ✓ |
| `max_interval_days` | ✓ | ✓ | ✓ |
| `jitter` | ✓ | ✓ | — |
| `bury_siblings` | ✓ | ✓ | — |
| `max_reviews_per_day` | ✓ | ✓ | ✓ |
| `max_new_per_day` | ✓ | ✓ | ✓ |
| `free_days` | ✓ | ✓ | — |
| FSRS weights | ✓ | ✓ | ✓ |

Jitter and free days are deliberately not per-collection, and the existing
comment at `config.rs:306` already makes the argument for jitter: it exists
to spread *one person's* review peaks across all their collections, so a
collection deciding it alone decides nothing. Free days are the same kind of
fact. Which days you are busy is a statement about your week, not about
Spanish.

Concretely, `SchedulingOverrides` grows the per-collection additions, and a
new `UserSettings` carries the user layer:

```rust
pub struct UserSettings {
    pub retention: Option<DesiredRetention>,
    pub max_interval: Option<MaxInterval>,
    pub jitter: Option<Jitter>,
    pub bury_siblings: Option<bool>,
    pub limits: DailyLimits,          // both fields Option
    pub free_days: Option<FreeDays>,
    pub weights: Option<Weights>,
}
```

Every field optional, `None` meaning "inherit". A user who has never opened
the settings page has an all-`None` value and is scheduled exactly as they
are scheduled today. That is the property the tests should assert first.

## 2. Storage

The user layer lives in the `meta` key/value table of the user's review
database, `{data_dir}/db/{tree}.db`. Migration 6 (`src/db.rs:1095`) already
created that table for "schema-adjacent settings", which is what these are.
No migration is needed.

One row per setting, keyed `setting.retention`, `setting.free_days` and so
on; the 19 weights as a JSON array under `setting.weights`. One row per
setting rather than one blob so that a single corrupt value cannot cost the
others.

Two constraints from `CLAUDE.md` shape the API.

The connection mutex is not reentrant, so `UserDatabase` gets exactly one
method — `user_settings()` — that takes the lock once and calls free
functions under it. Not seven accessors that each lock.

And reads are **lenient**, in precisely the way `collection_overrides` is
lenient, for the reason its doc comment gives: a number out of range, of the
wrong type, or unparseable yields no override and a `log::warn!`, never an
error. Here the stakes are lower than there — a bad value cannot cost you a
collection — but the principle carries: a preference you mistyped should
cost you that preference and nothing else. Writes, by contrast, validate and
refuse, because a write comes from a form whose author can be told what went
wrong.

## 3. The settings page

`GET`/`POST /settings`, authenticated, per-user. Linked from the landing
page beside the existing tokens link (`src/cmd/serve/landing.rs`); the
`the_landing_page_links_to_the_tokens_page` test there is the pattern for
its own test.

Four blocks, using `page_template` and `maud` like every other page:

**Scheduling.** Retention, max interval and jitter as `<input type="range">`
with a live numeric readout. Native inputs and the single existing
`script.js`; nothing is added to the front end. Each slider is labelled with
its consequence rather than its name alone:

> Desired retention — 0.90
> *The chance a card is still remembered when it comes back. At 0.95, a card
> you would have seen in 30 days comes back in about 18.*

This is the one FSRS number a person genuinely has an opinion about, and the
existing doc comment on `DesiredRetention` says so. The page should say it
too.

**Daily limits.** Two numbers. An empty field means *no limit*, which is
today's behaviour; `0` means *zero cards of that kind today*, which is a
thing someone might genuinely want for new cards. These are different
values and the form must not collapse them, so absent is `None` and `0` is
`Some(0)`.

**Your week.** Seven checkboxes: *cards never come due on these days.*

**FSRS weights** (part B). The 19 values in a table, grouped by what they
govern — W0–3 initial stability, W4–5 initial difficulty, W6–7 difficulty
updates, W8–16 stability updates, W17–18 short-term — each editable, plus
paste-a-whole-vector, reset-to-default, and the optimizer's button.

Per-collection overrides get a matching `GET`/`POST
/collection/{slug}/settings`, showing inherited values greyed out with a
per-field override control, and reusing `write_collection_overrides`
extended to the new fields. That function's existing contract is
load-bearing and must not be relaxed: it refuses to rewrite a
`.hashcards.toml` the parser rejects, because a file salvaged by eye would
be rewritten as id-plus-settings over whatever the user was in the middle
of.

## 4. Free days

The shift happens in `update_performance`, immediately after the due date is
computed (`performance.rs:238`). If that weekday is free, move to the
**nearest** non-free weekday, ties resolving **later** — landing early means
reviewing a card before the scheduler wanted it, which is the more harmful
of the two errors.

Details that matter:

- The search is bounded by construction: with at least one non-free day, the
  nearest one is within three days.
- **`interval_days` is recomputed from the shifted date**, so it and
  `due_date` never disagree. `interval_raw` stays unshifted. This follows
  the precedent the jitter comment sets at `performance.rs:229`: the raw
  interval is the un-massaged truth, kept so that changing the massaging
  later does not have to recover information thrown away.
- The cap wins over the tie-break. If pushing later would exceed
  `max_interval`, pull earlier instead.
- All seven days free is rejected at construction, not silently ignored.
- Naive dates throughout, per `CLAUDE.md`: `NaiveDate::weekday()`, and no
  timezone enters anywhere.

New newtype:

```rust
/// The weekdays on which no card is scheduled to come due.
pub struct FreeDays(u8);   // bitmask, Mon = bit 0

impl FreeDays {
    pub fn new(days: [bool; 7]) -> Fallible<FreeDays>;  // refuses all seven
    pub fn is_free(&self, day: Weekday) -> bool;
    pub fn shift(&self, date: NaiveDate, cap: NaiveDate) -> NaiveDate;
}
```

Note what this does *not* do. The original request was for heavier and
lighter days; free days are binary. "Saturdays are half as heavy as Mondays"
would be a proportional weight per weekday, and it is a later change to
`shift` alone — which is the reason to put the whole decision behind one
function now.

## 5. Daily limits

Limits apply **per collection**, as Anki's do per deck. The per-user value
is the default each collection inherits, not a shared pool. This avoids the
confusing case where drilling Spanish silently consumes your German
allowance, and it keeps the counting query the per-collection one that
already exists.

Definitions:

- `max_reviews_per_day` — count today's non-voided reviews for the
  collection, subtract, truncate the queue. Today's count comes from
  `reviews` filtered on the generated `reviewed_date` column, for which
  `idx_reviews_reviewed_date` already exists.
- `max_new_per_day` — "new" is `review_count = 0` in `cards`. Counted and
  capped separately from reviews, since the two limits answer different
  questions.

Where the code goes is determined by an existing invariant.
`create_session_from_sources` (`handlers.rs:496`) builds the queue and
`deck_card_counts` (`handlers.rs:670`) counts what that queue *will*
contain, and its comment states the contract outright: the due count is
filtered exactly as the queue is. There is a test holding the line,
`the_due_count_on_the_page_matches_the_session_it_starts`. So the limit
filter must be one function applied in both places, exactly as `Burial`
already is — and `Burial::new(&state.config.defaults)` will now need the
resolved user settings rather than the instance defaults, since
`bury_siblings` joins the user layer.

A limit must never *hide* a backlog. The landing row and the session header
read "40 of 312 due — limited by your daily cap", so that a growing backlog
stays visible rather than looking like a finished day.

## 6. Weights as data

Part B's prerequisite. `pub const W: [f64; 19]` becomes:

```rust
pub struct Weights([f64; 19]);

impl Weights {
    pub const DEFAULT: [f64; 19] = [ /* the current W, unchanged */ ];
    pub fn new(w: [f64; 19]) -> Fallible<Weights>;   // finite, per-index bounds
    pub fn get(&self, i: usize) -> f64;
}
```

Every function in `fsrs.rs` takes `&Weights`; `Scheduling` carries one, so
it reaches the scheduler by the route every other knob already takes.

The refactor is mechanical and its safety net is the existing test suite.
The tests in `fsrs.rs` — `test_3e`, `test_3g`, `test_2h`, `test_2f`,
`test_gf` — pass `Weights::default()` and must produce their current
expected values **unchanged**. If `test_3e` still expects `s: 15.69`, then
nobody's schedule moved, which is the only thing this step must prove.

Per-index bounds come from FSRS's own clamps rather than being invented
here. They exist because the formulas are not total: a negative `W[9]`
inverts the stability exponent, and the difficulty update divides by nine
only because difficulty is bounded to 1–10.

## 7. The optimizer

Part C. The review log already supports it, which is the fortunate part:
`reviews` carries `reviewed_at`, `grade` and the post-review state per card,
never hard-deletes (undo sets `voided`), and has `idx_reviews_card`.
Reconstructing what a fit needs is a query, not a schema change.

**Input.** Per `(collection_id, card_hash)`, the non-voided reviews in
order, reduced to a sequence of `(delta_t in days, grade)`. `delta_t` comes
from consecutive `reviewed_at` values, so it is the *real* elapsed time —
which means a free-day shift or a late review is accounted for correctly and
the calendar never contaminates the fit.

**Loss.** Binary log-loss of predicted retrievability against observed
recall (`Forgot` → 0, anything else → 1), replaying the FSRS recurrence
forward under candidate weights. This is the standard FSRS objective.

**Fit.** Adam over the 19 parameters with **central-difference numeric
gradients**. Analytic gradients through the full recurrence are a meaningful
quantity of error-prone calculus; numeric costs 38 replays per step, which
for a few thousand reviews and a bounded step count is cheap and obviously
correct. Analytic is a later optimization, if profiling ever asks.

**Guardrails**, all of which are part of the feature rather than polish:

- Refuse below 400 reviews. A fit on twelve reviews is noise wearing a
  number's clothes.
- Clamp every weight to its bounds each step, so no iterate leaves the
  region where the formulas are defined.
- Report loss before and after, and **refuse to save a fit that did not
  improve on the weights currently in force**.
- Results are *proposed*, never applied. The page shows the numbers and the
  improvement; Apply is a separate click.

**Where it runs.** Synchronously under the existing `run_blocking`, with a
hard iteration budget sized to stay interactive. Not a job queue — there is
no infrastructure for one, and adding it before a real review log has been
measured is speculation.

**Target.** A selector: the whole tree, or one collection.

## 8. Config and MCP

`[defaults]` gains the new keys, so an instance can ship a house style. Each
keeps `DefaultsSection`'s existing pattern of a `#[serde(default = "...")]`
function and validation in `scheduling()`:

```toml
[defaults]
desired_retention = 0.9
max_interval_days = 256
max_reviews_per_day = 200      # omit for no limit
max_new_per_day = 20
free_days = ["sat", "sun"]     # lowercase three-letter, Mon..Sun
weights = [0.40255, 1.18385, 3.173, 15.69105, 7.1949, 0.5345, 1.4604,
           0.0046, 1.54575, 0.1192, 1.01925, 1.9395, 0.11, 0.29605,
           2.2698, 0.2315, 2.9898, 0.51655, 0.6621]
```

`weights` must be exactly 19 numbers; a shorter or longer array is a
configuration error at startup, not a silent pad. Unlike the per-collection
and per-user layers, the instance layer refuses rather than warns, because
`[defaults]` is already validated at startup (`config.rs:272`) and an
administrator is there to read the message.

MCP **extends `set_scheduling_for`** and adds a getter. It does not grow an
optimizer tool. `CLAUDE.md` is explicit that the tools are adapters over the
same functions the web handlers call — never a reimplementation — and the
existing test that the MCP has no purge tool is the precedent for asserting
the absence.

`CHANGELOG.xml` gets an entry per part.

## 9. Testing

TDD throughout, per `CLAUDE.md`. The tests that carry real risk:

- **Layering.** An all-`None` user layer schedules identically to today.
  Collection beats user beats instance. A collection cannot override jitter
  or free days.
- **Leniency.** Junk in a `meta` row yields the inherited value and a
  warning, not an error.
- **Free days.** A table of (weekday, free set) → shifted date, including
  the tie resolving later, the cap winning over the tie, and all-seven
  rejected.
- **Limits.** A day already half spent. A limit larger than the due count.
  A limit of zero. And the existing
  `the_due_count_on_the_page_matches_the_session_it_starts` extended to a
  limited collection, since that is the invariant most likely to break.
- **The weights refactor.** The five golden sequences in `fsrs.rs`,
  unchanged.
- **The optimizer.** Recovering planted weights from a synthetic log to a
  stated tolerance; refusing a short log; refusing a fit that does not
  improve.

## 10. Plans

Three implementation plans, in order:

- **A — settings surface.** `UserSettings`, `meta` storage, the three-layer
  resolution, `/settings`, `/collection/{slug}/settings`, daily limits,
  `FreeDays`. Ships useful on its own: sliders for the knobs that already
  exist, plus two features that do not.
- **B — weights.** `Weights` newtype, the `fsrs.rs` parameterization, the
  weight editor, MCP surface.
- **C — optimizer.** Review-log extraction, loss, Adam, guardrails, the
  Optimize/Apply flow.
