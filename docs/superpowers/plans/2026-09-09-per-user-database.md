# One Database Per User Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the review database per *collection* with one per *user*, carrying `collection_id` as a column, and merge every existing per-collection database into it at startup without deleting anything.

**Architecture:** Three movements. First the groundwork (Tasks 1–3): a `CollectionId` newtype, a schema version 8 that only the new `UserDatabase` knows how to create, and a standalone startup merge that folds N old files into one new one — each landing green and tested on its own, none of them yet reachable from a request. Then the pivot (Task 4): `Database` becomes a *view* of one collection on a `UserDatabase`'s shared connection, `ResolvedCollection` carries the collection's id and the user's database path, and every call site follows. Then the consequences (Tasks 5–9): wiring the merge into startup behind a failure gate, sweeping sessions per user, deleting a collection's rows instead of its file, one connection for the landing page, and the docs.

**Tech Stack:** Rust 2024, rusqlite 0.39 (bundled SQLite), parking_lot, axum 0.8, maud, blake3, tokio.

**Spec:** `docs/superpowers/specs/2026-09-08-per-user-database-design.md` — read it before starting. Every task below argues from it. The handoff note that sent this plan into being is `docs/superpowers/specs/2026-09-08-mcp-server-handoff.md`; the MCP server it describes is **out of scope here** and is not to be built, started, or designed for beyond what this spec already says.

## Global Constraints

Copied from `CLAUDE.md` and the spec; these apply to every task and are not repeated per task.

- No `unwrap()` in production code. Tests may use it.
- Error handling is `Fallible<T>` and `?`. Create errors with `fail(...)`, which returns `Err`. All error messages are user-facing: write them for the person reading the page.
- Newtypes for domain concepts. Keep functions small and focused. Module files re-export what is needed and hide the rest.
- Prefer imports to fully qualified names: add `use foo::bar;` rather than writing `foo::bar()`.
- When fixing a bug, write the failing regression test **first**. Every task below is ordered test-first for the same reason.
- Cloze deletion positions are **byte** positions. Use `.bytes()`, never `.chars()`.
- Dates are naive on purpose. No timezones.
- New `CHANGELOG.xml` entries go in the `<unreleased>` block, under `<changed>`, `<added>`, `<fixed>`, `<removed>` or `<breaking>`, each as `<change author="claude">…</change>`. Task 9 writes them; earlier tasks do not touch the file.
- Verification commands, run at the end of every task:
  - `cargo fmt`
  - `cargo clippy --all-targets -- -D warnings`
  - `cargo test`
- **Baseline: 454 tests pass** on `master` at commit `fb2bee9`. Record the number after each task. It must never go down except where this plan says a test is deleted or rewritten, and the drop must be accounted for exactly.
- **Never widen a lock.** `Database` holds a `parking_lot::Mutex<Connection>`, which is **not reentrant**: a `&self` method that locks and then calls another `&self` method that locks will deadlock the process with no timeout and no error. Every public method on `Database` takes the lock exactly once, at the top, and then calls only free functions that accept `&Connection` or `&Transaction`. This rule is load-bearing; check it on every method you touch. The current code violates it in spirit already — `insert_card` calls `self.card_exists()` — so this is not hypothetical.
- **Naming, fixed across the plan.** `UserDatabase` is one user's file. `Database` is a view of one collection inside it. `CollectionId` is the id string from a folder's `.hashcards.toml`. `tree` means a directory under `{data_dir}/cards/`, and its `tree name` is that directory's file name (`default`, or `{email-slug}-{8 hex}`).
- **Opening a database by path in a test**, once Task 4 has landed, is always these two lines — there is no shorter form and no `Database::new` any more:

```rust
let id = collection_id(&folder)?;               // crate::cmd::serve::cards::collection_id
let db = UserDatabase::open(&db_path)?.collection(id);
```

## Deviations from the spec

Four things in the spec do not survive contact with the code. Each is resolved here, and the resolution is the instruction.

1. **`UserDatabase` does not compute the landing page's counts itself.** The spec says it "answers the two questions that are not about a single collection: per-collection counts for the landing page, and closing dangling sessions". The second is implemented as stated. The first cannot be: `compute_collection_counts` counts *parsed* cards looked up in the database, not rows — for the reason `gather_stats` gives at `src/cmd/stats_page.rs:49`, a row left behind by a deleted card must not be counted and a card with no row yet must be. SQL alone cannot answer it. Task 8 therefore satisfies the spec's actual requirement — "one connection instead of N" — by opening the user's `UserDatabase` once and reusing it across the loop, not by adding an aggregate query.

2. **The merge copies rows through Rust, not through `ATTACH`.** `ATTACH` cannot run inside a transaction, and SQLite's default `SQLITE_MAX_ATTACHED` is 10, so attaching every source up front would cap a user at ten collections. Sources are read on their own connection and inserted into the target's single transaction. This is what the spec's "an old→new map held in Rust" already implies for sessions; Task 3 does the same for the other three tables.

3. **`Database::new(&str)` becomes `UserDatabase::open(&Path)`, not `UserDatabase::open(&str)`.** `rusqlite::Connection::open` takes `AsRef<Path>`. The `&str` signature is the only reason several call sites carry a "the database path is not valid UTF-8" branch (`src/cmd/serve/server.rs:118`, `src/cmd/serve/handlers.rs:148`, `src/cmd/serve/files.rs:847`, `src/cmd/serve/edit.rs:381`, and four in tests). Task 4 deletes all of them. This is a reduction, not scope creep: every one of those call sites has to change anyway.

4. **`Database::close_dangling_sessions` survives Task 4 and dies in Task 6.** The spec moves the sweep to `UserDatabase` wholesale. Doing that inside Task 4 would drag the sweep, `AppState.interrupted_closed` and the FEAT-03 notice into an already large task. Task 4 keeps the method and adds `and collection_id = ?` to it, so the sweep keeps working unchanged (opening the user's file once per collection, which is wasteful but correct); Task 6 replaces it with the per-user version and deletes the scoped one.

## Order, and the one state you must not ship

Tasks 1–3 are additive: nothing reads the new code yet, and `master` stays shippable after each. **Task 4 is not shippable on its own.** At the end of it the server writes to `{data_dir}/db/{tree}.db` and an existing install's `{data_dir}/db/{id}.db` files are simply ignored — every user's history appears to vanish. Task 5 wires the merge in and is what makes the pair whole. Do not release, tag, or merge to a deployed branch between Task 4 and Task 5.

---

## Task 1: The `CollectionId` newtype

A collection's id travels as a bare `String` through `collection_id()`, `existing_collection_id()`, `salvage_id()`, a database file name, and — after Task 4 — every query in `db.rs`. `CLAUDE.md` asks for newtypes on domain concepts, and this is one. Introducing it first means Tasks 2–4 can take it as given.

**Files:**
- Create: `src/types/collection_id.rs`
- Modify: `src/types/mod.rs` (add `pub mod collection_id;`)
- Modify: `src/cmd/serve/cards.rs` (`salvage_id`, `collection_id`, `existing_collection_id`, `folder_id`)
- Modify: `src/cmd/serve/files.rs:565` and `:578` (`remove_collection_database` takes `&CollectionId`)

**Interfaces:**
- Consumes: nothing.
- Produces, in `crate::types::collection_id`:
  - `pub struct CollectionId` — `Clone, PartialEq, Eq, Hash, Debug`
  - `pub fn CollectionId::new(value: impl Into<String>) -> Fallible<Self>`
  - `pub fn CollectionId::as_str(&self) -> &str`
  - `impl Display for CollectionId`, `impl ToSql for CollectionId`, `impl FromSql for CollectionId`
- And, changed in `crate::cmd::serve::cards`:
  - `pub fn collection_id(folder: &Path) -> Fallible<CollectionId>`
  - `pub fn existing_collection_id(folder: &Path) -> Fallible<Option<CollectionId>>`

- [ ] **Step 1: Write the failing test**

Create `src/types/collection_id.rs` with the test module only for now, so the first `cargo test` fails against a type that does not exist yet:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// The id is what scopes a user's review rows. An empty one would scope
    /// every query to nothing, which reads as "this collection has no
    /// history" rather than as an error — so it is refused at construction.
    #[test]
    fn an_empty_id_is_refused() {
        assert!(CollectionId::new("").is_err());
        assert!(CollectionId::new("   ").is_err());
    }

    #[test]
    fn an_id_round_trips_through_sqlite() -> Fallible<()> {
        let conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch("create table t (id text not null) strict;")?;
        let id = CollectionId::new("a1b2c3d4")?;
        conn.execute("insert into t (id) values (?);", rusqlite::params![id])?;
        let back: CollectionId = conn.query_row("select id from t;", [], |row| row.get(0))?;
        assert_eq!(back, id);
        assert_eq!(back.as_str(), "a1b2c3d4");
        assert_eq!(back.to_string(), "a1b2c3d4");
        Ok(())
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test collection_id`
Expected: FAIL — `cannot find type CollectionId in this scope` (the module is not declared either).

- [ ] **Step 3: Write the type**

Prepend to `src/types/collection_id.rs`, above the test module:

```rust
use std::fmt::Display;
use std::fmt::Formatter;

use rusqlite::ToSql;
use rusqlite::types::FromSql;
use rusqlite::types::FromSqlError;
use rusqlite::types::FromSqlResult;
use rusqlite::types::ToSqlOutput;
use rusqlite::types::ValueRef;

use crate::error::ErrorReport;
use crate::error::Fallible;
use crate::error::fail;

/// The stable id of a collection: the string in its `.hashcards.toml`.
///
/// Minted from the clock and the process id when a folder is first seen, so
/// renaming the folder cannot change it. It scopes every row a collection
/// owns inside its user's review database, which is why an empty one is
/// refused here rather than silently matching nothing.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct CollectionId {
    inner: String,
}

impl CollectionId {
    pub fn new(value: impl Into<String>) -> Fallible<Self> {
        let inner: String = value.into();
        if inner.trim().is_empty() {
            return fail("A collection's id cannot be empty.");
        }
        Ok(Self { inner })
    }

    pub fn as_str(&self) -> &str {
        &self.inner
    }
}

impl Display for CollectionId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.inner)
    }
}

impl ToSql for CollectionId {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.inner.as_str()))
    }
}

impl FromSql for CollectionId {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let string: String = FromSql::column_result(value)?;
        CollectionId::new(string).map_err(|e: ErrorReport| FromSqlError::Other(Box::new(e)))
    }
}
```

Add to `src/types/mod.rs`, in alphabetical order after `pub mod card_hash;`:

```rust
pub mod collection_id;
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test collection_id`
Expected: PASS (2 tests).

- [ ] **Step 5: Thread the newtype through `cards.rs`**

In `src/cmd/serve/cards.rs`, add `use crate::types::collection_id::CollectionId;` and change four functions.

```rust
/// The `id` of a metadata file the TOML parser has rejected.
///
/// The id is always written as one plain `id = "..."` line, so it stays
/// legible however badly the rest of the file has been mangled. Nothing else
/// is recovered this way: the overrides are preferences and can wait for the
/// file to be fixed, whereas the id cannot be guessed again.
fn salvage_id(text: &str) -> Option<CollectionId> {
    text.lines().find_map(|line| {
        let value = line.trim_start().strip_prefix("id")?.trim_start();
        let value = value.strip_prefix('=')?.trim();
        let quote = value.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        let id = value[quote.len_utf8()..].split(quote).next()?;
        CollectionId::new(id).ok()
    })
}
```

`collection_id` keeps its body and wraps the minted string once at the end:

```rust
pub fn collection_id(folder: &Path) -> Fallible<CollectionId> {
    if let Some(id) = existing_collection_id(folder)? {
        return Ok(id);
    }
    let meta_path = folder.join(COLLECTION_META_FILE);
    let id = fresh_id(folder)?;
    let meta = CollectionMeta {
        id: id.clone(),
        desired_retention: None,
        max_interval_days: None,
    };
    write(&meta_path, toml::to_string(&meta)?)?;
    CollectionId::new(id)
}
```

`existing_collection_id` returns `Fallible<Option<CollectionId>>`. Its salvage branch already yields a `CollectionId`, so that becomes `return Ok(Some(id));`, and its tail becomes:

```rust
    if meta.id.is_empty() {
        return Ok(None);
    }
    Ok(Some(CollectionId::new(meta.id)?))
```

`folder_id` changes only its return type: `fn folder_id(path: &Path, policy: IdPolicy) -> Fallible<Option<CollectionId>>`.

`CollectionMeta.id` stays a `String`: it is the serde shape of the file on disk, not the domain type.

- [ ] **Step 6: Fix the call sites the type change touches**

`discover_local_collections` builds the file name through `Display`, so its line is unchanged in text:

```rust
            db_path: db_dir.join(format!("{id}.db")),
```

Change these:

| Site | Change |
|---|---|
| `src/cmd/serve/files.rs:578` | signature becomes `fn remove_collection_database(state: &AppState, id: &CollectionId) -> Fallible<()>`; the body's `format!("{id}.db{suffix}")` is unchanged; add `use crate::types::collection_id::CollectionId;` to the file |
| `src/cmd/serve/files.rs:565` | `remove_collection_database(state, &id)?` — unchanged text, `id` is now a `CollectionId` |
| `src/cmd/serve/files.rs:633` | `db_path_for`'s `format!("{}.db", collection_id(coll_dir)?)` — unchanged text |
| `src/cmd/serve/server.rs:466`, `src/cmd/serve/mod.rs:81` | tests that bind the id and interpolate it into a file name — unchanged text |
| `src/cmd/serve/cards.rs:594-623`, `:651-802` | tests compare ids with `assert_eq!`/`assert_ne!`; `CollectionId` derives `PartialEq` and `Debug`, so they compile unchanged |
| `src/cmd/serve/auth.rs:659`, `src/cmd/serve/export.rs:409`, `src/cmd/serve/handlers.rs:1090` | `collection_id(&folder)?;` called for effect only — unchanged |

- [ ] **Step 7: Run the whole suite**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: PASS, **456 tests** (454 + the 2 new ones).

- [ ] **Step 8: Commit**

```bash
git add src/types/collection_id.rs src/types/mod.rs src/cmd/serve/cards.rs src/cmd/serve/files.rs
git commit -m "feat: a CollectionId newtype for the id in .hashcards.toml

The id names a collection's review database today and will scope every
one of its rows tomorrow. It travelled as a bare String through four
functions and a file name; it is a domain concept, so it gets a type.
An empty id is refused at construction: scoped to nothing, it would read
as an empty history rather than as an error."
```

---

## Task 2: Schema version 8, and a ladder that only knows the old shape

Two schemas now exist and must never be confused. `src/schema.sql` becomes the **user** database at version 8, created only by `UserDatabase`. `src/schema_v7.sql` is the **per-collection** shape as deployed, created by nothing and read only by tests and the merge's fixtures. The `migrate` ladder is capped at 7 and renamed, so nothing can drive a legacy file to 8 in place: the spec is explicit that consolidation "does not fit the `migrate` ladder", which alters one file, whereas this merges N into one.

At the end of this task `UserDatabase` exists and is tested, and `Database` is untouched and still opens per-collection files at version 7.

**Files:**
- Create: `src/schema_v7.sql` (byte-for-byte today's `src/schema.sql`)
- Create: `src/user_db.rs`
- Modify: `src/schema.sql` (rewritten at version 8)
- Modify: `src/db.rs` (`SCHEMA_VERSION` → `LEGACY_SCHEMA_VERSION`, `migrate` → `migrate_legacy`, `Database::new` creates from `schema_v7.sql`, add `open_legacy_source`, add `test_support`)
- Modify: `src/main.rs` (add `mod user_db;`)

**Interfaces:**
- Consumes: `CollectionId` from Task 1 (only in doc comments here; `UserDatabase::collection` arrives in Task 4).
- Produces:
  - `crate::user_db::UserDatabase`, with `pub fn open(path: &Path) -> Fallible<Self>`, `#[cfg(test)] pub fn memory() -> Fallible<Self>`, `pub fn path(&self) -> &Path`, and `pub(crate) fn with_connection<T>(&self, f: impl FnOnce(&mut Connection) -> Fallible<T>) -> Fallible<T>`
  - `crate::db::SCHEMA_VERSION: i64 = 8` and `crate::db::LEGACY_SCHEMA_VERSION: i64 = 7`
  - `crate::db::open_legacy_source(path: &Path) -> Fallible<Connection>`
  - `crate::db::test_support::{create_legacy_v0, create_legacy_v7}`, both `fn(&Path) -> Fallible<Connection>`

- [ ] **Step 1: Write the failing tests**

Create `src/user_db.rs` containing only its test module for now:

```rust
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test user_db`
Expected: FAIL — the module is not declared and `UserDatabase` does not exist.

- [ ] **Step 3: Split the two schemas**

```bash
cp src/schema.sql src/schema_v7.sql
```

Then replace `src/schema.sql` entirely with the version-8 shape:

```sql
pragma foreign_keys = on;

-- One user's review database. Every table is scoped by `collection_id`, the
-- stable id in a collection folder's `.hashcards.toml`. The grain used to be
-- one file per collection, which made moving a card between collections a
-- cross-database row transfer and left a database nothing could attribute
-- behind whenever a folder was deleted from outside the application.

create table cards (
    collection_id text not null,
    card_hash     text not null,
    added_at text not null,
    last_reviewed_at text,
    stability real,
    difficulty real,
    interval_raw real,
    interval_days integer,
    due_date text,
    review_count integer not null,
    -- Identical cards in two collections keep two schedules, which is
    -- exactly what two files meant. This is also the key the reviews and
    -- bookmarks foreign keys cascade through.
    primary key (collection_id, card_hash)
) strict;

create table sessions (
    session_id integer primary key,
    -- New information: a session used to be identified by which file it was
    -- written in.
    collection_id text not null,
    started_at text not null,
    ended_at text not null,
    -- Heartbeat: stamped whenever the owning process serves a page or
    -- handles an action. Lets the startup sweep tell a session abandoned by
    -- a crash from one still live in another process.
    last_seen_at text,
    -- Explicit "this row has been closed" marker. `ended_at = started_at`
    -- cannot serve as one: a session whose reviews were all undone is
    -- rewritten back to that value and would be re-detected forever.
    closed integer not null default 0
) strict;

create table reviews (
    review_id integer primary key,
    session_id integer not null
        references sessions (session_id)
        on update cascade
        on delete cascade,
    collection_id text not null,
    card_hash text not null,
    reviewed_at text not null,
    grade text not null,
    stability real not null,
    difficulty real not null,
    interval_raw real not null,
    interval_days integer not null,
    due_date text not null,
    duration_ms integer,
    voided integer not null default 0,
    reviewed_date text generated always as (substr(reviewed_at, 1, 10)) virtual,
    -- ON UPDATE CASCADE is load-bearing twice over: an edit renames a card's
    -- hash and relies on it to carry the reviews across, and moving a card
    -- between collections will change `collection_id` the same way.
    foreign key (collection_id, card_hash)
        references cards (collection_id, card_hash)
        on update cascade
        on delete cascade
) strict;

create table bookmarks (
    collection_id text not null,
    card_hash text not null,
    note text,
    created_at text not null,
    primary key (collection_id, card_hash),
    foreign key (collection_id, card_hash)
        references cards (collection_id, card_hash)
        on update cascade
        on delete cascade
) strict;

create index idx_reviews_card on reviews (collection_id, card_hash);
create index idx_reviews_session_id on reviews (session_id);
create index idx_reviews_reviewed_date on reviews (collection_id, reviewed_date);
create index idx_cards_due on cards (collection_id, due_date);

create table schema_version (
    version integer not null
) strict;

create table meta (
    key text primary key,
    value text not null
) strict;
```

- [ ] **Step 4: Cap the ladder at 7 and give the merge a way in**

In `src/db.rs`, replace the `SCHEMA_VERSION` constant and its doc comment with two:

```rust
/// The schema version of a user database — one file per user, every table
/// scoped by `collection_id`. Created by `UserDatabase`, never by the
/// migration ladder: consolidation merges N files into one, which an
/// in-place `alter table` cannot express.
pub const SCHEMA_VERSION: i64 = 8;

/// The highest version a per-collection database can be at, and the top of
/// the `migrate_legacy` ladder. Deployed databases sit between 0 and here.
pub const LEGACY_SCHEMA_VERSION: i64 = 7;
```

Rename `fn migrate` to `fn migrate_legacy` and replace every `SCHEMA_VERSION` inside it with `LEGACY_SCHEMA_VERSION`, including in the "please upgrade" message:

```rust
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
```

In `Database::new`, point the creation branch at the v7 schema and the ladder at its new name. This keeps every existing test passing untouched:

```rust
            if !probe_schema_exists(&tx)? {
                tx.execute_batch(include_str!("schema_v7.sql"))?;
                set_schema_version(&tx, LEGACY_SCHEMA_VERSION)?;
            } else {
                migrate_legacy(&tx)?;
            }
```

Make the four schema helpers visible to `user_db.rs` by changing `fn` to `pub(crate) fn` on `probe_schema_exists`, `ensure_version_table`, `get_schema_version` and `set_schema_version`.

Add the merge's entry point, after `Database`'s `impl` block, and `use std::path::Path;` to the file's imports:

```rust
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
```

- [ ] **Step 5: Add the legacy fixtures the merge's tests will need**

Still in `src/db.rs`, above the existing `#[cfg(test)] mod tests`:

```rust
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
```

Then delete the `OLD_SCHEMA` constant from `src/db.rs`'s test module — it is now `test_support::create_legacy_v0` — and rewrite its users:

- `test_migrated_schema_matches_fresh_schema`: replace the block that opens a connection and runs `OLD_SCHEMA` with `crate::db::test_support::create_legacy_v0(&old_path)?;`. `Database::new` still produces the v7 shape on both sides, so the assertion holds. Rename it to `test_legacy_ladder_converges_on_the_v7_schema` and change its failure message to `"migrated schema diverged from schema_v7.sql"`.
- `test_reopening_migrated_db_is_stable`: same substitution.
- `test_populated_legacy_db_migrates`: same substitution, keeping its `execute_batch` of rows on the returned connection.
- `test_newer_schema_version_is_rejected`: its `include_str!("schema.sql")` is now the v8 file. Point it at `schema_v7.sql`; it is testing the legacy ladder's ceiling, and `a_future_schema_version_is_refused` in `user_db.rs` covers the new one.

- [ ] **Step 6: Write `UserDatabase`**

Prepend to `src/user_db.rs`, above its test module:

```rust
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
```

Add `mod user_db;` to `src/main.rs`, between `mod types;` and `mod utils;` so the list stays alphabetical.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test user_db`
Expected: PASS (4 tests).

- [ ] **Step 8: Run the whole suite**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: PASS, **460 tests** (456 + 4).

`UserDatabase::with_connection` has only its own tests as callers at this point. If clippy reports it as dead code, add above it:

```rust
    // Task 3's merge and Task 4's `Database` are its callers.
    #[allow(dead_code)]
```

and delete the attribute in Task 4.

- [ ] **Step 9: Commit**

```bash
git add src/schema.sql src/schema_v7.sql src/user_db.rs src/db.rs src/main.rs
git commit -m "feat: schema version 8, and a ladder that only knows the old shape

Two schemas now exist. schema.sql is one user's database, every table
scoped by collection_id; schema_v7.sql is the per-collection shape as
deployed, which nothing creates any more. The migrate ladder is capped
at 7 and renamed migrate_legacy, because consolidation merges N files
into one and an in-place alter table cannot say that.

UserDatabase opens the new file in WAL where the filesystem allows it,
and refuses both a version from the future and a per-collection file
from the past."
```

---

## Task 3: The startup merge

Fold every `db/{collection-id}.db` into `db/{tree-name}.db`, once, idempotently, without destroying anything. This task builds the module and its tests; **it is not called from anywhere yet** — Task 5 wires it into `start_serve`. Writing it now means the merge is proven before the pivot depends on it.

Idempotence rests on a `meta` row written inside the same transaction as the rows, not on the file move. A crash between commit and move leaves the sources in place, and the next start skips them by marker rather than importing them twice.

**Files:**
- Create: `src/cmd/serve/merge.rs`
- Modify: `src/cmd/serve/mod.rs` (add `mod merge;` between `mod landing;` and `pub mod server;`)

**Interfaces:**
- Consumes: `CollectionId` (Task 1); `UserDatabase::open`, `UserDatabase::with_connection`, `crate::db::open_legacy_source`, `crate::db::test_support::{create_legacy_v0, create_legacy_v7}` (Task 2); `crate::cmd::serve::cards::existing_collection_id`.
- Produces:
  - `pub fn merge_legacy_databases(data_dir: &Path) -> HashMap<PathBuf, String>` — keyed by the *target* user database path, holding the error message for every tree whose merge failed. Empty means every tree is consolidated.

- [ ] **Step 1: Write the failing tests**

Create `src/cmd/serve/merge.rs` with only its test module:

```rust
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
            std::fs::write(folder.join(COLLECTION_META_FILE), format!("id = \"{id}\"\n"))?;
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
        seed(&create_legacy_v7(&source(data_dir, &one))?, "hash-one", "2026-02-01", 2)?;
        seed(&create_legacy_v7(&source(data_dir, &two))?, "hash-two", "2026-03-01", 1)?;

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
        seed(&create_legacy_v7(&source(data_dir, &one))?, "hash-one", "2026-02-01", 2)?;
        seed(&create_legacy_v7(&source(data_dir, &two))?, "hash-two", "2026-03-01", 1)?;

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
        seed(&create_legacy_v7(&source(data_dir, &one))?, "hash-one", "2026-02-01", 2)?;

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
                 (1, 'hash-one', '2026-01-01T09:00:00.000', 'good', 2.0, 5.0, 2.0, 2, '2026-02-01');",
                [],
            )?;
        }

        assert!(merge_legacy_databases(data_dir).is_empty());

        let conn = Connection::open(data_dir.join("db").join(format!("{tree}.db")))?;
        assert_eq!(count(&conn, "select count(*) from reviews;")?, 1);
        let date: String = conn.query_row("select reviewed_date from reviews;", [], |r| r.get(0))?;
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
        seed(&create_legacy_v7(&source(data_dir, &one))?, "hash-one", "2026-02-01", 1)?;

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
        seed(&create_legacy_v7(&source(data_dir, &one))?, "hash-one", "2026-02-01", 1)?;

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
        seed(&create_legacy_v7(&source(data_dir, &one))?, "hash-one", "2026-02-01", 1)?;
        let orphan = source(data_dir, "deadbeef");
        create_legacy_v7(&orphan)?;

        assert!(merge_legacy_databases(data_dir).is_empty());

        assert!(orphan.exists(), "an unattributable database must be left alone");
        assert!(!data_dir.join("db").join("legacy").join("deadbeef.db").exists());
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test merge`
Expected: FAIL — `merge_legacy_databases` does not exist and the module is not declared.

- [ ] **Step 3: Write the merge**

Prepend to `src/cmd/serve/merge.rs`:

```rust
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
```

Add `mod merge;` to `src/cmd/serve/mod.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test merge`
Expected: PASS (8 tests).

If clippy reports `merge_legacy_databases` as dead code, add above it:

```rust
// Wired into start_serve in Task 5.
#[allow(dead_code)]
```

and delete the attribute there.

- [ ] **Step 5: Run the whole suite**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: PASS, **468 tests** (460 + 8).

- [ ] **Step 6: Commit**

```bash
git add src/cmd/serve/merge.rs src/cmd/serve/mod.rs
git commit -m "feat: merge per-collection review databases into one per user

One transaction per user, so a tree is either fully consolidated or
untouched, and one broken file costs one user rather than the server.
Idempotence rests on a meta marker written inside that transaction, not
on the file move: a crash in between leaves the sources in place and the
next start skips them by marker.

Nothing is deleted. Merged sources move into db/legacy/, where an
operator can still read them and an older binary will not find them --
left in place, it would write reviews the next upgrade would skip.

Not called from anywhere yet."
```

---

## Task 4: The pivot — `Database` becomes a view of one collection

This is the change everything else was built for. `Database` stops owning a connection and becomes a scoped view on a `UserDatabase`'s; every query gains `and collection_id = ?`; `ResolvedCollection` learns its collection's id and points at the user's file; and the three transaction methods lose their `&mut`.

**Read the Global Constraints entry on locking before writing a single method.** The mutex is not reentrant, and the code as it stands calls `self.card_exists()` from inside `self.insert_card()`.

**This task is not shippable on its own.** At the end of it a fresh install works completely and an existing install's history is invisible, because nothing has merged it yet. Task 5 closes that. Do not stop here.

**Files:**
- Modify: `src/db.rs` (the whole `Database` impl, `update_card_performance_tx`, the two free helpers, and the test module)
- Modify: `src/user_db.rs` (add `collection`)
- Modify: `src/collection.rs` (`with_db_path` → `open`; `new` follows)
- Modify: `src/cmd/serve/config.rs:238` (`ResolvedCollection` gains `collection_id`)
- Modify: `src/cmd/serve/cards.rs` (`discover_local_collections`, new `user_db_path`)
- Create: `src/cmd/serve/reviewdb.rs`
- Modify: `src/cmd/serve/mod.rs` (add `mod reviewdb;`)
- Modify: `src/cmd/serve/{counts,browse,stats,export,bookmarks,handlers,edit,files,server,state,decks}.rs`
- Modify: `src/cmd/drill/state.rs` (delete `for_card_mut`), `src/cmd/drill/post.rs`

**Interfaces:**
- Consumes: everything Tasks 1–3 produced.
- Produces:
  - `UserDatabase::collection(&self, id: CollectionId) -> Database`
  - `Database` — `pub struct Database { conn: Arc<Mutex<Connection>>, collection: CollectionId }`, with every existing method's signature unchanged except that `insert_review_and_update_performance`, `void_review_and_restore_performance` and `apply_edit_migration` take `&self`
  - `Database::erase(&self) -> Fallible<()>`
  - `#[cfg(test)] Database::memory() -> Fallible<Database>`
  - `ResolvedCollection.collection_id: CollectionId`, and `ResolvedCollection.db_path` now the user's file
  - `pub fn crate::cmd::serve::cards::user_db_path(root: &CardRoot, db_dir: &Path) -> Fallible<PathBuf>`
  - `pub fn crate::cmd::serve::files::db_target_for(root: &CardRoot, coll_dir: &Path, db_dir: &Path) -> Fallible<(PathBuf, CollectionId)>`
  - `pub fn crate::cmd::serve::reviewdb::open_collection_db(rc: &ResolvedCollection) -> Fallible<Database>`
  - `pub fn crate::collection::Collection::open(directory: PathBuf, db: Database) -> Fallible<Collection>`

- [ ] **Step 1: Write the failing schema tests**

Add to `src/db.rs`'s test module. These are the spec's three schema tests, and they are the proof that scoping works:

```rust
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
        biology.insert_review_immediately(session, &review_for(old, 2.0, now))?;

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
```

`review_for` is the existing helper in that module (its doc comment reads "Build a review record for `card_hash` with the given stability", around `src/db.rs:1100`). Read its exact name and parameter order before calling it, and use it as it stands rather than adding another.

Add `use crate::types::collection_id::CollectionId;` and `use crate::user_db::UserDatabase;` to the test module's imports.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib db::tests`
Expected: FAIL to compile — `UserDatabase::memory().collection(..)`, `Database::erase` and `&self` on `apply_edit_migration` do not exist.

- [ ] **Step 3: Rescope the type**

In `src/db.rs`, replace the struct and open the impl with the two constructors:

```rust
/// A view of one collection inside a user's review database.
///
/// Scoped, not separate: the connection is shared with every other view on
/// the same file, so a saved deck spanning three collections does not open
/// three writers that contend on every grade.
pub struct Database {
    conn: Arc<Mutex<Connection>>,
    collection: CollectionId,
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
```

Delete `Database::new` and its doc comment. Add `use std::sync::Arc;`, `use parking_lot::Mutex;` and `use crate::types::collection_id::CollectionId;` to the file's imports. `BUSY_TIMEOUT` and `DbConfig` stay — `open_legacy_source` still uses them.

In `src/user_db.rs`, add to the `impl UserDatabase` block, with `use crate::db::Database;` and `use crate::types::collection_id::CollectionId;`:

```rust
    /// A view of one collection on this database's connection.
    pub fn collection(&self, id: CollectionId) -> Database {
        Database::new_view(Arc::clone(&self.conn), id)
    }
```

- [ ] **Step 4: Rewrite every query, one lock per method**

Change the two free helpers to carry the collection and take a `&Connection`:

```rust
fn card_exists_in(
    conn: &Connection,
    collection: &CollectionId,
    card_hash: CardHash,
) -> Fallible<bool> {
    let sql = "select count(*) from cards where collection_id = ? and card_hash = ?;";
    let count: i64 = conn.query_row(sql, params![collection, card_hash], |row| row.get(0))?;
    Ok(count > 0)
}

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
```

and give `update_card_performance_tx` a `collection: &CollectionId` parameter, with `and collection_id = ?` on its `update cards set ... where`.

Then work down the `impl Database` block. Every method opens with `let conn = self.conn.lock();` — `let mut conn` where a transaction is taken — and uses `conn` where it used `self.conn`. `insert_card` is the one that would otherwise deadlock, and is the shape to copy:

```rust
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
```

The complete list. Every collection placeholder binds `self.collection` and comes first in the parameter list:

| Method | Scoping |
|---|---|
| `insert_card` | as above |
| `card_hashes` | `where collection_id = ?` |
| `due_today` | `where collection_id = ?` |
| `get_card_performance_opt` | `where collection_id = ? and card_hash = ?` |
| `get_card_performance` | unchanged — it calls `get_card_performance_opt` and takes no lock of its own |
| `update_card_performance` (`#[cfg(test)]`) | existence check and `update ... where collection_id = ? and card_hash = ?`, under one lock |
| `create_session` | `insert into sessions (collection_id, started_at, ended_at, last_seen_at) values (?, ?, ?, ?) returning session_id` |
| `insert_review_and_update_performance` | `&self`; the insert gains `collection_id`; calls `update_card_performance_tx(&tx, &self.collection, ..)` |
| `void_review_and_restore_performance` | `&self`; `update reviews set voided = 1 where review_id = ? and collection_id = ? and card_hash = ?`; the session reopen gains `and collection_id = ?` |
| `insert_review_immediately` (`#[cfg(test)]`) | the insert gains `collection_id` |
| `close_session` | `where session_id = ? and collection_id = ?` |
| `due_dates` | `where collection_id = ?` |
| `count_reviews_per_day_since` | `where collection_id = ? and voided = 0 and reviewed_date >= ?` |
| `grade_distribution` | `where collection_id = ? and voided = 0` |
| `retention_since` | `where collection_id = ? and voided = 0 and reviewed_date >= ?` |
| `touch_session` | `where session_id = ? and collection_id = ?` |
| `close_dangling_sessions` | `... where collection_id = ? and closed = 0 and coalesce(last_seen_at, started_at) < ?` — kept for now, deleted in Task 6 |
| `apply_edit_migration` | `&self`; the helpers take `&self.collection`; `update cards set card_hash = ? where collection_id = ? and card_hash = ?` |
| `delete_card` | `where collection_id = ? and card_hash = ?` |
| `card_exists` | delegates to `card_exists_in` under one lock |
| `insert_card_if_new` | delegates to `insert_card_if_new_in` under one lock |
| `bookmark_exists` | `where collection_id = ? and card_hash = ?` |
| `insert_bookmark` | `insert into bookmarks (collection_id, card_hash, note, created_at) values (?, ?, ?, ?) on conflict (collection_id, card_hash) do nothing` |
| `delete_bookmark` | `where collection_id = ? and card_hash = ?` |
| `get_bookmark` | `where collection_id = ? and card_hash = ?` |
| `list_bookmarks` | `where collection_id = ? order by created_at desc` |
| `update_bookmark_note` | `where collection_id = ? and card_hash = ?` |
| `count_bookmarks` | `where collection_id = ?` |
| `count_reviews_in_date` | `where collection_id = ? and reviewed_date = ? and voided = 0` |
| `get_all_sessions` | `where collection_id = ? order by started_at` |
| `get_reviews_for_session` | `where session_id = ? and collection_id = ? and voided = 0 order by reviewed_at` |

- [ ] **Step 5: Add `Database::erase`**

At the end of the `impl Database` block:

```rust
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
```

- [ ] **Step 6: Update `db.rs`'s own tests**

Every `Database::new(":memory:")` in that test module becomes `Database::memory()`, and every `let mut db` that was mutable only for a transaction method becomes `let db`. The four tests that open a file by path exercise the *legacy* ladder and must go through `open_legacy_source` rather than `Database::new`; `schema_snapshot(&migrated.conn)` becomes `schema_snapshot(&migrated)`, since those functions now hand back a raw `Connection`.

- [ ] **Step 7: Run the db tests**

Run: `cargo test --lib db::`
Expected: PASS. **If a test hangs rather than failing, a method is taking the lock twice.** Find it and flatten it.

- [ ] **Step 8: Give `ResolvedCollection` the id and the user's file**

In `src/cmd/serve/config.rs`, with `use crate::types::collection_id::CollectionId;`:

```rust
pub struct ResolvedCollection {
    pub name: String,
    pub slug: String,
    pub coll_dir: PathBuf,
    /// The owning user's review database, shared by every collection in
    /// their card tree. Which rows in it belong to this collection is
    /// `collection_id`, not the file name.
    pub db_path: PathBuf,
    pub collection_id: CollectionId,
    /// Owning user's email (lowercased), when `[oidc]` is configured.
    pub owner: Option<String>,
    /// Scheduling this collection asks for in place of the instance's.
    pub overrides: SchedulingOverrides,
}
```

In `src/cmd/serve/cards.rs`, add:

```rust
/// The review database of the user whose tree this is.
///
/// One file per tree, named for the tree's own directory — which is
/// `default` or `{email-slug}-{8 hex}`, and so can never collide with a
/// collection id, which is eight hex characters.
pub fn user_db_path(root: &CardRoot, db_dir: &Path) -> Fallible<PathBuf> {
    let name = root
        .path()
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| ErrorReport::new("the card folder has no readable name"))?;
    Ok(db_dir.join(format!("{name}.db")))
}
```

and in `discover_local_collections`, compute it once before the loop:

```rust
    let db_path = user_db_path(root, db_dir)?;
```

with the push becoming:

```rust
        collections.push(ResolvedCollection {
            slug,
            name,
            overrides: collection_overrides(&path),
            coll_dir: path,
            db_path: db_path.clone(),
            collection_id: id,
            owner: owner.map(|o| o.to_lowercase()),
        });
```

Two tests in that file assert on `db_path`. `discovered_db_path_is_named_from_the_id_not_the_slug` (line 651) becomes `discovered_db_path_is_the_users_file_not_the_collections` and asserts `db_dir.join("default.db")`; the one at line 724 asserts `dir.join("db").join("default.db")`. The test at line 774, which asserts that a second user's collection gets a different path, keeps its point and its shape — two trees, two files.

- [ ] **Step 9: One way in for handlers**

Create `src/cmd/serve/reviewdb.rs`:

```rust
//! Opening a collection's review database.
//!
//! One function, so that every handler resolves a collection to rows the
//! same way — and so that Task 5's check for a user whose startup merge
//! failed has exactly one place to live.

use crate::cmd::serve::config::ResolvedCollection;
use crate::db::Database;
use crate::error::Fallible;
use crate::user_db::UserDatabase;

/// The rows belonging to `rc`, inside its owner's review database.
pub fn open_collection_db(rc: &ResolvedCollection) -> Fallible<Database> {
    Ok(UserDatabase::open(&rc.db_path)?.collection(rc.collection_id.clone()))
}
```

and `mod reviewdb;` in `src/cmd/serve/mod.rs`, after `mod merge;`.

- [ ] **Step 10: Change `Collection` to be handed its database**

In `src/collection.rs`, rename `with_db_path` and change its signature:

```rust
    /// Load a collection from `directory`, reading its schedule out of `db`.
    ///
    /// The database is passed in rather than opened here: a session drawing
    /// on several collections shares one connection across all of them, and
    /// only the caller knows which.
    pub fn open(directory: PathBuf, db: Database) -> Fallible<Self> {
```

The body is the old one with the three lines that turned a path into a `Database` deleted. `Collection::new` (still `#[cfg(test)]`) becomes:

```rust
    #[cfg(test)]
    pub fn new(directory: Option<String>) -> Fallible<Self> {
        let directory: PathBuf = match directory {
            Some(dir) => PathBuf::from(dir),
            None => current_dir()?,
        };
        let directory: PathBuf = if directory.exists() {
            directory.canonicalize()?
        } else {
            return fail("directory does not exist.");
        };
        let db = UserDatabase::open(&directory.join("hashcards.db"))?
            .collection(CollectionId::new("test-collection")?);
        Self::open(directory, db)
    }
```

`test_reloading_a_collection_keeps_card_hashes` relies on two `Collection::new` calls seeing the same rows; with a fixed id and the same file, it still does.

- [ ] **Step 11: Follow the change through the server**

Each of these opens a database or builds a `Collection`. The rule is the same everywhere: `let db = open_collection_db(&rc)?;` then `Collection::open(rc.coll_dir.clone(), db)?`.

| File | Site | Change |
|---|---|---|
| `src/cmd/serve/counts.rs:22,44` | `compute_collection_counts(coll_dir, db_path)` | signature becomes `compute_collection_counts(coll_dir: &Path, db: Database) -> Fallible<(usize, usize)>`; `refresh_collection_info` calls `open_collection_db(rc)?` per collection for now — Task 8 hoists it |
| `src/cmd/serve/browse.rs:83` | `build_deck_tree(coll_dir, db_path)` | becomes `build_deck_tree(coll_dir: &Path, db: Database) -> Fallible<BrowseData>` |
| `src/cmd/serve/stats.rs:59` | `Collection::with_db_path` | `Collection::open(rc.coll_dir.clone(), open_collection_db(&rc)?)?` |
| `src/cmd/serve/export.rs:117` | same | same |
| `src/cmd/serve/bookmarks.rs:55,211,253` | same, three times | same |
| `src/cmd/serve/handlers.rs:147-155` | `build_deck_tree` + `Database::new` + the UTF-8 branch | `build_deck_tree` consumes its `Database`, so call `open_collection_db(&rc)?` twice: once for the tree, once for `count_bookmarks`. The UTF-8 branch goes |
| `src/cmd/serve/handlers.rs:509` | `Collection::with_db_path` in `create_session_from_sources` | see Step 12 |
| `src/cmd/serve/edit.rs:381-384` | `rc.db_path.to_str()` + `Database::new` + `let mut db` | `let db = open_collection_db(&rc)?;`; the UTF-8 branch goes |
| `src/cmd/serve/files.rs:841-858` | `db_path_for` + UTF-8 branch + `let mut db` | `db_path_for` becomes `db_target_for(root: &CardRoot, coll_dir: &Path, db_dir: &Path) -> Fallible<(PathBuf, CollectionId)>` returning `(user_db_path(root, db_dir)?, collection_id(coll_dir)?)`; then `let db = UserDatabase::open(&path)?.collection(id);` |
| `src/cmd/serve/files.rs:892,959` | `any_card_has_history(db_path_str, &old_cards)` | becomes `any_card_has_history(&db, &old_cards)`, taking `&Database` |
| `src/cmd/serve/server.rs:114-127` | the sweep | `Database::new(db_path)` becomes `open_collection_db(rc)`; the UTF-8 branch goes. `discover_all_collections` already yields `ResolvedCollection`s carrying both halves after Step 8 |

- [ ] **Step 12: Share one connection across a multi-collection session**

In `create_session_from_sources` (`src/cmd/serve/handlers.rs:487`), a saved deck's collections all belong to one user and so all name one file. Opening a `UserDatabase` per source would put three writers on it. Before the loop, with `use std::collections::hash_map::Entry;`:

```rust
    // Every collection a deck can draw on belongs to one user, so they all
    // name one file — but keyed by path rather than assumed, so a deck that
    // one day spanned two users would still be correct rather than silently
    // routed to the wrong database.
    let mut opened: HashMap<PathBuf, UserDatabase> = HashMap::new();
```

and inside the loop, in place of the `Collection::with_db_path` line:

```rust
        let user_db = match opened.entry(rc.db_path.clone()) {
            Entry::Occupied(slot) => slot.into_mut(),
            Entry::Vacant(slot) => slot.insert(UserDatabase::open(&rc.db_path)?),
        };
        let collection = Collection::open(
            rc.coll_dir.clone(),
            user_db.collection(rc.collection_id.clone()),
        )?;
```

- [ ] **Step 13: Drop `for_card_mut`**

The three transaction methods take `&self` now, so `SessionDbs::for_card_mut` (`src/cmd/drill/state.rs:99`) has no callers. Delete it and its doc comment, and change `mutable.dbs.for_card_mut(hash)` to `mutable.dbs.for_card(hash)` at `src/cmd/drill/post.rs:133` and `:245`.

- [ ] **Step 14: Update the remaining call sites**

Every remaining `Database::new(":memory:")` call becomes `Database::memory()`; the file-backed ones follow the two-line pattern in the Global Constraints. The full list, so none is missed:

| File | Sites |
|---|---|
| `src/cmd/stats_page.rs` | 307, 338, 388, 416 — all `:memory:` |
| `src/cmd/drill/state.rs` | 421, 460, 484, 664, 666 — all `:memory:`; 664/666 want two *independent* databases, which two `Database::memory()` calls give |
| `src/cmd/drill/post.rs` | 302, 334, 434, 594 — all `:memory:` |
| `src/cmd/drill/get.rs` | 468, 504, 590 — all `:memory:` |
| `src/cmd/serve/state.rs` | 339, 383 — file-backed, `dir.join("test.db")`, any fixed id |
| `src/cmd/serve/mod.rs` | 694, 719, 1024 — file-backed; the `card_collection` helper (line 62) now returns `(PathBuf, PathBuf, CollectionId)` and its callers bind the id |
| `src/cmd/serve/edit.rs` | 824, 851, 882, 907, 936, 962 — file-backed via `rc.db_path`, id from `rc.collection_id` |
| `src/cmd/serve/handlers.rs` | 1099, 1290, 1296 — file-backed via `rc.db_path`; the literals at 1382 and 1390 need `collection_id: CollectionId::new("one")?` / `("two")?` |
| `src/cmd/serve/files.rs` | 1189, 1196, 1205, 1444, 1501 — file-backed |
| `src/cmd/serve/server.rs` | 472 — file-backed |
| `src/cmd/serve/counts.rs` | 88 — a `ResolvedCollection` literal, needs `collection_id` |
| `src/cmd/serve/decks.rs` | 666 — a `ResolvedCollection` literal, needs `collection_id` |
| `src/cmd/serve/browse.rs` | 476, 494 — `build_deck_tree` now takes a `Database` |

`db_path_comes_from_the_top_level_folder_id` (`src/cmd/serve/files.rs:1111`) now tests something that no longer exists. Rewrite it:

```rust
    /// The database a save writes into is the *user's*, and which rows in it
    /// the save touches is the collection's id. Renaming the folder still
    /// changes neither.
    #[test]
    fn a_save_targets_the_users_database_and_the_folders_id() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let root = CardRoot::for_user(&dir, None)?;
        let folder = root.path().join("Spanish");
        std::fs::create_dir_all(&folder)?;
        std::fs::write(folder.join("verbs.md"), "Q: a\nA: b\n")?;
        let id = crate::cmd::serve::cards::collection_id(&folder)?;

        let db_dir = dir.join("db");
        let (path, found) = db_target_for(
            &root,
            &collection_folder(&root, "Spanish/verbs.md")?,
            &db_dir,
        )?;
        assert_eq!(path, db_dir.join("default.db"));
        assert_eq!(found, id);
        Ok(())
    }
```

- [ ] **Step 15: Run the whole suite**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: PASS, **471 tests** (468 + the 3 new schema tests; the `db_path_comes_from…` test was rewritten, not removed).

Two failure modes to read correctly: a test that **hangs** is the reentrant lock; a test that fails with `FOREIGN KEY constraint failed` is a review or bookmark being inserted for a card row that does not exist *in that collection* — check the scoping of the insert, not the schema.

- [ ] **Step 16: Commit**

```bash
git add -A
git commit -m "refactor: one review database per user, scoped by collection

Database stops owning a connection and becomes a view of one collection
on a UserDatabase's, with 'and collection_id = ?' on every query. The
connection is shared, so a saved deck spanning three collections no
longer opens three writers onto what is now one file -- which also
retires for_card_mut, since the transaction methods can take &self.

ResolvedCollection carries the collection's id and points at the user's
file. Collection is handed its database rather than opening one, because
only the caller knows whether a connection is already open for that user.

Not shippable alone: an existing install's per-collection databases are
not merged until the next commit."
```

---

## Task 5: Wire the merge into startup, behind a gate

Run the merge before the sweep, and make a user whose merge failed impossible to serve from an empty database. That last part is the whole point: the spec says silently starting a user from zero "is the one outcome this design exists to prevent".

**Files:**
- Modify: `src/cmd/serve/server.rs` (`start_serve`, `sweep_dangling_sessions`)
- Modify: `src/cmd/serve/state.rs` (`AppState.migration_failures`, and `test_support::state_with_data_dir`)
- Modify: `src/cmd/serve/reviewdb.rs` (`open_collection_db` takes `&AppState`)
- Modify: every caller of `open_collection_db` from Task 4
- Modify: `src/cmd/serve/mod.rs` (the integration test)

**Interfaces:**
- Consumes: `merge_legacy_databases` (Task 3), `open_collection_db` (Task 4).
- Produces:
  - `AppState.migration_failures: Arc<HashMap<PathBuf, String>>`
  - `pub fn open_collection_db(state: &AppState, rc: &ResolvedCollection) -> Fallible<Database>`

- [ ] **Step 1: Write the failing tests**

The spec's integration test, in `src/cmd/serve/mod.rs`'s test module:

```rust
    /// A server started on a data directory seeded with pre-consolidation
    /// databases serves the review history they hold. This is the upgrade,
    /// end to end.
    #[tokio::test]
    async fn test_legacy_databases_are_merged_at_startup() -> Fallible<()> {
        let port = pick_unused_port().unwrap();
        let dir = tempdir()?;
        let slug = "legacy-collection".to_string();
        let (folder, _db_path, id) =
            card_collection(dir.path(), &slug, &[("Deck.md", "Q: What is 1+1?\nA: 2\n")])?;

        // The card, hashed exactly as the old database would have had it.
        let parsed = crate::parser::parse_deck(&folder)?;
        let hash = parsed.cards[0].hash().to_hex();

        // A pre-consolidation database for that collection, with one review
        // and a due date far in the future.
        {
            let legacy = crate::db::test_support::create_legacy_v7(
                &dir.path().join("db").join(format!("{id}.db")),
            )?;
            legacy.execute(
                "insert into cards (card_hash, added_at, due_date, review_count) \
                 values (?, '2026-01-01T09:00:00.000', '2099-01-01', 1);",
                rusqlite::params![hash],
            )?;
            legacy.execute(
                "insert into sessions (session_id, started_at, ended_at) \
                 values (1, '2026-01-01T09:00:00.000', '2026-01-01T09:30:00.000');",
                [],
            )?;
            legacy.execute(
                "insert into reviews (session_id, card_hash, reviewed_at, grade, stability, \
                 difficulty, interval_raw, interval_days, due_date) values \
                 (1, ?, '2026-01-01T09:00:00.000', 'good', 2.0, 5.0, 2.0, 2, '2099-01-01');",
                rusqlite::params![hash],
            )?;
        }

        serve_data_dir(dir.path(), port).await?;

        // The card is scheduled far in the future, so the collection page
        // must report nothing due — which it can only know from the merged
        // history. Without the merge it would be a brand-new card, and due.
        let body = reqwest::get(format!("http://{TEST_HOST}:{port}/collection/{slug}"))
            .await?
            .text()
            .await?;
        assert!(
            !body.contains("1 due"),
            "the merged review history must be in force: {body}"
        );
        // And the source is kept.
        assert!(
            dir.path()
                .join("db")
                .join("legacy")
                .join(format!("{id}.db"))
                .exists(),
            "the source database must be kept, not deleted"
        );
        Ok(())
    }
```

Before asserting, render the page once by hand (or read `render_browse_page` in `src/cmd/serve/browse.rs`) and confirm the exact due-count wording; substitute whatever string that page actually produces for one card that is due, and keep the assertion negative — it is the one that fails loudly if the merge is skipped.

And in a new test module at the bottom of `src/cmd/serve/reviewdb.rs`:

```rust
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test legacy_databases_are_merged_at_startup merge_failed_is_refused`
Expected: FAIL — `AppState` has no `migration_failures`, and `open_collection_db` takes one argument.

- [ ] **Step 3: Add the failure map to `AppState`**

In `src/cmd/serve/state.rs`, after `interrupted_closed`:

```rust
    /// Users whose startup merge failed, keyed by their review database's
    /// path and holding the error to show them.
    ///
    /// Their collections refuse to open rather than starting from an empty
    /// database. A merge that half-succeeded is a transaction that rolled
    /// back, so no rows were lost — but the sources are still in place and
    /// the target is not what it should be, and serving that as though it
    /// were a fresh account is how a person concludes their history is gone.
    pub migration_failures: Arc<HashMap<PathBuf, String>>,
```

Add `migration_failures: Arc::new(HashMap::new())` to the test constructor at `src/cmd/serve/state.rs:296` and to `test_support::state_with_data_dir`.

- [ ] **Step 4: Gate the opener**

In `src/cmd/serve/reviewdb.rs`:

```rust
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
```

with `use crate::cmd::serve::state::AppState;` and `use crate::error::fail;`.

Pass `state` at every call site from Task 4's Step 11. The sweep in `server.rs` is the exception: it runs before `AppState` exists, so it opens `UserDatabase::open(&rc.db_path)` directly and skips the failure map explicitly — Step 5.

- [ ] **Step 5: Run the merge before the sweep**

In `start_serve` (`src/cmd/serve/server.rs:178`), replace the `interrupted_closed` block with:

```rust
    // The upgrade: fold any pre-consolidation per-collection databases into
    // one database per user. Before the sweep, which opens the databases
    // this produces. A tree whose merge failed is recorded rather than
    // fatal: one broken file must not take an instance down for everybody.
    let migration_failures = match &config.data_dir {
        Some(data_dir) => merge_legacy_databases(data_dir),
        None => HashMap::new(),
    };
    for (path, why) in &migration_failures {
        log::error!(
            "The review database at {} was not consolidated ({why}). The collections that use it \
             will refuse to open until this is fixed; their previous databases have not been \
             deleted.",
            path.display()
        );
    }

    // FEAT-03: close session rows left open by a crash or restart, once, at
    // startup. They cannot be resumed (the card queue lives only in memory),
    // so they are closed with all persisted reviews kept. This must not run
    // per request: the predicate cannot distinguish a crashed session from a
    // live one, and a second server may share the same database.
    let interrupted_closed = match &config.data_dir {
        Some(data_dir) => sweep_dangling_sessions(data_dir, &migration_failures),
        None => HashMap::new(),
    };
```

Add `migration_failures: Arc::new(migration_failures)` to the `AppState` literal, and `use crate::cmd::serve::merge::merge_legacy_databases;` to the imports.

`sweep_dangling_sessions` gains the parameter and skips any collection whose `rc.db_path` is a key of the map:

```rust
fn sweep_dangling_sessions(
    data_dir: &Path,
    failures: &HashMap<PathBuf, String>,
) -> HashMap<PathBuf, usize> {
```

with, inside the per-collection closure, before opening anything:

```rust
                    if failures.contains_key(&rc.db_path) {
                        // Already reported at startup, and the file is not
                        // what it should be. Do not write to it.
                        return (rc.db_path.clone(), Ok(0));
                    }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test legacy_databases_are_merged_at_startup merge_failed_is_refused`
Expected: PASS.

- [ ] **Step 7: Run the whole suite**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: PASS, **473 tests** (471 + 2).

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "feat: consolidate review databases at startup, behind a gate

The merge runs before the dangling-session sweep, and a tree whose merge
failed is recorded against its database path rather than taken as fatal:
every other user is served and the server starts.

Their own collections then refuse to open. An empty database would be
served as an empty history, which is indistinguishable from having lost
everything -- the one outcome the design exists to prevent."
```

---

## Task 6: Sweep sessions per user, and report them per collection

The sweep opens a user's file once per collection, and keys the notice by database path — which after Task 4 means every collection in a tree shares one entry: whichever collection page is opened first takes the notice, and it reports the whole user's count. Fix both.

**Files:**
- Modify: `src/user_db.rs` (add `close_dangling_sessions`)
- Modify: `src/db.rs` (delete `Database::close_dangling_sessions`)
- Modify: `src/cmd/serve/server.rs` (`sweep_dangling_sessions` and its test at line 462)
- Modify: `src/cmd/serve/state.rs` (`interrupted_closed` keyed by `CollectionId`)
- Modify: `src/cmd/serve/handlers.rs:161`

**Interfaces:**
- Consumes: `UserDatabase`, `CollectionId`.
- Produces:
  - `UserDatabase::close_dangling_sessions(&self, stale_before: Timestamp) -> Fallible<HashMap<CollectionId, usize>>`
  - `AppState.interrupted_closed: Arc<Mutex<HashMap<CollectionId, usize>>>`
  - `fn sweep_dangling_sessions(data_dir: &Path, failures: &HashMap<PathBuf, String>) -> HashMap<CollectionId, usize>`

- [ ] **Step 1: Write the failing test**

In `src/user_db.rs`'s test module:

```rust
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
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test dangling_sessions_are_closed_and_counted_per_collection`
Expected: FAIL — no such method on `UserDatabase`.

- [ ] **Step 3: Move the sweep to `UserDatabase`**

```rust
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
```

Add `use std::collections::HashMap;`, `use rusqlite::params;` and `use crate::types::timestamp::Timestamp;` to `src/user_db.rs`.

Delete `Database::close_dangling_sessions` from `src/db.rs`.

- [ ] **Step 4: Rewrite the startup sweep**

`sweep_dangling_sessions` walks trees rather than collections now:

```rust
/// Close session rows left dangling by a crash or restart, once per user,
/// and return the per-collection counts so the topic browser can report them
/// once.
///
/// Keyed by collection id rather than by URL slug: two users may each own a
/// collection called "Spanish", and a slug-keyed notice would be shown to
/// whichever of them opened the page first. Nor by database path any more —
/// after consolidation, that names a whole tree.
///
/// A user whose database cannot be opened is skipped with a log line rather
/// than failing startup: an unreadable database is the collection page's
/// problem to report, not a reason to refuse to serve everything else. Each
/// user's database is independent, so the sweeps run on their own threads.
fn sweep_dangling_sessions(
    data_dir: &Path,
    failures: &HashMap<PathBuf, String>,
) -> HashMap<CollectionId, usize> {
```

Its body:

1. `let stale_before = Timestamp::now().minus_minutes(SESSION_STALE_MINUTES);` — unchanged.
2. Collect the distinct database paths of `discover_all_collections(data_dir)` into a `BTreeSet<PathBuf>`, so the thread order is stable. Skip any path that is in `failures` (already reported, and the file is not what it should be) or that `is_file()` says does not exist — a tree with no database has nothing to sweep and must not have one created for it.
3. `std::thread::scope`, one thread per path, each running `UserDatabase::open(&path).and_then(|db| db.close_dangling_sessions(stale_before))`, returning `(path, result)`.
4. Fold: on `Err`, `log::error!("Could not close interrupted sessions in {}: {e}", path.display())`; on `Ok(map)`, extend the result and `log::info!("Closed {n} interrupted session(s) in collection {id}")` per non-zero entry.

Add `use std::collections::BTreeSet;` and `use crate::types::collection_id::CollectionId;`.

The sweep test at `src/cmd/serve/server.rs:462` asserts `counts.get(db_path)`; it becomes `counts.get(&id)`, where `id` is already bound in that test.

- [ ] **Step 5: Re-key the notice**

In `src/cmd/serve/state.rs`, `interrupted_closed` becomes `Arc<Mutex<HashMap<CollectionId, usize>>>`, and its doc comment's second paragraph becomes:

```rust
    /// Keyed by collection id — not by slug, and no longer by database path:
    /// two users may each own a collection called "Spanish", and after
    /// consolidation every collection in one tree shares a database file.
```

In `src/cmd/serve/handlers.rs:161`, `.remove(&rc.db_path)` becomes `.remove(&rc.collection_id)`.

- [ ] **Step 6: Run the whole suite**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: PASS, **474 tests** (473 + 1). `test_dangling_session_row_is_closed_and_reported` in `src/cmd/serve/mod.rs` still passes unchanged: it has one collection, so the count is the same either way.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "refactor: sweep dangling sessions once per user

One connection per user instead of one per collection, and the notice is
keyed by collection id: after consolidation a database path names a whole
tree, so path-keyed counts meant whichever collection page was opened
first took the notice and reported everyone else's sessions with it."
```

---

## Task 7: Deleting a collection deletes its rows

`remove_collection_database` deletes `db/{id}.db`, which no longer exists — so after Task 4 deleting a collection folder silently leaves its rows behind for ever, and a folder recreated under a fresh id would not even collide with them. Replace the file deletion with a row deletion.

**Files:**
- Modify: `src/cmd/serve/files.rs:557-591` (`delete_entry` and `remove_collection_database`)

**Interfaces:**
- Consumes: `Database::erase` (Task 4), `user_db_path` (Task 4).
- Produces: `fn remove_collection_rows(state: &AppState, root: &CardRoot, id: &CollectionId) -> Fallible<()>`.

- [ ] **Step 1: Write the failing test**

In `src/cmd/serve/files.rs`'s test module:

```rust
    /// Deleting a collection folder used to delete its database file. One
    /// file per user means deleting its rows instead — and only its rows.
    #[test]
    fn deleting_a_collection_erases_its_rows_and_no_others() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let state = crate::cmd::serve::state::test_support::state_with_data_dir(dir.clone());
        let root = CardRoot::for_user(&dir, None)?;
        for name in ["Biology", "Spanish"] {
            let folder = root.path().join(name);
            std::fs::create_dir_all(&folder)?;
            std::fs::write(folder.join("Deck.md"), "Q: a\nA: b\n")?;
        }
        let bio = crate::cmd::serve::cards::collection_id(&root.path().join("Biology"))?;
        let esp = crate::cmd::serve::cards::collection_id(&root.path().join("Spanish"))?;

        let db_dir = dir.join("db");
        ensure_dir(&db_dir, "review database directory")?;
        let user = UserDatabase::open(&user_db_path(&root, &db_dir)?)?;
        let now = Timestamp::now();
        let hash = crate::types::card_hash::CardHash::hash_bytes(b"a card");
        user.collection(bio.clone()).insert_card(hash, now)?;
        user.collection(esp.clone()).insert_card(hash, now)?;

        // Empty the folder first: a non-empty collection is refused.
        std::fs::remove_file(root.path().join("Biology").join("Deck.md"))?;
        delete_entry(
            &state,
            None,
            &DeleteForm {
                path: "Biology".to_string(),
            },
        )?;

        assert!(user.collection(bio).card_hashes()?.is_empty());
        assert!(user.collection(esp).card_hashes()?.contains(&hash));
        Ok(())
    }
```

Read `DeleteForm`'s declaration in `src/cmd/serve/files.rs` before writing the literal and construct it exactly as declared; if it carries more fields than `path`, fill them the way the neighbouring delete tests in that module do.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test deleting_a_collection_erases_its_rows`
Expected: FAIL — Biology's rows survive, because the function deletes a file that is not there.

- [ ] **Step 3: Replace the file deletion**

```rust
/// Erase the review history of the collection whose id is `id`.
///
/// Called after the folder itself is gone: a folder with no id never had
/// rows to begin with, and one whose removal failed still needs its history.
///
/// Rows rather than a file: one database holds every collection this user
/// has, so deleting the file would take all of them — and deleting nothing,
/// which is what the old path-based removal did after consolidation, left
/// rows nothing could ever address again.
fn remove_collection_rows(
    state: &AppState,
    root: &CardRoot,
    id: &CollectionId,
) -> Fallible<()> {
    let db_dir = match &state.config.data_dir {
        Some(d) => d.join("db"),
        None => return Ok(()),
    };
    let path = user_db_path(root, &db_dir)?;
    if !path.is_file() {
        // Nothing has ever been drilled in this tree.
        return Ok(());
    }
    UserDatabase::open(&path)?.collection(id.clone()).erase()
}
```

and at `src/cmd/serve/files.rs:565`, `remove_collection_database(state, &id)?` becomes `remove_collection_rows(state, &root, &id)?`. Delete the old function.

- [ ] **Step 4: Run the whole suite**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: PASS, **475 tests**.

- [ ] **Step 5: Commit**

```bash
git add src/cmd/serve/files.rs
git commit -m "fix: deleting a collection erases its rows, not a file

One database per user means the file belongs to the whole tree. Deleting
it would take every other collection with it; deleting nothing -- which
is what the path-based removal did after consolidation -- left rows that
nothing could ever address again."
```

---

## Task 8: One connection for the landing page

`refresh_collection_info` opens the user's database once per collection. It is one file now, so open it once.

**Files:**
- Modify: `src/cmd/serve/counts.rs`
- Modify: `src/cmd/serve/landing.rs:36`

**Interfaces:**
- Consumes: `UserDatabase`, `AppState.migration_failures`.
- Produces:
  - `pub fn refresh_collection_info(state: &AppState, collections: &[ResolvedCollection]) -> Vec<CollectionInfo>`
  - `pub fn compute_collection_counts(coll_dir: &Path, db: Database) -> Fallible<(usize, usize)>`

- [ ] **Step 1: Write the failing test**

In `src/cmd/serve/counts.rs`'s test module:

```rust
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
        let found = discover_local_collections(&root, &dir.join("db"), None, IdPolicy::CreateMissing)?;
        assert_eq!(found.len(), 2);

        let infos = refresh_collection_info(&state, &found);
        assert_eq!(infos.len(), 2);
        for info in &infos {
            assert_eq!(info.total_cards, 1, "{} counted wrong", info.name);
            assert_eq!(info.due_today, 1, "{} counted wrong", info.name);
        }
        Ok(())
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test each_collection_is_counted_against_its_own_rows`
Expected: FAIL to compile — `refresh_collection_info` takes one argument.

- [ ] **Step 3: Open each user's database once**

```rust
/// Count every collection, reporting a failure as zero rather than taking
/// the whole listing down: one unreadable collection must not empty the page
/// for the others.
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
```

`compute_collection_counts` takes `db: Database` instead of `db_path: &Path`, and its body's `Collection::with_db_path(...)` becomes `Collection::open(coll_dir.to_path_buf(), db)`.

`src/cmd/serve/landing.rs:36` becomes:

```rust
            refresh_collection_info(&state, &collections_for_user(&state, user.as_ref()))
```

Keep `test_refresh_collection_info_carries_owner` — the new test does not assert on the owner — and add `collection_id` to its literal and `&state` to its call.

- [ ] **Step 4: Run the whole suite**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: PASS, **476 tests**.

- [ ] **Step 5: Commit**

```bash
git add src/cmd/serve/counts.rs src/cmd/serve/landing.rs
git commit -m "perf: count a user's collections on one connection

The landing page opened one connection per collection. They are one file
now, so it opens it once. The counts stay per collection: they compare
parsed cards against rows, which no aggregate query can do."
```

---

## Task 9: The documentation, and the one-way upgrade

The upgrade cannot be undone by downgrading, and a downgrade fails *quietly*: an older binary looks for `db/{id}.db`, finds nothing, and creates empty per-collection databases. Say so where an operator will read it before upgrading.

**Files:**
- Modify: `CHANGELOG.xml`
- Modify: `README.md` (the `data_dir` paragraph near line 95, the collections paragraph near line 264, the deletion paragraph at line 275, and the whole "Database" section from line 541)
- Modify: `CLAUDE.md`

- [ ] **Step 1: Rewrite the README's database section**

Replace the opening of the "Database" section (`README.md:541`) with:

```markdown
## Database

Each **user** has one SQLite database at `{data_dir}/db/{tree}.db`, where
`tree` is the name of their card tree under `{data_dir}/cards/` — `default`
without `[oidc]`, and `{email-slug}-{hash}` with it. Every row carries the
`collection_id` of the collection it belongs to: the stable id in that
collection folder's `.hashcards.toml`, not the folder name, so renaming a
folder keeps its history.

Identical cards in two collections keep two schedules. That is what two
files necessarily meant, and it is now stated as the primary key
`(collection_id, card_hash)`.

Reviews are written as they happen and in the same transaction as the card's
performance, so an interrupted session keeps its progress. Undo marks a
review `voided` rather than deleting it, and read paths filter on
`voided = 0`.

Databases are opened with write-ahead logging where the filesystem supports
it. Where it does not — some NFS and SMB mounts — hashcards logs a line and
carries on in rollback-journal mode.

### Upgrading from per-collection databases

Before this release, each *collection* had its own database at
`{data_dir}/db/{id}.db`. On the first start after upgrading, those are
merged into one database per user, in a single transaction per user, and the
originals are **moved** — not deleted — into `{data_dir}/db/legacy/`.

A user whose merge fails is refused, loudly: their collections show the
error and point at the startup log, rather than being served from an empty
database. Every other user is served, and the server starts.

**The upgrade is one way.** An older binary will not look at
`db/{tree}.db`; it will look for `db/{id}.db`, find nothing, and quietly
create empty per-collection databases, so every session starts from zero
with no error at all. If you need to go back: stop the server, move the
files in `db/legacy/` back into `db/`, and delete the `db/{tree}.db` files
the newer binary wrote. The originals are kept precisely so that this is
possible — and moved rather than left where they were, because an older
binary writing into a file that has already been merged would have those
reviews skipped by the next upgrade and lost without a word.
```

Then add a `collection_id` row to the `cards`, `sessions`, `reviews` and `bookmarks` column tables — "The collection this row belongs to: the stable id in its folder's `.hashcards.toml`." — and change `cards.card_hash`'s type from `text primary key` to `text not null`, since the primary key is now composite and described in the prose above.

- [ ] **Step 2: Correct the three other README paragraphs**

- Near line 95: "and its review database is named from the stable id in the folder's `.hashcards.toml`, so renaming a folder keeps its history" becomes "and its rows in its owner's review database are scoped by the stable id in the folder's `.hashcards.toml`, so renaming a folder keeps its history".
- Near line 264: "review databases are named from that id rather than from the folder name" becomes "review rows are scoped by that id rather than by the folder name".
- Line 275: "Deleting a collection folder deletes its review database with it." becomes "Deleting a collection folder erases its review history with it — its rows, not the file, which belongs to every collection you own."

- [ ] **Step 3: Write the changelog**

In `CHANGELOG.xml`, replace the empty `<unreleased>` block with:

```xml
    <unreleased>
        <changed>
            <change author="claude">
            **One review database per user, instead of one per collection.**
            A collection's schedule lived in `{data_dir}/db/{id}.db`, a grain
            inherited from the CLI this server was forked from, where a
            collection was the directory you ran the tool in. Nothing about a
            multi-user server wanted it: `db/` was flat and shared, so which
            user owned a database could only be discovered by walking every
            tree and reading every `.hashcards.toml`; a folder deleted from
            outside the application orphaned its database for ever, with
            nothing able to collect it or even attribute it; the landing page
            opened one connection per collection on every request, and the
            startup sweep spawned a thread per database. Every row now
            carries the `collection_id` it belongs to, and one file holds
            everything one user has. Identical cards in two collections still
            keep two schedules — that is what two files necessarily meant,
            and it is now the primary key. Databases are opened with
            write-ahead logging where the filesystem allows it, so the stats
            page reading while a drill session writes no longer waits out a
            busy timeout; where it does not, hashcards logs a line and
            carries on in rollback-journal mode, because an optimisation must
            not refuse to start the server.
            </change>
        </changed>
        <breaking>
            <change author="claude">
            **Existing review databases are merged at first start, and the
            upgrade is one way.** Each user's per-collection databases are
            folded into one, in a single transaction per user, and the
            originals are moved — never deleted — into
            `{data_dir}/db/legacy/`. A user whose merge fails is refused with
            an error naming the startup log, and every other user is served:
            silently starting someone from an empty database is the one
            outcome this change exists to prevent. Downgrading afterwards
            fails *quietly* — an older binary looks for `db/{id}.db`, finds
            nothing, and creates empty per-collection databases with no error
            at all. To go back: stop the server, move `db/legacy/` back into
            `db/`, and delete the `db/{tree}.db` files. The originals are kept
            precisely so that this is possible, and are moved rather than
            left in place because an older binary writing into a file that
            has already been merged would have its reviews skipped by the
            next upgrade and lost without a word.
            </change>
        </breaking>
    </unreleased>
```

Validate: `xmllint --schema CHANGELOG.xsd CHANGELOG.xml --noout`. If `xmllint` is not installed, check the nesting by eye against `CHANGELOG.xsd` — `changesType` is a repeatable choice of `added`/`fixed`/`changed`/`removed`/`deprecated`/`security`/`breaking`, each holding `change` elements.

- [ ] **Step 4: Update `CLAUDE.md`**

Under "Design and Internals", after the content-addressing line:

```markdown
- One review database per user, at `{data_dir}/db/{tree}.db`. Every row
  carries its `collection_id`. `Database` is a *view* of one collection on a
  `UserDatabase`'s shared connection, and that mutex is not reentrant: a
  method takes the lock once and calls only free functions under it.
```

And under "Layout", correcting a line that has been stale since the remove-remote-sources work — `grep` finds neither `git` nor `hedgedoc` in `src/cmd/serve/`:

```markdown
- `src/cmd/serve/` is the server: routing, handlers, auth, config, editing,
  decks, export, and the startup merge of pre-consolidation databases.
```

- [ ] **Step 5: Verify**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: PASS, **476 tests** — no code changed in this task.

- [ ] **Step 6: Commit**

```bash
git add CHANGELOG.xml README.md CLAUDE.md
git commit -m "docs: one database per user, and the one-way upgrade

The downgrade path is the part worth writing down: an older binary does
not fail on the new file, it looks for the old one, finds nothing, and
starts every session from zero without saying anything. The recovery
sits in the README next to the upgrade that makes it necessary.

Also drops two modules from CLAUDE.md's description of src/cmd/serve
that have not existed since the remove-remote-sources work."
```

---

## What this plan deliberately does not do

Named here so nobody adds them on the way past, and so the next planner knows they are still open:

- **The MCP server.** Its handoff note is `docs/superpowers/specs/2026-09-08-mcp-server-handoff.md`; it is the reason this work exists, and it is a separate project, not a follow-on step of this plan.
- **`SessionDbs` hash routing.** A saved deck spanning two collections that hold the same card hash routes arbitrarily (`src/cmd/drill/state.rs:88`). This plan neither causes nor worsens it — but having just made "two collections keep two schedules" a primary key, it is a real bug, and it deserves its own failing test and its own fix.
- **Card states, suspend/bury, lapse counting** (ROADMAP §2). Schema 8 adds no columns for them; it moves the ones that exist.
- **Collecting orphan rows.** A collection folder deleted from outside hashcards now leaves rows rather than a file. That is strictly better — they are attributable, and every read path already ignores them — but nothing collects them yet.
