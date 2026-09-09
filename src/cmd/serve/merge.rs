//! Fold the per-collection review databases of a pre-consolidation install
//! into one database per user.
//!
//! Runs once at startup, before the dangling-session sweep, and is
//! idempotent: each source is recorded in the target's `meta` table inside
//! the same transaction as its rows, so a crash between the commit and the
//! file move cannot import anything twice. Nothing is deleted — merged
//! sources are moved into `db/legacy/`, where an operator can still read
//! them and where an older binary will not find them and write reviews into
//! a file the next upgrade would skip.

use std::collections::HashMap;
use std::fs::read_dir;
use std::fs::rename;
use std::path::Path;
use std::path::PathBuf;

use rusqlite::Connection;
use rusqlite::Transaction;
use rusqlite::params;

use crate::cmd::serve::cards::existing_collection_id;
use crate::db::open_legacy_source;
use crate::error::ErrorReport;
use crate::error::Fallible;
use crate::error::fail;
use crate::types::collection_id::CollectionId;
use crate::types::timestamp::Timestamp;
use crate::user_db::UserDatabase;
use crate::utils::ensure_dir;

/// One legacy database, and the collection whose rows it holds.
struct Source {
    id: CollectionId,
    path: PathBuf,
}

/// Merge every tree's per-collection databases into its user database.
///
/// Returns the trees whose merge failed, keyed by the target database path
/// and carrying the message to show whoever tries to use it. A failure is
/// confined to its own tree: every other user is served normally and the
/// server starts, because the alternative — refusing to start — takes an
/// instance down for everybody over one broken file.
pub fn merge_legacy_databases(data_dir: &Path) -> HashMap<PathBuf, String> {
    let db_dir = data_dir.join("db");
    let trees_dir = data_dir.join("cards");
    let mut failures = HashMap::new();

    let entries = match read_dir(&trees_dir) {
        Ok(entries) => entries,
        // No tree yet is the ordinary state of a fresh install.
        Err(_) => return failures,
    };

    let mut claimed: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let tree = entry.path();
        if !tree.is_dir() || tree.is_symlink() {
            continue;
        }
        let target = match target_path(&tree, &db_dir) {
            Ok(target) => target,
            Err(e) => {
                log::error!("Skipping the card tree at {}: {e}", tree.display());
                continue;
            }
        };
        claimed.push(target.clone());
        if let Err(e) = merge_tree(&tree, &db_dir, &target, &mut claimed) {
            log::error!(
                "Could not consolidate the review databases for the card tree at {}: {e}",
                tree.display()
            );
            failures.insert(target, e.to_string());
        }
    }

    report_orphans(&db_dir, &claimed);
    failures
}

/// `db/{tree-name}.db`. No collection id can collide with a tree name: ids
/// are eight hex characters, and a tree is named `default` or
/// `{email-slug}-{8 hex}`.
fn target_path(tree: &Path, db_dir: &Path) -> Fallible<PathBuf> {
    let name = tree
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| ErrorReport::new("the card tree has no readable name"))?;
    Ok(db_dir.join(format!("{name}.db")))
}

/// Merge one tree. Every source is imported in a single transaction: a user
/// is either fully consolidated or untouched.
fn merge_tree(
    tree: &Path,
    db_dir: &Path,
    target: &Path,
    claimed: &mut Vec<PathBuf>,
) -> Fallible<()> {
    let sources = collect_sources(tree, db_dir, target)?;
    claimed.extend(sources.iter().map(|s| s.path.clone()));
    if sources.is_empty() {
        // A user who signed up after the upgrade. Their database is created
        // on first use, like everyone else's.
        return Ok(());
    }
    ensure_dir(db_dir, "review database directory")?;
    let db = UserDatabase::open(target)?;

    db.with_connection(|conn| {
        let tx = conn.transaction()?;
        for source in &sources {
            if already_merged(&tx, &source.id)? {
                continue;
            }
            let legacy = open_legacy_source(&source.path)?;
            import(&tx, &legacy, &source.id)?;
            mark_merged(&tx, &source.id)?;
        }
        tx.commit()?;
        Ok(())
    })?;

    // Only after the commit. A crash in between leaves the sources where
    // they are and the next start skips them by marker.
    for source in &sources {
        set_aside(&source.path, db_dir)?;
    }
    Ok(())
}

/// The legacy databases belonging to the collections in this tree.
///
/// Reads ids but never mints one: startup must not write into a user's tree,
/// and a folder with no id has no database to find.
fn collect_sources(tree: &Path, db_dir: &Path, target: &Path) -> Fallible<Vec<Source>> {
    let mut sources = Vec::new();
    for entry in read_dir(tree)?.flatten() {
        let folder = entry.path();
        if !folder.is_dir() || folder.is_symlink() {
            continue;
        }
        match folder.file_name().and_then(|n| n.to_str()) {
            Some(n) if !n.starts_with('.') => {}
            _ => continue,
        }
        let Some(id) = existing_collection_id(&folder)? else {
            continue;
        };
        let path = db_dir.join(format!("{id}.db"));
        // Belt and braces: a source that *is* the target would be imported
        // into itself.
        if !path.is_file() || path == target {
            continue;
        }
        sources.push(Source { id, path });
    }
    Ok(sources)
}

fn marker(id: &CollectionId) -> String {
    format!("merged:{id}")
}

fn already_merged(tx: &Transaction, id: &CollectionId) -> Fallible<bool> {
    let sql = "select count(*) from meta where key = ?;";
    let count: i64 = tx.query_row(sql, params![marker(id)], |row| row.get(0))?;
    Ok(count > 0)
}

fn mark_merged(tx: &Transaction, id: &CollectionId) -> Fallible<()> {
    tx.execute(
        "insert into meta (key, value) values (?, ?) on conflict (key) do nothing;",
        params![marker(id), Timestamp::now()],
    )?;
    Ok(())
}

/// Copy one source's four tables into the target, scoped to its collection.
///
/// Cards first: reviews and bookmarks reference them, and foreign keys are
/// enforced. Sessions are renumbered, because every source numbers its first
/// session 1, and their reviews follow through a map held here rather than
/// in SQL — the alternative is relying on rowid assignment order, which is
/// true today and is not a promise.
fn import(tx: &Transaction, src: &Connection, id: &CollectionId) -> Fallible<()> {
    import_cards(tx, src, id)?;
    let sessions = import_sessions(tx, src, id)?;
    import_reviews(tx, src, id, &sessions)?;
    import_bookmarks(tx, src, id)?;
    Ok(())
}

fn import_cards(tx: &Transaction, src: &Connection, id: &CollectionId) -> Fallible<()> {
    let mut stmt = src.prepare(
        "select card_hash, added_at, last_reviewed_at, stability, difficulty, interval_raw, \
         interval_days, due_date, review_count from cards;",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        tx.execute(
            "insert into cards (collection_id, card_hash, added_at, last_reviewed_at, stability, \
             difficulty, interval_raw, interval_days, due_date, review_count) \
             values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?);",
            params![
                id,
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<f64>>(3)?,
                row.get::<_, Option<f64>>(4)?,
                row.get::<_, Option<f64>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, i64>(8)?,
            ],
        )?;
    }
    Ok(())
}

fn import_sessions(
    tx: &Transaction,
    src: &Connection,
    id: &CollectionId,
) -> Fallible<HashMap<i64, i64>> {
    let mut map = HashMap::new();
    let mut stmt = src.prepare(
        "select session_id, started_at, ended_at, last_seen_at, closed from sessions \
         order by session_id;",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let old: i64 = row.get(0)?;
        let new: i64 = tx.query_row(
            "insert into sessions (collection_id, started_at, ended_at, last_seen_at, closed) \
             values (?, ?, ?, ?, ?) returning session_id;",
            params![
                id,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
            ],
            |r| r.get(0),
        )?;
        map.insert(old, new);
    }
    Ok(map)
}

fn import_reviews(
    tx: &Transaction,
    src: &Connection,
    id: &CollectionId,
    sessions: &HashMap<i64, i64>,
) -> Fallible<()> {
    let mut stmt = src.prepare(
        "select session_id, card_hash, reviewed_at, grade, stability, difficulty, interval_raw, \
         interval_days, due_date, duration_ms, voided from reviews order by review_id;",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let old: i64 = row.get(0)?;
        let Some(new) = sessions.get(&old) else {
            return fail(format!(
                "a review names session {old}, which does not exist in this database"
            ));
        };
        // `review_id` is not carried over: it collides between sources, and
        // nothing outside a live session refers to one.
        tx.execute(
            "insert into reviews (session_id, collection_id, card_hash, reviewed_at, grade, \
             stability, difficulty, interval_raw, interval_days, due_date, duration_ms, voided) \
             values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);",
            params![
                new,
                id,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, f64>(4)?,
                row.get::<_, f64>(5)?,
                row.get::<_, f64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, Option<i64>>(9)?,
                row.get::<_, i64>(10)?,
            ],
        )?;
    }
    Ok(())
}

fn import_bookmarks(tx: &Transaction, src: &Connection, id: &CollectionId) -> Fallible<()> {
    let mut stmt = src.prepare("select card_hash, note, created_at from bookmarks;")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        tx.execute(
            "insert into bookmarks (collection_id, card_hash, note, created_at) \
             values (?, ?, ?, ?);",
            params![
                id,
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
            ],
        )?;
    }
    Ok(())
}

/// Move a merged source into `db/legacy/`, with the write-ahead log and
/// shared-memory files SQLite leaves beside it.
///
/// Moved rather than left in place: an older binary would find a file left
/// where it was, write reviews into it, and the next upgrade would skip
/// those reviews on the `merged:` marker and lose them without a word.
fn set_aside(path: &Path, db_dir: &Path) -> Fallible<()> {
    let legacy = db_dir.join("legacy");
    ensure_dir(&legacy, "directory for superseded review databases")?;
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return fail(format!("{} has no readable name", path.display()));
    };
    for suffix in ["", "-wal", "-shm"] {
        let from = path.with_file_name(format!("{name}{suffix}"));
        if from.exists() {
            rename(&from, legacy.join(format!("{name}{suffix}")))?;
        }
    }
    Ok(())
}

/// Log, once, every `db/*.db` that no collection in any tree claims.
///
/// It cannot be attributed to a user, so it cannot be merged into one — the
/// usual cause is a collection folder deleted from outside hashcards. It is
/// left exactly where it is.
fn report_orphans(db_dir: &Path, claimed: &[PathBuf]) {
    let Ok(entries) = read_dir(db_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("db") {
            continue;
        }
        if claimed.contains(&path) {
            continue;
        }
        log::info!(
            "{} belongs to no collection in any card tree, so it cannot be attributed to a user. \
             Left untouched.",
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::cmd::serve::cards::COLLECTION_META_FILE;
    use crate::db::test_support::create_legacy_v0;
    use crate::db::test_support::create_legacy_v7;

    /// A `default` tree with two collections carrying fixed ids. Returns the
    /// tree name and the two ids.
    fn two_collections(dir: &Path) -> Fallible<(String, String, String)> {
        let tree = "default";
        for (name, id) in [("Biology", "aaaa1111"), ("Spanish", "bbbb2222")] {
            let folder = dir.join("cards").join(tree).join(name);
            std::fs::create_dir_all(&folder)?;
            std::fs::write(
                folder.join(COLLECTION_META_FILE),
                format!("id = \"{id}\"\n"),
            )?;
        }
        std::fs::create_dir_all(dir.join("db"))?;
        Ok((
            tree.to_string(),
            "aaaa1111".to_string(),
            "bbbb2222".to_string(),
        ))
    }

    /// One card, one session, `reviews` reviews and a bookmark, in a legacy
    /// database.
    fn seed(conn: &Connection, hash: &str, due: &str, reviews: usize) -> Fallible<()> {
        conn.execute(
            "insert into cards (card_hash, added_at, due_date, review_count) values (?, ?, ?, ?);",
            rusqlite::params![hash, "2026-01-01T09:00:00.000", due, reviews as i64],
        )?;
        conn.execute(
            "insert into sessions (session_id, started_at, ended_at) values (1, ?, ?);",
            rusqlite::params!["2026-01-01T09:00:00.000", "2026-01-01T09:30:00.000"],
        )?;
        for n in 0..reviews {
            conn.execute(
                "insert into reviews (session_id, card_hash, reviewed_at, grade, stability, \
                 difficulty, interval_raw, interval_days, due_date) \
                 values (1, ?, ?, 'good', 2.0, 5.0, 2.0, 2, ?);",
                rusqlite::params![hash, format!("2026-01-01T09:0{n}:00.000"), due],
            )?;
        }
        conn.execute(
            "insert into bookmarks (card_hash, note, created_at) values (?, 'keep', ?);",
            rusqlite::params![hash, "2026-01-01T09:00:00.000"],
        )?;
        Ok(())
    }

    fn count(conn: &Connection, sql: &str) -> Fallible<i64> {
        Ok(conn.query_row(sql, [], |row| row.get(0))?)
    }

    fn source(dir: &Path, id: &str) -> PathBuf {
        dir.join("db").join(format!("{id}.db"))
    }

    #[test]
    fn two_collections_merge_keeping_counts_dates_and_bookmarks() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let data_dir = dir.path();
        let (tree, one, two) = two_collections(data_dir)?;
        seed(
            &create_legacy_v7(&source(data_dir, &one))?,
            "hash-one",
            "2026-02-01",
            2,
        )?;
        seed(
            &create_legacy_v7(&source(data_dir, &two))?,
            "hash-two",
            "2026-03-01",
            1,
        )?;

        let failures = merge_legacy_databases(data_dir);
        assert!(failures.is_empty(), "{failures:?}");

        let conn = Connection::open(data_dir.join("db").join(format!("{tree}.db")))?;
        assert_eq!(count(&conn, "select count(*) from cards;")?, 2);
        assert_eq!(count(&conn, "select count(*) from reviews;")?, 3);
        assert_eq!(count(&conn, "select count(*) from bookmarks;")?, 2);
        let due: String = conn.query_row(
            "select due_date from cards where collection_id = ? and card_hash = 'hash-one';",
            rusqlite::params![one],
            |row| row.get(0),
        )?;
        assert_eq!(due, "2026-02-01");
        let reviewed: i64 = conn.query_row(
            "select review_count from cards where collection_id = ? and card_hash = 'hash-one';",
            rusqlite::params![one],
            |row| row.get(0),
        )?;
        assert_eq!(reviewed, 2);
        Ok(())
    }

    /// Both sources number their first session 1. The target must renumber
    /// them and keep each review attached to the session it happened in.
    #[test]
    fn colliding_session_ids_are_renumbered_and_reviews_follow() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let data_dir = dir.path();
        let (tree, one, two) = two_collections(data_dir)?;
        seed(
            &create_legacy_v7(&source(data_dir, &one))?,
            "hash-one",
            "2026-02-01",
            2,
        )?;
        seed(
            &create_legacy_v7(&source(data_dir, &two))?,
            "hash-two",
            "2026-03-01",
            1,
        )?;

        assert!(merge_legacy_databases(data_dir).is_empty());

        let conn = Connection::open(data_dir.join("db").join(format!("{tree}.db")))?;
        assert_eq!(count(&conn, "select count(*) from sessions;")?, 2);
        assert_eq!(
            count(&conn, "select count(distinct session_id) from reviews;")?,
            2
        );
        // Every review sits in a session belonging to its own collection.
        assert_eq!(
            count(
                &conn,
                "select count(*) from reviews r join sessions s on s.session_id = r.session_id \
                 where s.collection_id <> r.collection_id;"
            )?,
            0
        );
        Ok(())
    }

    #[test]
    fn a_second_run_imports_nothing_and_duplicates_nothing() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let data_dir = dir.path();
        let (tree, one, _two) = two_collections(data_dir)?;
        seed(
            &create_legacy_v7(&source(data_dir, &one))?,
            "hash-one",
            "2026-02-01",
            2,
        )?;

        assert!(merge_legacy_databases(data_dir).is_empty());
        // Put the source back, to prove the marker is what makes this
        // idempotent rather than the file having moved.
        std::fs::rename(
            data_dir.join("db").join("legacy").join(format!("{one}.db")),
            source(data_dir, &one),
        )?;
        assert!(merge_legacy_databases(data_dir).is_empty());

        let conn = Connection::open(data_dir.join("db").join(format!("{tree}.db")))?;
        assert_eq!(count(&conn, "select count(*) from cards;")?, 1);
        assert_eq!(count(&conn, "select count(*) from reviews;")?, 2);
        Ok(())
    }

    #[test]
    fn a_source_from_before_the_version_table_is_lifted_then_merged() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let data_dir = dir.path();
        let (tree, one, _two) = two_collections(data_dir)?;
        {
            let src = create_legacy_v0(&source(data_dir, &one))?;
            src.execute(
                "insert into cards (card_hash, added_at, due_date, review_count) \
                 values ('hash-one', '2026-01-01T09:00:00.000', '2026-02-01', 1);",
                [],
            )?;
            src.execute(
                "insert into sessions (session_id, started_at, ended_at) \
                 values (1, '2026-01-01T09:00:00.000', '2026-01-01T09:30:00.000');",
                [],
            )?;
            src.execute(
                "insert into reviews (session_id, card_hash, reviewed_at, grade, stability, \
                 difficulty, interval_raw, interval_days, due_date) values \
                 (1, 'hash-one', '2026-01-01T09:00:00.000', 'good', 2.0, 5.0, 2.0, 2, \
                 '2026-02-01');",
                [],
            )?;
        }

        assert!(merge_legacy_databases(data_dir).is_empty());

        let conn = Connection::open(data_dir.join("db").join(format!("{tree}.db")))?;
        assert_eq!(count(&conn, "select count(*) from reviews;")?, 1);
        let date: String =
            conn.query_row("select reviewed_date from reviews;", [], |r| r.get(0))?;
        assert_eq!(date, "2026-01-01");
        Ok(())
    }

    /// One user's broken database must not stop the others being served.
    #[test]
    fn a_corrupt_source_fails_only_its_own_tree() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let data_dir = dir.path();
        // A tree whose only database is not a database at all.
        let broken = data_dir.join("cards").join("broken").join("Biology");
        std::fs::create_dir_all(&broken)?;
        std::fs::write(broken.join(COLLECTION_META_FILE), "id = \"cccc3333\"\n")?;
        std::fs::create_dir_all(data_dir.join("db"))?;
        std::fs::write(source(data_dir, "cccc3333"), b"not a database")?;
        // And a tree that is fine.
        let (tree, one, _two) = two_collections(data_dir)?;
        seed(
            &create_legacy_v7(&source(data_dir, &one))?,
            "hash-one",
            "2026-02-01",
            1,
        )?;

        let failures = merge_legacy_databases(data_dir);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures.contains_key(&data_dir.join("db").join("broken.db")));

        let conn = Connection::open(data_dir.join("db").join(format!("{tree}.db")))?;
        assert_eq!(count(&conn, "select count(*) from cards;")?, 1);
        Ok(())
    }

    #[test]
    fn merged_sources_are_moved_to_legacy_and_stay_readable() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let data_dir = dir.path();
        let (_tree, one, _two) = two_collections(data_dir)?;
        seed(
            &create_legacy_v7(&source(data_dir, &one))?,
            "hash-one",
            "2026-02-01",
            1,
        )?;

        assert!(merge_legacy_databases(data_dir).is_empty());

        assert!(!source(data_dir, &one).exists());
        let moved = data_dir.join("db").join("legacy").join(format!("{one}.db"));
        assert!(moved.exists(), "the source must be kept, not deleted");
        let conn = Connection::open(&moved)?;
        assert_eq!(count(&conn, "select count(*) from cards;")?, 1);
        Ok(())
    }

    /// A database no collection in any tree claims cannot be attributed to a
    /// user, so it cannot be merged into one. Leave it exactly where it is.
    #[test]
    fn an_orphan_database_is_left_untouched() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let data_dir = dir.path();
        let (_tree, one, _two) = two_collections(data_dir)?;
        seed(
            &create_legacy_v7(&source(data_dir, &one))?,
            "hash-one",
            "2026-02-01",
            1,
        )?;
        let orphan = source(data_dir, "deadbeef");
        create_legacy_v7(&orphan)?;

        assert!(merge_legacy_databases(data_dir).is_empty());

        assert!(
            orphan.exists(),
            "an unattributable database must be left alone"
        );
        assert!(
            !data_dir
                .join("db")
                .join("legacy")
                .join("deadbeef.db")
                .exists()
        );
        Ok(())
    }

    /// A user who signs up after the upgrade has no sources at all, and must
    /// not have an empty database materialised here: it is created on first
    /// use, like everyone else's.
    #[test]
    fn a_tree_with_no_sources_is_left_alone() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let data_dir = dir.path();
        let folder = data_dir.join("cards").join("default").join("Biology");
        std::fs::create_dir_all(&folder)?;
        std::fs::write(folder.join(COLLECTION_META_FILE), "id = \"aaaa1111\"\n")?;
        std::fs::create_dir_all(data_dir.join("db"))?;

        assert!(merge_legacy_databases(data_dir).is_empty());
        assert!(!data_dir.join("db").join("default.db").exists());
        Ok(())
    }
}
