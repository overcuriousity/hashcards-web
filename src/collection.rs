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

#[cfg(test)]
use std::env::current_dir;
use std::fs::read_to_string;
use std::path::PathBuf;
use std::time::Instant;

use crate::db::Database;
use crate::error::Fallible;
use crate::error::fail;
use crate::media::validate::validate_media_files;
use crate::parser::DuplicateCard;
use crate::parser::parse_deck;
use crate::types::card::Card;
#[cfg(test)]
use crate::types::collection_id::CollectionId;
#[cfg(test)]
use crate::user_db::UserDatabase;

pub struct Collection {
    pub directory: PathBuf,
    pub db: Database,
    pub cards: Vec<Card>,
    pub macros: Vec<(String, String)>,
    /// Byte-identical cards that were deduplicated at load time.
    pub duplicates: Vec<DuplicateCard>,
}

impl Collection {
    /// Load a collection from `directory`, using the `hashcards.db` inside
    /// it. Test-only: the server resolves each collection's database from
    /// the config and uses `open`.
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

    /// Load a collection from `directory`, reading its schedule out of `db`.
    ///
    /// The database is passed in rather than opened here: a session drawing
    /// on several collections shares one connection across all of them, and
    /// only the caller knows which.
    pub fn open(directory: PathBuf, db: Database) -> Fallible<Self> {
        let directory: PathBuf = if directory.exists() {
            directory.canonicalize()?
        } else {
            return fail("directory does not exist.");
        };

        let macros: Vec<(String, String)> = {
            let macros_path = directory.join("macros.tex");
            if macros_path.exists() {
                let content = read_to_string(&macros_path)?;
                let (parsed, malformed) = parse_macros(&content);
                for line_no in malformed {
                    log::warn!(
                        "{}: line {line_no} is not a valid macro definition (expected a name and a definition separated by a space); line ignored.",
                        macros_path.display()
                    );
                }
                parsed
            } else {
                Vec::new()
            }
        };

        let parsed = {
            log::debug!("Loading deck...");
            let start = Instant::now();
            let parsed = parse_deck(&directory)?;
            let end = Instant::now();
            let duration = end.duration_since(start).as_millis();
            log::debug!("Deck loaded in {duration}ms.");
            parsed
        };
        let cards: Vec<Card> = parsed.cards;
        let duplicates: Vec<DuplicateCard> = parsed.duplicates;

        // Validate media files
        validate_media_files(&cards, &directory)?;

        Ok(Self {
            directory,
            db,
            cards,
            macros,
            duplicates,
        })
    }
}

/// Parse the contents of `macros.tex` into `(name, definition)` pairs.
///
/// Returns the parsed macros and the 1-based line numbers of malformed
/// lines: non-comment, non-blank lines without a space separating the
/// macro name from its definition. Comment lines start with `%`.
fn parse_macros(content: &str) -> (Vec<(String, String)>, Vec<usize>) {
    let mut macros = Vec::new();
    let mut malformed = Vec::new();
    for (index, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('%') {
            continue;
        }
        match line.split_once(' ') {
            Some((name, definition)) => {
                macros.push((name.to_string(), definition.to_string()));
            }
            None => malformed.push(index + 1),
        }
    }
    (macros, malformed)
}

#[cfg(test)]
mod tests {
    use std::fs::write;

    use super::*;
    use crate::error::Fallible;
    use crate::helper::create_tmp_directory;

    #[test]
    fn test_parse_macros_reports_malformed_lines() {
        let content = "\\foo bar\n% comment\n\nnospace\n\\baz qux quux\n";
        let (macros, malformed) = parse_macros(content);
        assert_eq!(
            macros,
            vec![
                ("\\foo".to_string(), "bar".to_string()),
                ("\\baz".to_string(), "qux quux".to_string()),
            ]
        );
        // Line 4 ("nospace") has no space separator; comment and blank
        // lines are not malformed.
        assert_eq!(malformed, vec![4]);
    }

    /// Regression test for BUG-12: two byte-identical cards must not abort
    /// collection loading (and therefore drill startup); one copy is kept
    /// and the duplicate is reported with both file locations.
    #[test]
    fn test_duplicate_cards_across_files_load_with_warning() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        write(dir.join("a.md"), "Q: same question\nA: same answer\n")?;
        write(dir.join("b.md"), "Q: same question\nA: same answer\n")?;

        let collection = Collection::new(Some(dir.display().to_string()))?;

        // Exactly one copy survives.
        assert_eq!(collection.cards.len(), 1);

        // The duplicate is reported, naming both files.
        assert_eq!(collection.duplicates.len(), 1);
        let dup = &collection.duplicates[0];
        let locations = format!("{} {}", dup.kept(), dup.ignored());
        assert!(locations.contains("a.md:1"), "locations were: {locations}");
        assert!(locations.contains("b.md:1"), "locations were: {locations}");
        Ok(())
    }

    #[test]
    fn test_collection_without_duplicates_reports_none() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        write(dir.join("a.md"), "Q: question one\nA: answer one\n")?;
        write(dir.join("b.md"), "Q: question two\nA: answer two\n")?;
        let collection = Collection::new(Some(dir.display().to_string()))?;
        assert_eq!(collection.cards.len(), 2);
        assert!(collection.duplicates.is_empty());
        Ok(())
    }

    /// Reloading a collection is stable: card hashes are content-derived, so
    /// a second load sees exactly the rows the first one wrote.
    #[test]
    fn test_reloading_a_collection_keeps_card_hashes() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        write(dir.join("Deck.md"), "C: Water is [H2O].\n")?;
        let first = Collection::new(Some(dir.display().to_string()))?;
        let hashes_before = first.db.card_hashes()?;
        drop(first);
        let second = Collection::new(Some(dir.display().to_string()))?;
        assert_eq!(second.db.card_hashes()?, hashes_before);
        Ok(())
    }
}
