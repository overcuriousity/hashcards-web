# MCP Server Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An MCP endpoint at `/mcp` inside the existing server, letting a model read and write one user's cards, decks and collections through the same code the web UI runs.

**Architecture:** Three movements. First a trash (Tasks 1–5): nothing the server deletes is destroyed any more, for the web UI as much as for the MCP, because the MCP's delete tools are only safe on top of it. Then the door (Tasks 6–9): a token store, a minting page, an `[mcp]` section, and `/mcp` mounted with `rmcp` carrying the protocol and a bearer middleware in front of it — reachable, authenticated, and answering a handshake with no tools on it. Then the tools (Tasks 10–16), in groups, each group a thin adapter over a domain function that already exists.

**Tech Stack:** Rust 2024, rmcp 3.2 (Apache-2.0), rusqlite 0.39 (bundled SQLite), parking_lot, axum 0.8, maud, blake3, getrandom, tokio.

**Spec:** `docs/superpowers/specs/2026-09-09-mcp-server-design.md` — read it before starting. Every task below argues from it. It supersedes the handoff note at `docs/superpowers/specs/2026-09-08-mcp-server-handoff.md`, which was notes handed off mid-design and is no longer the authority on anything.

## Global Constraints

Copied from `CLAUDE.md` and the spec; these apply to every task and are not repeated per task.

- No `unwrap()` in production code. Tests may use it.
- Error handling is `Fallible<T>` and `?`. Create errors with `fail(...)`, which returns `Err`. All error messages are user-facing — and on this feature they are *model*-facing too, which is the same requirement written twice: a tool that fails must say what went wrong clearly enough that the reader can fix it and try again.
- Newtypes for domain concepts. Keep functions small and focused. Module files re-export what is needed and hide the rest.
- Prefer imports to fully qualified names: add `use foo::bar;` rather than writing `foo::bar()`.
- When fixing a bug, write the failing regression test **first**. Every task below is ordered test-first for the same reason.
- Cloze deletion positions are **byte** positions. Use `.bytes()`, never `.chars()`.
- Dates are naive on purpose. No timezones.
- New `CHANGELOG.xml` entries go in the `<unreleased>` block, under `<changed>`, `<added>`, `<fixed>`, `<removed>` or `<breaking>`, each as `<change author="claude">…</change>`. Task 16 writes them; earlier tasks do not touch the file.
- Verification commands, run at the end of every task:
  - `cargo fmt`
  - `cargo clippy --all-targets -- -D warnings`
  - `cargo test`
- **Baseline: 476 tests pass** at commit `2bb3144` on `feat/mcp`. Record the number after each task. It must never go down except in Task 4, where this plan says which tests are rewritten and why, and that drop must be accounted for exactly.
- **Never widen a lock.** `UserDatabase` and the new `AuthDatabase` each hold a `parking_lot::Mutex<Connection>`, which is **not reentrant**: a `&self` method that locks and then calls another `&self` method that locks will deadlock the process with no timeout and no error. Every public method takes the lock exactly once, at the top, and then calls only free functions that accept `&Connection` or `&Transaction`.
- **Every tool handler's work runs through `run_blocking`** (`src/cmd/mod.rs:28`), like every other path that touches SQLite or the card tree. A handler that parses a deck on the async executor is a bug (BUG-44), whether a browser or a model asked for it.
- **A read tool must not write.** Collection discovery has two forms and they are not interchangeable: `collections_for_user` uses `IdPolicy::CreateMissing` and will write a `.hashcards.toml` into a folder that has none; `existing_collections_for_user` uses `IdPolicy::ExistingOnly` and will not. Read tools use the second.
- **Naming, fixed across the plan.** `TokenSecret` is the plaintext shown once. `TokenHash` is the blake3 digest stored. `McpCaller` is the resolved identity a tool handler sees. `tree` means a directory under `{data_dir}/cards/`, and its `tree name` is that directory's file name (`default`, or `{email-slug}-{8 hex}`). `owner` is the lowercased email, or `None` when `[oidc]` is absent.

## Order, and what is shippable when

Tasks 1–5 are the trash. They stand alone: they are an improvement to the web UI on their own terms, they do not mention MCP, and `feat/mcp` is shippable after each. **Task 4 is the behaviour change** — deleting stops erasing — and Task 5 is the page that lets a human undo and empty. Do not ship Task 4 without Task 5: a trash nobody can see or empty fills the disk silently.

Tasks 6–9 build the door. After Task 9 `/mcp` answers a real MCP handshake and lists zero tools. That is a legitimate state to stop in.

Tasks 10–15 add tools in groups. Each group is shippable. Task 16 is documentation.

---

## Task 1: The token store

A bearer token has to resolve to a user *before* the user is known, so it cannot live in a per-user database without opening every one on every request. It gets a server-level file of its own.

**Files:**
- Create: `src/auth_db.rs`
- Create: `src/auth_schema.sql`
- Modify: `src/main.rs` (add `mod auth_db;`)
- Modify: `Cargo.toml` (add `getrandom`)

**Interfaces:**
- Consumes: `crate::error::{Fallible, fail}`, `crate::types::timestamp::Timestamp`.
- Produces, in `crate::auth_db`:
  - `pub struct TokenSecret` — the plaintext. `Display`, and `fn generate() -> Fallible<Self>`, `fn parse(&str) -> Fallible<Self>`, `fn digest(&self) -> TokenHash`.
  - `pub struct TokenHash` — `Clone, PartialEq, Eq, Debug`, `ToSql`, `FromSql`, `Display`.
  - `pub struct TokenRecord { pub hash: TokenHash, pub owner: Option<String>, pub name: String, pub created_at: Timestamp, pub last_used_at: Option<Timestamp> }`
  - `pub struct AuthDatabase`
  - `pub fn AuthDatabase::open(path: &Path) -> Fallible<Self>`
  - `pub fn AuthDatabase::mint(&self, owner: Option<&str>, name: &str, now: Timestamp) -> Fallible<TokenSecret>`
  - `pub fn AuthDatabase::resolve(&self, secret: &TokenSecret, now: Timestamp) -> Fallible<Option<Option<String>>>` — outer `None` means no such live token; inner is the owner.
  - `pub fn AuthDatabase::list(&self, owner: Option<&str>) -> Fallible<Vec<TokenRecord>>`
  - `pub fn AuthDatabase::revoke(&self, owner: Option<&str>, hash: &TokenHash) -> Fallible<bool>`

- [ ] **Step 1: Write the failing tests**

Create `src/auth_db.rs` with the licence header copied from `src/user_db.rs`, then this test module. Nothing it names exists yet.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn db() -> Fallible<(TempDir, AuthDatabase)> {
        let dir = TempDir::new()?;
        let db = AuthDatabase::open(&dir.path().join("auth.db"))?;
        Ok((dir, db))
    }

    #[test]
    fn a_minted_token_resolves_to_its_owner() -> Fallible<()> {
        let (_dir, db) = db()?;
        let secret = db.mint(Some("me@example.com"), "laptop", Timestamp::now())?;
        let owner = db.resolve(&secret, Timestamp::now())?;
        assert_eq!(owner, Some(Some("me@example.com".to_string())));
        Ok(())
    }

    #[test]
    fn a_token_minted_without_oidc_resolves_to_no_owner() -> Fallible<()> {
        let (_dir, db) = db()?;
        let secret = db.mint(None, "laptop", Timestamp::now())?;
        assert_eq!(db.resolve(&secret, Timestamp::now())?, Some(None));
        Ok(())
    }

    #[test]
    fn an_unknown_token_resolves_to_nothing() -> Fallible<()> {
        let (_dir, db) = db()?;
        db.mint(Some("me@example.com"), "laptop", Timestamp::now())?;
        let other = TokenSecret::generate()?;
        assert_eq!(db.resolve(&other, Timestamp::now())?, None);
        Ok(())
    }

    #[test]
    fn a_revoked_token_resolves_to_nothing() -> Fallible<()> {
        let (_dir, db) = db()?;
        let secret = db.mint(Some("me@example.com"), "laptop", Timestamp::now())?;
        assert!(db.revoke(Some("me@example.com"), &secret.digest())?);
        assert_eq!(db.resolve(&secret, Timestamp::now())?, None);
        Ok(())
    }

    /// Revocation is scoped, or one user could revoke another's tokens by
    /// pasting a digest.
    #[test]
    fn a_token_cannot_be_revoked_by_another_user() -> Fallible<()> {
        let (_dir, db) = db()?;
        let secret = db.mint(Some("me@example.com"), "laptop", Timestamp::now())?;
        assert!(!db.revoke(Some("you@example.com"), &secret.digest())?);
        assert_eq!(
            db.resolve(&secret, Timestamp::now())?,
            Some(Some("me@example.com".to_string()))
        );
        Ok(())
    }

    #[test]
    fn listing_shows_only_your_own_tokens() -> Fallible<()> {
        let (_dir, db) = db()?;
        db.mint(Some("me@example.com"), "laptop", Timestamp::now())?;
        db.mint(Some("you@example.com"), "phone", Timestamp::now())?;
        let mine = db.list(Some("me@example.com"))?;
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].name, "laptop");
        Ok(())
    }

    #[test]
    fn resolving_records_when_the_token_was_last_used() -> Fallible<()> {
        let (_dir, db) = db()?;
        let secret = db.mint(Some("me@example.com"), "laptop", Timestamp::now())?;
        assert!(db.list(Some("me@example.com"))?[0].last_used_at.is_none());
        db.resolve(&secret, Timestamp::now())?;
        assert!(db.list(Some("me@example.com"))?[0].last_used_at.is_some());
        Ok(())
    }

    /// The plaintext is never stored: a stolen database yields no token.
    #[test]
    fn the_database_holds_no_plaintext_secret() -> Fallible<()> {
        let dir = TempDir::new()?;
        let path = dir.path().join("auth.db");
        let db = AuthDatabase::open(&path)?;
        let secret = db.mint(Some("me@example.com"), "laptop", Timestamp::now())?;
        drop(db);
        let bytes = std::fs::read(&path)?;
        let needle = secret.to_string();
        assert!(
            !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
            "the plaintext secret was found in the database file"
        );
        Ok(())
    }

    #[test]
    fn a_secret_that_is_not_a_hashcards_token_is_refused() {
        assert!(TokenSecret::parse("").is_err());
        assert!(TokenSecret::parse("not-a-token").is_err());
        assert!(TokenSecret::parse("hcw_nothex").is_err());
    }

    #[test]
    fn two_generated_secrets_differ() -> Fallible<()> {
        let a = TokenSecret::generate()?;
        let b = TokenSecret::generate()?;
        assert_ne!(a.to_string(), b.to_string());
        Ok(())
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test auth_db`
Expected: compilation failure — `AuthDatabase`, `TokenSecret`, `TokenHash` do not exist.

- [ ] **Step 3: Add the dependency**

In `Cargo.toml`, under `[dependencies]`, keeping the list alphabetical:

```toml
getrandom = "0.3"
```

It is already in the tree transitively through `openidconnect`; this makes it direct, for minting.

- [ ] **Step 4: Write the schema**

Create `src/auth_schema.sql`:

```sql
-- Tokens that authenticate MCP clients.
--
-- Server-level rather than per user: a bearer token has to resolve to a
-- user before the user is known, so it cannot live in a file that is
-- chosen by knowing the user.
--
-- Only the digest is stored. The plaintext is shown once, when it is
-- minted, and never again -- so a stolen copy of this file yields nothing
-- that can be presented to the server.
create table if not exists tokens (
    token_hash   text primary key not null,
    owner        text,
    name         text not null,
    created_at   text not null,
    last_used_at text,
    revoked      integer not null default 0
);

create index if not exists tokens_owner on tokens (owner);
```

- [ ] **Step 5: Write the types and the database**

In `src/auth_db.rs`, above the test module:

```rust
use std::fmt::Display;
use std::fmt::Formatter;
use std::path::Path;
use std::time::Duration;

use parking_lot::Mutex;
use rusqlite::Connection;
use rusqlite::ToSql;
use rusqlite::params;
use rusqlite::types::FromSql;
use rusqlite::types::FromSqlError;
use rusqlite::types::FromSqlResult;
use rusqlite::types::ToSqlOutput;
use rusqlite::types::ValueRef;

use crate::error::Fallible;
use crate::error::fail;
use crate::types::timestamp::Timestamp;

/// How long a connection waits for a lock held by another before giving up.
/// The same value `UserDatabase` uses.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Every token starts with this, so one pasted into the wrong field is
/// recognisable as a hashcards token rather than an opaque blob.
const TOKEN_PREFIX: &str = "hcw_";

/// Bytes of randomness behind a token. 32 bytes is 256 bits: far past
/// anything guessable, and the reason no rate limit is needed on the
/// bearer check itself.
const TOKEN_BYTES: usize = 32;

/// The plaintext of a token: `hcw_` followed by 64 hex characters.
///
/// Shown to the user exactly once, at minting. Nothing stores it — the
/// database keeps only its `TokenHash` — so this type exists to be
/// generated, displayed, parsed back out of an `Authorization` header, and
/// digested.
pub struct TokenSecret {
    inner: String,
}

impl TokenSecret {
    /// A fresh token from the operating system's randomness.
    pub fn generate() -> Fallible<Self> {
        let mut bytes = [0u8; TOKEN_BYTES];
        getrandom::fill(&mut bytes)
            .map_err(|e| crate::error::ErrorReport::new(format!(
                "Could not generate a token: the system's random number source failed ({e})."
            )))?;
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        Ok(Self {
            inner: format!("{TOKEN_PREFIX}{hex}"),
        })
    }

    /// A token as presented by a client. Shape is checked here so a
    /// malformed header is refused before it reaches the database.
    pub fn parse(raw: &str) -> Fallible<Self> {
        let trimmed = raw.trim();
        let Some(hex) = trimmed.strip_prefix(TOKEN_PREFIX) else {
            return fail(format!(
                "That is not a hashcards token: a token starts with `{TOKEN_PREFIX}`."
            ));
        };
        if hex.len() != TOKEN_BYTES * 2 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return fail(format!(
                "That is not a hashcards token: the part after `{TOKEN_PREFIX}` must be {} \
                 hexadecimal characters.",
                TOKEN_BYTES * 2
            ));
        }
        Ok(Self {
            inner: trimmed.to_string(),
        })
    }

    /// What the database stores in place of the secret.
    pub fn digest(&self) -> TokenHash {
        TokenHash {
            inner: blake3::hash(self.inner.as_bytes()).to_hex().to_string(),
        }
    }
}

impl Display for TokenSecret {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.inner)
    }
}

/// The blake3 digest of a `TokenSecret`, hex encoded.
///
/// This is what a row is keyed by, what a revocation names, and the only
/// form of a token that is ever written down.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TokenHash {
    inner: String,
}

impl TokenHash {
    pub fn as_str(&self) -> &str {
        &self.inner
    }

    /// A digest as it comes back from a form. Hex and length are checked so
    /// a revoke form cannot smuggle arbitrary text into a query.
    pub fn parse(raw: &str) -> Fallible<Self> {
        let trimmed = raw.trim();
        if trimmed.len() != 64 || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
            return fail("That is not a token id.");
        }
        Ok(Self {
            inner: trimmed.to_string(),
        })
    }
}

impl Display for TokenHash {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.inner)
    }
}

impl ToSql for TokenHash {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.inner.as_str()))
    }
}

impl FromSql for TokenHash {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        if text.is_empty() {
            return Err(FromSqlError::InvalidType);
        }
        Ok(Self {
            inner: text.to_string(),
        })
    }
}

/// One token as the minting page shows it. Never the secret.
pub struct TokenRecord {
    pub hash: TokenHash,
    pub owner: Option<String>,
    pub name: String,
    pub created_at: Timestamp,
    pub last_used_at: Option<Timestamp>,
}

/// The server's token store, at `{data_dir}/auth.db`.
///
/// One connection behind one mutex, like `UserDatabase`, and for the same
/// reason: the mutex is not reentrant, so every method here takes it once
/// at the top and then calls only free functions taking `&Connection`.
pub struct AuthDatabase {
    conn: Mutex<Connection>,
}

impl AuthDatabase {
    pub fn open(path: &Path) -> Fallible<Self> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.execute_batch(include_str!("auth_schema.sql"))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// A new token for `owner`, returned in plaintext once. Only its digest
    /// is written.
    pub fn mint(&self, owner: Option<&str>, name: &str, now: Timestamp) -> Fallible<TokenSecret> {
        let name = name.trim();
        if name.is_empty() {
            return fail("Give the token a name, so you can tell it apart from your others.");
        }
        let secret = TokenSecret::generate()?;
        let conn = self.conn.lock();
        conn.execute(
            "insert into tokens (token_hash, owner, name, created_at) values (?1, ?2, ?3, ?4)",
            params![secret.digest(), owner, name, now.to_string()],
        )?;
        Ok(secret)
    }

    /// The owner a live token names, and `None` when there is no such token.
    ///
    /// The two levels of `Option` are different questions: the outer is
    /// "does this token exist and is it live", the inner is "which user is
    /// it for", and `None` there is the shared `default` tree.
    pub fn resolve(
        &self,
        secret: &TokenSecret,
        now: Timestamp,
    ) -> Fallible<Option<Option<String>>> {
        let hash = secret.digest();
        let conn = self.conn.lock();
        let owner: Option<Option<String>> = conn
            .query_row(
                "select owner from tokens where token_hash = ?1 and revoked = 0",
                params![hash],
                |row| row.get(0),
            )
            .ok();
        if owner.is_some() {
            conn.execute(
                "update tokens set last_used_at = ?1 where token_hash = ?2",
                params![now.to_string(), hash],
            )?;
        }
        Ok(owner)
    }

    /// One user's live tokens, newest first.
    pub fn list(&self, owner: Option<&str>) -> Fallible<Vec<TokenRecord>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "select token_hash, owner, name, created_at, last_used_at \
             from tokens \
             where revoked = 0 and owner is ?1 \
             order by created_at desc",
        )?;
        let rows = stmt.query_map(params![owner], |row| {
            Ok((
                row.get::<_, TokenHash>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (hash, owner, name, created_at, last_used_at) = row?;
            out.push(TokenRecord {
                hash,
                owner,
                name,
                created_at: Timestamp::try_from(created_at)?,
                last_used_at: match last_used_at {
                    Some(t) => Some(Timestamp::try_from(t)?),
                    None => None,
                },
            });
        }
        Ok(out)
    }

    /// Retire a token. Scoped to its owner: a digest alone must not be
    /// enough to revoke somebody else's. `false` when there was no such
    /// live token of theirs to revoke.
    pub fn revoke(&self, owner: Option<&str>, hash: &TokenHash) -> Fallible<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "update tokens set revoked = 1 where token_hash = ?1 and owner is ?2 and revoked = 0",
            params![hash, owner],
        )?;
        Ok(n > 0)
    }
}
```

Note `owner is ?1` rather than `owner = ?1`: SQL equality against `NULL` is never true, and `None` is a real owner here — the no-`[oidc]` instance.

- [ ] **Step 6: Register the module**

In `src/main.rs`, next to the other `mod` declarations, in alphabetical order:

```rust
mod auth_db;
```

- [ ] **Step 7: Check `Timestamp` converts from a string**

`AuthDatabase::list` calls `Timestamp::try_from(String)`. Confirm the impl exists:

Run: `grep -n "impl TryFrom" src/types/timestamp.rs`
Expected: a `TryFrom<String>` (the `test_try_from_string` test in that file exercises it). If it takes `&str` instead, adjust the two call sites in `list` to match rather than adding a second impl.

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test auth_db`
Expected: 10 passing tests.

- [ ] **Step 9: Run the whole suite**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: 486 passed (476 + 10). Record the number.

- [ ] **Step 10: Commit**

```bash
git add src/auth_db.rs src/auth_schema.sql src/main.rs Cargo.toml Cargo.lock
git commit -m "feat: a token store for MCP clients

A bearer token has to resolve to a user before the user is known, so it
cannot live in a per-user database without opening every one of them on
every request. It gets {data_dir}/auth.db instead.

Only the blake3 digest is stored: the plaintext is shown once, at
minting, and a stolen copy of the file yields nothing presentable.
Revocation is scoped to the owner, so a leaked digest is not enough to
retire somebody else's token.

Nothing reads this yet.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_014ccw7LJasy5iUoo9pcZM7U"
```

---

## Task 2: The trash — putting things into it

Deleting stops meaning destroying. This task builds the store and the move into it; nothing calls it yet, so the server behaves exactly as it did.

The design's "good trick" lives here: **a trashed collection's review rows are not touched.** Card hashes are content addresses, so a restored folder finds its own rows again and its whole history comes back with no dump to write or replay. Every read path already ignores rows whose card is gone (`stats_page.rs:49` says why, for the forecast).

**Files:**
- Create: `src/cmd/serve/trash.rs`
- Modify: `src/cmd/serve/mod.rs` (add `pub mod trash;`)
- Modify: `src/cmd/serve/cards.rs` (add `CardRoot::tree_name`)

**Interfaces:**
- Consumes: `CardRoot` (`cards.rs:29`), `CollectionId`, `Timestamp`, `slugify` (`config.rs:209`), `ensure_dir` (`utils.rs`).
- Produces, in `crate::cmd::serve::trash`:
  - `pub enum TrashKind { File, Folder, Collection }`, with `fn as_str(&self) -> &'static str` and `fn parse(&str) -> Fallible<Self>`
  - `pub struct TrashId` — `Clone, PartialEq, Eq, Debug, Display`, `fn parse(&str) -> Fallible<Self>`, `fn as_str(&self) -> &str`
  - `pub struct TrashEntry { pub id: TrashId, pub kind: TrashKind, pub original_path: String, pub deleted_at: Timestamp, pub collection_id: Option<CollectionId> }`
  - `pub fn move_to_trash(data_dir: &Path, root: &CardRoot, rel: &str, kind: TrashKind, collection_id: Option<CollectionId>, now: Timestamp) -> Fallible<TrashId>`
- And, in `crate::cmd::serve::cards`:
  - `pub fn CardRoot::tree_name(&self) -> Fallible<&str>`

- [ ] **Step 1: Write the failing tests**

Create `src/cmd/serve/trash.rs` with the licence header from `src/cmd/serve/files.rs` and this test module.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A data directory with one user tree holding `Spanish/verbs.md`.
    fn fixture() -> Fallible<(TempDir, PathBuf, CardRoot)> {
        let dir = TempDir::new()?;
        let data_dir = dir.path().to_path_buf();
        let root = CardRoot::for_user(&data_dir, None)?;
        std::fs::create_dir_all(root.path().join("Spanish"))?;
        std::fs::write(root.path().join("Spanish/verbs.md"), "Q: hablar\nA: to speak\n")?;
        Ok((dir, data_dir, root))
    }

    #[test]
    fn a_trashed_file_leaves_the_tree_and_arrives_in_the_trash() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        let id = move_to_trash(
            &data_dir,
            &root,
            "Spanish/verbs.md",
            TrashKind::File,
            None,
            Timestamp::now(),
        )?;
        assert!(!root.path().join("Spanish/verbs.md").exists());
        let entry = entry_dir(&data_dir, "default", &id);
        assert!(entry.join("manifest.toml").is_file());
        assert_eq!(
            std::fs::read_to_string(entry.join("content"))?,
            "Q: hablar\nA: to speak\n"
        );
        Ok(())
    }

    #[test]
    fn a_trashed_folder_keeps_its_contents() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        let id = move_to_trash(
            &data_dir,
            &root,
            "Spanish",
            TrashKind::Folder,
            None,
            Timestamp::now(),
        )?;
        let content = entry_dir(&data_dir, "default", &id).join("content");
        assert!(content.join("verbs.md").is_file());
        Ok(())
    }

    #[test]
    fn the_manifest_records_what_was_deleted_and_from_where() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        let id = move_to_trash(
            &data_dir,
            &root,
            "Spanish",
            TrashKind::Collection,
            Some(CollectionId::new("abc12345")?),
            Timestamp::now(),
        )?;
        let entries = list_trash(&data_dir, "default")?;
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.id, id);
        assert_eq!(entry.original_path, "Spanish");
        assert!(matches!(entry.kind, TrashKind::Collection));
        assert_eq!(entry.collection_id.as_ref().map(|c| c.as_str()), Some("abc12345"));
        Ok(())
    }

    /// Two deletions of the same path in the same second must not collide.
    #[test]
    fn deleting_the_same_path_twice_makes_two_entries() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        let now = Timestamp::now();
        let first = move_to_trash(&data_dir, &root, "Spanish/verbs.md", TrashKind::File, None, now)?;
        std::fs::write(root.path().join("Spanish/verbs.md"), "Q: comer\nA: to eat\n")?;
        let second = move_to_trash(&data_dir, &root, "Spanish/verbs.md", TrashKind::File, None, now)?;
        assert_ne!(first, second);
        assert_eq!(list_trash(&data_dir, "default")?.len(), 2);
        Ok(())
    }

    /// One user's trash is not another's, keyed the way `cards/` and `db/`
    /// already are.
    #[test]
    fn each_tree_has_its_own_trash() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        move_to_trash(&data_dir, &root, "Spanish", TrashKind::Folder, None, Timestamp::now())?;
        let other = CardRoot::for_user(&data_dir, Some("you@example.com"))?;
        let other_tree = other.tree_name()?.to_string();
        assert!(list_trash(&data_dir, &other_tree)?.is_empty());
        assert_eq!(list_trash(&data_dir, "default")?.len(), 1);
        Ok(())
    }

    #[test]
    fn an_empty_trash_lists_nothing() -> Fallible<()> {
        let (_dir, data_dir, _root) = fixture()?;
        assert!(list_trash(&data_dir, "default")?.is_empty());
        Ok(())
    }

    #[test]
    fn trashing_something_that_is_not_there_is_refused() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        let err = move_to_trash(
            &data_dir,
            &root,
            "Spanish/nope.md",
            TrashKind::File,
            None,
            Timestamp::now(),
        )
        .unwrap_err();
        assert!(err.message().contains("nope.md"), "{}", err.message());
        Ok(())
    }

    /// A trash id comes back from a form, so it is checked before it is
    /// used to build a path.
    #[test]
    fn a_trash_id_cannot_escape_the_trash_directory() {
        assert!(TrashId::parse("../../etc").is_err());
        assert!(TrashId::parse("a/b").is_err());
        assert!(TrashId::parse("").is_err());
        assert!(TrashId::parse("20260909T120000-Spanish").is_ok());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test trash`
Expected: compilation failure — nothing in the module exists.

- [ ] **Step 3: Give `CardRoot` its tree name**

The trash is keyed by tree name, the same string `user_db_path` derives for the database file. It is asked for in two places now, so it becomes a method rather than being derived twice.

In `src/cmd/serve/cards.rs`, inside `impl CardRoot`, after `path`:

```rust
    /// This tree's directory name: `default`, or `{email-slug}-{8 hex}`.
    ///
    /// The key under which everything belonging to one user is filed —
    /// their review database (`{data_dir}/db/{tree}.db`) and their trash
    /// (`{data_dir}/trash/{tree}/`).
    pub fn tree_name(&self) -> Fallible<&str> {
        self.root
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| ErrorReport::new("the card folder has no readable name"))
    }
```

Then rewrite `user_db_path` (`cards.rs:421`) to use it, so the derivation exists once:

```rust
pub fn user_db_path(root: &CardRoot, db_dir: &Path) -> Fallible<PathBuf> {
    Ok(db_dir.join(format!("{}.db", root.tree_name()?)))
}
```

- [ ] **Step 4: Write the trash**

In `src/cmd/serve/trash.rs`, above the tests:

```rust
use std::fs::read_dir;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use crate::cmd::serve::cards::CardRoot;
use crate::cmd::serve::config::slugify;
use crate::error::ErrorReport;
use crate::error::Fallible;
use crate::error::fail;
use crate::types::collection_id::CollectionId;
use crate::types::timestamp::Timestamp;
use crate::utils::ensure_dir;

/// What the trash holds inside one entry, whatever it was.
///
/// A fixed name rather than the original one: a path that survived
/// `CardRoot::resolve_entry` is still an arbitrary file name, and putting
/// it back on disk under a name we chose means restoring never has to trust
/// it a second time. `manifest.toml` records what it was called.
const CONTENT: &str = "content";

const MANIFEST: &str = "manifest.toml";

/// What was deleted. The distinction matters at restore time: a collection
/// brings its `.hashcards.toml`, and so its id, back with it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrashKind {
    File,
    Folder,
    Collection,
}

impl TrashKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TrashKind::File => "file",
            TrashKind::Folder => "folder",
            TrashKind::Collection => "collection",
        }
    }

    pub fn parse(raw: &str) -> Fallible<Self> {
        match raw {
            "file" => Ok(TrashKind::File),
            "folder" => Ok(TrashKind::Folder),
            "collection" => Ok(TrashKind::Collection),
            other => fail(format!("A trashed item cannot be a `{other}`.")),
        }
    }
}

/// One entry's directory name, `{timestamp}-{slug}`.
///
/// It arrives from a form, so it is parsed rather than trusted: it names a
/// directory, and a value with a separator or a `..` in it would name one
/// outside the trash.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TrashId {
    inner: String,
}

impl TrashId {
    pub fn parse(raw: &str) -> Fallible<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return fail("No trash entry was named.");
        }
        let ok = trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
        if !ok || trimmed.starts_with('.') {
            return fail(format!("`{trimmed}` is not a trash entry."));
        }
        Ok(Self {
            inner: trimmed.to_string(),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.inner
    }
}

impl std::fmt::Display for TrashId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.inner)
    }
}

/// One thing in the trash, as the page and the MCP both see it.
pub struct TrashEntry {
    pub id: TrashId,
    pub kind: TrashKind,
    pub original_path: String,
    pub deleted_at: Timestamp,
    pub collection_id: Option<CollectionId>,
}

/// `manifest.toml`, on disk.
#[derive(Serialize, Deserialize)]
struct Manifest {
    kind: String,
    original_path: String,
    deleted_at: String,
    collection_id: Option<String>,
}

/// One user's trash: `{data_dir}/trash/{tree}/`.
pub fn tree_trash_dir(data_dir: &Path, tree: &str) -> PathBuf {
    data_dir.join("trash").join(tree)
}

/// One entry inside it.
pub fn entry_dir(data_dir: &Path, tree: &str, id: &TrashId) -> PathBuf {
    tree_trash_dir(data_dir, tree).join(id.as_str())
}

/// Move `rel` out of the user's tree and into their trash.
///
/// The bytes move; nothing is copied and nothing is read, so a collection
/// of any size costs one rename where the filesystem allows it.
///
/// **Review rows are deliberately left alone.** A card hash is a content
/// address, so a restored folder addresses its own rows again and its
/// history comes back for free. Until then the rows are orphans, which
/// every read path already ignores. Emptying the trash is what collects
/// them.
pub fn move_to_trash(
    data_dir: &Path,
    root: &CardRoot,
    rel: &str,
    kind: TrashKind,
    collection_id: Option<CollectionId>,
    now: Timestamp,
) -> Fallible<TrashId> {
    let entry = root.resolve_entry(rel)?;
    if !entry.path.exists() {
        return fail(format!("`{}` does not exist.", entry.rel));
    }
    let tree = root.tree_name()?;
    let dir = tree_trash_dir(data_dir, tree);
    ensure_dir(&dir, "trash directory")?;

    let id = allocate_id(&dir, &entry.rel, now)?;
    let target = dir.join(id.as_str());
    ensure_dir(&target, "trash entry")?;

    // The manifest is written *after* the bytes arrive: an entry with no
    // manifest is skipped by `list_trash`, so a crash between the two
    // leaves something inert rather than something that claims to hold
    // what it does not.
    move_path(&entry.path, &target.join(CONTENT))?;

    let manifest = Manifest {
        kind: kind.as_str().to_string(),
        original_path: entry.rel.clone(),
        deleted_at: now.to_string(),
        collection_id: collection_id.map(|c| c.to_string()),
    };
    let toml = toml::to_string_pretty(&manifest)
        .map_err(|e| ErrorReport::new(format!("Could not record the deletion: {e}")))?;
    std::fs::write(target.join(MANIFEST), toml)?;
    Ok(id)
}

/// A directory name nothing else has taken.
///
/// The timestamp has one-second resolution, so two deletions of the same
/// path in the same second would otherwise land on each other and the
/// second would destroy the first — which is the one thing the trash
/// exists to prevent.
fn allocate_id(dir: &Path, rel: &str, now: Timestamp) -> Fallible<TrashId> {
    let stamp = now.into_inner().format("%Y%m%dT%H%M%S");
    let slug = slugify(rel);
    for n in 0..1000 {
        let candidate = if n == 0 {
            format!("{stamp}-{slug}")
        } else {
            format!("{stamp}-{slug}-{n}")
        };
        if !dir.join(&candidate).exists() {
            return TrashId::parse(&candidate);
        }
    }
    fail("Too many things were deleted at once. Try again in a moment.")
}

/// Rename where the filesystem allows it, copy and remove where it does not
/// — `data_dir` and the trash are normally the same device, but a bind
/// mount or a container volume can put them on two.
fn move_path(from: &Path, to: &Path) -> Fallible<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    if from.is_dir() {
        copy_dir(from, to)?;
        std::fs::remove_dir_all(from)?;
    } else {
        std::fs::copy(from, to)?;
        std::fs::remove_file(from)?;
    }
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> Fallible<()> {
    ensure_dir(to, "trash entry")?;
    for entry in read_dir(from)? {
        let entry = entry?;
        let path = entry.path();
        let target = to.join(entry.file_name());
        if path.is_dir() {
            copy_dir(&path, &target)?;
        } else {
            std::fs::copy(&path, &target)?;
        }
    }
    Ok(())
}

/// Everything in one user's trash, newest first.
///
/// An entry with no readable manifest is skipped and logged rather than
/// failing the listing: one damaged directory must not hide the rest of
/// somebody's trash from them.
pub fn list_trash(data_dir: &Path, tree: &str) -> Fallible<Vec<TrashEntry>> {
    let dir = tree_trash_dir(data_dir, tree);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let id = match TrashId::parse(&name) {
            Ok(id) => id,
            Err(_) => continue,
        };
        match read_entry(&entry.path(), id) {
            Ok(e) => out.push(e),
            Err(e) => log::warn!("Skipping unreadable trash entry `{name}`: {e}"),
        }
    }
    out.sort_by(|a, b| b.deleted_at.to_string().cmp(&a.deleted_at.to_string()));
    Ok(out)
}

fn read_entry(dir: &Path, id: TrashId) -> Fallible<TrashEntry> {
    let raw = std::fs::read_to_string(dir.join(MANIFEST))?;
    let manifest: Manifest = toml::from_str(&raw)
        .map_err(|e| ErrorReport::new(format!("its manifest does not parse: {e}")))?;
    Ok(TrashEntry {
        id,
        kind: TrashKind::parse(&manifest.kind)?,
        original_path: manifest.original_path,
        deleted_at: Timestamp::try_from(manifest.deleted_at)?,
        collection_id: match manifest.collection_id {
            Some(c) => Some(CollectionId::new(c)?),
            None => None,
        },
    })
}
```

- [ ] **Step 5: Register the module**

In `src/cmd/serve/mod.rs`, with the other `pub mod` lines, alphabetically:

```rust
pub mod trash;
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test trash`
Expected: 8 passing tests.

If `Timestamp::into_inner()` is not public or does not return something with `.format()`, check `src/types/timestamp.rs:44` — it returns the inner `NaiveDateTime`, which has `format`. Do not add a timezone anywhere near this.

- [ ] **Step 7: Run the whole suite**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: 494 passed (486 + 8). Record the number.

- [ ] **Step 8: Commit**

```bash
git add src/cmd/serve/trash.rs src/cmd/serve/mod.rs src/cmd/serve/cards.rs
git commit -m "feat: a trash, keyed per user tree

Bytes move into {data_dir}/trash/{tree}/{timestamp}-{slug}/ with a
manifest saying what they were and where they came from. The rename is
the whole cost, so trashing a large collection is not a copy.

Review rows are deliberately not touched. A card hash is a content
address, so a restored folder addresses its own rows again and gets its
history back with nothing to replay; until then they are orphans, which
every read path already ignores.

Nothing calls this yet.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_014ccw7LJasy5iUoo9pcZM7U"
```

---

## Task 3: The trash — getting things back out

Restoring and purging. Still nothing calls it.

Purging is the only thing in hashcards that destroys anything, and it is the only place review rows are erased — which is where Task 7 of the per-user-database plan's concern now lives: a collection recreated after a purge must start fresh, and it does, because the purge took its rows.

**Files:**
- Modify: `src/cmd/serve/trash.rs`

**Interfaces:**
- Consumes: everything Task 2 produced.
- Produces, in `crate::cmd::serve::trash`:
  - `pub fn restore_from_trash(data_dir: &Path, root: &CardRoot, id: &TrashId) -> Fallible<String>` — the restored relative path.
  - `pub fn purge_entry(data_dir: &Path, tree: &str, id: &TrashId) -> Fallible<Option<CollectionId>>` — the collection whose rows the caller must now erase, if any.
  - `pub fn purge_all(data_dir: &Path, tree: &str) -> Fallible<Vec<CollectionId>>`

- [ ] **Step 1: Write the failing tests**

Add to the test module in `src/cmd/serve/trash.rs`:

```rust
    #[test]
    fn a_restored_file_comes_back_where_it_was() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        let id = move_to_trash(
            &data_dir,
            &root,
            "Spanish/verbs.md",
            TrashKind::File,
            None,
            Timestamp::now(),
        )?;
        let rel = restore_from_trash(&data_dir, &root, &id)?;
        assert_eq!(rel, "Spanish/verbs.md");
        assert_eq!(
            std::fs::read_to_string(root.path().join("Spanish/verbs.md"))?,
            "Q: hablar\nA: to speak\n"
        );
        assert!(list_trash(&data_dir, "default")?.is_empty());
        Ok(())
    }

    #[test]
    fn a_restored_collection_brings_its_id_back_with_it() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        std::fs::write(
            root.path().join("Spanish/.hashcards.toml"),
            "id = \"abc12345\"\n",
        )?;
        let id = move_to_trash(
            &data_dir,
            &root,
            "Spanish",
            TrashKind::Collection,
            Some(CollectionId::new("abc12345")?),
            Timestamp::now(),
        )?;
        restore_from_trash(&data_dir, &root, &id)?;
        let meta = std::fs::read_to_string(root.path().join("Spanish/.hashcards.toml"))?;
        assert!(meta.contains("abc12345"), "{meta}");
        Ok(())
    }

    /// The restore must not overwrite whatever took the name in the
    /// meantime — that would delete something without trashing it.
    #[test]
    fn restoring_onto_an_occupied_path_is_refused() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        let id = move_to_trash(
            &data_dir,
            &root,
            "Spanish/verbs.md",
            TrashKind::File,
            None,
            Timestamp::now(),
        )?;
        std::fs::write(root.path().join("Spanish/verbs.md"), "something else\n")?;
        let err = restore_from_trash(&data_dir, &root, &id).unwrap_err();
        assert!(err.message().contains("already"), "{}", err.message());
        assert_eq!(
            std::fs::read_to_string(root.path().join("Spanish/verbs.md"))?,
            "something else\n"
        );
        assert_eq!(list_trash(&data_dir, "default")?.len(), 1);
        Ok(())
    }

    /// The parent may have gone too — restoring a deck into a collection
    /// that was itself deleted has to recreate the folder.
    #[test]
    fn restoring_recreates_a_missing_parent() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        let id = move_to_trash(
            &data_dir,
            &root,
            "Spanish/verbs.md",
            TrashKind::File,
            None,
            Timestamp::now(),
        )?;
        std::fs::remove_dir_all(root.path().join("Spanish"))?;
        restore_from_trash(&data_dir, &root, &id)?;
        assert!(root.path().join("Spanish/verbs.md").is_file());
        Ok(())
    }

    #[test]
    fn restoring_something_that_is_not_there_is_refused() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        let id = TrashId::parse("20260909T120000-nothing")?;
        assert!(restore_from_trash(&data_dir, &root, &id).is_err());
        Ok(())
    }

    #[test]
    fn purging_removes_the_bytes_and_names_the_collection() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        let id = move_to_trash(
            &data_dir,
            &root,
            "Spanish",
            TrashKind::Collection,
            Some(CollectionId::new("abc12345")?),
            Timestamp::now(),
        )?;
        let erased = purge_entry(&data_dir, "default", &id)?;
        assert_eq!(erased.as_ref().map(|c| c.as_str()), Some("abc12345"));
        assert!(!entry_dir(&data_dir, "default", &id).exists());
        assert!(list_trash(&data_dir, "default")?.is_empty());
        Ok(())
    }

    /// A trashed deck has no rows of its own to erase: its cards belong to
    /// the collection, which is still there.
    #[test]
    fn purging_a_deck_names_no_collection() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        let id = move_to_trash(
            &data_dir,
            &root,
            "Spanish/verbs.md",
            TrashKind::File,
            None,
            Timestamp::now(),
        )?;
        assert_eq!(purge_entry(&data_dir, "default", &id)?, None);
        Ok(())
    }

    #[test]
    fn emptying_the_trash_names_every_collection_it_held() -> Fallible<()> {
        let (_dir, data_dir, root) = fixture()?;
        std::fs::create_dir_all(root.path().join("German"))?;
        move_to_trash(
            &data_dir,
            &root,
            "Spanish",
            TrashKind::Collection,
            Some(CollectionId::new("abc12345")?),
            Timestamp::now(),
        )?;
        move_to_trash(
            &data_dir,
            &root,
            "German",
            TrashKind::Collection,
            Some(CollectionId::new("def67890")?),
            Timestamp::now(),
        )?;
        let mut erased: Vec<String> = purge_all(&data_dir, "default")?
            .iter()
            .map(|c| c.as_str().to_string())
            .collect();
        erased.sort();
        assert_eq!(erased, vec!["abc12345", "def67890"]);
        assert!(list_trash(&data_dir, "default")?.is_empty());
        Ok(())
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test trash`
Expected: compilation failure — `restore_from_trash`, `purge_entry`, `purge_all` do not exist.

- [ ] **Step 3: Write restore and purge**

Append to the implementation part of `src/cmd/serve/trash.rs`:

```rust
/// Put a trashed entry back where it came from, and return the path it
/// went to.
///
/// The original path is re-resolved through `CardRoot::resolve_entry`
/// rather than joined raw: it has been sitting on disk in a file somebody
/// could have edited, so it is checked exactly as hard as a path arriving
/// from a browser.
pub fn restore_from_trash(data_dir: &Path, root: &CardRoot, id: &TrashId) -> Fallible<String> {
    let tree = root.tree_name()?;
    let dir = entry_dir(data_dir, tree, id);
    if !dir.is_dir() {
        return fail("That item is not in the trash any more.");
    }
    let entry = read_entry(&dir, id.clone())?;
    let target = root.resolve_entry(&entry.original_path)?;
    if target.path.exists() {
        return fail(format!(
            "`{}` already exists, so the deleted copy was left in the trash. Rename or move \
             what is there now, then restore again.",
            target.rel
        ));
    }
    // The collection this belonged to may itself have been deleted since.
    if let Some(parent) = target.path.parent() {
        ensure_dir(parent, "card folder")?;
    }
    move_path(&dir.join(CONTENT), &target.path)?;
    std::fs::remove_dir_all(&dir)?;
    Ok(target.rel)
}

/// Destroy one trashed entry.
///
/// The only thing in hashcards that destroys anything. Returns the
/// collection whose review rows are now unreachable and must be erased by
/// the caller — which cannot happen here, because the trash knows nothing
/// about databases.
pub fn purge_entry(data_dir: &Path, tree: &str, id: &TrashId) -> Fallible<Option<CollectionId>> {
    let dir = entry_dir(data_dir, tree, id);
    if !dir.is_dir() {
        return fail("That item is not in the trash any more.");
    }
    let entry = read_entry(&dir, id.clone())?;
    std::fs::remove_dir_all(&dir)?;
    Ok(entry.collection_id)
}

/// Destroy everything in one user's trash, and name every collection whose
/// rows the caller must now erase.
pub fn purge_all(data_dir: &Path, tree: &str) -> Fallible<Vec<CollectionId>> {
    let mut erased = Vec::new();
    for entry in list_trash(data_dir, tree)? {
        if let Some(id) = purge_entry(data_dir, tree, &entry.id)? {
            erased.push(id);
        }
    }
    Ok(erased)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test trash`
Expected: 16 passing tests in this module.

- [ ] **Step 5: Run the whole suite**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: 502 passed (494 + 8). Record the number.

- [ ] **Step 6: Commit**

```bash
git add src/cmd/serve/trash.rs
git commit -m "feat: restore from the trash, and empty it

Restore re-resolves the original path through CardRoot rather than
joining it raw -- it has been sitting in a file on disk, so it gets the
same checking a path from a browser gets -- and refuses when something
has taken the name, leaving the copy in the trash rather than
overwriting.

Purging is the only thing in hashcards that destroys anything. It names
the collection whose rows are now unreachable; erasing them is the
caller's job, because the trash knows nothing about databases.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_014ccw7LJasy5iUoo9pcZM7U"
```

---

## Task 4: Deleting goes through the trash

The behaviour change. Deleting stops destroying, for the web UI as much as for the MCP that comes later — one deletion semantic for the product, or else deleting a collection means different things depending on which door you came in.

**This is the one task where the test count moves for a reason other than addition, and the accounting is exact:**

- `a_non_empty_folder_is_not_deleted` (`files.rs:1092`) is **deleted**, together with the `non_empty_children` function it tests. The refusal exists because "deleting a whole collection on a misclick would take its review history with it"; with a trash in the way, a misclick no longer does. `files.rs:546` is its only other caller, so the function is dead once the refusal goes, and `clippy -D warnings` will say so.
- `deleting_a_collection_folder_removes_its_review_database` (`files.rs:1400`) is **rewritten** and renamed. Its premise — rows must not be orphaned — is now backwards: orphaned rows are the mechanism that makes a restore recover the history. Purging is what erases them, and the rewritten test asserts both halves.

Net: −2 tests, +4 new ones. Two refusals are deliberately **kept**: `refuse_if_drilling`, because a live session still holds the tree open, and the `media/`-holds-decks check, because those files are invisible in the file manager and the spec did not ask for it to go.

**Files:**
- Modify: `src/cmd/serve/files.rs` (`delete_entry` at `:513`, `remove_collection_rows` at `:585`, `non_empty_children` at `:224`, tests)
- Modify: `src/cmd/serve/state.rs` (nothing — listed only so nobody goes looking; `AppState` is unchanged)

**Interfaces:**
- Consumes: `move_to_trash`, `TrashKind`, `purge_entry`, `purge_all` from Task 2 and Task 3.
- Produces, in `crate::cmd::serve::files`:
  - `pub(crate) fn erase_collection_rows(state: &AppState, root: &CardRoot, id: &CollectionId) -> Fallible<()>` — `remove_collection_rows` renamed and raised, so the trash page can call it after a purge.

- [ ] **Step 1: Write the failing tests**

Add to the test module in `src/cmd/serve/files.rs`. These replace the rewritten test; write them before touching the implementation.

```rust
    /// Deleting a collection no longer erases its rows: they stay as
    /// orphans, which is what lets a restore bring the review history back
    /// with nothing to replay. Purging is what erases them.
    #[test]
    fn deleting_a_collection_trashes_it_and_keeps_its_rows() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let state = state_for(&dir);
        create_entry(
            &state,
            None,
            &NewEntryForm {
                parent: String::new(),
                name: "Spanish".to_string(),
            },
            true,
        )?;
        let root = user_root(&state, None)?;
        let id = collection_id(&root.path().join("Spanish"))?;
        let db_dir = dir.join("db");
        ensure_dir(&db_dir, "review database directory")?;
        let user = UserDatabase::open(&user_db_path(&root, &db_dir)?)?;
        let hash = crate::types::card_hash::CardHash::hash_bytes(b"a card");
        user.collection(id.clone())
            .insert_card(hash, Timestamp::now())?;

        delete_entry(
            &state,
            None,
            &DeleteForm {
                path: "Spanish".to_string(),
            },
        )?;

        assert!(!root.path().join("Spanish").exists());
        assert_eq!(
            user.collection(id).card_hashes()?.len(),
            1,
            "the rows a restore would need are gone"
        );
        let trashed = list_trash(&dir, "default")?;
        assert_eq!(trashed.len(), 1);
        assert_eq!(trashed[0].original_path, "Spanish");
        assert!(matches!(trashed[0].kind, TrashKind::Collection));
        Ok(())
    }

    /// The whole point of leaving the rows behind.
    #[test]
    fn restoring_a_collection_brings_its_review_history_back() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let state = state_for(&dir);
        create_entry(
            &state,
            None,
            &NewEntryForm {
                parent: String::new(),
                name: "Spanish".to_string(),
            },
            true,
        )?;
        let root = user_root(&state, None)?;
        let id = collection_id(&root.path().join("Spanish"))?;
        let db_dir = dir.join("db");
        ensure_dir(&db_dir, "review database directory")?;
        let user = UserDatabase::open(&user_db_path(&root, &db_dir)?)?;
        let hash = crate::types::card_hash::CardHash::hash_bytes(b"a card");
        user.collection(id.clone())
            .insert_card(hash, Timestamp::now())?;

        delete_entry(
            &state,
            None,
            &DeleteForm {
                path: "Spanish".to_string(),
            },
        )?;
        let trashed = list_trash(&dir, "default")?;
        restore_from_trash(&dir, &root, &trashed[0].id)?;

        assert!(root.path().join("Spanish").exists());
        let back = collection_id(&root.path().join("Spanish"))?;
        assert_eq!(back, id, "the restored folder has a different id");
        assert!(
            user.collection(back).card_hashes()?.contains(&hash),
            "the review history did not come back"
        );
        Ok(())
    }

    /// Purging is where the concern the old behaviour served now lives: a
    /// collection recreated after a purge must start fresh.
    #[test]
    fn purging_a_trashed_collection_erases_its_rows() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let state = state_for(&dir);
        create_entry(
            &state,
            None,
            &NewEntryForm {
                parent: String::new(),
                name: "Spanish".to_string(),
            },
            true,
        )?;
        let root = user_root(&state, None)?;
        let id = collection_id(&root.path().join("Spanish"))?;
        let db_dir = dir.join("db");
        ensure_dir(&db_dir, "review database directory")?;
        let user = UserDatabase::open(&user_db_path(&root, &db_dir)?)?;
        let hash = crate::types::card_hash::CardHash::hash_bytes(b"a card");
        user.collection(id.clone())
            .insert_card(hash, Timestamp::now())?;

        delete_entry(
            &state,
            None,
            &DeleteForm {
                path: "Spanish".to_string(),
            },
        )?;
        let trashed = list_trash(&dir, "default")?;
        let erased = purge_entry(&dir, "default", &trashed[0].id)?;
        assert_eq!(erased.as_ref(), Some(&id));
        erase_collection_rows(&state, &root, &id)?;

        assert!(user.collection(id).card_hashes()?.is_empty());
        Ok(())
    }

    /// The refusal that was lifted. A collection full of decks goes to the
    /// trash in one move, because getting it back is now one click.
    #[test]
    fn a_non_empty_collection_can_be_deleted() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let state = state_for(&dir);
        create_entry(
            &state,
            None,
            &NewEntryForm {
                parent: String::new(),
                name: "Spanish".to_string(),
            },
            true,
        )?;
        let root = user_root(&state, None)?;
        std::fs::write(
            root.path().join("Spanish").join("verbs.md"),
            "Q: hablar\nA: to speak\n",
        )?;

        delete_entry(
            &state,
            None,
            &DeleteForm {
                path: "Spanish".to_string(),
            },
        )?;
        assert!(!root.path().join("Spanish").exists());

        let trashed = list_trash(&dir, "default")?;
        restore_from_trash(&dir, &root, &trashed[0].id)?;
        assert!(root.path().join("Spanish/verbs.md").is_file());
        Ok(())
    }
```

Add the imports these need to the test module's `use` block:

```rust
    use crate::cmd::serve::trash::TrashKind;
    use crate::cmd::serve::trash::list_trash;
    use crate::cmd::serve::trash::purge_entry;
    use crate::cmd::serve::trash::restore_from_trash;
```

- [ ] **Step 2: Delete the two tests this replaces**

Remove `a_non_empty_folder_is_not_deleted` (`files.rs:1092`) and `deleting_a_collection_folder_removes_its_review_database` (`files.rs:1400`) in full, including their doc comments.

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --lib files::tests`
Expected: compilation failure — `erase_collection_rows` does not exist — and, once that is stubbed, `deleting_a_collection_trashes_it_and_keeps_its_rows` fails because `delete_entry` still erases.

- [ ] **Step 4: Rewrite `delete_entry`**

Replace the body at `files.rs:513` with this. The `refuse_if_drilling` guard and the `media/` check stay; the non-empty refusal and the row erasure go.

```rust
fn delete_entry(
    state: &AppState,
    user: Option<&CurrentUser>,
    form: &DeleteForm,
) -> Fallible<String> {
    let root = user_root(state, user)?;
    let entry = root.resolve_entry(&form.path)?;
    let rel = entry.rel;
    let target = entry.path;
    if !target.exists() {
        return fail(format!("`{rel}` does not exist."));
    }
    // A live session drills the cards it cached when it started and writes
    // its grades to the collection's database. Moving either underneath it
    // strands those grades, so the same guard `save_file` uses applies here
    // — the trash makes a deletion undoable, not invisible to a session
    // that is mid-drill.
    refuse_if_drilling(state, &root, &rel)?;

    let data_dir = data_dir(state)?;
    let (kind, id) = if target.is_dir() {
        let is_collection_root = !rel.contains('/');
        // `media` does not make a collection count as non-empty, and it is
        // hidden from the tree — both because hashcards put it there. One
        // made by hand before that name was reserved can hold decks, and
        // those would go with the collection having never been listed at
        // all. Say where they are instead of moving them somewhere the
        // person who made them will not think to look.
        if is_collection_root && holds_decks(&target.join(MEDIA_DIR))? {
            return fail(format!(
                "`{rel}/{MEDIA_DIR}` holds card files. That folder is where hashcards keeps a \
                 collection's pasted images, so it is not shown here — move the files out of it \
                 from outside hashcards before deleting this collection."
            ));
        }
        if is_collection_root {
            (TrashKind::Collection, existing_collection_id(&target)?)
        } else {
            (TrashKind::Folder, None)
        }
    } else {
        (TrashKind::File, None)
    };

    // The rows stay behind. A card hash is a content address, so restoring
    // this folder addresses its own rows again and the whole review history
    // comes back with nothing to replay; until then they are orphans, which
    // every read path already ignores. Emptying the trash is what erases
    // them — which is also where "a collection recreated under an old name
    // must start fresh" now lives.
    move_to_trash(&data_dir, &root, &rel, kind, id, Timestamp::now())?;
    Ok(format!("Moved `{rel}` to the trash."))
}
```

- [ ] **Step 5: Rename and raise the row eraser**

`remove_collection_rows` (`files.rs:585`) is no longer called by `delete_entry`; the trash page calls it after a purge. Rename it, raise its visibility, and rewrite the doc comment, which currently argues for a behaviour that no longer exists:

```rust
/// Erase the review history of the collection whose id is `id`.
///
/// Called when a trashed collection is purged — never on deletion, which
/// leaves the rows behind on purpose so a restore can find them. This is
/// the only path that erases them, and purging is the only thing in
/// hashcards that destroys anything.
///
/// Rows rather than a file: one database holds every collection this user
/// has, so deleting the file would take all of them.
pub(crate) fn erase_collection_rows(
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

- [ ] **Step 6: Delete `non_empty_children`**

Remove the function at `files.rs:224` in full. Then fix the two doc comments that name it — `files.rs:173` and the test comment at `files.rs:1903` — so they describe `read_tree` alone rather than a function that is gone.

- [ ] **Step 7: Add the imports `delete_entry` now needs**

At the top of `src/cmd/serve/files.rs`, with the other `use` lines:

```rust
use crate::cmd::serve::trash::TrashKind;
use crate::cmd::serve::trash::move_to_trash;
```

- [ ] **Step 8: Check the flash message that changed**

`delete_entry` now says "Moved `x` to the trash." rather than "Deleted `x`.". Run `grep -rn 'Deleted \`' src/` and fix any test asserting the old wording.

- [ ] **Step 9: Run the whole suite**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: 504 passed (502 − 2 + 4). Record the number. If clippy reports anything as unused, it is something the non-empty refusal was the last caller of — delete it rather than adding an `allow`.

- [ ] **Step 10: Commit**

```bash
git add src/cmd/serve/files.rs
git commit -m "feat: deleting moves to the trash instead of destroying

One deletion semantic for the product: a collection deleted from the
file manager goes where a collection deleted over MCP will go, and both
come back the same way.

The rows now stay behind deliberately. Task 7 of the per-user-database
work erased them so a folder recreated under an old name could not
inherit a stale history; that concern moves to the purge, which is now
the only thing in hashcards that erases anything.

The refusal of a non-empty folder goes with it. It existed because a
misclick was unrecoverable, and it is not any more. refuse_if_drilling
stays -- a live session still holds the tree open -- and so does the
check for card files hidden in media/, because those are invisible in
the file manager and moving them somewhere else invisible helps nobody.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_014ccw7LJasy5iUoo9pcZM7U"
```

---

## Task 5: The trash page

A trash nobody can see or empty fills the disk silently. This is what makes Task 4 shippable.

**Files:**
- Create: `src/cmd/serve/trash_ui.rs`
- Modify: `src/cmd/serve/mod.rs` (add `pub mod trash_ui;`)
- Modify: `src/cmd/serve/server.rs` (three routes)
- Modify: `src/cmd/serve/files_ui.rs` (a link to `/trash` from the file manager)

**Interfaces:**
- Consumes: `list_trash`, `restore_from_trash`, `purge_entry`, `purge_all`, `TrashId` (Task 2, Task 3); `erase_collection_rows` (Task 4); `user_root`, `Flash`, `run_blocking`.
- Produces, in `crate::cmd::serve::trash_ui`:
  - `pub async fn trash_get_handler(...) -> (StatusCode, Html<String>)`
  - `pub async fn trash_restore_handler(...) -> Redirect`
  - `pub async fn trash_purge_handler(...) -> Redirect`
  - `pub struct TrashActionForm { pub id: String }`

- [ ] **Step 1: Write the failing tests**

Create `src/cmd/serve/trash_ui.rs` with the licence header and this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::serve::cards::CardRoot;
    use crate::cmd::serve::state::state_with_data_dir;
    use crate::cmd::serve::trash::TrashKind;
    use crate::cmd::serve::trash::move_to_trash;
    use crate::types::timestamp::Timestamp;
    use tempfile::TempDir;

    fn fixture() -> Fallible<(TempDir, AppState, CardRoot)> {
        let dir = TempDir::new()?;
        let state = state_with_data_dir(dir.path().to_path_buf());
        let root = CardRoot::for_user(dir.path(), None)?;
        std::fs::create_dir_all(root.path().join("Spanish"))?;
        std::fs::write(root.path().join("Spanish/verbs.md"), "Q: a\nA: b\n")?;
        Ok((dir, state, root))
    }

    #[test]
    fn the_page_lists_what_is_in_the_trash() -> Fallible<()> {
        let (dir, state, root) = fixture()?;
        move_to_trash(
            dir.path(),
            &root,
            "Spanish/verbs.md",
            TrashKind::File,
            None,
            Timestamp::now(),
        )?;
        let html = render_trash(&trash_rows(&state, None)?, None).into_string();
        assert!(html.contains("Spanish/verbs.md"), "{html}");
        Ok(())
    }

    #[test]
    fn an_empty_trash_says_so() -> Fallible<()> {
        let (_dir, state, _root) = fixture()?;
        let html = render_trash(&trash_rows(&state, None)?, None).into_string();
        assert!(html.contains("nothing in the trash"), "{html}");
        Ok(())
    }

    #[test]
    fn restoring_puts_the_file_back() -> Fallible<()> {
        let (dir, state, root) = fixture()?;
        let id = move_to_trash(
            dir.path(),
            &root,
            "Spanish/verbs.md",
            TrashKind::File,
            None,
            Timestamp::now(),
        )?;
        let msg = restore_one(&state, None, &id.to_string())?;
        assert!(msg.contains("Spanish/verbs.md"), "{msg}");
        assert!(root.path().join("Spanish/verbs.md").is_file());
        Ok(())
    }

    #[test]
    fn emptying_the_trash_removes_everything() -> Fallible<()> {
        let (dir, state, root) = fixture()?;
        move_to_trash(
            dir.path(),
            &root,
            "Spanish/verbs.md",
            TrashKind::File,
            None,
            Timestamp::now(),
        )?;
        empty_trash(&state, None)?;
        assert!(trash_rows(&state, None)?.is_empty());
        Ok(())
    }

    /// A crafted id must not reach the filesystem.
    #[test]
    fn a_bad_trash_id_is_refused() -> Fallible<()> {
        let (_dir, state, _root) = fixture()?;
        assert!(restore_one(&state, None, "../../etc/passwd").is_err());
        Ok(())
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test trash_ui`
Expected: compilation failure — nothing in the module exists.

- [ ] **Step 3: Write the page**

In `src/cmd/serve/trash_ui.rs`, above the tests. Follow `files_ui.rs` for the surrounding page chrome — read it first and reuse its shell rather than inventing a second one.

```rust
use std::collections::HashMap;

use axum::Form;
use axum::extract::Query;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Html;
use axum::response::Redirect;
use maud::Markup;
use maud::html;

use crate::cmd::run_blocking;
use crate::cmd::serve::auth::CurrentUser;
use crate::cmd::serve::files::erase_collection_rows;
use crate::cmd::serve::files::user_root;
use crate::cmd::serve::state::AppState;
use crate::cmd::serve::trash::TrashEntry;
use crate::cmd::serve::trash::TrashId;
use crate::cmd::serve::trash::list_trash;
use crate::cmd::serve::trash::purge_all;
use crate::cmd::serve::trash::purge_entry;
use crate::cmd::serve::trash::restore_from_trash;
use crate::error::Fallible;
use crate::error::fail;
use crate::flash::Flash;

/// Everything in the caller's trash.
fn trash_rows(state: &AppState, user: Option<&CurrentUser>) -> Fallible<Vec<TrashEntry>> {
    let data_dir = match &state.config.data_dir {
        Some(d) => d.clone(),
        None => return fail("No data directory is configured."),
    };
    let root = user_root(state, user)?;
    list_trash(&data_dir, root.tree_name()?)
}

fn restore_one(state: &AppState, user: Option<&CurrentUser>, raw_id: &str) -> Fallible<String> {
    let data_dir = match &state.config.data_dir {
        Some(d) => d.clone(),
        None => return fail("No data directory is configured."),
    };
    let id = TrashId::parse(raw_id)?;
    let root = user_root(state, user)?;
    let rel = restore_from_trash(&data_dir, &root, &id)?;
    Ok(format!("Restored `{rel}`."))
}

/// Destroy one entry, and the review rows it was the last thing holding on
/// to.
fn purge_one(state: &AppState, user: Option<&CurrentUser>, raw_id: &str) -> Fallible<String> {
    let data_dir = match &state.config.data_dir {
        Some(d) => d.clone(),
        None => return fail("No data directory is configured."),
    };
    let id = TrashId::parse(raw_id)?;
    let root = user_root(state, user)?;
    if let Some(collection) = purge_entry(&data_dir, root.tree_name()?, &id)? {
        erase_collection_rows(state, &root, &collection)?;
    }
    Ok("Deleted for good.".to_string())
}

fn empty_trash(state: &AppState, user: Option<&CurrentUser>) -> Fallible<String> {
    let data_dir = match &state.config.data_dir {
        Some(d) => d.clone(),
        None => return fail("No data directory is configured."),
    };
    let root = user_root(state, user)?;
    let erased = purge_all(&data_dir, root.tree_name()?)?;
    for collection in &erased {
        erase_collection_rows(state, &root, collection)?;
    }
    Ok("The trash is empty.".to_string())
}

fn render_trash(entries: &[TrashEntry], flash: Option<Flash>) -> Markup {
    html! {
        h1 { "Trash" }
        @if let Some(f) = flash { (f.render()) }
        p {
            "Deleting something in hashcards moves it here. Nothing is destroyed until you \
             empty the trash — and emptying it also erases the review history of any \
             collection in it."
        }
        @if entries.is_empty() {
            p { "There is nothing in the trash." }
        } @else {
            form method="post" action="/trash/empty" {
                button type="submit" { "Empty the trash" }
            }
            table {
                thead { tr { th { "What" } th { "Kind" } th { "Deleted" } th {} } }
                tbody {
                    @for entry in entries {
                        tr {
                            td { code { (entry.original_path) } }
                            td { (entry.kind.as_str()) }
                            td { (entry.deleted_at) }
                            td {
                                form method="post" action="/trash/restore" {
                                    input type="hidden" name="id" value=(entry.id);
                                    button type="submit" { "Restore" }
                                }
                                form method="post" action="/trash/purge" {
                                    input type="hidden" name="id" value=(entry.id);
                                    button type="submit" { "Delete for good" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

pub async fn trash_get_handler(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    current_user: Option<CurrentUser>,
) -> (StatusCode, Html<String>) {
    let flash = Flash::from_query(&query);
    // Reading a directory is blocking work (BUG-44).
    let entries = run_blocking(move || trash_rows(&state, current_user.as_ref()))
        .await
        .unwrap_or_else(|e| {
            log::error!("Cannot list the trash: {e}");
            Vec::new()
        });
    let markup = render_trash(&entries, flash);
    (StatusCode::OK, Html(markup.into_string()))
}

pub struct TrashActionForm {
    pub id: String,
}

impl<'de> serde::Deserialize<'de> for TrashActionForm {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct Raw {
            id: String,
        }
        Raw::deserialize(deserializer).map(|r| TrashActionForm { id: r.id })
    }
}

pub async fn trash_restore_handler(
    State(state): State<AppState>,
    current_user: Option<CurrentUser>,
    Form(form): Form<TrashActionForm>,
) -> Redirect {
    flash_for(run_blocking(move || restore_one(&state, current_user.as_ref(), &form.id)).await)
}

pub async fn trash_purge_handler(
    State(state): State<AppState>,
    current_user: Option<CurrentUser>,
    Form(form): Form<TrashActionForm>,
) -> Redirect {
    flash_for(run_blocking(move || purge_one(&state, current_user.as_ref(), &form.id)).await)
}

pub async fn trash_empty_handler(
    State(state): State<AppState>,
    current_user: Option<CurrentUser>,
) -> Redirect {
    flash_for(run_blocking(move || empty_trash(&state, current_user.as_ref())).await)
}

/// Every trash action reports back on `/trash` the same way, as the file
/// manager's mutations do on `/files`.
fn flash_for(outcome: Fallible<String>) -> Redirect {
    match outcome {
        Ok(msg) => Flash::success(msg).redirect("/trash"),
        Err(e) => Flash::error(e.to_string()).redirect("/trash"),
    }
}
```

If `Flash` has no `render` method, look at how `files_ui.rs` renders its banner and match that instead — do not add a second way to draw a flash.

- [ ] **Step 4: Register the module and the routes**

In `src/cmd/serve/mod.rs`:

```rust
pub mod trash_ui;
```

In `src/cmd/serve/server.rs`, in the main `Router::new()` chain after the `/files/*` routes (so it sits inside `require_auth` like every other UI route):

```rust
        .route("/trash", get(trash_get_handler))
        .route("/trash/restore", post(trash_restore_handler))
        .route("/trash/purge", post(trash_purge_handler))
        .route("/trash/empty", post(trash_empty_handler))
```

with the matching imports at the top of the file, alongside the other handler imports.

- [ ] **Step 5: Link to it from the file manager**

In `src/cmd/serve/files_ui.rs`, in the page's header area next to the existing navigation, add:

```rust
            a href="/trash" { "Trash" }
```

Match the surrounding markup — read the header block first and follow whatever element and class the neighbouring links use.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test trash_ui`
Expected: 5 passing tests.

- [ ] **Step 7: Run the whole suite**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: 509 passed (504 + 5). Record the number.

- [ ] **Step 8: Commit**

```bash
git add src/cmd/serve/trash_ui.rs src/cmd/serve/mod.rs src/cmd/serve/server.rs src/cmd/serve/files_ui.rs
git commit -m "feat: a trash page, to undo a delete or make it final

The half of the previous commit that makes it shippable: a trash nobody
can see or empty fills the disk silently.

Emptying is the only thing in hashcards that destroys anything, and the
page says so before you do it -- including that it takes the review
history of any collection in the trash with it.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_014ccw7LJasy5iUoo9pcZM7U"
```

---

## Task 6: Minting tokens

The page a user gets a token from. Behind `require_auth`, like every other UI route.

**Files:**
- Create: `src/cmd/serve/tokens.rs`
- Modify: `src/cmd/serve/mod.rs` (add `pub mod tokens;`)
- Modify: `src/cmd/serve/state.rs` (`AppState` gains `auth: Option<Arc<AuthDatabase>>`)
- Modify: `src/cmd/serve/server.rs` (open `auth.db` at startup; three routes)

**Interfaces:**
- Consumes: `AuthDatabase`, `TokenSecret`, `TokenHash`, `TokenRecord` (Task 1).
- Produces, in `crate::cmd::serve::tokens`:
  - `pub async fn tokens_get_handler(...) -> (StatusCode, Html<String>)`
  - `pub async fn tokens_mint_handler(...) -> (StatusCode, Html<String>)` — renders the page with the secret on it, rather than redirecting, because a redirect would put the secret in a URL.
  - `pub async fn tokens_revoke_handler(...) -> Redirect`
- And, in `crate::cmd::serve::state`:
  - `AppState.auth: Option<Arc<AuthDatabase>>` — `None` only when there is no `data_dir`.

- [ ] **Step 1: Write the failing tests**

Create `src/cmd/serve/tokens.rs` with the licence header and this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture() -> Fallible<(TempDir, AppState)> {
        let dir = TempDir::new()?;
        let mut state = state_with_data_dir(dir.path().to_path_buf());
        state.auth = Some(Arc::new(AuthDatabase::open(&dir.path().join("auth.db"))?));
        Ok((dir, state))
    }

    #[test]
    fn minting_returns_a_usable_secret() -> Fallible<()> {
        let (_dir, state) = fixture()?;
        let secret = mint_token(&state, None, "laptop")?;
        let auth = state.auth.clone().expect("auth db");
        assert_eq!(
            auth.resolve(&TokenSecret::parse(&secret.to_string())?, Timestamp::now())?,
            Some(None)
        );
        Ok(())
    }

    #[test]
    fn a_token_needs_a_name() -> Fallible<()> {
        let (_dir, state) = fixture()?;
        assert!(mint_token(&state, None, "   ").is_err());
        Ok(())
    }

    /// The secret appears on the page that mints it, and that page says it
    /// will not appear again.
    #[test]
    fn the_secret_is_shown_once_with_a_warning() -> Fallible<()> {
        let (_dir, state) = fixture()?;
        let secret = mint_token(&state, None, "laptop")?;
        let html = render_tokens(&list_tokens(&state, None)?, Some(&secret), None).into_string();
        assert!(html.contains(&secret.to_string()), "the secret is not on the page");
        assert!(html.contains("will not be shown again"), "{html}");
        Ok(())
    }

    /// Every later render of the page must not carry it.
    #[test]
    fn the_secret_is_not_on_the_listing_page() -> Fallible<()> {
        let (_dir, state) = fixture()?;
        let secret = mint_token(&state, None, "laptop")?;
        let html = render_tokens(&list_tokens(&state, None)?, None, None).into_string();
        assert!(!html.contains(&secret.to_string()), "the secret is still on the page");
        assert!(html.contains("laptop"), "{html}");
        Ok(())
    }

    #[test]
    fn revoking_makes_the_token_stop_working() -> Fallible<()> {
        let (_dir, state) = fixture()?;
        let secret = mint_token(&state, None, "laptop")?;
        let hash = secret.digest();
        revoke_token(&state, None, &hash.to_string())?;
        let auth = state.auth.clone().expect("auth db");
        assert_eq!(auth.resolve(&secret, Timestamp::now())?, None);
        assert!(list_tokens(&state, None)?.is_empty());
        Ok(())
    }

    #[test]
    fn a_malformed_token_id_is_refused() -> Fallible<()> {
        let (_dir, state) = fixture()?;
        assert!(revoke_token(&state, None, "not-a-digest").is_err());
        Ok(())
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test tokens`
Expected: compilation failure.

- [ ] **Step 3: Give `AppState` the token store**

In `src/cmd/serve/state.rs`, add to `AppState` (`state.rs:59`):

```rust
    /// The server's MCP token store, at `{data_dir}/auth.db`.
    ///
    /// `None` only when no data directory is configured, which is the same
    /// condition under which there is nothing to serve.
    pub auth: Option<Arc<AuthDatabase>>,
```

with `use crate::auth_db::AuthDatabase;` at the top. Then add `auth: None` to `state_with_data_dir` (`state.rs:308`) so the test helper still compiles — the tests above set it themselves.

- [ ] **Step 4: Open it at startup**

In `src/cmd/serve/server.rs`, in `start_serve`, next to the existing `ensure_dir` calls:

```rust
    // The MCP token store. Opened here rather than per request: it creates
    // its schema on open, and a data directory that cannot hold it should
    // fail at startup with a clear error rather than at a client's first
    // call.
    let auth = match &config.data_dir {
        Some(data_dir) => Some(Arc::new(AuthDatabase::open(&data_dir.join("auth.db"))?)),
        None => None,
    };
```

and add `auth,` to the `AppState { .. }` literal.

- [ ] **Step 5: Write the page**

In `src/cmd/serve/tokens.rs`, above the tests:

```rust
use std::collections::HashMap;
use std::sync::Arc;

use axum::Form;
use axum::extract::Query;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Html;
use axum::response::Redirect;
use maud::Markup;
use maud::html;
use serde::Deserialize;

use crate::auth_db::AuthDatabase;
use crate::auth_db::TokenHash;
use crate::auth_db::TokenRecord;
use crate::auth_db::TokenSecret;
use crate::cmd::run_blocking;
use crate::cmd::serve::auth::CurrentUser;
use crate::cmd::serve::state::AppState;
use crate::error::Fallible;
use crate::error::fail;
use crate::flash::Flash;
use crate::types::timestamp::Timestamp;

/// The caller's owner key, lowercased as it is stored.
fn owner_key(user: Option<&CurrentUser>) -> Option<String> {
    user.map(|u| u.email.to_lowercase())
}

fn auth_db(state: &AppState) -> Fallible<Arc<AuthDatabase>> {
    match &state.auth {
        Some(a) => Ok(Arc::clone(a)),
        None => fail("No data directory is configured, so tokens cannot be stored."),
    }
}

fn list_tokens(state: &AppState, user: Option<&CurrentUser>) -> Fallible<Vec<TokenRecord>> {
    auth_db(state)?.list(owner_key(user).as_deref())
}

fn mint_token(
    state: &AppState,
    user: Option<&CurrentUser>,
    name: &str,
) -> Fallible<TokenSecret> {
    auth_db(state)?.mint(owner_key(user).as_deref(), name, Timestamp::now())
}

fn revoke_token(state: &AppState, user: Option<&CurrentUser>, raw: &str) -> Fallible<String> {
    let hash = TokenHash::parse(raw)?;
    if auth_db(state)?.revoke(owner_key(user).as_deref(), &hash)? {
        Ok("Token revoked.".to_string())
    } else {
        fail("That token is not one of yours, or it was already revoked.")
    }
}

/// `secret` is `Some` only on the response to the request that minted it.
fn render_tokens(
    tokens: &[TokenRecord],
    secret: Option<&TokenSecret>,
    flash: Option<Flash>,
) -> Markup {
    html! {
        h1 { "MCP tokens" }
        @if let Some(f) = flash { (f.render()) }
        p {
            "A token lets an MCP client read and write your cards, decks and collections. \
             Give it to software you trust, over a connection you trust: anyone holding it \
             can do anything to your cards that you can."
        }
        @if let Some(secret) = secret {
            div class="flash success" {
                p { strong { "Here is your new token. Copy it now — it will not be shown again." } }
                pre { code { (secret) } }
            }
        }
        form method="post" action="/tokens/new" {
            label for="name" { "What is this token for?" }
            input type="text" id="name" name="name" placeholder="laptop" required;
            button type="submit" { "Mint a token" }
        }
        @if tokens.is_empty() {
            p { "You have no tokens." }
        } @else {
            table {
                thead { tr { th { "Name" } th { "Created" } th { "Last used" } th {} } }
                tbody {
                    @for token in tokens {
                        tr {
                            td { (token.name) }
                            td { (token.created_at) }
                            td {
                                @match &token.last_used_at {
                                    Some(t) => (t.to_string()),
                                    None => "never",
                                }
                            }
                            td {
                                form method="post" action="/tokens/revoke" {
                                    input type="hidden" name="id" value=(token.hash);
                                    button type="submit" { "Revoke" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

pub async fn tokens_get_handler(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    current_user: Option<CurrentUser>,
) -> (StatusCode, Html<String>) {
    let flash = Flash::from_query(&query);
    let tokens = run_blocking(move || list_tokens(&state, current_user.as_ref()))
        .await
        .unwrap_or_else(|e| {
            log::error!("Cannot list tokens: {e}");
            Vec::new()
        });
    (
        StatusCode::OK,
        Html(render_tokens(&tokens, None, flash).into_string()),
    )
}

#[derive(Deserialize)]
pub struct MintForm {
    pub name: String,
}

/// Answers with the page rather than redirecting: a redirect would have to
/// carry the secret in a URL, where it would land in the browser's history
/// and in every access log between here and there.
pub async fn tokens_mint_handler(
    State(state): State<AppState>,
    current_user: Option<CurrentUser>,
    Form(form): Form<MintForm>,
) -> (StatusCode, Html<String>) {
    let outcome = run_blocking(move || {
        let secret = mint_token(&state, current_user.as_ref(), &form.name)?;
        let tokens = list_tokens(&state, current_user.as_ref())?;
        Ok((secret, tokens))
    })
    .await;
    match outcome {
        Ok((secret, tokens)) => (
            StatusCode::OK,
            Html(render_tokens(&tokens, Some(&secret), None).into_string()),
        ),
        Err(e) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Html(render_tokens(&[], None, Some(Flash::error(e.to_string()))).into_string()),
        ),
    }
}

#[derive(Deserialize)]
pub struct RevokeForm {
    pub id: String,
}

pub async fn tokens_revoke_handler(
    State(state): State<AppState>,
    current_user: Option<CurrentUser>,
    Form(form): Form<RevokeForm>,
) -> Redirect {
    match run_blocking(move || revoke_token(&state, current_user.as_ref(), &form.id)).await {
        Ok(msg) => Flash::success(msg).redirect("/tokens"),
        Err(e) => Flash::error(e.to_string()).redirect("/tokens"),
    }
}
```

- [ ] **Step 6: Register the module and the routes**

`src/cmd/serve/mod.rs`: `pub mod tokens;`

`src/cmd/serve/server.rs`, in the main router chain (inside `require_auth`):

```rust
        .route("/tokens", get(tokens_get_handler))
        .route("/tokens/new", post(tokens_mint_handler))
        .route("/tokens/revoke", post(tokens_revoke_handler))
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test tokens`
Expected: 6 passing tests.

- [ ] **Step 8: Run the whole suite**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: 515 passed (509 + 6). Record the number.

- [ ] **Step 9: Commit**

```bash
git add src/cmd/serve/tokens.rs src/cmd/serve/mod.rs src/cmd/serve/state.rs src/cmd/serve/server.rs
git commit -m "feat: mint and revoke MCP tokens from the web UI

Behind require_auth, like every other page: a token is minted by the
user it belongs to, not written into the config by an administrator.

Minting answers with the page rather than redirecting. A redirect would
have to carry the secret in a URL, where it would sit in the browser
history and in every access log between here and there.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_014ccw7LJasy5iUoo9pcZM7U"
```

---

## Task 7: The `[mcp]` section

Two settings. One of them is not in the spec, and the reason is a real thing found while checking the SDK: **`rmcp` validates the `Host` header and defaults to loopback only**, as protection against DNS rebinding attacks on locally-running MCP servers. A hashcards instance served at `cards.example.com` would have every `/mcp` request rejected with no obvious cause. So the deployment's hostnames have to be configurable, and the failure has to be documented where somebody will find it.

**Files:**
- Modify: `src/cmd/serve/config.rs` (`McpSection`, `ResolvedMcp`, resolution, tests)
- Modify: `hashcards.example.toml`

**Interfaces:**
- Consumes: nothing.
- Produces, in `crate::cmd::serve::config`:
  - `pub struct McpSection { pub enabled: Option<bool>, pub allowed_hosts: Option<Vec<String>> }` — `Deserialize`
  - `pub struct ResolvedMcp { pub enabled: bool, pub allowed_hosts: Vec<String> }`
  - `ResolvedServeConfig.mcp: ResolvedMcp`

- [ ] **Step 1: Write the failing tests**

Add to the test module in `src/cmd/serve/config.rs`, following the shape of the existing `from_toml` tests there:

```rust
    #[test]
    fn mcp_is_enabled_when_no_section_is_given() -> Fallible<()> {
        let toml = "[server]\ndata_dir = \"/var/lib/hashcards\"\n";
        let config = ResolvedServeConfig::from_toml(toml, None)?;
        assert!(config.mcp.enabled);
        Ok(())
    }

    #[test]
    fn mcp_can_be_turned_off() -> Fallible<()> {
        let toml = "[server]\ndata_dir = \"/var/lib/hashcards\"\n\n[mcp]\nenabled = false\n";
        let config = ResolvedServeConfig::from_toml(toml, None)?;
        assert!(!config.mcp.enabled);
        Ok(())
    }

    /// Without this, an instance on a real hostname answers every MCP
    /// request with a rejection and says nothing about why.
    #[test]
    fn mcp_allows_the_configured_hosts_as_well_as_loopback() -> Fallible<()> {
        let toml = "[server]\ndata_dir = \"/var/lib/hashcards\"\n\n\
                    [mcp]\nallowed_hosts = [\"cards.example.com\"]\n";
        let config = ResolvedServeConfig::from_toml(toml, None)?;
        assert!(config.mcp.allowed_hosts.contains(&"cards.example.com".to_string()));
        assert!(
            config.mcp.allowed_hosts.iter().any(|h| h == "localhost"),
            "loopback must stay allowed, or a local client stops working"
        );
        Ok(())
    }
```

Check the exact name and signature of the constructor these call — `grep -n "fn from_toml" src/cmd/serve/config.rs` — and match it rather than the sketch above.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test config::tests::mcp`
Expected: compilation failure — `config.mcp` does not exist.

- [ ] **Step 3: Add the section**

In `src/cmd/serve/config.rs`, next to `OidcSection`:

```rust
/// `[mcp]`, the MCP endpoint's settings. Absent means enabled with
/// loopback-only hosts.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpSection {
    pub enabled: Option<bool>,
    pub allowed_hosts: Option<Vec<String>>,
}

/// `[mcp]` after defaults are applied.
pub struct ResolvedMcp {
    pub enabled: bool,
    /// Hostnames `/mcp` will answer to.
    ///
    /// `rmcp` validates the `Host` header against this list to stop a web
    /// page in a browser from driving a locally-running MCP server through
    /// DNS rebinding. Its default is loopback only, which is right for a
    /// desktop MCP server and wrong for hashcards, which is normally served
    /// on a real hostname — so the deployment's names go here, and loopback
    /// is always kept alongside them so a client on the same machine keeps
    /// working.
    pub allowed_hosts: Vec<String>,
}
```

Add `pub mcp: Option<McpSection>` to `ServeConfig` and `pub mcp: ResolvedMcp` to `ResolvedServeConfig`, then resolve it in `from_toml`:

```rust
        let mcp = {
            let section = config.mcp.as_ref();
            let mut allowed_hosts = vec![
                "localhost".to_string(),
                "127.0.0.1".to_string(),
                "[::1]".to_string(),
            ];
            if let Some(extra) = section.and_then(|m| m.allowed_hosts.as_ref()) {
                for host in extra {
                    let host = host.trim();
                    if host.is_empty() {
                        return fail(
                            "configuration error: [mcp].allowed_hosts must not contain an empty \
                             entry.",
                        );
                    }
                    if !allowed_hosts.iter().any(|h| h == host) {
                        allowed_hosts.push(host.to_string());
                    }
                }
            }
            ResolvedMcp {
                enabled: section.and_then(|m| m.enabled).unwrap_or(true),
                allowed_hosts,
            }
        };
```

and add `mcp,` to the `ResolvedServeConfig { .. }` literal.

- [ ] **Step 4: Document it in the example config**

In `hashcards.example.toml`, at the end, matching the commenting style of the `[oidc]` block already there:

```toml
# The MCP endpoint, at /mcp. A model can read and write your cards through
# it, authenticated by a token you mint at /tokens.
#
# [mcp]
# enabled = true
#
# The hostnames /mcp will answer to. hashcards always allows loopback, so a
# client on the same machine works with no configuration; add the names this
# instance is actually served under, or every MCP request arrives from a
# host the endpoint does not recognise and is refused. The check exists to
# stop a web page in a browser from driving a local MCP server by rebinding
# DNS at it.
#
# allowed_hosts = ["cards.example.com"]
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test config`
Expected: the three new tests pass, and every existing `config` test still does. `ResolvedServeConfig::from_directories` (`#[cfg(test)]`) also needs `mcp` filled in; give it `ResolvedMcp { enabled: true, allowed_hosts: vec!["localhost".to_string()] }`.

- [ ] **Step 6: Run the whole suite**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: 518 passed (515 + 3). Record the number.

- [ ] **Step 7: Commit**

```bash
git add src/cmd/serve/config.rs hashcards.example.toml
git commit -m "feat: an [mcp] config section

enabled defaults to true: a token is required regardless and the minting
page is behind auth, so on is not an open door, while off means a
freshly minted token silently does nothing.

allowed_hosts is the setting the design did not anticipate. rmcp
validates the Host header and defaults to loopback only, which is right
for a desktop MCP server and wrong for a hashcards instance on a real
hostname -- it would refuse every request and say nothing useful about
why. Loopback is always kept, so a client on the same machine needs no
configuration.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_014ccw7LJasy5iUoo9pcZM7U"
```

Note: `serve_data_dir` in `src/cmd/serve/mod.rs`'s test module builds a `ResolvedServeConfig` literal and will need `mcp: ResolvedMcp { enabled: true, allowed_hosts: vec!["localhost".to_string(), "127.0.0.1".to_string()] }` added to it in this task, or the suite will not compile.

---

## Task 8: `/mcp`, authenticated, with no tools on it

The door. After this task an MCP client connects, completes a handshake, and is told there are zero tools — which is a legitimate state to stop in.

**Files:**
- Create: `src/cmd/serve/mcp/mod.rs`
- Create: `src/cmd/serve/mcp/auth.rs`
- Create: `src/cmd/serve/mcp/server.rs`
- Modify: `src/cmd/serve/mod.rs` (add `pub mod mcp;`)
- Modify: `src/cmd/serve/server.rs` (merge the routes)
- Modify: `Cargo.toml`

**Interfaces:**
- Consumes: `AuthDatabase` and `TokenSecret` (Task 1); `ResolvedMcp` (Task 7).
- Produces, in `crate::cmd::serve::mcp`:
  - `pub fn mcp_routes(state: &AppState) -> Router<AppState>`
- In `crate::cmd::serve::mcp::auth`:
  - `pub struct McpCaller { owner: Option<String> }` — `Clone`, with `pub fn owner(&self) -> Option<&str>` and `pub fn current_user(&self) -> Option<CurrentUser>`
  - `pub async fn require_bearer(...) -> Result<Response, Response>` — axum middleware
- In `crate::cmd::serve::mcp::server`:
  - `pub struct HashcardsMcp { state: AppState, tool_router: ToolRouter<Self> }`
  - `pub fn HashcardsMcp::new(state: AppState) -> Self`
  - `pub fn HashcardsMcp::caller(&self, ctx: &RequestContext<RoleServer>) -> Result<McpCaller, ErrorData>` — every tool handler starts with this.
  - `pub const INSTRUCTIONS: &str`

- [ ] **Step 1: Add the dependency**

In `Cargo.toml`, under `[dependencies]`:

```toml
rmcp = { version = "3.2", features = ["server", "transport-streamable-http-server"] }
```

and under `[dev-dependencies]`, so the tests can drive the endpoint with a real client:

```toml
rmcp = { version = "3.2", features = ["server", "transport-streamable-http-server", "client", "transport-streamable-http-client-reqwest"] }
```

Then confirm the licence gate is still satisfied:

Run: `cargo deny check licenses`
Expected: no new failures. If a transitive crate arrives under a licence `deny.toml` does not allow, stop and report it rather than widening the allow list — that list carries a written justification per entry, and adding one silently is how it stops meaning anything.

- [ ] **Step 2: Write the failing tests**

Create `src/cmd/serve/mcp/mod.rs` with the licence header and this test module. `serve_data_dir_with` is a variant of the existing `serve_data_dir` helper taking a `ResolvedMcp`; add it to `src/cmd/serve/mod.rs`'s test module next to the original and make the original call it.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth_db::AuthDatabase;
    use crate::types::timestamp::Timestamp;

    /// A data directory with a token minted in it, and the token.
    fn seeded(dir: &std::path::Path) -> Fallible<String> {
        let auth = AuthDatabase::open(&dir.join("auth.db"))?;
        Ok(auth.mint(None, "test", Timestamp::now())?.to_string())
    }

    #[tokio::test]
    async fn a_request_with_no_token_is_refused_without_a_redirect() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let token = seeded(dir.path())?;
        let _ = token;
        let port = 21_001;
        serve_data_dir(dir.path(), port).await?;
        let res = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/mcp"))
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .map_err(|e| ErrorReport::new(e.to_string()))?;
        assert_eq!(res.status(), 401);
        assert!(
            res.headers().contains_key("www-authenticate"),
            "an MCP client needs to be told how to authenticate, not redirected to a login page"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_request_with_an_unknown_token_is_refused() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        seeded(dir.path())?;
        let port = 21_002;
        serve_data_dir(dir.path(), port).await?;
        let res = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/mcp"))
            .header("authorization", "Bearer hcw_00000000000000000000000000000000000000000000000000000000000000ff")
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .map_err(|e| ErrorReport::new(e.to_string()))?;
        assert_eq!(res.status(), 401);
        Ok(())
    }

    #[tokio::test]
    async fn a_revoked_token_stops_working() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let auth = AuthDatabase::open(&dir.path().join("auth.db"))?;
        let secret = auth.mint(None, "test", Timestamp::now())?;
        auth.revoke(None, &secret.digest())?;
        drop(auth);
        let port = 21_003;
        serve_data_dir(dir.path(), port).await?;
        let res = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/mcp"))
            .header("authorization", format!("Bearer {secret}"))
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .map_err(|e| ErrorReport::new(e.to_string()))?;
        assert_eq!(res.status(), 401);
        Ok(())
    }

    /// The handshake, driven by a real MCP client rather than a
    /// hand-written frame, so protocol negotiation is exercised.
    #[tokio::test]
    async fn a_good_token_completes_the_handshake() -> Fallible<()> {
        use rmcp::ServiceExt;
        use rmcp::transport::StreamableHttpClientTransport;

        let dir = tempfile::tempdir()?;
        let token = seeded(dir.path())?;
        let port = 21_004;
        serve_data_dir(dir.path(), port).await?;

        let transport = StreamableHttpClientTransport::from_uri_with_auth(
            format!("http://127.0.0.1:{port}/mcp"),
            token,
        );
        let client = ()
            .serve(transport)
            .await
            .map_err(|e| ErrorReport::new(format!("handshake failed: {e}")))?;
        let info = client.peer_info();
        assert!(info.is_some(), "the server sent no server info");
        let tools = client
            .list_tools(Default::default())
            .await
            .map_err(|e| ErrorReport::new(format!("tools/list failed: {e}")))?;
        assert!(tools.tools.is_empty(), "no tools exist yet");
        client.cancel().await.ok();
        Ok(())
    }

    /// The instructions are how a model learns the domain, so their
    /// presence is a test rather than a hope.
    #[test]
    fn the_instructions_teach_the_taxonomy_and_the_card_syntax() {
        use crate::cmd::serve::mcp::server::INSTRUCTIONS;
        for needle in ["collection", "deck", "content address", "Q:", "A:", "C:", "---"] {
            assert!(
                INSTRUCTIONS.contains(needle),
                "the instructions never mention `{needle}`"
            );
        }
    }

    #[tokio::test]
    async fn the_endpoint_is_absent_when_mcp_is_disabled() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        seeded(dir.path())?;
        let port = 21_005;
        serve_data_dir_with(
            dir.path(),
            port,
            ResolvedMcp {
                enabled: false,
                allowed_hosts: vec!["127.0.0.1".to_string()],
            },
        )
        .await?;
        let res = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/mcp"))
            .body("{}")
            .send()
            .await
            .map_err(|e| ErrorReport::new(e.to_string()))?;
        assert_eq!(res.status(), 404);
        Ok(())
    }
}
```

The exact client constructor may differ; `grep -rn "from_uri" ~/.cargo/registry/src/*/rmcp-3.2.0/src/transport/streamable_http_client.rs` and use what is actually there. The assertion is what matters: a real client, a real handshake, an empty tool list.

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test mcp`
Expected: compilation failure — the module has no implementation.

- [ ] **Step 4: Write the bearer middleware**

Create `src/cmd/serve/mcp/auth.rs`:

```rust
use axum::extract::Request;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header::WWW_AUTHENTICATE;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;

use crate::auth_db::TokenSecret;
use crate::cmd::run_blocking;
use crate::cmd::serve::auth::CurrentUser;
use crate::cmd::serve::state::AppState;
use crate::types::timestamp::Timestamp;

/// Who an MCP request is from, once its token has been resolved.
///
/// `None` is the shared `default` tree — an instance with no `[oidc]` — and
/// is a real identity here rather than an absence, exactly as it is
/// everywhere else in the server.
#[derive(Clone)]
pub struct McpCaller {
    owner: Option<String>,
}

impl McpCaller {
    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    /// The same identity in the form every domain function already takes.
    ///
    /// This is the whole of multi-user support in the MCP: past this point
    /// a tool handler calls exactly the function a web handler calls, with
    /// exactly the argument a browser session would have produced.
    pub fn current_user(&self) -> Option<CurrentUser> {
        self.owner.as_ref().map(|email| CurrentUser {
            email: email.clone(),
        })
    }
}

/// Refuse anything without a live bearer token, and hand the resolved
/// identity to the service behind us in the request's extensions.
///
/// `401` with a `WWW-Authenticate` header, never a redirect: this route
/// deliberately sits outside `require_auth`, because being sent to
/// `/auth/login` means nothing to an MCP client.
pub async fn require_bearer(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(secret) = bearer_token(&headers) else {
        return unauthorized("A bearer token is required. Mint one at /tokens.");
    };
    let Some(auth) = state.auth.clone() else {
        return unauthorized("This server has no token store, so it cannot authenticate you.");
    };
    // SQLite, so not on the async executor.
    let resolved = run_blocking(move || auth.resolve(&secret, Timestamp::now())).await;
    let owner = match resolved {
        Ok(Some(owner)) => owner,
        Ok(None) => {
            return unauthorized("That token is not valid. It may have been revoked.");
        }
        Err(e) => {
            log::error!("Could not check an MCP token: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Could not check your token.")
                .into_response();
        }
    };
    request.extensions_mut().insert(McpCaller { owner });
    next.run(request).await
}

/// The token out of an `Authorization: Bearer …` header, if it is one and
/// it is shaped like a hashcards token.
fn bearer_token(headers: &HeaderMap) -> Option<TokenSecret> {
    let raw = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    let rest = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))?;
    TokenSecret::parse(rest).ok()
}

fn unauthorized(message: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(WWW_AUTHENTICATE, "Bearer realm=\"hashcards\"")],
        message.to_string(),
    )
        .into_response()
}
```

`CurrentUser`'s `email` field is `pub(crate)` (`auth.rs:29`); if constructing one from outside that module does not compile, add a constructor there rather than widening the field.

- [ ] **Step 5: Write the server handler**

Create `src/cmd/serve/mcp/server.rs`:

```rust
use rmcp::ErrorData;
use rmcp::RoleServer;
use rmcp::ServerHandler;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::model::Implementation;
use rmcp::model::ServerCapabilities;
use rmcp::model::ServerInfo;
use rmcp::service::RequestContext;
use rmcp::tool_handler;

use crate::cmd::serve::mcp::auth::McpCaller;
use crate::cmd::serve::state::AppState;

/// What the model is told at `initialize`.
///
/// Everything here is something a model cannot infer from tool names, and
/// getting any of it wrong produces cards that look right and hash
/// differently. Tool descriptions repeat the syntax wherever a tool takes
/// card text, because a model reading one schema may never have read this.
pub const INSTRUCTIONS: &str = "\
hashcards is a spaced-repetition system over plain Markdown files.

The taxonomy, outermost first:

  user -> collection -> deck -> card

A collection is a top-level folder in the user's card tree, addressed by a
slug derived from its folder name. A deck is a Markdown file inside a
collection; the folder structure below the collection is yours to organise.
A card is a block inside a deck file.

A card hash is a CONTENT ADDRESS: it is derived from the card's text, so
editing a card changes its hash. `update_card` takes the hash of the card as
it is now; if that hash no longer resolves, the card has already been changed
or moved, and you should read it again rather than retrying.

Card syntax. Cards in a file are separated by a line containing only `---`.

A basic card is a question and an answer:

    Q: What is the capital of France?
    A: Paris.

Either may run over several lines, in which case the text starts on the line
after the marker.

A cloze card is one text with deletions marked by square brackets. Each
deletion becomes its own card:

    C: The [order] of a group is [the cardinality of its underlying set].

A file may begin with TOML frontmatter between `---` lines. A `name` there
overrides the deck name that would otherwise come from the file name.

Scheduling is read-only. You can read a card's statistics and review history,
but you cannot set a due date, suspend a card, or make it be forgotten.

Nothing you delete is destroyed: it goes to the user's trash, and you can
list and restore from it. Only the user can empty the trash, from the web
interface.";

/// The MCP server: one per request, cheap to build, holding the same
/// `AppState` every web handler holds.
#[derive(Clone)]
pub struct HashcardsMcp {
    pub state: AppState,
    tool_router: ToolRouter<Self>,
}

impl HashcardsMcp {
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            // Groups are added here as later tasks land:
            //   Self::read_router() + Self::card_router() + ...
            tool_router: ToolRouter::new(),
        }
    }

    /// Who is calling, out of the request extensions the bearer middleware
    /// filled in. Every tool handler starts with this.
    ///
    /// A failure here is a bug rather than a client error — the middleware
    /// refuses anything it cannot resolve, so reaching a tool without a
    /// caller means the route was mounted without the layer.
    pub fn caller(&self, ctx: &RequestContext<RoleServer>) -> Result<McpCaller, ErrorData> {
        ctx.extensions
            .get::<http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<McpCaller>())
            .cloned()
            .ok_or_else(|| {
                ErrorData::internal_error(
                    "This request arrived without an identity, which should not be possible. \
                     Please report this.",
                    None,
                )
            })
    }
}

#[tool_handler]
impl ServerHandler for HashcardsMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "hashcards-web".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                ..Default::default()
            },
            instructions: Some(INSTRUCTIONS.to_string()),
            ..Default::default()
        }
    }
}
```

`ServerInfo`, `Implementation` and `ServerCapabilities` are `#[non_exhaustive]` in places; keep the `..Default::default()` tails. If `Implementation` has a required field this omits, fill it rather than removing the tail.

- [ ] **Step 6: Mount it**

Create `src/cmd/serve/mcp/mod.rs`, above the tests:

```rust
pub mod auth;
pub mod server;

use std::sync::Arc;

use axum::Router;
use rmcp::transport::streamable_http_server::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::StreamableHttpServerConfig;

use crate::cmd::serve::mcp::auth::require_bearer;
use crate::cmd::serve::mcp::server::HashcardsMcp;
use crate::cmd::serve::state::AppState;
use crate::cmd::serve::upload::MAX_UPLOAD_BYTES;

/// `/mcp`, with its own authentication.
///
/// Returned as its own `Router` so `server.rs` can merge it *after* the
/// `require_auth` layer, exactly as the `/auth/*` routes are merged and for
/// the same reason: that layer redirects to `/auth/login`, which means
/// nothing to an MCP client.
pub fn mcp_routes(state: &AppState) -> Router<AppState> {
    let for_service = state.clone();
    let service = StreamableHttpService::new(
        move || Ok(HashcardsMcp::new(for_service.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig {
            // A tool call is request-response, so it is answered as plain
            // JSON with no stream and no session id to carry. The transport
            // falls back to SSE by itself if a handler ever emits a
            // notification mid-call.
            json_response: true,
            legacy_session_mode: false,
            // `write_deck` sends a whole card file, which is the same order
            // of size as a pasted image.
            max_request_body_bytes: MAX_UPLOAD_BYTES,
            allowed_hosts: state.config.mcp.allowed_hosts.clone(),
            ..Default::default()
        },
    );
    Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ))
}
```

In `src/cmd/serve/mod.rs`: `pub mod mcp;`

In `src/cmd/serve/server.rs`, after the static routes are merged and before `with_state`:

```rust
    // After `require_auth`, like `/auth/*`: an MCP client cannot follow a
    // redirect to a login page, so `/mcp` does its own bearer check.
    let app = if state.config.mcp.enabled {
        app.merge(mcp_routes(&state))
    } else {
        app
    };
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test mcp`
Expected: 6 passing tests.

Two things are likely to need adjusting against the real SDK, and both are mechanical: the exact module path of `StreamableHttpServerConfig` (it may be re-exported at `rmcp::transport::streamable_http_server::StreamableHttpServerConfig`) and the client transport constructor. Follow the compiler.

- [ ] **Step 8: Run the whole suite**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: 524 passed (518 + 6). Record the number.

- [ ] **Step 9: Commit**

```bash
git add src/cmd/serve/mcp Cargo.toml Cargo.lock src/cmd/serve/mod.rs src/cmd/serve/server.rs
git commit -m "feat: an authenticated /mcp endpoint, with no tools on it yet

Mounted after require_auth, exactly where /auth/* is merged and for the
same reason: that layer redirects to /auth/login, which means nothing to
an MCP client. /mcp answers 401 with WWW-Authenticate instead.

rmcp's service factory never sees the HTTP request, so the bearer check
is a layer in front and the resolved identity travels in request
extensions, which rmcp propagates to handlers. It resolves to the same
Option<CurrentUser> every domain function in the server already takes --
which is the whole of multi-user support here.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_014ccw7LJasy5iUoo9pcZM7U"
```

---

## Task 9: The adapter seam

A pure refactor: no behaviour changes, no tests are added or removed. It exists so the tool tasks that follow are adapters rather than a second implementation of hashcards.

The mutations the tools need are already blocking free functions taking the owner and returning `Fallible<T>` — the axum handler on top of each is only a flash message and a redirect. What stops a tool calling them is that four of them take `Form`-shaped structs, and one is private to its module.

**Files:**
- Modify: `src/cmd/serve/files.rs` (`NewEntryForm`, `RenameForm`, `DeleteForm`, `SaveForm`, and the four functions taking them)
- Modify: `src/cmd/serve/edit.rs` (`edit_post_inner` visibility, `EditForm`)
- Modify: `src/cmd/stats_page.rs`, `src/cmd/serve/decks.rs`, `src/cmd/serve/cards.rs` (visibility only)

**Interfaces:**
- Consumes: nothing.
- Produces, raised to `pub(crate)` and taking plain argument structs:
  - `pub(crate) fn create_entry(state: &AppState, user: Option<&CurrentUser>, parent: &str, name: &str, is_dir: bool) -> Fallible<String>`
  - `pub(crate) fn rename_entry(state: &AppState, user: Option<&CurrentUser>, path: &str, name: &str) -> Fallible<String>`
  - `pub(crate) fn delete_entry(state: &AppState, user: Option<&CurrentUser>, path: &str) -> Fallible<String>`
  - `pub(crate) fn save_file(state: &AppState, user: Option<&CurrentUser>, rel: &str, content: &str, mtime: u64) -> Fallible<String>`
  - `pub(crate) fn edit_post_inner(state: &AppState, slug: &str, hash_hex: &str, form: EditForm, owner: Option<&str>) -> Fallible<EditOutcome>` — unchanged except for visibility; `EditForm` becomes `pub(crate)` too.
  - `pub(crate) fn gather_stats(...)`, `pub(crate) fn persist_custom_decks(...)`, `pub(crate) fn existing_collections_for_user(...)` — already public enough; confirm rather than change.

- [ ] **Step 1: Change the four signatures**

For each of `create_entry`, `rename_entry`, `delete_entry` and `save_file`, replace the `&Form` parameter with the fields it reads. The form structs stay exactly as they are — they are what axum deserializes into — and the handler unpacks them at the call site:

```rust
pub async fn files_folder_handler(
    State(state): State<AppState>,
    current_user: Option<CurrentUser>,
    Form(form): Form<NewEntryForm>,
) -> Redirect {
    flash_for(
        run_blocking(move || {
            create_entry(&state, current_user.as_ref(), &form.parent, &form.name, true)
        })
        .await,
    )
}
```

Inside each function, replace `form.parent` with `parent`, `form.name` with `name`, and so on. Nothing else moves.

- [ ] **Step 2: Raise the visibility**

`create_entry`, `rename_entry`, `delete_entry`, `save_file` become `pub(crate)`. In `edit.rs`, `edit_post_inner` and `EditForm` become `pub(crate)`.

- [ ] **Step 3: Fix the tests**

The existing tests call these with form structs. Update the call sites to pass the fields directly. This is the bulk of the diff and it is entirely mechanical:

```rust
        create_entry(&state, None, "", "Spanish", true)?;
```

in place of

```rust
        create_entry(
            &state,
            None,
            &NewEntryForm {
                parent: String::new(),
                name: "Spanish".to_string(),
            },
            true,
        )?;
```

- [ ] **Step 4: Run the whole suite**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: 524 passed — **exactly the same number as Task 8.** A refactor that changes the count has changed behaviour; find out why before continuing.

- [ ] **Step 5: Commit**

```bash
git add src/cmd/serve/files.rs src/cmd/serve/edit.rs src/cmd/stats_page.rs src/cmd/serve/decks.rs src/cmd/serve/cards.rs
git commit -m "refactor: domain functions take arguments, not form structs

No behaviour change: the same test count before and after.

The file manager's mutations were already free functions taking the
owner and returning Fallible<String>, with the axum handler a flash
message and a redirect on top. Only the Form-shaped argument stopped
anything but a form calling them. Now the handler unpacks the form and
the function takes what it reads, so the MCP tools can be adapters over
these rather than a second implementation of the same rules -- and every
guard they carry (refuse_if_drilling, the slug collision check, the
migration gate, path validation) applies to the MCP because it is the
same code.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_014ccw7LJasy5iUoo9pcZM7U"
```

---

## Task 10: The read tools

Seven tools, and one new `Database` method. Every one of them goes through `existing_collections_for_user`, never `collections_for_user`: a tool described as read-only must not write a `.hashcards.toml` into a folder that has none.

**Files:**
- Create: `src/cmd/serve/mcp/tools/mod.rs`
- Create: `src/cmd/serve/mcp/tools/read.rs`
- Modify: `src/cmd/serve/mcp/server.rs` (add the group to the router)
- Modify: `src/cmd/serve/handlers.rs` (`find_collection` becomes `pub(crate)`)
- Modify: `src/db.rs` (add `reviews_for_card`)

**Interfaces:**
- Consumes: `HashcardsMcp::caller` (Task 8); `existing_collections_for_user`, `find_collection`, `gather_stats`, `parse_deck`, `UserDatabase`.
- Produces, in `crate::cmd::serve::mcp::tools::read`:
  - `impl HashcardsMcp` with `#[tool_router(router = read_router, vis = pub(crate))]` and seven `#[tool]` methods.
- And, in `crate::db`:
  - `pub fn Database::reviews_for_card(&self, card_hash: CardHash) -> Fallible<Vec<ReviewRow>>`

- [ ] **Step 1: Write the failing test for the new database method**

In `src/db.rs`'s test module:

```rust
    #[test]
    fn a_cards_reviews_come_back_in_order_and_skip_voided_ones() -> Fallible<()> {
        let db = UserDatabase::memory()?.collection(CollectionId::new("abc12345")?);
        let hash = CardHash::hash_bytes(b"a card");
        db.insert_card(hash, Timestamp::now())?;
        let session = db.create_session(Timestamp::now())?;
        db.insert_review_immediately(session, hash, Grade::Good, Timestamp::now())?;
        db.insert_review_immediately(session, hash, Grade::Easy, Timestamp::now())?;
        let reviews = db.reviews_for_card(hash)?;
        assert_eq!(reviews.len(), 2);
        db.void_review_and_restore_performance(reviews[1].review_id)?;
        assert_eq!(db.reviews_for_card(hash)?.len(), 1);
        Ok(())
    }
```

Check the exact signatures of `insert_review_immediately` and `void_review_and_restore_performance` (`db.rs:424`, `db.rs:386`) and match them; the assertion is what matters.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test reviews_for_card`
Expected: FAIL — no method `reviews_for_card`.

- [ ] **Step 3: Add the method**

In `src/db.rs`, next to `get_reviews_for_session` (`db.rs:777`), following its shape exactly — one lock at the top, free functions below it:

```rust
    /// Every surviving review of one card, oldest first.
    ///
    /// `voided = 0`, like every other read path: an undone review is marked
    /// rather than deleted, and showing it here would tell a model a card
    /// was graded when the user took that back.
    pub fn reviews_for_card(&self, card_hash: CardHash) -> Fallible<Vec<ReviewRow>> {
        let conn = self.conn.lock();
        reviews_for_card_on(&conn, &self.collection_id, card_hash)
    }
```

with the free function beside the other free functions in that file. Copy the row-mapping out of `get_reviews_for_session` rather than inventing a second shape, and scope it with `and collection_id = ?`.

- [ ] **Step 4: Write the failing tool tests**

Create `src/cmd/serve/mcp/tools/read.rs` with the licence header and this test module. `mcp_fixture` is a helper added to `src/cmd/serve/mcp/tools/mod.rs`'s test module: a temp `data_dir`, an `AppState` with an `auth` db, one collection `Spanish` holding `verbs.md`, and a `HashcardsMcp` over it.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::serve::mcp::tools::tests::mcp_fixture;

    #[test]
    fn listing_collections_finds_the_users_own() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        let found = list_collections_for(&mcp.state, None)?;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].slug, "Spanish");
        Ok(())
    }

    /// A read tool must not materialise anything.
    #[test]
    fn listing_collections_does_not_write_an_id_into_a_bare_folder() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        let root = crate::cmd::serve::cards::CardRoot::for_user(dir.path(), None)?;
        std::fs::create_dir_all(root.path().join("German"))?;
        list_collections_for(&mcp.state, None)?;
        assert!(
            !root.path().join("German/.hashcards.toml").exists(),
            "a read tool created a collection id"
        );
        Ok(())
    }

    #[test]
    fn reading_a_deck_returns_its_source() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        let text = read_deck_for(&mcp.state, None, "Spanish", "verbs.md")?;
        assert!(text.contains("Q: hablar"), "{text}");
        Ok(())
    }

    #[test]
    fn reading_a_deck_outside_the_collection_is_refused() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        assert!(read_deck_for(&mcp.state, None, "Spanish", "../../../etc/passwd").is_err());
        Ok(())
    }

    #[test]
    fn listing_cards_finds_them_and_filters_by_text() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        let all = list_cards_for(&mcp.state, None, "Spanish", None, false, None, 50)?;
        assert_eq!(all.len(), 2);
        let filtered = list_cards_for(&mcp.state, None, "Spanish", None, false, Some("comer"), 50)?;
        assert_eq!(filtered.len(), 1);
        Ok(())
    }

    #[test]
    fn getting_a_card_returns_its_text_and_an_empty_history() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        let cards = list_cards_for(&mcp.state, None, "Spanish", None, false, None, 50)?;
        let card = get_card_for(&mcp.state, None, "Spanish", &cards[0].hash)?;
        assert!(!card.front.is_empty());
        assert!(card.reviews.is_empty());
        Ok(())
    }

    #[test]
    fn getting_a_card_that_is_not_there_says_so_usefully() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        let err = get_card_for(&mcp.state, None, "Spanish", "0".repeat(64).as_str()).unwrap_err();
        assert!(
            err.message().contains("changed") || err.message().contains("no card"),
            "a stale hash must tell the model to read the card again: {}",
            err.message()
        );
        Ok(())
    }

    #[test]
    fn an_unknown_collection_is_refused_by_name() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        let err = read_deck_for(&mcp.state, None, "Klingon", "verbs.md").unwrap_err();
        assert!(err.message().contains("Klingon"), "{}", err.message());
        Ok(())
    }

    /// The isolation property, tested on its own rather than riding along.
    #[test]
    fn one_users_token_cannot_read_anothers_collection() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        let theirs = crate::cmd::serve::cards::CardRoot::for_user(dir.path(), Some("you@example.com"))?;
        std::fs::create_dir_all(theirs.path().join("German"))?;
        std::fs::write(theirs.path().join("German/nouns.md"), "Q: der Hund\nA: the dog\n")?;
        crate::cmd::serve::cards::collection_id(&theirs.path().join("German"))?;

        let mine = list_collections_for(&mcp.state, None)?;
        assert!(
            mine.iter().all(|c| c.slug != "German"),
            "another user's collection is visible"
        );
        assert!(read_deck_for(&mcp.state, None, "German", "nouns.md").is_err());
        Ok(())
    }
}
```

- [ ] **Step 5: Run the tests to verify they fail**

Run: `cargo test mcp::tools`
Expected: compilation failure.

- [ ] **Step 6: Write the tools**

Create `src/cmd/serve/mcp/tools/mod.rs`:

```rust
pub mod read;

#[cfg(test)]
pub(crate) mod tests {
    use crate::auth_db::AuthDatabase;
    use crate::cmd::serve::cards::CardRoot;
    use crate::cmd::serve::cards::collection_id;
    use crate::cmd::serve::mcp::server::HashcardsMcp;
    use crate::cmd::serve::state::state_with_data_dir;
    use crate::error::Fallible;
    use std::sync::Arc;
    use tempfile::TempDir;

    /// A server over one collection, `Spanish`, holding two cards.
    pub(crate) fn mcp_fixture() -> Fallible<(TempDir, HashcardsMcp)> {
        let dir = TempDir::new()?;
        let mut state = state_with_data_dir(dir.path().to_path_buf());
        state.auth = Some(Arc::new(AuthDatabase::open(&dir.path().join("auth.db"))?));
        let root = CardRoot::for_user(dir.path(), None)?;
        std::fs::create_dir_all(root.path().join("Spanish"))?;
        std::fs::write(
            root.path().join("Spanish/verbs.md"),
            "Q: hablar\nA: to speak\n\n---\n\nQ: comer\nA: to eat\n",
        )?;
        collection_id(&root.path().join("Spanish"))?;
        std::fs::create_dir_all(dir.path().join("db"))?;
        Ok((dir, HashcardsMcp::new(state)))
    }
}
```

Then `src/cmd/serve/mcp/tools/read.rs`. The pattern is the same for every tool in every later task, so read it carefully once: a plain `Fallible` free function holding all the logic, and a thin `#[tool]` method that resolves the caller, hands the work to `run_blocking`, and converts the error.

```rust
use rmcp::ErrorData;
use rmcp::RoleServer;
use rmcp::handler::server::wrapper::Json;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::service::RequestContext;
use rmcp::tool;
use rmcp::tool_router;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

use crate::cmd::run_blocking;
use crate::cmd::serve::auth::CurrentUser;
use crate::cmd::serve::files::existing_collections_for_user;
use crate::cmd::serve::handlers::find_collection;
use crate::cmd::serve::mcp::server::HashcardsMcp;
use crate::cmd::serve::state::AppState;
use crate::error::ErrorReport;
use crate::error::Fallible;
use crate::error::fail;

/// Every tool answers a `Fallible`, and every failure reaches the model as
/// the message that would have reached a person. That is not a shortcut:
/// these messages are already written to be read by whoever has to fix the
/// problem, and here that reader is the one who can fix it fastest.
fn to_mcp(e: ErrorReport) -> ErrorData {
    ErrorData::invalid_params(e.message().to_string(), None)
}

#[derive(Serialize, JsonSchema)]
pub struct CollectionSummary {
    pub slug: String,
    pub name: String,
}

fn list_collections_for(
    state: &AppState,
    user: Option<&CurrentUser>,
) -> Fallible<Vec<CollectionSummary>> {
    Ok(existing_collections_for_user(state, user)
        .into_iter()
        .map(|c| CollectionSummary {
            slug: c.slug,
            name: c.name,
        })
        .collect())
}

/// The collection `slug` names, refused by name when it is not there.
fn collection_of(
    state: &AppState,
    user: Option<&CurrentUser>,
    slug: &str,
) -> Fallible<crate::cmd::serve::config::ResolvedCollection> {
    let owner = user.map(|u| u.email.to_lowercase());
    find_collection(state, slug, owner.as_deref()).ok_or_else(|| {
        ErrorReport::new(format!(
            "There is no collection called `{slug}`. Use list_collections to see what there is."
        ))
    })
}

fn read_deck_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    slug: &str,
    deck: &str,
) -> Fallible<String> {
    let rc = collection_of(state, user, slug)?;
    let root = crate::cmd::serve::files::user_root_readonly(state, user)?;
    // Resolved through `CardRoot`, so `..` and symlinks are refused here
    // exactly as they are for a browser.
    let rel = format!("{slug}/{deck}");
    let entry = root.resolve_entry(&rel)?;
    if !entry.path.starts_with(&rc.coll_dir) || !entry.path.is_file() {
        return fail(format!("There is no deck called `{deck}` in `{slug}`."));
    }
    Ok(std::fs::read_to_string(&entry.path)?)
}
```

Write `list_cards_for`, `get_card_for`, `collection_stats_for` and `user_stats_for` in the same shape, then the tool methods:

```rust
#[derive(Deserialize, JsonSchema)]
pub struct CollectionArgs {
    /// The collection's slug, as returned by list_collections.
    pub collection: String,
}

#[tool_router(router = read_router, vis = pub(crate))]
impl HashcardsMcp {
    #[tool(
        description = "List the collections you can read and write. A collection is a top-level \
                       folder of Markdown card files; every other tool takes its slug."
    )]
    async fn list_collections(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<Vec<CollectionSummary>>, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || list_collections_for(&state, caller.current_user().as_ref()))
            .await
            .map(Json)
            .map_err(to_mcp)
    }

    #[tool(
        description = "Read a deck's Markdown source, exactly as it is on disk. A deck is one \
                       file inside a collection; `deck` is its path relative to the collection \
                       folder, such as `verbs.md` or `Unit 2/nouns.md`."
    )]
    async fn read_deck(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<ReadDeckArgs>,
    ) -> Result<String, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || {
            read_deck_for(
                &state,
                caller.current_user().as_ref(),
                &args.collection,
                &args.deck,
            )
        })
        .await
        .map_err(to_mcp)
    }

    // ... get_collection, list_cards, get_card, get_collection_stats,
    // get_user_stats, each in exactly this shape.
}
```

Every description that takes card text repeats the syntax, because a model reading one tool's schema may never have read the `initialize` instructions:

```rust
    #[tool(
        description = "List the cards in a collection. Filter by `deck`, by `due_only`, or by \
                       `query`, which matches the card's text. Returns each card's content \
                       address (`hash`), which is what get_card and update_card take -- and \
                       which changes whenever the card's text changes."
    )]
```

- [ ] **Step 7: Register the group**

In `src/cmd/serve/mcp/server.rs`, in `HashcardsMcp::new`:

```rust
            tool_router: Self::read_router(),
```

and `pub mod tools;` in `src/cmd/serve/mcp/mod.rs`.

- [ ] **Step 8: Raise `find_collection`**

In `src/cmd/serve/handlers.rs:266`, `pub(super)` becomes `pub(crate)`. Same for `find_drill_target` at `:221` if a later task needs it.

- [ ] **Step 9: Run the tests to verify they pass**

Run: `cargo test mcp`
Expected: 9 new passing tests in `read`, plus 1 in `db`.

- [ ] **Step 10: Run the whole suite**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: 534 passed (524 + 10). Record the number.

- [ ] **Step 11: Commit**

```bash
git add src/cmd/serve/mcp src/db.rs src/cmd/serve/handlers.rs
git commit -m "feat: the MCP read tools

Seven tools over collections, decks, cards and statistics, and one new
database method for a card's review history -- filtered on voided = 0
like every other read path, because an undone review must not be
reported as a grade the user took back.

All of them go through existing_collections_for_user, never
collections_for_user: a tool described as read-only must not write a
.hashcards.toml into a folder that has none, and there is a test that
says so.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_014ccw7LJasy5iUoo9pcZM7U"
```

---

## The shape every write tool has

Tasks 11–15 all follow this. Read it once; each task then gives its own tools, tests and code.

1. Resolve the caller: `let caller = self.caller(&ctx)?;`
2. Do every bit of work inside `run_blocking` — parsing a deck or touching SQLite on the async executor is a bug (BUG-44) whether a browser or a model asked for it.
3. Call the domain function from Task 9. **Never reimplement one.** The guards those functions carry — `refuse_if_drilling`, the slug-collision check, the `migration_failures` gate, `CardRoot::resolve_entry` — apply to the MCP only because it is the same code.
4. Convert the error with `to_mcp` from `read.rs`.

Each group lives in its own file under `src/cmd/serve/mcp/tools/`, declares its own router with `#[tool_router(router = <group>_router, vis = pub(crate))]`, and is added to `HashcardsMcp::new` with `+`, which `ToolRouter` implements:

```rust
            tool_router: Self::read_router() + Self::card_router(),
```

---

## Task 11: Card tools

`update_card` goes through `edit_post_inner`, which re-keys a running drill session through `migrate_sessions`. That is decision 1 of the spec paying for itself: a card edited over MCP updates a live session for free, and a separate stdio binary could not have done it.

**Files:**
- Create: `src/cmd/serve/mcp/tools/cards.rs`
- Modify: `src/cmd/serve/mcp/tools/mod.rs` (`pub mod cards;`)
- Modify: `src/cmd/serve/mcp/server.rs` (add `+ Self::card_router()`)

**Interfaces:**
- Consumes: `edit_post_inner`, `EditForm`, `save_file` (Task 9); `to_mcp`, `collection_of` (Task 10, raised to `pub(super)`).
- Produces: `#[tool_router(router = card_router, vis = pub(crate))] impl HashcardsMcp` with `create_card`, `update_card`, `delete_card`.

- [ ] **Step 1: Write the failing tests**

Create `src/cmd/serve/mcp/tools/cards.rs` with the licence header and:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::serve::mcp::tools::read::list_cards_for;
    use crate::cmd::serve::mcp::tools::tests::mcp_fixture;

    #[test]
    fn a_created_card_is_in_the_file_and_in_the_listing() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        create_card_for(
            &mcp.state,
            None,
            "Spanish",
            "verbs.md",
            "Q: beber\nA: to drink\n",
        )?;
        let cards = list_cards_for(&mcp.state, None, "Spanish", None, false, Some("beber"), 50)?;
        assert_eq!(cards.len(), 1);
        Ok(())
    }

    #[test]
    fn a_created_card_that_does_not_parse_is_refused_and_changes_nothing() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        let path = dir.path().join("cards/default/Spanish/verbs.md");
        let before = std::fs::read_to_string(&path)?;
        assert!(create_card_for(&mcp.state, None, "Spanish", "verbs.md", "A: no question\n").is_err());
        assert_eq!(std::fs::read_to_string(&path)?, before);
        Ok(())
    }

    /// The property the whole edit path exists to preserve.
    #[test]
    fn an_updated_card_keeps_its_review_history() -> Fallible<()> {
        use crate::cmd::serve::cards::CardRoot;
        use crate::cmd::serve::cards::collection_id;
        use crate::cmd::serve::cards::user_db_path;
        use crate::types::card_hash::CardHash;
        use crate::types::timestamp::Timestamp;
        use crate::user_db::UserDatabase;

        let (dir, mcp) = mcp_fixture()?;
        let root = CardRoot::for_user(dir.path(), None)?;
        let id = collection_id(&root.path().join("Spanish"))?;
        let db = UserDatabase::open(&user_db_path(&root, &dir.path().join("db"))?)?;

        let cards = list_cards_for(&mcp.state, None, "Spanish", None, false, Some("hablar"), 50)?;
        let old = CardHash::from_hex(&cards[0].hash)?;
        db.collection(id.clone()).insert_card(old, Timestamp::now())?;

        update_card_for(
            &mcp.state,
            None,
            "Spanish",
            &cards[0].hash,
            "Q: hablar\nA: to speak, to talk\n",
        )?;

        let after = list_cards_for(&mcp.state, None, "Spanish", None, false, Some("to talk"), 50)?;
        assert_eq!(after.len(), 1);
        let new = CardHash::from_hex(&after[0].hash)?;
        assert_ne!(new, old, "the hash must change when the text changes");
        assert!(
            db.collection(id).card_hashes()?.contains(&new),
            "the review history did not follow the card"
        );
        Ok(())
    }

    /// The reason the endpoint is in this process and not a second binary.
    /// Mirrors the web-path test in `edit.rs` — find it with
    /// `grep -n "session" src/cmd/serve/edit.rs` and follow its setup.
    #[test]
    fn editing_a_card_rekeys_a_running_drill_session() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        let cards = list_cards_for(&mcp.state, None, "Spanish", None, false, Some("hablar"), 50)?;
        let old = cards[0].hash.clone();

        // A session holding the old hash, exactly as a drill on
        // /collection/Spanish would leave one.
        seed_session(&mcp.state, "Spanish", &old)?;

        update_card_for(
            &mcp.state,
            None,
            "Spanish",
            &old,
            "Q: hablar\nA: to speak, to talk\n",
        )?;

        let queued = session_card_hashes(&mcp.state, "Spanish");
        assert!(!queued.contains(&old), "the session still holds a dead hash");
        Ok(())
    }

    #[test]
    fn a_stale_hash_tells_the_model_to_read_the_card_again() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        let err = update_card_for(
            &mcp.state,
            None,
            "Spanish",
            &"0".repeat(64),
            "Q: x\nA: y\n",
        )
        .unwrap_err();
        let msg = err.message();
        assert!(
            msg.contains("changed") || msg.contains("moved") || msg.contains("no card"),
            "unhelpful message: {msg}"
        );
        Ok(())
    }

    #[test]
    fn a_deleted_card_leaves_the_others_alone() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        let cards = list_cards_for(&mcp.state, None, "Spanish", None, false, Some("hablar"), 50)?;
        delete_card_for(&mcp.state, None, "Spanish", &cards[0].hash)?;
        let left = list_cards_for(&mcp.state, None, "Spanish", None, false, None, 50)?;
        assert_eq!(left.len(), 1);
        assert!(left[0].front.contains("comer"), "{}", left[0].front);
        Ok(())
    }

    #[test]
    fn another_users_collection_is_refused() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        let theirs = crate::cmd::serve::cards::CardRoot::for_user(dir.path(), Some("you@example.com"))?;
        std::fs::create_dir_all(theirs.path().join("German"))?;
        std::fs::write(theirs.path().join("German/nouns.md"), "Q: der Hund\nA: the dog\n")?;
        crate::cmd::serve::cards::collection_id(&theirs.path().join("German"))?;
        assert!(create_card_for(&mcp.state, None, "German", "nouns.md", "Q: a\nA: b\n").is_err());
        Ok(())
    }
}
```

`seed_session` and `session_card_hashes` are test helpers; put them in `src/cmd/serve/mcp/tools/mod.rs`'s test module beside `mcp_fixture`, modelled on how `edit.rs`'s session test builds and inspects a `DrillSession`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test mcp::tools::cards`
Expected: compilation failure — none of the functions exist.

- [ ] **Step 3: Write the domain functions**

In `src/cmd/serve/mcp/tools/cards.rs`, above the tests:

```rust
use rmcp::ErrorData;
use rmcp::RoleServer;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::service::RequestContext;
use rmcp::tool;
use rmcp::tool_router;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::cmd::run_blocking;
use crate::cmd::serve::auth::CurrentUser;
use crate::cmd::serve::edit::EditForm;
use crate::cmd::serve::edit::edit_post_inner;
use crate::cmd::serve::files::save_file;
use crate::cmd::serve::files::user_root;
use crate::cmd::serve::mcp::server::HashcardsMcp;
use crate::cmd::serve::mcp::tools::read::collection_of;
use crate::cmd::serve::mcp::tools::read::to_mcp;
use crate::cmd::serve::state::AppState;
use crate::error::Fallible;
use crate::utils::file_mtime_ms;

/// Append a card to a deck file.
///
/// Through `save_file`, so the buffer is parsed before it is kept, media
/// is validated, hashes are migrated and a running session is re-keyed —
/// all of it the same code the whole-file editor runs.
fn create_card_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    slug: &str,
    deck: &str,
    card: &str,
) -> Fallible<String> {
    let _rc = collection_of(state, user, slug)?;
    let root = user_root(state, user)?;
    let entry = root.resolve_entry(&format!("{slug}/{deck}"))?;
    let existing = std::fs::read_to_string(&entry.path)?;
    let mut next = existing.trim_end().to_string();
    // `---` on its own line is the separator between cards.
    next.push_str("\n\n---\n\n");
    next.push_str(card.trim_start());
    if !next.ends_with('\n') {
        next.push('\n');
    }
    // The mtime is read here rather than carried by the model: `save_file`
    // re-checks it before the rename, which is what closes the window.
    let mtime = file_mtime_ms(&entry.path)?;
    save_file(state, user, &entry.rel, &next, mtime)?;
    Ok(format!("Added a card to `{deck}`."))
}

/// Replace one card, keeping its review history.
///
/// `edit_post_inner` is the whole of it: it finds the card by hash,
/// re-checks the file's mtime just before the rename, migrates the review
/// rows from the old hash to the new one, and re-keys any running drill
/// session. Reimplementing any of that here would produce a second set of
/// rules that only a model ever exercises.
fn update_card_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    slug: &str,
    hash_hex: &str,
    card: &str,
) -> Fallible<String> {
    let owner = user.map(|u| u.email.to_lowercase());
    let mtime = card_file_mtime(state, user, slug, hash_hex)?;
    let form = EditForm {
        content: card.to_string(),
        mtime_ms: mtime.to_string(),
        return_to: None,
    };
    let outcome = edit_post_inner(state, slug, hash_hex, form, owner.as_deref())?;
    let mut msg = String::from("Card updated.");
    if outcome.skipped > 0 {
        msg.push_str(&format!(
            " {} card(s) could not be matched to their previous review history and start fresh.",
            outcome.skipped
        ));
    }
    Ok(msg)
}
```

`EditForm`'s field names are at `edit.rs:250` — read them and match exactly rather than trusting the sketch. `card_file_mtime` finds the card's file through `find_collection` plus `parse_deck` and reads its mtime; write it beside these. `delete_card_for` splices the card's block out with `extract_card_block`/`block_end` (`edit.rs:437`, `:424`) and goes through `save_file`.

- [ ] **Step 4: Write the tool methods**

```rust
#[derive(Deserialize, JsonSchema)]
pub struct CreateCardArgs {
    /// The collection's slug, from list_collections.
    pub collection: String,
    /// The deck file, relative to the collection folder, e.g. `verbs.md`.
    pub deck: String,
    /// The card's text. `Q:` and `A:` for a basic card, or `C:` with
    /// `[cloze]` deletions. Do not include the `---` separator.
    pub card: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct UpdateCardArgs {
    pub collection: String,
    /// The card's current content address, from list_cards or get_card.
    pub hash: String,
    /// The card's new text, in the same syntax.
    pub card: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct DeleteCardArgs {
    pub collection: String,
    pub hash: String,
}

#[tool_router(router = card_router, vis = pub(crate))]
impl HashcardsMcp {
    #[tool(
        description = "Add a card to a deck. Card syntax: `Q:` then `A:` for a basic card, or \
                       `C:` with `[cloze]` deletions for a cloze card. Do not include the `---` \
                       separator -- it is added for you."
    )]
    async fn create_card(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<CreateCardArgs>,
    ) -> Result<String, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || {
            create_card_for(
                &state,
                caller.current_user().as_ref(),
                &args.collection,
                &args.deck,
                &args.card,
            )
        })
        .await
        .map_err(to_mcp)
    }

    #[tool(
        description = "Replace a card, keeping its review history. `hash` is the card's content \
                       address as it is NOW -- editing a card changes its hash, so if this call \
                       fails saying the card has changed, read it again with get_card rather \
                       than retrying. Card syntax: `Q:`/`A:`, or `C:` with `[cloze]` deletions."
    )]
    async fn update_card(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<UpdateCardArgs>,
    ) -> Result<String, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || {
            update_card_for(
                &state,
                caller.current_user().as_ref(),
                &args.collection,
                &args.hash,
                &args.card,
            )
        })
        .await
        .map_err(to_mcp)
    }

    #[tool(
        description = "Remove a card from its deck. Its review history stays in the database \
                       until the collection is deleted and the trash emptied, so adding the same \
                       card back restores its schedule."
    )]
    async fn delete_card(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<DeleteCardArgs>,
    ) -> Result<String, ErrorData> {
        let caller = self.caller(&ctx)?;
        let state = self.state.clone();
        run_blocking(move || {
            delete_card_for(
                &state,
                caller.current_user().as_ref(),
                &args.collection,
                &args.hash,
            )
        })
        .await
        .map_err(to_mcp)
    }
}
```

- [ ] **Step 5: Register the group**

`src/cmd/serve/mcp/tools/mod.rs`: `pub mod cards;`
`src/cmd/serve/mcp/server.rs`: `tool_router: Self::read_router() + Self::card_router(),`

Raise these in `read.rs` to `pub(super)`, because every later group's code and tests use them: `to_mcp`, `collection_of`, `list_collections_for`, `read_deck_for`, `list_cards_for`, `get_card_for`, and the `CollectionSummary`, `CardSummary` and `CardDetail` types. Do this once, here, rather than a field at a time in each later task.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test mcp::tools::cards`
Expected: 7 passing tests.

- [ ] **Step 7: Run the whole suite**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: 541 passed (534 + 7). Record the number.

- [ ] **Step 8: Commit**

```bash
git add src/cmd/serve/mcp
git commit -m "feat: the MCP card tools

update_card goes through edit_post_inner, so a card edited over MCP
migrates its review history to the new hash and re-keys any running
drill session -- which is decision 1 of the design paying for itself. A
separate stdio binary could not have done the second half: the session's
card queue lives in this process's memory.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_014ccw7LJasy5iUoo9pcZM7U"
```

---

## Task 12: Deck and file tools

**Files:**
- Create: `src/cmd/serve/mcp/tools/decks.rs`
- Modify: `src/cmd/serve/mcp/tools/mod.rs`, `src/cmd/serve/mcp/server.rs`

**Interfaces:**
- Consumes: `create_entry`, `rename_entry`, `delete_entry`, `save_file` (Task 9).
- Produces: `#[tool_router(router = deck_router, vis = pub(crate))]` with `create_deck`, `write_deck`, `move_decks`, `delete_deck`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::serve::mcp::tools::read::list_cards_for;
    use crate::cmd::serve::mcp::tools::read::read_deck_for;
    use crate::cmd::serve::mcp::tools::tests::mcp_fixture;
    use crate::cmd::serve::trash::list_trash;

    #[test]
    fn a_created_deck_is_empty_and_readable() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        create_deck_for(&mcp.state, None, "Spanish", "nouns.md")?;
        assert_eq!(read_deck_for(&mcp.state, None, "Spanish", "nouns.md")?.trim(), "");
        Ok(())
    }

    #[test]
    fn a_written_deck_replaces_the_file() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        write_deck_for(&mcp.state, None, "Spanish", "verbs.md", "Q: beber\nA: to drink\n")?;
        let cards = list_cards_for(&mcp.state, None, "Spanish", None, false, None, 50)?;
        assert_eq!(cards.len(), 1);
        Ok(())
    }

    /// A buffer that does not parse never stays on disk.
    #[test]
    fn a_deck_that_does_not_parse_is_refused_and_the_file_is_unchanged() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        let before = read_deck_for(&mcp.state, None, "Spanish", "verbs.md")?;
        let err = write_deck_for(&mcp.state, None, "Spanish", "verbs.md", "A: dangling\n")
            .unwrap_err();
        assert!(!err.message().is_empty());
        assert_eq!(read_deck_for(&mcp.state, None, "Spanish", "verbs.md")?, before);
        Ok(())
    }

    #[test]
    fn a_deleted_deck_goes_to_the_trash() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        delete_deck_for(&mcp.state, None, "Spanish", "verbs.md")?;
        assert!(read_deck_for(&mcp.state, None, "Spanish", "verbs.md").is_err());
        let trashed = list_trash(dir.path(), "default")?;
        assert_eq!(trashed.len(), 1);
        assert_eq!(trashed[0].original_path, "Spanish/verbs.md");
        Ok(())
    }

    /// The point of the per-user database, and the reason this project
    /// waited for it: moving a deck between collections is a row update,
    /// not a cross-database transfer.
    #[test]
    fn a_deck_moved_between_collections_keeps_its_cards() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        let root = crate::cmd::serve::cards::CardRoot::for_user(dir.path(), None)?;
        std::fs::create_dir_all(root.path().join("German"))?;
        crate::cmd::serve::cards::collection_id(&root.path().join("German"))?;

        move_decks_for(&mcp.state, None, "Spanish", "verbs.md", "German")?;

        assert_eq!(
            list_cards_for(&mcp.state, None, "German", None, false, None, 50)?.len(),
            2
        );
        assert!(list_cards_for(&mcp.state, None, "Spanish", None, false, None, 50)?.is_empty());
        Ok(())
    }

    #[test]
    fn a_deck_path_that_escapes_the_collection_is_refused() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        assert!(create_deck_for(&mcp.state, None, "Spanish", "../escaped.md").is_err());
        assert!(write_deck_for(&mcp.state, None, "Spanish", "../escaped.md", "Q: a\nA: b\n").is_err());
        Ok(())
    }

    #[test]
    fn another_users_collection_is_refused() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        let theirs = crate::cmd::serve::cards::CardRoot::for_user(dir.path(), Some("you@example.com"))?;
        std::fs::create_dir_all(theirs.path().join("German"))?;
        crate::cmd::serve::cards::collection_id(&theirs.path().join("German"))?;
        assert!(create_deck_for(&mcp.state, None, "German", "nouns.md").is_err());
        Ok(())
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test mcp::tools::decks`
Expected: compilation failure.

- [ ] **Step 3: Write the domain functions**

Each is a few lines over Task 9's functions:

```rust
fn create_deck_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    slug: &str,
    deck: &str,
) -> Fallible<String> {
    let _rc = collection_of(state, user, slug)?;
    // `create_entry` appends `.md` itself and refuses a name that would
    // escape, so the deck name is checked exactly as the file manager
    // checks one typed into a browser.
    create_entry(state, user, slug, deck, false)
}

fn write_deck_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    slug: &str,
    deck: &str,
    content: &str,
) -> Fallible<String> {
    let _rc = collection_of(state, user, slug)?;
    let root = user_root(state, user)?;
    let entry = root.resolve_entry(&format!("{slug}/{deck}"))?;
    // Read here, not carried by the model: `save_file` re-checks it just
    // before the rename, which is what closes the window.
    let mtime = file_mtime_ms(&entry.path)?;
    save_file(state, user, &entry.rel, content, mtime)
}

fn delete_deck_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    slug: &str,
    deck: &str,
) -> Fallible<String> {
    let _rc = collection_of(state, user, slug)?;
    delete_entry(state, user, &format!("{slug}/{deck}"))
}

/// Move a deck into another collection.
///
/// `rename_entry` is a move: the destination is a path, not just a name.
/// Its cards' review rows are keyed by `(collection_id, card_hash)`, so
/// after the per-user-database work this is a row update rather than a
/// transfer between two files — which is why this tool exists at all.
fn move_decks_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    from_slug: &str,
    deck: &str,
    to_slug: &str,
) -> Fallible<String> {
    let _from = collection_of(state, user, from_slug)?;
    let _to = collection_of(state, user, to_slug)?;
    rename_entry(
        state,
        user,
        &format!("{from_slug}/{deck}"),
        &format!("{to_slug}/{deck}"),
    )
}
```

Check `rename_entry`'s second argument at `files.rs` — if it takes a bare *name* rather than a path, `move_decks_for` must instead resolve both paths and move the file itself, then call the same session-migration path `rename_entry` uses. Read it before writing this one; do not guess.

- [ ] **Step 4: Write the tool methods**

Four `#[tool]` methods in the shape Task 11 shows, with these descriptions:

```rust
    #[tool(description = "Create an empty deck (a Markdown card file) inside a collection.")]
    #[tool(
        description = "Replace a deck's entire contents. The text must parse as cards or the \
                       write is refused and the file is left as it was. Cards are separated by \
                       a line containing only `---`; each is `Q:` then `A:`, or `C:` with \
                       `[cloze]` deletions. A file may start with TOML frontmatter between \
                       `---` lines, where `name` overrides the deck's name."
    )]
    #[tool(
        description = "Move a deck from one collection to another. Its cards keep their review \
                       history."
    )]
    #[tool(
        description = "Delete a deck. It goes to the user's trash and can be restored; only the \
                       user can empty the trash."
    )]
```

- [ ] **Step 5: Register the group**

`pub mod decks;` and `+ Self::deck_router()`.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test mcp::tools::decks`
Expected: 7 passing tests.

- [ ] **Step 7: Run the whole suite**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: 548 passed (541 + 7). Record the number.

- [ ] **Step 8: Commit**

```bash
git add src/cmd/serve/mcp
git commit -m "feat: the MCP deck tools

move_decks is the tool this whole project waited for the per-user
database to make possible: a deck's cards are rows keyed by
(collection_id, card_hash), so moving them is an update rather than a
transfer between two database files.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_014ccw7LJasy5iUoo9pcZM7U"
```

---

## Task 13: Collection tools

**Files:**
- Create: `src/cmd/serve/mcp/tools/collections.rs`
- Modify: `src/cmd/serve/mcp/tools/mod.rs`, `src/cmd/serve/mcp/server.rs`
- Modify: `src/cmd/serve/cards.rs` (a writer for `SchedulingOverrides`)

**Interfaces:**
- Consumes: `create_entry`, `rename_entry`, `delete_entry` (Task 9); `collection_overrides` (`cards.rs:200`).
- Produces: `#[tool_router(router = collection_router, vis = pub(crate))]` with `create_collection`, `rename_collection`, `delete_collection`, `set_collection_scheduling`.
- And, in `crate::cmd::serve::cards`:
  - `pub fn write_collection_overrides(folder: &Path, overrides: &SchedulingOverrides) -> Fallible<()>` — rewrites `.hashcards.toml` **keeping its `id`**.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::serve::cards::collection_id;
    use crate::cmd::serve::cards::collection_overrides;
    use crate::cmd::serve::mcp::tools::read::list_collections_for;
    use crate::cmd::serve::mcp::tools::tests::mcp_fixture;
    use crate::cmd::serve::trash::list_trash;

    #[test]
    fn a_created_collection_is_listed_and_has_an_id() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        create_collection_for(&mcp.state, None, "German")?;
        assert!(list_collections_for(&mcp.state, None)?.iter().any(|c| c.slug == "German"));
        let root = crate::cmd::serve::cards::CardRoot::for_user(dir.path(), None)?;
        assert!(root.path().join("German/.hashcards.toml").is_file());
        Ok(())
    }

    /// The id is what the review rows are keyed by, so a rename that
    /// changed it would silently start the history over.
    #[test]
    fn a_renamed_collection_keeps_its_id() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        let root = crate::cmd::serve::cards::CardRoot::for_user(dir.path(), None)?;
        let before = collection_id(&root.path().join("Spanish"))?;
        rename_collection_for(&mcp.state, None, "Spanish", "Castellano")?;
        let after = collection_id(&root.path().join("Castellano"))?;
        assert_eq!(before, after);
        Ok(())
    }

    #[test]
    fn a_colliding_name_is_refused_and_says_what_it_collides_with() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        let err = create_collection_for(&mcp.state, None, "Spanish").unwrap_err();
        assert!(err.message().contains("Spanish"), "{}", err.message());
        Ok(())
    }

    #[test]
    fn a_deleted_collection_is_in_the_trash_with_its_rows_intact() -> Fallible<()> {
        use crate::cmd::serve::cards::user_db_path;
        use crate::types::card_hash::CardHash;
        use crate::types::timestamp::Timestamp;
        use crate::user_db::UserDatabase;

        let (dir, mcp) = mcp_fixture()?;
        let root = crate::cmd::serve::cards::CardRoot::for_user(dir.path(), None)?;
        let id = collection_id(&root.path().join("Spanish"))?;
        let db = UserDatabase::open(&user_db_path(&root, &dir.path().join("db"))?)?;
        let hash = CardHash::hash_bytes(b"a card");
        db.collection(id.clone()).insert_card(hash, Timestamp::now())?;

        delete_collection_for(&mcp.state, None, "Spanish")?;

        assert!(list_collections_for(&mcp.state, None)?.is_empty());
        assert_eq!(list_trash(dir.path(), "default")?.len(), 1);
        assert!(
            db.collection(id).card_hashes()?.contains(&hash),
            "the rows a restore would need are gone"
        );
        Ok(())
    }

    #[test]
    fn scheduling_overrides_round_trip() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        set_scheduling_for(&mcp.state, None, "Spanish", Some(0.85), None)?;
        let root = crate::cmd::serve::cards::CardRoot::for_user(dir.path(), None)?;
        let overrides = collection_overrides(&root.path().join("Spanish"));
        assert!(format!("{overrides:?}").contains("0.85"), "{overrides:?}");
        // And the id survived the rewrite.
        assert!(root.path().join("Spanish/.hashcards.toml").is_file());
        collection_id(&root.path().join("Spanish"))?;
        Ok(())
    }

    #[test]
    fn a_retention_outside_the_allowed_range_is_refused() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        assert!(set_scheduling_for(&mcp.state, None, "Spanish", Some(2.0), None).is_err());
        Ok(())
    }

    #[test]
    fn another_users_collection_is_refused() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        let theirs = crate::cmd::serve::cards::CardRoot::for_user(dir.path(), Some("you@example.com"))?;
        std::fs::create_dir_all(theirs.path().join("German"))?;
        collection_id(&theirs.path().join("German"))?;
        assert!(rename_collection_for(&mcp.state, None, "German", "Deutsch").is_err());
        assert!(delete_collection_for(&mcp.state, None, "German").is_err());
        Ok(())
    }
}
```

`DesiredRetention`'s valid range is in `src/types/performance.rs`; read it and make the refusal message the one that type already produces.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test mcp::tools::collections`
Expected: compilation failure.

- [ ] **Step 3: Write the overrides writer**

In `src/cmd/serve/cards.rs`, beside `collection_overrides` (`cards.rs:200`):

```rust
/// Rewrite a collection's `.hashcards.toml` with new scheduling
/// overrides.
///
/// The `id` is read and written back unchanged. It is what every review row
/// in this collection is keyed by, so losing it here would orphan the whole
/// history and give the folder a new one on its next read — the schedule
/// would appear to reset for no reason a user could see.
pub fn write_collection_overrides(
    folder: &Path,
    overrides: &SchedulingOverrides,
) -> Fallible<()> {
    let id = collection_id(folder)?;
    // Build the document from the id plus the overrides, and write it
    // atomically the way the rest of the tree is written.
    // ...
}
```

Read how `collection_id` creates the file in the first place and mirror it, so there is one shape of `.hashcards.toml` and not two.

- [ ] **Step 4: Write the domain functions and tool methods**

```rust
fn create_collection_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    name: &str,
) -> Fallible<String> {
    // A collection is a top-level folder: parent is the root.
    create_entry(state, user, "", name, true)
}

fn rename_collection_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    slug: &str,
    name: &str,
) -> Fallible<String> {
    let _rc = collection_of(state, user, slug)?;
    rename_entry(state, user, slug, name)
}

fn delete_collection_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    slug: &str,
) -> Fallible<String> {
    let _rc = collection_of(state, user, slug)?;
    delete_entry(state, user, slug)
}
```

Tool descriptions:

```rust
    #[tool(
        description = "Create a collection: a top-level folder of decks, with its own review \
                       schedule. The name becomes a URL slug, which must not collide with an \
                       existing collection or saved deck."
    )]
    #[tool(
        description = "Rename a collection. Its review history follows it: the schedule is keyed \
                       by an id inside the folder, not by its name."
    )]
    #[tool(
        description = "Delete a collection and everything in it. It goes to the user's trash \
                       with its review history intact and can be restored; only the user can \
                       empty the trash, and only that erases the history."
    )]
    #[tool(
        description = "Set a collection's scheduling overrides -- desired retention and maximum \
                       interval -- in place of the instance defaults. This does not change any \
                       card's current due date."
    )]
```

- [ ] **Step 5: Register the group**

`pub mod collections;` and `+ Self::collection_router()`.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test mcp::tools::collections`
Expected: 7 passing tests.

- [ ] **Step 7: Run the whole suite**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: 555 passed (548 + 7). Record the number.

- [ ] **Step 8: Commit**

```bash
git add src/cmd/serve/mcp src/cmd/serve/cards.rs
git commit -m "feat: the MCP collection tools

Rewriting .hashcards.toml for a scheduling override reads the id and
writes it back. It is what every review row in the collection is keyed
by, and dropping it would orphan the whole history while the folder
quietly minted itself a new one on the next read.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_014ccw7LJasy5iUoo9pcZM7U"
```

---

## Task 14: Saved deck tools

A saved deck is a user-assembled selection spanning collections, stored in `hashcards.toml` rather than in the card tree.

**Read `deck_add_handler` (`decks.rs:412`) before writing anything.** It does two things — rewrite the config file through `persist_custom_decks`, and refresh `state.custom_decks` in place — and the order matters: getting it backwards leaves the running server disagreeing with its own config file until a restart.

**Files:**
- Create: `src/cmd/serve/mcp/tools/saved.rs`
- Modify: `src/cmd/serve/mcp/tools/mod.rs`, `src/cmd/serve/mcp/server.rs`

**Interfaces:**
- Consumes: `persist_custom_decks`, `resolve_custom_decks`, `slug_for_deck`, `find_custom_deck` (`decks.rs`).
- Produces: `#[tool_router(router = saved_router, vis = pub(crate))]` with `list_saved_decks`, `set_saved_deck`, `delete_saved_deck`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::serve::mcp::tools::tests::mcp_fixture_with_config;

    #[test]
    fn a_saved_deck_round_trips_through_the_config_file() -> Fallible<()> {
        let (dir, mcp, config_path) = mcp_fixture_with_config()?;
        let _ = dir;
        set_saved_deck_for(&mcp.state, None, "Everything", &["Spanish".to_string()])?;
        let toml = std::fs::read_to_string(&config_path)?;
        assert!(toml.contains("Everything"), "{toml}");
        assert_eq!(list_saved_decks_for(&mcp.state, None)?.len(), 1);
        Ok(())
    }

    /// The running server and its config file must not disagree.
    #[test]
    fn a_saved_deck_is_live_without_a_restart() -> Fallible<()> {
        let (_dir, mcp, _config_path) = mcp_fixture_with_config()?;
        set_saved_deck_for(&mcp.state, None, "Everything", &["Spanish".to_string()])?;
        assert!(
            mcp.state.custom_decks.lock().iter().any(|d| d.name == "Everything"),
            "the deck is in the file but not in the running server"
        );
        Ok(())
    }

    #[test]
    fn a_deck_naming_a_collection_that_is_not_yours_is_refused() -> Fallible<()> {
        let (dir, mcp, _config_path) = mcp_fixture_with_config()?;
        let theirs = crate::cmd::serve::cards::CardRoot::for_user(dir.path(), Some("you@example.com"))?;
        std::fs::create_dir_all(theirs.path().join("German"))?;
        crate::cmd::serve::cards::collection_id(&theirs.path().join("German"))?;
        assert!(set_saved_deck_for(&mcp.state, None, "Both", &["German".to_string()]).is_err());
        Ok(())
    }

    #[test]
    fn a_deck_whose_slug_collides_with_a_collection_is_refused() -> Fallible<()> {
        let (_dir, mcp, _config_path) = mcp_fixture_with_config()?;
        let err = set_saved_deck_for(&mcp.state, None, "Spanish", &["Spanish".to_string()])
            .unwrap_err();
        assert!(err.message().contains("Spanish"), "{}", err.message());
        Ok(())
    }

    #[test]
    fn a_deleted_saved_deck_leaves_the_file_and_the_server() -> Fallible<()> {
        let (_dir, mcp, config_path) = mcp_fixture_with_config()?;
        set_saved_deck_for(&mcp.state, None, "Everything", &["Spanish".to_string()])?;
        delete_saved_deck_for(&mcp.state, None, "Everything")?;
        assert!(!std::fs::read_to_string(&config_path)?.contains("Everything"));
        assert!(mcp.state.custom_decks.lock().is_empty());
        Ok(())
    }

    #[test]
    fn a_deck_with_no_members_is_refused() -> Fallible<()> {
        let (_dir, mcp, _config_path) = mcp_fixture_with_config()?;
        assert!(set_saved_deck_for(&mcp.state, None, "Empty", &[]).is_err());
        Ok(())
    }
}
```

`mcp_fixture_with_config` extends `mcp_fixture` with a real `hashcards.toml` on disk and `state.config_path` pointing at it; add it beside `mcp_fixture`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test mcp::tools::saved`
Expected: compilation failure.

- [ ] **Step 3: Write the domain functions**

Follow `deck_add_handler` and `deck_delete_handler` exactly — the validation, the persist, and the in-place refresh, in that order. Do not reorder them and do not skip the refresh.

A saved deck spanning two collections that both hold the same card hash routes arbitrarily (`drill/state.rs:61`). That is pre-existing and out of scope, but do not make it worse: `set_saved_deck` takes an explicit member list and creates nothing implicitly.

- [ ] **Step 4: Write the tool methods**

```rust
    #[tool(
        description = "List the user's saved decks. A saved deck is a selection of collections \
                       drilled together; it holds no cards of its own."
    )]
    #[tool(
        description = "Create or replace a saved deck: a named selection of collections that are \
                       drilled together. Members are collection slugs. This does not move or \
                       copy any cards."
    )]
    #[tool(description = "Delete a saved deck. The collections it drew on are not touched.")]
```

- [ ] **Step 5: Register the group**

`pub mod saved;` and `+ Self::saved_router()`.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test mcp::tools::saved`
Expected: 6 passing tests.

- [ ] **Step 7: Run the whole suite**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: 561 passed (555 + 6). Record the number.

- [ ] **Step 8: Commit**

```bash
git add src/cmd/serve/mcp
git commit -m "feat: the MCP saved-deck tools

Persist to hashcards.toml and refresh state.custom_decks in place, in
that order and both of them -- the same thing deck_add_handler does.
Doing only the first leaves the running server disagreeing with its own
config file until somebody restarts it.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_014ccw7LJasy5iUoo9pcZM7U"
```

---

## Task 15: Trash tools, and no way to empty it

Two tools. **There is deliberately no purge tool, and adding one later is not a small change — it is the entire safety story of this feature.** Emptying the trash is a human action in the web UI, which is what makes "the model cannot destroy anything irrecoverably" a property of the design rather than a hope.

**Files:**
- Create: `src/cmd/serve/mcp/tools/trash.rs`
- Modify: `src/cmd/serve/mcp/tools/mod.rs`, `src/cmd/serve/mcp/server.rs`

**Interfaces:**
- Consumes: `list_trash`, `restore_from_trash`, `TrashId` (Tasks 2–3).
- Produces: `#[tool_router(router = trash_router, vis = pub(crate))]` with `list_trash`, `restore_from_trash`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::serve::mcp::tools::read::list_collections_for;
    use crate::cmd::serve::mcp::tools::tests::mcp_fixture;

    #[test]
    fn the_trash_lists_what_was_deleted() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        crate::cmd::serve::files::delete_entry(&mcp.state, None, "Spanish/verbs.md")?;
        let entries = list_trash_for(&mcp.state, None)?;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].original_path, "Spanish/verbs.md");
        Ok(())
    }

    #[test]
    fn restoring_a_collection_brings_its_history_back() -> Fallible<()> {
        use crate::cmd::serve::cards::CardRoot;
        use crate::cmd::serve::cards::collection_id;
        use crate::cmd::serve::cards::user_db_path;
        use crate::types::card_hash::CardHash;
        use crate::types::timestamp::Timestamp;
        use crate::user_db::UserDatabase;

        let (dir, mcp) = mcp_fixture()?;
        let root = CardRoot::for_user(dir.path(), None)?;
        let id = collection_id(&root.path().join("Spanish"))?;
        let db = UserDatabase::open(&user_db_path(&root, &dir.path().join("db"))?)?;
        let hash = CardHash::hash_bytes(b"a card");
        db.collection(id.clone()).insert_card(hash, Timestamp::now())?;

        crate::cmd::serve::files::delete_entry(&mcp.state, None, "Spanish")?;
        let entries = list_trash_for(&mcp.state, None)?;
        restore_for(&mcp.state, None, entries[0].id.as_str())?;

        assert!(list_collections_for(&mcp.state, None)?.iter().any(|c| c.slug == "Spanish"));
        assert!(db.collection(id).card_hashes()?.contains(&hash));
        Ok(())
    }

    #[test]
    fn restoring_onto_an_occupied_path_is_refused_and_keeps_the_entry() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        crate::cmd::serve::files::delete_entry(&mcp.state, None, "Spanish/verbs.md")?;
        let root = crate::cmd::serve::cards::CardRoot::for_user(dir.path(), None)?;
        std::fs::write(root.path().join("Spanish/verbs.md"), "Q: x\nA: y\n")?;
        let entries = list_trash_for(&mcp.state, None)?;
        assert!(restore_for(&mcp.state, None, entries[0].id.as_str()).is_err());
        assert_eq!(list_trash_for(&mcp.state, None)?.len(), 1);
        Ok(())
    }

    #[test]
    fn one_user_cannot_see_anothers_trash() -> Fallible<()> {
        let (dir, mcp) = mcp_fixture()?;
        let theirs = crate::cmd::serve::cards::CardRoot::for_user(dir.path(), Some("you@example.com"))?;
        std::fs::create_dir_all(theirs.path().join("German"))?;
        crate::cmd::serve::trash::move_to_trash(
            dir.path(),
            &theirs,
            "German",
            crate::cmd::serve::trash::TrashKind::Folder,
            None,
            crate::types::timestamp::Timestamp::now(),
        )?;
        assert!(list_trash_for(&mcp.state, None)?.is_empty());
        Ok(())
    }

    /// The safety property of the whole feature, asserted rather than
    /// hoped for. A tool added in a hurry cannot quietly break it.
    #[test]
    fn no_tool_in_the_whole_surface_destroys_anything() -> Fallible<()> {
        let (_dir, mcp) = mcp_fixture()?;
        let names: Vec<String> = mcp.tool_names();
        for name in &names {
            let lower = name.to_lowercase();
            for forbidden in ["purge", "empty", "destroy", "erase", "wipe"] {
                assert!(
                    !lower.contains(forbidden),
                    "`{name}` looks like it destroys data. Emptying the trash is a human action \
                     in the web UI, and that is what makes this feature safe to hand a model."
                );
            }
        }
        assert!(names.iter().any(|n| n == "restore_from_trash"));
        Ok(())
    }
}
```

`tool_names` is a small accessor on `HashcardsMcp` reading its `ToolRouter` — add it in this task, `#[cfg(test)]` if nothing else needs it.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test mcp::tools::trash`
Expected: compilation failure.

- [ ] **Step 3: Write the domain functions and tool methods**

```rust
fn list_trash_for(state: &AppState, user: Option<&CurrentUser>) -> Fallible<Vec<TrashEntry>> {
    let data_dir = data_dir_of(state)?;
    let root = user_root_readonly(state, user)?;
    list_trash(&data_dir, root.tree_name()?)
}

fn restore_for(state: &AppState, user: Option<&CurrentUser>, raw_id: &str) -> Fallible<String> {
    let data_dir = data_dir_of(state)?;
    let id = TrashId::parse(raw_id)?;
    let root = user_root(state, user)?;
    let rel = restore_from_trash(&data_dir, &root, &id)?;
    Ok(format!("Restored `{rel}`."))
}
```

```rust
    #[tool(
        description = "List what is in the user's trash. Anything you or they deleted is here \
                       until they empty it, and it can be restored."
    )]
    #[tool(
        description = "Restore something from the trash to where it was. A restored collection \
                       gets its review history back. This fails if something has since taken \
                       the same path -- move that first. There is no tool to empty the trash: \
                       only the user can do that, from the web interface."
    )]
```

- [ ] **Step 4: Register the group**

`pub mod trash;` and `+ Self::trash_router()`. The full router in `HashcardsMcp::new` is now:

```rust
            tool_router: Self::read_router()
                + Self::card_router()
                + Self::deck_router()
                + Self::collection_router()
                + Self::saved_router()
                + Self::trash_router(),
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test mcp::tools::trash`
Expected: 5 passing tests.

- [ ] **Step 6: Count the tool surface**

Run: `cargo test mcp` and check the handshake test from Task 8, which asserted an empty tool list — it now needs updating to assert **23** tools. That number is the design's, and a mismatch means a tool was forgotten or invented.

- [ ] **Step 7: Run the whole suite**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: 566 passed (561 + 5). Record the number.

- [ ] **Step 8: Commit**

```bash
git add src/cmd/serve/mcp
git commit -m "feat: the MCP trash tools, and no way to empty it

list_trash and restore_from_trash. There is no purge tool, and a test
asserts that no tool in the whole surface has a name that looks like
one -- so a tool added in a hurry cannot quietly take away the property
this feature rests on. Emptying the trash is a human action in the web
UI, which is what makes handing a model a write token safe.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_014ccw7LJasy5iUoo9pcZM7U"
```

---

## Task 16: The documentation

**Files:**
- Modify: `README.md`
- Modify: `CLAUDE.md`
- Modify: `CHANGELOG.xml`

- [ ] **Step 1: Document the endpoint in the README**

A new section after the configuration section. Cover, in this order: what `/mcp` is and what a model can do through it; minting a token at `/tokens` and that it is shown once; that every token is read-write, so a token is as much authority as the account it belongs to; the `[mcp]` block; and `allowed_hosts` — specifically that without the instance's own hostname in it, every MCP request is refused, because the check exists to stop a web page rebinding DNS at a local MCP server.

End the section with the sentence that matters most operationally: nothing a model deletes is destroyed, because there is no tool that empties the trash.

- [ ] **Step 2: Document the trash in the README**

Its own short section: deleting moves to the trash; restoring is one action; emptying is the only thing in hashcards that destroys anything; emptying takes the review history of a trashed collection with it, so a collection restored before the trash is emptied comes back with its schedule, and one recreated afterwards starts fresh.

- [ ] **Step 3: Correct `CLAUDE.md`**

Terse, as that file asks.

To Layout, add: `src/cmd/serve/mcp/` is the MCP endpoint; `src/cmd/serve/trash.rs` and `trash_ui.rs` are the trash; `src/auth_db.rs` is the token store at `{data_dir}/auth.db`.

To Design and Internals, add: deleting moves to the trash and leaves review rows as orphans, which every read path already ignores; purging is the only thing that erases them; the MCP has no purge tool by design.

And fix what is already wrong: the Layout section lists "git, HedgeDoc" as parts of `src/cmd/serve/`. Neither exists — `grep` finds nothing — and it has been stale since the remove-remote-sources work.

- [ ] **Step 4: Write the changelog entries**

In `CHANGELOG.xml`, inside `<unreleased>`, each as `<change author="claude">…</change>`.

Under `<added>` — the MCP endpoint. What it is; that a token is minted per user at `/tokens` and shown once; that every token is read-write; that content and structure are writable but the schedule is not; and that `[mcp].allowed_hosts` must name the instance's hostnames or every request is refused with nothing in the log to explain it.

Under `<added>` — the trash. Deleting is undoable; emptying is the only thing that destroys anything.

Under `<breaking>` — **deleting no longer takes effect immediately.** A collection or deck deleted from the file manager goes to the trash and keeps its review rows until the trash is emptied. The refusal of a non-empty folder is gone, so deleting a collection no longer requires emptying it first. Say plainly that disk space is not reclaimed until the trash is emptied, because that is the operational surprise.

- [ ] **Step 5: Run the whole suite**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: 566 passed — the count from Task 15, unchanged.

- [ ] **Step 6: Commit**

```bash
git add README.md CLAUDE.md CHANGELOG.xml
git commit -m "docs: the MCP endpoint, and the trash

Also drops git and HedgeDoc from CLAUDE.md's layout section. Neither has
existed since the remove-remote-sources work.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_014ccw7LJasy5iUoo9pcZM7U"
```

---

## What this plan does not do

- **MCP resources.** Tools cover the same ground, and a second read path would have to be kept consistent with the first. Addable later without breaking anything.
- **Writing the schedule** — forget, set due date, suspend. Those are card states, ROADMAP §2, and doing them here would pre-empt that decision.
- **A purge tool over MCP.** Permanently, by design. Task 15 has a test that keeps it that way.
- **`split_collection`, `merge_collections`, `search_cards`.** The first two are compositions of tools that exist — splitting is `create_collection` plus `move_decks`, merging is `move_decks` plus `delete_collection` — and the third is a parameter on `list_cards`.
- **Rate limiting.** The trash is the mitigation for a write token, not a limit on how fast one can be used.
- **`SessionDbs` routing cards by hash** (`drill/state.rs:61`): a saved deck spanning two collections that hold the same card hash routes arbitrarily. Real, pre-existing, noted in the per-user-database spec, and neither caused nor worsened here. Task 14 must not make it worse.

## Risks the executor should know about

- **Card text is untrusted input read by a model.** A collection can hold anything, including text shaped like instructions, and `get_card` hands it to a model that is also holding a write token. Nothing in this plan prevents that, and nothing should try — sanitising the prose would break the product, because the prose *is* the product. The trash is what bounds the damage. Do not add a tool that makes damage unbounded.
- **A hand-written test against `rmcp`'s API will drift from the real one.** Every code block in Tasks 8–15 that touches the SDK is written against version 3.2 as read from its source, but follow the compiler over this document where they disagree, and prefer the SDK's own types to reimplementing them.
- **Task 4 changes behaviour users depend on.** It is the only task that removes a refusal and the only one whose test count goes down. If the count does not land exactly on 504, stop and find out why before continuing.
