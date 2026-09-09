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

// Nothing outside the tests reads this module yet: the /tokens page mints
// through it and the /mcp bearer check resolves through it, and neither
// exists so far. The attribute comes off when they land.
#![cfg_attr(not(test), allow(dead_code))]

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

use crate::error::ErrorReport;
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
/// Shown to the user exactly once, at minting. Nothing stores it -- the
/// database keeps only its `TokenHash` -- so this type exists to be
/// generated, displayed, parsed back out of an `Authorization` header, and
/// digested.
pub struct TokenSecret {
    inner: String,
}

impl TokenSecret {
    /// A fresh token from the operating system's randomness.
    pub fn generate() -> Fallible<Self> {
        let mut bytes = [0u8; TOKEN_BYTES];
        getrandom::fill(&mut bytes).map_err(|e| {
            ErrorReport::new(format!(
                "Could not generate a token: the system's random number source failed ({e})."
            ))
        })?;
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

/// Redacted, deliberately. `Debug` is what a panic message, a `dbg!` and
/// every `{:?}` in a log line reach for, and a secret that only has to be
/// readable once should not be one interpolation away from a log file.
/// `Display` is the way to get at the value, and it has exactly one caller:
/// the page that shows it.
impl std::fmt::Debug for TokenSecret {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("TokenSecret(redacted)")
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
    /// A digest as it comes back from a form. Hex and length are checked so
    /// a revoke form cannot smuggle arbitrary text into a query.
    pub fn parse(raw: &str) -> Fallible<Self> {
        let trimmed = raw.trim();
        if trimmed.len() != blake3::OUT_LEN * 2 || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
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
        insert_token(&conn, &secret.digest(), owner, name, now)?;
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
        let Some(owner) = owner_of(&conn, &hash)? else {
            return Ok(None);
        };
        touch_token(&conn, &hash, now)?;
        Ok(Some(owner))
    }

    /// One user's live tokens, newest first.
    pub fn list(&self, owner: Option<&str>) -> Fallible<Vec<TokenRecord>> {
        let conn = self.conn.lock();
        tokens_of(&conn, owner)
    }

    /// Retire a token. Scoped to its owner: a digest alone must not be
    /// enough to revoke somebody else's. `false` when there was no such
    /// live token of theirs to revoke.
    pub fn revoke(&self, owner: Option<&str>, hash: &TokenHash) -> Fallible<bool> {
        let conn = self.conn.lock();
        revoke_token(&conn, owner, hash)
    }
}

fn insert_token(
    conn: &Connection,
    hash: &TokenHash,
    owner: Option<&str>,
    name: &str,
    now: Timestamp,
) -> Fallible<()> {
    conn.execute(
        "insert into tokens (token_hash, owner, name, created_at) values (?1, ?2, ?3, ?4)",
        params![hash, owner, name, now.to_string()],
    )?;
    Ok(())
}

/// The owner of a live token. `None` when there is no such row, which is a
/// different answer from a row whose owner column is null.
fn owner_of(conn: &Connection, hash: &TokenHash) -> Fallible<Option<Option<String>>> {
    let mut stmt =
        conn.prepare("select owner from tokens where token_hash = ?1 and revoked = 0")?;
    let mut rows = stmt.query(params![hash])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get::<_, Option<String>>(0)?)),
        None => Ok(None),
    }
}

fn touch_token(conn: &Connection, hash: &TokenHash, now: Timestamp) -> Fallible<()> {
    conn.execute(
        "update tokens set last_used_at = ?1 where token_hash = ?2",
        params![now.to_string(), hash],
    )?;
    Ok(())
}

/// `owner is ?1`, not `owner = ?1`: SQL equality against null is never
/// true, and null is a real owner here -- the instance with no `[oidc]`.
fn tokens_of(conn: &Connection, owner: Option<&str>) -> Fallible<Vec<TokenRecord>> {
    let mut stmt = conn.prepare(
        "select token_hash, name, created_at, last_used_at \
         from tokens \
         where revoked = 0 and owner is ?1 \
         order by created_at desc",
    )?;
    let rows = stmt.query_map(params![owner], |row| {
        Ok((
            row.get::<_, TokenHash>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (hash, name, created_at, last_used_at) = row?;
        out.push(TokenRecord {
            hash,
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

fn revoke_token(conn: &Connection, owner: Option<&str>, hash: &TokenHash) -> Fallible<bool> {
    let n = conn.execute(
        "update tokens set revoked = 1 where token_hash = ?1 and owner is ?2 and revoked = 0",
        params![hash, owner],
    )?;
    Ok(n > 0)
}

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
        let now = Timestamp::now();
        let secret = db.mint(Some("me@example.com"), "laptop", now)?;
        db.mint(Some("you@example.com"), "phone", now)?;
        let mine = db.list(Some("me@example.com"))?;
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].name, "laptop");
        // The listing carries what the page needs to draw a row and a
        // revoke button: a digest to name the token by, and when it was
        // made. Never the secret.
        assert_eq!(mine[0].hash, secret.digest());
        assert_eq!(mine[0].created_at, now);
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

    /// A digest arrives from the revoke form, so it is parsed rather than
    /// trusted -- it goes into a query, and the shape is the whole check.
    #[test]
    fn a_token_id_that_is_not_a_digest_is_refused() -> Fallible<()> {
        assert!(TokenHash::parse("").is_err());
        assert!(TokenHash::parse("not-a-digest").is_err());
        assert!(TokenHash::parse("' or 1=1 --").is_err());
        // Right length, wrong alphabet.
        assert!(TokenHash::parse(&"z".repeat(64)).is_err());
        let secret = TokenSecret::generate()?;
        assert_eq!(
            TokenHash::parse(&secret.digest().to_string())?,
            secret.digest()
        );
        Ok(())
    }

    #[test]
    fn two_generated_secrets_differ() -> Fallible<()> {
        let a = TokenSecret::generate()?;
        let b = TokenSecret::generate()?;
        assert_ne!(a.to_string(), b.to_string());
        Ok(())
    }
}
