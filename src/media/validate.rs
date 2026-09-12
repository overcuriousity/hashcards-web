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

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

use percent_encoding::percent_decode_str;
use pulldown_cmark::Event;
use pulldown_cmark::Parser;
use pulldown_cmark::Tag;

use crate::error::ErrorReport;
use crate::error::Fallible;
use crate::media::resolve::MediaResolver;
use crate::media::resolve::MediaResolverBuilder;
use crate::types::card::Card;
use crate::types::card::CardContent;

/// Represents a missing media file reference.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MissingMedia {
    pub file_path: String,
    pub card_file: PathBuf,
    pub card_lines: (usize, usize),
}

/// Extract all media file paths from markdown text.
fn extract_media_paths(markdown: &str) -> Vec<String> {
    let parser = Parser::new(markdown);
    let mut paths = Vec::new();

    for event in parser {
        if let Event::Start(Tag::Image { dest_url, .. }) = event {
            paths.push(dest_url.to_string());
        }
    }

    paths
}

/// The Markdown texts of a card, each of which may reference media.
fn markdown_texts(card: &Card) -> Vec<&str> {
    match card.content() {
        CardContent::Basic { question, answer } => vec![question.as_str(), answer.as_str()],
        CardContent::Cloze { text, .. } => vec![text.as_str()],
    }
}

/// A resolver for the media paths in `card`, a card of the collection at
/// `base_dir`.
fn resolver_for(card: &Card, base_dir: &Path) -> Fallible<MediaResolver> {
    MediaResolverBuilder::new()
        .with_collection_path(base_dir.to_path_buf())?
        .with_deck_path(card.relative_file_path(base_dir)?)?
        .build()
}

/// Every media file `cards` reference, relative to `base_dir`.
///
/// References that do not resolve are left out, external URLs among them:
/// reporting a missing file is `validate_media_files`' job.
pub fn referenced_media_files(cards: &[Card], base_dir: &Path) -> Fallible<BTreeSet<PathBuf>> {
    let mut found = BTreeSet::new();
    for card in cards {
        let resolver = resolver_for(card, base_dir)?;
        for markdown in markdown_texts(card) {
            for path in extract_media_paths(markdown) {
                if let Ok(rel) = resolver.resolve(&path) {
                    found.insert(rel);
                }
            }
        }
    }
    Ok(found)
}

/// Validate that all media files referenced in cards exist.
pub fn validate_media_files(cards: &[Card], base_dir: &Path) -> Fallible<()> {
    let base_dir = base_dir.to_path_buf();
    let mut missing = HashSet::new();

    for card in cards {
        let resolver: MediaResolver = resolver_for(card, &base_dir)?;
        for markdown in markdown_texts(card) {
            for path in extract_media_paths(markdown) {
                use crate::media::resolve::ResolveError;
                match resolver.resolve(&path) {
                    Ok(_) => {}
                    // External URLs are intentional;
                    // the browser fetches them directly, so they are never "missing".
                    Err(ResolveError::ExternalUrl) => {}
                    Err(_) => {
                        // Decode percent-encoded characters for better error display.
                        let decoded_path: Cow<str> = percent_decode_str(&path).decode_utf8_lossy();
                        missing.insert(MissingMedia {
                            file_path: decoded_path.into_owned(),
                            card_file: card.file_path().clone(),
                            card_lines: card.range(),
                        });
                    }
                }
            }
        }
    }

    if !missing.is_empty() {
        // Sort missing files for consistent error messages.
        let mut missing: Vec<MissingMedia> = missing.into_iter().collect();
        missing.sort();

        // Build error message.
        let mut msg = String::from("Missing media files referenced in cards:\n");
        for m in missing {
            msg.push_str(&format!(
                "  - {} (referenced in {}:{})\n",
                m.file_path,
                m.card_file.display(),
                m.card_lines.0
            ));
        }

        return Err(ErrorReport::new(&msg));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::env::temp_dir;
    use std::fs::create_dir_all;

    use super::*;
    use crate::parser::Parser as CardParser;

    #[test]
    fn test_extract_media_paths() {
        let markdown = "Here is an image: ![alt](@/foo.jpg)\nAnd another: ![](@/bar.png)";
        let paths = extract_media_paths(markdown);
        assert_eq!(paths, vec!["@/foo.jpg", "@/bar.png"]);
    }

    #[test]
    fn test_extract_media_paths_with_audio() {
        let markdown = "Audio file: ![](@/sound.mp3)";
        let paths = extract_media_paths(markdown);
        assert_eq!(paths, vec!["@/sound.mp3"]);
    }

    #[test]
    fn test_extract_media_paths_no_media() {
        let markdown = "Just some **bold** text.";
        let paths = extract_media_paths(markdown);
        assert!(paths.is_empty());
    }

    #[test]
    fn test_extract_media_paths_with_urls() {
        let markdown = "![](https://example.com/image.jpg) and ![](local.png)";
        let paths = extract_media_paths(markdown);
        assert_eq!(paths, vec!["https://example.com/image.jpg", "local.png"]);
    }

    #[test]
    fn test_validate_media_files_with_missing_files() -> Fallible<()> {
        // Create a temporary directory for the test
        let test_dir = temp_dir().join("hashcards_media_test");
        create_dir_all(&test_dir)?;

        // Create a markdown file path (doesn't need to exist for this test)
        let card_file = test_dir.join("test_deck.md");
        std::fs::write(&card_file, b"fake deck data")?;

        // Parse cards from markdown with missing media references
        let markdown = "Q: What is this image?\n\n![](missing_image.jpg)\n\nA: Unknown\n\nQ: What is this audio?\nA: ![](missing_audio.mp3)";
        let parser = CardParser::new("test_deck".to_string(), card_file.clone(), 0);
        let cards = parser.parse(markdown)?;

        // Validate media files - should return an error
        let result = validate_media_files(&cards, &test_dir);

        // Assert that validation failed
        assert!(result.is_err());

        // Assert the error message contains expected information
        let err = result.err().unwrap();
        let err_msg = err.to_string();

        assert!(err_msg.contains("Missing media files referenced in cards:"));
        assert!(err_msg.contains("missing_image.jpg"));
        assert!(err_msg.contains("missing_audio.mp3"));
        assert!(err_msg.contains("test_deck.md"));

        Ok(())
    }

    #[test]
    fn test_validate_media_files_with_existing_files() -> Fallible<()> {
        // Create a temporary directory for the test.
        let test_dir = temp_dir().join("hashcards_media_test_existing");
        create_dir_all(&test_dir)?;

        // Create actual media files.
        let image_path = test_dir.join("existing_image.jpg");
        std::fs::write(&image_path, b"fake image data")?;

        // Create a markdown file path.
        let card_file = test_dir.join("test_deck.md");
        std::fs::write(&card_file, b"fake deck data")?;

        // Parse cards from markdown with existing media reference.
        let markdown = "Q: What is this image?\n\n![](existing_image.jpg)\n\nA: A test image";
        let parser = CardParser::new("test_deck".to_string(), card_file.clone(), 0);
        let cards = parser.parse(markdown)?;

        // Validate media files - should succeed.
        let result = validate_media_files(&cards, &test_dir);

        // Assert that validation succeeded.
        assert_eq!(result, Ok(()));

        Ok(())
    }

    #[test]
    fn test_validate_media_files_with_external_urls() -> Fallible<()> {
        // External image URLs must not be treated
        // as missing local files — the browser fetches them directly.
        let test_dir = crate::helper::create_tmp_directory()?;

        let card_file = test_dir.join("test_deck.md");
        std::fs::write(&card_file, b"")?;

        let markdown = "Q: What is shown?\nA: ![](https://example.com/image.png)";
        let parser = CardParser::new("test_deck".to_string(), card_file.clone(), 0);
        let cards = parser.parse(markdown)?;

        let result = validate_media_files(&cards, &test_dir);
        assert!(
            result.is_ok(),
            "external URLs should not fail validation: {result:?}"
        );

        Ok(())
    }

    #[test]
    fn test_validate_media_files_with_cloze_cards() -> Fallible<()> {
        // Create a temporary directory for the test.
        let test_dir = temp_dir().join("hashcards_media_test_cloze");
        create_dir_all(&test_dir)?;

        // Create a markdown file path.
        let card_file = test_dir.join("test_deck.md");
        std::fs::write(&card_file, "")?;

        // Parse cloze card with missing media reference.
        let markdown = "C: The capital of [France] is ![](@/paris.jpg)";
        let parser = CardParser::new("test_deck".to_string(), card_file.clone(), 0);
        let cards: Vec<Card> = parser.parse(markdown)?;

        // Validate media files - should fail.
        let result = validate_media_files(&cards, &test_dir);

        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(err_msg.contains("paris.jpg"));

        Ok(())
    }
}
