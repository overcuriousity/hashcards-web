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

// Nothing outside this module's own tests reaches `UserDatabase` yet: the
// startup merge and the collection view that use it land in the next two
// commits. `expect` rather than `allow`, so that clippy asks for this line
// back once they do.
#![expect(dead_code)]

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use rusqlite::Connection;
use rusqlite::config::DbConfig;

use crate::db::SCHEMA_VERSION;
use crate::db::ensure_version_table;
use crate::db::get_schema_version;
use crate::db::probe_schema_exists;
use crate::db::set_schema_version;
use crate::error::Fallible;
use crate::error::fail;

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

    pub fn path(&self) -> &Path {
        &self.path
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::LEGACY_SCHEMA_VERSION;

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
}
