//! The trash page: undo a delete, or make it final.
//!
//! Emptying the trash is the only thing in hashcards that destroys
//! anything, and it is deliberately a human action here rather than
//! something any other caller can reach.

use std::collections::HashMap;

use axum::Form;
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
    let data_dir = data_dir(state)?;
    let root = user_root(state, user)?;
    list_trash(&data_dir, root.tree_name()?)
}

fn data_dir(state: &AppState) -> Fallible<std::path::PathBuf> {
    match &state.config.data_dir {
        Some(d) => Ok(d.clone()),
        None => fail("The trash needs a data directory. Start hashcards-web with a config file."),
    }
}

fn restore_one(state: &AppState, user: Option<&CurrentUser>, raw_id: &str) -> Fallible<String> {
    let data_dir = data_dir(state)?;
    let id = TrashId::parse(raw_id)?;
    let root = user_root(state, user)?;
    let rel = restore_from_trash(&data_dir, &root, &id)?;
    Ok(format!("Restored `{rel}`."))
}

/// Destroy one entry, and the review rows it was the last thing holding on
/// to.
fn purge_one(state: &AppState, user: Option<&CurrentUser>, raw_id: &str) -> Fallible<String> {
    let data_dir = data_dir(state)?;
    let id = TrashId::parse(raw_id)?;
    let root = user_root(state, user)?;
    if let Some(collection) = purge_entry(&data_dir, root.tree_name()?, &id)? {
        erase_collection_rows(state, &root, &collection)?;
    }
    Ok("Deleted for good.".to_string())
}

fn empty_trash(state: &AppState, user: Option<&CurrentUser>) -> Fallible<String> {
    let data_dir = data_dir(state)?;
    let root = user_root(state, user)?;
    let erased = purge_all(&data_dir, root.tree_name()?)?;
    for collection in &erased {
        erase_collection_rows(state, &root, collection)?;
    }
    Ok("The trash is empty.".to_string())
}

fn render_trash(entries: &[TrashEntry], flash: Option<Flash>) -> Markup {
    page_template(html! {
        div.landing {
            @if let Some(f) = &flash { (f.render()) }
            div.browse-header {
                a.back-link href="/files" { "← My Cards" }
                h1 { "Trash" }
            }

            p.hint {
                "Deleting something moves it here. Nothing is destroyed until you empty the \
                 trash — and emptying it also erases the review history of any collection \
                 in it."
            }

            @if entries.is_empty() {
                p.notice { "There is nothing in the trash." }
            } @else {
                ul.file-tree {
                    @for entry in entries {
                        li.file-row {
                            span.file-name.(class_for(entry)) { (entry.original_path) }
                            span.hint { (entry.deleted_at) }
                            div.file-actions {
                                form.file-form action="/trash/restore" method="post" {
                                    input type="hidden" name="id" value=(entry.id);
                                    input.btn.btn-sm type="submit" value="Restore";
                                }
                                form.file-form action="/trash/purge" method="post" {
                                    input type="hidden" name="id" value=(entry.id);
                                    input.btn.btn-sm.btn-danger type="submit"
                                        value="Delete for good";
                                }
                            }
                        }
                    }
                }

                form.file-form action="/trash/empty" method="post" {
                    input.btn.btn-danger type="submit" value="Empty the trash";
                }
            }
        }
    })
}

/// A trashed collection is drawn as a folder: it was one.
fn class_for(entry: &TrashEntry) -> &'static str {
    match entry.kind {
        crate::cmd::serve::trash::TrashKind::File => "file",
        _ => "folder",
    }
}

pub async fn trash_get_handler(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    current_user: Option<CurrentUser>,
) -> (StatusCode, Html<String>) {
    let flash = Flash::from_query(&query);
    // Reading a directory is blocking work (BUG-44).
    let entries = run_blocking(move || trash_rows(&state, current_user.as_ref())).await;
    let markup = match entries {
        Ok(entries) => render_trash(&entries, flash),
        Err(e) => render_trash(&[], Some(Flash::error(e.to_string()))),
    };
    (StatusCode::OK, Html(markup.into_string()))
}

#[derive(Deserialize)]
pub struct TrashActionForm {
    pub id: String,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::serve::cards::CardRoot;
    use crate::cmd::serve::state::test_support::state_with_data_dir;
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

    fn trash_a_file(dir: &TempDir, root: &CardRoot) -> Fallible<TrashId> {
        move_to_trash(
            dir.path(),
            root,
            "Spanish/verbs.md",
            TrashKind::File,
            None,
            Timestamp::now(),
        )
    }

    #[test]
    fn the_page_lists_what_is_in_the_trash() -> Fallible<()> {
        let (dir, state, root) = fixture()?;
        trash_a_file(&dir, &root)?;
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

    /// Emptying is the only thing that destroys anything, so the page says
    /// what it costs before the button is pressed.
    #[test]
    fn the_page_warns_that_emptying_erases_review_history() -> Fallible<()> {
        let (dir, state, root) = fixture()?;
        trash_a_file(&dir, &root)?;
        let html = render_trash(&trash_rows(&state, None)?, None).into_string();
        assert!(html.contains("review history"), "{html}");
        Ok(())
    }

    #[test]
    fn restoring_puts_the_file_back() -> Fallible<()> {
        let (dir, state, root) = fixture()?;
        let id = trash_a_file(&dir, &root)?;
        let msg = restore_one(&state, None, id.as_str())?;
        assert!(msg.contains("Spanish/verbs.md"), "{msg}");
        assert!(root.path().join("Spanish/verbs.md").is_file());
        Ok(())
    }

    #[test]
    fn purging_one_entry_leaves_the_others() -> Fallible<()> {
        let (dir, state, root) = fixture()?;
        let id = trash_a_file(&dir, &root)?;
        move_to_trash(
            dir.path(),
            &root,
            "Spanish",
            TrashKind::Folder,
            None,
            Timestamp::now(),
        )?;
        purge_one(&state, None, id.as_str())?;
        let left = trash_rows(&state, None)?;
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].original_path, "Spanish");
        Ok(())
    }

    #[test]
    fn emptying_the_trash_removes_everything() -> Fallible<()> {
        let (dir, state, root) = fixture()?;
        trash_a_file(&dir, &root)?;
        empty_trash(&state, None)?;
        assert!(trash_rows(&state, None)?.is_empty());
        Ok(())
    }

    /// A crafted id must not reach the filesystem.
    #[test]
    fn a_bad_trash_id_is_refused() -> Fallible<()> {
        let (_dir, state, _root) = fixture()?;
        assert!(restore_one(&state, None, "../../etc/passwd").is_err());
        assert!(purge_one(&state, None, "../../etc/passwd").is_err());
        Ok(())
    }
}
