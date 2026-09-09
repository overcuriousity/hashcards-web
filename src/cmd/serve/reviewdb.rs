//! Opening a collection's review database.
//!
//! One function, so that every handler resolves a collection to rows the
//! same way — and so that the check for a user whose startup merge failed
//! has exactly one place to live.

use crate::cmd::serve::config::ResolvedCollection;
use crate::cmd::serve::state::AppState;
use crate::db::Database;
use crate::error::Fallible;
use crate::error::fail;
use crate::user_db::UserDatabase;

/// The rows belonging to `rc`, inside its owner's review database.
///
/// Refuses outright when this user's startup merge failed: an empty database
/// would be served as an empty history, which is indistinguishable from
/// having lost everything.
pub fn open_collection_db(state: &AppState, rc: &ResolvedCollection) -> Fallible<Database> {
    if let Some(why) = state.migration_failures.get(&rc.db_path) {
        return fail(format!(
            "This account's review databases could not be consolidated when the server started, \
             so its history cannot be read: {why}. Nothing has been lost — the previous databases \
             are in the server's `db/legacy` directory or still where they were. Ask whoever runs \
             this server to check the startup log."
        ));
    }
    Ok(UserDatabase::open(&rc.db_path)?.collection(rc.collection_id.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::serve::state::test_support::state_with_data_dir;
    use crate::types::collection_id::CollectionId;

    /// A user whose merge failed must never be handed an empty database:
    /// starting their history from zero, silently, is the outcome this whole
    /// design exists to prevent.
    #[test]
    fn a_user_whose_merge_failed_is_refused_rather_than_started_from_zero() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let mut state = state_with_data_dir(dir.path().to_path_buf());
        let db_path = dir.path().join("db").join("default.db");
        state.migration_failures = std::sync::Arc::new(
            [(db_path.clone(), "disk is on fire".to_string())]
                .into_iter()
                .collect(),
        );
        let rc = ResolvedCollection {
            name: "Biology".to_string(),
            slug: "biology".to_string(),
            coll_dir: dir.path().to_path_buf(),
            db_path,
            collection_id: CollectionId::new("aaaa1111")?,
            owner: None,
            overrides: Default::default(),
        };

        let message = match open_collection_db(&state, &rc) {
            Ok(_) => return crate::error::fail("a failed merge must refuse to open"),
            Err(e) => e.to_string(),
        };
        assert!(message.contains("disk is on fire"), "{message}");
        assert!(message.contains("legacy"), "{message}");
        Ok(())
    }
}
