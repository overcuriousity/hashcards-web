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
use std::error::Error;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fs::read_to_string;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use walkdir::WalkDir;

use crate::error::ErrorReport;
use crate::error::Fallible;
use crate::types::aliases::DeckName;
use crate::types::card::Card;
use crate::types::card::CardContent;
use crate::types::card_hash::CardHash;

/// Metadata that can be specified at the top of a deck file.
#[derive(Debug, Deserialize)]
struct DeckMetadata {
    name: Option<String>,
}

/// The 0-based line at which a deck file's cards begin.
///
/// Zero unless the file opens with TOML frontmatter, in which case it is the
/// line after its closing `---`. Anything that edits a deck file by line
/// needs this: the frontmatter's delimiters are byte-for-byte what a card
/// separator is, so a rewrite that does not know where the body starts will
/// happily take the closing one away and leave a file that no longer parses.
///
/// A file whose frontmatter is never closed has no body to speak of, and
/// whoever is about to write it will hear so from `extract_frontmatter`.
pub fn body_start_line(text: &str) -> usize {
    let mut lines = text.lines();
    match lines.next() {
        Some(first) if first.trim() == "---" => {}
        _ => return 0,
    }
    for (offset, line) in lines.enumerate() {
        if line.trim() == "---" {
            // `offset` counts from the line after the opening delimiter, so
            // the closing one is file line `offset + 1`.
            return offset + 2;
        }
    }
    0
}

/// Extract TOML frontmatter from markdown text.
/// Returns (frontmatter_metadata, content_without_frontmatter, content_start_line)
/// where `content_start_line` is the 0-based file line at which the content
/// begins (0 when there is no frontmatter).
///
/// This function returns a slice of the original text to avoid
/// collecting lines, joining them, and then re-splitting in parse().
fn extract_frontmatter(text: &str) -> Fallible<(DeckMetadata, &str, usize)> {
    let mut lines = text.lines().enumerate().peekable();

    // Check if the file starts with frontmatter delimiter
    match lines.peek() {
        Some((_, line)) if line.trim() == "---" => {}
        _ => return Ok((DeckMetadata { name: None }, text, 0)),
    };
    lines.next(); // consume the opening delimiter

    // Collect frontmatter lines and find closing delimiter
    let mut frontmatter_lines = Vec::new();
    let mut closing_line_idx = None;

    for (idx, line) in lines {
        if line.trim() == "---" {
            closing_line_idx = Some(idx);
            break;
        }
        frontmatter_lines.push(line);
    }

    let closing_line_idx = closing_line_idx
        .ok_or_else(|| ErrorReport::new("Frontmatter opening '---' found but no closing '---'"))?;

    // Parse TOML from frontmatter
    let frontmatter_str = frontmatter_lines.join("\n");
    let metadata: DeckMetadata = toml::from_str(&frontmatter_str)
        .map_err(|e| ErrorReport::new(format!("Failed to parse TOML frontmatter: {}", e)))?;

    // Find byte offset where content starts (line after closing delimiter)
    // We do this by finding the position of the closing delimiter line in the original text
    let content_start_line = closing_line_idx + 1;
    let mut current_line = 0;
    let mut byte_pos = None;

    for (pos, ch) in text.char_indices() {
        if ch == '\n' {
            current_line += 1;
            if current_line == content_start_line {
                byte_pos = Some(pos + 1); // Start after the newline
                break;
            }
        }
    }

    // If byte_pos was never set, content starts at end of text (empty content)
    let content = match byte_pos {
        Some(pos) if pos < text.len() => &text[pos..],
        _ => "",
    };

    Ok((metadata, content, content_start_line))
}

/// Strip TOML frontmatter and return only the card content portion of a file.
///
/// Kept as a convenience wrapper around `strip_frontmatter_with_offset` for
/// callers that don't need the line offset.
#[allow(dead_code)]
pub fn strip_frontmatter(text: &str) -> Fallible<&str> {
    let (content, _) = strip_frontmatter_with_offset(text)?;
    Ok(content)
}

/// Like `strip_frontmatter`, but also return the 0-based file line at which
/// the content starts (0 when there is no frontmatter). Pass this as the
/// `line_offset` of `Parser::new` so parse errors and card ranges report
/// real file lines.
pub fn strip_frontmatter_with_offset(text: &str) -> Fallible<(&str, usize)> {
    let (_, content, offset) = extract_frontmatter(text)?;
    Ok((content, offset))
}

/// The result of parsing every deck file in a collection directory.
pub struct ParsedDeck {
    pub cards: Vec<Card>,
    pub duplicates: Vec<DuplicateCard>,
}

/// Parses all Markdown files in the given directory.
///
/// Byte-identical cards (same hash) are deduplicated: the first copy
/// encountered is kept, and every dropped copy is reported in
/// `ParsedDeck::duplicates` with both locations.
pub fn parse_deck(directory: &PathBuf) -> Fallible<ParsedDeck> {
    let mut all_cards = Vec::new();
    let mut duplicates = Vec::new();
    for entry in WalkDir::new(directory) {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && path.extension().is_some_and(|ext| ext == "md") {
            let text = read_to_string(path)
                .map_err(|e| ErrorReport::new(format!("Failed to read {}: {e}", path.display())))?;

            // Extract frontmatter and get custom deck name if specified
            let (metadata, content, line_offset) = extract_frontmatter(&text).map_err(|e| {
                ErrorReport::new(format!("{} File: {}", e.message(), path.display()))
            })?;

            let deck_name: DeckName = metadata.name.unwrap_or_else(|| {
                path.strip_prefix(directory)
                    .ok()
                    .map(|rel| {
                        rel.with_extension("")
                            .components()
                            .map(|c| c.as_os_str().to_string_lossy().into_owned())
                            .collect::<Vec<_>>()
                            .join("/")
                    })
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| {
                        path.file_stem()
                            .map(|os_str| os_str.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "None".to_string())
                    })
            });

            let parser = Parser::new(deck_name, path.to_path_buf(), line_offset);
            let parsed = parser.parse_with_duplicates(content)?;
            all_cards.extend(parsed.cards);
            duplicates.extend(parsed.duplicates);
        }
    }

    // Cards are sorted by their hash to make subsequent code more
    // deterministic. The sort is stable, so among byte-identical cards the
    // first one encountered on disk stays first and is the copy we keep.
    all_cards.sort_by_key(|c| c.hash());

    // Remove cross-file duplicates, recording both locations.
    let mut cards: Vec<Card> = Vec::new();
    for card in all_cards {
        match cards.last() {
            Some(kept) if kept.hash() == card.hash() => {
                duplicates.push(DuplicateCard::new(
                    card.hash(),
                    CardLocation::of(kept),
                    CardLocation::of(&card),
                ));
            }
            _ => cards.push(card),
        }
    }

    Ok(ParsedDeck { cards, duplicates })
}

pub struct Parser {
    deck_name: DeckName,
    file_path: PathBuf,
    /// The 0-based file line at which the parsed text begins. Non-zero when
    /// TOML frontmatter was stripped before parsing, so that all error
    /// locations and card ranges refer to real file lines.
    line_offset: usize,
}

#[derive(Debug)]
pub struct ParserError {
    pub message: String,
    pub file_path: PathBuf,
    pub line_num: usize,
}

impl ParserError {
    fn new(message: impl Into<String>, file_path: PathBuf, line_num: usize) -> Self {
        ParserError {
            message: message.into(),
            file_path,
            line_num,
        }
    }
}

impl Display for ParserError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} Location: {}:{}",
            self.message,
            self.file_path.display(),
            self.line_num + 1
        )
    }
}

impl Error for ParserError {}

/// The location of a card in the collection: file path and 1-based line
/// number of the card's first line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardLocation {
    file_path: PathBuf,
    line: usize,
}

impl CardLocation {
    pub fn of(card: &Card) -> Self {
        Self {
            file_path: card.file_path().clone(),
            line: card.range().0 + 1,
        }
    }

    /// The location as shown to a reader of the collection, with `base`
    /// stripped from the file path.
    ///
    /// `Display` prints the path the parser walked, which is absolute and
    /// rooted at the server's `data_dir`. That belongs in logs, not on a
    /// page: under `[oidc]` the reader has no filesystem access, and the
    /// server's directory layout is none of their business.
    pub fn display_under(&self, base: &Path) -> String {
        let path = self.file_path.strip_prefix(base).unwrap_or(&self.file_path);
        format!("{}:{}", path.display(), self.line)
    }
}

impl Display for CardLocation {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.file_path.display(), self.line)
    }
}

/// Two byte-identical cards found while loading a collection. The `kept`
/// copy stays in the deck; the `ignored` copy is dropped.
#[derive(Debug, Clone)]
pub struct DuplicateCard {
    hash: CardHash,
    kept: CardLocation,
    ignored: CardLocation,
}

impl DuplicateCard {
    pub fn new(hash: CardHash, kept: CardLocation, ignored: CardLocation) -> Self {
        Self {
            hash,
            kept,
            ignored,
        }
    }

    pub fn kept(&self) -> &CardLocation {
        &self.kept
    }

    pub fn ignored(&self) -> &CardLocation {
        &self.ignored
    }

    /// The report as shown to a reader of the collection, with `base`
    /// stripped from both file paths. See `CardLocation::display_under`.
    pub fn display_under(&self, base: &Path) -> String {
        format!(
            "duplicate card {}: kept {}, ignored {}",
            self.hash,
            self.kept.display_under(base),
            self.ignored.display_under(base)
        )
    }
}

impl Display for DuplicateCard {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "duplicate card {}: kept {}, ignored {}",
            self.hash,
            self.kept(),
            self.ignored()
        )
    }
}

/// The result of parsing a single deck file.
pub struct ParsedFile {
    pub cards: Vec<Card>,
    pub duplicates: Vec<DuplicateCard>,
}

enum State {
    /// Start state.
    Start,
    /// Reading a question (Q:)
    ReadingQuestion { question: String, start_line: usize },
    /// Reading an answer (A:)
    ReadingAnswer {
        question: String,
        answer: String,
        start_line: usize,
    },
    /// Reading a cloze card (C:)
    ReadingCloze { text: String, start_line: usize },
    /// Reading a term (T:), waiting for its definition.
    ReadingTerm { term: String, start_line: usize },
    /// Reading a definition (D:) for a term.
    ReadingDefinition {
        term: String,
        definition: String,
        start_line: usize,
    },
    /// End state.
    End,
}

enum Line {
    /// A line like `Q: <text>`.
    StartQuestion(String),
    /// A line like `A: <text>`.
    StartAnswer(String),
    /// A line like `C: <text>`.
    StartCloze(String),
    /// A line like `T: <text>` (term-definition shorthand).
    StartTerm(String),
    /// A line like `D: <text>` (term-definition shorthand).
    StartDefinition(String),
    /// A line that's just `---` (flashcard separator).
    Separator,
    /// Any other line.
    Text(String),
    /// End of file
    Eof,
}

/// The kind of fenced code block currently open.
#[derive(PartialEq)]
enum FenceKind {
    /// ``` fences.
    Backtick,
    /// ~~~ fences.
    Tilde,
}

/// Classifies lines, tracking fenced-code-block state: while inside a
/// ``` or ~~~ fence, every line (including the closing fence) is `Text`,
/// so card syntax inside code blocks is never parsed.
struct LineReader {
    fence: Option<FenceKind>,
}

impl LineReader {
    fn new() -> Self {
        LineReader { fence: None }
    }

    /// `definition_expected` says whether the state machine is currently
    /// reading a term, which is the only place a `D:` line is a tag. An
    /// answer that merely begins with `D:` is ordinary text (BUG: it used to
    /// fail the whole collection load).
    fn read(&mut self, line: &str, definition_expected: bool) -> Line {
        let trimmed = line.trim_start();
        match self.fence {
            Some(FenceKind::Backtick) => {
                if trimmed.starts_with("```") {
                    self.fence = None;
                }
                return Line::Text(line.to_string());
            }
            Some(FenceKind::Tilde) => {
                if trimmed.starts_with("~~~") {
                    self.fence = None;
                }
                return Line::Text(line.to_string());
            }
            None => {}
        }
        if opens_backtick_fence(trimmed) {
            self.fence = Some(FenceKind::Backtick);
            return Line::Text(line.to_string());
        }
        if trimmed.starts_with("~~~") {
            self.fence = Some(FenceKind::Tilde);
            return Line::Text(line.to_string());
        }
        if is_question(line) {
            Line::StartQuestion(trim(line))
        } else if is_answer(line) {
            Line::StartAnswer(trim(line))
        } else if is_cloze(line) {
            Line::StartCloze(trim(line))
        } else if is_term(line) {
            Line::StartTerm(trim(line))
        } else if definition_expected && is_definition(line) {
            Line::StartDefinition(trim(line))
        } else if is_separator(line) {
            Line::Separator
        } else {
            Line::Text(line.to_string())
        }
    }
}

/// Does this line open a backtick code fence?
///
/// A fence opener is a run of at least three backticks whose info string
/// contains no backtick (CommonMark). Without that second condition a line
/// like ```` ```code``` ```` -- an inline code span -- opens a fence that is
/// never closed, silently swallowing every card after it in the file.
fn opens_backtick_fence(trimmed: &str) -> bool {
    let run = trimmed.chars().take_while(|c| *c == '`').count();
    run >= 3 && !trimmed[run..].contains('`')
}

/// Is the parser in the one state where a `D:` line is a definition tag?
fn definition_expected(state: &State) -> bool {
    matches!(state, State::ReadingTerm { .. })
}

fn is_question(line: &str) -> bool {
    line.starts_with("Q:")
}

fn is_answer(line: &str) -> bool {
    line.starts_with("A:")
}

fn is_cloze(line: &str) -> bool {
    line.starts_with("C:")
}

fn is_term(line: &str) -> bool {
    line.starts_with("T:")
}

fn is_definition(line: &str) -> bool {
    line.starts_with("D:")
}

fn is_separator(line: &str) -> bool {
    line.trim() == "---"
}

fn trim(line: &str) -> String {
    line[2..].trim().to_string()
}

/// Join a term-definition card's prompt to its body.
///
/// A one-line body reads well on the prompt's own line, and staying there
/// keeps the hash identical to the hand-written `Q: Define: ...` form. A
/// body spanning several lines cannot: markdown folds its first line into
/// the prompt and renders the rest as a sibling block, so the card reads as
/// a copy of its twin with the answer stranded underneath. Give the prompt
/// its own line and the body stays a block — which is also what `Q:`
/// followed by those lines parses to, so the two forms still share a hash.
fn prompt(label: &str, body: &str) -> String {
    if body.contains('\n') {
        format!("{label}\n\n{body}")
    } else {
        format!("{label} {body}")
    }
}

/// Returns true if the unescaped `[` at byte position `open_pos` in `text`
/// opens a markdown link: its bracket group closes with `](`. Nested `[`
/// before the close means this is not a simple link.
fn is_markdown_link_open(text: &str, open_pos: usize) -> bool {
    let bytes = text.as_bytes();
    let mut i = open_pos + 1;
    // A CommonMark link label may contain balanced nested brackets, so track
    // depth rather than giving up on the first inner `[`.
    let mut depth: usize = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'[' => {
                depth += 1;
                i += 1;
            }
            b']' => {
                if depth == 0 {
                    return bytes.get(i + 1) == Some(&b'(');
                }
                depth -= 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    false
}

impl Parser {
    pub fn new(deck_name: DeckName, file_path: PathBuf, line_offset: usize) -> Self {
        Parser {
            deck_name,
            file_path,
            line_offset,
        }
    }

    /// Parse all the cards in the given text, silently dropping duplicates.
    ///
    /// This is the historical API; serve-mode edit relies on its exact
    /// dedup behavior. Prefer `parse_with_duplicates` where duplicate
    /// reporting matters.
    pub fn parse(&self, text: &str) -> Result<Vec<Card>, ParserError> {
        Ok(self.parse_with_duplicates(text)?.cards)
    }

    /// Parse all the cards in the given text, reporting duplicates.
    ///
    /// Byte-identical cards share a hash; the first occurrence is kept and
    /// each further occurrence is recorded as a `DuplicateCard`.
    pub fn parse_with_duplicates(&self, text: &str) -> Result<ParsedFile, ParserError> {
        let mut cards = Vec::new();
        let mut state = State::Start;
        let mut reader = LineReader::new();
        let lines: Vec<&str> = text.lines().collect();
        let last_line = if lines.is_empty() { 0 } else { lines.len() - 1 };
        for (line_num, line) in lines.iter().enumerate() {
            let line = reader.read(line, definition_expected(&state));
            state = self.parse_line(state, line, line_num + self.line_offset, &mut cards)?;
        }
        self.parse_line(state, Line::Eof, last_line + self.line_offset, &mut cards)?;

        let mut index_of: HashMap<CardHash, usize> = HashMap::new();
        let mut unique_cards: Vec<Card> = Vec::new();
        let mut duplicates: Vec<DuplicateCard> = Vec::new();
        for card in cards {
            match index_of.get(&card.hash()) {
                Some(&kept_index) => {
                    // `kept_index` always points into `unique_cards`.
                    duplicates.push(DuplicateCard::new(
                        card.hash(),
                        CardLocation::of(&unique_cards[kept_index]),
                        CardLocation::of(&card),
                    ));
                }
                None => {
                    index_of.insert(card.hash(), unique_cards.len());
                    unique_cards.push(card);
                }
            }
        }
        Ok(ParsedFile {
            cards: unique_cards,
            duplicates,
        })
    }

    fn parse_line(
        &self,
        state: State,
        line: Line,
        line_num: usize,
        cards: &mut Vec<Card>,
    ) -> Result<State, ParserError> {
        match state {
            State::Start => match line {
                Line::StartQuestion(text) => Ok(State::ReadingQuestion {
                    question: text,
                    start_line: line_num,
                }),
                Line::StartAnswer(_) => Err(ParserError::new(
                    "Found answer tag without a question.",
                    self.file_path.clone(),
                    line_num,
                )),
                Line::StartCloze(text) => Ok(State::ReadingCloze {
                    text,
                    start_line: line_num,
                }),
                Line::StartTerm(text) => Ok(State::ReadingTerm {
                    term: text,
                    start_line: line_num,
                }),
                Line::StartDefinition(_) => Err(ParserError::new(
                    "Found definition tag without a term.",
                    self.file_path.clone(),
                    line_num,
                )),
                Line::Separator => Ok(State::Start),
                Line::Text(_) => Ok(State::Start),
                Line::Eof => Ok(State::End),
            },
            State::ReadingQuestion {
                question,
                start_line,
            } => match line {
                Line::StartQuestion(_) => Err(ParserError::new(
                    "New question without answer.",
                    self.file_path.clone(),
                    line_num,
                )),
                Line::StartAnswer(text) => Ok(State::ReadingAnswer {
                    question,
                    answer: text,
                    start_line,
                }),
                Line::StartCloze(_) => Err(ParserError::new(
                    "Found cloze tag while reading a question.",
                    self.file_path.clone(),
                    line_num,
                )),
                Line::StartTerm(_) => Err(ParserError::new(
                    "Found term tag while reading a question.",
                    self.file_path.clone(),
                    line_num,
                )),
                Line::StartDefinition(_) => Err(ParserError::new(
                    "Found definition tag while reading a question.",
                    self.file_path.clone(),
                    line_num,
                )),
                Line::Separator => Err(ParserError::new(
                    "Found flashcard separator while reading a question.",
                    self.file_path.clone(),
                    line_num,
                )),
                Line::Text(text) => Ok(State::ReadingQuestion {
                    question: format!("{question}\n{text}"),
                    start_line,
                }),
                Line::Eof => Err(ParserError::new(
                    "File ended while reading a question without an answer.",
                    self.file_path.clone(),
                    line_num,
                )),
            },
            State::ReadingAnswer {
                question,
                answer,
                start_line,
            } => {
                match line {
                    Line::StartQuestion(text) => {
                        // Finalize the previous card.
                        let card = Card::new(
                            self.deck_name.clone(),
                            self.file_path.clone(),
                            (start_line, line_num),
                            CardContent::new_basic(question, answer),
                        );
                        cards.push(card);
                        // Start a new question.
                        Ok(State::ReadingQuestion {
                            question: text,
                            start_line: line_num,
                        })
                    }
                    Line::StartAnswer(_) => Err(ParserError::new(
                        "Found answer tag while reading an answer.",
                        self.file_path.clone(),
                        line_num,
                    )),
                    Line::StartCloze(text) => {
                        // Finalize the previous card.
                        let card = Card::new(
                            self.deck_name.clone(),
                            self.file_path.clone(),
                            (start_line, line_num),
                            CardContent::new_basic(question, answer),
                        );
                        cards.push(card);
                        // Start reading a new cloze card.
                        Ok(State::ReadingCloze {
                            text,
                            start_line: line_num,
                        })
                    }
                    Line::StartTerm(text) => {
                        // Finalize the previous card.
                        let card = Card::new(
                            self.deck_name.clone(),
                            self.file_path.clone(),
                            (start_line, line_num),
                            CardContent::new_basic(question, answer),
                        );
                        cards.push(card);
                        // Start reading a term.
                        Ok(State::ReadingTerm {
                            term: text,
                            start_line: line_num,
                        })
                    }
                    Line::StartDefinition(_) => Err(ParserError::new(
                        "Found definition tag without a term.",
                        self.file_path.clone(),
                        line_num,
                    )),
                    Line::Separator => {
                        // Finalize the current card.
                        let card = Card::new(
                            self.deck_name.clone(),
                            self.file_path.clone(),
                            (start_line, line_num),
                            CardContent::new_basic(question, answer),
                        );
                        cards.push(card);
                        // Return to start state.
                        Ok(State::Start)
                    }
                    Line::Text(text) => Ok(State::ReadingAnswer {
                        question,
                        answer: format!("{answer}\n{text}"),
                        start_line,
                    }),
                    Line::Eof => {
                        // Finalize the current card.
                        let card = Card::new(
                            self.deck_name.clone(),
                            self.file_path.clone(),
                            (start_line, line_num),
                            CardContent::new_basic(question, answer),
                        );
                        cards.push(card);
                        Ok(State::End)
                    }
                }
            }
            State::ReadingCloze { text, start_line } => {
                match line {
                    Line::StartQuestion(new_text) => {
                        // Finalize the previous cloze card.
                        cards.extend(self.parse_cloze_cards(text, start_line, line_num)?);
                        // Start a new question card
                        Ok(State::ReadingQuestion {
                            question: new_text,
                            start_line: line_num,
                        })
                    }
                    Line::StartAnswer(_) => Err(ParserError::new(
                        "Found answer tag while reading a cloze card.",
                        self.file_path.clone(),
                        line_num,
                    )),
                    Line::StartCloze(new_text) => {
                        // Finalize the previous card.
                        cards.extend(self.parse_cloze_cards(text, start_line, line_num)?);
                        // Start reading a new cloze card.
                        Ok(State::ReadingCloze {
                            text: new_text,
                            start_line: line_num,
                        })
                    }
                    Line::StartTerm(new_text) => {
                        // Finalize the previous cloze card.
                        cards.extend(self.parse_cloze_cards(text, start_line, line_num)?);
                        // Start reading a term.
                        Ok(State::ReadingTerm {
                            term: new_text,
                            start_line: line_num,
                        })
                    }
                    Line::StartDefinition(_) => Err(ParserError::new(
                        "Found definition tag without a term.",
                        self.file_path.clone(),
                        line_num,
                    )),
                    Line::Separator => {
                        // Finalize the current cloze card.
                        cards.extend(self.parse_cloze_cards(text, start_line, line_num)?);
                        // Return to start state.
                        Ok(State::Start)
                    }
                    Line::Text(new_text) => Ok(State::ReadingCloze {
                        text: format!("{text}\n{new_text}"),
                        start_line,
                    }),
                    Line::Eof => {
                        // Finalize the current cloze card.
                        cards.extend(self.parse_cloze_cards(text, start_line, line_num)?);
                        Ok(State::End)
                    }
                }
            }
            State::ReadingTerm { term, start_line } => match line {
                Line::StartQuestion(_) => Err(ParserError::new(
                    "Found question tag while reading a term without a definition.",
                    self.file_path.clone(),
                    line_num,
                )),
                Line::StartAnswer(_) => Err(ParserError::new(
                    "Found answer tag while reading a term. Terms take a definition (D:), not an answer.",
                    self.file_path.clone(),
                    line_num,
                )),
                Line::StartCloze(_) => Err(ParserError::new(
                    "Found cloze tag while reading a term without a definition.",
                    self.file_path.clone(),
                    line_num,
                )),
                Line::StartTerm(_) => Err(ParserError::new(
                    "New term without a definition.",
                    self.file_path.clone(),
                    line_num,
                )),
                Line::StartDefinition(text) => Ok(State::ReadingDefinition {
                    term,
                    definition: text,
                    start_line,
                }),
                Line::Separator => Err(ParserError::new(
                    "Found flashcard separator while reading a term without a definition.",
                    self.file_path.clone(),
                    line_num,
                )),
                Line::Text(text) => Ok(State::ReadingTerm {
                    term: format!("{term}\n{text}"),
                    start_line,
                }),
                Line::Eof => Err(ParserError::new(
                    "File ended while reading a term without a definition.",
                    self.file_path.clone(),
                    line_num,
                )),
            },
            State::ReadingDefinition {
                term,
                definition,
                start_line,
            } => match line {
                Line::StartQuestion(text) => {
                    self.push_term_cards(term, definition, start_line, line_num, cards);
                    Ok(State::ReadingQuestion {
                        question: text,
                        start_line: line_num,
                    })
                }
                Line::StartAnswer(_) => Err(ParserError::new(
                    "Found answer tag while reading a definition.",
                    self.file_path.clone(),
                    line_num,
                )),
                Line::StartCloze(text) => {
                    self.push_term_cards(term, definition, start_line, line_num, cards);
                    Ok(State::ReadingCloze {
                        text,
                        start_line: line_num,
                    })
                }
                Line::StartTerm(text) => {
                    self.push_term_cards(term, definition, start_line, line_num, cards);
                    Ok(State::ReadingTerm {
                        term: text,
                        start_line: line_num,
                    })
                }
                Line::StartDefinition(_) => Err(ParserError::new(
                    "Found definition tag while already reading a definition.",
                    self.file_path.clone(),
                    line_num,
                )),
                Line::Separator => {
                    self.push_term_cards(term, definition, start_line, line_num, cards);
                    Ok(State::Start)
                }
                Line::Text(text) => Ok(State::ReadingDefinition {
                    term,
                    definition: format!("{definition}\n{text}"),
                    start_line,
                }),
                Line::Eof => {
                    self.push_term_cards(term, definition, start_line, line_num, cards);
                    Ok(State::End)
                }
            },
            State::End => Err(ParserError::new(
                "Internal parser error: a line was parsed after the end of the file.",
                self.file_path.clone(),
                line_num,
            )),
        }
    }

    /// Expand a term-definition pair into its two reciprocal basic cards.
    ///
    /// The generated cards are ordinary basic cards, so their hashes are
    /// identical to hand-written `Q: Define: ...` / `Q: Term for: ...`
    /// equivalents.
    fn push_term_cards(
        &self,
        term: String,
        definition: String,
        start_line: usize,
        end_line: usize,
        cards: &mut Vec<Card>,
    ) {
        let term = term.trim();
        let definition = definition.trim();
        cards.push(Card::new(
            self.deck_name.clone(),
            self.file_path.clone(),
            (start_line, end_line),
            CardContent::new_basic(prompt("Define:", term), definition),
        ));
        cards.push(Card::new(
            self.deck_name.clone(),
            self.file_path.clone(),
            (start_line, end_line),
            CardContent::new_basic(prompt("Term for:", definition), term),
        ));
    }

    fn parse_cloze_cards(
        &self,
        text: String,
        start_line: usize,
        end_line: usize,
    ) -> Result<Vec<Card>, ParserError> {
        let text = text.trim();
        let mut cards = Vec::new();

        // The full text of the card, without cloze deletion brackets.
        let clean_text: String = {
            let mut clean_text: Vec<u8> = Vec::new();
            // Flags to indicate should treat the next `[` or `]` differently.
            // Set when the preceeding byte indicates it should be evaluated as
            // markdown and not part of the cloze and therefore added to clean_text.
            let mut image_mode = false; // ![
            // Nesting depth inside a `[text](url)` label. A label may contain
            // balanced brackets, so a single flag would close at the first
            // inner `]` and drop the label's real closing bracket.
            let mut link_depth: usize = 0;
            let mut escape_mode = false; // \[ and \]
            // We use `bytes` rather than `chars` because the cloze start/end
            // positions are byte positions, not character positions. This
            // keeps things tractable: bytes are well-understood, "characters"
            // are a vague abstract concept.
            for (bytepos, c) in text.bytes().enumerate() {
                if c == b'[' {
                    if image_mode {
                        clean_text.push(c);
                    } else if link_depth > 0 {
                        // A balanced bracket inside a link label.
                        link_depth += 1;
                        clean_text.push(c);
                    } else if escape_mode {
                        escape_mode = false;
                        clean_text.push(c);
                    } else if is_markdown_link_open(text, bytepos) {
                        // This bracket opens a markdown link; keep it verbatim.
                        link_depth = 1;
                        clean_text.push(c);
                    }
                } else if c == b']' {
                    if image_mode {
                        // We are in image mode, so this closing bracket is
                        // part of a Markdown image.
                        image_mode = false;
                        clean_text.push(c);
                    } else if link_depth > 0 {
                        // Closing bracket inside or at the end of a link label.
                        link_depth -= 1;
                        clean_text.push(c);
                    } else if escape_mode {
                        // We are in escape mode, so this closing bracket is
                        // part of the markdown text.
                        escape_mode = false;
                        clean_text.push(c);
                    }
                } else if c == b'!' {
                    if !image_mode {
                        // image_mode must be turned on *only* if the '!' is
                        // immediately before a `[`. Otherwise, exclamation
                        // marks in other positions would trigger it.
                        let nextopt = text.as_bytes().get(bytepos + 1).copied();
                        match nextopt {
                            Some(b'[') => {
                                image_mode = true;
                            }
                            _ => {}
                        }
                    }
                    clean_text.push(c);
                } else if c == b'\\' {
                    if !escape_mode {
                        // escape_mode must be turned on *only* if the '\' is
                        // immediately before a `[` or `]`. Otherwise, backslashes
                        // in other positions would trigger it.
                        let nextopt = text.as_bytes().get(bytepos + 1).copied();
                        match nextopt {
                            Some(b'[') | Some(b']') => {
                                escape_mode = true;
                            }
                            _ => {
                                clean_text.push(c);
                            }
                        }
                    }
                } else {
                    clean_text.push(c);
                }
            }
            match String::from_utf8(clean_text) {
                Ok(s) => s,
                Err(_) => {
                    return Err(ParserError::new(
                        "Cloze card contains invalid UTF-8.",
                        self.file_path.clone(),
                        start_line,
                    ));
                }
            }
        };

        let mut start = None;
        let mut index = 0;
        let mut image_mode = false;
        // See the matching counter in the clean_text pass above.
        let mut link_depth: usize = 0;
        let mut escape_mode = false;
        for (bytepos, c) in text.bytes().enumerate() {
            if c == b'[' {
                if image_mode {
                    index += 1;
                } else if link_depth > 0 {
                    // A balanced bracket inside a link label, not a cloze.
                    link_depth += 1;
                    index += 1;
                } else if escape_mode {
                    index += 1;
                    escape_mode = false;
                } else if is_markdown_link_open(text, bytepos) {
                    // This bracket opens a markdown link; it stays in the text.
                    link_depth = 1;
                    index += 1;
                } else if start.is_some() {
                    return Err(ParserError::new(
                        "Nested cloze brackets.",
                        self.file_path.clone(),
                        start_line,
                    ));
                } else {
                    start = Some(index);
                }
            } else if c == b']' {
                if image_mode {
                    image_mode = false;
                    index += 1;
                } else if link_depth > 0 {
                    link_depth -= 1;
                    index += 1;
                } else if escape_mode {
                    escape_mode = false;
                    index += 1;
                } else if let Some(s) = start {
                    let end = index;
                    if end == s {
                        return Err(ParserError::new(
                            "Cloze deletion is empty.",
                            self.file_path.clone(),
                            start_line,
                        ));
                    }
                    let content = CardContent::new_cloze(clean_text.clone(), s, end - 1);
                    let card = Card::new(
                        self.deck_name.clone(),
                        self.file_path.clone(),
                        (start_line, end_line),
                        content,
                    );
                    cards.push(card);
                    start = None;
                }
            } else if c == b'!' {
                if !image_mode {
                    // image_mode must be turned on *only* if the '!' is
                    // immediately before a `[`. Otherwise, exclamation
                    // marks in other positions would trigger it.
                    let nextopt = text.as_bytes().get(bytepos + 1).copied();
                    match nextopt {
                        Some(b'[') => {
                            image_mode = true;
                        }
                        _ => {}
                    }
                }
                index += 1;
            } else if c == b'\\' {
                if !escape_mode {
                    // escape_mode must be turned on *only* if the '\' is
                    // immediately before a `[` or `]`. Otherwise, backslashes
                    // in other positions would trigger it.
                    let nextopt = text.as_bytes().get(bytepos + 1).copied();
                    match nextopt {
                        Some(b'[') | Some(b']') => {
                            escape_mode = true;
                        }
                        _ => {
                            index += 1;
                        }
                    }
                }
            } else if c == b'\n' {
                if start.is_some() {
                    return Err(ParserError::new(
                        "Unterminated cloze deletion.",
                        self.file_path.clone(),
                        start_line,
                    ));
                }
                index += 1;
            } else {
                index += 1;
            }
        }

        if start.is_some() {
            return Err(ParserError::new(
                "Unterminated cloze deletion.",
                self.file_path.clone(),
                start_line,
            ));
        }

        if cards.is_empty() {
            Err(ParserError::new(
                "Cloze card must contain at least one cloze deletion.",
                self.file_path.clone(),
                start_line,
            ))
        } else {
            Ok(cards)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::env::temp_dir;
    use std::fs::create_dir_all;

    use super::*;

    #[test]
    fn test_empty_string() -> Result<(), ParserError> {
        let input = "";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;
        assert_eq!(cards.len(), 0);
        Ok(())
    }

    #[test]
    fn test_whitespace_string() -> Result<(), ParserError> {
        let input = "\n\n\n";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;
        assert_eq!(cards.len(), 0);
        Ok(())
    }

    #[test]
    fn test_basic_card() -> Result<(), ParserError> {
        let input = "Q: What is Rust?\nA: A systems programming language.";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;

        assert_eq!(cards.len(), 1);
        assert!(matches!(
            &cards[0].content(),
            CardContent::Basic {
                question,
                answer,
            } if question == "What is Rust?" && answer == "A systems programming language."
        ));
        Ok(())
    }

    #[test]
    fn test_multiline_qa() -> Result<(), ParserError> {
        let input = "Q: foo\nbaz\nbaz\nA: FOO\nBAR\nBAZ";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;

        assert_eq!(cards.len(), 1);
        assert!(matches!(
            &cards[0].content(),
            CardContent::Basic {
                question,
                answer,
            } if question == "foo\nbaz\nbaz" && answer == "FOO\nBAR\nBAZ"
        ));
        Ok(())
    }

    #[test]
    fn test_two_questions() -> Result<(), ParserError> {
        let input = "Q: foo\nA: bar\n\nQ: baz\nA: quux\n\n";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;

        assert_eq!(cards.len(), 2);
        assert!(matches!(
            &cards[0].content(),
            CardContent::Basic {
                question,
                answer,
            } if question == "foo" && answer == "bar"
        ));
        assert!(matches!(
            &cards[1].content(),
            CardContent::Basic {
                question,
                answer,
            } if question == "baz" && answer == "quux"
        ));
        Ok(())
    }

    #[test]
    fn test_cloze_followed_by_question() -> Result<(), ParserError> {
        let input = "C: [foo]\nQ: Question\nA: Answer";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;

        assert_eq!(cards.len(), 2);
        assert_cloze(&cards[0..1], "foo", &[(0, 2)]);
        assert!(matches!(
            &cards[1].content(),
            CardContent::Basic {
                question,
                answer,
            } if question == "Question" && answer == "Answer"
        ));
        Ok(())
    }

    #[test]
    fn test_cloze_single() -> Result<(), ParserError> {
        let input = "C: Foo [bar] baz.";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;

        assert_cloze(&cards, "Foo bar baz.", &[(4, 6)]);
        Ok(())
    }

    #[test]
    fn test_cloze_multiple() -> Result<(), ParserError> {
        let input = "C: Foo [bar] baz [quux].";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;

        assert_cloze(&cards, "Foo bar baz quux.", &[(4, 6), (12, 15)]);
        Ok(())
    }

    #[test]
    fn test_cloze_with_image() -> Result<(), ParserError> {
        let input = "C: Foo [bar] ![](image.jpg) [quux].";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;

        assert_cloze(&cards, "Foo bar ![](image.jpg) quux.", &[(4, 6), (23, 26)]);
        Ok(())
    }

    #[test]
    fn test_cloze_with_escaped_square_bracket() -> Result<(), ParserError> {
        let input = "C: Key: [`\\[`]";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;

        assert_cloze(&cards, "Key: `[`", &[(5, 7)]);
        Ok(())
    }

    #[test]
    fn test_cloze_with_multiple_escaped_square_brackets() -> Result<(), ParserError> {
        let input = "C: \\[markdown\\] [`\\[cloze\\]`]";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;

        assert_cloze(&cards, "[markdown] `[cloze]`", &[(11, 19)]);
        Ok(())
    }

    #[test]
    fn test_multi_line_cloze() -> Result<(), ParserError> {
        let input = "C: [foo]\n[bar]\nbaz.";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;

        assert_cloze(&cards, "foo\nbar\nbaz.", &[(0, 2), (4, 6)]);
        Ok(())
    }

    #[test]
    fn test_two_clozes() -> Result<(), ParserError> {
        let input = "C: [foo]\nC: [bar]";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;

        assert_eq!(cards.len(), 2);
        assert_cloze(&cards[0..1], "foo", &[(0, 2)]);
        assert_cloze(&cards[1..2], "bar", &[(0, 2)]);
        Ok(())
    }

    #[test]
    fn test_question_without_answer() -> Result<(), ParserError> {
        let input = "Q: Question without answer";
        let parser = make_test_parser();
        let result = parser.parse(input);

        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn test_answer_without_question() -> Result<(), ParserError> {
        let input = "A: Answer without question";
        let parser = make_test_parser();
        let result = parser.parse(input);

        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn test_question_followed_by_cloze() -> Result<(), ParserError> {
        let input = "Q: Question\nC: Cloze";
        let parser = make_test_parser();
        let result = parser.parse(input);

        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn test_question_followed_by_question() -> Result<(), ParserError> {
        let input = "Q: Question\nQ: Another";
        let parser = make_test_parser();
        let result = parser.parse(input);

        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn test_multiple_answers() -> Result<(), ParserError> {
        let input = "Q: Question\nA: Answer\nA: Another answer";
        let parser = make_test_parser();
        let result = parser.parse(input);

        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn test_cloze_followed_by_answer() -> Result<(), ParserError> {
        let input = "C: Cloze\nA: Answer";
        let parser = make_test_parser();
        let result = parser.parse(input);

        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn test_cloze_without_deletions() -> Result<(), ParserError> {
        let input = "C: Cloze";
        let parser = make_test_parser();
        let result = parser.parse(input);

        assert!(result.is_err());
        Ok(())
    }

    /// BUG-16: an empty cloze deletion must be a parse error, not a usize
    /// underflow (debug: panic; release: usize::MAX positions).
    #[test]
    fn test_empty_cloze_deletion_is_error() {
        let input = "C: [] foo";
        let parser = make_test_parser();
        let result = parser.parse(input);
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert_eq!(
            err.to_string(),
            "Cloze deletion is empty. Location: test.md:1"
        );
    }

    /// BUG-16 companion: a one-byte deletion still parses.
    #[test]
    fn test_single_byte_cloze_deletion_parses() -> Result<(), ParserError> {
        let input = "C: [a]";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;
        assert_cloze(&cards, "a", &[(0, 0)]);
        Ok(())
    }

    /// BUG-19: a `[` while a deletion is already open must be an error, not a
    /// silent restart of the deletion.
    #[test]
    fn test_nested_cloze_brackets_is_error() {
        let input = "C: [[a]]";
        let parser = make_test_parser();
        let result = parser.parse(input);
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert_eq!(
            err.to_string(),
            "Nested cloze brackets. Location: test.md:1"
        );
    }

    /// BUG-19: an unmatched `[` at end of text must say so, not complain that
    /// the card has no deletions.
    #[test]
    fn test_unterminated_cloze_at_eof_is_error() {
        let input = "C: foo [bar";
        let parser = make_test_parser();
        let result = parser.parse(input);
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert_eq!(
            err.to_string(),
            "Unterminated cloze deletion. Location: test.md:1"
        );
    }

    /// BUG-19: a deletion left open at the end of a line is an error.
    #[test]
    fn test_unterminated_cloze_at_eol_is_error() {
        let input = "C: foo [bar\nbaz] quux";
        let parser = make_test_parser();
        let result = parser.parse(input);
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert_eq!(
            err.to_string(),
            "Unterminated cloze deletion. Location: test.md:1"
        );
    }

    /// BUG-17: `[text](url)` is a markdown link, not a cloze deletion.
    /// Byte positions: "See [the docs](https://x) for " is 30 bytes of clean
    /// text; the deletion covers "answer", bytes 30..=35.
    #[test]
    fn test_markdown_link_is_not_a_deletion() -> Result<(), ParserError> {
        let input = "C: See [the docs](https://x) for [answer]";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;

        assert_cloze(&cards, "See [the docs](https://x) for answer", &[(30, 35)]);
        Ok(())
    }

    #[test]
    fn test_cloze_with_initial_blank_line() -> Result<(), ParserError> {
        let input = "C:\nBuild something people want in Lisp.\n\n— [Paul Graham], [_Hackers and Painters_]\n\n";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;

        assert_cloze(
            &cards,
            "Build something people want in Lisp.\n\n— Paul Graham, _Hackers and Painters_",
            &[(42, 52), (55, 76)],
        );
        Ok(())
    }

    #[test]
    fn test_parse_deck() -> Fallible<()> {
        let directory = PathBuf::from("./test");
        let deck = parse_deck(&directory);

        assert!(deck.is_ok());
        let cards = deck?.cards;
        assert_eq!(cards.len(), 2);
        Ok(())
    }

    #[test]
    fn test_identical_basic_cards() -> Result<(), ParserError> {
        let input = "Q: foo\nA: bar\n\nQ: foo\nA: bar\n\n";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;

        assert_eq!(cards.len(), 1);
        Ok(())
    }

    #[test]
    fn test_identical_cloze_cards() -> Result<(), ParserError> {
        let input = "C: foo [bar]\n\nC: foo [bar]";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;

        assert_eq!(cards.len(), 1);
        Ok(())
    }

    #[test]
    fn test_identical_cards_across_files() -> Fallible<()> {
        let directory = temp_dir();
        let directory = directory.join("identical_cards_test");
        create_dir_all(&directory).expect("Failed to create test directory");
        let file1 = directory.join("file1.md");
        let file2 = directory.join("file2.md");
        std::fs::write(&file1, "Q: foo\nA: bar").expect("Failed to write test file");
        std::fs::write(&file2, "Q: foo\nA: bar").expect("Failed to write test file");
        let deck = parse_deck(&directory)?;

        assert_eq!(deck.cards.len(), 1);
        Ok(())
    }

    fn make_test_parser() -> Parser {
        Parser::new("test_deck".to_string(), PathBuf::from("test.md"), 0)
    }

    #[test]
    fn test_duplicate_cards_within_file_reported() -> Result<(), ParserError> {
        let input = "Q: a\nA: b\n\n---\n\nQ: a\nA: b";
        let parser = make_test_parser();
        let parsed = parser.parse_with_duplicates(input)?;
        assert_eq!(parsed.cards.len(), 1);
        assert_eq!(parsed.duplicates.len(), 1);
        let dup = &parsed.duplicates[0];
        assert_eq!(dup.kept().to_string(), "test.md:1");
        assert_eq!(dup.ignored().to_string(), "test.md:6");
        Ok(())
    }

    #[test]
    fn test_duplicate_card_display_names_both_locations() -> Result<(), ParserError> {
        let input = "Q: a\nA: b\n\n---\n\nQ: a\nA: b";
        let parser = make_test_parser();
        let parsed = parser.parse_with_duplicates(input)?;
        let message = parsed.duplicates[0].to_string();
        assert!(message.contains("test.md:1"), "message was: {message}");
        assert!(message.contains("test.md:6"), "message was: {message}");
        assert!(message.contains("duplicate card"), "message was: {message}");
        Ok(())
    }

    #[test]
    fn test_parse_still_dedups_silently() -> Result<(), ParserError> {
        // The plain `parse` API (used by serve-mode edit) must keep returning
        // the deduplicated card list with no behavior change.
        let input = "Q: a\nA: b\n\n---\n\nQ: a\nA: b";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;
        assert_eq!(cards.len(), 1);
        Ok(())
    }

    fn assert_cloze(cards: &[Card], clean_text: &str, deletions: &[(usize, usize)]) {
        assert_eq!(cards.len(), deletions.len());
        for (i, (start, end)) in deletions.iter().enumerate() {
            assert!(matches!(
                &cards[i].content(),
                CardContent::Cloze {
                    text,
                    start: s,
                    end: e,
                } if text == clean_text && *s == *start && *e == *end
            ));
        }
    }

    /// Parsing invalid UTF-8.
    ///
    /// This is tricky to test directly because Rust strings are UTF-8. We can
    /// simulate it by creating a byte array with invalid UTF-8, and using an
    /// unsafe method to convert it to a string without validation.
    #[test]
    fn test_invalid_utf8() {
        let input = unsafe {
            #[allow(invalid_from_utf8_unchecked)]
            std::str::from_utf8_unchecked(b"C: Valid text [\xFF\xFF]")
        };
        let parser = make_test_parser();
        let result = parser.parse(input);
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert_eq!(
            err.to_string(),
            "Cloze card contains invalid UTF-8. Location: test.md:1"
        );
    }

    /// See: <https://github.com/eudoxia0/hashcards/issues/29>
    #[test]
    fn test_cloze_deletion_with_exclamation_sign() -> Result<(), ParserError> {
        let input = "C: The notation [$n!$] means 'n factorial'.";
        let parser = make_test_parser();
        let result = parser.parse(input);
        let cards = result.unwrap();
        assert_eq!(cards.len(), 1);
        let card: Card = cards[0].clone();
        match &card.content() {
            CardContent::Cloze { text, .. } => {
                assert_eq!(text, "The notation $n!$ means 'n factorial'.");
            }
            _ => panic!("Expected cloze card."),
        }
        Ok(())
    }

    #[test]
    fn test_cloze_deletion_with_math() -> Result<(), ParserError> {
        let input = "C: The string `\\alpha` renders as [$\\alpha$].";
        let parser = make_test_parser();
        let result = parser.parse(input);
        let cards = result.unwrap();
        assert_eq!(cards.len(), 1);
        let card: Card = cards[0].clone();
        match &card.content() {
            CardContent::Cloze { text, .. } => {
                assert_eq!(text, "The string `\\alpha` renders as $\\alpha$.");
            }
            _ => panic!("Expected cloze card."),
        }
        Ok(())
    }

    #[test]
    fn test_extract_frontmatter_with_name() {
        let input = r#"---
name = "Custom Deck Name"
---

Q: What is Rust?
A: A systems programming language."#;

        let result = extract_frontmatter(input);
        assert!(result.is_ok());
        let (metadata, content, offset) = result.unwrap();
        assert_eq!(metadata.name, Some("Custom Deck Name".to_string()));
        assert_eq!(
            content.trim(),
            "Q: What is Rust?\nA: A systems programming language."
        );
        assert_eq!(offset, 3);
    }

    #[test]
    fn test_extract_frontmatter_without_name() {
        let input = r#"---
other_field = "value"
---

Q: What is Rust?
A: A systems programming language."#;

        let result = extract_frontmatter(input);
        assert!(result.is_ok());
        let (metadata, content, offset) = result.unwrap();
        assert_eq!(metadata.name, None);
        assert_eq!(
            content.trim(),
            "Q: What is Rust?\nA: A systems programming language."
        );
        assert_eq!(offset, 3);
    }

    #[test]
    fn test_extract_frontmatter_empty() {
        let input = r#"---
---

Q: What is Rust?
A: A systems programming language."#;

        let result = extract_frontmatter(input);
        assert!(result.is_ok());
        let (metadata, content, offset) = result.unwrap();
        assert_eq!(metadata.name, None);
        assert_eq!(
            content.trim(),
            "Q: What is Rust?\nA: A systems programming language."
        );
        assert_eq!(offset, 2);
    }

    #[test]
    fn test_no_frontmatter() {
        let input = "Q: What is Rust?\nA: A systems programming language.";
        let result = extract_frontmatter(input);
        assert!(result.is_ok());
        let (metadata, content, offset) = result.unwrap();
        assert_eq!(metadata.name, None);
        assert_eq!(content, input);
        assert_eq!(offset, 0);
    }

    #[test]
    fn test_frontmatter_unclosed() {
        let input = r#"---
name = "Custom Deck Name"

Q: What is Rust?
A: A systems programming language."#;

        let result = extract_frontmatter(input);
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.to_string().contains("no closing '---'"));
    }

    #[test]
    fn test_frontmatter_invalid_toml() {
        let input = r#"---
name = Custom Deck Name (missing quotes)
---

Q: What is Rust?"#;

        let result = extract_frontmatter(input);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_with_frontmatter() -> Result<(), ParserError> {
        let input = r#"---
name = "Custom Deck Name"
---

Q: What is Rust?
A: A systems programming language."#;

        let (metadata, content, _offset) = extract_frontmatter(input).unwrap();
        assert_eq!(metadata.name, Some("Custom Deck Name".to_string()));

        let parser = make_test_parser();
        let cards = parser.parse(content)?;
        assert_eq!(cards.len(), 1);
        Ok(())
    }

    #[test]
    fn test_parse_deck_with_frontmatter() -> Fallible<()> {
        let directory = temp_dir();
        let directory = directory.join("frontmatter_test");
        create_dir_all(&directory).expect("Failed to create test directory");

        let file1 = directory.join("ch1.md");
        let file2 = directory.join("ch2.md");

        std::fs::write(
            &file1,
            r#"---
name = "Cell Biology"
---

Q: What is a cell?
A: The basic unit of life."#,
        )
        .expect("Failed to write test file");

        std::fs::write(
            &file2,
            r#"---
name = "Cell Biology"
---

Q: What is DNA?
A: Genetic material."#,
        )
        .expect("Failed to write test file");

        let deck = parse_deck(&directory)?;

        // Both cards should have the custom deck name "Cell Biology"
        assert_eq!(deck.cards.len(), 2);
        for card in &deck.cards {
            assert_eq!(card.deck_name(), "Cell Biology");
        }

        // Clean up
        std::fs::remove_dir_all(&directory).ok();

        Ok(())
    }

    /// BUG-20: a parse error after `---` frontmatter reports the real file
    /// line, not the line within the stripped content.
    #[test]
    fn test_parse_error_after_frontmatter_reports_real_line() -> Fallible<()> {
        let directory = temp_dir().join("frontmatter_error_line_test");
        create_dir_all(&directory).expect("Failed to create test directory");
        let file = directory.join("deck.md");
        // "A: orphan" is on file line 5 (1-based).
        std::fs::write(&file, "---\nname = \"X\"\n---\n\nA: orphan answer\n")
            .expect("Failed to write test file");

        let result = parse_deck(&directory);
        std::fs::remove_dir_all(&directory).ok();

        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(
            err.to_string().contains("deck.md:5"),
            "expected real file line 5 in: {err}"
        );
        Ok(())
    }

    /// BUG-20: card ranges are absolute file lines when frontmatter is present.
    #[test]
    fn test_card_range_accounts_for_frontmatter() -> Fallible<()> {
        let directory = temp_dir().join("frontmatter_range_test");
        create_dir_all(&directory).expect("Failed to create test directory");
        let file = directory.join("deck.md");
        // Q: is on 0-based file line 4, A: on line 5.
        std::fs::write(&file, "---\nname = \"X\"\n---\n\nQ: question\nA: answer\n")
            .expect("Failed to write test file");

        let deck = parse_deck(&directory);
        std::fs::remove_dir_all(&directory).ok();

        let cards = deck?.cards;
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].range(), (4, 5));
        Ok(())
    }

    #[test]
    fn test_separator_between_basic_cards() -> Result<(), ParserError> {
        let input = "Q: foo\nA: bar\n---\nQ: baz\nA: quux";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;

        assert_eq!(cards.len(), 2);
        assert!(matches!(
            &cards[0].content(),
            CardContent::Basic {
                question,
                answer,
            } if question == "foo" && answer == "bar"
        ));
        assert!(matches!(
            &cards[1].content(),
            CardContent::Basic {
                question,
                answer,
            } if question == "baz" && answer == "quux"
        ));
        Ok(())
    }

    #[test]
    fn test_separator_after_cloze_card() -> Result<(), ParserError> {
        let input = "C: [foo]\n---\nQ: Question\nA: Answer";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;

        assert_eq!(cards.len(), 2);
        assert_cloze(&cards[0..1], "foo", &[(0, 2)]);
        assert!(matches!(
            &cards[1].content(),
            CardContent::Basic {
                question,
                answer,
            } if question == "Question" && answer == "Answer"
        ));
        Ok(())
    }

    #[test]
    fn test_separator_between_cloze_cards() -> Result<(), ParserError> {
        let input = "C: [foo]\n---\nC: [bar]";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;

        assert_eq!(cards.len(), 2);
        assert_cloze(&cards[0..1], "foo", &[(0, 2)]);
        assert_cloze(&cards[1..2], "bar", &[(0, 2)]);
        Ok(())
    }

    #[test]
    fn test_separator_in_question_errors() -> Result<(), ParserError> {
        let input = "Q: Question\n---\nA: Answer";
        let parser = make_test_parser();
        let result = parser.parse(input);

        assert!(result.is_err());
        if let Err(e) = result {
            assert!(e.to_string().contains("separator"));
        }
        Ok(())
    }

    #[test]
    fn test_multiple_separators() -> Result<(), ParserError> {
        let input = "Q: foo\nA: bar\n---\n---\nQ: baz\nA: quux";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;

        assert_eq!(cards.len(), 2);
        assert!(matches!(
            &cards[0].content(),
            CardContent::Basic {
                question,
                answer,
            } if question == "foo" && answer == "bar"
        ));
        assert!(matches!(
            &cards[1].content(),
            CardContent::Basic {
                question,
                answer,
            } if question == "baz" && answer == "quux"
        ));
        Ok(())
    }

    #[test]
    fn test_separator_at_end() -> Result<(), ParserError> {
        let input = "Q: foo\nA: bar\n---";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;

        assert_eq!(cards.len(), 1);
        assert!(matches!(
            &cards[0].content(),
            CardContent::Basic {
                question,
                answer,
            } if question == "foo" && answer == "bar"
        ));
        Ok(())
    }

    #[test]
    fn test_line_after_end_state_is_error() {
        let parser = make_test_parser();
        let mut cards = Vec::new();
        let result = parser.parse_line(State::End, Line::Eof, 0, &mut cards);
        assert!(result.is_err());
    }

    /// BUG-18: `Q:`/`C:`/`---` lines inside a fenced code block are literal
    /// text, so an answer containing a fence round-trips as one card.
    #[test]
    fn test_card_syntax_inside_backtick_fence_is_text() -> Result<(), ParserError> {
        let input = "Q: What does the file look like?\nA: Like this:\n```\nQ: not a card\n---\nC: not [a] cloze\n```\nDone.";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;

        assert_eq!(cards.len(), 1);
        assert!(matches!(
            &cards[0].content(),
            CardContent::Basic {
                question,
                answer,
            } if question == "What does the file look like?"
                && answer == "Like this:\n```\nQ: not a card\n---\nC: not [a] cloze\n```\nDone."
        ));
        Ok(())
    }

    /// BUG-18: tilde fences count too.
    #[test]
    fn test_card_syntax_inside_tilde_fence_is_text() -> Result<(), ParserError> {
        let input = "Q: q\nA: a\n~~~\nQ: not a card\n~~~";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;

        assert_eq!(cards.len(), 1);
        assert!(matches!(
            &cards[0].content(),
            CardContent::Basic {
                question,
                answer,
            } if question == "q" && answer == "a\n~~~\nQ: not a card\n~~~"
        ));
        Ok(())
    }

    /// BUG-21: frontmatter errors name the file they came from.
    #[test]
    fn test_frontmatter_error_carries_file_path() -> Fallible<()> {
        let directory = temp_dir().join("frontmatter_path_test");
        create_dir_all(&directory).expect("Failed to create test directory");
        let file = directory.join("broken.md");
        std::fs::write(&file, "---\nname = \"X\"\n").expect("Failed to write test file");

        let result = parse_deck(&directory);
        std::fs::remove_dir_all(&directory).ok();

        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(
            err.to_string().contains("broken.md"),
            "expected file path in: {err}"
        );
        assert!(err.to_string().contains("no closing '---'"));
        Ok(())
    }

    /// BUG-26: cloze positions are byte offsets. Multi-byte characters before
    /// and inside the deletion must yield byte positions, not char positions.
    /// "Größe: " is 9 bytes; "10 µm" is 6 bytes (µ is 2 bytes).
    #[test]
    fn test_non_ascii_cloze_positions_are_bytes() -> Result<(), ParserError> {
        let input = "C: Größe: [10 µm]";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;

        assert_cloze(&cards, "Größe: 10 µm", &[(9, 14)]);
        Ok(())
    }

    /// FEAT-06: `T:`/`D:` shorthand expands into two reciprocal cards.
    #[test]
    fn test_term_definition_expands_to_two_cards() -> Result<(), ParserError> {
        let input = "T: Monoid\nD: A semigroup with an identity element.";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;
        assert_eq!(cards.len(), 2);
        assert!(matches!(
            &cards[0].content(),
            CardContent::Basic { question, answer }
                if question == "Define: Monoid"
                && answer == "A semigroup with an identity element."
        ));
        assert!(matches!(
            &cards[1].content(),
            CardContent::Basic { question, answer }
                if question == "Term for: A semigroup with an identity element."
                && answer == "Monoid"
        ));
        Ok(())
    }

    /// Regression: `D:` is the definition tag only while a term is being
    /// read. An answer line that merely begins with `D:` is ordinary text,
    /// not a tag, and must not fail the whole collection load.
    #[test]
    fn test_definition_prefix_in_answer_is_plain_text() -> Result<(), ParserError> {
        let input = "Q: What is the genotype?\nA: Aa\nD: dominant allele shown";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;
        assert_eq!(cards.len(), 1);
        assert!(matches!(
            &cards[0].content(),
            CardContent::Basic { question, answer }
                if question == "What is the genotype?"
                && answer == "Aa\nD: dominant allele shown"
        ));
        Ok(())
    }

    /// A `D:` line still forms a term-definition pair directly under a `T:`.
    #[test]
    fn test_definition_after_term_is_still_a_tag() -> Result<(), ParserError> {
        let input = "T: Monoid\nD: A semigroup with an identity element.";
        let parser = make_test_parser();
        assert_eq!(parser.parse(input)?.len(), 2);
        Ok(())
    }

    /// Regression: a line like ```` ```code``` ```` is inline code under
    /// CommonMark, not a fence opener -- a backtick fence's info string may
    /// not contain backticks. Treating it as an opener swallows every card
    /// after it in the file.
    #[test]
    fn test_inline_code_span_does_not_open_a_fence() -> Result<(), ParserError> {
        let input = "Q: First\n\
                     A: Its literal form is\n\
                     ```code``` and nothing more\n\
                     \n\
                     ---\n\
                     \n\
                     Q: Second\n\
                     A: Still parsed";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;
        assert_eq!(
            cards.len(),
            2,
            "an inline code span must not swallow later cards"
        );
        Ok(())
    }

    /// A real fence still suppresses card tags inside it.
    #[test]
    fn test_real_fence_still_suppresses_tags() -> Result<(), ParserError> {
        let input = "Q: First\n\
                     A: See below\n\
                     ```\n\
                     Q: not a card\n\
                     ```";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;
        assert_eq!(cards.len(), 1);
        Ok(())
    }

    /// Regression: a markdown link label may contain balanced nested
    /// brackets. Those belong to the link, not to a cloze deletion, and must
    /// not trip the nested-bracket guard.
    #[test]
    fn test_nested_brackets_in_link_label() -> Result<(), ParserError> {
        let input = "C: See [the [inner] docs](http://x) for [answer]";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;
        assert_eq!(cards.len(), 1, "only [answer] is a cloze deletion");
        assert_cloze(
            &cards,
            "See [the [inner] docs](http://x) for answer",
            &[(37, 42)],
        );
        Ok(())
    }

    #[test]
    fn test_term_definition_hashes_match_handwritten_cards() -> Result<(), ParserError> {
        let shorthand = "T: Monoid\nD: A semigroup with an identity element.";
        let handwritten = "Q: Define: Monoid\n\
                           A: A semigroup with an identity element.\n\
                           \n\
                           ---\n\
                           \n\
                           Q: Term for: A semigroup with an identity element.\n\
                           A: Monoid";
        let parser = make_test_parser();
        let from_shorthand: HashSet<_> =
            parser.parse(shorthand)?.iter().map(|c| c.hash()).collect();
        let from_handwritten: HashSet<_> = parser
            .parse(handwritten)?
            .iter()
            .map(|c| c.hash())
            .collect();
        assert_eq!(from_shorthand.len(), 2);
        assert_eq!(from_shorthand, from_handwritten);
        Ok(())
    }

    #[test]
    fn test_two_term_pairs_separated() -> Result<(), ParserError> {
        let input = "T: Monoid\nD: A semigroup with an identity element.\n\n---\n\nT: Magma\nD: A set with a binary operation.";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;
        assert_eq!(cards.len(), 4);
        Ok(())
    }

    #[test]
    fn test_term_pair_followed_directly_by_term_pair() -> Result<(), ParserError> {
        // A new T: finalizes the previous pair, like Q: after an answer.
        let input = "T: Monoid\nD: A semigroup with an identity element.\nT: Magma\nD: A set with a binary operation.";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;
        assert_eq!(cards.len(), 4);
        Ok(())
    }

    /// `D:` is a tag only directly under a `T:`. A `D:` line with no term
    /// above it is ordinary prose, exactly as it was before the T:/D:
    /// shorthand existed, and is skipped rather than failing the load.
    #[test]
    fn test_definition_without_term_is_prose() -> Result<(), ParserError> {
        let input = "D: A semigroup with an identity element.";
        let parser = make_test_parser();
        assert!(parser.parse(input)?.is_empty());
        Ok(())
    }

    #[test]
    fn test_term_without_definition_at_eof_errors() {
        let input = "T: Monoid";
        let parser = make_test_parser();
        let result = parser.parse(input);
        assert!(result.is_err());
        let message = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            message.contains("without a definition"),
            "message was: {message}"
        );
    }

    #[test]
    fn test_term_then_question_errors() {
        let input = "T: Monoid\nQ: What is a monoid?";
        let parser = make_test_parser();
        assert!(parser.parse(input).is_err());
    }

    #[test]
    fn test_term_then_separator_errors() {
        let input = "T: Monoid\n---";
        let parser = make_test_parser();
        assert!(parser.parse(input).is_err());
    }

    #[test]
    fn test_multiline_term_and_definition() -> Result<(), ParserError> {
        let input = "T: Monoid\nhomomorphism\nD: A map between monoids\nthat preserves the operation and identity.";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;
        assert_eq!(cards.len(), 2);
        assert!(matches!(
            &cards[0].content(),
            CardContent::Basic { question, .. }
                if question == "Define:\n\nMonoid\nhomomorphism"
        ));
        Ok(())
    }

    /// Regression: a multi-line term has the same defect the reciprocal
    /// card had — markdown folds its first line into the `Define:` prompt
    /// and renders the rest as a sibling block.
    #[test]
    fn test_multiline_term_stays_a_block_on_the_forward_card() -> Result<(), ParserError> {
        let input = "T:\n- Algorithmus\n- Rechenvorschrift\nD: Eine endliche Vorschrift.";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;
        assert_eq!(cards.len(), 2);
        assert!(
            matches!(
                &cards[0].content(),
                CardContent::Basic { question, answer }
                    if question == "Define:\n\n- Algorithmus\n- Rechenvorschrift"
                    && answer == "Eine endliche Vorschrift."
            ),
            "the forward card's question keeps the term inline"
        );
        Ok(())
    }

    /// Regression: a multi-line definition must not be interpolated into
    /// the reciprocal card's question line. Markdown folds the definition's
    /// first line into the `Term for:` prompt and renders the rest as a
    /// sibling block, so the card reads as a copy of its twin with the
    /// answer stranded underneath. The prompt keeps its own line instead.
    #[test]
    fn test_multiline_definition_stays_a_block_on_the_reverse_card() -> Result<(), ParserError> {
        let input = "T: Algorithmus\nD:\n- präzise Vorschrift\n- endliche Schritte";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;
        assert_eq!(cards.len(), 2);
        assert!(
            matches!(
                &cards[1].content(),
                CardContent::Basic { question, answer }
                    if question == "Term for:\n\n- präzise Vorschrift\n- endliche Schritte"
                    && answer == "Algorithmus"
            ),
            "the reverse card's question keeps the definition inline"
        );
        Ok(())
    }

    #[test]
    fn test_term_pair_after_basic_card() -> Result<(), ParserError> {
        let input = "Q: What is Rust?\nA: A systems programming language.\nT: Monoid\nD: A semigroup with an identity element.";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;
        assert_eq!(cards.len(), 3);
        Ok(())
    }

    #[test]
    fn test_term_pair_after_cloze_card() -> Result<(), ParserError> {
        let input = "C: An [agonist] activates a receptor.\nT: Monoid\nD: A semigroup with an identity element.";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;
        assert_eq!(cards.len(), 3);
        Ok(())
    }

    #[test]
    fn test_term_pair_followed_by_question() -> Result<(), ParserError> {
        let input =
            "T: Monoid\nD: A semigroup with an identity element.\nQ: What is Rust?\nA: A language.";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;
        assert_eq!(cards.len(), 3);
        Ok(())
    }

    /// A second `D:` under the same term continues the definition as text
    /// rather than erroring: the parser is reading a definition, not a term,
    /// so the prefix is no longer a tag.
    #[test]
    fn test_second_definition_line_continues_the_definition() -> Result<(), ParserError> {
        let input = "T: Monoid\nD: first\nD: second";
        let parser = make_test_parser();
        let cards = parser.parse(input)?;
        assert_eq!(cards.len(), 2);
        assert!(matches!(
            &cards[0].content(),
            CardContent::Basic { question, answer }
                if question == "Define: Monoid" && answer == "first\nD: second"
        ));
        Ok(())
    }
}
