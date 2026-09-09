//! Card and due-card counts for a collection.
//!
//! Counting reads the collection off disk and its schedule out of SQLite,
//! which is why it is not done inline in a handler. It lived in `git.rs`
//! because the git sync task was once the only thing that refreshed it.

use std::path::Path;

use crate::cmd::serve::config::ResolvedCollection;
use crate::cmd::serve::reviewdb::open_collection_db;
use crate::cmd::serve::state::AppState;
use crate::cmd::serve::state::CollectionInfo;
use crate::collection::Collection;
use crate::db::Database;
use crate::error::Fallible;
use crate::types::date::Date;
use crate::types::timestamp::Timestamp;

/// Count every collection, reporting a failure as zero rather than taking
/// the whole listing down: one unreadable collection must not empty the
/// page for the others.
pub fn refresh_collection_info(
    state: &AppState,
    collections: &[ResolvedCollection],
) -> Vec<CollectionInfo> {
    let mut infos = Vec::new();
    for rc in collections {
        let counts = open_collection_db(state, rc)
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
    use crate::cmd::serve::config::ResolvedCollection;
    use crate::error::Fallible;
    use crate::types::collection_id::CollectionId;

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
