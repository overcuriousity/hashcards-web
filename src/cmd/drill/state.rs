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

use std::path::PathBuf;

use std::collections::HashMap;
use std::collections::HashSet;

use crate::cmd::drill::cache::Cache;
use crate::db::Database;
use crate::error::Fallible;
use crate::fsrs::Grade;
use crate::rng::TinyRng;
use crate::types::card::Card;
use crate::types::card_hash::CardHash;
use crate::types::performance::Performance;
use crate::types::performance::Scheduling;
use crate::types::timestamp::Timestamp;

/// One collection's database, with the session row opened in it.
pub struct SessionDb {
    pub db: Database,
    pub session_id: i64,
    /// Set only for a session drawing on more than one collection, where a
    /// card's media must resolve against the collection it actually came
    /// from. `None` means "use the session's own collection", which is the
    /// CLI drill and the ordinary single-collection serve.
    pub source: Option<SessionSource>,
    /// How this collection's cards are scheduled. Held per database rather
    /// than per session because a deck's collections may each ask for their
    /// own retention, and a card keeps exactly one schedule however it was
    /// reached.
    pub scheduling: Scheduling,
}

/// Where a routed card's files live, and how to address them over HTTP.
#[derive(Clone)]
pub struct SessionSource {
    pub coll_dir: PathBuf,
    pub file_url_prefix: String,
}

/// The databases a drill session writes to.
///
/// Drilling one collection uses exactly one. A custom deck draws its cards
/// from several collections, and each review must land in the database its
/// card actually belongs to — a card scheduled in two places would come due
/// twice, on schedules that drift apart. Routing is by card hash, which is
/// the card's identity everywhere else too.
pub struct SessionDbs {
    dbs: Vec<SessionDb>,
    /// Card hash to index into `dbs`. Empty for a single-collection session,
    /// where every card belongs to `dbs[0]`.
    routes: HashMap<CardHash, usize>,
}

impl SessionDbs {
    /// A session over a single collection: every card routes to this database.
    pub fn single(db: Database, session_id: i64, scheduling: Scheduling) -> Self {
        Self {
            dbs: vec![SessionDb {
                db,
                session_id,
                source: None,
                scheduling,
            }],
            routes: HashMap::new(),
        }
    }

    /// A session drawing on several collections. `routes` maps each card to
    /// its collection's index in `dbs`.
    pub fn routed(dbs: Vec<SessionDb>, routes: HashMap<CardHash, usize>) -> Self {
        Self { dbs, routes }
    }

    /// The database owning `hash`.
    ///
    /// A card with no route falls back to the first database: that is the
    /// single-collection case, where the map is empty by construction.
    pub fn for_card(&self, hash: CardHash) -> &SessionDb {
        let idx = self.routes.get(&hash).copied().unwrap_or(0);
        // `dbs` is never empty, and every index in `routes` came from it.
        &self.dbs[idx.min(self.dbs.len().saturating_sub(1))]
    }

    /// How the collection owning `hash` schedules it.
    pub fn scheduling_for(&self, hash: CardHash) -> Scheduling {
        self.for_card(hash).scheduling
    }

    /// The first database. Used for session-wide bookkeeping that is not
    /// tied to a particular card.
    pub fn primary(&self) -> &SessionDb {
        &self.dbs[0]
    }

    pub fn all(&self) -> &[SessionDb] {
        &self.dbs
    }

    /// Mark every open session row as still alive.
    pub fn touch_all(&self, now: Timestamp) -> Fallible<()> {
        for entry in &self.dbs {
            entry.db.touch_session(entry.session_id, now)?;
        }
        Ok(())
    }

    /// Close every open session row.
    pub fn close_all(&self, ended_at: Timestamp) -> Fallible<()> {
        for entry in &self.dbs {
            entry.db.close_session(entry.session_id, ended_at)?;
        }
        Ok(())
    }

    /// Re-key the card-to-database routes after an edit renamed or removed
    /// cards.
    ///
    /// Rebuilt wholesale rather than patched in place: a rename whose target
    /// is also some other rename's source would otherwise give a different
    /// answer depending on iteration order. A single-collection session has
    /// an empty map by construction and needs no work — `for_card` falls
    /// back to the first database.
    pub fn rekey_routes(
        &mut self,
        renamed: &HashMap<CardHash, CardHash>,
        removed: &HashSet<CardHash>,
    ) {
        if self.routes.is_empty() {
            return;
        }
        let mut next: HashMap<CardHash, usize> = HashMap::with_capacity(self.routes.len());
        for (hash, idx) in self.routes.drain() {
            if removed.contains(&hash) {
                continue;
            }
            next.insert(renamed.get(&hash).copied().unwrap_or(hash), idx);
        }
        self.routes = next;
    }

    /// Every review recorded in this session, across all its databases.
    pub fn all_reviews(&self) -> Fallible<Vec<crate::db::ReviewRow>> {
        let mut rows = Vec::new();
        for entry in &self.dbs {
            rows.extend(entry.db.get_reviews_for_session(entry.session_id)?);
        }
        Ok(rows)
    }
}

/// What an edit did to a collection's cards, in the terms a live session
/// needs: which hashes were renamed to which cards, and which cards the
/// edit removed from the corpus outright.
pub struct CardMigration {
    /// Old hash to the re-parsed card that replaces it.
    pub renamed: Vec<(CardHash, Card)>,
    /// Cards the edit removed from the corpus.
    pub removed: Vec<CardHash>,
}

/// What the migration did to one session, for the post-save flash.
pub struct MigrationEffect {
    pub renamed: usize,
    pub dropped: usize,
    pub session_finished: bool,
}

pub struct MutableState {
    pub reveal: bool,
    pub dbs: SessionDbs,
    pub cache: Cache,
    pub cards: Vec<Card>,
    pub reviews: Vec<Review>,
    pub finished_at: Option<Timestamp>,
    /// Timestamp when the current card was first shown (for per-card timing).
    pub card_shown_at: Option<Timestamp>,
    /// RNG for interval jitter, seeded once per session.
    pub rng: TinyRng,
}

impl MutableState {
    /// State for a freshly started session: nothing revealed, nothing graded.
    pub fn new(dbs: SessionDbs, cache: Cache, cards: Vec<Card>, rng: TinyRng) -> Self {
        Self {
            reveal: false,
            dbs,
            cache,
            cards,
            reviews: Vec::new(),
            finished_at: None,
            card_shown_at: None,
            rng,
        }
    }

    /// Record that the current card has just been served to the client.
    ///
    /// Called from the GET path so per-card durations include recall time
    /// (measured from display, not from reveal). Idempotent: a page refresh
    /// does not restart the timer. No-op when the session is finished or the
    /// queue is empty.
    pub fn mark_card_shown(&mut self) {
        if self.card_shown_at.is_none() && self.finished_at.is_none() && !self.cards.is_empty() {
            self.card_shown_at = Some(Timestamp::now());
        }
    }

    /// Re-key this session onto the cards an edit produced.
    ///
    /// A live session holds card identity in four places: the queue (where
    /// the same card may appear twice, a Forgot or Hard grade having pushed
    /// it to the back while it is also in `reviews`), the undo stack, the
    /// performance cache, and the per-card database routes. An edit renames
    /// a hash, and it must move all four together or none.
    ///
    /// Deliberately infallible. It runs after the database transaction has
    /// committed and the file is on disk, so there is nothing left to roll
    /// back to: a failure here could only be reported, never repaired.
    ///
    /// A removed card leaves the queue, leaves the cache, and its reviews
    /// leave the undo stack. Grades already written stay written — they
    /// happened, and the history stays attached to the hash that existed at
    /// the time — but undoing back to a card that is in no file would put
    /// the user in front of something they cannot edit, re-drill or reach
    /// again, and a further grade on it would land on a hash orphaned from
    /// every file.
    pub fn apply_card_migration(&mut self, m: &CardMigration) -> MigrationEffect {
        let renames: HashMap<CardHash, Card> = m
            .renamed
            .iter()
            .map(|(old, card)| (*old, card.clone()))
            .collect();
        let rename_hashes: HashMap<CardHash, CardHash> = m
            .renamed
            .iter()
            .map(|(old, card)| (*old, card.hash()))
            .collect();
        let removed: HashSet<CardHash> = m.removed.iter().copied().collect();

        // Counted before anything moves, and over what the *session* holds:
        // an edit renames cards across a whole file, and this session may
        // hold only some of them.
        let held: HashSet<CardHash> = self
            .cards
            .iter()
            .map(|c| c.hash())
            .chain(self.reviews.iter().map(|r| r.card.hash()))
            .collect();
        let renamed = m
            .renamed
            .iter()
            .filter(|(old, _)| held.contains(old))
            .count();
        let dropped = m.removed.iter().filter(|h| held.contains(h)).count();

        let head_before: Option<CardHash> = self.cards.first().map(|c| c.hash());

        // The queue, every copy of every card.
        let mut cards: Vec<Card> = Vec::with_capacity(self.cards.len());
        for card in self.cards.drain(..) {
            let hash = card.hash();
            match renames.get(&hash) {
                Some(replacement) => cards.push(replacement.clone()),
                None if removed.contains(&hash) => {}
                None => cards.push(card),
            }
        }
        self.cards = cards;

        // The undo stack. `prev_performance` travels with its review, so
        // undo across a rename still restores the right performance.
        self.reviews.retain(|r| !removed.contains(&r.card.hash()));
        for review in &mut self.reviews {
            if let Some(replacement) = renames.get(&review.card.hash()) {
                review.card = replacement.clone();
            }
        }

        // The cache and the database routes.
        for (old, card) in &m.renamed {
            self.cache.rekey(*old, card.hash());
        }
        for hash in &m.removed {
            self.cache.remove(*hash);
        }
        self.dbs.rekey_routes(&rename_hashes, &removed);

        // The rendered page carries the head card's answer. If the head is
        // the same card under a new hash, the reveal still describes what is
        // on screen; if a different card is now in front, it does not.
        let head_after: Option<CardHash> = self.cards.first().map(|c| c.hash());
        let head_followed = match head_before {
            Some(before) => {
                head_after == Some(rename_hashes.get(&before).copied().unwrap_or(before))
            }
            None => head_after.is_none(),
        };
        if !head_followed {
            self.reveal = false;
            self.card_shown_at = None;
        }

        // An edit can empty the queue. Left running, the GET path would
        // render a live session with no card in it.
        let mut session_finished = false;
        if self.cards.is_empty() && self.finished_at.is_none() {
            let ended_at = Timestamp::now();
            if let Err(e) = self.dbs.close_all(ended_at) {
                log::error!("Could not close the session an edit emptied: {e}");
            }
            self.finished_at = Some(ended_at);
            session_finished = true;
        }

        MigrationEffect {
            renamed,
            dropped,
            session_finished,
        }
    }

    /// Session progress as `(first_graded, repeats)`.
    ///
    /// `first_graded` counts distinct cards graded at least once — the
    /// progress bar advances on the first grade of each card. `repeats`
    /// counts the additional grades of already-counted cards, i.e. the
    /// completed re-queues from Forgot/Hard. Both are derived from the
    /// undo-aware `reviews` list, so Undo rolls progress back and no extra
    /// session state is stored.
    pub fn progress(&self) -> (usize, usize) {
        let mut seen: HashSet<CardHash> = HashSet::new();
        for review in &self.reviews {
            seen.insert(review.card.hash());
        }
        let first_graded = seen.len();
        let repeats = self.reviews.len() - first_graded;
        (first_graded, repeats)
    }
}

#[derive(Clone)]
pub struct Review {
    pub card: Card,
    pub review_id: i64,
    pub grade: Grade,
    /// Performance before this review, used to restore state on undo.
    pub prev_performance: Performance,
}

impl Review {
    pub fn should_repeat(&self) -> bool {
        self.grade == Grade::Forgot || self.grade == Grade::Hard
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::cmd::drill::cache::Cache;
    use crate::db::Database;
    use crate::types::card::CardContent;
    use crate::types::performance::DesiredRetention;
    use crate::types::performance::Jitter;
    use crate::types::performance::Performance;
    use crate::types::performance::Scheduling;

    fn make_card(question: &str) -> Card {
        Card::new(
            "test-deck".to_string(),
            PathBuf::from("/tmp/deck.md"),
            (1, 2),
            CardContent::new_basic(question, "answer"),
        )
    }

    fn make_review(card: &Card, grade: Grade) -> Review {
        Review {
            card: card.clone(),
            review_id: 1,
            grade,
            prev_performance: Performance::New,
        }
    }

    /// A card whose text — and therefore hash — differs from `make_card`'s.
    fn make_card_with(question: &str, answer: &str) -> Card {
        Card::new(
            "test-deck".to_string(),
            PathBuf::from("/tmp/deck.md"),
            (1, 2),
            CardContent::new_basic(question, answer),
        )
    }

    /// A session holding `cards`, with every card in the cache as `New` and
    /// a real in-memory database behind it.
    fn session_over(cards: Vec<Card>) -> Fallible<MutableState> {
        let db = Database::memory()?;
        let session_id = db.create_session(Timestamp::now())?;
        let mut cache = Cache::new();
        for card in &cards {
            // A card queued twice is cached once.
            let _ = cache.insert(card.hash(), Performance::New);
        }
        Ok(MutableState::new(
            SessionDbs::single(
                db,
                session_id,
                Scheduling {
                    jitter: Jitter::none(),
                    ..Scheduling::default()
                },
            ),
            cache,
            cards,
            TinyRng::from_seed(1),
        ))
    }

    /// A deck draws on several collections, and each may ask for its own
    /// retention. A card must be scheduled by the collection it belongs to
    /// however it was reached: the alternative is one card carrying two
    /// schedules depending on which deck you opened.
    #[test]
    fn scheduling_follows_the_card_to_its_own_collection() -> Fallible<()> {
        let strict = Scheduling {
            retention: DesiredRetention::new(0.95)?,
            ..Scheduling::default()
        };
        let relaxed = Scheduling {
            retention: DesiredRetention::new(0.8)?,
            ..Scheduling::default()
        };
        let a = make_card("belongs to the strict collection");
        let b = make_card("belongs to the relaxed collection");
        let db = |scheduling| -> Fallible<SessionDb> {
            let db = Database::memory()?;
            let session_id = db.create_session(Timestamp::now())?;
            Ok(SessionDb {
                db,
                session_id,
                source: None,
                scheduling,
            })
        };
        let routes = HashMap::from([(a.hash(), 0), (b.hash(), 1)]);
        let dbs = SessionDbs::routed(vec![db(strict)?, db(relaxed)?], routes);

        assert_eq!(dbs.scheduling_for(a.hash()), strict);
        assert_eq!(dbs.scheduling_for(b.hash()), relaxed);
        // An unrouted card is the single-collection case: the first database.
        assert_eq!(dbs.scheduling_for(make_card("unrouted").hash()), strict);
        Ok(())
    }

    /// FEAT-08: the bar advances on the first grade of each card; further
    /// grades of the same card count as repeats. Derived from `reviews`,
    /// so Undo (which pops `reviews`) rolls progress back automatically.
    #[test]
    fn test_progress_counts_first_grades_and_repeats() {
        let db = Database::memory().unwrap();
        let session_id = db.create_session(Timestamp::now()).unwrap();
        let a = make_card("question a");
        let b = make_card("question b");
        let mut mutable = MutableState::new(
            SessionDbs::single(
                db,
                session_id,
                Scheduling {
                    jitter: Jitter::none(),
                    ..Scheduling::default()
                },
            ),
            Cache::new(),
            vec![a.clone(), b.clone()],
            TinyRng::from_seed(1),
        );
        assert_eq!(mutable.progress(), (0, 0));
        // First grade of card a; Forgot re-queues it but it still counts once.
        mutable.reviews.push(make_review(&a, Grade::Forgot));
        assert_eq!(mutable.progress(), (1, 0));
        // First grade of card b.
        mutable.reviews.push(make_review(&b, Grade::Good));
        assert_eq!(mutable.progress(), (2, 0));
        // Card a comes around again: a repeat, not new progress.
        mutable.reviews.push(make_review(&a, Grade::Good));
        assert_eq!(mutable.progress(), (2, 1));
        // Undo pops the repeat review; progress recovers on its own.
        mutable.reviews.pop();
        assert_eq!(mutable.progress(), (2, 0));
    }

    /// A rename must reach the queue, the undo stack and the cache in one
    /// move. Leaving any one of them on the old hash strands the grades the
    /// session is about to write.
    #[test]
    fn migration_renames_a_card_in_queue_cache_and_reviews() -> Fallible<()> {
        let old = make_card("question a");
        let new = make_card_with("question a", "a better answer");
        let mut mutable = session_over(vec![old.clone()])?;
        mutable.reviews.push(make_review(&old, Grade::Good));

        let effect = mutable.apply_card_migration(&CardMigration {
            renamed: vec![(old.hash(), new.clone())],
            removed: Vec::new(),
        });

        assert_eq!(effect.renamed, 1);
        assert_eq!(effect.dropped, 0);
        assert!(!effect.session_finished);
        assert_eq!(mutable.cards[0].hash(), new.hash());
        assert_eq!(mutable.reviews[0].card.hash(), new.hash());
        assert!(mutable.cache.get(new.hash())?.is_new());
        assert!(mutable.cache.get(old.hash()).is_err());
        Ok(())
    }

    /// A Forgot or Hard grade pushes a card to the back of the queue while
    /// it is also in `reviews`, so the same card is in the queue twice at
    /// the moment an edit lands. Both copies must follow the rename.
    #[test]
    fn migration_renames_every_copy_of_a_requeued_card() -> Fallible<()> {
        let old = make_card("question a");
        let other = make_card("question b");
        let new = make_card_with("question a", "a better answer");
        let mut mutable = session_over(vec![old.clone(), other, old.clone()])?;

        mutable.apply_card_migration(&CardMigration {
            renamed: vec![(old.hash(), new.clone())],
            removed: Vec::new(),
        });

        let renamed = mutable
            .cards
            .iter()
            .filter(|c| c.hash() == new.hash())
            .count();
        assert_eq!(renamed, 2, "both copies of the requeued card follow");
        assert!(!mutable.cards.iter().any(|c| c.hash() == old.hash()));
        Ok(())
    }

    /// Undo restores the performance a `Review` carries, so a review that
    /// travelled with its card must still restore the right one.
    #[test]
    fn migration_keeps_prev_performance_on_a_renamed_review() -> Fallible<()> {
        let old = make_card("question a");
        let new = make_card_with("question a", "a better answer");
        let mut mutable = session_over(vec![old.clone()])?;
        let mut review = make_review(&old, Grade::Good);
        review.prev_performance = Performance::New;
        review.review_id = 42;
        mutable.reviews.push(review);

        mutable.apply_card_migration(&CardMigration {
            renamed: vec![(old.hash(), new.clone())],
            removed: Vec::new(),
        });

        assert_eq!(mutable.reviews.len(), 1);
        assert_eq!(mutable.reviews[0].review_id, 42);
        assert!(mutable.reviews[0].prev_performance.is_new());
        assert_eq!(mutable.reviews[0].card.hash(), new.hash());
        Ok(())
    }

    /// A card the edit deleted leaves the queue, leaves the cache, and its
    /// reviews leave the undo stack: undoing back to a card that is in no
    /// file would put the user in front of something they cannot edit,
    /// re-drill or reach again. `progress()` rewinds with it.
    #[test]
    fn migration_drops_a_removed_card_and_its_reviews() -> Fallible<()> {
        let gone = make_card("question a");
        let kept = make_card("question b");
        let mut mutable = session_over(vec![gone.clone(), kept.clone()])?;
        mutable.reviews.push(make_review(&gone, Grade::Good));
        assert_eq!(mutable.progress(), (1, 0));

        let effect = mutable.apply_card_migration(&CardMigration {
            renamed: Vec::new(),
            removed: vec![gone.hash()],
        });

        assert_eq!(effect.dropped, 1);
        assert_eq!(mutable.cards.len(), 1);
        assert_eq!(mutable.cards[0].hash(), kept.hash());
        assert!(mutable.reviews.is_empty());
        assert!(mutable.cache.get(gone.hash()).is_err());
        assert_eq!(mutable.progress(), (0, 0));
        Ok(())
    }

    /// An edit can empty the queue. Left alone, the GET path would render a
    /// live session with nothing in it, so the session finishes here.
    #[test]
    fn migration_finishes_a_session_it_empties() -> Fallible<()> {
        let gone = make_card("question a");
        let mut mutable = session_over(vec![gone.clone()])?;

        let effect = mutable.apply_card_migration(&CardMigration {
            renamed: Vec::new(),
            removed: vec![gone.hash()],
        });

        assert!(effect.session_finished);
        assert!(mutable.cards.is_empty());
        assert!(mutable.finished_at.is_some());
        Ok(())
    }

    /// A session that started after the file was written already holds the
    /// new hashes. The migration looks up old hashes, finds none, and
    /// changes nothing — which is why the write-then-migrate ordering has
    /// no race in it.
    #[test]
    fn migration_against_a_session_holding_the_new_hashes_is_a_noop() -> Fallible<()> {
        let old = make_card("question a");
        let new = make_card_with("question a", "a better answer");
        let mut mutable = session_over(vec![new.clone()])?;

        let effect = mutable.apply_card_migration(&CardMigration {
            renamed: vec![(old.hash(), new.clone())],
            removed: Vec::new(),
        });

        assert_eq!(effect.renamed, 0);
        assert_eq!(effect.dropped, 0);
        assert!(!effect.session_finished);
        assert_eq!(mutable.cards.len(), 1);
        assert_eq!(mutable.cards[0].hash(), new.hash());
        Ok(())
    }

    /// A card drilled inside a custom deck routes its grades to its own
    /// collection's database. The route must follow the rename, or the
    /// grade after an edit lands in the wrong database.
    #[test]
    fn migration_rekeys_the_database_route() -> Fallible<()> {
        let old = make_card("question a");
        let new = make_card_with("question a", "a better answer");
        let db_a = Database::memory()?;
        let session_a = db_a.create_session(Timestamp::now())?;
        let db_b = Database::memory()?;
        let session_b = db_b.create_session(Timestamp::now())?;
        let source = SessionSource {
            coll_dir: PathBuf::from("/tmp/coll"),
            file_url_prefix: "/collection/x/file".to_string(),
        };
        let dbs = SessionDbs::routed(
            vec![
                SessionDb {
                    db: db_a,
                    session_id: session_a,
                    source: Some(source.clone()),
                    scheduling: Scheduling::default(),
                },
                SessionDb {
                    db: db_b,
                    session_id: session_b,
                    source: Some(source),
                    scheduling: Scheduling::default(),
                },
            ],
            HashMap::from([(old.hash(), 1)]),
        );
        let mut cache = Cache::new();
        cache.insert(old.hash(), Performance::New)?;
        let mut mutable = MutableState::new(dbs, cache, vec![old.clone()], TinyRng::from_seed(1));

        mutable.apply_card_migration(&CardMigration {
            renamed: vec![(old.hash(), new.clone())],
            removed: Vec::new(),
        });

        // Routing by the new hash must still reach the second database, not
        // fall back to the first.
        assert_eq!(mutable.dbs.for_card(new.hash()).session_id, session_b);
        Ok(())
    }
}
