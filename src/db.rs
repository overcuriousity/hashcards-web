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
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use rusqlite::Connection;
use rusqlite::Transaction;
use rusqlite::config::DbConfig;
use rusqlite::params;

use crate::error::Fallible;
use crate::error::fail;
use crate::fsrs::Difficulty;
use crate::fsrs::Grade;
use crate::fsrs::Stability;
use crate::types::card_hash::CardHash;
use crate::types::collection_id::CollectionId;
use crate::types::date::Date;
use crate::types::performance::Performance;
use crate::types::performance::ReviewedPerformance;
use crate::types::timestamp::Timestamp;

/// A view of one collection inside a user's review database.
///
/// Scoped, not separate: the connection is shared with every other view on
/// the same file, so a saved deck spanning three collections does not open
/// three writers that contend on every grade.
pub struct Database {
    conn: Arc<Mutex<Connection>>,
    collection: CollectionId,
}

pub struct ReviewRecord {
    pub card_hash: CardHash,
    pub reviewed_at: Timestamp,
    pub grade: Grade,
    pub stability: f64,
    pub difficulty: f64,
    pub interval_raw: f64,
    pub interval_days: i64,
    pub due_date: Date,
    pub duration_ms: Option<i64>,
}

/// Outcome of [`Database::apply_edit_migration`]. See that method's doc
/// comment for what does and doesn't count toward each field.
#[derive(Debug, PartialEq, Eq)]
pub struct EditMigrationCounts {
    pub renamed: usize,
    pub collided: usize,
}

pub struct SessionRow {
    pub session_id: i64,
    pub started_at: Timestamp,
    pub ended_at: Timestamp,
}

pub struct ReviewRow {
    pub review_id: i64,
    pub data: ReviewRecord,
}

/// Number of non-voided reviews per grade, across all time.
#[derive(Debug, PartialEq, Eq)]
pub struct GradeDistribution {
    pub forgot: usize,
    pub hard: usize,
    pub good: usize,
    pub easy: usize,
}

pub struct Bookmark {
    pub card_hash: CardHash,
    pub note: Option<String>,
    pub created_at: Timestamp,
}

/// The schema version of a user database — one file per user, every table
/// scoped by `collection_id`. Created by `UserDatabase`, never by the
/// migration ladder: consolidation merges N files into one, which an
/// in-place `alter table` cannot express.
pub const SCHEMA_VERSION: i64 = 8;

/// The highest version a per-collection database can be at, and the top of
/// the `migrate_legacy` ladder. Deployed databases sit between 0 and here.
pub const LEGACY_SCHEMA_VERSION: i64 = 7;

/// How long a connection waits for a lock held by another connection before
/// giving up with SQLITE_BUSY.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Shared by `Database::card_exists` and `apply_edit_migration`, which needs
/// it inside a `Transaction` rather than on `&self`.
fn card_exists_in(
    conn: &Connection,
    collection: &CollectionId,
    card_hash: CardHash,
) -> Fallible<bool> {
    let sql = "select count(*) from cards where collection_id = ? and card_hash = ?;";
    let count: i64 = conn.query_row(sql, params![collection, card_hash], |row| row.get(0))?;
    Ok(count > 0)
}

/// Shared by `Database::insert_card_if_new` and `apply_edit_migration`, which
/// needs it inside a `Transaction` rather than on `&self`.
fn insert_card_if_new_in(
    conn: &Connection,
    collection: &CollectionId,
    card_hash: CardHash,
    added_at: Timestamp,
) -> Fallible<()> {
    let sql = "insert into cards (collection_id, card_hash, added_at, review_count) \
               values (?, ?, ?, 0) on conflict (collection_id, card_hash) do nothing;";
    conn.execute(sql, params![collection, card_hash, added_at])?;
    Ok(())
}

impl Database {
    /// Only `UserDatabase::collection` calls this: a view must never be
    /// built on a connection whose schema has not been checked.
    pub(crate) fn new_view(conn: Arc<Mutex<Connection>>, collection: CollectionId) -> Self {
        Self { conn, collection }
    }

    /// An empty single-collection database in memory. Test-only.
    #[cfg(test)]
    pub fn memory() -> Fallible<Self> {
        Ok(crate::user_db::UserDatabase::memory()?
            .collection(CollectionId::new("test-collection")?))
    }

    /// Insert a new card in this collection.
    ///
    /// If a card with the given hash exists *in this collection*, returns an
    /// error. The same hash in another collection is a different card here:
    /// two collections keep two schedules.
    pub fn insert_card(&self, card_hash: CardHash, added_at: Timestamp) -> Fallible<()> {
        let conn = self.conn.lock();
        if card_exists_in(&conn, &self.collection, card_hash)? {
            return fail("Card already exists");
        }
        let sql = "insert into cards (collection_id, card_hash, added_at, review_count) \
                   values (?, ?, ?, 0);";
        conn.execute(sql, params![self.collection, card_hash, added_at])?;
        Ok(())
    }

    /// Return the set of all card hashes in the database.
    pub fn card_hashes(&self) -> Fallible<HashSet<CardHash>> {
        let conn = self.conn.lock();
        let sql = "select card_hash from cards where collection_id = ?;";
        let mut stmt = conn.prepare(sql)?;
        let card_iter = stmt.query_map(params![self.collection], |row| {
            let card_hash: CardHash = row.get(0)?;
            Ok(card_hash)
        })?;
        let mut card_hashes = HashSet::new();
        for card in card_iter {
            card_hashes.insert(card?);
        }
        Ok(card_hashes)
    }

    /// Find the hashes of the cards due today.
    pub fn due_today(&self, today: Date) -> Fallible<HashSet<CardHash>> {
        let conn = self.conn.lock();
        let mut due = HashSet::new();
        let sql = "select card_hash, due_date from cards where collection_id = ?;";
        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt.query(params![self.collection])?;
        while let Some(row) = rows.next()? {
            let hash: CardHash = row.get(0)?;
            let due_date: Option<Date> = row.get(1)?;
            match due_date {
                None => {
                    // Never reviewed, so it's due.
                    due.insert(hash);
                }
                Some(due_date) => {
                    if due_date <= today {
                        due.insert(hash);
                    }
                }
            }
        }
        Ok(due)
    }

    /// Get a card's performance information.
    pub fn get_card_performance_opt(&self, card_hash: CardHash) -> Fallible<Option<Performance>> {
        let conn = self.conn.lock();
        let sql = "select last_reviewed_at, stability, difficulty, interval_raw, interval_days, \
                   due_date, review_count from cards where collection_id = ? and card_hash = ?;";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![self.collection, card_hash], |row| {
            let last_reviewed_at: Option<Timestamp> = row.get(0)?;
            let stability: Option<Stability> = row.get(1)?;
            let difficulty: Option<Difficulty> = row.get(2)?;
            let interval_raw: Option<f64> = row.get(3)?;
            let interval_days: Option<i64> = row.get(4)?;
            let due_date: Option<Date> = row.get(5)?;
            let review_count: i32 = row.get(6)?;
            if let (
                Some(last_reviewed_at),
                Some(stability),
                Some(difficulty),
                Some(interval_raw),
                Some(interval_days),
                Some(due_date),
            ) = (
                last_reviewed_at,
                stability,
                difficulty,
                interval_raw,
                interval_days,
                due_date,
            ) {
                Ok(Performance::Reviewed(ReviewedPerformance {
                    last_reviewed_at,
                    stability,
                    difficulty,
                    interval_raw,
                    interval_days,
                    due_date,
                    review_count: review_count as usize,
                }))
            } else {
                Ok(Performance::New)
            }
        })?;
        if let Some(row) = rows.into_iter().next() {
            Ok(Some(row?))
        } else {
            Ok(None)
        }
    }

    /// Get a card's performance information. If the card does not exist,
    /// returns an error.
    pub fn get_card_performance(&self, card_hash: CardHash) -> Fallible<Performance> {
        match self.get_card_performance_opt(card_hash)? {
            Some(performance) => Ok(performance),
            None => fail(format!(
                "No performance data found for card with hash {card_hash}"
            )),
        }
    }

    /// Update a card's performance information.
    ///
    /// If no card with the given hash exists, returns an error.
    ///
    /// Test-only: production grading updates performance inside the same
    /// transaction as the review, via
    /// [`Database::insert_review_and_update_performance`].
    #[cfg(test)]
    pub fn update_card_performance(
        &self,
        card_hash: CardHash,
        performance: Performance,
    ) -> Fallible<()> {
        let conn = self.conn.lock();
        if !card_exists_in(&conn, &self.collection, card_hash)? {
            return fail("Card not found");
        }
        let (
            last_reviewed_at,
            stability,
            difficulty,
            interval_raw,
            interval_days,
            due_date,
            review_count,
        ) = match performance {
            Performance::New => (None, None, None, None, None, None, 0),
            Performance::Reviewed(rp) => (
                Some(rp.last_reviewed_at),
                Some(rp.stability),
                Some(rp.difficulty),
                Some(rp.interval_raw),
                Some(rp.interval_days as i32),
                Some(rp.due_date),
                rp.review_count as i32,
            ),
        };
        let sql = "update cards set last_reviewed_at = ?, stability = ?, difficulty = ?, \
                   interval_raw = ?, interval_days = ?, due_date = ?, review_count = ? \
                   where collection_id = ? and card_hash = ?;";
        let params = params![
            last_reviewed_at,
            stability,
            difficulty,
            interval_raw,
            interval_days,
            due_date,
            review_count,
            self.collection,
            card_hash
        ];
        conn.execute(sql, params)?;
        Ok(())
    }

    /// Create a new session row at session start. Returns the session_id.
    /// ended_at is initially set to started_at as a placeholder; call
    /// close_session when the session finishes.
    pub fn create_session(&self, started_at: Timestamp) -> Fallible<i64> {
        let conn = self.conn.lock();
        let sql = "insert into sessions (collection_id, started_at, ended_at, last_seen_at) \
                   values (?, ?, ?, ?) returning session_id;";
        let session_id: i64 = conn.query_row(
            sql,
            params![self.collection, started_at, started_at, started_at],
            |row| row.get(0),
        )?;
        Ok(session_id)
    }

    /// Atomically insert a review and update the card's performance.
    ///
    /// Both operations run inside a single transaction so a crash between them
    /// cannot leave the DB in an inconsistent state. Returns the new review_id.
    pub fn insert_review_and_update_performance(
        &self,
        session_id: i64,
        review: &ReviewRecord,
        performance: Performance,
    ) -> Fallible<i64> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let review_id: i64 = {
            let sql = "insert into reviews (session_id, collection_id, card_hash, reviewed_at, \
                       grade, stability, difficulty, interval_raw, interval_days, due_date, \
                       duration_ms) values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                       returning review_id;";
            tx.query_row(
                sql,
                params![
                    session_id,
                    self.collection,
                    review.card_hash,
                    review.reviewed_at,
                    review.grade,
                    review.stability,
                    review.difficulty,
                    review.interval_raw,
                    review.interval_days as i32,
                    review.due_date,
                    review.duration_ms
                ],
                |row| row.get(0),
            )?
        };
        update_card_performance_tx(&tx, &self.collection, review.card_hash, performance)?;
        tx.commit()?;
        Ok(review_id)
    }

    /// Atomically void a review and restore prior card performance (undo).
    ///
    /// When `reopen_session` is `Some(session_id)`, the session row is
    /// reopened in the same transaction (its `ended_at` is reset to
    /// `started_at`, the placeholder used for open sessions), so undoing
    /// past a finished session cannot leave a closed row accumulating
    /// new reviews.
    ///
    /// All operations run inside a single transaction so a crash between
    /// them cannot leave the DB in an inconsistent state.
    pub fn void_review_and_restore_performance(
        &self,
        review_id: i64,
        card_hash: CardHash,
        prev_performance: Performance,
        reopen_session: Option<i64>,
    ) -> Fallible<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let rows = tx.execute(
            "update reviews set voided = 1 \
             where review_id = ? and collection_id = ? and card_hash = ?;",
            params![review_id, self.collection, card_hash],
        )?;
        if rows != 1 {
            return fail("review not found or does not belong to this card");
        }
        update_card_performance_tx(&tx, &self.collection, card_hash, prev_performance)?;
        if let Some(session_id) = reopen_session {
            let rows = tx.execute(
                "update sessions set ended_at = started_at, closed = 0 \
                 where session_id = ? and collection_id = ?;",
                params![session_id, self.collection],
            )?;
            if rows != 1 {
                return fail(format!("No session with ID {session_id} to reopen"));
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Insert a single review without touching card performance.
    ///
    /// Test-only: production grading goes through
    /// [`Database::insert_review_and_update_performance`], which keeps the
    /// review and the card's performance in one transaction.
    #[cfg(test)]
    pub fn insert_review_immediately(
        &self,
        session_id: i64,
        review: &ReviewRecord,
    ) -> Fallible<i64> {
        let conn = self.conn.lock();
        let sql = "insert into reviews (session_id, collection_id, card_hash, reviewed_at, grade, \
                   stability, difficulty, interval_raw, interval_days, due_date, duration_ms) \
                   values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) returning review_id;";
        let review_id: i64 = conn.query_row(
            sql,
            params![
                session_id,
                self.collection,
                review.card_hash,
                review.reviewed_at,
                review.grade,
                review.stability,
                review.difficulty,
                review.interval_raw,
                review.interval_days as i32,
                review.due_date,
                review.duration_ms
            ],
            |row| row.get(0),
        )?;
        Ok(review_id)
    }

    /// Update ended_at to mark a session as complete.
    pub fn close_session(&self, session_id: i64, ended_at: Timestamp) -> Fallible<()> {
        let conn = self.conn.lock();
        let sql = "update sessions set ended_at = ?, closed = 1 \
                   where session_id = ? and collection_id = ?;";
        let rows = conn.execute(sql, params![ended_at, session_id, self.collection])?;
        if rows != 1 {
            return fail(format!("No session with ID {session_id} to close"));
        }
        Ok(())
    }

    /// Count cards grouped by due date. `None` means never reviewed (due
    /// immediately). Ordered with `None` first, then ascending date.
    /// The due date recorded for every card row, by hash. `None` means the
    /// card exists but has never been reviewed.
    ///
    /// Callers that report on a collection should look each *parsed* card up
    /// here rather than aggregating the table directly: a card the collection
    /// contains may have no row yet (nothing inserts one before the first
    /// drill), and a row may survive a card that was deleted from the deck.
    pub fn due_dates(&self) -> Fallible<HashMap<CardHash, Option<Date>>> {
        let conn = self.conn.lock();
        let sql = "select card_hash, due_date from cards where collection_id = ?;";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![self.collection], |row| {
            let hash: CardHash = row.get(0)?;
            let due: Option<Date> = row.get(1)?;
            Ok((hash, due))
        })?;
        let mut result = HashMap::new();
        for row in rows {
            let (hash, due) = row?;
            result.insert(hash, due);
        }
        Ok(result)
    }

    /// Count non-voided reviews per day from `since` (inclusive) onward,
    /// ascending. Days with no reviews are absent. Range-scans the indexed
    /// `reviewed_date` column.
    pub fn count_reviews_per_day_since(&self, since: Date) -> Fallible<Vec<(Date, usize)>> {
        let conn = self.conn.lock();
        let sql = "select reviewed_date, count(*) from reviews \
                   where collection_id = ? and voided = 0 and reviewed_date >= ? \
                   group by reviewed_date order by reviewed_date;";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![self.collection, since], |row| {
            let date: Date = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((date, count as usize))
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Count non-voided reviews per grade, across all time.
    pub fn grade_distribution(&self) -> Fallible<GradeDistribution> {
        let conn = self.conn.lock();
        let sql = "select grade, count(*) from reviews \
                   where collection_id = ? and voided = 0 group by grade;";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![self.collection], |row| {
            let grade: Grade = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((grade, count as usize))
        })?;
        let mut dist = GradeDistribution {
            forgot: 0,
            hard: 0,
            good: 0,
            easy: 0,
        };
        for row in rows {
            let (grade, count) = row?;
            match grade {
                Grade::Forgot => dist.forgot = count,
                Grade::Hard => dist.hard = count,
                Grade::Good => dist.good = count,
                Grade::Easy => dist.easy = count,
            }
        }
        Ok(dist)
    }

    /// The fraction of non-voided reviews since `since` (inclusive) that were
    /// graded better than Forgot. `None` when the window has no reviews.
    pub fn retention_since(&self, since: Date) -> Fallible<Option<f64>> {
        let conn = self.conn.lock();
        let sql = "select count(*), \
                   coalesce(sum(case when grade <> 'forgot' then 1 else 0 end), 0) \
                   from reviews where collection_id = ? and voided = 0 and reviewed_date >= ?;";
        let (total, remembered): (i64, i64) =
            conn.query_row(sql, params![self.collection, since], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
        if total == 0 {
            Ok(None)
        } else {
            Ok(Some(remembered as f64 / total as f64))
        }
    }

    /// Stamp a session's heartbeat.
    ///
    /// Called whenever the owning process serves a page or handles an action,
    /// so the startup sweep can tell a session abandoned by a crash from one
    /// still running in another process. A session that no longer exists is
    /// not an error: the caller is on a request path, not doing bookkeeping.
    pub fn touch_session(&self, session_id: i64, now: Timestamp) -> Fallible<()> {
        let conn = self.conn.lock();
        let sql = "update sessions set last_seen_at = ? \
                   where session_id = ? and collection_id = ?;";
        conn.execute(sql, params![now, session_id, self.collection])?;
        Ok(())
    }

    /// Apply a web edit's hash migration in a single transaction.
    ///
    /// Each rename moves a card row in place; its reviews and bookmarks
    /// follow via ON UPDATE CASCADE, and the performance columns ride along
    /// in the row itself. A rename whose target hash already exists is
    /// skipped rather than failing: the edited card is now byte-identical to
    /// another card that has its own history, and neither row may be
    /// destroyed. The old row stays behind as an orphan, exactly as
    /// `hashcards orphans` understands one, and the caller reports it.
    ///
    /// Doing this one statement at a time left the database half-migrated
    /// when a later rename failed, with the source file already rewritten.
    ///
    /// Returns the counts of renames actually applied and of renames skipped
    /// as true collisions (target hash already had history). A rename whose
    /// old hash had no history of its own is neither: there was nothing to
    /// carry over, so it is just recorded as a fresh card and not counted in
    /// either number.
    pub fn apply_edit_migration(
        &self,
        renames: &[(CardHash, CardHash)],
        fresh: &[CardHash],
        now: Timestamp,
    ) -> Fallible<EditMigrationCounts> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let mut renamed: usize = 0;
        let mut collided: usize = 0;
        for (old, new) in renames {
            if card_exists_in(&tx, &self.collection, *new)? {
                // The new content already has history of its own.
                collided += 1;
                continue;
            }
            if !card_exists_in(&tx, &self.collection, *old)? {
                // Nothing to carry over; just record the new card.
                insert_card_if_new_in(&tx, &self.collection, *new, now)?;
                continue;
            }
            tx.execute(
                "update cards set card_hash = ? where collection_id = ? and card_hash = ?;",
                params![new, self.collection, old],
            )?;
            renamed += 1;
        }
        for new in fresh {
            insert_card_if_new_in(&tx, &self.collection, *new, now)?;
        }
        tx.commit()?;
        Ok(EditMigrationCounts { renamed, collided })
    }

    /// Delete a card. The foreign-key cascade removes its reviews and
    /// bookmarks in the same statement, so the deletion is atomic.
    ///
    /// Note that this permanently erases the card's review history; it does
    /// not go through the `voided` audit-trail model that undo uses.
    ///
    /// If no card with the given hash exists, returns an error.
    pub fn delete_card(&self, card_hash: CardHash) -> Fallible<()> {
        let conn = self.conn.lock();
        let sql = "delete from cards where collection_id = ? and card_hash = ?;";
        let rows = conn.execute(sql, params![self.collection, card_hash])?;
        if rows != 1 {
            return fail("Card not found");
        }
        Ok(())
    }

    /// Does a card with the given hash exist?
    pub fn card_exists(&self, card_hash: CardHash) -> Fallible<bool> {
        let conn = self.conn.lock();
        card_exists_in(&conn, &self.collection, card_hash)
    }

    /// Insert a card only if it doesn't already exist.
    pub fn insert_card_if_new(&self, card_hash: CardHash, added_at: Timestamp) -> Fallible<()> {
        let conn = self.conn.lock();
        insert_card_if_new_in(&conn, &self.collection, card_hash, added_at)
    }

    /// Does a bookmark for this card hash exist?
    pub fn bookmark_exists(&self, card_hash: CardHash) -> Fallible<bool> {
        let conn = self.conn.lock();
        let sql = "select count(*) from bookmarks where collection_id = ? and card_hash = ?;";
        let count: i64 =
            conn.query_row(sql, params![self.collection, card_hash], |row| row.get(0))?;
        Ok(count > 0)
    }

    /// Insert a bookmark for this card if none exists. An existing bookmark —
    /// including its note and creation time — is left completely untouched.
    /// The card row must already exist; drill callers ensure this with
    /// `insert_card_if_new`.
    ///
    /// Unlike reviews, bookmarks write to the DB mid-session so they survive aborted sessions.
    pub fn insert_bookmark(
        &self,
        card_hash: CardHash,
        note: Option<String>,
        now: Timestamp,
    ) -> Fallible<()> {
        let conn = self.conn.lock();
        let sql = "insert into bookmarks (collection_id, card_hash, note, created_at) \
                   values (?, ?, ?, ?) on conflict (collection_id, card_hash) do nothing;";
        conn.execute(sql, params![self.collection, card_hash, note, now])?;
        Ok(())
    }

    /// Delete a bookmark.
    pub fn delete_bookmark(&self, card_hash: CardHash) -> Fallible<()> {
        let conn = self.conn.lock();
        let sql = "delete from bookmarks where collection_id = ? and card_hash = ?;";
        conn.execute(sql, params![self.collection, card_hash])?;
        Ok(())
    }

    /// Get the bookmark for a card, if any.
    #[allow(dead_code)]
    pub fn get_bookmark(&self, card_hash: CardHash) -> Fallible<Option<Bookmark>> {
        let conn = self.conn.lock();
        let sql = "select note, created_at from bookmarks \
                   where collection_id = ? and card_hash = ?;";
        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt.query_map(params![self.collection, card_hash], |row| {
            Ok(Bookmark {
                card_hash,
                note: row.get(0)?,
                created_at: row.get(1)?,
            })
        })?;
        if let Some(row) = rows.next() {
            Ok(Some(row?))
        } else {
            Ok(None)
        }
    }

    /// List all bookmarks, newest first.
    pub fn list_bookmarks(&self) -> Fallible<Vec<Bookmark>> {
        let conn = self.conn.lock();
        let sql = "select card_hash, note, created_at from bookmarks \
                   where collection_id = ? order by created_at desc;";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![self.collection], |row| {
            Ok(Bookmark {
                card_hash: row.get(0)?,
                note: row.get(1)?,
                created_at: row.get(2)?,
            })
        })?;
        let mut bookmarks = Vec::new();
        for row in rows {
            bookmarks.push(row?);
        }
        Ok(bookmarks)
    }

    /// Update the note on an existing bookmark.
    pub fn update_bookmark_note(&self, card_hash: CardHash, note: Option<String>) -> Fallible<()> {
        let conn = self.conn.lock();
        let sql = "update bookmarks set note = ? where collection_id = ? and card_hash = ?;";
        conn.execute(sql, params![note, self.collection, card_hash])?;
        Ok(())
    }

    /// Count the number of bookmarks in the database.
    pub fn count_bookmarks(&self) -> Fallible<usize> {
        let conn = self.conn.lock();
        let sql = "select count(*) from bookmarks where collection_id = ?;";
        let count: i64 = conn.query_row(sql, params![self.collection], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// Count the number of non-voided reviews performed on the given date.
    pub fn count_reviews_in_date(&self, date: Date) -> Fallible<usize> {
        let conn = self.conn.lock();
        let sql = "select count(*) from reviews \
                   where collection_id = ? and reviewed_date = ? and voided = 0;";
        let count: i64 = conn.query_row(sql, params![self.collection, date], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// Get the list of all sessions in the database.
    pub fn get_all_sessions(&self) -> Fallible<Vec<SessionRow>> {
        let conn = self.conn.lock();
        let sql = "select session_id, started_at, ended_at from sessions \
                   where collection_id = ? order by started_at;";
        let mut stmt = conn.prepare(sql)?;
        let session_iter = stmt.query_map(params![self.collection], |row| {
            Ok(SessionRow {
                session_id: row.get(0)?,
                started_at: row.get(1)?,
                ended_at: row.get(2)?,
            })
        })?;
        let mut sessions = Vec::new();
        for session in session_iter {
            sessions.push(session?);
        }
        Ok(sessions)
    }

    /// Get the list of all non-voided reviews for a given session.
    pub fn get_reviews_for_session(&self, session_id: i64) -> Fallible<Vec<ReviewRow>> {
        let conn = self.conn.lock();
        let sql = "select review_id, card_hash, reviewed_at, grade, stability, difficulty, \
                   interval_raw, interval_days, due_date, duration_ms from reviews \
                   where session_id = ? and collection_id = ? and voided = 0 \
                   order by reviewed_at;";
        let mut stmt = conn.prepare(sql)?;
        let review_iter = stmt.query_map(params![session_id, self.collection], |row| {
            Ok(ReviewRow {
                review_id: row.get(0)?,
                data: ReviewRecord {
                    card_hash: row.get(1)?,
                    reviewed_at: row.get(2)?,
                    grade: row.get(3)?,
                    stability: row.get(4)?,
                    difficulty: row.get(5)?,
                    interval_raw: row.get(6)?,
                    interval_days: row.get(7)?,
                    due_date: row.get(8)?,
                    duration_ms: row.get(9)?,
                },
            })
        })?;
        let mut reviews = Vec::new();
        for review in review_iter {
            reviews.push(review?);
        }
        Ok(reviews)
    }

    /// Every surviving review of one card, oldest first.
    ///
    /// `voided = 0`, like every other read path: an undone review is marked
    /// rather than deleted, and reporting it here would tell a reader the
    /// card was graded when the user took that back.
    pub fn reviews_for_card(&self, card_hash: CardHash) -> Fallible<Vec<ReviewRow>> {
        let conn = self.conn.lock();
        let sql = "select review_id, card_hash, reviewed_at, grade, stability, difficulty, \
                   interval_raw, interval_days, due_date, duration_ms from reviews \
                   where card_hash = ? and collection_id = ? and voided = 0 \
                   order by reviewed_at;";
        let mut stmt = conn.prepare(sql)?;
        let review_iter = stmt.query_map(params![card_hash, self.collection], |row| {
            Ok(ReviewRow {
                review_id: row.get(0)?,
                data: ReviewRecord {
                    card_hash: row.get(1)?,
                    reviewed_at: row.get(2)?,
                    grade: row.get(3)?,
                    stability: row.get(4)?,
                    difficulty: row.get(5)?,
                    interval_raw: row.get(6)?,
                    interval_days: row.get(7)?,
                    due_date: row.get(8)?,
                    duration_ms: row.get(9)?,
                },
            })
        })?;
        let mut reviews = Vec::new();
        for review in review_iter {
            reviews.push(review?);
        }
        Ok(reviews)
    }

    /// Remove every row this collection owns.
    ///
    /// What deleting a collection folder used to do by deleting a file. The
    /// cards go first and take their reviews and bookmarks with them by
    /// cascade; the sessions have no card to hang from and are deleted
    /// explicitly. One transaction, so a collection is never half-erased.
    ///
    /// This permanently destroys the collection's review history. It does
    /// not go through the `voided` audit trail that undo uses.
    pub fn erase(&self) -> Fallible<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "delete from cards where collection_id = ?;",
            params![self.collection],
        )?;
        tx.execute(
            "delete from sessions where collection_id = ?;",
            params![self.collection],
        )?;
        tx.commit()?;
        Ok(())
    }
}

/// Open a per-collection database and bring it up to the last version that
/// shape ever had.
///
/// Only the startup merge calls this. Nothing else may write to these files:
/// after they have been merged they are moved into `db/legacy/`, and an older
/// binary writing into one would strand every review it recorded there.
pub fn open_legacy_source(path: &Path) -> Fallible<Connection> {
    let mut conn = Connection::open(path)?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_FKEY, true)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    {
        let tx = conn.transaction()?;
        if !probe_schema_exists(&tx)? {
            return fail(format!(
                "{} has no `cards` table, so it is not a review database.",
                path.display()
            ));
        }
        migrate_legacy(&tx)?;
        tx.commit()?;
    }
    Ok(conn)
}

fn update_card_performance_tx(
    tx: &Transaction,
    collection: &CollectionId,
    card_hash: CardHash,
    performance: Performance,
) -> Fallible<()> {
    let (
        last_reviewed_at,
        stability,
        difficulty,
        interval_raw,
        interval_days,
        due_date,
        review_count,
    ) = match performance {
        Performance::New => (None, None, None, None, None, None, 0i32),
        Performance::Reviewed(rp) => (
            Some(rp.last_reviewed_at),
            Some(rp.stability),
            Some(rp.difficulty),
            Some(rp.interval_raw),
            Some(rp.interval_days as i32),
            Some(rp.due_date),
            rp.review_count as i32,
        ),
    };
    let sql = "update cards set last_reviewed_at = ?, stability = ?, difficulty = ?, \
               interval_raw = ?, interval_days = ?, due_date = ?, review_count = ? \
               where collection_id = ? and card_hash = ?;";
    tx.execute(
        sql,
        params![
            last_reviewed_at,
            stability,
            difficulty,
            interval_raw,
            interval_days,
            due_date,
            review_count,
            collection,
            card_hash
        ],
    )?;
    Ok(())
}

fn migrate_add_duration_ms(tx: &Transaction) -> Fallible<()> {
    let sql = "select count(*) from pragma_table_info('reviews') where name = 'duration_ms';";
    let count: i64 = tx.query_row(sql, [], |row| row.get(0))?;
    if count == 0 {
        tx.execute_batch("alter table reviews add column duration_ms integer;")?;
    }
    Ok(())
}

fn migrate_add_bookmarks(tx: &Transaction) -> Fallible<()> {
    let sql = "select count(*) from sqlite_master where type='table' AND name='bookmarks';";
    let count: i64 = tx.query_row(sql, [], |row| row.get(0))?;
    if count == 0 {
        tx.execute_batch(
            "create table bookmarks (
                card_hash text primary key
                    references cards (card_hash)
                    on update cascade
                    on delete cascade,
                note text,
                created_at text not null
            ) strict;",
        )?;
    }
    Ok(())
}

fn migrate_add_voided(tx: &Transaction) -> Fallible<()> {
    let sql = "select count(*) from pragma_table_info('reviews') where name = 'voided';";
    let count: i64 = tx.query_row(sql, [], |row| row.get(0))?;
    if count == 0 {
        tx.execute_batch("alter table reviews add column voided integer not null default 0;")?;
    }
    Ok(())
}

/// Migration 7: session liveness. `last_seen_at` is a heartbeat the owning
/// process stamps as it works, so the startup sweep can distinguish a session
/// abandoned by a crash from one still running in another process. `closed`
/// replaces the `ended_at = started_at` predicate, which re-detected a
/// session whose reviews had all been undone on every single sweep.
fn migrate_add_session_liveness(tx: &Transaction) -> Fallible<()> {
    let has = |name: &str| -> Fallible<bool> {
        let sql = "select count(*) from pragma_table_info('sessions') where name = ?;";
        let count: i64 = tx.query_row(sql, params![name], |row| row.get(0))?;
        Ok(count > 0)
    };
    if !has("last_seen_at")? {
        tx.execute_batch("alter table sessions add column last_seen_at text;")?;
        // The last review is the best evidence of when the session was alive.
        tx.execute_batch(
            "update sessions set last_seen_at = coalesce(
                 (select max(reviewed_at) from reviews
                  where reviews.session_id = sessions.session_id),
                 started_at
             );",
        )?;
    }
    if !has("closed")? {
        tx.execute_batch("alter table sessions add column closed integer not null default 0;")?;
        // A row whose end time was moved off its start time was closed.
        tx.execute_batch("update sessions set closed = 1 where ended_at <> started_at;")?;
    }
    Ok(())
}

/// Bring an existing database up to SCHEMA_VERSION by applying numbered
/// migrations in order. Databases from before the version table existed
/// start at version 0; migrations 1-3 probe before altering, so they are
/// safe no-ops on legacy databases that already have the feature.
fn migrate_legacy(tx: &Transaction) -> Fallible<()> {
    ensure_version_table(tx)?;
    let current = get_schema_version(tx)?;
    if current > LEGACY_SCHEMA_VERSION {
        return fail(format!(
            "This database uses schema version {current}, but a per-collection database only \
             goes up to schema version {LEGACY_SCHEMA_VERSION}. Please upgrade hashcards-web."
        ));
    }
    for version in (current + 1)..=LEGACY_SCHEMA_VERSION {
        match version {
            1 => migrate_add_duration_ms(tx)?,
            2 => migrate_add_bookmarks(tx)?,
            3 => migrate_add_voided(tx)?,
            4 => migrate_add_review_indexes(tx)?,
            5 => migrate_add_reviewed_date(tx)?,
            6 => migrate_add_meta(tx)?,
            7 => migrate_add_session_liveness(tx)?,
            other => {
                return fail(format!(
                    "Internal error: no migration defined for schema version {other}."
                ));
            }
        }
        set_schema_version(tx, version)?;
    }
    Ok(())
}

/// Create the schema_version table if missing and seed it at version 0
/// (the state of databases created before versioning existed).
pub(crate) fn ensure_version_table(tx: &Transaction) -> Fallible<()> {
    tx.execute_batch(
        "create table if not exists schema_version (version integer not null) strict;",
    )?;
    let count: i64 = tx.query_row("select count(*) from schema_version;", [], |row| row.get(0))?;
    if count == 0 {
        tx.execute("insert into schema_version (version) values (0);", [])?;
    }
    Ok(())
}

pub(crate) fn get_schema_version(tx: &Transaction) -> Fallible<i64> {
    let version: i64 = tx.query_row("select version from schema_version;", [], |row| row.get(0))?;
    Ok(version)
}

pub(crate) fn set_schema_version(tx: &Transaction, version: i64) -> Fallible<()> {
    tx.execute("delete from schema_version;", [])?;
    tx.execute(
        "insert into schema_version (version) values (?);",
        params![version],
    )?;
    Ok(())
}

fn migrate_add_review_indexes(tx: &Transaction) -> Fallible<()> {
    tx.execute_batch(
        "create index if not exists idx_reviews_card_hash on reviews (card_hash);
         create index if not exists idx_reviews_session_id on reviews (session_id);",
    )?;
    Ok(())
}

fn migrate_add_reviewed_date(tx: &Transaction) -> Fallible<()> {
    tx.execute_batch(
        "alter table reviews add column reviewed_date text generated always as (substr(reviewed_at, 1, 10)) virtual;
         create index if not exists idx_reviews_reviewed_date on reviews (reviewed_date);",
    )?;
    Ok(())
}

pub(crate) fn probe_schema_exists(tx: &Transaction) -> Fallible<bool> {
    let sql = "select count(*) from sqlite_master where type='table' AND name=?;";
    let count: i64 = tx.query_row(sql, ["cards"], |row| row.get(0))?;
    Ok(count > 0)
}

/// Migration 6: a generic key/value meta table for schema-adjacent settings.
fn migrate_add_meta(tx: &Transaction) -> Fallible<()> {
    tx.execute_batch(
        "create table if not exists meta (
            key text primary key,
            value text not null
        ) strict;",
    )?;
    Ok(())
}

/// Builders for the database shapes this release replaces. Tests only: the
/// server never creates a per-collection database again.
#[cfg(test)]
pub mod test_support {
    use std::path::Path;

    use rusqlite::Connection;

    use super::LEGACY_SCHEMA_VERSION;
    use crate::error::Fallible;

    /// The last per-collection shape, at version 7.
    pub fn create_legacy_v7(path: &Path) -> Fallible<Connection> {
        let conn = Connection::open(path)?;
        conn.execute_batch(include_str!("schema_v7.sql"))?;
        conn.execute(
            "insert into schema_version (version) values (?);",
            rusqlite::params![LEGACY_SCHEMA_VERSION],
        )?;
        Ok(conn)
    }

    /// The shape from before the version table existed, which
    /// `migrate_legacy` reads as version 0.
    pub fn create_legacy_v0(path: &Path) -> Fallible<Connection> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "create table cards (
                card_hash text primary key,
                added_at text not null,
                last_reviewed_at text,
                stability real,
                difficulty real,
                interval_raw real,
                interval_days integer,
                due_date text,
                review_count integer not null
            ) strict;

            create table sessions (
                session_id integer primary key,
                started_at text not null,
                ended_at text not null
            ) strict;

            create table reviews (
                review_id integer primary key,
                session_id integer not null
                    references sessions (session_id)
                    on update cascade
                    on delete cascade,
                card_hash text not null
                    references cards (card_hash)
                    on update cascade
                    on delete cascade,
                reviewed_at text not null,
                grade text not null,
                stability real not null,
                difficulty real not null,
                interval_raw real not null,
                interval_days integer not null,
                due_date text not null
            ) strict;",
        )?;
        Ok(conn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsrs::Grade;
    use crate::types::collection_id::CollectionId;
    use crate::types::performance::ReviewedPerformance;
    use crate::user_db::UserDatabase;

    /// The spec's central promise: the same card text in two collections
    /// keeps two schedules. Two files necessarily meant two schedules, so
    /// this is today's behaviour restated as a constraint on one file.
    #[test]
    fn the_same_hash_in_two_collections_keeps_two_schedules() -> Fallible<()> {
        let user = UserDatabase::memory()?;
        let biology = user.collection(CollectionId::new("bio")?);
        let spanish = user.collection(CollectionId::new("esp")?);
        let hash = CardHash::hash_bytes(b"shared card");
        let now = Timestamp::now();
        biology.insert_card(hash, now)?;
        spanish.insert_card(hash, now)?;

        let reviewed = Performance::Reviewed(ReviewedPerformance {
            last_reviewed_at: now,
            stability: 2.0,
            difficulty: 5.0,
            interval_raw: 30.0,
            interval_days: 30,
            due_date: now.date(),
            review_count: 1,
        });
        biology.update_card_performance(hash, reviewed)?;

        assert_eq!(biology.get_card_performance(hash)?, reviewed);
        assert_eq!(spanish.get_card_performance(hash)?, Performance::New);
        Ok(())
    }

    /// Deleting one collection's rows must leave its neighbour's alone. This
    /// is what replaces deleting a file when a collection folder is removed.
    #[test]
    fn erasing_one_collection_leaves_the_other_intact() -> Fallible<()> {
        let user = UserDatabase::memory()?;
        let biology = user.collection(CollectionId::new("bio")?);
        let spanish = user.collection(CollectionId::new("esp")?);
        let hash = CardHash::hash_bytes(b"shared card");
        let now = Timestamp::now();
        biology.insert_card(hash, now)?;
        spanish.insert_card(hash, now)?;
        biology.insert_bookmark(hash, Some("note".to_string()), now)?;
        spanish.insert_bookmark(hash, Some("note".to_string()), now)?;
        biology.create_session(now)?;

        biology.erase()?;

        assert!(biology.card_hashes()?.is_empty());
        assert_eq!(biology.count_bookmarks()?, 0);
        assert!(biology.get_all_sessions()?.is_empty());
        assert!(spanish.card_hashes()?.contains(&hash));
        assert_eq!(spanish.count_bookmarks()?, 1);
        Ok(())
    }

    /// An edit renames a card's hash and its reviews follow by cascade. The
    /// cascade must stop at the collection boundary: an identical card in a
    /// neighbouring collection has its own history and must not move.
    #[test]
    fn an_edit_rename_cascades_within_its_collection_only() -> Fallible<()> {
        let user = UserDatabase::memory()?;
        let biology = user.collection(CollectionId::new("bio")?);
        let spanish = user.collection(CollectionId::new("esp")?);
        let old = CardHash::hash_bytes(b"before");
        let new = CardHash::hash_bytes(b"after");
        let now = Timestamp::now();
        biology.insert_card(old, now)?;
        spanish.insert_card(old, now)?;
        let session = biology.create_session(now)?;
        biology.insert_review_immediately(session, &sample_review(old, now, 2.0))?;

        let counts = biology.apply_edit_migration(&[(old, new)], &[], now)?;
        assert_eq!(
            counts,
            EditMigrationCounts {
                renamed: 1,
                collided: 0
            }
        );

        assert!(biology.card_hashes()?.contains(&new));
        assert!(!biology.card_hashes()?.contains(&old));
        assert_eq!(
            biology.get_reviews_for_session(session)?[0].data.card_hash,
            new
        );
        // The neighbour still holds the old hash, and never gained the new.
        assert!(spanish.card_hashes()?.contains(&old));
        assert!(!spanish.card_hashes()?.contains(&new));
        Ok(())
    }

    #[test]
    fn test_probe_schema_exists() -> Fallible<()> {
        let mut conn = Connection::open_in_memory()?;
        let tx = conn.transaction()?;
        assert!(!probe_schema_exists(&tx)?);
        Ok(())
    }

    /// Insert a card, and see that its hash is returned by `card_hashes`, and
    /// that `get_card_performance` returns an initial empty performance, and
    /// `due_today` returns it since it's new.
    #[test]
    fn test_insert_card() -> Fallible<()> {
        let db = Database::memory()?;
        let card_hash = CardHash::hash_bytes(b"a");
        let now = Timestamp::now();
        db.insert_card(card_hash, now)?;
        let hashes = db.card_hashes()?;
        assert!(hashes.contains(&card_hash));
        let performance = db.get_card_performance(card_hash)?;
        assert_eq!(performance, Performance::New);
        let due_today = db.due_today(now.date())?;
        assert!(due_today.contains(&card_hash));
        Ok(())
    }

    /// Inserting a card twice returns an error.
    #[test]
    fn test_insert_twice() -> Fallible<()> {
        let db = Database::memory()?;
        let card_hash = CardHash::hash_bytes(b"a");
        let now = Timestamp::now();
        db.insert_card(card_hash, now)?;
        let result = db.insert_card(card_hash, now);
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert_eq!(err.to_string(), "error: Card already exists");
        Ok(())
    }

    /// Updating a card's performance, and checking that `get_card_performance`
    /// works and that `due_today` returns the card.
    #[test]
    fn test_update_performance() -> Fallible<()> {
        let db = Database::memory()?;
        let card_hash = CardHash::hash_bytes(b"a");
        let now = Timestamp::now();
        db.insert_card(card_hash, now)?;
        let performance = Performance::Reviewed(ReviewedPerformance {
            last_reviewed_at: now,
            stability: 2.0,
            difficulty: 2.0,
            interval_raw: 1.0,
            interval_days: 1,
            due_date: now.date(),
            review_count: 1,
        });
        db.update_card_performance(card_hash, performance)?;
        let fetched_performance = db.get_card_performance(card_hash)?;
        assert_eq!(fetched_performance, performance);
        let due_today = db.due_today(now.date())?;
        assert!(due_today.contains(&card_hash));
        Ok(())
    }

    /// `get_card_performance` fails if the card does not exist.
    #[test]
    fn test_get_performance_nonexistent() -> Fallible<()> {
        let db = Database::memory()?;
        let card_hash = CardHash::hash_bytes(b"a");
        let result = db.get_card_performance(card_hash);
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert_eq!(
            err.to_string(),
            format!("error: No performance data found for card with hash {card_hash}")
        );
        Ok(())
    }

    /// `update_card_performance` fails if the card does not exist.
    #[test]
    fn test_update_performance_nonexistent() -> Fallible<()> {
        let db = Database::memory()?;
        let card_hash = CardHash::hash_bytes(b"a");
        let performance = Performance::New;
        let result = db.update_card_performance(card_hash, performance);
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert_eq!(err.to_string(), "error: Card not found");
        Ok(())
    }

    /// Create a session, insert a review immediately, and close the session.
    #[test]
    fn test_session_persistence() -> Fallible<()> {
        let db = Database::memory()?;
        let card_hash = CardHash::hash_bytes(b"a");
        let now = Timestamp::now();
        db.insert_card(card_hash, now)?;

        let session_id = db.create_session(now)?;
        let review = ReviewRecord {
            card_hash,
            reviewed_at: now,
            grade: Grade::Good,
            stability: 2.0,
            difficulty: 2.0,
            interval_raw: 1.0,
            interval_days: 1,
            due_date: now.date(),
            duration_ms: Some(3500),
        };
        let review_id = db.insert_review_immediately(session_id, &review)?;
        db.close_session(session_id, now)?;

        let sessions = db.get_all_sessions()?;
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.started_at, now);
        assert_eq!(session.ended_at, now);
        let reviews = db.get_reviews_for_session(session.session_id)?;
        assert_eq!(reviews.len(), 1);
        let fetched_review = &reviews[0];
        assert_eq!(fetched_review.review_id, review_id);
        assert_eq!(fetched_review.data.card_hash, card_hash);
        assert_eq!(fetched_review.data.reviewed_at, now);
        assert_eq!(fetched_review.data.grade, Grade::Good);
        assert_eq!(fetched_review.data.stability, 2.0);
        assert_eq!(fetched_review.data.difficulty, 2.0);
        assert_eq!(fetched_review.data.interval_raw, 1.0);
        assert_eq!(fetched_review.data.interval_days, 1);
        assert_eq!(fetched_review.data.due_date, now.date());
        assert_eq!(fetched_review.data.duration_ms, Some(3500));
        Ok(())
    }

    /// Build a review record for `card_hash` with the given stability.
    #[cfg(test)]
    /// A card's history is what `get_card` reports, so it must show what
    /// survived — an undone review was taken back and is not a grade.
    #[test]
    fn a_cards_reviews_come_back_in_order_and_skip_voided_ones() -> Fallible<()> {
        let db = UserDatabase::memory()?.collection(CollectionId::new("abc12345")?);
        let now = Timestamp::now();
        let hash = CardHash::hash_bytes(b"a card");
        db.insert_card(hash, now)?;
        let session = db.create_session(now)?;
        db.insert_review_immediately(session, &sample_review(hash, now, 1.0))?;
        let second = db.insert_review_immediately(session, &sample_review(hash, now, 2.0))?;

        let reviews = db.reviews_for_card(hash)?;
        assert_eq!(reviews.len(), 2);
        assert!(reviews.iter().all(|r| r.data.card_hash == hash));

        db.void_review_and_restore_performance(second, hash, Performance::New, None)?;
        assert_eq!(db.reviews_for_card(hash)?.len(), 1);
        Ok(())
    }

    /// Another card's reviews are not this card's, and neither are another
    /// collection's.
    #[test]
    fn a_cards_reviews_are_scoped_to_it_and_its_collection() -> Fallible<()> {
        let user = UserDatabase::memory()?;
        let spanish = user.collection(CollectionId::new("abc12345")?);
        let german = user.collection(CollectionId::new("def67890")?);
        let now = Timestamp::now();
        let hash = CardHash::hash_bytes(b"a card");
        let other = CardHash::hash_bytes(b"another card");

        for db in [&spanish, &german] {
            db.insert_card(hash, now)?;
            db.insert_card(other, now)?;
            let session = db.create_session(now)?;
            db.insert_review_immediately(session, &sample_review(hash, now, 1.0))?;
            db.insert_review_immediately(session, &sample_review(other, now, 1.0))?;
        }

        assert_eq!(spanish.reviews_for_card(hash)?.len(), 1);
        assert_eq!(german.reviews_for_card(hash)?.len(), 1);
        Ok(())
    }

    fn sample_review(card_hash: CardHash, now: Timestamp, stability: f64) -> ReviewRecord {
        ReviewRecord {
            card_hash,
            reviewed_at: now,
            grade: Grade::Good,
            stability,
            difficulty: 2.0,
            interval_raw: 1.0,
            interval_days: 1,
            due_date: now.date(),
            duration_ms: None,
        }
    }

    /// Build a reviewed performance with the given stability and review count.
    #[cfg(test)]
    fn sample_performance(now: Timestamp, stability: f64, review_count: usize) -> Performance {
        Performance::Reviewed(ReviewedPerformance {
            last_reviewed_at: now,
            stability,
            difficulty: 2.0,
            interval_raw: 1.0,
            interval_days: 1,
            due_date: now.date(),
            review_count,
        })
    }

    /// Grading writes the review and the card's performance in one transaction.
    #[test]
    fn test_insert_review_and_update_performance() -> Fallible<()> {
        let db = Database::memory()?;
        let card_hash = CardHash::hash_bytes(b"a");
        let now = Timestamp::now();
        db.insert_card(card_hash, now)?;
        assert!(matches!(
            db.get_card_performance(card_hash)?,
            Performance::New
        ));

        let session_id = db.create_session(now)?;
        let review = sample_review(card_hash, now, 2.0);
        let performance = sample_performance(now, 2.0, 1);
        let review_id =
            db.insert_review_and_update_performance(session_id, &review, performance)?;

        // The review landed...
        let reviews = db.get_reviews_for_session(session_id)?;
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].review_id, review_id);

        // ...and the card's performance moved with it.
        match db.get_card_performance(card_hash)? {
            Performance::Reviewed(rp) => {
                assert_eq!(rp.stability, 2.0);
                assert_eq!(rp.review_count, 1);
            }
            Performance::New => panic!("expected card performance to be updated"),
        }
        Ok(())
    }

    /// Undo voids the review and restores the card's prior performance, atomically.
    #[test]
    fn test_void_review_and_restore_performance() -> Fallible<()> {
        let db = Database::memory()?;
        let card_hash = CardHash::hash_bytes(b"a");
        let now = Timestamp::now();
        db.insert_card(card_hash, now)?;

        let session_id = db.create_session(now)?;
        // First grade: stability 2.0, one review.
        let first = sample_review(card_hash, now, 2.0);
        db.insert_review_and_update_performance(
            session_id,
            &first,
            sample_performance(now, 2.0, 1),
        )?;
        // Second grade: stability 5.0, two reviews.
        let second = sample_review(card_hash, now, 5.0);
        let second_id = db.insert_review_and_update_performance(
            session_id,
            &second,
            sample_performance(now, 5.0, 2),
        )?;
        assert_eq!(db.get_reviews_for_session(session_id)?.len(), 2);

        // Undo the second grade, restoring the performance the card had before it.
        db.void_review_and_restore_performance(
            second_id,
            card_hash,
            sample_performance(now, 2.0, 1),
            None,
        )?;

        // The voided review is excluded from read paths...
        let reviews = db.get_reviews_for_session(session_id)?;
        assert_eq!(reviews.len(), 1);
        assert_ne!(reviews[0].review_id, second_id);
        assert_eq!(db.count_reviews_in_date(now.date())?, 1);

        // ...and the card's performance rolled back.
        match db.get_card_performance(card_hash)? {
            Performance::Reviewed(rp) => {
                assert_eq!(rp.stability, 2.0);
                assert_eq!(rp.review_count, 1);
            }
            Performance::New => panic!("expected card to still be reviewed"),
        }
        Ok(())
    }

    /// Voiding a review that does not belong to the given card is rejected,
    /// and leaves the card's performance untouched.
    #[test]
    fn test_void_review_wrong_card_is_rejected() -> Fallible<()> {
        let db = Database::memory()?;
        let card_hash = CardHash::hash_bytes(b"a");
        let other_hash = CardHash::hash_bytes(b"b");
        let now = Timestamp::now();
        db.insert_card(card_hash, now)?;
        db.insert_card(other_hash, now)?;

        let session_id = db.create_session(now)?;
        let review = sample_review(card_hash, now, 2.0);
        let review_id = db.insert_review_and_update_performance(
            session_id,
            &review,
            sample_performance(now, 2.0, 1),
        )?;

        // Same review ID, wrong card: must fail rather than corrupt state.
        let result = db.void_review_and_restore_performance(
            review_id,
            other_hash,
            sample_performance(now, 9.0, 7),
            None,
        );
        assert!(result.is_err());

        // The review is still live and neither card was modified.
        assert_eq!(db.get_reviews_for_session(session_id)?.len(), 1);
        match db.get_card_performance(card_hash)? {
            Performance::Reviewed(rp) => assert_eq!(rp.stability, 2.0),
            Performance::New => panic!("expected card to remain reviewed"),
        }
        assert!(matches!(
            db.get_card_performance(other_hash)?,
            Performance::New
        ));
        Ok(())
    }

    /// Closing a session that does not exist is an error, not a silent no-op.
    #[test]
    fn test_close_nonexistent_session_is_error() -> Fallible<()> {
        let db = Database::memory()?;
        assert!(db.close_session(999, Timestamp::now()).is_err());
        Ok(())
    }

    /// Trying to delete a non-existent card returns an error.
    #[test]
    fn test_delete_nonexistent_card() -> Fallible<()> {
        let db = Database::memory()?;
        let card_hash = CardHash::hash_bytes(b"a");
        let result = db.delete_card(card_hash);
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert_eq!(err.to_string(), "error: Card not found");
        Ok(())
    }

    /// Delete a card and see that it is gone.
    #[test]
    fn test_delete_card() -> Fallible<()> {
        let db = Database::memory()?;
        let card_hash = CardHash::hash_bytes(b"a");
        let now = Timestamp::now();
        db.insert_card(card_hash, now)?;
        db.delete_card(card_hash)?;
        let result = db.get_card_performance(card_hash);
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert_eq!(
            err.to_string(),
            format!("error: No performance data found for card with hash {card_hash}")
        );
        Ok(())
    }

    /// Render a normalized, order-stable description of every user table:
    /// all columns (via table_xinfo, so generated columns are included) and
    /// every explicitly created index. Comparing two snapshots with
    /// assert_eq! yields a readable diff on mismatch.
    fn schema_snapshot(conn: &Connection) -> Fallible<String> {
        use std::fmt::Write;
        let mut out = String::new();
        let tables: Vec<String> = {
            let mut stmt = conn.prepare(
                "select name from sqlite_master where type = 'table' and name not like 'sqlite_%' order by name;",
            )?;
            let rows = stmt.query_map([], |row| row.get(0))?;
            let mut tables = Vec::new();
            for row in rows {
                tables.push(row?);
            }
            tables
        };
        for table in &tables {
            writeln!(out, "table {table}").unwrap();
            let mut stmt = conn.prepare(
                "select cid, name, type, \"notnull\", dflt_value, pk, hidden from pragma_table_xinfo(?) order by cid;",
            )?;
            let mut rows = stmt.query(params![table])?;
            while let Some(row) = rows.next()? {
                let cid: i64 = row.get(0)?;
                let name: String = row.get(1)?;
                let col_type: String = row.get(2)?;
                let notnull: i64 = row.get(3)?;
                let dflt: Option<String> = row.get(4)?;
                let pk: i64 = row.get(5)?;
                let hidden: i64 = row.get(6)?;
                writeln!(
                    out,
                    "  column {cid} {name} {col_type} notnull={notnull} default={dflt:?} pk={pk} hidden={hidden}"
                )
                .unwrap();
            }
            let index_names: Vec<String> = {
                let mut stmt = conn.prepare(
                    "select name from pragma_index_list(?) where origin = 'c' order by name;",
                )?;
                let rows = stmt.query_map(params![table], |row| row.get(0))?;
                let mut names = Vec::new();
                for row in rows {
                    names.push(row?);
                }
                names
            };
            for index in &index_names {
                let mut stmt =
                    conn.prepare("select name from pragma_index_info(?) order by seqno;")?;
                let mut rows = stmt.query(params![index])?;
                let mut columns: Vec<String> = Vec::new();
                while let Some(row) = rows.next()? {
                    columns.push(row.get(0)?);
                }
                writeln!(out, "  index {index} on ({})", columns.join(", ")).unwrap();
            }
        }
        Ok(out)
    }

    /// Migrating a pre-migration-era DB produces exactly the same schema as
    /// executing a fresh schema_v7.sql, and stamps the legacy schema version.
    #[test]
    fn test_legacy_ladder_converges_on_the_v7_schema() -> Fallible<()> {
        let dir = tempfile::TempDir::new().unwrap();
        let old_path = dir.path().join("old.db");
        test_support::create_legacy_v0(&old_path)?;
        let migrated = open_legacy_source(&old_path)?;
        let fresh = test_support::create_legacy_v7(&dir.path().join("fresh.db"))?;
        assert_eq!(
            schema_snapshot(&migrated)?,
            schema_snapshot(&fresh)?,
            "migrated schema diverged from schema_v7.sql"
        );
        let version: i64 =
            migrated.query_row("select version from schema_version;", [], |row| row.get(0))?;
        assert_eq!(version, LEGACY_SCHEMA_VERSION);
        Ok(())
    }

    /// Reopening an already-migrated DB is a no-op (migrations are not
    /// re-applied, the version is stable).
    #[test]
    fn test_reopening_migrated_db_is_stable() -> Fallible<()> {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("stable.db");
        test_support::create_legacy_v0(&path)?;
        let first = open_legacy_source(&path)?;
        let snapshot = schema_snapshot(&first)?;
        drop(first);
        let second = open_legacy_source(&path)?;
        assert_eq!(schema_snapshot(&second)?, snapshot);
        Ok(())
    }

    /// A DB stamped with a schema version newer than this build supports is
    /// rejected with a clear error instead of being mangled.
    #[test]
    fn test_newer_schema_version_is_rejected() -> Fallible<()> {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("future.db");
        {
            let conn = Connection::open(&path)?;
            conn.execute_batch(include_str!("schema_v7.sql"))?;
            conn.execute_batch(
                "create table if not exists schema_version (version integer not null) strict;",
            )?;
            conn.execute("delete from schema_version;", [])?;
            conn.execute("insert into schema_version (version) values (999);", [])?;
        }
        let result = open_legacy_source(&path);
        assert!(result.is_err());
        let message = result.err().unwrap().to_string();
        assert!(message.contains("999"), "unhelpful error: {message}");
        Ok(())
    }

    /// Migrating a legacy DB that already holds rows backfills the generated
    /// reviewed_date column for existing reviews (the convergence test above
    /// only covers empty tables).
    #[test]
    fn test_populated_legacy_db_migrates() -> Fallible<()> {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("pop.db");
        {
            let conn = test_support::create_legacy_v0(&path)?;
            conn.execute_batch(
                "insert into cards (card_hash, added_at, review_count) values ('abc', '2026-08-30T09:00:00.000', 1);
                 insert into sessions (session_id, started_at, ended_at) values (1, '2026-08-30T09:00:00.000', '2026-08-30T09:00:00.000');
                 insert into reviews (review_id, session_id, card_hash, reviewed_at, grade, stability, difficulty, interval_raw, interval_days, due_date)
                 values (1, 1, 'abc', '2026-08-30T09:00:00.000', 'good', 2.0, 5.0, 2.0, 2, '2026-09-01');",
            )?;
        }
        let conn = open_legacy_source(&path)?;
        let d: String = conn.query_row("select reviewed_date from reviews;", [], |r| r.get(0))?;
        assert_eq!(d, "2026-08-30");
        // The same predicate `count_reviews_in_date` uses, asked of the
        // legacy shape directly: this file has no `collection_id` to scope by.
        let day = Date::new(chrono::NaiveDate::from_ymd_opt(2026, 8, 30).unwrap());
        let count: i64 = conn.query_row(
            "select count(*) from reviews where reviewed_date = ? and voided = 0;",
            params![day],
            |r| r.get(0),
        )?;
        assert_eq!(count, 1);
        Ok(())
    }

    /// count_reviews_in_date filters on the indexed reviewed_date column
    /// (no substr() full scan) and still counts per-day correctly.
    #[test]
    fn test_count_reviews_in_date_uses_date_index() -> Fallible<()> {
        use chrono::NaiveDate;
        let db = Database::memory()?;
        let card_hash = CardHash::hash_bytes(b"a");
        db.insert_card(card_hash, Timestamp::now())?;
        let day1 = Timestamp::new(
            NaiveDate::from_ymd_opt(2026, 8, 30)
                .unwrap()
                .and_hms_opt(9, 0, 0)
                .unwrap(),
        );
        let day2 = Timestamp::new(
            NaiveDate::from_ymd_opt(2026, 8, 31)
                .unwrap()
                .and_hms_opt(9, 0, 0)
                .unwrap(),
        );
        let session_id = db.create_session(day1)?;
        db.insert_review_and_update_performance(
            session_id,
            &sample_review(card_hash, day1, 2.0),
            sample_performance(day1, 2.0, 1),
        )?;
        db.insert_review_and_update_performance(
            session_id,
            &sample_review(card_hash, day1, 2.0),
            sample_performance(day1, 2.0, 2),
        )?;
        db.insert_review_and_update_performance(
            session_id,
            &sample_review(card_hash, day2, 2.0),
            sample_performance(day2, 2.0, 3),
        )?;

        assert_eq!(db.count_reviews_in_date(day1.date())?, 2);
        assert_eq!(db.count_reviews_in_date(day2.date())?, 1);

        let plan: String = db.conn.lock().query_row(
            "explain query plan select count(*) from reviews \
             where collection_id = 'test-collection' and reviewed_date = '2026-08-31' \
             and voided = 0;",
            [],
            |row| row.get(3),
        )?;
        assert!(
            plan.contains("idx_reviews_reviewed_date"),
            "date query does not use the index; plan: {plan}"
        );
        Ok(())
    }

    /// The hot reviews lookups (by session and by card) are index searches,
    /// not full table scans.
    #[test]
    fn test_reviews_queries_use_indexes() -> Fallible<()> {
        let db = Database::memory()?;
        let plan: String = db.conn.lock().query_row(
            "explain query plan select count(*) from reviews where session_id = 1 and voided = 0;",
            [],
            |row| row.get(3),
        )?;
        assert!(
            plan.contains("idx_reviews_session_id"),
            "session_id query does not use the index; plan: {plan}"
        );
        let plan: String = db.conn.lock().query_row(
            "explain query plan select count(*) from reviews \
             where collection_id = 'test-collection' and card_hash = 'abc';",
            [],
            |row| row.get(3),
        )?;
        assert!(
            plan.contains("idx_reviews_card"),
            "card_hash query does not use the index; plan: {plan}"
        );
        Ok(())
    }

    /// A failed card deletion must not leave partial state behind.
    ///
    /// We force `delete from cards` to fail with a RESTRICT foreign key from a
    /// scratch table; the card's reviews must survive the failed deletion.
    /// The old implementation deleted reviews in a separate statement first,
    /// so they were lost even though delete_card returned an error.
    #[test]
    fn test_delete_card_failure_leaves_no_partial_state() -> Fallible<()> {
        let db = Database::memory()?;
        let card_hash = CardHash::hash_bytes(b"a");
        let now = Timestamp::now();
        db.insert_card(card_hash, now)?;
        let session_id = db.create_session(now)?;
        let review = sample_review(card_hash, now, 2.0);
        db.insert_review_and_update_performance(
            session_id,
            &review,
            sample_performance(now, 2.0, 1),
        )?;

        // Block deletion of this card with a restricting reference. The
        // reference is composite because the primary key is.
        db.conn.lock().execute_batch(&format!(
            "create table blocker (
                 collection_id text,
                 card_hash text,
                 foreign key (collection_id, card_hash)
                     references cards (collection_id, card_hash) on delete restrict
             );
             insert into blocker (collection_id, card_hash)
             values ('test-collection', '{card_hash}');"
        ))?;

        let result = db.delete_card(card_hash);
        assert!(result.is_err());

        // The failed deletion must leave the card AND its reviews intact.
        assert!(db.card_exists(card_hash)?);
        assert_eq!(db.get_reviews_for_session(session_id)?.len(), 1);
        Ok(())
    }

    /// Deleting a card removes its reviews via the FK cascade.
    #[test]
    fn test_delete_card_cascades_to_reviews() -> Fallible<()> {
        let db = Database::memory()?;
        let card_hash = CardHash::hash_bytes(b"a");
        let now = Timestamp::now();
        db.insert_card(card_hash, now)?;
        let session_id = db.create_session(now)?;
        let review = sample_review(card_hash, now, 2.0);
        db.insert_review_and_update_performance(
            session_id,
            &review,
            sample_performance(now, 2.0, 1),
        )?;

        db.delete_card(card_hash)?;
        assert!(!db.card_exists(card_hash)?);
        assert_eq!(db.get_reviews_for_session(session_id)?.len(), 0);
        Ok(())
    }

    /// Round-trip bookmark insert/get/list/delete.
    #[test]
    fn test_bookmark_crud() -> Fallible<()> {
        let db = Database::memory()?;
        let hash = CardHash::hash_bytes(b"a");
        let now = Timestamp::now();
        db.insert_card(hash, now)?;

        assert!(!db.bookmark_exists(hash)?);
        db.insert_bookmark(hash, Some("needs rephrasing".to_string()), now)?;
        assert!(db.bookmark_exists(hash)?);

        let bm = db.get_bookmark(hash)?.unwrap();
        assert_eq!(bm.card_hash, hash);
        assert_eq!(bm.note, Some("needs rephrasing".to_string()));

        let list = db.list_bookmarks()?;
        assert_eq!(list.len(), 1);

        db.update_bookmark_note(hash, None)?;
        let bm = db.get_bookmark(hash)?.unwrap();
        assert_eq!(bm.note, None);

        db.delete_bookmark(hash)?;
        assert!(!db.bookmark_exists(hash)?);
        Ok(())
    }

    /// Deleting a card cascades to its bookmark.
    #[test]
    fn test_bookmark_cascade_delete() -> Fallible<()> {
        let db = Database::memory()?;
        let hash = CardHash::hash_bytes(b"a");
        let now = Timestamp::now();
        db.insert_card(hash, now)?;
        db.insert_bookmark(hash, None, now)?;
        assert!(db.bookmark_exists(hash)?);
        db.delete_card(hash)?;
        assert!(!db.bookmark_exists(hash)?);
        Ok(())
    }

    /// A migrated rename carries the bookmark FK via ON UPDATE CASCADE.
    #[test]
    fn test_edit_migration_cascades_bookmark() -> Fallible<()> {
        let db = Database::memory()?;
        let old_hash = CardHash::hash_bytes(b"old");
        let new_hash = CardHash::hash_bytes(b"new");
        let now = Timestamp::now();
        db.insert_card(old_hash, now)?;
        db.insert_bookmark(old_hash, Some("note".to_string()), now)?;
        let counts = db.apply_edit_migration(&[(old_hash, new_hash)], &[], now)?;
        assert_eq!(
            counts,
            EditMigrationCounts {
                renamed: 1,
                collided: 0
            }
        );
        assert!(!db.bookmark_exists(old_hash)?);
        assert!(db.bookmark_exists(new_hash)?);
        let bm = db.get_bookmark(new_hash)?.unwrap();
        assert_eq!(bm.note, Some("note".to_string()));
        Ok(())
    }

    /// A rename whose target already has history is skipped, not an error:
    /// destroying either row would lose review data. Both rows survive and
    /// the caller sees that the rename did not happen, counted as a
    /// collision.
    #[test]
    fn test_edit_migration_skips_existing_target() -> Fallible<()> {
        let db = Database::memory()?;
        let hash_a = CardHash::hash_bytes(b"a");
        let hash_b = CardHash::hash_bytes(b"b");
        let now = Timestamp::now();
        db.insert_card(hash_a, now)?;
        db.insert_card(hash_b, now)?;
        let counts = db.apply_edit_migration(&[(hash_a, hash_b)], &[], now)?;
        assert_eq!(
            counts,
            EditMigrationCounts {
                renamed: 0,
                collided: 1
            }
        );
        assert!(db.card_exists(hash_a)?);
        assert!(db.card_exists(hash_b)?);
        Ok(())
    }

    /// Regression: a rename whose *old* hash never had a card row (e.g. a
    /// newly added card, or history already gone) is not a collision — there
    /// is nothing to lose. It must not be counted as one, or the edit UI
    /// wrongly warns the user that review history could not be matched.
    #[test]
    fn test_edit_migration_rename_with_no_prior_row_is_not_a_collision() -> Fallible<()> {
        let db = Database::memory()?;
        let old_hash = CardHash::hash_bytes(b"old");
        let new_hash = CardHash::hash_bytes(b"new");
        let now = Timestamp::now();
        // Neither hash has a card row yet.
        let counts = db.apply_edit_migration(&[(old_hash, new_hash)], &[], now)?;
        assert_eq!(
            counts,
            EditMigrationCounts {
                renamed: 0,
                collided: 0
            }
        );
        assert!(!db.card_exists(old_hash)?);
        assert!(db.card_exists(new_hash)?);
        Ok(())
    }

    /// Renames and fresh inserts are applied together in one call, and a
    /// fresh insert naming a hash the batch just renamed onto is a no-op
    /// rather than a conflict.
    #[test]
    fn test_edit_migration_renames_and_inserts_together() -> Fallible<()> {
        let db = Database::memory()?;
        let a = CardHash::hash_bytes(b"a");
        let b = CardHash::hash_bytes(b"b");
        let c = CardHash::hash_bytes(b"c");
        let now = Timestamp::now();
        db.insert_card(a, now)?;
        let counts = db.apply_edit_migration(&[(a, b)], &[b, c], now)?;
        assert_eq!(
            counts,
            EditMigrationCounts {
                renamed: 1,
                collided: 0
            }
        );
        assert!(!db.card_exists(a)?);
        assert!(db.card_exists(b)?);
        assert!(db.card_exists(c)?);
        Ok(())
    }

    /// insert_card_if_new inserts only when missing.
    #[test]
    fn test_insert_card_if_new() -> Fallible<()> {
        let db = Database::memory()?;
        let hash = CardHash::hash_bytes(b"a");
        let now = Timestamp::now();
        db.insert_card_if_new(hash, now)?;
        db.insert_card_if_new(hash, now)?; // no error on second call
        assert_eq!(db.card_hashes()?.len(), 1);
        Ok(())
    }

    /// BUG-07: bookmark, add note, re-bookmark — the note and created_at survive.
    #[test]
    fn test_rebookmark_preserves_note_and_created_at() -> Fallible<()> {
        use chrono::NaiveDate;

        let db = Database::memory()?;
        let hash = CardHash::hash_bytes(b"a");
        let created = Timestamp::new(
            NaiveDate::from_ymd_opt(2026, 8, 30)
                .unwrap()
                .and_hms_opt(10, 0, 0)
                .unwrap(),
        );
        let later = Timestamp::new(
            NaiveDate::from_ymd_opt(2026, 8, 31)
                .unwrap()
                .and_hms_opt(12, 30, 0)
                .unwrap(),
        );
        db.insert_card(hash, created)?;
        // Bookmark from the drill UI (the drill path always passes note: None).
        db.insert_bookmark(hash, None, created)?;
        // The user adds a note via the bookmark notes form.
        db.update_bookmark_note(hash, Some("needs rephrasing".to_string()))?;
        // The user presses `b` again on the same card in a later session.
        db.insert_bookmark(hash, None, later)?;
        let bm = db.get_bookmark(hash)?.unwrap();
        assert_eq!(bm.note, Some("needs rephrasing".to_string()));
        assert_eq!(bm.created_at, created);
        Ok(())
    }

    /// Build a timestamp on a specific date, for date-window queries.
    fn ts(date_time: &str) -> Timestamp {
        Timestamp::new(
            chrono::NaiveDateTime::parse_from_str(date_time, "%Y-%m-%d %H:%M:%S").unwrap(),
        )
    }

    /// Seed: card A reviewed good on 08-29 and forgot on 08-30; card B easy
    /// on 08-30; one voided review on 08-30 that must count nowhere.
    fn seed_stats_db() -> Fallible<Database> {
        let db = Database::memory()?;
        let a = CardHash::hash_bytes(b"a");
        let b = CardHash::hash_bytes(b"b");
        let now = ts("2026-08-29 10:00:00");
        db.insert_card(a, now)?;
        db.insert_card(b, now)?;
        let session_id = db.create_session(now)?;

        let review = |db: &Database, hash, when: Timestamp, grade| -> Fallible<i64> {
            let mut r = sample_review(hash, when, 2.0);
            r.grade = grade;
            db.insert_review_and_update_performance(
                session_id,
                &r,
                sample_performance(when, 2.0, 1),
            )
        };
        review(&db, a, ts("2026-08-29 10:00:00"), Grade::Good)?;
        review(&db, a, ts("2026-08-30 09:00:00"), Grade::Forgot)?;
        review(&db, b, ts("2026-08-30 11:00:00"), Grade::Easy)?;
        let voided = review(&db, b, ts("2026-08-30 12:00:00"), Grade::Good)?;
        db.void_review_and_restore_performance(
            voided,
            b,
            sample_performance(ts("2026-08-30 11:00:00"), 2.0, 1),
            None,
        )?;
        Ok(db)
    }

    #[test]
    fn test_count_reviews_per_day_since() -> Fallible<()> {
        let db = seed_stats_db()?;
        let since = Date::try_from("2026-08-01".to_string())?;
        let per_day = db.count_reviews_per_day_since(since)?;
        assert_eq!(
            per_day,
            vec![
                (Date::try_from("2026-08-29".to_string())?, 1),
                (Date::try_from("2026-08-30".to_string())?, 2),
            ]
        );
        // A later window excludes earlier reviews.
        let since = Date::try_from("2026-08-30".to_string())?;
        assert_eq!(db.count_reviews_per_day_since(since)?.len(), 1);
        Ok(())
    }

    #[test]
    fn test_grade_distribution() -> Fallible<()> {
        let db = seed_stats_db()?;
        let dist = db.grade_distribution()?;
        assert_eq!(dist.forgot, 1);
        assert_eq!(dist.hard, 0);
        assert_eq!(dist.good, 1); // the voided Good review does not count
        assert_eq!(dist.easy, 1);
        Ok(())
    }

    #[test]
    fn test_retention_since() -> Fallible<()> {
        let db = seed_stats_db()?;
        // All three live reviews: 2 of 3 remembered.
        let since = Date::try_from("2026-08-01".to_string())?;
        assert_eq!(db.retention_since(since)?, Some(2.0 / 3.0));
        // Only 08-30: forgot + easy = 1 of 2.
        let since = Date::try_from("2026-08-30".to_string())?;
        assert_eq!(db.retention_since(since)?, Some(0.5));
        // Empty window.
        let since = Date::try_from("2026-09-01".to_string())?;
        assert_eq!(db.retention_since(since)?, None);
        Ok(())
    }
}
