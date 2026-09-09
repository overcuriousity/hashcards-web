use std::collections::HashMap;
use std::path::Path;

use axum::Form;
use axum::extract::Path as AxumPath;
use axum::extract::Query;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Html;
use axum::response::Redirect;
use maud::Markup;
use maud::html;
use serde::Deserialize;

use crate::cmd::drill::template::page_template;
use crate::cmd::run_blocking;
use crate::cmd::serve::auth::CurrentUser;
use crate::cmd::serve::handlers::find_collection;
use crate::cmd::serve::reviewdb::open_collection_db;
use crate::cmd::serve::state::AppState;
use crate::collection::Collection;
use crate::db::Bookmark;
use crate::error::ErrorReport;
use crate::error::Fallible;
use crate::flash::Flash;
use crate::types::card::Card;
use crate::types::card_hash::CardHash;

// ── List ─────────────────────────────────────────────────────────────────────

pub async fn bookmark_list_handler(
    State(state): State<AppState>,
    AxumPath(slug): AxumPath<String>,
    Query(query): Query<HashMap<String, String>>,
    current_user: Option<CurrentUser>,
) -> (StatusCode, Html<String>) {
    let flash = Flash::from_query(&query);
    let owner = current_user.map(|u| u.email);
    let state2 = state.clone();
    let slug2 = slug.clone();
    match run_blocking(move || bookmark_list_inner(&state2, &slug2, flash, owner.as_deref())).await
    {
        Ok(html) => (StatusCode::OK, Html(html)),
        Err(e) => error_page(&slug, e),
    }
}

fn bookmark_list_inner(
    state: &AppState,
    slug: &str,
    flash: Option<Flash>,
    owner: Option<&str>,
) -> Fallible<String> {
    let rc = find_collection(state, slug, owner)
        .ok_or_else(|| ErrorReport::new(format!("Unknown collection: {slug}")))?;
    let collection = Collection::open(rc.coll_dir.clone(), open_collection_db(&rc)?)?;
    let bookmarks = collection.db.list_bookmarks()?;
    let cards_by_hash: HashMap<CardHash, &Card> =
        collection.cards.iter().map(|c| (c.hash(), c)).collect();
    let html = render_bookmark_list(
        &rc.name,
        slug,
        &rc.coll_dir,
        &bookmarks,
        &cards_by_hash,
        flash,
    );
    Ok(html.into_string())
}

fn render_bookmark_list(
    collection_name: &str,
    slug: &str,
    coll_dir: &Path,
    bookmarks: &[Bookmark],
    cards: &HashMap<CardHash, &Card>,
    flash: Option<Flash>,
) -> Markup {
    let active: Vec<(&Bookmark, &Card)> = bookmarks
        .iter()
        .filter_map(|bm| cards.get(&bm.card_hash).map(|c| (bm, *c)))
        .collect();
    let orphaned: Vec<&Bookmark> = bookmarks
        .iter()
        .filter(|bm| !cards.contains_key(&bm.card_hash))
        .collect();

    page_template(html! {
        @if let Some(f) = &flash { (f.render()) }
        div.bookmarks {
            div.browse-header {
                a.back-link href=(format!("/collection/{slug}")) { "\u{2190} " (collection_name) }
                h1 { "Bookmarks" }
            }

            @if active.is_empty() && orphaned.is_empty() {
                p.empty { "No bookmarks yet. Press " b { "b" } " during drilling to bookmark a card." }
            } @else {
                @if !active.is_empty() {
                    div.bookmark-list {
                        @for (bm, card) in &active {
                            (render_bookmark_row(slug, coll_dir, bm, card))
                        }
                    }
                }
                @if !orphaned.is_empty() {
                    h2.orphaned-heading { "Orphaned" }
                    p.orphaned-note { "These cards no longer exist in the collection (likely edited or deleted outside the web UI)." }
                    div.bookmark-list {
                        @for bm in &orphaned {
                            (render_orphaned_row(slug, bm))
                        }
                    }
                }
            }
        }
    })
}

fn render_bookmark_row(slug: &str, coll_dir: &Path, bm: &Bookmark, card: &Card) -> Markup {
    let hash_hex = bm.card_hash.to_hex();
    let preview = card.preview();
    let rel_path = card
        .file_path()
        .strip_prefix(coll_dir)
        .unwrap_or(card.file_path())
        .display()
        .to_string();

    html! {
        div.bookmark-row {
            div.bookmark-meta {
                span.bookmark-deck { (card.deck_name()) }
                span.bookmark-path { (rel_path) }
                span.bookmark-date { (bm.created_at) }
            }
            p.bookmark-preview { (preview) }
            @if let Some(ref note) = bm.note {
                p.bookmark-note-display { (note) }
            }
            div.bookmark-actions {
                a.edit-link.btn.btn-secondary
                    href=(format!("/collection/{slug}/edit/{hash_hex}?return_to=bookmarks"))
                { "Edit" }
                form.inline-form
                    action=(format!("/collection/{slug}/bookmarks/{hash_hex}/delete"))
                    method="post"
                {
                    button type="submit" class="btn btn-secondary" { "Remove" }
                }
                form.note-form
                    action=(format!("/collection/{slug}/bookmarks/{hash_hex}/note"))
                    method="post"
                {
                    input
                        type="text"
                        name="note"
                        class="note-input"
                        placeholder="Add a note…"
                        value=(bm.note.as_deref().unwrap_or(""));
                    button type="submit" class="btn btn-secondary" { "Save note" }
                }
            }
        }
    }
}

fn render_orphaned_row(slug: &str, bm: &Bookmark) -> Markup {
    let hash_hex = bm.card_hash.to_hex();
    html! {
        div.bookmark-row.bookmark-orphaned {
            p.bookmark-preview { code { (hash_hex) } }
            div.bookmark-actions {
                form.inline-form
                    action=(format!("/collection/{slug}/bookmarks/{hash_hex}/delete"))
                    method="post"
                {
                    button type="submit" class="btn btn-secondary" { "Remove" }
                }
            }
        }
    }
}

// ── Delete ────────────────────────────────────────────────────────────────────

pub async fn bookmark_delete_handler(
    State(state): State<AppState>,
    AxumPath((slug, hash_hex)): AxumPath<(String, String)>,
    current_user: Option<CurrentUser>,
) -> Redirect {
    let to = format!("/collection/{slug}/bookmarks");
    let owner = current_user.map(|u| u.email);
    let state2 = state.clone();
    let slug2 = slug.clone();
    match run_blocking(move || bookmark_delete_inner(&state2, &slug2, &hash_hex, owner.as_deref()))
        .await
    {
        Ok(()) => Flash::success("Bookmark removed.").redirect(&to),
        Err(e) => Flash::error(format!("Failed to remove bookmark: {e}")).redirect(&to),
    }
}

fn bookmark_delete_inner(
    state: &AppState,
    slug: &str,
    hash_hex: &str,
    owner: Option<&str>,
) -> Fallible<()> {
    let rc = find_collection(state, slug, owner)
        .ok_or_else(|| ErrorReport::new(format!("Unknown collection: {slug}")))?;
    let collection = Collection::open(rc.coll_dir.clone(), open_collection_db(&rc)?)?;
    let hash = CardHash::from_hex(hash_hex)?;
    collection.db.delete_bookmark(hash)?;
    Ok(())
}

// ── Update note ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct NoteForm {
    pub note: String,
}

pub async fn bookmark_note_handler(
    State(state): State<AppState>,
    AxumPath((slug, hash_hex)): AxumPath<(String, String)>,
    current_user: Option<CurrentUser>,
    Form(form): Form<NoteForm>,
) -> Redirect {
    let to = format!("/collection/{slug}/bookmarks");
    let owner = current_user.map(|u| u.email);
    let state2 = state.clone();
    let slug2 = slug.clone();
    match run_blocking(move || {
        bookmark_note_inner(&state2, &slug2, &hash_hex, form.note, owner.as_deref())
    })
    .await
    {
        Ok(()) => Flash::success("Note saved.").redirect(&to),
        Err(e) => Flash::error(format!("Failed to save note: {e}")).redirect(&to),
    }
}

fn bookmark_note_inner(
    state: &AppState,
    slug: &str,
    hash_hex: &str,
    note: String,
    owner: Option<&str>,
) -> Fallible<()> {
    let rc = find_collection(state, slug, owner)
        .ok_or_else(|| ErrorReport::new(format!("Unknown collection: {slug}")))?;
    let collection = Collection::open(rc.coll_dir.clone(), open_collection_db(&rc)?)?;
    let hash = CardHash::from_hex(hash_hex)?;
    let note = if note.trim().is_empty() {
        None
    } else {
        Some(note.trim().to_string())
    };
    collection.db.update_bookmark_note(hash, note)?;
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn error_page(slug: &str, e: impl std::fmt::Display) -> (StatusCode, Html<String>) {
    let html = page_template(html! {
        div.error {
            h1 { "Error" }
            p { (e) }
            a href=(format!("/collection/{slug}")) { "\u{2190} Back" }
        }
    })
    .into_string();
    (StatusCode::INTERNAL_SERVER_ERROR, Html(html))
}
