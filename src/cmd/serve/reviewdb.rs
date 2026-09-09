//! Opening a collection's review database.
//!
//! One function, so that every handler resolves a collection to rows the
//! same way — and so that the check for a user whose startup merge failed
//! has exactly one place to live.

use crate::cmd::serve::config::ResolvedCollection;
use crate::db::Database;
use crate::error::Fallible;
use crate::user_db::UserDatabase;

/// The rows belonging to `rc`, inside its owner's review database.
pub fn open_collection_db(rc: &ResolvedCollection) -> Fallible<Database> {
    Ok(UserDatabase::open(&rc.db_path)?.collection(rc.collection_id.clone()))
}
