# Roadmap

Ordered. Items are worked top to bottom; anything unordered lives in
`IDEAS.md`.

## 0. Ending a session must always be possible

**Explained and fixed.** The `End session` line was not missing from the
markup; it was underneath the gesture bar. An installed app on Android 15 or
later is drawn edge to edge, so the layout viewport — and every `100dvh`
measured against it — spans the status bar and the gesture bar, while the
drill is exactly one viewport tall with a bar pinned to each edge. Nothing in
the page reserved what the system draws over, so the bottom band of
`div.controls` was covered: at v0.4.10 that was the whole of the End button,
and once it was restyled to a quiet line it was the line plus the lower edge
of the grade row. It only ever happened in the installed app, which is why
the same URL in a tab on the same phone looked right.

The page now declares `viewport-fit=cover` — until it does, `env(safe-area-inset-*)`
reports zero whether or not the bars are covering anything — and reserves the
insets once, as padding on a border-box `body`, with the drill filling the
body's content box rather than measuring the screen a second time. The fixed
theme switch adds them to its own offsets, since a fixed element is placed
against the viewport and the body's padding cannot reach it.

The earlier note here read the screenshot as showing the End button taking no
height at all, because the visible controls bar ended exactly at the top of
the gesture bar. That is what a bar clipped by the gesture bar looks like.

Two defects found while looking, both real regardless, both fixed at the time:

- ~~`.end-link` never got its own styling.~~ `.controls button`
  (specificity 0-1-1) outranked `.end-link` (0-1-0) on every property the two
  share, so the way out of a session rendered as a bordered 44px button
  rather than the quiet line the rule was written to produce. The grade rules
  now exclude it by name. This is why the button was hidden *completely*
  rather than partly: at 44px plus its margin it fitted inside the covered
  band exactly.
- ~~`/style.css` was served `immutable` from an unversioned path.~~ It is now
  content addressed, so `immutable` is honest and a deploy invalidates it.
  Note this was only ever true of the stylesheet: `/script.js` and a
  collection's `script.js` sent no cache header at all, which left them to
  heuristic caching; all three now revalidate or carry a hash.

Still open from this report: **there is no service worker.** The manifest
(`template.rs`) makes the app installable, but every reveal, grade and `End`
is a form POST that needs the network. On a train, the whole session locks
up — which is the situation the report came from.

## 1. FSRS

`src/fsrs.rs` is a real FSRS-5: the nineteen stock weights, the power-law
forgetting curve (`F = 19/81`, `C = -0.5`), stability-on-success and
stability-on-failure kept apart, difficulty with mean reversion, and ±5%
interval fuzz (`types/performance.rs:47`). What is missing is everything
around it.

1. ~~**Desired retention is a constant.**~~ Done: `[defaults]
   desired_retention`, overridable per collection.
2. ~~**The maximum interval is a constant.**~~ Done: `[defaults]
   max_interval_days`, overridable per collection. Note the earlier entry
   here claimed there was *no* cap; there was one, hardcoded at 256 days,
   which is low enough that it was doing real scheduling invisibly. The
   default is unchanged so no existing schedule moved, but raising it is
   worth considering as a separate decision.
3. **No parameter optimisation.** The weights are the published defaults, so
   the scheduler is calibrated to a population rather than to the person
   using it. The `reviews` table already stores every input an optimiser
   needs — grade, stability, difficulty, timestamps, and a `voided` flag to
   exclude undone rows — so this is a computation over data already held,
   not a schema change. Report the fitted weights and let them be adopted or
   discarded, rather than swapping them in silently.
4. **No learning or relearning steps.** A forgotten card is requeued inside
   the session, but there is no notion of same-day steps, and no separate
   path for a card that lapsed after being mature. This overlaps item 5
   below and is best done with it.

## 2. Scheduling and the card lifecycle

The largest gap against Anki, and the one users notice without knowing the
vocabulary. The `cards` table (`src/schema.sql`) has `review_count` and
nothing else: a card seen once and a card held for two years are the same
kind of row.

1. **Card states.** New, learning, review, relearning, as an explicit state
   machine rather than a count. Everything else in this section needs it,
   and so does FSRS item 4. `IDEAS.md` has carried this since the fork.
2. **Suspend and bury.** There is no way to take a card out of rotation.
   Bookmark is a note to self, not a scheduling act — a card you cannot
   answer today should be droppable for the day (bury) or indefinitely
   (suspend), and both are reversible from the browse page.
3. **Leeches.** No lapse counter exists, so a card that is failed forever is
   failed forever in silence. Count lapses, and at a threshold suspend the
   card and say so: the answer to a card you cannot learn is to rewrite it,
   which now takes one tap.
4. **Per-day limits.** The session-size dropdown
   (`cmd/serve/browse.rs:277`) caps one sitting. Anki's limits are per day
   and persist across sittings, which is what actually keeps a backlog from
   becoming unfaceable — separate caps for new cards and reviews.
5. **Manual overrides.** Forget (reset a card to new), set due date, and
   reschedule. Each is a small write against `cards`, and each is the escape
   hatch for a schedule that has gone wrong — which, without item 3 above,
   it eventually will.
6. **Filtered study.** Cram a topic before an exam, or re-drill today's
   failures, without disturbing the real schedule. Saved decks are a static
   selection; this is a query, and it wants the card states from item 1.
