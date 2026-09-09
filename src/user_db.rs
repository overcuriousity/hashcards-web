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

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use rusqlite::Connection;
use rusqlite::config::DbConfig;
use rusqlite::params;

use crate::db::Database;
use crate::db::SCHEMA_VERSION;
use crate::db::ensure_version_table;
use crate::db::get_schema_version;
use crate::db::probe_schema_exists;
use crate::db::set_schema_version;
use crate::error::Fallible;
use crate::error::fail;
use crate::types::card_hash::CardHash;
use crate::types::collection_id::CollectionId;
use crate::types::timestamp::Timestamp;

/// How long a connection waits for a lock held by another connection before
/// giving up with SQLITE_BUSY. WAL removes most of the contention this was
/// papering over, but a second writer still has to wait for the first.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// One user's review database: every collection in their card tree, in one
/// file at `{data_dir}/db/{tree-name}.db`.
///
/// The connection is shared rather than duplicated. A saved deck spanning
/// three collections opens three `Database` views, and after consolidation
/// all three name the same file — three write connections would contend on
/// every single grade, and would have to be locked in a fixed order to avoid
/// deadlocking. One connection behind one mutex has neither problem.
pub struct UserDatabase {
    conn: Arc<Mutex<Connection>>,
    /// Which file this is. Production code always already knows — it passed
    /// the path in — so only tests read it back.
    #[cfg_attr(not(test), allow(dead_code))]
    path: PathBuf,
}

impl UserDatabase {
    pub fn open(path: &Path) -> Fallible<Self> {
        let mut conn = Connection::open(path)?;
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_FKEY, true)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // Before the schema transaction: `pragma journal_mode` cannot change
        // inside one.
        set_wal(&conn, path)?;
        prepare_schema(&mut conn, path)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path: path.to_path_buf(),
        })
    }

    /// An empty user database in memory, at the current schema. Test-only:
    /// the server always has a file. No WAL — an in-memory database has no
    /// journal to write.
    #[cfg(test)]
    pub fn memory() -> Fallible<Self> {
        let mut conn = Connection::open_in_memory()?;
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_FKEY, true)?;
        prepare_schema(&mut conn, Path::new(":memory:"))?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path: PathBuf::from(":memory:"),
        })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A view of one collection on this database's connection.
    pub fn collection(&self, id: CollectionId) -> Database {
        Database::new_view(Arc::clone(&self.conn), id)
    }

    /// Move some cards' review history from one collection to another.
    ///
    /// This is what the per-user database bought: after consolidation both
    /// collections are rows in one file, so a deck moving between them is
    /// an update, not a transfer between two databases. The schema's
    /// `on update cascade` on `(collection_id, card_hash)` carries each
    /// card's reviews and bookmark across with it -- see the comment in
    /// `schema.sql`, which anticipated exactly this.
    ///
    /// A card the destination *already* has keeps the destination's
    /// schedule: identical cards in two collections are two schedules by
    /// design, and the one already in force where the card is going is the
    /// one that applies. The source row is dropped rather than overwriting
    /// it.
    ///
    /// One transaction, so a deck is never half-moved.
    pub fn move_cards(
        &self,
        from: &CollectionId,
        to: &CollectionId,
        hashes: &[CardHash],
    ) -> Fallible<usize> {
        if from == to || hashes.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let mut moved = 0;
        for hash in hashes {
            if card_row_exists(&tx, to, *hash)? {
                // The destination already schedules this card. Drop the
                // source's row rather than colliding with it.
                tx.execute(
                    "delete from cards where collection_id = ?1 and card_hash = ?2;",
                    params![from, hash],
                )?;
                continue;
            }
            moved += tx.execute(
                "update cards set collection_id = ?1 where collection_id = ?2 and card_hash = ?3;",
                params![to, from, hash],
            )?;
        }
        tx.commit()?;
        Ok(moved)
    }

    /// Close session rows left dangling by a crash or restart, across every
    /// collection in this database, and return how many were closed in each.
    ///
    /// A row is dangling when it has not been closed and its heartbeat has
    /// been silent since before `stale_before`. Each is closed at the time
    /// of its last surviving (non-voided) review, or left at `started_at` if
    /// no review was recorded.
    ///
    /// The heartbeat is what makes this safe to run while other processes
    /// are working: nothing in the row itself distinguishes a session
    /// abandoned by a crash from one that is simply mid-drill elsewhere.
    /// `closed` is the marker rather than `ended_at <> started_at`, because a
    /// session whose reviews were all undone is rewritten back to
    /// `started_at` and would otherwise be re-detected on every sweep,
    /// forever.
    ///
    /// The counts are collected inside the same transaction as the update,
    /// because afterwards there is nothing left to count.
    pub fn close_dangling_sessions(
        &self,
        stale_before: Timestamp,
    ) -> Fallible<HashMap<CollectionId, usize>> {
        self.with_connection(|conn| {
            let tx = conn.transaction()?;
            let mut counts = HashMap::new();
            {
                let sql = "select collection_id, count(*) from sessions \
                           where closed = 0 and coalesce(last_seen_at, started_at) < ? \
                           group by collection_id;";
                let mut stmt = tx.prepare(sql)?;
                let mut rows = stmt.query(params![stale_before])?;
                while let Some(row) = rows.next()? {
                    let id: CollectionId = row.get(0)?;
                    let n: i64 = row.get(1)?;
                    counts.insert(id, n as usize);
                }
            }
            tx.execute(
                "update sessions set ended_at = coalesce((select max(reviewed_at) from reviews \
                 where reviews.session_id = sessions.session_id and reviews.voided = 0), \
                 started_at), closed = 1 \
                 where closed = 0 and coalesce(last_seen_at, started_at) < ?;",
                params![stale_before],
            )?;
            tx.commit()?;
            Ok(counts)
        })
    }

    /// Run `f` against the connection. The only way in from outside this
    /// module, so that every caller takes the lock exactly once.
    pub(crate) fn with_connection<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> Fallible<T>,
    ) -> Fallible<T> {
        let mut conn = self.conn.lock();
        f(&mut conn)
    }
}

/// Ask for write-ahead logging, and settle for what the filesystem gives.
///
/// WAL lets the stats page read while a drill session writes, instead of
/// waiting out the busy timeout. It is unavailable on some NFS and SMB
/// mounts, where the pragma answers with the mode actually in force — so
/// this logs the divergence and continues. An optimisation must not refuse
/// to start the server.
fn set_wal(conn: &Connection, path: &Path) -> Fallible<()> {
    let mode: String = conn.query_row("pragma journal_mode = wal;", [], |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        log::warn!(
            "{} could not be opened in write-ahead logging mode (the filesystem gave `{mode}`). \
             hashcards will continue in that mode; reads may briefly wait on writes.",
            path.display()
        );
    }
    Ok(())
}

/// Create the schema, or check that the file already carries this exact
/// version. There is no ladder here on purpose: version 8 is the first user
/// database, and every earlier version is a per-collection file that the
/// startup merge — not this function — knows how to read.
fn prepare_schema(conn: &mut Connection, path: &Path) -> Fallible<()> {
    let tx = conn.transaction()?;
    if !probe_schema_exists(&tx)? {
        tx.execute_batch(include_str!("schema.sql"))?;
        set_schema_version(&tx, SCHEMA_VERSION)?;
        tx.commit()?;
        return Ok(());
    }
    ensure_version_table(&tx)?;
    let version = get_schema_version(&tx)?;
    if version > SCHEMA_VERSION {
        return fail(format!(
            "The review database at {} uses schema version {version}, but this version of \
             hashcards-web only supports up to schema version {SCHEMA_VERSION}. Please upgrade \
             hashcards-web.",
            path.display()
        ));
    }
    if version < SCHEMA_VERSION {
        return fail(format!(
            "The review database at {} is at schema version {version}, which is a per-collection \
             database from before hashcards-web kept one database per user. It should have been \
             merged at startup — see the log. Move it into `db/legacy/` and restart, or restore \
             the user's database from a backup.",
            path.display()
        ));
    }
    tx.commit()?;
    Ok(())
}

/// Does this collection already have a row for this card?
fn card_row_exists(
    tx: &rusqlite::Transaction<'_>,
    collection: &CollectionId,
    hash: CardHash,
) -> Fallible<bool> {
    let n: i64 = tx.query_row(
        "select count(*) from cards where collection_id = ?1 and card_hash = ?2;",
        params![collection, hash],
        |row| row.get(0),
    )?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::LEGACY_SCHEMA_VERSION;
    use crate::db::ReviewRecord;
    use crate::fsrs::Grade;
    use crate::types::card_hash::CardHash;

    fn version_of(path: &Path) -> Fallible<i64> {
        let conn = Connection::open(path)?;
        Ok(conn.query_row("select version from schema_version;", [], |row| row.get(0))?)
    }

    /// A fresh user database is created at version 8, and reopening it does
    /// not change it.
    #[test]
    fn a_fresh_user_database_is_created_at_version_8() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("someone.db");
        let db = UserDatabase::open(&path)?;
        assert_eq!(db.path(), path);
        assert_eq!(version_of(&path)?, SCHEMA_VERSION);
        drop(db);
        UserDatabase::open(&path)?;
        assert_eq!(version_of(&path)?, SCHEMA_VERSION);
        Ok(())
    }

    /// WAL is what stops the stats page's second connection waiting on a
    /// drill session's writer. It is an optimisation, so a filesystem that
    /// refuses it gets a log line and rollback journalling, not a dead
    /// server — but where it is available it must actually be set.
    #[test]
    fn a_user_database_is_opened_in_wal_mode() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("someone.db");
        let db = UserDatabase::open(&path)?;
        let mode: String = db.with_connection(|conn| {
            Ok(conn.query_row("pragma journal_mode;", [], |r| r.get(0))?)
        })?;
        assert_eq!(mode.to_lowercase(), "wal");
        Ok(())
    }

    /// A database from a future release is refused with a message that says
    /// what to do, rather than mangled by a ladder that does not know its
    /// shape.
    #[test]
    fn a_future_schema_version_is_refused() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("future.db");
        {
            let conn = Connection::open(&path)?;
            conn.execute_batch(include_str!("schema.sql"))?;
            conn.execute("insert into schema_version (version) values (999);", [])?;
        }
        let message = match UserDatabase::open(&path) {
            Ok(_) => return fail("a version-999 database must be refused"),
            Err(e) => e.to_string(),
        };
        assert!(message.contains("999"), "unhelpful error: {message}");
        assert!(message.contains("upgrade"), "unhelpful error: {message}");
        Ok(())
    }

    /// The sweep runs once per user, but the notice is shown on a collection
    /// page — so it has to come back split by collection. Keyed by database
    /// path it could not be: every collection in a tree shares one file.
    #[test]
    fn dangling_sessions_are_closed_and_counted_per_collection() -> Fallible<()> {
        let user = UserDatabase::memory()?;
        let bio = CollectionId::new("bio")?;
        let esp = CollectionId::new("esp")?;
        let old = Timestamp::try_from("2026-01-01T09:00:00.000".to_string())?;
        user.collection(bio.clone()).create_session(old)?;
        user.collection(bio.clone()).create_session(old)?;
        user.collection(esp.clone()).create_session(old)?;

        let cutoff = Timestamp::try_from("2026-06-01T09:00:00.000".to_string())?;
        let closed = user.close_dangling_sessions(cutoff)?;
        assert_eq!(closed.get(&bio), Some(&2));
        assert_eq!(closed.get(&esp), Some(&1));

        // A second sweep finds nothing: `closed` is the marker.
        assert!(user.close_dangling_sessions(cutoff)?.is_empty());
        Ok(())
    }

    /// A review record for `card_hash` at `when`. The sweep closes a session
    /// at its last surviving review, so these tests need real review rows.
    fn review_at(card_hash: CardHash, when: Timestamp) -> ReviewRecord {
        ReviewRecord {
            card_hash,
            reviewed_at: when,
            grade: Grade::Good,
            stability: 2.0,
            difficulty: 2.0,
            interval_raw: 1.0,
            interval_days: 1,
            due_date: when.date(),
            duration_ms: None,
        }
    }

    /// FEAT-03: sessions whose heartbeat has been silent since before the
    /// cutoff are dangling; closing them uses the last surviving review's
    /// time, or the start time when no review was recorded.
    #[test]
    fn dangling_sessions_close_at_their_last_review() -> Fallible<()> {
        let user = UserDatabase::memory()?;
        let id = CollectionId::new("bio")?;
        let db = user.collection(id.clone());
        let t0 = Timestamp::try_from("2026-01-01T10:00:00.000".to_string())?;
        let t1 = Timestamp::try_from("2026-01-01T10:05:00.000".to_string())?;

        // A properly closed session must be left untouched.
        let closed_session = db.create_session(t0)?;
        db.close_session(closed_session, t1)?;

        // A dangling session with one review.
        let card_hash = CardHash::hash_bytes(b"a");
        db.insert_card(card_hash, t0)?;
        let dangling = db.create_session(t0)?;
        db.insert_review_immediately(dangling, &review_at(card_hash, t1))?;

        // A dangling session with no reviews at all.
        let empty_dangling = db.create_session(t1)?;

        // A cutoff after every heartbeat above, so all stale rows qualify.
        let cutoff = Timestamp::try_from("2026-01-01T11:00:00.000".to_string())?;
        assert_eq!(user.close_dangling_sessions(cutoff)?.get(&id), Some(&2));

        let sessions = db.get_all_sessions()?;
        let find = |session_id: i64| {
            sessions
                .iter()
                .find(|s| s.session_id == session_id)
                .ok_or_else(|| crate::error::ErrorReport::new("session row missing"))
        };
        assert_eq!(
            find(dangling)?.ended_at,
            t1,
            "closed at its last review time"
        );
        assert_eq!(
            find(empty_dangling)?.ended_at,
            t1,
            "closed at its start time"
        );
        assert_eq!(
            find(closed_session)?.ended_at,
            t1,
            "already-closed row untouched"
        );

        // Running it again closes nothing: the sweep marks rows `closed`
        // rather than inferring it from `ended_at <> started_at`. A session
        // closed at its own start time — one with no reviews, or one whose
        // reviews were all undone — used to be indistinguishable from the
        // placeholder and was re-detected on every single sweep, forever.
        assert!(user.close_dangling_sessions(cutoff)?.is_empty());
        assert_eq!(find(empty_dangling)?.ended_at, t1);
        Ok(())
    }

    /// A session whose heartbeat is recent is still running somewhere — a
    /// second server may share the file — and must not be closed. Stamping
    /// `ended_at` on it left the live session appending reviews to a row
    /// claiming to have ended.
    #[test]
    fn a_live_session_is_not_swept() -> Fallible<()> {
        let user = UserDatabase::memory()?;
        let id = CollectionId::new("bio")?;
        let db = user.collection(id.clone());
        let t0 = Timestamp::try_from("2026-01-01T10:00:00.000".to_string())?;
        let crashed = db.create_session(t0)?;
        let live = db.create_session(t0)?;

        // The live session has just checked in; the crashed one never did.
        let now = Timestamp::try_from("2026-01-01T12:00:00.000".to_string())?;
        db.touch_session(live, now)?;

        // Anything silent for over an hour is presumed dead.
        let cutoff = now.minus_minutes(60);
        assert_eq!(user.close_dangling_sessions(cutoff)?.get(&id), Some(&1));

        let sessions = db.get_all_sessions()?;
        let find = |session_id: i64| {
            sessions
                .iter()
                .find(|s| s.session_id == session_id)
                .ok_or_else(|| crate::error::ErrorReport::new("session row missing"))
        };
        assert_eq!(find(crashed)?.ended_at, t0, "the crashed session is closed");
        assert_eq!(
            find(live)?.ended_at,
            t0,
            "the live session's row is left open"
        );
        // It is not protected forever: once its heartbeat falls behind a
        // later cutoff, it is swept like any other abandoned session.
        let much_later = Timestamp::try_from("2026-01-01T14:00:00.000".to_string())?;
        assert_eq!(
            user.close_dangling_sessions(much_later.minus_minutes(60))?
                .get(&id),
            Some(&1)
        );
        Ok(())
    }

    /// A per-collection database from before consolidation must never be
    /// opened as a user database: its rows carry no collection, so every
    /// scoped query would silently match nothing. The merge is the only
    /// thing allowed to read one.
    #[test]
    fn a_legacy_per_collection_database_is_refused() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("a1b2c3d4.db");
        crate::db::test_support::create_legacy_v7(&path)?;
        let message = match UserDatabase::open(&path) {
            Ok(_) => return fail("a per-collection database must be refused"),
            Err(e) => e.to_string(),
        };
        assert!(
            message.contains(&LEGACY_SCHEMA_VERSION.to_string()),
            "the error must name the version it found: {message}"
        );
        assert!(
            message.contains("legacy"),
            "the error must point at where the file went: {message}"
        );
        Ok(())
    }

    /// Moving a deck between collections is an update, not a transfer
    /// between two database files -- which is what the per-user database
    /// bought, and the reason the MCP has a move_decks tool at all.
    #[test]
    fn moving_a_card_carries_its_reviews_across() -> Fallible<()> {
        let user = UserDatabase::memory()?;
        let from = CollectionId::new("aaaaaaaa")?;
        let to = CollectionId::new("bbbbbbbb")?;
        let hash = CardHash::hash_bytes(b"a card");
        let now = Timestamp::now();

        let source = user.collection(from.clone());
        source.insert_card(hash, now)?;
        let session = source.create_session(now)?;
        source.insert_review_immediately(
            session,
            &ReviewRecord {
                card_hash: hash,
                reviewed_at: now,
                grade: Grade::Good,
                stability: 1.0,
                difficulty: 2.0,
                interval_raw: 1.0,
                interval_days: 1,
                due_date: now.date(),
                duration_ms: None,
            },
        )?;

        assert_eq!(user.move_cards(&from, &to, &[hash])?, 1);

        assert!(user.collection(from).card_hashes()?.is_empty());
        let dest = user.collection(to);
        assert!(dest.card_hashes()?.contains(&hash));
        assert_eq!(
            dest.reviews_for_card(hash)?.len(),
            1,
            "on update cascade did not carry the reviews across"
        );
        Ok(())
    }

    /// Identical cards in two collections are two schedules by design, so
    /// the destination's own schedule is the one that applies there.
    #[test]
    fn moving_a_card_the_destination_already_has_keeps_the_destinations_schedule() -> Fallible<()> {
        let user = UserDatabase::memory()?;
        let from = CollectionId::new("aaaaaaaa")?;
        let to = CollectionId::new("bbbbbbbb")?;
        let hash = CardHash::hash_bytes(b"a card");
        let now = Timestamp::now();

        user.collection(from.clone()).insert_card(hash, now)?;
        user.collection(to.clone()).insert_card(hash, now)?;

        // Nothing moved, and nothing collided.
        assert_eq!(user.move_cards(&from, &to, &[hash])?, 0);
        assert!(user.collection(from).card_hashes()?.is_empty());
        assert!(user.collection(to).card_hashes()?.contains(&hash));
        Ok(())
    }

    #[test]
    fn moving_a_card_to_where_it_already_is_does_nothing() -> Fallible<()> {
        let user = UserDatabase::memory()?;
        let id = CollectionId::new("aaaaaaaa")?;
        let hash = CardHash::hash_bytes(b"a card");
        user.collection(id.clone())
            .insert_card(hash, Timestamp::now())?;
        assert_eq!(user.move_cards(&id, &id, &[hash])?, 0);
        assert!(user.collection(id).card_hashes()?.contains(&hash));
        Ok(())
    }
}
