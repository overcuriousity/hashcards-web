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

//! JSON export of a collection: every card, its scheduling state, and the
//! full review history.
//!
//! The markdown lives in git, but the review databases live under
//! `data_dir` and are in nobody's repository. Under `[oidc]` a user has no
//! filesystem access either, so this route is their only way to get their
//! own review history out.

use axum::extract::Path;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::CONTENT_DISPOSITION;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::response::Response;
use serde::Serialize;

use crate::cmd::run_blocking;
use crate::cmd::serve::auth::CurrentUser;
use crate::cmd::serve::handlers::collection_exists;
use crate::cmd::serve::handlers::find_collection;
use crate::cmd::serve::reviewdb::open_collection_db;
use crate::cmd::serve::state::AppState;
use crate::collection::Collection;
use crate::db::ReviewRow;
use crate::db::SessionRow;
use crate::error::ErrorReport;
use crate::error::Fallible;
use crate::fsrs::Difficulty;
use crate::fsrs::Grade;
use crate::fsrs::Interval;
use crate::fsrs::Stability;
use crate::types::aliases::DeckName;
use crate::types::card::CardContent;
use crate::types::card_hash::CardHash;
use crate::types::date::Date;
use crate::types::performance::Performance;
use crate::types::performance::ReviewedPerformance;
use crate::types::timestamp::Timestamp;

/// GET /collection/{slug}/export — the collection as a JSON download.
///
/// Owner-gated through `find_collection`, so a collection belonging to
/// somebody else 404s exactly like one that does not exist.
pub async fn collection_export_handler(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    current_user: Option<CurrentUser>,
) -> Response {
    let owner = current_user.map(|u| u.email);
    let state2 = state.clone();
    let slug2 = slug.clone();
    let owner2 = owner.clone();
    // Parsing the collection and reading SQLite is blocking work.
    match run_blocking(move || export_inner(&state2, &slug2, owner2.as_deref())).await {
        Ok(json) => (
            StatusCode::OK,
            [
                (CONTENT_TYPE, "application/json".to_string()),
                (
                    CONTENT_DISPOSITION,
                    format!(
                        "attachment; filename=\"{}-export.json\"",
                        download_name(&slug)
                    ),
                ),
            ],
            json,
        )
            .into_response(),
        Err(e) => {
            // Only now, and off the executor: the lookup reads the caller's
            // local card folder, and it is needed only to tell "no such
            // collection" apart from "it broke".
            let known = collection_exists(&state, &slug, owner).await;
            let status = if known {
                StatusCode::INTERNAL_SERVER_ERROR
            } else {
                StatusCode::NOT_FOUND
            };
            (status, e.to_string()).into_response()
        }
    }
}

/// Slugs come from `slugify`, which already restricts them to alphanumerics,
/// `-`, `_` and `.`. Belt and braces: a quote or newline reaching the
/// `Content-Disposition` header would let a filename split it.
fn download_name(slug: &str) -> String {
    let name: String = slug
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if name.is_empty() {
        "collection".to_string()
    } else {
        name
    }
}

fn export_inner(state: &AppState, slug: &str, owner: Option<&str>) -> Fallible<String> {
    let rc = find_collection(state, slug, owner)
        .ok_or_else(|| ErrorReport::new(format!("Unknown collection: {slug}")))?;
    let collection = Collection::open(rc.coll_dir.clone(), open_collection_db(state, &rc)?)?;
    let export = get_export(collection)?;
    Ok(serde_json::to_string_pretty(&export)?)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Export {
    cards: Vec<CardExport>,
    sessions: Vec<SessionExport>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CardExport {
    hash: CardHash,
    family_hash: Option<CardHash>,
    deck_name: DeckName,
    location: LocationExport,
    content: CardContentExport,
    performance: Option<PerformanceExport>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LocationExport {
    file_path: String,
    line_start: usize,
    line_end: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
enum CardContentExport {
    Basic {
        question: String,
        answer: String,
    },
    Cloze {
        /// The text of the card without cloze brackets.
        text: String,
        /// Byte offset (not character offset) of the first byte of the
        /// deletion within `text`.
        start: usize,
        /// Byte offset (not character offset) of the last byte of the
        /// deletion within `text`, inclusive.
        end: usize,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PerformanceExport {
    last_reviewed_at: Timestamp,
    stability: Stability,
    difficulty: Difficulty,
    /// The raw FSRS interval in days, before rounding and clamping.
    interval_raw: Interval,
    interval_days: i64,
    due_date: Date,
    review_count: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionExport {
    session_id: i64,
    started_at: Timestamp,
    ended_at: Timestamp,
    reviews: Vec<ReviewExport>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewExport {
    review_id: i64,
    hash: CardHash,
    reviewed_at: Timestamp,
    grade: Grade,
    stability: Stability,
    difficulty: Difficulty,
    /// The raw FSRS interval in days, before rounding and clamping.
    interval_raw: Interval,
    interval_days: i64,
    due_date: Date,
}

fn get_export(coll: Collection) -> Fallible<Export> {
    let cards: Vec<CardExport> = get_card_export(&coll)?;
    let sessions: Vec<SessionExport> = get_session_export(&coll)?;
    Ok(Export { cards, sessions })
}

fn get_card_export(coll: &Collection) -> Fallible<Vec<CardExport>> {
    let mut cards: Vec<CardExport> = Vec::new();
    for card in coll.cards.iter() {
        let p = coll.db.get_card_performance_opt(card.hash())?;
        let ce = CardExport {
            hash: card.hash(),
            family_hash: card.family_hash(),
            deck_name: card.deck_name().to_owned(),
            location: LocationExport {
                file_path: card.file_path().clone().display().to_string(),
                line_start: card.range().0,
                line_end: card.range().1,
            },
            content: match card.content() {
                CardContent::Basic { question, answer } => CardContentExport::Basic {
                    question: question.clone(),
                    answer: answer.clone(),
                },
                CardContent::Cloze { text, start, end } => CardContentExport::Cloze {
                    text: text.clone(),
                    start: *start,
                    end: *end,
                },
            },
            performance: export_performance(p),
        };
        cards.push(ce);
    }
    Ok(cards)
}

fn export_performance(p: Option<Performance>) -> Option<PerformanceExport> {
    match p {
        Some(p) => match p {
            Performance::New => None,
            Performance::Reviewed(ReviewedPerformance {
                last_reviewed_at,
                stability,
                difficulty,
                interval_raw,
                interval_days,
                due_date,
                review_count,
            }) => Some(PerformanceExport {
                last_reviewed_at,
                stability,
                difficulty,
                interval_raw,
                interval_days,
                due_date,
                review_count,
            }),
        },
        None => None,
    }
}

fn get_session_export(coll: &Collection) -> Fallible<Vec<SessionExport>> {
    let sessions = coll.db.get_all_sessions()?;
    let mut session_exports: Vec<SessionExport> = Vec::new();
    for session in sessions.into_iter() {
        let session_export = export_session(coll, session)?;
        session_exports.push(session_export);
    }
    Ok(session_exports)
}

fn export_session(coll: &Collection, session: SessionRow) -> Fallible<SessionExport> {
    let reviews = coll.db.get_reviews_for_session(session.session_id)?;
    let mut review_exports: Vec<ReviewExport> = Vec::new();
    for review in reviews.into_iter() {
        let review_export = export_review(review);
        review_exports.push(review_export)
    }
    Ok(SessionExport {
        session_id: session.session_id,
        started_at: session.started_at,
        ended_at: session.ended_at,
        reviews: review_exports,
    })
}

fn export_review(review: ReviewRow) -> ReviewExport {
    ReviewExport {
        review_id: review.review_id,
        hash: review.data.card_hash,
        reviewed_at: review.data.reviewed_at,
        grade: review.data.grade,
        stability: review.data.stability,
        difficulty: review.data.difficulty,
        interval_raw: review.data.interval_raw,
        interval_days: review.data.interval_days,
        due_date: review.data.due_date,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::cmd::serve::state::test_support::state_with_data_dir;
    use crate::db::ReviewRecord;
    use crate::helper::create_tmp_copy_of_test_directory;
    use crate::helper::create_tmp_directory;
    use crate::parser::parse_deck;
    use crate::types::card_hash::Hasher;

    /// A collection with review history exports its cards and its sessions.
    #[test]
    fn test_full_export() -> Fallible<()> {
        let dir = create_tmp_copy_of_test_directory()?;
        let coll = Collection::new(Some(dir.clone()))?;
        let deck = parse_deck(&PathBuf::from(dir.clone()))?;
        let now = Timestamp::now();
        let mut reviews = Vec::new();
        for card in deck.cards {
            coll.db.insert_card(card.hash(), now)?;
            let performance = Performance::Reviewed(ReviewedPerformance {
                last_reviewed_at: now,
                stability: 1.0,
                difficulty: 3.0,
                interval_raw: 1.0,
                interval_days: 1,
                due_date: now.date(),
                review_count: 1,
            });
            coll.db.update_card_performance(card.hash(), performance)?;
            reviews.push(ReviewRecord {
                card_hash: card.hash(),
                reviewed_at: now,
                grade: Grade::Easy,
                stability: 1.0,
                difficulty: 3.0,
                interval_raw: 1.0,
                interval_days: 1,
                duration_ms: None,
                due_date: now.date(),
            });
        }
        let session_id = coll.db.create_session(now)?;
        for review in &reviews {
            coll.db.insert_review_immediately(session_id, review)?;
        }
        coll.db.close_session(session_id, now)?;

        let export = get_export(Collection::new(Some(dir))?)?;
        assert!(!export.cards.is_empty(), "the export must carry the cards");
        assert_eq!(
            export.sessions.len(),
            1,
            "the export must carry the closed session"
        );
        assert_eq!(
            export.sessions[0].reviews.len(),
            reviews.len(),
            "every review must be exported"
        );
        Ok(())
    }

    /// The JSON export carries the content-based (v2) cloze hashes.
    #[test]
    fn test_export_uses_content_based_cloze_hashes() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        std::fs::write(dir.join("Deck.md"), "C: Water is [wet].\n")?;
        let coll = Collection::new(Some(dir.display().to_string()))?;
        let export = get_export(coll)?;
        let json = serde_json::to_string(&export)?;
        // Reference formula: clean text "Water is wet." with deletion "wet"
        // (bytes 9..=11), first occurrence.
        let mut hasher = Hasher::new();
        hasher.update(b"ClozeV2");
        hasher.update(b"Water is wet.");
        hasher.update(&[0xFF]);
        hasher.update(b"wet");
        hasher.update(&[0xFF]);
        hasher.update(b"0");
        let expected = hasher.finalize();
        assert!(
            json.contains(&expected.to_hex()),
            "export does not contain the v2 cloze hash"
        );
        Ok(())
    }

    /// The export route is owner-gated: the owner gets JSON, and another
    /// logged-in user gets the same "unknown collection" treatment as for a
    /// slug that does not exist at all.
    #[test]
    fn test_export_is_scoped_to_the_owner() -> Fallible<()> {
        use crate::cmd::serve::cards::CardRoot;
        use crate::cmd::serve::cards::collection_id;

        let data_dir = create_tmp_directory()?;
        let root = CardRoot::for_user(&data_dir, Some("alice@example.com"))?;
        let folder = root.path().join("alice-deck");
        std::fs::create_dir_all(&folder)?;
        std::fs::write(folder.join("Deck.md"), "Q: What is 1+1?\nA: 2\n")?;
        collection_id(&folder)?;
        std::fs::create_dir_all(data_dir.join("db"))?;
        let state = state_with_data_dir(data_dir);

        let json = export_inner(&state, "alice-deck", Some("alice@example.com"))?;
        assert!(
            json.contains("\"cards\""),
            "the owner must receive the export, got: {json}"
        );

        assert!(
            export_inner(&state, "alice-deck", Some("bob@example.com")).is_err(),
            "another user must not be able to export Alice's collection"
        );
        assert!(
            export_inner(&state, "alice-deck", None).is_err(),
            "an anonymous caller must not be able to export an owned collection"
        );
        Ok(())
    }

    /// A slug can never break out of the Content-Disposition header.
    #[test]
    fn test_download_name_strips_header_breaking_characters() {
        assert_eq!(download_name("japanese"), "japanese");
        assert_eq!(download_name("a\"b\nc"), "abc");
        assert_eq!(download_name("...."), "collection");
    }
}
