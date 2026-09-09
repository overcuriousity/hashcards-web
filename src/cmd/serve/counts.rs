//! Card and due-card counts for a collection.
//!
//! Counting reads the collection off disk and its schedule out of SQLite,
//! which is why it is not done inline in a handler. It lived in `git.rs`
//! because the git sync task was once the only thing that refreshed it.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::Path;
use std::path::PathBuf;

use crate::cmd::serve::config::ResolvedCollection;
use crate::cmd::serve::state::AppState;
use crate::cmd::serve::state::CollectionInfo;
use crate::collection::Collection;
use crate::db::Database;
use crate::error::Fallible;
use crate::error::fail;
use crate::types::date::Date;
use crate::types::timestamp::Timestamp;
use crate::user_db::UserDatabase;

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
        let counts = open_user_db(state, rc, &mut opened)
            .and_then(|db| compute_collection_counts(&rc.coll_dir, db));
        let (total_cards, due_today) = match counts {
            Ok(counts) => counts,
            Err(e) => {
                log::warn!("Failed to load collection '{}': {e}", rc.name);
                (0, 0)
            }
        };

        infos.push(CollectionInfo {
            name: rc.name.clone(),
            slug: rc.slug.clone(),
            total_cards,
            due_today,
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
pub fn compute_collection_counts(coll_dir: &Path, db: Database) -> Fallible<(usize, usize)> {
    if !coll_dir.exists() {
        return Ok((0, 0));
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
    let due_today = collection
        .cards
        .iter()
        .filter(|c| due_hashes.contains(&c.hash()))
        .count();

    Ok((total_cards, due_today))
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
}
