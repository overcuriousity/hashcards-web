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

    /// The id as it is written in the file. Production code reaches the
    /// value through `Display` or `ToSql` instead, so this is the accessor
    /// tests use to look at the string itself.
    #[allow(dead_code)]
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
