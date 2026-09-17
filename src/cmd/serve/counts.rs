//! Card and due-card counts for a collection.
//!
//! Counting reads the collection off disk and its schedule out of SQLite,
//! which is why it is not done inline in a handler. It lived in `git.rs`
//! because the git sync task was once the only thing that refreshed it.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::path::Path;
use std::path::PathBuf;

use crate::cmd::serve::config::DefaultsSection;
use crate::cmd::serve::config::ResolvedCollection;
use crate::cmd::serve::reviewdb::user_settings_for;
use crate::cmd::serve::state::AppState;
use crate::cmd::serve::state::CollectionInfo;
use crate::collection::Collection;
use crate::db::Database;
use crate::error::Fallible;
use crate::error::fail;
use crate::types::card::Card;
use crate::types::card_hash::CardHash;
use crate::types::date::Date;
use crate::types::limits::DailyBudget;
use crate::types::limits::DailyLimits;
use crate::types::timestamp::Timestamp;
use crate::user_db::UserDatabase;
use crate::user_settings::UserSettings;

/// The instance's sibling-burying policy, applied one card at a time.
///
/// A cloze note with several deletions parses to several cards that share a
/// family hash. A drill session queues one of the family and leaves the
/// rest for another day — so a due count taken without burying promises
/// cards the session will never show: "Start (12 due)" opening on
/// "0 of 7". Counting and queueing therefore run through this same filter.
///
/// What a collection's row says about its cards.
///
/// `due_today` is the size of the session its Drill button starts;
/// `due_uncapped` is what is really waiting behind any daily limit. They
/// differ only when a limit trimmed the queue, and the page shows both so
/// that a growing backlog is never hidden by a cap.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CollectionCounts {
    pub total_cards: usize,
    pub due_today: usize,
    pub due_uncapped: usize,
}

/// Which of the selected topics' cards a session queues.
///
/// Every scheduled path in the app asks for `DueToday`; a card the schedule
/// has placed in the future is not offered. `Ahead` is the one deliberate
/// exception: the user pointed at a topic and asked to review it now, so the
/// whole topic is queued, and neither sibling burial nor the daily limits
/// filter it — both exist to shape what the schedule hands out unasked, and
/// nothing here was unasked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueScope {
    /// Cards whose due date has arrived, or which were never reviewed.
    DueToday,
    /// Every card in the selected topics, due or not.
    Ahead,
}

impl QueueScope {
    /// Whether this scope leaves the schedule to decide.
    pub fn is_scheduled(self) -> bool {
        matches!(self, QueueScope::DueToday)
    }
}

/// Everything a resolved collection says about which due cards actually
/// reach a queue: whether to bury siblings, and how many cards it will hand
/// out today.
///
/// Resolved once, then used by the session builder *and* by every counter
/// that claims to describe it. A count filtered differently from the queue
/// is the bug this type exists to prevent.
#[derive(Clone, Copy, Debug)]
pub struct QueuePolicy {
    pub bury_siblings: bool,
    pub limits: DailyLimits,
}

impl QueuePolicy {
    /// The instance's answers, with the user's and the collection's laid
    /// over them.
    pub fn resolve(
        defaults: &DefaultsSection,
        user: &UserSettings,
        rc: &ResolvedCollection,
    ) -> Self {
        Self {
            bury_siblings: user.bury_siblings.unwrap_or(defaults.bury_siblings),
            limits: rc.limits(defaults.limits(), user),
        }
    }

    /// The instance's answers alone, for tests. Production always has a
    /// user to consult, even if it is the one who set nothing.
    #[cfg(test)]
    pub fn from_defaults(defaults: &DefaultsSection) -> Self {
        Self {
            bury_siblings: defaults.bury_siblings,
            limits: defaults.limits(),
        }
    }
}

/// What this collection has left of its limits today, and which of its cards
/// are new.
///
/// The two are read together because they are spent together: a card is new
/// or it is not, and which budget it spends follows from that.
pub fn budget_for(
    db: &Database,
    limits: DailyLimits,
    today: Date,
) -> Fallible<(DailyBudget, HashSet<CardHash>)> {
    let budget = DailyBudget::new(
        limits,
        db.count_reviews_in_date(today)?,
        db.new_cards_today_count(today)?,
    );
    Ok((budget, db.new_cards()?))
}

/// One `Burial` covers everything a single session would draw on: a family
/// is buried across the whole queue, not once per topic, and a cloze note
/// written into two collections is one family in both.
pub struct Burial {
    enabled: bool,
    seen: HashSet<CardHash>,
}

impl Burial {
    /// Burying as the resolved settings ask for it.
    pub fn new(bury_siblings: bool) -> Self {
        Self {
            enabled: bury_siblings,
            seen: HashSet::new(),
        }
    }

    /// Whether a session would queue `card`, given the families already
    /// taken. A card with no family — every basic card — always passes.
    pub fn admits(&mut self, card: &Card) -> bool {
        if !self.enabled {
            return true;
        }
        match card.family_hash() {
            Some(family) => self.seen.insert(family),
            None => true,
        }
    }
}

/// Count every collection, reporting a failure as zero rather than taking
/// the whole listing down: one unreadable collection must not empty the
/// page for the others.
///
/// Each user's database is opened once and reused across their collections.
/// The counts themselves stay per collection: they compare the *parsed*
/// cards against the rows, for the reason `gather_stats` gives — a row left
/// by a deleted card must not be counted, and a card with no row yet must be
/// — which no aggregate query can answer.
pub fn refresh_collection_info(
    state: &AppState,
    collections: &[ResolvedCollection],
) -> Vec<CollectionInfo> {
    let mut opened: HashMap<PathBuf, UserDatabase> = HashMap::new();
    let mut infos = Vec::new();
    for rc in collections {
        let counts = open_user_db(state, rc, &mut opened).and_then(|db| {
            let policy = QueuePolicy::resolve(
                &state.config.defaults,
                &user_settings_for(state, &rc.db_path),
                rc,
            );
            compute_collection_counts(&rc.coll_dir, db, policy)
        });
        let counts = match counts {
            Ok(counts) => counts,
            Err(e) => {
                log::warn!("Failed to load collection '{}': {e}", rc.name);
                CollectionCounts::default()
            }
        };

        infos.push(CollectionInfo {
            name: rc.name.clone(),
            slug: rc.slug.clone(),
            total_cards: counts.total_cards,
            due_today: counts.due_today,
            due_uncapped: counts.due_uncapped,
            owner: rc.owner.clone(),
        });
    }
    infos
}

/// `rc`'s collection view, on a connection shared with every other
/// collection in the same tree.
fn open_user_db(
    state: &AppState,
    rc: &ResolvedCollection,
    opened: &mut HashMap<PathBuf, UserDatabase>,
) -> Fallible<Database> {
    if let Some(why) = state.migration_failures.get(&rc.db_path) {
        return fail(format!(
            "its review database was not consolidated at startup: {why}"
        ));
    }
    let db = match opened.entry(rc.db_path.clone()) {
        Entry::Occupied(slot) => slot.into_mut(),
        Entry::Vacant(slot) => slot.insert(UserDatabase::open(&rc.db_path)?),
    };
    Ok(db.collection(rc.collection_id.clone()))
}

/// `(total cards, cards due today)`, inserting any card the database has
/// not seen before so a freshly written card is counted from the moment it
/// exists.
///
/// The due count is what a drill over this collection would actually hold,
/// buried siblings and all: it is read as the size of the session the row's
/// Drill button starts.
pub fn compute_collection_counts(
    coll_dir: &Path,
    db: Database,
    policy: QueuePolicy,
) -> Fallible<CollectionCounts> {
    if !coll_dir.exists() {
        return Ok(CollectionCounts::default());
    }

    let collection = Collection::open(coll_dir.to_path_buf(), db)?;
    let total_cards = collection.cards.len();

    let today: Date = Timestamp::now().date();

    // Sync new cards to DB
    let db_hashes = collection.db.card_hashes()?;
    let now = Timestamp::now();
    for card in collection.cards.iter() {
        if !db_hashes.contains(&card.hash()) {
            collection.db.insert_card(card.hash(), now)?;
        }
    }

    let due_hashes = collection.db.due_today(today)?;
    let mut burial = Burial::new(policy.bury_siblings);
    // Burial first, then the budget, in the order the session builder
    // applies them. Filtering in a different order here would count a
    // different set.
    let (mut budget, new_cards) = budget_for(&collection.db, policy.limits, today)?;
    // Both counts come from one pass, because `burial.admits` is spent as
    // it is asked: a second pass would bury against a used-up filter.
    let mut due_today = 0;
    let mut due_uncapped = 0;
    for card in collection
        .cards
        .iter()
        .filter(|c| due_hashes.contains(&c.hash()))
    {
        if !burial.admits(card) {
            continue;
        }
        // What is really waiting, before any cap. A limit must never make a
        // backlog look like a finished day.
        due_uncapped += 1;
        if budget.admits(new_cards.contains(&card.hash())) {
            due_today += 1;
        }
    }

    Ok(CollectionCounts {
        total_cards,
        due_today,
        due_uncapped,
    })
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::refresh_collection_info;
    use crate::cmd::serve::cards::CardRoot;
    use crate::cmd::serve::cards::IdPolicy;
    use crate::cmd::serve::cards::discover_local_collections;
    use crate::cmd::serve::config::ResolvedCollection;
    use crate::error::Fallible;
    use crate::helper::create_tmp_directory;
    use crate::types::collection_id::CollectionId;
    use crate::utils::ensure_dir;

    /// Every collection in a tree shares one database and the landing page
    /// counts them all in one pass — so a card written into one collection
    /// must not be counted in its neighbour, and both must be counted.
    #[test]
    fn each_collection_is_counted_against_its_own_rows() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let state = crate::cmd::serve::state::test_support::state_with_data_dir(dir.clone());
        let root = CardRoot::for_user(&dir, None)?;
        for (name, card) in [
            ("Biology", "Q: cell?\nA: yes\n"),
            ("Spanish", "Q: hola?\nA: hi\n"),
        ] {
            let folder = root.path().join(name);
            std::fs::create_dir_all(&folder)?;
            std::fs::write(folder.join("Deck.md"), card)?;
        }
        ensure_dir(&dir.join("db"), "review database directory")?;
        let found =
            discover_local_collections(&root, &dir.join("db"), None, IdPolicy::CreateMissing)?;
        assert_eq!(found.len(), 2);

        let infos = refresh_collection_info(&state, &found);
        assert_eq!(infos.len(), 2);
        for info in &infos {
            assert_eq!(info.total_cards, 1, "{} counted wrong", info.name);
            assert_eq!(info.due_today, 1, "{} counted wrong", info.name);
        }
        Ok(())
    }

    #[test]
    fn test_refresh_collection_info_carries_owner() -> Fallible<()> {
        let dir = tempdir()?;
        let rc = ResolvedCollection {
            name: "Japanese".to_string(),
            slug: "japanese".to_string(),
            coll_dir: dir.path().to_path_buf(),
            db_path: dir.path().join("hashcards.db"),
            collection_id: CollectionId::new("test-collection")?,
            owner: Some("me@example.com".to_string()),
            overrides: Default::default(),
        };
        let state =
            crate::cmd::serve::state::test_support::state_with_data_dir(dir.path().to_path_buf());
        let infos = refresh_collection_info(&state, &[rc]);
        assert_eq!(infos[0].owner.as_deref(), Some("me@example.com"));
        Ok(())
    }

    /// The landing page's "N due" is the size of the session its Drill
    /// button starts, so it buries siblings exactly as the queue does. A
    /// cloze note with two deletions is one card today, not two.
    #[test]
    fn a_collection_row_counts_a_cloze_family_once() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let state = crate::cmd::serve::state::test_support::state_with_data_dir(dir.clone());
        let root = CardRoot::for_user(&dir, None)?;
        let folder = root.path().join("Spanish");
        std::fs::create_dir_all(&folder)?;
        std::fs::write(folder.join("Deck.md"), "C: Foo [bar] baz [quux].\n")?;
        ensure_dir(&dir.join("db"), "review database directory")?;
        let found =
            discover_local_collections(&root, &dir.join("db"), None, IdPolicy::CreateMissing)?;

        let infos = refresh_collection_info(&state, &found);
        assert_eq!(infos.len(), 1);
        assert_eq!(
            infos[0].total_cards, 2,
            "the collection holds both deletions"
        );
        assert_eq!(infos[0].due_today, 1, "but a drill would show one of them");
        Ok(())
    }
}
