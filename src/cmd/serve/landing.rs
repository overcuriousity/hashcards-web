use std::collections::HashMap;

use axum::extract::Query;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Html;
use maud::Markup;
use maud::html;

use crate::cmd::drill::template::page_template;
use crate::cmd::serve::auth::CurrentUser;
use crate::cmd::serve::counts::refresh_collection_info;
use crate::cmd::serve::decks::ResolvedCustomDeck;
use crate::cmd::serve::files::collections_for_user;
use crate::cmd::serve::handlers::deck_card_counts;
use crate::cmd::serve::handlers::deck_sources;
use crate::cmd::serve::state::AppState;
use crate::cmd::serve::state::CollectionInfo;
use crate::flash::Flash;

pub async fn landing_handler(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    current_user: Option<CurrentUser>,
) -> (StatusCode, Html<String>) {
    let flash = Flash::from_query(&query);
    let owner = current_user.as_ref().map(|u| u.email.clone());

    // Collections are discovered per request: a folder created a moment ago
    // must appear at once, and reading a directory is cheap next to the
    // count refresh that follows it.
    let local_infos = {
        let state = state.clone();
        let user = current_user.clone();
        match tokio::task::spawn_blocking(move || {
            refresh_collection_info(&state, &collections_for_user(&state, user.as_ref()))
        })
        .await
        {
            Ok(infos) => infos,
            Err(e) => {
                log::error!("Failed to count local collections: {e}");
                Vec::new()
            }
        }
    };

    let collections: Vec<&CollectionInfo> = local_infos
        .iter()
        .filter(|c| c.owner.as_deref() == owner.as_deref())
        .collect();
    let config_available = state.config.data_dir.is_some();
    let custom_decks: Vec<ResolvedCustomDeck> = state
        .custom_decks
        .lock()
        .iter()
        .filter(|d| d.owner.as_deref() == owner.as_deref())
        .cloned()
        .collect();
    // FEAT-03: surface running sessions so they can be resumed rather than
    // silently restarted. Only the caller's own: another user's session may
    // be filed under the same slug, and it is not this page's to offer.
    let resume: HashMap<String, usize> = {
        let sessions = state.sessions.lock();
        sessions
            .iter()
            .filter(|(key, _)| key.owner() == owner.as_deref())
            .filter_map(|(key, s)| {
                let session = s.lock();
                if session.mutable.finished_at.is_none() {
                    Some((key.slug().to_string(), session.mutable.cards.len()))
                } else {
                    None
                }
            })
            .collect()
    };

    // Collections and decks are both things you sit down and drill, so they
    // share one list. A deck's counts are not cached anywhere — they are a
    // selection over collections — so they are computed here.
    let mut rows: Vec<DrillRow> = collections
        .iter()
        .map(|c| DrillRow {
            name: c.name.clone(),
            slug: c.slug.clone(),
            counts: Some(RowCounts {
                due_today: c.due_today,
                total_cards: c.total_cards,
            }),
            is_deck: false,
        })
        .collect();
    for deck in custom_decks {
        let state = state.clone();
        let user = current_user.clone();
        // The counting closure takes the deck and hands it back on success;
        // the error arms need its name and slug either way.
        let uncounted = deck.clone();
        let counts = tokio::task::spawn_blocking(move || {
            let sources = deck_sources(&state, &deck, user.as_ref().map(|u| u.email.as_str()));
            deck_card_counts(&state, &sources).map(|(due, total)| (deck, due, total))
        })
        .await;
        // Counting can fail either way — the collection would not load, or
        // the blocking task did not finish — and the row is the same either
        // way, so the two are flattened to one message.
        let counted = match counts {
            Ok(inner) => inner.map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        // A deck whose members cannot be read must not take the page down
        // with it, and must not vanish from it either: a deck the user saved
        // and can still open is listed, uncounted, and says so.
        match counted {
            Ok((deck, due_today, total_cards)) => rows.push(DrillRow {
                name: deck.name.clone(),
                slug: deck.slug.clone(),
                counts: Some(RowCounts {
                    due_today,
                    total_cards,
                }),
                is_deck: true,
            }),
            Err(e) => {
                log::error!("Failed to count the deck '{}': {e}", uncounted.name);
                rows.push(uncounted_row(&uncounted));
            }
        }
    }

    let status = LandingStatus {
        config_available,
        signed_in_as: owner.clone(),
    };
    let html = render_landing_page(&rows, &resume, &status, flash);
    (StatusCode::OK, Html(html.into_string()))
}

/// The server-status details shown under the collection list: whether the
/// config file is in play, how the sources stand, and who is signed in.
struct LandingStatus {
    config_available: bool,
    /// The logged-in user's email, when `[oidc]` is configured. Drives the
    /// only logout control in the UI.
    signed_in_as: Option<String>,
}

/// One row of the list: a collection or a saved deck. Both are things you
/// sit down and drill, so the list does not separate them; only the tag on a
/// deck row says which is which.
struct DrillRow {
    name: String,
    slug: String,
    /// `None` when the counts could not be computed — a deck whose member
    /// collection has stopped parsing, say. The row still appears: it is
    /// something the user saved and can still open, and the page it opens
    /// is where the failure is explained.
    counts: Option<RowCounts>,
    is_deck: bool,
}

/// What a row says about its cards, when it could be worked out.
struct RowCounts {
    due_today: usize,
    total_cards: usize,
}

/// A deck that could not be counted, as a row.
fn uncounted_row(deck: &ResolvedCustomDeck) -> DrillRow {
    DrillRow {
        name: deck.name.clone(),
        slug: deck.slug.clone(),
        counts: None,
        is_deck: true,
    }
}

/// A row with nothing to do is dimmed, so the list reads as "these are the
/// ones waiting for you" at a glance.
fn row_class(row: &DrillRow, resume: &HashMap<String, usize>) -> &'static str {
    let nothing_due = matches!(row.counts, Some(RowCounts { due_today: 0, .. }));
    // A row whose counts failed is not dimmed: "nothing to do" is exactly
    // what is not known about it.
    if nothing_due && !resume.contains_key(&row.slug) {
        "drill-row muted"
    } else {
        "drill-row"
    }
}

/// "36 due · 36 cards", or "Nothing due · 11 cards" — and, when the
/// counts could not be computed at all, the fact that they could not.
fn row_meta(counts: &Option<RowCounts>) -> Markup {
    let Some(RowCounts {
        due_today,
        total_cards,
    }) = counts
    else {
        return html! {
            span.row-total.muted { "Counts unavailable" }
        };
    };
    html! {
        @if *due_today > 0 {
            span.row-due { (format!("{due_today} due")) }
        } @else {
            span.row-due.muted { "Nothing due" }
        }
        span.row-sep { "\u{00b7}" }
        span.row-total {
            (format!("{total_cards} card{}", if *total_cards == 1 { "" } else { "s" }))
        }
    }
}

/// The one status line under the title: where the cards came from and when
/// they last arrived. It replaced a stack of full-width bars, each of which
/// announced a background detail as loudly as the list itself.
fn status_line(status: &LandingStatus) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(email) = &status.signed_in_as {
        parts.push(email.clone());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" \u{00b7} "))
    }
}

fn render_landing_page(
    rows: &[DrillRow],
    resume: &HashMap<String, usize>,
    status: &LandingStatus,
    flash: Option<Flash>,
) -> Markup {
    let LandingStatus {
        config_available,
        ref signed_in_as,
        ..
    } = *status;
    let line = status_line(status);
    page_template(html! {
        @if let Some(f) = &flash { (f.render()) }
        div.landing {
            // Title, then everything that is not the list, on one line each.
            // The list is what the page is for; the rest is upkeep.
            header.app-bar {
                h1 { "hashcards-web" }
                nav.app-nav {
                    @if config_available {
                        a.nav-link href="/files" { "My cards" }
                    }
                    @if signed_in_as.is_some() {
                        // POST, so a third-party page cannot log the user out
                        // by embedding the URL.
                        form action="/auth/logout" method="post" {
                            button.nav-link type="submit" { "Log out" }
                        }
                    }
                }
            }
            @if let Some(line) = line {
                p.app-status { (line) }
            }

            @if rows.is_empty() {
                p.empty { "No collections configured." }
            } @else {
                ul.drill-list {
                    @for row in rows {
                        // One `class` attribute: two would be emitted verbatim
                        // and the browser would keep only the first.
                        li class=(row_class(row, resume)) {
                            div.drill-row-main {
                                // The name opens the collection: topics,
                                // stats, bookmarks, export. The button skips
                                // all of it and starts drilling.
                                a.drill-row-name href=(format!("/collection/{}", row.slug)) {
                                    (row.name)
                                    @if row.is_deck { span.row-tag { "deck" } }
                                }
                                div.drill-row-meta { (row_meta(&row.counts)) }
                            }
                            div.drill-row-action {
                                @if let Some(remaining) = resume.get(&row.slug) {
                                    a.btn.btn-primary href=(format!("/collection/{}", row.slug)) {
                                        (format!("Resume ({remaining} left)"))
                                    }
                                } @else if row.counts.as_ref().is_some_and(|c| c.due_today > 0) {
                                    // One tap into card one. Every topic, no
                                    // limit — the choices live behind the name
                                    // for the sessions that want them.
                                    form action=(format!("/collection/{}/start", row.slug)) method="post" {
                                        input type="hidden" name="all_topics" value="1";
                                        input .btn.btn-primary type="submit" value="Drill";
                                    }
                                }
                            }
                        }
                    }
                }
            }
            @if config_available {
                p.list-foot {
                    a href="/decks" { "Decks \u{2192}" }
                    span.row-sep { "\u{00b7}" }
                    "a deck is a saved selection of topics from any of your collections"
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::serve::config::DeckMember;
    use crate::cmd::serve::decks::slug_for_deck;
    use crate::error::Fallible;

    /// A deck the user saved must stay on the list even when its counts
    /// cannot be worked out. It used to be logged and dropped, so a deck
    /// whose member collection had stopped loading — a missing image is
    /// enough — silently disappeared from the one page that lists it, with
    /// nothing on screen to say anything had gone wrong.
    #[tokio::test]
    async fn a_deck_that_cannot_be_counted_is_still_listed() -> Fallible<()> {
        use crate::cmd::serve::cards::CardRoot;
        use crate::cmd::serve::cards::collection_id;
        use crate::cmd::serve::state::test_support::state_with_data_dir;

        let dir = tempfile::tempdir()?;
        let data_dir = dir.path().to_path_buf();
        let root = CardRoot::for_user(&data_dir, None)?;
        let folder = root.path().join("Spanish");
        std::fs::create_dir_all(&folder)?;
        // The image does not exist, so loading the collection fails.
        std::fs::write(folder.join("Verbs.md"), "Q: One\nA: ![](gone.png)\n")?;
        std::fs::create_dir_all(data_dir.join("db"))?;
        collection_id(&folder)?;

        let state = state_with_data_dir(data_dir);
        state.custom_decks.lock().push(ResolvedCustomDeck {
            name: "Exam".to_string(),
            slug: slug_for_deck("Exam", None),
            owner: None,
            members: vec![
                DeckMember::parse("Spanish/Verbs")
                    .ok_or_else(|| crate::error::ErrorReport::new("the member did not parse"))?,
            ],
        });

        let (status, html) = landing_handler(State(state), Query(HashMap::new()), None).await;
        assert_eq!(status, StatusCode::OK);
        let html = html.0;
        assert!(
            html.contains("Exam"),
            "the deck must still be listed: {html}"
        );
        assert!(
            html.contains("Counts unavailable"),
            "and must say why it has no counts: {html}"
        );
        Ok(())
    }
}
